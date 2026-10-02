use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use gix::filter::plumbing::driver::apply::Delay;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use gix::bstr::ByteSlice;
use gix::diff::Rewrites;
use gix::dir::entry::Status;
use gix::objs::tree::EntryKind;
use gix::sec::trust::DefaultForLevel;
use gix::status::{
    index_worktree::Item,
    plumbing::index_as_worktree::{Change, EntryStatus},
    UntrackedFiles,
};
use gix::{Commit, ObjectId, Repository, ThreadSafeRepository};

use crate::{FileChange, PreparedVcs, RevisionDiffBase};

#[cfg(test)]
mod test;

#[inline]
fn get_repo_dir(file: &Path) -> Result<&Path> {
    file.parent().context("file has no parent directory")
}

pub fn get_diff_base(file: &Path, trust_full: bool) -> Result<Vec<u8>> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    // TODO cache repository lookup

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir, trust_full)
        .context("failed to open git repo")?
        .to_thread_local();
    let head = repo.head_commit()?;
    get_diff_base_from_repo(&repo, &head, &file, None, false)
}

pub fn get_review_base(
    file: &Path,
    reference: &str,
    trust_full: bool,
    cancel: &helix_event::TaskHandle,
) -> Result<RevisionDiffBase> {
    if cancel.is_canceled() {
        bail!("Git review canceled");
    }
    let file = gix::path::realpath(file).context("resolve symlinks")?;
    let repo = open_repo(get_repo_dir(&file)?, trust_full)?.to_thread_local();
    let head = repo.head_commit()?;
    let target = repo
        .rev_parse_single(reference.as_bytes().as_bstr())
        .with_context(|| format!("Cannot resolve Git revision '{reference}'"))?
        .object()?
        .peel_to_commit()
        .with_context(|| format!("Git revision '{reference}' is not a commit"))?;
    if cancel.is_canceled() {
        bail!("Git review canceled");
    }
    let base = repo
        .merge_base(head.id, target.id)
        .with_context(|| format!("No common ancestor between HEAD and '{reference}'"))?
        .object()?
        .peel_to_commit()?;
    Ok(RevisionDiffBase {
        bytes: get_diff_base_from_repo(&repo, &base, &file, Some(cancel), true)?,
        commit: base.id.to_string(),
    })
}

fn get_diff_base_from_repo(
    repo: &Repository,
    head: &Commit,
    file: &Path,
    cancel: Option<&helix_event::TaskHandle>,
    allow_missing: bool,
) -> Result<Vec<u8>> {
    if cancel.is_some_and(helix_event::TaskHandle::is_canceled) {
        bail!("VCS preparation canceled");
    }
    let Some(file_oid) = find_file_in_commit(repo, head, file)? else {
        if allow_missing {
            return Ok(Vec::new());
        }
        bail!("file is untracked");
    };

    let file_object = repo.find_object(file_oid)?;
    let data = file_object.detach().data;
    if cancel.is_some_and(helix_event::TaskHandle::is_canceled) {
        bail!("VCS preparation canceled");
    }
    // Get the actual data that git would make out of the git object.
    // This will apply the user's git config or attributes like crlf conversions.
    //
    // The whole filter pipeline still runs in untrusted (`Trust::Reduced`) mode so built-in
    // conversions like autocrlf keep working, but gix drops `filter.*.clean` / `filter.*.smudge`
    // drivers defined in untrusted (repository-local) config, so those external programs are not
    // executed unless the workspace was explicitly trusted. This relies on `open_repo` forcing the
    // trust level instead of letting gix re-derive it from `.git` ownership; see the note there.
    if let Some(work_dir) = repo.workdir() {
        let rela_path = file.strip_prefix(work_dir)?;
        let rela_path = gix::path::try_into_bstr(rela_path)?;
        let (mut pipeline, _) = repo.filter_pipeline(None)?;
        let mut worktree_outcome =
            pipeline.convert_to_worktree(&data, rela_path.as_ref(), Delay::Forbid)?;
        let mut buf = Vec::with_capacity(data.len());
        if let Some(cancel) = cancel {
            let mut chunk = [0; 64 * 1024];
            loop {
                if cancel.is_canceled() {
                    bail!("VCS preparation canceled");
                }
                let len = worktree_outcome.read(&mut chunk)?;
                if len == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..len]);
            }
        } else {
            worktree_outcome.read_to_end(&mut buf)?;
        }
        Ok(buf)
    } else {
        Ok(data)
    }
}

