//! Non-interactive local review commands. No terminal or language server starts.

use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    ops::Range,
    path::{Path, PathBuf},
};

use anyhow::{bail, ensure, Context, Result};
use helix_core::{encoding::Encoding, Rope};
use helix_view::document::{
    read_to_string,
    review_comments::{
        content_hash, default_author, CommentAnchor, CommentSide, ReviewSession, ReviewSnapshot,
        ReviewStore,
    },
};
use serde_json::{json, Value};

const HELP: &str = "\
Local review threads for Helix and agents. All results are JSON.

USAGE:
    hx review start FILE [--target REF] [--pr URL]
    hx review list FILE [--review ID | --all]
    hx review add FILE [--review ID] --line N [--end-line N]
        [--start-column N] [--end-column N] [--side current|base]
        [--quote TEXT] [--expected-head SHA] [--expected-hash SHA256]
        [--author NAME] (--body TEXT | --body-file FILE|-)
    hx review reply FILE --thread ID [--author NAME] (--body TEXT | --body-file FILE|-)
    hx review edit FILE --message ID (--body TEXT | --body-file FILE|-)
    hx review resolve FILE --thread ID
    hx review reopen FILE --thread ID
    hx review remove FILE --message ID

Start captures the target, merge base, HEAD and file hashes. Starting again
refreshes that snapshot while retaining thread IDs and original anchors.
Add requires the file and HEAD to still match the captured snapshot.
Lines/columns are 1-based; columns count Unicode characters. End lines are
inclusive and an explicit end column is exclusive. Omitted columns select
whole lines. Deleted-text positions use the base file's line numbers.
Authors default to HELIX_REVIEW_AUTHOR, USER, USERNAME, then 'user'.
Comments remain local in FILE.review.json. No command publishes to GitHub.
";

struct Arguments {
    command: String,
    source: PathBuf,
    options: BTreeMap<String, String>,
}

impl Arguments {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let command = args.next().context("Missing review command")?;
        let allowed: &[&str] = match command.as_str() {
            "start" => &["target", "pr"],
            "list" => &["review", "all"],
            "add" => &[
                "review",
                "line",
                "end-line",
                "start-column",
                "end-column",
                "side",
                "quote",
                "expected-head",
                "expected-hash",
                "author",
                "body",
                "body-file",
            ],
            "reply" => &["thread", "author", "body", "body-file"],
            "edit" => &["message", "body", "body-file"],
            "resolve" | "reopen" => &["thread"],
            "remove" => &["message"],
            _ => bail!("Unknown review command '{command}'; use hx review --help"),
        };
        let source = args.next().context("Specify the source file")?;
        ensure!(
            !source.starts_with("--"),
            "Specify FILE before review options"
        );
        let source = helix_stdx::path::canonicalize(PathBuf::from(source));
        ensure!(
            !source.is_dir() && source.parent().is_some_and(Path::is_dir),
            "Review source must be a file with an existing parent directory"
        );
        let mut options = BTreeMap::new();
        while let Some(arg) = args.next() {
            let name = arg
                .strip_prefix("--")
                .context("Expected a --review-option")?;
            ensure!(
                allowed.contains(&name),
                "Unknown option {arg} for review {command}"
            );
            let value = if name == "all" {
                String::new()
            } else {
                args.next()
                    .with_context(|| format!("Missing value for {arg}"))?
            };
            ensure!(
                options.insert(name.into(), value).is_none(),
                "Duplicate option {arg}"
            );
        }
        Ok(Self {
            command,
            source,
            options,
        })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.options.get(name).map(String::as_str)
    }

    fn required(&self, name: &str) -> Result<&str> {
        self.get(name).with_context(|| format!("Missing --{name}"))
    }

    fn number(&self, name: &str) -> Result<u64> {
        self.required(name)?
            .parse()
            .with_context(|| format!("Invalid --{name}"))
    }

    fn author(&self) -> String {
        self.get("author")
            .map(str::to_owned)
            .unwrap_or_else(default_author)
    }

    fn body(&self) -> Result<String> {
        match (self.get("body"), self.get("body-file")) {
            (Some(body), None) => Ok(body.into()),
            (None, Some(path)) => {
                let mut body = String::new();
                let reader: Box<dyn Read> = if path == "-" {
                    Box::new(std::io::stdin())
                } else {
                    Box::new(File::open(path)?)
                };
                reader.take(8 * 1024 * 1024 + 1).read_to_string(&mut body)?;
                ensure!(body.len() <= 8 * 1024 * 1024, "Review message is too large");
                Ok(body)
            }
            _ => bail!("Specify exactly one of --body or --body-file"),
        }
    }
}

