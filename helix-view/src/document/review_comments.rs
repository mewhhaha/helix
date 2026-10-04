//! Review notes belong to a sidecar, never to the source buffer or its history.

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use helix_core::{Assoc, ChangeSet, Rope, RopeSlice};
use serde::{Deserialize, Serialize};

use super::Document;
use crate::DocumentId;

pub(crate) struct ReviewCommentTarget {
    pub source: DocumentId,
    pub id: u64,
    pub previous: Option<String>,
    pub was_dirty: bool,
}

mod store;
pub use store::{
    content_hash, default_author, read_sidecar, ReviewMessage, ReviewSession, ReviewSidecar,
    ReviewSnapshot, ReviewStore, ReviewThread,
};
pub(super) type ReviewComments = ReviewStore;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentSide {
    Current,
    Base,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentAnchor {
    pub side: CommentSide,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Unicode character offsets, with an exclusive end.
    pub range: Range<usize>,
    /// An excerpt from the beginning of the range, bounded for large selections.
    pub quote: String,
    pub prefix: String,
    pub suffix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_hash: Option<String>,
}

impl CommentAnchor {
    pub fn new(
        side: CommentSide,
        reference: Option<String>,
        text: RopeSlice,
        range: Range<usize>,
    ) -> Self {
        Self {
            side,
            reference,
            quote: text
                .slice(range.start..range.end.min(range.start.saturating_add(1024)))
                .to_string(),
            prefix: text
                .slice(range.start.saturating_sub(40)..range.start)
                .to_string(),
            suffix: text
                .slice(range.end..(range.end + 40).min(text.len_chars()))
                .to_string(),
            selection_hash: Some(content_hash(text.slice(range.clone()))),
            range,
        }
    }

    /// Keep unchanged notes attached after lines are inserted before their anchor.
    /// Ambiguous or missing context stays detached instead of pointing at other code.
    pub fn locate(&self, text: RopeSlice) -> Option<Range<usize>> {
        let quote_chars = self.quote.chars().count();
        if self.range.start > self.range.end || quote_chars > self.range.len() {
            return None;
        }
        let prefix_chars = self.prefix.chars().count();
        let suffix_chars = self.suffix.chars().count();
        let context_matches = |range: &Range<usize>| {
            let prefix = if prefix_chars == 0 {
                range.start == 0
            } else {
                range.start >= prefix_chars
                    && text.slice(range.start - prefix_chars..range.start) == self.prefix
            };
            let suffix = if suffix_chars == 0 {
                range.end == text.len_chars()
            } else {
                suffix_chars <= text.len_chars() - range.end
                    && text.slice(range.end..range.end + suffix_chars) == self.suffix
            };
            prefix || suffix
        };
        let hash_matches = |range: &Range<usize>| {
            self.selection_hash
                .as_ref()
                .is_none_or(|hash| *hash == content_hash(text.slice(range.clone())))
        };
        if self.range.end <= text.len_chars()
            && text.slice(self.range.start..self.range.start + quote_chars) == self.quote
            && context_matches(&self.range)
            && hash_matches(&self.range)
        {
            return Some(self.range.clone());
        }
        if self.quote.is_empty() {
            return None;
        }
        let string = text.to_string();
        let mut matches = string
            .match_indices(&self.quote)
            .filter_map(|(byte_start, _)| {
                let start = text.byte_to_char(byte_start);
                let end = start.checked_add(self.range.len())?;
                let range = start..end;
                (end <= text.len_chars() && context_matches(&range) && hash_matches(&range))
                    .then_some(range)
            });
        let range = matches.next()?;
        matches.next().is_none().then_some(range)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub anchor: CommentAnchor,
    pub text: String,
}

pub fn sidecar_path(source: &Path) -> PathBuf {
    let mut name = source.as_os_str().to_owned();
    name.push(".review.json");
    PathBuf::from(name)
}

impl Document {
    /// Internal editable buffer whose text is persisted in its source's sidecar.
    pub fn review_comment_target(&self) -> Option<(DocumentId, u64)> {
        self.review_comment_target
            .as_ref()
            .map(|target| (target.source, target.id))
    }

    pub fn review_comments(&self) -> &[ReviewComment] {
        &self.review_comments.comments
    }

    pub fn review_comments_generation(&self) -> u64 {
        self.review_comments.generation
    }

    pub fn review_comments_dirty(&self) -> bool {
        self.review_comments.dirty
    }

    pub fn review_comment(&self, id: u64) -> Option<&ReviewComment> {
        self.review_comments()
            .iter()
            .find(|comment| comment.id == id)
    }

    pub fn load_review_comments(&mut self) -> Result<()> {
        let Some(source) = self.path().map(Path::to_owned) else {
            return Ok(());
        };
        let selected = self
            .is_diff_mode_read_only()
            .then(|| self.review_session().map(|review| review.id.clone()))
            .flatten();
        self.review_comments.load(&source)?;
        if let Some(id) = selected {
            self.restore_review_session(&id);
        }
        Ok(())
    }

    pub fn review_store(&self) -> &ReviewStore {
        &self.review_comments
    }

    pub fn review_session(&self) -> Option<&ReviewSession> {
        self.review_comments.active()
    }

    pub fn local_review_branch(&self) -> Option<&str> {
        self.local_review_revision
            .as_ref()
            .and_then(|revision| revision.branch.as_deref())
    }

    pub fn activate_review_session(&mut self, id: &str) -> Result<()> {
        self.review_comments.activate(id)
    }

    pub(crate) fn restore_review_session(&mut self, id: &str) {
        if self.review_comments.session(id).is_some() {
            self.review_comments.data.active_review = Some(id.into());
        }
    }

    pub fn begin_review_session(&mut self, pr: Option<String>) -> Result<()> {
        let was_dirty = self.review_comments.dirty;
        self.select_review_session(pr)?;
        if !self.review_comments.data.threads.is_empty() && self.review_comments.dirty {
            self.save_review_comments()?;
        } else {
            // Merely viewing a diff doesn't create a sidecar. The first note
            // persists the captured session along with its thread.
            self.review_comments.dirty = was_dirty;
        }
        Ok(())
    }

    pub fn review_comment_thread(&self, id: u64) -> Option<&ReviewThread> {
        self.review_comments.message_thread(id)
    }

    pub fn review_comment_visible(&self, id: u64) -> bool {
        self.review_comment_thread(id).is_some_and(|thread| {
            !thread.resolved
                && self.review_session().is_some_and(|review| {
                    thread.review_id == review.id
                        && review.target == self.review_diff_reference().unwrap_or("HEAD")
                })
        })
    }

    pub fn select_review_session(&mut self, pr: Option<String>) -> Result<String> {
        self.load_review_comments()?;
        let target = self.review_diff_reference().unwrap_or("HEAD").to_owned();
        let revision = self
            .review_diff
            .as_ref()
            .and_then(|base| base.revision.as_ref())
            .or(self.local_review_revision.as_ref());
        let branch = revision.and_then(|revision| revision.branch.clone());
        let pr = pr.or_else(|| {
            self.review_session()
                .filter(|review| review.target == target && review.branch == branch)
                .and_then(|review| review.pr.clone())
        });
        let snapshot = ReviewSnapshot {
            target_commit: revision.map(|revision| revision.target_commit.clone()),
            base_commit: revision.map(|revision| revision.base_commit.clone()),
            head_commit: revision.map(|revision| revision.head_commit.clone()),
            content_hash: content_hash(self.text().slice(..)),
            base_content_hash: self
                .review_diff
                .as_ref()
                .map(|base| base.base_hash.clone())
                .or_else(|| {
                    self.review_diff_handle()
                        .map(|handle| content_hash(handle.load().diff_base().slice(..)))
                }),
        };
        Ok(self
            .review_comments
            .select(ReviewSession::new(target, branch, pr, snapshot)))
    }

    pub fn add_review_comment(&mut self, anchor: CommentAnchor) -> Result<u64> {
        if let Some(error) = &self.review_comments.error {
            bail!("{error}");
        }
        if self.path().is_none() {
            bail!("Save this buffer to a file before adding review comments");
        }
        if self.local_review_revision.is_none()
            && self
                .review_diff
                .as_ref()
                .is_none_or(|base| base.revision.is_none())
            && self.vcs_controller.is_running()
        {
            bail!("Review baseline is still loading; retry when Git preparation finishes");
        }
        // Pending startup and non-Git buffers can still hold local notes. A
        // deleted-row anchor carries its selected target even in that case.
        let target = match anchor.side {
            CommentSide::Current => self.review_diff_reference().unwrap_or("HEAD"),
            CommentSide::Base => anchor.reference.as_deref().unwrap_or("HEAD"),
        }
        .to_owned();
        let id = if self.review_diff_reference().unwrap_or("HEAD") == target {
            self.select_review_session(None)?
        } else {
            self.review_comments.select(ReviewSession::new(
                target,
                None,
                None,
                ReviewSnapshot {
                    content_hash: content_hash(self.text().slice(..)),
                    ..ReviewSnapshot::default()
                },
            ))
        };
        self.review_comments
            .add(&id, anchor, default_author(), String::new())
    }

    pub fn reply_review_comment(&mut self, thread: u64) -> Result<u64> {
        self.review_comments
            .reply(thread, default_author(), String::new())
    }

    pub fn set_review_comment_text(&mut self, id: u64, text: String) -> bool {
        self.review_comments.set_text(id, text)
    }

    pub fn resolve_review_thread(&mut self, id: u64, resolved: bool) -> Result<()> {
        let previous = self.review_comments.clone();
        self.review_comments.resolve(id, resolved)?;
        if let Err(error) = self.save_review_comments() {
            self.review_comments = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn remove_review_comment(&mut self, id: u64) -> Result<()> {
        let previous = self.review_comments.clone();
        self.review_comments.remove(id)?;
        if let Err(error) = self.save_review_comments() {
            self.review_comments = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn cancel_review_comment_edit(
        &mut self,
        id: u64,
        previous: Option<String>,
        was_dirty: bool,
    ) {
        if let Some(text) = previous {
            self.set_review_comment_text(id, text);
        } else {
            let _ = self.review_comments.remove(id);
        }
        self.review_comments.dirty = was_dirty;
        self.review_comments.error = None;
        let result = if was_dirty {
            self.load_review_comments()
        } else if let Some(path) = self.path().map(Path::to_owned) {
            self.review_comments.discard(&path)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            log::warn!("Cannot reload review comments: {error:#}");
        }
    }

    pub fn save_review_comments(&mut self) -> Result<()> {
        let source = self
            .path()
            .context("Review comments require a source path")?
            .to_owned();
        self.review_comments.save(&source)
    }

    /// Only persist mapped anchors when the completed save matches this buffer.
    /// An older queued save must not publish positions from newer unsaved edits.
    pub fn save_review_comments_after_source_write(
        &mut self,
        text: &Rope,
        path: &Path,
    ) -> Result<()> {
        if self.review_comments.dirty
            && !self.review_comments().is_empty()
            && !self.is_diff_mode_read_only()
            && self.path() == Some(path)
            && self.text().is_instance(text)
        {
            self.save_review_comments()?;
        }
        Ok(())
    }

    pub(super) fn map_review_comments(&mut self, changes: &ChangeSet, old_text: RopeSlice) {
        let mut mapped: Vec<_> = self
            .review_comments
            .data
            .threads
            .iter()
            .enumerate()
            .filter_map(|(index, thread)| {
                (thread.anchor.side == CommentSide::Current)
                    .then(|| thread.anchor.locate(old_text))
                    .flatten()
                    .map(|range| (index, range))
            })
            .collect();
        let mut positions: Vec<_> = mapped
            .iter_mut()
            .flat_map(|(_, range)| {
                [
                    (&mut range.start, Assoc::After),
                    (&mut range.end, Assoc::Before),
                ]
            })
            .collect();
        positions.sort_unstable_by_key(|(pos, _)| **pos);
        changes.update_positions(positions.into_iter());
        let mut changed = false;
        for (index, range) in mapped {
            let start = range.start.min(self.text.len_chars());
            let end = range.end.min(self.text.len_chars()).max(start);
            let anchor = &mut self.review_comments.data.threads[index].anchor;
            changed |= anchor.range != (start..end);
            // Keep the quote and its fingerprint: editing the referenced code
            // makes a thread outdated rather than silently changing its subject.
            anchor.range = start..end;
        }
        if changed {
            self.review_comments.changed();
            self.review_comments.rebuild_comments();
        }
    }
}

#[cfg(test)]
mod tests;
