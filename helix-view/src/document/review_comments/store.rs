//! Shared sidecar transactions for the editor and non-interactive review tools.

use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, ensure, Context, Result};
use helix_core::RopeSlice;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{sidecar_path, CommentAnchor, ReviewComment};

const MAX_SIDECAR_BYTES: u64 = 8 * 1024 * 1024;

/// SHA-256 of the UTF-8 editor text, independent of rope chunk boundaries.
pub fn content_hash(text: RopeSlice<'_>) -> String {
    let mut hash = Sha256::new();
    for chunk in text.chunks() {
        hash.update(chunk.as_bytes());
    }
    hex(&hash.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}

pub fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn default_author() -> String {
    std::env::var("HELIX_REVIEW_AUTHOR")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".into())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    pub content_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_content_hash: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSession {
    pub id: String,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
    pub snapshot: ReviewSnapshot,
    pub created_at: u64,
    pub updated_at: u64,
}

impl ReviewSession {
    pub fn new(
        target: String,
        branch: Option<String>,
        pr: Option<String>,
        snapshot: ReviewSnapshot,
    ) -> Self {
        // A review survives new commits, but is isolated from other branches,
        // targets and PRs. The same identity is usable across file sidecars.
        let detached = branch
            .is_none()
            .then_some(snapshot.head_commit.as_deref())
            .flatten();
        let identity = if pr.is_some() {
            serde_json::to_vec(&(&target, &pr)).unwrap()
        } else {
            serde_json::to_vec(&(&target, &branch, detached)).unwrap()
        };
        let id = hex(&Sha256::digest(identity));
        let now = timestamp();
        Self {
            id,
            target,
            branch,
            pr,
            snapshot,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewMessage {
    pub id: u64,
    pub author: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewThread {
    pub id: u64,
    pub review_id: String,
    /// Position in the current file, updated separately from the original.
    pub anchor: CommentAnchor,
    pub original_anchor: CommentAnchor,
    /// Immutable snapshot against which the first message was written.
    pub snapshot: ReviewSnapshot,
    pub resolved: bool,
    pub messages: Vec<ReviewMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewSidecar {
    pub version: u32,
    pub next_id: u64,
    pub active_review: Option<String>,
    pub reviews: Vec<ReviewSession>,
    pub threads: Vec<ReviewThread>,
}

impl Default for ReviewSidecar {
    fn default() -> Self {
        Self {
            version: 2,
            next_id: 1,
            active_review: None,
            reviews: Vec::new(),
            threads: Vec::new(),
        }
    }
}

impl ReviewSidecar {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        #[derive(Deserialize)]
        struct Version {
            version: u32,
        }
        let version = serde_json::from_slice::<Version>(bytes)
            .context("Invalid review sidecar")?
            .version;
        let sidecar = match version {
            1 => {
                #[derive(Deserialize)]
                struct Legacy {
                    comments: Vec<ReviewComment>,
                }
                let legacy: Legacy = serde_json::from_slice(bytes)?;
                let mut sidecar = Self::default();
                for comment in legacy.comments {
                    let target = comment.anchor.reference.as_deref().unwrap_or("HEAD");
                    let session =
                        ReviewSession::new(target.into(), None, None, ReviewSnapshot::default());
                    if !sidecar.reviews.iter().any(|review| review.id == session.id) {
                        sidecar.reviews.push(session.clone());
                    }
                    sidecar.next_id = sidecar
                        .next_id
                        .max(comment.id.checked_add(1).context("Review IDs exhausted")?);
                    sidecar.threads.push(ReviewThread {
                        id: comment.id,
                        review_id: session.id,
                        original_anchor: comment.anchor.clone(),
                        anchor: comment.anchor,
                        snapshot: ReviewSnapshot::default(),
                        resolved: false,
                        messages: vec![ReviewMessage {
                            id: comment.id,
                            author: "unknown".into(),
                            created_at: 0,
                            updated_at: 0,
                            text: comment.text,
                        }],
                    });
                }
                sidecar.active_review = sidecar
                    .reviews
                    .iter()
                    .find(|review| review.target == "HEAD")
                    .or_else(|| sidecar.reviews.first())
                    .map(|review| review.id.clone());
                sidecar
            }
            2 => serde_json::from_slice(bytes).context("Invalid review sidecar")?,
            _ => bail!("Unsupported review sidecar version {version}"),
        };
        sidecar.validate()?;
        Ok(sidecar)
    }

    fn validate(&self) -> Result<()> {
        let mut reviews = HashSet::new();
        ensure!(
            self.reviews
                .iter()
                .all(|review| !review.id.is_empty() && reviews.insert(&review.id)),
            "Invalid or duplicate reviews"
        );
        ensure!(
            self.active_review
                .as_ref()
                .is_none_or(|id| reviews.contains(id)),
            "Unknown active review"
        );
        let mut threads = HashSet::new();
        let mut messages = HashSet::new();
        for thread in &self.threads {
            ensure!(
                thread.id != 0 && threads.insert(thread.id) && reviews.contains(&thread.review_id),
                "Invalid or duplicate review threads"
            );
            ensure!(
                !thread.messages.is_empty()
                    && thread.anchor.range.start <= thread.anchor.range.end
                    && thread.original_anchor.range.start <= thread.original_anchor.range.end,
                "Invalid review thread anchor"
            );
            for message in &thread.messages {
                ensure!(
                    message.id != 0 && message.id < self.next_id && messages.insert(message.id),
                    "Invalid or duplicate review message IDs"
                );
            }
        }
        Ok(())
    }
}

/// Readers retain their last bytes; writers lock, compare, then atomically replace.
/// CLI transactions take the same lock before reading, so concurrent additions
/// allocate distinct IDs and preserve each other's threads.
#[derive(Clone, Default)]
pub struct ReviewStore {
    pub(crate) data: ReviewSidecar,
    pub(crate) comments: Vec<ReviewComment>,
    pub(crate) generation: u64,
    pub(crate) path: Option<PathBuf>,
    saved: Option<Vec<u8>>,
    stamp: Option<(SystemTime, u64)>,
    pub(crate) dirty: bool,
    pub(crate) error: Option<String>,
}

pub fn read_sidecar(path: &Path) -> Result<Option<Vec<u8>>> {
    match File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(MAX_SIDECAR_BYTES + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_SIDECAR_BYTES,
                "Review sidecar is too large"
            );
            Ok(Some(bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn lock(path: &Path, wait: bool) -> Result<File> {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(name))?;
    if wait {
        file.lock().context("Cannot lock review sidecar")?;
    } else {
        file.try_lock()
            .context("Another review writer is saving this sidecar; retry the operation")?;
    }
    Ok(file)
}

fn file_stamp(path: &Path) -> Result<Option<(SystemTime, u64)>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some((metadata.modified()?, metadata.len()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl ReviewStore {
    pub fn data(&self) -> &ReviewSidecar {
        &self.data
    }

    pub fn load(&mut self, source: &Path) -> Result<bool> {
        let path = sidecar_path(source);
        let result = (|| {
            let stamp = file_stamp(&path)?;
            let bytes =
                read_sidecar(&path).with_context(|| format!("Cannot read {}", path.display()))?;
            if self.path.as_ref() == Some(&path) {
                if bytes == self.saved {
                    self.stamp = stamp;
                    return Ok(false);
                }
                ensure!(!self.dirty, "Review sidecar changed outside Helix; finish or discard the pending comment first");
            }
            self.data = bytes
                .as_deref()
                .map(ReviewSidecar::from_bytes)
                .transpose()?
                .unwrap_or_default();
            self.path = Some(path);
            self.saved = bytes;
            self.stamp = stamp;
            self.dirty = false;
            self.generation = self.generation.wrapping_add(1);
            self.rebuild_comments();
            Ok(true)
        })();
        self.error = result.as_ref().err().map(|error| format!("{error:#}"));
        result
    }

    pub fn save(&mut self, source: &Path) -> Result<()> {
        let path = sidecar_path(source);
        let _guard = lock(&path, false)?;
        self.save_locked(&path)
    }

    fn save_locked(&mut self, path: &Path) -> Result<()> {
        if let Some(error) = &self.error {
            bail!("{error}");
        }
        ensure!(
            self.path.as_deref() == Some(path),
            "Source path changed while editing a comment"
        );
        ensure!(
            read_sidecar(path)? == self.saved,
            "Review sidecar changed outside Helix; refusing to overwrite its comments"
        );
        self.data.validate()?;
        if self.data.reviews.is_empty() && self.data.threads.is_empty() {
            if self.saved.is_some() {
                std::fs::remove_file(path)?;
            }
            self.saved = None;
        } else {
            let mut bytes = serde_json::to_vec_pretty(&self.data)?;
            bytes.push(b'\n');
            ensure!(
                bytes.len() as u64 <= MAX_SIDECAR_BYTES,
                "Review sidecar is too large"
            );
            let mut file = tempfile::NamedTempFile::new_in(
                path.parent().context("Missing sidecar directory")?,
            )?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.persist(path).map_err(|error| error.error)?;
            self.saved = Some(bytes);
        }
        self.dirty = false;
        self.stamp = file_stamp(path)?;
        Ok(())
    }

    pub fn externally_changed(&self) -> Result<bool> {
        self.path
            .as_deref()
            .map(|path| file_stamp(path).map(|stamp| stamp != self.stamp))
            .transpose()
            .map(|changed| changed.unwrap_or(false))
    }

    pub fn transaction<T>(source: &Path, edit: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let path = sidecar_path(source);
        let _guard = lock(&path, true)?;
        let mut store = Self::default();
        store.load(source)?;
        let result = edit(&mut store)?;
        if store.dirty {
            store.save_locked(&path)?;
        }
        Ok(result)
    }

    pub fn active(&self) -> Option<&ReviewSession> {
        self.data
            .active_review
            .as_ref()
            .and_then(|id| self.session(id))
    }

    pub fn discard(&mut self, source: &Path) -> Result<()> {
        self.dirty = false;
        self.error = None;
        self.path = None;
        self.load(source)?;
        Ok(())
    }

    pub fn session(&self, id: &str) -> Option<&ReviewSession> {
        self.data.reviews.iter().find(|review| review.id == id)
    }

    pub fn select(&mut self, mut session: ReviewSession) -> String {
        let existing = self.data.reviews.iter_mut().find(|review| {
            review.id == session.id
                || (review.target == session.target
                    && review.pr == session.pr
                    && review.branch.is_none()
                    && review.snapshot.head_commit.is_none())
        });
        if let Some(existing) = existing {
            session.id = existing.id.clone();
            session.created_at = existing.created_at;
            if existing.snapshot != session.snapshot || existing.branch != session.branch {
                *existing = session.clone();
                self.changed();
            }
        } else {
            self.data.reviews.push(session.clone());
            self.changed();
        }
        if self.data.active_review.as_deref() != Some(&session.id) {
            self.data.active_review = Some(session.id.clone());
            self.changed();
        }
        session.id
    }

    pub fn activate(&mut self, id: &str) -> Result<()> {
        ensure!(self.session(id).is_some(), "Unknown review {id}");
        if self.data.active_review.as_deref() != Some(id) {
            self.data.active_review = Some(id.into());
            self.changed();
        }
        Ok(())
    }

    pub fn thread(&self, id: u64) -> Option<&ReviewThread> {
        self.data.threads.iter().find(|thread| thread.id == id)
    }

    pub fn message_thread(&self, id: u64) -> Option<&ReviewThread> {
        self.data
            .threads
            .iter()
            .find(|thread| thread.messages.iter().any(|message| message.id == id))
    }

    fn allocate_id(&mut self) -> Result<u64> {
        let id = self.data.next_id;
        self.data.next_id = id.checked_add(1).context("Review IDs exhausted")?;
        Ok(id)
    }

    pub fn add(
        &mut self,
        review_id: &str,
        anchor: CommentAnchor,
        author: String,
        text: String,
    ) -> Result<u64> {
        let snapshot = self
            .session(review_id)
            .context("Review session is missing")?
            .snapshot
            .clone();
        let id = self.allocate_id()?;
        let now = timestamp();
        self.data.threads.push(ReviewThread {
            id,
            review_id: review_id.into(),
            original_anchor: anchor.clone(),
            anchor,
            snapshot,
            resolved: false,
            messages: vec![ReviewMessage {
                id,
                author,
                created_at: now,
                updated_at: now,
                text,
            }],
        });
        self.changed();
        self.rebuild_comments();
        Ok(id)
    }

    pub fn reply(&mut self, thread_id: u64, author: String, text: String) -> Result<u64> {
        ensure!(
            self.thread(thread_id).is_some(),
            "Unknown review thread {thread_id}"
        );
        let id = self.allocate_id()?;
        let now = timestamp();
        let thread = self
            .data
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
            .unwrap();
        thread.messages.push(ReviewMessage {
            id,
            author,
            created_at: now,
            updated_at: now,
            text,
        });
        thread.resolved = false;
        self.changed();
        self.rebuild_comments();
        Ok(id)
    }

    pub fn set_text(&mut self, id: u64, text: String) -> bool {
        let Some(message) = self
            .data
            .threads
            .iter_mut()
            .flat_map(|thread| &mut thread.messages)
            .find(|message| message.id == id)
        else {
            return false;
        };
        if message.text != text {
            message.text.clone_from(&text);
            message.updated_at = timestamp();
            if let Some(comment) = self.comments.iter_mut().find(|comment| comment.id == id) {
                comment.text = text;
            }
            self.changed();
        }
        true
    }

    pub fn resolve(&mut self, id: u64, resolved: bool) -> Result<()> {
        let thread = self
            .data
            .threads
            .iter_mut()
            .find(|thread| thread.id == id)
            .context("Unknown review thread")?;
        if thread.resolved != resolved {
            thread.resolved = resolved;
            self.changed();
        }
        Ok(())
    }

    pub fn remove(&mut self, id: u64) -> Result<()> {
        ensure!(
            self.message_thread(id).is_some(),
            "Review comment no longer exists"
        );
        for thread in &mut self.data.threads {
            thread.messages.retain(|message| message.id != id);
        }
        self.data
            .threads
            .retain(|thread| !thread.messages.is_empty());
        if self.data.threads.is_empty() {
            self.data.reviews.clear();
            self.data.active_review = None;
        }
        self.changed();
        self.rebuild_comments();
        Ok(())
    }

    pub(crate) fn changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.dirty = true;
    }

    pub(crate) fn rebuild_comments(&mut self) {
        self.comments = self
            .data
            .threads
            .iter()
            .flat_map(|thread| {
                thread.messages.iter().map(|message| ReviewComment {
                    id: message.id,
                    anchor: thread.anchor.clone(),
                    text: message.text.clone(),
                })
            })
            .collect();
    }
}