fn source_text(args: &Arguments) -> Result<(Rope, &'static Encoding)> {
    let mut file = match File::open(&args.source) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Rope::new(), helix_core::encoding::UTF_8))
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        file.metadata()?.len() <= 64 * 1024 * 1024,
        "Review source is too large"
    );
    let (text, encoding, _) = read_to_string(&mut file, None)?;
    ensure!(!text.contains('\0'), "Review source is binary");
    Ok((Rope::from_str(&text), encoding))
}

fn snapshot(args: &Arguments, text: &Rope, encoding: &'static Encoding) -> Result<ReviewSession> {
    let target = args.get("target").unwrap_or("HEAD");
    let mut controller = helix_event::TaskController::new();
    let cancel = controller.restart();
    // CLI operation never prompts or executes repository-local Git filters.
    let prepared = helix_vcs::DiffProviderRegistry::default().review_base(
        &args.source,
        target,
        false,
        &cancel,
    )?;
    let (base, _, _) = read_to_string(&mut prepared.bytes.as_slice(), Some(encoding))?;
    Ok(ReviewSession::new(
        target.into(),
        prepared.revision.branch,
        args.get("pr").map(str::to_owned),
        ReviewSnapshot {
            target_commit: Some(prepared.revision.target_commit),
            base_commit: Some(prepared.revision.base_commit),
            head_commit: Some(prepared.revision.head_commit),
            content_hash: content_hash(text.slice(..)),
            base_content_hash: Some(content_hash(Rope::from_str(&base).slice(..))),
        },
    ))
}

fn base_text(
    args: &Arguments,
    review: &ReviewSession,
    encoding: &'static Encoding,
) -> Result<Rope> {
    let commit = review
        .snapshot
        .base_commit
        .as_deref()
        .unwrap_or(&review.target);
    let mut controller = helix_event::TaskController::new();
    let cancel = controller.restart();
    let bytes = helix_vcs::DiffProviderRegistry::default().revision_file(
        &args.source,
        commit,
        false,
        &cancel,
    )?;
    let (text, _, _) = read_to_string(&mut bytes.as_slice(), Some(encoding))?;
    Ok(Rope::from_str(&text))
}

fn selected_review<'a>(args: &Arguments, store: &'a ReviewStore) -> Result<&'a ReviewSession> {
    match args.get("review") {
        Some(id) => store.session(id),
        None => store.active(),
    }
    .context("No matching review; use hx review start FILE --target REF first")
}

fn line_range(args: &Arguments, text: &Rope) -> Result<Range<usize>> {
    let line = usize::try_from(args.number("line")?)?;
    let end_line = args
        .get("end-line")
        .map(str::parse::<usize>)
        .transpose()?
        .unwrap_or(line);
    ensure!(
        line != 0 && line <= end_line && end_line <= text.len_lines(),
        "Review line range is outside the file"
    );
    let start_line = text.line(line - 1);
    let start_column = args
        .get("start-column")
        .map(str::parse::<usize>)
        .transpose()?
        .unwrap_or(1);
    ensure!(
        start_column != 0 && start_column <= start_line.len_chars() + 1,
        "Start column is outside the line"
    );
    let start = text.line_to_char(line - 1) + start_column - 1;
    let end = if let Some(column) = args.get("end-column") {
        let column: usize = column.parse()?;
        ensure!(
            column != 0 && column <= text.line(end_line - 1).len_chars() + 1,
            "End column is outside the line"
        );
        text.line_to_char(end_line - 1) + column - 1
    } else {
        text.line_to_char(end_line.min(text.len_lines()))
    };
    ensure!(start <= end, "Review range is reversed");
    Ok(start..end)
}

fn add(args: &Arguments) -> Result<Value> {
    let (text, encoding) = source_text(args)?;
    let body = args.body()?;
    ReviewStore::transaction(&args.source, |store| {
        let review = selected_review(args, store)?.clone();
        let hash = content_hash(text.slice(..));
        ensure!(
            hash == review.snapshot.content_hash,
            "Source changed since review start; start the review again before adding findings"
        );
        if let Some(expected) = args.get("expected-hash") {
            ensure!(
                hash == expected,
                "Reviewed file hash does not match --expected-hash"
            );
        }
        let mut controller = helix_event::TaskController::new();
        let cancel = controller.restart();
        let current = helix_vcs::DiffProviderRegistry::default().review_base(
            &args.source,
            "HEAD",
            false,
            &cancel,
        )?;
        ensure!(
            review.snapshot.head_commit.as_deref() == Some(current.revision.head_commit.as_str()),
            "HEAD changed since review start; start the review again before adding findings"
        );
        if let Some(expected) = args.get("expected-head") {
            ensure!(
                current.revision.head_commit == expected,
                "HEAD does not match --expected-head"
            );
        }
        let (side, selected_text) = match args.get("side").unwrap_or("current") {
            "current" => (CommentSide::Current, text.clone()),
            "base" => (CommentSide::Base, base_text(args, &review, encoding)?),
            _ => bail!("--side must be current or base"),
        };
        let range = line_range(args, &selected_text)?;
        if let Some(quote) = args.get("quote") {
            ensure!(
                selected_text
                    .slice(range.clone())
                    .to_string()
                    .contains(quote),
                "Quoted code does not match the selected range"
            );
        }
        let anchor = CommentAnchor::new(
            side,
            (side == CommentSide::Base).then(|| review.target.clone()),
            selected_text.slice(..),
            range,
        );
        ensure!(
            content_hash(source_text(args)?.0.slice(..)) == hash,
            "Source changed while preparing the finding; retry"
        );
        let id = store.add(&review.id, anchor, args.author(), body)?;
        Ok(json!({ "review_id": review.id, "thread": store.thread(id) }))
    })
}

