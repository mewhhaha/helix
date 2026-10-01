//! Indexing of words from open buffers.
//!
//! This provides an eventually consistent set of words used in any open buffers. This set is
//! later used for lexical completion.

use std::{borrow::Cow, collections::VecDeque, iter, sync::Arc, time::Duration};

use foldhash::HashMap;
use futures_util::{stream::FuturesUnordered, StreamExt};
use helix_core::{
    chars::char_is_word, diff::compare_ropes, fuzzy::fuzzy_match_parallel, ChangeSet, Rope,
    RopeSlice,
};
use helix_event::{register_hook, AsyncHook, TaskController, TaskHandle};
use helix_stdx::rope::RopeSliceExt as _;
use parking_lot::{Mutex, RwLock};
use tokio::{
    sync::{mpsc, Notify},
    time::Instant,
};

use crate::{
    events::{ConfigDidChange, DocumentDidChange, DocumentDidClose, DocumentDidOpen},
    DocumentId,
};

use super::Handlers;

#[derive(Debug)]
struct Change {
    old_text: Rope,
    text: Rope,
    changes: ChangeSet,
    /// Whether `changes` is stale and must be recomputed from `old_text`/`text` before use.
    ///
    /// Set when this change is the result of coalescing several observed changes (see
    /// [`Hook::handle_event`]). The observed changesets cannot be reliably chained, so the
    /// coalesced changeset is recomputed once, lazily, at [`Hook::finish_debounce`].
    dirty: bool,
    generation: u64,
}

#[derive(Debug)]
enum Event {
    Insert(DocumentId, Rope),
    Update(DocumentId, Change),
    Delete(DocumentId, Rope),
    /// Clear the entire word index.
    /// This is used to clear memory when the feature is turned off.
    Clear,
}

/// At most one pending revision per open document. Sending a newer revision
/// cancels its active preparation before replacing the pending work.
#[derive(Debug, Default, Clone)]
struct Coordinator {
    pending: Arc<Mutex<Pending>>,
    notify: Arc<Notify>,
}

#[derive(Debug, Default)]
struct Pending {
    events: HashMap<DocumentId, Event>,
    order: VecDeque<DocumentId>,
    clear: bool,
    active: HashMap<DocumentId, TaskController>,
    generation: u64,
}

fn send(coordinator: &Coordinator, event: Event) {
    let mut pending = coordinator.pending.lock();
    if matches!(&event, Event::Update(_, change) if change.generation != pending.generation) {
        return;
    }
    let doc = match &event {
        Event::Insert(doc, _) | Event::Update(doc, _) | Event::Delete(doc, _) => *doc,
        Event::Clear => {
            pending.generation = pending.generation.wrapping_add(1);
            pending.clear = true;
            pending.events.clear();
            pending.order.clear();
            for controller in pending.active.values_mut() {
                controller.cancel();
            }
            coordinator.notify.notify_one();
            return;
        }
    };
    if let Some(controller) = pending.active.get_mut(&doc) {
        controller.cancel();
    }
    if !pending.events.contains_key(&doc) {
        pending.order.push_back(doc);
    }
    pending.events.insert(doc, event);
    coordinator.notify.notify_one();
}

#[derive(Debug)]
pub struct Handler {
    pub(super) index: WordIndex,
    /// A sender into an async hook which debounces updates to the index.
    hook: mpsc::Sender<Event>,
    /// A sender to a tokio task which coordinates the indexing of documents.
    ///
    /// See [WordIndex::run]. A supervisor-like task is in charge of spawning tasks to update the
    /// index. This ensures that consecutive edits to a document trigger the correct order of
    /// insertions and deletions into the word set.
    coordinator: Coordinator,
    /// Cancels in-flight indexing when the handler is dropped.
    ///
    /// Indexing a large document runs on a blocking task which cannot be preempted. Without this,
    /// dropping the tokio runtime on shutdown would block until that task finishes, keeping the
    /// process alive and unresponsive. The indexing task holds a [TaskHandle] from this
    /// controller and checks it periodically.
    _cancel: TaskController,
}

impl Handler {
    pub fn spawn() -> Self {
        let index = WordIndex::default();
        let coordinator = Coordinator::default();
        let mut cancel = TaskController::new();
        tokio::spawn(index.clone().run(coordinator.clone(), cancel.restart()));
        Self {
            hook: Hook {
                changes: HashMap::default(),
                coordinator: coordinator.clone(),
                generation: 0,
            }
            .spawn(),
            index,
            coordinator,
            _cancel: cancel,
        }
    }
}

#[derive(Debug)]
struct Hook {
    changes: HashMap<DocumentId, Change>,
    coordinator: Coordinator,
    generation: u64,
}

impl Hook {
    fn sync_generation(&mut self) {
        let generation = self.coordinator.pending.lock().generation;
        if self.generation != generation {
            self.changes.clear();
            self.generation = generation;
        }
    }
}

const DEBOUNCE: Duration = Duration::from_secs(1);

impl AsyncHook for Hook {
    type Event = Event;

    fn handle_event(&mut self, event: Self::Event, timeout: Option<Instant>) -> Option<Instant> {
        self.sync_generation();
        match event {
            Event::Insert(_, _) => unreachable!("inserts are sent to the worker directly"),
            Event::Update(doc, change) => {
                if change.generation != self.generation {
                    return timeout;
                }
                if let Some(pending_change) = self.changes.get_mut(&doc) {
                    // There is already a change waiting for this document. Coalesce: keep the
                    // original `old_text` and advance to the latest `text`.
                    //
                    // We deliberately do NOT chain the observed changesets here. The index skips
                    // ghost transactions (see `register_hooks`), so a ghost edit may have altered
                    // the document between these two observed changes, which may leave them
                    // non-contiguous in ways a cheap check can't catch. Instead, mark the change
                    // dirty and recompute the changeset from the real ropes at `finish_debounce`,
                    // i.e.  once per debounce window rather than once per keystroke.
                    pending_change.text = change.text;
                    pending_change.dirty = true;
                    Some(Instant::now() + DEBOUNCE)
                } else if !is_changeset_significant(&change.changes) {
                    // The change is small: debounce so a burst of edits coalesces before the
                    // index updates.
                    self.changes.insert(doc, change);
                    Some(Instant::now() + DEBOUNCE)
                } else {
                    // The change is large: update the index immediately rather than waiting out
                    // the debounce.
                    send(&self.coordinator, Event::Update(doc, change));
                    timeout
                }
            }
            Event::Delete(doc, text) => {
                // If there are pending changes that haven't been indexed since the last debounce,
                // forget them and delete the old text.
                if let Some(change) = self.changes.remove(&doc) {
                    send(&self.coordinator, Event::Delete(doc, change.old_text));
                } else {
                    send(&self.coordinator, Event::Delete(doc, text));
                }
                timeout
            }
            Event::Clear => unreachable!("clear is sent to the worker directly"),
        }
    }