pub fn get_current_head_name(file: &Path, trust_full: bool) -> Result<Arc<ArcSwap<Box<str>>>> {
    debug_assert!(!file.exists() || file.is_file());
    debug_assert!(file.is_absolute());
    let file = gix::path::realpath(file).context("resolve symlinks")?;

    let repo_dir = get_repo_dir(&file)?;
    let repo = open_repo(repo_dir, trust_full)
        .context("failed to open git repo")?
        .to_thread_local();
    let head_commit = repo.head_commit()?;
    current_head_name(&repo, &head_commit)
}

fn current_head_name(repo: &Repository, head_commit: &Commit) -> Result<Arc<ArcSwap<Box<str>>>> {
    let head_ref = repo.head_ref()?;

    let name = match head_ref {
        Some(reference) => reference.name().shorten().to_string(),
        None => head_commit.id.to_hex_with_len(8).to_string(),
    };

    Ok(Arc::new(ArcSwap::from_pointee(name.into_boxed_str())))
}

pub fn prepare_vcs(
    file: &Path,
    trust_full: bool,
    cancel: &helix_event::TaskHandle,
) -> Result<PreparedVcs> {
    if cancel.is_canceled() {
        bail!("VCS preparation canceled");
    }
    let file = gix::path::realpath(file).context("resolve symlinks")?;
    let repo = open_repo(get_repo_dir(&file)?, trust_full)?.to_thread_local();
    if cancel.is_canceled() {
        bail!("VCS preparation canceled");
    }
    let head = repo.head_commit()?;
    let head_name = current_head_name(&repo, &head)?;
    let diff_base = get_diff_base_from_repo(&repo, &head, &file, Some(cancel), false)
        .map_err(|err| log::debug!("Loading VCS baseline for {}: {err:#}", file.display()))
        .ok();
    Ok(PreparedVcs {
        diff_base,
        head: Some(head_name),
    })
}

pub fn for_each_changed_file(
    cwd: &Path,
    trust_full: bool,
    interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    f: impl Fn(Result<FileChange>) -> bool,
) -> Result<()> {
    if interrupt.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    status(&open_repo(cwd, trust_full)?.to_thread_local(), interrupt, f)
}

fn open_repo(path: &Path, trust_full: bool) -> Result<ThreadSafeRepository> {
    // `trust_full` is the workspace-trust decision made by the caller, and it must be the
    // authority on the gix trust level. gix's own discovery (`discover_*`) ignores a
    // caller-supplied trust level: it always re-derives trust from `.git` ownership, so a malicious
    // `.git/config` in a user-owned directory would be opened as `Trust::Full` regardless of our
    // gate. Worse, the GIT_DIR-environment branch of that discovery panics because it never sets a
    // trust level at all. So we split discovery from opening: find the repository path ourselves,
    // then `open_opts(..).with(trust)`, which forces the trust level and skips gix's ownership
    // check. Under `Trust::Reduced`, gix then refuses to honor untrusted repository-local config
    // such as `filter.*` smudge/clean drivers.

    let trust = if trust_full {
        gix::sec::Trust::Full
    } else {
        gix::sec::Trust::Reduced
    };

    // On Windows various configuration options are bundled as part of the git installation. The
    // lookup is expensive; only do it there.
    let config = gix::open::permissions::Config {
        system: true,
        git: true,
        user: true,
        env: true,
        includes: true,
        git_binary: cfg!(windows),
    };

    let permissions = gix::open::Permissions {
        config,
        ..gix::open::Permissions::default_for_level(trust)
    };

    let discover_options = gix::discover::upwards::Options {
        dot_git_only: true,
        ..Default::default()
    };
    let (repo_path, _trust_from_ownership) = gix::discover::upwards_opts(path, discover_options)
        .context("failed to discover git repo")?;
    let (git_dir, _work_dir) = repo_path.into_repository_and_work_tree_directories();

    let options = gix::open::Options::default()
        .permissions(permissions)
        // `git_dir` is the discovered `.git` directory (or a linked-worktree git dir), so open it
        // as-is rather than letting gix append `.git` again.
        .open_path_as_is(true)
        .with(trust);

    Ok(ThreadSafeRepository::open_opts(git_dir, options)?)
}