fn list(args: &Arguments) -> Result<Value> {
    let mut store = ReviewStore::default();
    store.load(&args.source)?;
    ensure!(
        args.get("review").is_none() || args.get("all").is_none(),
        "Use --review or --all"
    );
    let selected = if args.get("all").is_some()
        || (args.get("review").is_none() && store.active().is_none())
    {
        None
    } else {
        Some(selected_review(args, &store)?.id.clone())
    };
    let (text, encoding) = source_text(args)?;
    let mut bases = BTreeMap::new();
    let mut threads = Vec::new();
    for thread in &store.data().threads {
        if selected.as_ref().is_some_and(|id| *id != thread.review_id) {
            continue;
        }
        let review = store.session(&thread.review_id).unwrap();
        let located = match thread.anchor.side {
            CommentSide::Current => thread.anchor.locate(text.slice(..)),
            CommentSide::Base => {
                if !bases.contains_key(&review.id) {
                    bases.insert(review.id.clone(), base_text(args, review, encoding).ok());
                }
                bases[&review.id]
                    .as_ref()
                    .and_then(|base| thread.anchor.locate(base.slice(..)))
            }
        };
        let selected_text = if thread.anchor.side == CommentSide::Current {
            Some(&text)
        } else {
            bases.get(&review.id).and_then(Option::as_ref)
        };
        let location = located.as_ref().zip(selected_text).map(|(range, text)| {
            let start_line = text.char_to_line(range.start);
            let end_pos = range.end.saturating_sub(usize::from(!range.is_empty()));
            let end_line = text.char_to_line(end_pos);
            json!({ "range": range, "start_line": start_line + 1, "end_line": end_line + 1, "start_column": range.start - text.line_to_char(start_line) + 1 })
        });
        threads.push(json!({
            "thread": thread,
            "status": if thread.resolved { "resolved" } else if located.is_none() { "outdated" } else { "open" },
            "relocated": located.as_ref().is_some_and(|range| *range != thread.original_anchor.range),
            "location": location,
        }));
    }
    Ok(
        json!({ "source": args.source, "active_review": store.data().active_review, "reviews": store.data().reviews, "threads": threads }),
    )
}

/// Execute already separated `hx review ...` arguments and return JSON.
pub fn execute(args: impl Iterator<Item = String>) -> Result<Value> {
    let args = Arguments::parse(args)?;
    match args.command.as_str() {
        "start" => {
            let (text, encoding) = source_text(&args)?;
            let session = snapshot(&args, &text, encoding)?;
            ReviewStore::transaction(&args.source, |store| {
                let id = store.select(session);
                Ok(json!({ "source": args.source, "review": store.session(&id) }))
            })
        }
        "list" => list(&args),
        "add" => add(&args),
        "reply" | "edit" | "resolve" | "reopen" | "remove" => {
            let body = matches!(args.command.as_str(), "reply" | "edit")
                .then(|| args.body())
                .transpose()?;
            ReviewStore::transaction(&args.source, |store| match args.command.as_str() {
                "reply" => {
                    let thread = args.number("thread")?;
                    let id = store.reply(thread, args.author(), body.unwrap())?;
                    Ok(json!({ "message_id": id, "thread": store.thread(thread) }))
                }
                "edit" => {
                    let id = args.number("message")?;
                    ensure!(
                        store.set_text(id, body.unwrap()),
                        "Unknown review message {id}"
                    );
                    Ok(json!({ "thread": store.message_thread(id) }))
                }
                "resolve" | "reopen" => {
                    let id = args.number("thread")?;
                    store.resolve(id, args.command == "resolve")?;
                    Ok(json!({ "thread": store.thread(id) }))
                }
                "remove" => {
                    let id = args.number("message")?;
                    store.remove(id)?;
                    Ok(json!({ "removed_message": id }))
                }
                _ => unreachable!(),
            })
        }
        _ => unreachable!(),
    }
}

pub fn run(args: Vec<String>) -> Result<i32> {
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
    {
        print!("{HELP}");
    } else {
        let value = execute(args.into_iter())?;
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(0)
}

#[cfg(all(test, feature = "git"))]
mod tests;