    fn finish_debounce(&mut self) {
        self.sync_generation();
        // Recomputing a coalesced diff belongs to blocking preparation, not the
        // Tokio worker running this hook.
        for (doc, change) in self.changes.drain() {
            send(&self.coordinator, Event::Update(doc, change));
        }
    }
}

/// Minimum number of grapheme clusters required to include a word in the index
const MIN_WORD_GRAPHEMES: usize = 3;
/// Maximum word length allowed (in chars)
const MAX_WORD_LEN: usize = 50;
/// Number of words to index between checks of the cancellation handle.
const CANCEL_CHECK_INTERVAL: usize = 4096;

type Word = kstring::KString;

#[derive(Debug, Default)]
struct WordIndexInner {
    /// Reference counted storage for words.
    ///
    /// Words are very likely to be reused many times. Instead of storing duplicates we keep a
    /// reference count of times a word is used. When the reference count drops to zero the word
    /// is removed from the index.
    words: HashMap<Word, u32>,
    generation: u64,
    snapshot: std::sync::OnceLock<Arc<Vec<Word>>>,
}

impl WordIndexInner {
    fn clear(&mut self) {
        std::mem::take(&mut self.words);
        self.generation = self.generation.wrapping_add(1);
        self.snapshot.take();
    }
}

#[derive(Debug, Default, Clone)]
pub struct WordIndex {
    inner: Arc<RwLock<WordIndexInner>>,
    candidates: Arc<Mutex<Option<CandidateCache>>>,
}

#[derive(Debug)]
struct CandidateCache {
    generation: u64,
    pattern: String,
    words: Arc<Vec<Word>>,
}

impl CandidateCache {
    fn can_refine(&self, pattern: &str) -> bool {
        pattern == self.pattern
            || (pattern.starts_with(&self.pattern)
                && pattern
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    }
}

type WordDelta = HashMap<Word, i64>;

impl WordIndex {
    pub fn matches(&self, pattern: &str) -> Vec<String> {
        self.matches_with_cancel(pattern, || false)
    }

    pub fn matches_cancelable(&self, pattern: &str, cancel: &TaskHandle) -> Vec<String> {
        self.matches_with_cancel(pattern, || cancel.is_canceled())
    }

    fn matches_with_cancel(
        &self,
        pattern: &str,
        canceled: impl Fn() -> bool + Sync,
    ) -> Vec<String> {
        if canceled() {
            return Vec::new();
        }
        // Only copying cheap, shared word handles happens under the index lock.
        // Matching and sorting must not stall the background index writer.
        let (generation, words) = {
            let inner = self.inner.read();
            let cached = self.candidates.lock();
            let words = cached
                .as_ref()
                .filter(|cache| cache.generation == inner.generation && cache.can_refine(pattern))
                .map(|cache| cache.words.clone())
                .unwrap_or_else(|| {
                    inner
                        .snapshot
                        .get_or_init(|| Arc::new(inner.words.keys().cloned().collect()))
                        .clone()
                });
            (inner.generation, words)
        };
        let Some(mut matches) = fuzzy_match_parallel(pattern, &words, &canceled) else {
            return Vec::new();
        };
        if matches.len() <= 32_768
            && matches
                .iter()
                .map(|(word, _)| word.len() + std::mem::size_of::<Word>())
                .sum::<usize>()
                <= 2 * 1024 * 1024
        {
            // Publishing an obsolete snapshot is harmless: its generation will
            // never match the current index. Canceled scans publish nothing.
            *self.candidates.lock() = Some(CandidateCache {
                generation,
                pattern: pattern.into(),
                words: Arc::new(matches.iter().map(|(word, _)| (*word).clone()).collect()),
            });
        }
        // Sort in bounded chunks so cancellation also interrupts ordering a
        // large result set. Merge them in score order while building labels.
        const CHUNK: usize = 256;
        for chunk in matches.chunks_mut(CHUNK) {
            if canceled() {
                return Vec::new();
            }
            chunk.sort_unstable_by_key(|(_, score)| *score);
        }
        let mut heap = std::collections::BinaryHeap::new();
        for (chunk, items) in matches.chunks(CHUNK).enumerate() {
            if let Some((_, score)) = items.first() {
                heap.push(std::cmp::Reverse((*score, chunk * CHUNK)));
            }
        }
        let mut result = Vec::with_capacity(matches.len());
        while let Some(std::cmp::Reverse((_, index))) = heap.pop() {
            if result.len() % 64 == 0 && canceled() {
                return Vec::new();
            }
            result.push(matches[index].0.to_string());
            let next = index + 1;
            if next < matches.len() && next % CHUNK != 0 {
                heap.push(std::cmp::Reverse((matches[next].1, next)));
            }
        }
        result
    }

    fn stage_words(
        delta: &mut WordDelta,
        text: RopeSlice,
        amount: i64,
        cancel: &TaskHandle,
    ) -> bool {
        for word in words_with_cancel(text, || cancel.is_canceled()) {
            let word: Cow<str> = word.into();
            let word = match word {
                Cow::Owned(s) => Word::from_string(s),
                Cow::Borrowed(s) => Word::from_ref(s),
            };
            *delta.entry(word).or_default() += amount;
        }
        !cancel.is_canceled()
    }

    fn prepare_delta(
        old: Option<&Rope>,
        text: Option<&Rope>,
        changes: Option<&ChangeSet>,
        cancel: &TaskHandle,
    ) -> Option<WordDelta> {
        let mut delta = WordDelta::default();
        match (old, text) {
            (Some(old), Some(text)) => {
                let calculated;
                let changes = if let Some(changes) = changes {
                    changes
                } else {
                    calculated = compare_ropes(old, text);
                    calculated.changes()
                };
                if cancel.is_canceled() {
                    return None;
                }
                for (old_window, new_window) in
                    changed_windows(old.slice(..), text.slice(..), changes)
                {
                    if !Self::stage_words(&mut delta, new_window, 1, cancel)
                        || !Self::stage_words(&mut delta, old_window, -1, cancel)
                    {
                        return None;
                    }
                }
            }
            (None, Some(text)) => {
                if !Self::stage_words(&mut delta, text.slice(..), 1, cancel) {
                    return None;
                }
            }
            (Some(old), None) => {
                if !Self::stage_words(&mut delta, old.slice(..), -1, cancel) {
                    return None;
                }
            }
            (None, None) => (),
        }
        (!cancel.is_canceled()).then_some(delta)
    }