/// Emulates the result of running `git status` from the command line.
fn status(
    repo: &Repository,
    interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    f: impl Fn(Result<FileChange>) -> bool,
) -> Result<()> {
    let work_dir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("working tree not found"))?
        .to_path_buf();

    let status_platform = repo
        .status(gix::progress::Discard)?
        // Here we discard the `status.showUntrackedFiles` config, as it makes little sense in
        // our case to not list new (untracked) files. We could have respected this config
        // if the default value weren't `Collapsed` though, as this default value would render
        // the feature unusable to many.
        .untracked_files(UntrackedFiles::Files)
        .should_interrupt_owned(interrupt.clone())
        // Turn on file rename detection, which is off by default.
        .index_worktree_rewrites(Some(Rewrites {
            copies: None,
            percentage: Some(0.5),
            limit: 1000,
            ..Default::default()
        }));

    // No filtering based on path
    let empty_patterns = vec![];

    let status_iter = status_platform.into_index_worktree_iter(empty_patterns)?;

    for item in status_iter {
        if interrupt.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let item = match item {
            Ok(item) => item,
            Err(err) => {
                if !f(Err(err.into())) {
                    interrupt.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
                continue;
            }
        };
        let change = match item {
            Item::Modification {
                rela_path, status, ..
            } => {
                let path = work_dir.join(rela_path.to_path()?);
                match status {
                    EntryStatus::Conflict { .. } => FileChange::Conflict { path },
                    EntryStatus::Change(Change::Removed) => FileChange::Deleted { path },
                    EntryStatus::Change(Change::Modification { .. }) => {
                        FileChange::Modified { path }
                    }
                    // Files marked with `git add --intent-to-add`. Such files
                    // still show up as new in `git status`, so it's appropriate
                    // to show them the same way as untracked files in the
                    // "changed file" picker. One example of this being used
                    // is Jujutsu, a Git-compatible VCS. It marks all new files
                    // with `--intent-to-add` automatically.
                    EntryStatus::IntentToAdd => FileChange::Untracked { path },
                    _ => continue,
                }
            }
            Item::DirectoryContents { entry, .. } if entry.status == Status::Untracked => {
                FileChange::Untracked {
                    path: work_dir.join(entry.rela_path.to_path()?),
                }
            }
            Item::Rewrite {
                source,
                dirwalk_entry,
                ..
            } => FileChange::Renamed {
                from_path: work_dir.join(source.rela_path().to_path()?),
                to_path: work_dir.join(dirwalk_entry.rela_path.to_path()?),
            },
            _ => continue,
        };
        if !f(Ok(change)) {
            interrupt.store(true, std::sync::atomic::Ordering::Relaxed);
            break;
        }
    }

    Ok(())
}

/// Finds the object that contains the contents of a file at a specific commit.
fn find_file_in_commit(
    repo: &Repository,
    commit: &Commit,
    file: &Path,
) -> Result<Option<ObjectId>> {
    let repo_dir = repo.workdir().context("repo has no worktree")?;
    let rel_path = file.strip_prefix(repo_dir)?;
    let tree = commit.tree()?;
    let Some(tree_entry) = tree.lookup_entry_by_path(rel_path)? else {
        return Ok(None);
    };
    match tree_entry.mode().kind() {
        // not a file, everything is new, do not show diff
        mode @ (EntryKind::Tree | EntryKind::Commit | EntryKind::Link) => {
            bail!("entry at {} is not a file but a {mode:?}", file.display())
        }
        // found a file
        EntryKind::Blob | EntryKind::BlobExecutable => Ok(Some(tree_entry.object_id())),
    }
}