    fn apply_delta(&self, delta: WordDelta) {
        let mut inner = self.inner.write();
        let mut changed = false;
        for (word, difference) in delta {
            if difference == 0 {
                continue;
            }
            let count = (i64::from(*inner.words.get(&word).unwrap_or(&0)) + difference)
                .clamp(0, u32::MAX as i64) as u32;
            if count == 0 {
                changed |= inner.words.remove(&word).is_some();
            } else {
                changed |= inner.words.insert(word, count).is_none();
            }
        }
        if changed {
            inner.generation = inner.generation.wrapping_add(1);
            inner.snapshot.take();
        }
    }

    #[cfg(any(test, feature = "bench"))]
    fn add_document(&self, text: &Rope, cancel: &TaskHandle) {
        if let Some(delta) = Self::prepare_delta(None, Some(text), None, cancel) {
            self.apply_delta(delta);
        }
    }

    #[cfg(test)]
    fn update_document(
        &self,
        old_text: &Rope,
        text: &Rope,
        changes: &ChangeSet,
        cancel: &TaskHandle,
    ) {
        if let Some(delta) = Self::prepare_delta(Some(old_text), Some(text), Some(changes), cancel)
        {
            self.apply_delta(delta);
        }
    }

    fn clear(&self) {
        self.inner.write().clear();
        self.candidates.lock().take();
    }

    async fn run(self, coordinator: Coordinator, cancel: TaskHandle) {
        // These are the revisions actually committed to the index, rather than
        // the event's old text (which can contain skipped completion previews).
        let mut indexed = HashMap::<DocumentId, Rope>::default();
        let mut preparing = FuturesUnordered::new();
        loop {
            if cancel.is_canceled() {
                coordinator.pending.lock().active.clear();
                return;
            }
            let clear = {
                let mut pending = coordinator.pending.lock();
                std::mem::take(&mut pending.clear)
            };
            if clear {
                let previous = std::mem::take(&mut indexed);
                let this = self.clone();
                helix_event::spawn_cpu(move || {
                    this.clear();
                    drop(previous);
                })
                .await;
                continue;
            }
            while preparing.len() < helix_stdx::cpu::worker_count() {
                let next = {
                    let mut pending = coordinator.pending.lock();
                    // One preparation per document. Other documents may bypass
                    // a queued revision which is waiting for its canceled work.
                    let position = (!pending.clear)
                        .then(|| {
                            pending
                                .order
                                .iter()
                                .position(|doc| !pending.active.contains_key(doc))
                        })
                        .flatten();
                    position.map(|position| {
                        let doc = pending.order.remove(position).unwrap();
                        let event = pending.events.remove(&doc).unwrap();
                        let mut controller = TaskController::new();
                        let handle = controller.restart();
                        pending.active.insert(doc, controller);
                        (doc, event, handle)
                    })
                };
                let Some((doc, event, handle)) = next else {
                    break;
                };
                let old = indexed.get(&doc).cloned();
                let (text, changes) = match event {
                    Event::Insert(_, text) => (Some(text), None),
                    Event::Update(_, change) => {
                        let changes = (!change.dirty
                            && old
                                .as_ref()
                                .is_some_and(|old| old.is_instance(&change.old_text)))
                        .then_some(change.changes);
                        (Some(change.text), changes)
                    }
                    Event::Delete(_, _) => (None, None),
                    Event::Clear => unreachable!(),
                };
                let work_text = text.clone();
                let work_handle = handle.clone();
                let shutdown = cancel.clone();
                let task = helix_event::spawn_cpu(move || {
                    if shutdown.is_canceled() || work_handle.is_canceled() {
                        return None;
                    }
                    Self::prepare_delta(
                        old.as_ref(),
                        work_text.as_ref(),
                        changes.as_ref(),
                        &work_handle,
                    )
                });
                preparing.push(async move {
                    // Retain the document identity even if preparation panics.
                    let result = tokio::spawn(task).await;
                    (doc, text, handle, result)
                });
            }
            let (doc, text, handle, result) = tokio::select! {
                biased;
                _ = cancel.canceled() => {
                    coordinator.pending.lock().active.clear();
                    return;
                }
                Some(prepared) = preparing.next(), if !preparing.is_empty() => prepared,
                _ = coordinator.notify.notified() => continue,
            };
            // Accept the complete delta atomically with cancellation. Commit it
            // in full before scheduling this document's next revision. Prepared
            // deltas for other documents can finish in any order.
            let accepted = {
                let mut pending = coordinator.pending.lock();
                let accepted = if !handle.is_canceled() && !cancel.is_canceled() {
                    match result {
                        Ok(delta) => delta,
                        Err(error) => {
                            log::error!("word indexing task failed: {error}");
                            None
                        }
                    }
                } else {
                    None
                };
                pending.active.remove(&doc);
                accepted
            };
            if let Some(delta) = accepted {
                let this = self.clone();
                helix_event::spawn_cpu(move || this.apply_delta(delta)).await;
                if let Some(text) = text {
                    indexed.insert(doc, text);
                } else {
                    indexed.remove(&doc);
                }
            }
        }
    }
}

/// Extracts indexable words from a rope slice.
///
/// A word is a run of grapheme clusters whose first character is a
/// w[word character][char_is_word], spanning at least [`MIN_WORD_GRAPHEMES`] clusters and at
/// most [`MAX_WORD_LEN`] chars. All other text is skipped.
///
/// ASCII slices use byte runs; Unicode slices retain a forward grapheme scan.
/// Only emitted words seek into the rope, keeping extraction roughly linear.
#[cfg(any(test, feature = "bench"))]
fn words(text: RopeSlice) -> impl Iterator<Item = RopeSlice> {
    words_with_cancel(text, || false)
}

fn words_with_cancel(
    text: RopeSlice<'_>,
    mut is_canceled: impl FnMut() -> bool,
) -> impl Iterator<Item = RopeSlice<'_>> {
    // Validate the entire slice before using byte boundaries: an ASCII run can
    // belong to a Unicode grapheme whose combining marks are in the next chunk.
    let mut ascii = Some(true);
    let mut checked = CANCEL_CHECK_INTERVAL;
    for chunk in text.chunks() {
        if checked >= CANCEL_CHECK_INTERVAL {
            if is_canceled() {
                ascii = None;
                break;
            }
            checked = 0;
        }
        if !chunk.is_ascii() {
            ascii = Some(false);
            break;
        }
        checked += chunk.len();
    }
    match ascii {
        Some(true) => Words::Ascii(ascii_words_with_cancel(text, is_canceled)),
        Some(false) => Words::Unicode(unicode_words_with_cancel(text, is_canceled)),
        None => Words::Canceled,
    }
}

enum Words<A, U> {
    Ascii(A),
    Unicode(U),
    Canceled,
}

impl<'a, A, U> Iterator for Words<A, U>
where
    A: Iterator<Item = RopeSlice<'a>>,
    U: Iterator<Item = RopeSlice<'a>>,
{
    type Item = RopeSlice<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Ascii(words) => words.next(),
            Self::Unicode(words) => words.next(),
            Self::Canceled => None,
        }
    }
}

fn ascii_words_with_cancel(
    text: RopeSlice<'_>,
    mut is_canceled: impl FnMut() -> bool,
) -> impl Iterator<Item = RopeSlice<'_>> {
    let mut blocks = text.chunks().flat_map(|chunk| chunk.as_bytes().chunks(32));
    let mut start = None;
    let mut base = 0;
    let mut offset = 0;
    let mut len = 0;
    let mut mask = 0u32;
    let mut visited = 0;
    let mut done = false;
    iter::from_fn(move || {
        while !done {
            if offset == len {
                base += len;
                if visited >= CANCEL_CHECK_INTERVAL {
                    visited = 0;
                    if is_canceled() {
                        done = true;
                        return None;
                    }
                }
                let Some(bytes) = blocks.next() else {
                    done = true;
                    return start.take().and_then(|begin| {
                        (MIN_WORD_GRAPHEMES..=MAX_WORD_LEN)
                            .contains(&(base - begin))
                            .then(|| text.byte_slice(begin..base))
                    });
                };
                len = bytes.len();
                offset = 0;
                visited += len;
                // Independent byte classifications let LLVM vectorize this
                // loop, without requiring an architecture-specific instruction.
                mask = bytes.iter().enumerate().fold(0u32, |mask, (i, &byte)| {
                    mask | (u32::from(byte.is_ascii_alphanumeric() || byte == b'_') << i)
                });
            }
            let word = mask & 1 != 0;
            let run = if word {
                mask.trailing_ones()
            } else {
                mask.trailing_zeros()
            };
            let run = (run as usize).min(len - offset);
            let end = base + offset;
            offset += run;
            mask = mask.checked_shr(run as u32).unwrap_or(0);
            if word {
                start.get_or_insert(end);
            } else if let Some(begin) = start.take() {
                if (MIN_WORD_GRAPHEMES..=MAX_WORD_LEN).contains(&(end - begin)) {
                    return Some(text.byte_slice(begin..end));
                }
            }
        }
        None
    })
}

fn unicode_words_with_cancel(
    text: RopeSlice<'_>,
    mut is_canceled: impl FnMut() -> bool,
) -> impl Iterator<Item = RopeSlice<'_>> {
    let mut graphemes = text.grapheme_indices();
    // The in-progress word run: the byte offset of its first cluster, its length in chars, and the
    // number of graphemes it spans. `graphemes_len == 0` means we are between words.
    let mut visited = 0usize;
    let mut start_byte = 0;
    let mut char_len = 0;
    let mut graphemes_len = 0;

    // Yields `text[start_byte..end_byte]` if that run satisfies the length bounds.
    let qualify = move |start_byte, end_byte, char_len, graphemes_len| {
        (graphemes_len >= MIN_WORD_GRAPHEMES && char_len <= MAX_WORD_LEN)
            .then(|| text.byte_slice(start_byte..end_byte))
    };

    iter::from_fn(move || {
        loop {
            if visited.is_multiple_of(CANCEL_CHECK_INTERVAL) && is_canceled() {
                return None;
            }
            visited += 1;
            let Some((byte_idx, grapheme)) = graphemes.next() else {
                // Flush a word that runs up to the end of the text.
                let word = qualify(start_byte, text.len_bytes(), char_len, graphemes_len);
                graphemes_len = 0;
                return word;
            };

            if grapheme.chars().next().is_some_and(char_is_word) {
                if graphemes_len == 0 {
                    start_byte = byte_idx;
                    char_len = 0;
                }
                graphemes_len += 1;
                char_len += grapheme.len_chars();
            } else if graphemes_len != 0 {
                // A non-word cluster ends the current run; `byte_idx` is one past the run's end.
                let word = qualify(start_byte, byte_idx, char_len, graphemes_len);
                graphemes_len = 0;
                if word.is_some() {
                    return word;
                }
            }
        }
    })
}

/// Finds areas of the old and new texts around each operation in `changes`.
///
/// The window is larger than the changed area and can encompass multiple insert/delete operations
/// if they are grouped closely together.
///
/// The ranges of the old and new text should usually be of different sizes. For example a
/// deletion of "foo" surrounded by large retain sections would give a longer window into the
/// `old_text` and shorter window of `new_text`. Vice-versa for an insertion. A full replacement
/// of a word though would give two slices of the same size.
fn changed_windows<'a>(
    old_text: RopeSlice<'a>,
    new_text: RopeSlice<'a>,
    changes: &'a ChangeSet,
) -> impl Iterator<Item = (RopeSlice<'a>, RopeSlice<'a>)> {
    use helix_core::Operation::*;

    let mut operations = changes.changes().iter().peekable();
    let mut old_pos = 0;
    let mut new_pos = 0;
    iter::from_fn(move || loop {
        let operation = operations.next()?;
        let old_start = old_pos;
        let new_start = new_pos;
        let len = operation.len_chars();
        match operation {
            Retain(_) => {
                old_pos += len;
                new_pos += len;
                continue;
            }
            Insert(_) => new_pos += len,
            Delete(_) => old_pos += len,
        }

        // Scan ahead until a `Retain` is found which would end a window.
        while let Some(o) = operations.next_if(|op| !matches!(op, Retain(n) if *n > MAX_WORD_LEN)) {
            let len = o.len_chars();
            match o {
                Retain(_) => {
                    old_pos += len;
                    new_pos += len;
                }
                Delete(_) => old_pos += len,
                Insert(_) => new_pos += len,
            }
        }

        let old_window = old_start.saturating_sub(MAX_WORD_LEN)
            ..(old_pos + MAX_WORD_LEN).min(old_text.len_chars());
        let new_window = new_start.saturating_sub(MAX_WORD_LEN)
            ..(new_pos + MAX_WORD_LEN).min(new_text.len_chars());

        return Some((old_text.slice(old_window), new_text.slice(new_window)));
    })
}

/// Estimates whether a changeset is significant or small.
fn is_changeset_significant(changes: &ChangeSet) -> bool {
    use helix_core::Operation::*;

    let mut diff = 0;
    for operation in changes.changes() {
        match operation {
            Retain(_) => continue,
            Delete(_) | Insert(_) => diff += operation.len_chars(),
        }
    }

    // This is arbitrary and could be tuned further:
    diff > 1_000
}

pub(crate) fn register_hooks(handlers: &Handlers) {
    let coordinator = handlers.word_index.coordinator.clone();
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        let doc = doc!(event.editor, &event.doc);
        if doc.word_completion_enabled() {
            send(&coordinator, Event::Insert(doc.id(), doc.text().clone()));
        }
        Ok(())
    });

    let tx = handlers.word_index.hook.clone();
    let coordinator = handlers.word_index.coordinator.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if !event.ghost_transaction && event.doc.word_completion_enabled() {
            helix_event::send_blocking(
                &tx,
                Event::Update(
                    event.doc.id(),
                    Change {
                        old_text: event.old_text.clone(),
                        text: event.doc.text().clone(),
                        changes: event.changes.clone(),
                        dirty: false,
                        generation: coordinator.pending.lock().generation,
                    },
                ),
            );
        }
        Ok(())
    });

    let tx = handlers.word_index.hook.clone();
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        if event.doc.word_completion_enabled() {
            helix_event::send_blocking(
                &tx,
                Event::Delete(event.doc.id(), event.doc.text().clone()),
            );
        }
        Ok(())
    });

    let coordinator = handlers.word_index.coordinator.clone();
    register_hook!(move |event: &mut ConfigDidChange<'_>| {
        // The feature has been turned off. Clear the index and reclaim any used memory.
        if event.old.word_completion.enable && !event.new.word_completion.enable {
            send(&coordinator, Event::Clear);
        }

        // The feature has been turned on. Index open documents.
        if !event.old.word_completion.enable && event.new.word_completion.enable {
            for doc in event.editor.documents() {
                if doc.word_completion_enabled() {
                    send(&coordinator, Event::Insert(doc.id(), doc.text().clone()));
                }
            }
        }

        Ok(())
    });
}

// See `benches/word_index.rs`.
#[cfg(feature = "bench")]
pub mod bench {
    use helix_core::{Rope, RopeSlice};

    pub use super::WordIndex;

    pub fn add_document(index: &WordIndex, text: &Rope) {
        let mut cancel = helix_event::TaskController::new();
        index.add_document(text, &cancel.restart());
    }

    pub fn words(text: RopeSlice) -> impl Iterator<Item = RopeSlice> {
        super::words(text)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use quickcheck::{Arbitrary, Gen};

    #[test]
    fn refined_matches_preserve_unicode_case_rules_and_follow_index_updates() {
        let index = WordIndex::default();
        let text = Rope::from_str("alpha alphabet Alpine álphabet beta almanac ALPHA omega");
        let mut controller = TaskController::new();
        index.add_document(&text, &controller.restart());
        for pattern in [
            "a", "al", "alp", "alpha", "Al", "AlP", "ál", "b", "be", "", "omega",
        ] {
            let actual: HashSet<_> = index.matches(pattern).into_iter().collect();
            let expected: HashSet<_> =
                helix_core::fuzzy::fuzzy_match(pattern, index.words(), false)
                    .into_iter()
                    .map(|(word, _)| word)
                    .collect();
            assert_eq!(actual, expected);
        }
        index.add_document(&Rope::from_str("alphonse"), &controller.restart());
        assert!(index.matches("alph").contains(&"alphonse".into()));
        index.clear();
        assert!(index.matches("alph").is_empty());
        assert!(index.candidates.lock().as_ref().unwrap().words.is_empty());
        let cancel = controller.restart();
        controller.cancel();
        assert!(index.matches_cancelable("", &cancel).is_empty());
    }

    impl WordIndex {
        fn words(&self) -> HashSet<String> {
            let inner = self.inner.read();
            inner.words.keys().map(|w| w.to_string()).collect()
        }

        /// The full reference-counted word multiset. Unlike [`WordIndex::words`] this keeps the
        /// counts, which the incremental update path must hold exactly in step with a fresh index:
        /// a word is only freed once its count falls to zero, so any drift leaks stale words.
        fn counts(&self) -> std::collections::HashMap<String, u32> {
            let inner = self.inner.read();
            inner
                .words
                .iter()
                .map(|(w, c)| (w.to_string(), *c))
                .collect()
        }
    }

    #[track_caller]
    fn assert_words<I: ToString, T: IntoIterator<Item = I>>(text: &str, expected: T) {
        let text = Rope::from_str(text);
        let index = WordIndex::default();
        let mut cancel = TaskController::new();
        index.add_document(&text, &cancel.restart());
        let actual = index.words();
        let expected: HashSet<_> = expected.into_iter().map(|i| i.to_string()).collect();
        assert_eq!(expected, actual);
    }

    #[test]
    fn parse() {
        assert_words("one two three", ["one", "two", "three"]);
        assert_words("a foo c", ["foo"]);
    }

    #[test]
    fn ascii_runs_and_unicode_fallback_preserve_grapheme_boundaries() {
        for input in [
            "abc_123\r\nnext\0last".into(),
            format!("{} {}", "a".repeat(50), "b".repeat(51)),
            format!(
                "{}boundary_word\r\n{}",
                " ".repeat(997),
                "alpha beta\r\n".repeat(1000)
            ),
            "abc e\u{301}fg résumé 中文词 emoji😀 next".into(),
            "a".repeat(1023) + "e\u{301}fg last",
        ] {
            let rope = Rope::from_str(&input);
            for text in [rope.slice(..), rope.slice(1..rope.len_chars())] {
                let expected: Vec<_> = unicode_words_with_cancel(text, || false).collect();
                assert_eq!(words(text).collect::<Vec<_>>(), expected);
            }
        }
    }

    #[tokio::test]
    async fn concurrent_preparations_cancel_before_clear_and_new_revisions() {
        if helix_stdx::cpu::worker_count() < 2 {
            return;
        }
        let coordinator = Coordinator::default();
        let index = WordIndex::default();
        let mut controller = TaskController::new();
        let task = tokio::spawn(index.clone().run(coordinator.clone(), controller.restart()));
        let first = DocumentId::new(1);
        let second = DocumentId::new(2);
        let large = Rope::from_str(&"shared résumés repeated\n".repeat(100_000));
        send(&coordinator, Event::Insert(first, large.clone()));
        send(&coordinator, Event::Insert(second, large));
        tokio::time::timeout(Duration::from_secs(5), async {
            while coordinator.pending.lock().active.len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        send(
            &coordinator,
            Event::Update(first, build_change("ghost", "shared newest")),
        );
        send(&coordinator, Event::Delete(second, Rope::new()));
        send(&coordinator, Event::Clear);
        send(
            &coordinator,
            Event::Insert(first, Rope::from_str("shared final")),
        );
        send(
            &coordinator,
            Event::Insert(second, Rope::from_str("shared second")),
        );
        let expected = std::collections::HashMap::from([
            ("shared".into(), 2),
            ("final".into(), 1),
            ("second".into(), 1),
        ]);
        tokio::time::timeout(Duration::from_secs(5), async {
            while index.counts() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        controller.cancel();
        task.await.unwrap();
    }

    #[test]
    fn long_tokens_and_nonword_runs_check_cancellation_without_emitting_words() {
        for input in ["a".repeat(100_000), " ".repeat(100_000)] {
            let text = Rope::from_str(&input);
            let mut checks = 0;
            let mut scan = words_with_cancel(text.slice(..), || {
                checks += 1;
                checks >= 2
            });
            assert!(scan.next().is_none());
            drop(scan);
            assert_eq!(checks, 2);
        }
    }

    #[test]
    fn canceled_staging_never_changes_reference_counts() {
        let index = WordIndex::default();
        let before = Rope::from_str("shared shared first");
        let after = Rope::from_str("shared later");
        let mut cancel = TaskController::new();
        let handle = cancel.restart();
        index.add_document(&before, &handle);
        let expected = index.counts();
        cancel.cancel();
        let diff = compare_ropes(&before, &after);
        index.update_document(&before, &after, diff.changes(), &handle);
        assert_eq!(index.counts(), expected);
        assert!(index.matches_cancelable("sh", &handle).is_empty());
    }

    #[test]
    fn clear_discards_debounced_and_already_queued_old_generation_events() {
        let coordinator = Coordinator::default();
        let mut hook = Hook {
            changes: HashMap::default(),
            coordinator: coordinator.clone(),
            generation: 0,
        };
        let doc = DocumentId::default();
        hook.handle_event(Event::Update(doc, build_change("before", "after")), None);
        let queued_old = build_change("after", "stale");
        send(&coordinator, Event::Clear);
        hook.handle_event(Event::Update(doc, queued_old), None);
        hook.finish_debounce();
        assert!(coordinator.pending.lock().events.is_empty());
    }

    #[tokio::test]
    async fn latest_revisions_close_and_clear_converge_to_exact_counts() {
        let coordinator = Coordinator::default();
        let index = WordIndex::default();
        let mut cancel = TaskController::new();
        let task = tokio::spawn(index.clone().run(coordinator.clone(), cancel.restart()));
        let first = DocumentId::new(1);
        let second = DocumentId::new(2);
        send(
            &coordinator,
            Event::Insert(first, Rope::from_str("shared alpha")),
        );
        send(
            &coordinator,
            Event::Insert(second, Rope::from_str("shared bravo")),
        );
        for _ in 0..1000 {
            send(
                &coordinator,
                Event::Update(first, build_change("ghost ignored", "shared omega")),
            );
        }
        assert_eq!(coordinator.pending.lock().events.len(), 2);
        let expected = std::collections::HashMap::from([
            ("shared".to_owned(), 2),
            ("bravo".to_owned(), 1),
            ("omega".to_owned(), 1),
        ]);
        tokio::time::timeout(Duration::from_secs(5), async {
            while index.counts() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        send(
            &coordinator,
            Event::Delete(second, Rope::from_str("ghost close text")),
        );
        let expected =
            std::collections::HashMap::from([("shared".to_owned(), 1), ("omega".to_owned(), 1)]);
        tokio::time::timeout(Duration::from_secs(5), async {
            while index.counts() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        send(&coordinator, Event::Clear);
        send(
            &coordinator,
            Event::Insert(first, Rope::from_str("final shared")),
        );
        let expected =
            std::collections::HashMap::from([("shared".to_owned(), 1), ("final".to_owned(), 1)]);
        tokio::time::timeout(Duration::from_secs(5), async {
            while index.counts() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        task.await.unwrap();
    }

    #[track_caller]
    fn assert_diff<S, R, I>(before: &str, after: &str, expect_removed: R, expect_inserted: I)
    where
        S: ToString,
        R: IntoIterator<Item = S>,
        I: IntoIterator<Item = S>,
    {
        let before = Rope::from_str(before);
        let after = Rope::from_str(after);
        let diff = compare_ropes(&before, &after);
        let expect_removed: HashSet<_> =
            expect_removed.into_iter().map(|i| i.to_string()).collect();
        let expect_inserted: HashSet<_> =
            expect_inserted.into_iter().map(|i| i.to_string()).collect();

        let index = WordIndex::default();
        let mut cancel = TaskController::new();
        let handle = cancel.restart();
        index.add_document(&before, &handle);
        let words_before = index.words();
        index.update_document(&before, &after, diff.changes(), &handle);
        let words_after = index.words();

        let actual_removed = words_before.difference(&words_after).cloned().collect();
        let actual_inserted = words_after.difference(&words_before).cloned().collect();

        eprintln!("\"{before}\" {words_before:?} => \"{after}\" {words_after:?}");
        assert_eq!(
            expect_removed, actual_removed,
            "expected {expect_removed:?} to be removed, instead {actual_removed:?} was"
        );
        assert_eq!(
            expect_inserted, actual_inserted,
            "expected {expect_inserted:?} to be inserted, instead {actual_inserted:?} was"
        );
    }

    #[test]
    fn diff() {
        assert_diff("one two three", "one five three", ["two"], ["five"]);
        assert_diff("one two three", "one to three", ["two"], []);
        assert_diff("one two three", "one three", ["two"], []);
        assert_diff("one two three", "one t{o three", ["two"], []);
        assert_diff("one foo three", "one fooo three", ["foo"], ["fooo"]);
    }

    fn build_change(old: &str, new: &str) -> Change {
        let old_text = Rope::from_str(old);
        let text = Rope::from_str(new);
        let changes = compare_ropes(&old_text, &text).changes().clone();
        Change {
            old_text,
            text,
            changes,
            dirty: false,
            generation: 0,
        }
    }

    /// Drives a [`Hook`] through a sequence of observed `(old_text, new_text)` changes to a single
    /// document, flushes the debounce, and returns the change the hook hands to the indexer (with
    /// its changeset recomputed if the observed changes were coalesced). Returns `None` if no
    /// change was emitted.
    fn coalesce<'a>(observations: impl IntoIterator<Item = (&'a str, &'a str)>) -> Option<Change> {
        let coordinator = Coordinator::default();
        let observed = coordinator.clone();
        let mut hook = Hook {
            changes: HashMap::default(),
            coordinator,
            generation: 0,
        };
        let doc = DocumentId::default();
        for (old, new) in observations {
            hook.handle_event(Event::Update(doc, build_change(old, new)), None);
        }
        hook.finish_debounce();
        let event = observed.pending.lock().events.remove(&doc);
        match event {
            Some(Event::Update(_, mut change)) => {
                if change.dirty {
                    change.changes = compare_ropes(&change.old_text, &change.text)
                        .changes()
                        .clone();
                }
                Some(change)
            }
            _ => None,
        }
    }

    #[track_caller]
    fn assert_coalesces(observations: &[(&str, &str)]) {
        let change = coalesce(observations.iter().copied())
            .expect("a coalesced change should have been emitted");
        let first = observations.first().unwrap();
        let last = observations.last().unwrap();

        // The coalesced change spans from the first text observed to the last.
        assert_eq!(change.old_text, Rope::from_str(first.0));
        assert_eq!(change.text, Rope::from_str(last.1));

        // Its changeset must faithfully map `old_text` -> `text`; `update_document` relies on this
        // to locate the windows it reindexes.
        let mut applied = change.old_text.clone();
        assert!(change.changes.apply(&mut applied));
        assert_eq!(applied, change.text);

        // Reindexing must converge onto exactly the word multiset of the final text (words and
        // refcounts) matching a fresh index built from scratch.
        let mut cancel = TaskController::new();
        let handle = cancel.restart();
        let index = WordIndex::default();
        index.add_document(&change.old_text, &handle);
        index.update_document(&change.old_text, &change.text, &change.changes, &handle);

        let fresh = WordIndex::default();
        fresh.add_document(&change.text, &handle);

        assert_eq!(index.counts(), fresh.counts());
    }

    #[test]
    fn hook_coalesces_contiguous_changes() {
        // Two real edits that chain cleanly: the text after the first edit is the text before the
        // second, so the changesets compose directly.
        assert_coalesces(&[
            ("the quick brown fox", "the slowish brown fox"),
            ("the slowish brown fox", "the slowish green fox"),
        ]);
    }

    #[test]
    fn hook_coalesces_across_skipped_ghost_edit() {
        // A ghost transaction edited the document between the two real edits and was not reported
        // to the index, so the text before the second edit no longer matches the text after the
        // first. Here it is longer, which would make a chained `ChangeSet::compose` panic on its
        // `len_after == len` precondition.
        assert_coalesces(&[
            ("the quick brown fox", "the quick brown foxes"),
            ("the quick brown foxes jumped over", "the lazy brown foxes"),
        ]);
    }

    #[test]
    fn hook_coalesces_across_length_preserving_ghost_edit() {
        // The subtle case: a ghost edit changes content without changing length, in a region the
        // next real edit leaves untouched. The two observed changes then have matching lengths
        // but different contents, so a length check alone cannot tell they are non-contiguous.
        let p = ".".repeat(60);
        assert_coalesces(&[
            (&format!("aaa{p}ggg"), &format!("bbb{p}ggg")),
            (&format!("bbb{p}hhh"), &format!("ccc{p}hhh")),
        ]);
    }

    const CORPORA: &[(&str, &str)] = &[
        ("arabic", include_str!("../../benches/texts/arabic.txt")),
        ("english", include_str!("../../benches/texts/english.txt")),
        ("hindi", include_str!("../../benches/texts/hindi.txt")),
        ("japanese", include_str!("../../benches/texts/japanese.txt")),
        ("korean", include_str!("../../benches/texts/korean.txt")),
        ("mandarin", include_str!("../../benches/texts/mandarin.txt")),
        ("russian", include_str!("../../benches/texts/russian.txt")),
        (
            "source_code",
            include_str!("../../benches/texts/source_code.txt"),
        ),
    ];

    #[track_caller]
    fn assert_extracts(text: &str, expected: &[&str]) {
        let rope = Rope::from_str(text);
        let got: Vec<String> = words(rope.slice(..)).map(|w| w.to_string()).collect();
        assert_eq!(got, expected, "extracting words from {text:?}");
    }

    /// `words` categorizes whole grapheme clusters, so a word boundary never falls inside a
    /// cluster. A non-word combining mark stays attached to the word character it modifies.
    #[test]
    fn extract_respects_grapheme_clusters() {
        // Whitespace and punctuation separate words. Runs under MIN_WORD_GRAPHEMES are dropped.
        assert_extracts("a foo c", &["foo"]);
        assert_extracts(
            "foo.bar.baz qux::quux",
            &["foo", "bar", "baz", "qux", "quux"],
        );
        assert_extracts("snake_case_id CamelCase", &["snake_case_id", "CamelCase"]);
        // Precomposed and decomposed accents both keep the whole word: the combining mark rides
        // along with the letter it attaches to rather than truncating the word before it.
        assert_extracts("naïve café résumé", &["naïve", "café", "résumé"]);
        assert_extracts(
            "nai\u{0308}ve cafe\u{0301}",
            &["nai\u{0308}ve", "cafe\u{0301}"],
        );
        // A Devanagari conjunct is joined by a virama (itself a non-word char) yet stays one word.
        assert_extracts("ज्ञानकोश है", &["ज्ञानकोश"]);
        // Hangul syllables are word characters.
        assert_extracts("한국어 위키백과", &["한국어", "위키백과"]);
        // An emoji cluster is not.
        assert_extracts("emoji 👍🏽 word", &["emoji", "word"]);
    }

    /// Every word extracted from the corpora must satisfy the documented invariants.
    #[test]
    fn extract_corpora_invariants() {
        for (name, text) in CORPORA {
            let rope = Rope::from_str(text);
            let mut count = 0;
            for word in words(rope.slice(..)) {
                count += 1;
                let clusters = word.graphemes().count();
                assert!(
                    clusters >= MIN_WORD_GRAPHEMES,
                    "{name}: {word:?} spans only {clusters} grapheme cluster(s)"
                );
                assert!(
                    word.len_chars() <= MAX_WORD_LEN,
                    "{name}: {word:?} is {} chars long",
                    word.len_chars()
                );
                assert!(
                    word.chars().next().is_some_and(char_is_word),
                    "{name}: {word:?} starts with a non-word character"
                );
            }
            assert!(count > 0, "{name}: expected to extract some words");
        }
    }

    /// Grapheme-cluster edge cases which the prose corpora underrepresent: combining marks at
    /// word starts and seams, a skin-tone emoji, a ZWJ family sequence, a Devanagari conjunct,
    /// and a lone Hangul syllable.
    const SPICE: &[&str] = &[
        "a\u{0301}",              // base + combining acute
        "\u{0301}lead",           // leading combining mark
        "mid\u{0308}dle",         // combining diaeresis mid-word
        "👍🏽",                     // emoji + skin-tone modifier
        "👨\u{200D}👩\u{200D}👧", // ZWJ family sequence
        "क्ष",                     // Devanagari conjunct (virama)
        "한",                     // Hangul syllable
    ];

    /// Picks a chunk of text: usually a random char span sliced out of one of the multilingual
    /// corpora (real grapheme complexity for free, and slicing on char boundaries deliberately
    /// frays some clusters), occasionally an adversarial cluster from [`SPICE`].
    fn sample_text(g: &mut Gen) -> String {
        if usize::arbitrary(g) % 4 == 0 {
            return g.choose(SPICE).unwrap().to_string();
        }
        let (_, corpus) = g.choose(CORPORA).unwrap();
        let chars: Vec<char> = corpus.chars().collect();
        if chars.is_empty() {
            return String::new();
        }
        let span = usize::arbitrary(g) % 200;
        let start = usize::arbitrary(g) % chars.len();
        let end = (start + span).min(chars.len());
        chars[start..end].iter().collect()
    }

    /// Applies one random splice to `text`: delete a random char range and insert a fresh chunk.
    fn random_edit(g: &mut Gen, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let len = chars.len();
        let del_start = usize::arbitrary(g) % (len + 1);
        let del_len = usize::arbitrary(g) % (len - del_start + 1);
        let mut out: String = chars[..del_start].iter().collect();
        out.push_str(&sample_text(g));
        out.extend(&chars[del_start + del_len..]);
        out
    }

    /// An obvious, independent reimplementation of [`words`]: walk grapheme clusters, accumulate
    /// a run of consecutive word-character clusters, and emit it when it ends if it satisfies the
    /// length bounds. Used as an oracle for the optimized single-pass extractor.
    fn reference_words(text: &str) -> Vec<String> {
        let rope = Rope::from_str(text);
        let mut out = Vec::new();
        let mut run = String::new();
        let mut clusters = 0usize;
        let flush = |run: &mut String, clusters: &mut usize, out: &mut Vec<String>| {
            if *clusters >= MIN_WORD_GRAPHEMES && run.chars().count() <= MAX_WORD_LEN {
                out.push(run.clone());
            }
            run.clear();
            *clusters = 0;
        };
        for cluster in rope.slice(..).graphemes() {
            if cluster.chars().next().is_some_and(char_is_word) {
                run.extend(cluster.chars());
                clusters += 1;
            } else {
                flush(&mut run, &mut clusters, &mut out);
            }
        }
        flush(&mut run, &mut clusters, &mut out);
        out
    }

    #[derive(Clone, Debug)]
    struct SampledText(String);

    impl Arbitrary for SampledText {
        fn arbitrary(g: &mut Gen) -> Self {
            SampledText(sample_text(g))
        }
        fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
            Box::new(self.0.shrink().map(SampledText))
        }
    }

    #[derive(Clone, Debug)]
    struct EditPair {
        old: String,
        new: String,
    }

    impl Arbitrary for EditPair {
        fn arbitrary(g: &mut Gen) -> Self {
            let old = sample_text(g);
            let new = random_edit(g, &old);
            EditPair { old, new }
        }
        fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
            Box::new(
                (self.old.clone(), self.new.clone())
                    .shrink()
                    .map(|(old, new)| EditPair { old, new }),
            )
        }
    }

    /// A sequence of `(old_text, new_text)` changes as the hook observes them. Some steps leave
    /// a gap (the next change's `old_text` differs from the previous `new_text`) modelling an
    /// unobserved ghost edit applied to the document between two real ones.
    #[derive(Clone, Debug)]
    struct Observations(Vec<(String, String)>);

    impl Arbitrary for Observations {
        fn arbitrary(g: &mut Gen) -> Self {
            let steps = usize::arbitrary(g) % 5 + 1;
            let mut current = sample_text(g);
            let mut observations = Vec::with_capacity(steps);
            for _ in 0..steps {
                let observed_old = if bool::arbitrary(g) {
                    // A ghost edit perturbed the document before this real edit, so the observed
                    // `old_text` no longer matches the previous `new_text`.
                    random_edit(g, &current)
                } else {
                    current.clone()
                };
                let new = random_edit(g, &observed_old);
                observations.push((observed_old, new.clone()));
                current = new;
            }
            Observations(observations)
        }
        fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
            Box::new(self.0.shrink().map(Observations))
        }
    }

    quickcheck::quickcheck! {
        /// The optimized single-pass extractor agrees with the obvious reference on arbitrary
        /// (multilingual, cluster-frayed) text.
        fn prop_words_match_reference(text: SampledText) -> bool {
            let rope = Rope::from_str(&text.0);
            let got: Vec<String> = words(rope.slice(..)).map(|w| w.to_string()).collect();
            got == reference_words(&text.0)
        }

        /// Incrementally updating across one edit lands on exactly the same word multiset (words
        /// and refcounts) as reindexing the new text from scratch.
        fn prop_update_document_matches_fresh(pair: EditPair) -> bool {
            let old = Rope::from_str(&pair.old);
            let new = Rope::from_str(&pair.new);
            let changes = compare_ropes(&old, &new).changes().clone();

            let mut cancel = TaskController::new();
            let handle = cancel.restart();
            let incremental = WordIndex::default();
            incremental.add_document(&old, &handle);
            incremental.update_document(&old, &new, &changes, &handle);

            let fresh = WordIndex::default();
            fresh.add_document(&new, &handle);

            incremental.counts() == fresh.counts()
        }

        /// Coalescing a sequence of observed changes (gaps and all) and flushing yields a change
        /// that maps its `old_text` to its `text` and drives the index to the same multiset as a
        /// fresh index of the final text.
        fn prop_hook_coalescing_matches_fresh(obs: Observations) -> bool {
            let Some(change) = coalesce(obs.0.iter().map(|(o, n)| (o.as_str(), n.as_str()))) else {
                return true;
            };

            let mut applied = change.old_text.clone();
            if !change.changes.apply(&mut applied) || applied != change.text {
                return false;
            }

            let mut cancel = TaskController::new();
            let handle = cancel.restart();
            let incremental = WordIndex::default();
            incremental.add_document(&change.old_text, &handle);
            incremental.update_document(&change.old_text, &change.text, &change.changes, &handle);

            let fresh = WordIndex::default();
            fresh.add_document(&change.text, &handle);

            incremental.counts() == fresh.counts()
        }
    }
}
