//! The `DocumentFormatter` forms the bridge between the raw document text
//! and onscreen positioning. It yields the text graphemes as an iterator
//! and traverses (part) of the document text. During that traversal it
//! handles grapheme detection, softwrapping and annotations.
//! It yields `FormattedGrapheme`s and their corresponding visual coordinates.
//!
//! As both virtual text and softwrapping can insert additional lines into the document
//! it is generally not possible to find the start of the previous visual line.
//! Instead the `DocumentFormatter` starts at the last "checkpoint" (usually a linebreak)
//! called a "block" and the caller must advance it as needed.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use parking_lot::Mutex;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::mem::replace;

#[cfg(test)]
mod test;

use unicode_segmentation::{Graphemes, UnicodeSegmentation};

use helix_stdx::rope::{RopeGraphemes, RopeSliceExt};

use crate::graphemes::{Grapheme, GraphemeStr};
use crate::syntax::Highlight;
use crate::text_annotations::{LineAnnotationCheckpoint, TextAnnotations};
use crate::{ChangeSet, Position, RopeSlice};

#[derive(Debug, Clone, Copy)]
pub enum GraphemeSource {
    Document {
        codepoints: u32,
    },
    /// Inline virtual text can not be highlighted with a `Highlight` iterator
    /// because it's not part of the document. Instead the `Highlight`
    /// is emitted right by the document formatter
    VirtualText {
        highlight: Option<Highlight>,
    },
}

impl GraphemeSource {
    /// Returns whether this grapheme is virtual inline text
    pub fn is_virtual(self) -> bool {
        matches!(self, GraphemeSource::VirtualText { .. })
    }

    pub fn is_eof(self) -> bool {
        // all doc chars except the EOF char have non-zero codepoints
        matches!(self, GraphemeSource::Document { codepoints: 0 })
    }

    pub fn doc_chars(self) -> usize {
        match self {
            GraphemeSource::Document { codepoints } => codepoints as usize,
            GraphemeSource::VirtualText { .. } => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FormattedGrapheme<'a> {
    pub raw: Grapheme<'a>,
    pub source: GraphemeSource,
    pub visual_pos: Position,
    /// Document line at the start of the grapheme
    pub line_idx: usize,
    /// Document char position at the start of the grapheme
    pub char_idx: usize,
}

impl FormattedGrapheme<'_> {
    pub fn is_virtual(&self) -> bool {
        self.source.is_virtual()
    }

    pub fn doc_chars(&self) -> usize {
        self.source.doc_chars()
    }

    pub fn is_whitespace(&self) -> bool {
        self.raw.is_whitespace()
    }

    pub fn width(&self) -> usize {
        self.raw.width()
    }

    pub fn is_word_boundary(&self) -> bool {
        self.raw.is_word_boundary()
    }
}

#[derive(Debug, Clone)]
struct GraphemeWithSource<'a> {
    grapheme: Grapheme<'a>,
    source: GraphemeSource,
}

impl<'a> GraphemeWithSource<'a> {
    fn new(
        g: GraphemeStr<'a>,
        visual_x: usize,
        tab_width: u16,
        source: GraphemeSource,
    ) -> GraphemeWithSource<'a> {
        GraphemeWithSource {
            grapheme: Grapheme::new(g, visual_x, tab_width),
            source,
        }
    }
    fn placeholder() -> Self {
        GraphemeWithSource {
            grapheme: Grapheme::Other { g: " ".into() },
            source: GraphemeSource::Document { codepoints: 0 },
        }
    }

    fn doc_chars(&self) -> usize {
        self.source.doc_chars()
    }

    fn is_whitespace(&self) -> bool {
        self.grapheme.is_whitespace()
    }

    fn is_newline(&self) -> bool {
        matches!(self.grapheme, Grapheme::Newline)
    }

    fn is_eof(&self) -> bool {
        self.source.is_eof()
    }

    fn width(&self) -> usize {
        self.grapheme.width()
    }

    fn is_word_boundary(&self) -> bool {
        self.grapheme.is_word_boundary()
    }
}

// Checkpoints are recorded only when the word buffer is exhausted, so restoring
// one never changes word wrapping or splits a Unicode grapheme.
const CHECKPOINT_INTERVAL: usize = 1024;
const MAX_CACHED_LINES: usize = 16;
const MAX_CHECKPOINTS: usize = 4096;
const MAX_CACHED_LAYOUTS: usize = 4;

#[derive(Debug, Clone)]
enum OwnedGrapheme {
    Newline,
    Tab(usize),
    Other(String),
}

impl OwnedGrapheme {
    fn from_grapheme(grapheme: &Grapheme<'_>) -> Self {
        match grapheme {
            Grapheme::Newline => Self::Newline,
            Grapheme::Tab { width } => Self::Tab(*width),
            Grapheme::Other { g } => Self::Other(g.to_string()),
        }
    }

    fn into_grapheme<'a>(self) -> Grapheme<'a> {
        match self {
            Self::Newline => Grapheme::Newline,
            Self::Tab(width) => Grapheme::Tab { width },
            Self::Other(g) => Grapheme::Other { g: g.into() },
        }
    }
}

#[derive(Debug, Clone)]
struct Checkpoint {
    char_pos: usize,
    line_pos: usize,
    visual_pos: Position,
    indent_level: Option<usize>,
    rendered_indent: Option<usize>,
    previous_doc: (usize, usize),
    peeked: Option<(OwnedGrapheme, u32)>,
    exhausted: bool,
    line_width: usize,
    line_annotations: Vec<LineAnnotationCheckpoint>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TextIdentity {
    first_chunk: usize,
    bytes: usize,
    chars: usize,
}

impl From<RopeSlice<'_>> for TextIdentity {
    fn from(text: RopeSlice<'_>) -> Self {
        Self {
            first_chunk: text.chunks().next().unwrap_or("").as_ptr() as usize,
            bytes: text.len_bytes(),
            chars: text.len_chars(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CacheKey {
    source: TextIdentity,
    layout: u64,
    soft_wrap: bool,
}

#[derive(Debug, Default)]
struct CachedLine {
    checkpoints: Vec<Checkpoint>,
    end: Option<Checkpoint>,
    used: u64,
}

#[derive(Debug, Default)]
struct Checkpoints {
    key: CacheKey,
    clock: u64,
    lines: BTreeMap<usize, CachedLine>,
    count: usize,
}

#[derive(Debug, Default)]
struct LayoutCaches {
    entries: Vec<Checkpoints>,
    clock: u64,
}

/// A cached physical-line endpoint suitable for skipping an invisible tail.
#[derive(Debug, Clone, Copy)]
pub struct CachedLineEnd {
    pub char_idx: usize,
    pub width: usize,
    pub indent_level: Option<usize>,
    /// The synthetic EOF grapheme also has callbacks at `char_idx`.
    pub includes_eof: bool,
}

/// Bounded, reusable visual layout checkpoints for one document.
///
/// Invalidate this cache after changing its text. Layout and annotation changes are
/// detected automatically. Checkpoints contain owned data and can cross threads.
#[derive(Debug, Default)]
pub struct FormatterCache(Mutex<LayoutCaches>);

impl FormatterCache {
    pub fn clear(&self) {
        *self.0.lock() = LayoutCaches::default();
    }

    /// Retain the unchanged prefix after an edit. Wrapped text conservatively
    /// invalidates the edited physical line because word lookahead can move
    /// preceding text. Unwrapped text retains earlier grapheme checkpoints.
    pub fn invalidate_after_change(
        &self,
        old: RopeSlice<'_>,
        new: RopeSlice<'_>,
        changes: &ChangeSet,
    ) {
        let mut from = 0;
        for operation in changes.changes() {
            match operation {
                crate::Operation::Retain(chars) => from += chars,
                _ => break,
            }
        }
        if changes.is_empty() {
            return;
        }
        let safe_pos = crate::graphemes::prev_grapheme_boundary(old, from);
        let line_start = old.line_to_char(old.char_to_line(safe_pos));
        let mut caches = self.0.lock();
        caches.entries.retain_mut(|cache| {
            if cache.key.source != TextIdentity::from(old) {
                return false;
            }
            let cutoff = if cache.key.soft_wrap {
                line_start
            } else {
                safe_pos
            };
            Self::retain_prefix(cache, line_start, cutoff);
            cache.key.source = TextIdentity::from(new);
            true
        });
    }

    /// An annotation removed by an edit can change an earlier physical line.
    /// Retain checkpoints strictly before that line in every cached layout.
    pub fn invalidate_annotations_from(&self, text: RopeSlice<'_>, char_idx: usize) {
        let line_start = text.line_to_char(text.char_to_line(char_idx.min(text.len_chars())));
        let mut caches = self.0.lock();
        for cache in &mut caches.entries {
            Self::retain_prefix(cache, line_start, line_start);
        }
    }

    fn retain_prefix(cache: &mut Checkpoints, line_start: usize, cutoff: usize) {
        cache.lines.retain(|&start, line| {
            if start > line_start {
                return false;
            }
            if start == line_start {
                line.end = None;
                line.checkpoints.retain(|checkpoint| {
                    checkpoint.char_pos
                        + checkpoint
                            .peeked
                            .as_ref()
                            .map_or(0, |(_, chars)| *chars as usize)
                        < cutoff
                        && !checkpoint.exhausted
                });
                return !line.checkpoints.is_empty();
            }
            true
        });
        cache.count = cache
            .lines
            .values()
            .map(|line| line.checkpoints.len())
            .sum();
    }

    fn with_key(&self, key: CacheKey) -> parking_lot::MappedMutexGuard<'_, Checkpoints> {
        let mut caches = self.0.lock();
        caches.clock = caches.clock.wrapping_add(1);
        let used = caches.clock;
        let index = if let Some(index) = caches.entries.iter().position(|cache| cache.key == key) {
            index
        } else {
            if caches.entries.len() == MAX_CACHED_LAYOUTS {
                let oldest = caches
                    .entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, cache)| cache.clock)
                    .unwrap()
                    .0;
                caches.entries.swap_remove(oldest);
            }
            caches.entries.push(Checkpoints {
                key,
                ..Checkpoints::default()
            });
            caches.entries.len() - 1
        };
        caches.entries[index].clock = used;
        parking_lot::MutexGuard::map(caches, |caches| &mut caches.entries[index])
    }

    fn find(
        &self,
        key: CacheKey,
        line: usize,
        target: impl Fn(&Checkpoint) -> bool,
        valid: impl Fn(&Checkpoint) -> bool,
    ) -> Option<Checkpoint> {
        let mut cache = self.with_key(key);
        let used = cache.clock;
        let line = cache.lines.get_mut(&line)?;
        line.used = used;
        let i = line.checkpoints.partition_point(target);
        line.checkpoints[..i]
            .iter()
            .rev()
            .find(|checkpoint| valid(checkpoint))
            .cloned()
    }

    fn end(&self, key: CacheKey, line: usize) -> Option<Checkpoint> {
        self.with_key(key).lines.get(&line)?.end.clone()
    }

    fn insert(&self, key: CacheKey, line_start: usize, checkpoint: Checkpoint, end: bool) {
        let mut cache = self.with_key(key);
        if !cache.lines.contains_key(&line_start) && cache.lines.len() == MAX_CACHED_LINES {
            let oldest = *cache
                .lines
                .iter()
                .min_by_key(|(_, line)| line.used)
                .unwrap()
                .0;
            let removed = cache.lines.remove(&oldest).unwrap();
            cache.count -= removed.checkpoints.len();
        }
        let used = cache.clock;
        let line = cache.lines.entry(line_start).or_default();
        line.used = used;
        if end {
            line.end = Some(checkpoint);
            return;
        }
        let i = line
            .checkpoints
            .partition_point(|p| p.char_pos < checkpoint.char_pos);
        if line
            .checkpoints
            .get(i)
            .is_some_and(|p| p.char_pos == checkpoint.char_pos)
        {
            line.checkpoints[i] = checkpoint;
            return;
        }
        // Evict a line rather than shifting all checkpoints on every insertion.
        if cache.count == MAX_CHECKPOINTS {
            let oldest = *cache
                .lines
                .iter()
                .min_by_key(|(_, line)| line.used)
                .unwrap()
                .0;
            let removed = cache.lines.remove(&oldest).unwrap();
            cache.count -= removed.checkpoints.len();
        }
        let line = cache.lines.entry(line_start).or_default();
        line.used = used;
        line.checkpoints
            .insert(i.min(line.checkpoints.len()), checkpoint);
        cache.count += 1;
    }
}

#[derive(Debug, Clone)]
pub struct TextFormat {
    pub soft_wrap: bool,
    pub tab_width: u16,
    pub max_wrap: u16,
    pub max_indent_retain: u16,
    pub wrap_indicator: Box<str>,
    pub wrap_indicator_highlight: Option<Highlight>,
    pub viewport_width: u16,
    pub soft_wrap_at_text_width: bool,
    pub checkpoint_cache: Option<Arc<FormatterCache>>,
}

// test implementation is basically only used for testing or when softwrap is always disabled
impl Default for TextFormat {
    fn default() -> Self {
        TextFormat {
            soft_wrap: false,
            tab_width: 4,
            max_wrap: 3,
            max_indent_retain: 4,
            wrap_indicator: Box::from(" "),
            viewport_width: 17,
            wrap_indicator_highlight: None,
            soft_wrap_at_text_width: false,
            checkpoint_cache: None,
        }
    }
}

#[derive(Debug)]
pub struct DocumentFormatter<'t> {
    text: RopeSlice<'t>,
    cache_key: CacheKey,
    block_start: usize,
    physical_line_start: usize,
    physical_row_start: usize,
    next_checkpoint: usize,
    previous_doc: (usize, usize),
    rendered_indent: Option<usize>,
    text_fmt: &'t TextFormat,
    annotations: &'t TextAnnotations<'t>,

    /// The visual position at the end of the last yielded word boundary
    visual_pos: Position,
    graphemes: RopeGraphemes<'t>,
    /// The character pos of the `graphemes` iter used for inserting annotations
    char_pos: usize,
    /// The line pos of the `graphemes` iter used for inserting annotations
    line_pos: usize,
    exhausted: bool,

    inline_annotation_graphemes: Option<(Graphemes<'t>, Option<Highlight>)>,

    // softwrap specific
    /// The indentation of the current line
    /// Is set to `None` if the indentation level is not yet known
    /// because no non-whitespace graphemes have been encountered yet
    indent_level: Option<usize>,
    /// In case a long word needs to be split a single grapheme might need to be wrapped
    /// while the rest of the word stays on the same line
    peeked_grapheme: Option<GraphemeWithSource<'t>>,
    /// A first-in first-out (fifo) buffer for the Graphemes of any given word
    word_buf: Vec<GraphemeWithSource<'t>>,
    /// The index of the next grapheme that will be yielded from the `word_buf`
    word_i: usize,
}

impl<'t> DocumentFormatter<'t> {
    /// Creates a new formatter at the last block before `char_idx`.
    /// A block is a chunk which always ends with a linebreak.
    /// This is usually just a normal line break.
    /// A warm cache resumes from a grapheme/word boundary near `char_idx`;
    /// a cold cache starts at the physical line break.
    pub fn new_at_prev_checkpoint(
        text: RopeSlice<'t>,
        text_fmt: &'t TextFormat,
        annotations: &'t TextAnnotations,
        char_idx: usize,
    ) -> Self {
        Self::new(text, text_fmt, annotations, char_idx, None)
    }

    /// Resume before the supplied visual position, relative to the physical line
    /// containing `anchor`. A cold cache falls back to the physical line start.
    pub fn new_at_visual_checkpoint(
        text: RopeSlice<'t>,
        text_fmt: &'t TextFormat,
        annotations: &'t TextAnnotations,
        anchor: usize,
        visual_pos: Position,
    ) -> Self {
        Self::new(text, text_fmt, annotations, anchor, Some(visual_pos))
    }

    fn new(
        text: RopeSlice<'t>,
        text_fmt: &'t TextFormat,
        annotations: &'t TextAnnotations,
        char_idx: usize,
        visual_target: Option<Position>,
    ) -> Self {
        let block_line_idx = text.char_to_line(char_idx.min(text.len_chars()));
        let block_char_idx = text.line_to_char(block_line_idx);
        let cache_key = if text_fmt.checkpoint_cache.is_some() && annotations.can_checkpoint() {
            Self::layout_key(text, text_fmt, annotations)
        } else {
            CacheKey::default()
        };
        let checkpoint = text_fmt
            .checkpoint_cache
            .as_ref()
            .filter(|_| annotations.can_checkpoint())
            .and_then(|cache| {
                cache.find(
                    cache_key,
                    block_char_idx,
                    |checkpoint| {
                        visual_target.map_or(checkpoint.char_pos <= char_idx, |pos| {
                            checkpoint.visual_pos <= pos
                        })
                    },
                    |checkpoint| {
                        annotations
                            .checkpoint_is_valid(&checkpoint.line_annotations, checkpoint.char_pos)
                    },
                )
            });
        let mut formatter = DocumentFormatter {
            text,
            cache_key,
            block_start: block_char_idx,
            physical_line_start: block_char_idx,
            physical_row_start: 0,
            next_checkpoint: block_char_idx.saturating_add(CHECKPOINT_INTERVAL),
            previous_doc: (block_char_idx, 0),
            rendered_indent: None,
            text_fmt,
            annotations,
            visual_pos: Position::default(),
            graphemes: text.graphemes_at(text.char_to_byte(block_char_idx)),
            char_pos: block_char_idx,
            exhausted: false,
            indent_level: None,
            peeked_grapheme: None,
            word_buf: Vec::with_capacity(64),
            word_i: 0,
            line_pos: block_line_idx,
            inline_annotation_graphemes: None,
        };
        if let Some(checkpoint) = checkpoint {
            formatter.restore(checkpoint, 0);
        } else {
            annotations.reset_pos(block_char_idx);
        }
        formatter
    }

    fn layout_key(
        text: RopeSlice<'_>,
        format: &TextFormat,
        annotations: &TextAnnotations,
    ) -> CacheKey {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        format.soft_wrap.hash(&mut hasher);
        format.tab_width.hash(&mut hasher);
        format.max_wrap.hash(&mut hasher);
        format.max_indent_retain.hash(&mut hasher);
        format.wrap_indicator.hash(&mut hasher);
        format
            .wrap_indicator_highlight
            .map(|highlight| highlight.get())
            .hash(&mut hasher);
        format.viewport_width.hash(&mut hasher);
        format.soft_wrap_at_text_width.hash(&mut hasher);
        annotations.layout_key().hash(&mut hasher);
        CacheKey {
            source: text.into(),
            layout: hasher.finish(),
            soft_wrap: format.soft_wrap,
        }
    }

    fn restore(&mut self, checkpoint: Checkpoint, row_start: usize) {
        let raw_char_pos = checkpoint.char_pos
            + checkpoint
                .peeked
                .as_ref()
                .map_or(0, |(_, chars)| *chars as usize);
        self.annotations.reset_pos(raw_char_pos);
        assert!(
            self.annotations
                .restore_checkpoint(&checkpoint.line_annotations, checkpoint.char_pos),
            "matching annotation checkpoints must restore"
        );
        self.graphemes = self.text.graphemes_at(self.text.char_to_byte(raw_char_pos));
        self.char_pos = checkpoint.char_pos;
        self.line_pos = checkpoint.line_pos;
        self.visual_pos = checkpoint.visual_pos;
        self.visual_pos.row += row_start;
        self.indent_level = checkpoint.indent_level;
        self.rendered_indent = checkpoint.rendered_indent;
        self.previous_doc = (
            checkpoint.previous_doc.0,
            checkpoint.previous_doc.1 + row_start,
        );
        self.peeked_grapheme = checkpoint
            .peeked
            .map(|(grapheme, codepoints)| GraphemeWithSource {
                grapheme: grapheme.into_grapheme(),
                source: GraphemeSource::Document { codepoints },
            });
        self.exhausted = checkpoint.exhausted;
        self.inline_annotation_graphemes = None;
        self.word_buf.clear();
        self.word_i = 0;
        self.next_checkpoint = self.char_pos.saturating_add(CHECKPOINT_INTERVAL);
    }

    fn record_checkpoint(&mut self, end: bool, line_width: usize) {
        let Some(cache) = self.text_fmt.checkpoint_cache.as_ref() else {
            return;
        };
        if !self.annotations.can_checkpoint()
            || (end && self.char_pos - self.physical_line_start < CHECKPOINT_INTERVAL)
            || self.word_i < self.word_buf.len()
            || self
                .peeked_grapheme
                .as_ref()
                .is_some_and(|g| g.source.is_virtual() || g.is_eof())
        {
            return;
        }
        let checkpoint = Checkpoint {
            char_pos: self.char_pos,
            line_pos: self.line_pos,
            visual_pos: Position::new(
                self.visual_pos.row - self.physical_row_start,
                self.visual_pos.col,
            ),
            indent_level: self.indent_level,
            rendered_indent: self.rendered_indent,
            previous_doc: (
                self.previous_doc.0,
                self.previous_doc.1 - self.physical_row_start,
            ),
            peeked: self.peeked_grapheme.as_ref().map(|g| {
                (
                    OwnedGrapheme::from_grapheme(&g.grapheme),
                    g.doc_chars() as u32,
                )
            }),
            exhausted: self.exhausted,
            line_width,
            line_annotations: match self.annotations.checkpoint() {
                Some(state) => state,
                None => return,
            },
        };
        cache.insert(self.cache_key, self.physical_line_start, checkpoint, end);
        self.next_checkpoint = self.char_pos.saturating_add(CHECKPOINT_INTERVAL);
    }

    /// Physical line origin of this formatter's visual coordinates.
    pub fn block_start(&self) -> usize {
        self.block_start
    }

    /// Last document grapheme before a resumed checkpoint (character and visual row).
    pub fn previous_document_position(&self) -> (usize, usize) {
        self.previous_doc
    }

    pub fn rendered_indent(&self) -> Option<usize> {
        self.rendered_indent
    }

    /// Cached physical-line endpoint, used to avoid traversing an invisible tail.
    pub fn cached_line_end(&self) -> Option<CachedLineEnd> {
        if !self.annotations.can_checkpoint() {
            return None;
        }
        self.text_fmt
            .checkpoint_cache
            .as_ref()?
            .end(self.cache_key, self.physical_line_start)
            .filter(|p| {
                self.annotations
                    .checkpoint_is_valid(&p.line_annotations, p.char_pos)
            })
            .filter(|p| {
                // An inline newline later in this physical line could bring text
                // back into the viewport. Only skip a tail with no further visual lines.
                let row = self.visual_pos.row - self.physical_row_start;
                p.visual_pos.row == row + usize::from(p.line_pos != self.line_pos)
            })
            .map(|p| CachedLineEnd {
                char_idx: p.char_pos,
                width: p.line_width,
                indent_level: p.rendered_indent,
                includes_eof: p.exhausted,
            })
    }

    /// Skip to a previously traversed physical line's end, restoring supported
    /// line annotations. Opaque annotations retain ordinary traversal.
    pub fn skip_to_line_end(&mut self) -> bool {
        if !self.annotations.can_checkpoint() {
            return false;
        }
        let Some(checkpoint) = self
            .text_fmt
            .checkpoint_cache
            .as_ref()
            .and_then(|c| c.end(self.cache_key, self.physical_line_start))
            .filter(|p| {
                self.annotations
                    .checkpoint_is_valid(&p.line_annotations, p.char_pos)
            })
        else {
            return false;
        };
        let row_start = self.physical_row_start;
        self.restore(checkpoint, row_start);
        self.physical_line_start = self.char_pos;
        self.physical_row_start = self.visual_pos.row;
        if !self.exhausted {
            self.rendered_indent = None;
        }
        true
    }

    fn next_inline_annotation_grapheme(
        &mut self,
        char_pos: usize,
    ) -> Option<(&'t str, Option<Highlight>)> {
        loop {
            if let Some(&mut (ref mut annotation, highlight)) =
                self.inline_annotation_graphemes.as_mut()
            {
                if let Some(grapheme) = annotation.next() {
                    return Some((grapheme, highlight));
                }
            }

            if let Some((annotation, highlight)) =
                self.annotations.next_inline_annotation_at(char_pos)
            {
                self.inline_annotation_graphemes = Some((
                    UnicodeSegmentation::graphemes(&*annotation.text, true),
                    highlight,
                ))
            } else {
                return None;
            }
        }
    }

    fn advance_grapheme(&mut self, col: usize, char_pos: usize) -> Option<GraphemeWithSource<'t>> {
        let (grapheme, source) =
            if let Some((grapheme, highlight)) = self.next_inline_annotation_grapheme(char_pos) {
                (grapheme.into(), GraphemeSource::VirtualText { highlight })
            } else if let Some(grapheme) = self.graphemes.next() {
                let codepoints = grapheme.len_chars() as u32;

                let overlay = self.annotations.overlay_at(char_pos);
                let grapheme = match overlay {
                    Some((overlay, _)) => overlay.grapheme.as_str().into(),
                    None => Cow::from(grapheme).into(),
                };

                (grapheme, GraphemeSource::Document { codepoints })
            } else {
                if self.exhausted {
                    return None;
                }
                self.exhausted = true;
                // EOF grapheme is required for rendering
                // and correct position computations
                return Some(GraphemeWithSource {
                    grapheme: Grapheme::Other { g: " ".into() },
                    source: GraphemeSource::Document { codepoints: 0 },
                });
            };

        let grapheme = GraphemeWithSource::new(grapheme, col, self.text_fmt.tab_width, source);

        Some(grapheme)
    }

    /// Move a word to the next visual line
    fn wrap_word(&mut self) -> usize {
        // softwrap this word to the next line
        let indent_carry_over = if let Some(indent) = self.indent_level {
            if indent as u16 <= self.text_fmt.max_indent_retain {
                indent as u16
            } else {
                0
            }
        } else {
            // ensure the indent stays 0
            self.indent_level = Some(0);
            0
        };

        let virtual_lines =
            self.annotations
                .virtual_lines_at(self.char_pos, self.visual_pos, self.line_pos);
        self.visual_pos.col = indent_carry_over as usize;
        self.visual_pos.row += 1 + virtual_lines;
        let mut i = 0;
        let mut word_width = 0;
        let wrap_indicator = UnicodeSegmentation::graphemes(&*self.text_fmt.wrap_indicator, true)
            .map(|g| {
                i += 1;
                let grapheme = GraphemeWithSource::new(
                    g.into(),
                    self.visual_pos.col + word_width,
                    self.text_fmt.tab_width,
                    GraphemeSource::VirtualText {
                        highlight: self.text_fmt.wrap_indicator_highlight,
                    },
                );
                word_width += grapheme.width();
                grapheme
            });
        self.word_buf.splice(0..0, wrap_indicator);

        for grapheme in &mut self.word_buf[i..] {
            let visual_x = self.visual_pos.col + word_width;
            grapheme
                .grapheme
                .change_position(visual_x, self.text_fmt.tab_width);
            word_width += grapheme.width();
        }
        if let Some(grapheme) = &mut self.peeked_grapheme {
            let visual_x = self.visual_pos.col + word_width;
            grapheme
                .grapheme
                .change_position(visual_x, self.text_fmt.tab_width);
        }
        word_width
    }

    fn peek_grapheme(&mut self, col: usize, char_pos: usize) -> Option<&GraphemeWithSource<'t>> {
        if self.peeked_grapheme.is_none() {
            self.peeked_grapheme = self.advance_grapheme(col, char_pos);
        }
        self.peeked_grapheme.as_ref()
    }

    fn next_grapheme(&mut self, col: usize, char_pos: usize) -> Option<GraphemeWithSource<'t>> {
        self.peek_grapheme(col, char_pos);
        self.peeked_grapheme.take()
    }

    fn advance_to_next_word(&mut self) {
        self.word_buf.clear();
        let mut word_width = 0;
        let mut word_chars = 0;

        if self.exhausted {
            return;
        }

        loop {
            let mut col = self.visual_pos.col + word_width;
            let char_pos = self.char_pos + word_chars;
            match col.cmp(&(self.text_fmt.viewport_width as usize)) {
                // The EOF char and newline chars are always selectable in helix. That means
                // that wrapping happens "too-early" if a word fits a line perfectly. This
                // is intentional so that all selectable graphemes are always visible (and
                // therefore the cursor never disappears). However if the user manually set a
                // lower softwrap width then this is undesirable. Just increasing the viewport-
                // width by one doesn't work because if a line is wrapped multiple times then
                // some words may extend past the specified width.
                //
                // So we special case a word that ends exactly at line bounds and is followed
                // by a newline/eof character here.
                Ordering::Equal
                    if self.text_fmt.soft_wrap_at_text_width
                        && self
                            .peek_grapheme(col, char_pos)
                            .is_some_and(|grapheme| grapheme.is_newline() || grapheme.is_eof()) => {
                }
                Ordering::Equal if word_width > self.text_fmt.max_wrap as usize => return,
                Ordering::Greater if word_width > self.text_fmt.max_wrap as usize => {
                    self.peeked_grapheme = self.word_buf.pop();
                    return;
                }
                Ordering::Equal | Ordering::Greater => {
                    word_width = self.wrap_word();
                    col = self.visual_pos.col + word_width;
                }
                Ordering::Less => (),
            }

            let Some(grapheme) = self.next_grapheme(col, char_pos) else {
                return;
            };
            word_chars += grapheme.doc_chars();

            // Track indentation
            if !grapheme.is_whitespace() && self.indent_level.is_none() {
                self.indent_level = Some(self.visual_pos.col);
            } else if grapheme.grapheme == Grapheme::Newline {
                self.indent_level = None;
            }

            let is_word_boundary = grapheme.is_word_boundary();
            word_width += grapheme.width();
            self.word_buf.push(grapheme);

            if is_word_boundary {
                return;
            }
        }
    }

    /// returns the char index at the end of the last yielded grapheme
    pub fn next_char_pos(&self) -> usize {
        self.char_pos
    }
    /// returns the visual position at the end of the last yielded grapheme
    pub fn next_visual_pos(&self) -> Position {
        self.visual_pos
    }
}

impl<'t> Iterator for DocumentFormatter<'t> {
    type Item = FormattedGrapheme<'t>;

    fn next(&mut self) -> Option<Self::Item> {
        let grapheme = if self.text_fmt.soft_wrap {
            if self.word_i >= self.word_buf.len() {
                self.advance_to_next_word();
                self.word_i = 0;
            }
            let grapheme = replace(
                self.word_buf.get_mut(self.word_i)?,
                GraphemeWithSource::placeholder(),
            );
            self.word_i += 1;
            grapheme
        } else {
            self.advance_grapheme(self.visual_pos.col, self.char_pos)?
        };

        let grapheme = FormattedGrapheme {
            raw: grapheme.grapheme,
            source: grapheme.source,
            visual_pos: self.visual_pos,
            line_idx: self.line_pos,
            char_idx: self.char_pos,
        };

        self.char_pos += grapheme.doc_chars();
        if !grapheme.is_whitespace() && self.rendered_indent.is_none() {
            self.rendered_indent = Some(grapheme.visual_pos.col);
        }
        if !grapheme.is_virtual() {
            self.previous_doc = (grapheme.char_idx, grapheme.visual_pos.row);
        }
        if !grapheme.is_virtual() {
            self.annotations.process_virtual_text_anchors(&grapheme);
        }
        if grapheme.raw == Grapheme::Newline {
            // move to end of newline char
            self.visual_pos.col += 1;
            let virtual_lines =
                self.annotations
                    .virtual_lines_at(self.char_pos, self.visual_pos, self.line_pos);
            self.visual_pos.row += 1 + virtual_lines;
            self.visual_pos.col = 0;
            if !grapheme.is_virtual() {
                self.line_pos += 1;
            }
        } else {
            self.visual_pos.col += grapheme.width();
        }
        if !grapheme.is_virtual() {
            let line_end = grapheme.raw == Grapheme::Newline || grapheme.source.is_eof();
            if self.text_fmt.checkpoint_cache.is_some()
                && (line_end || self.char_pos >= self.next_checkpoint)
            {
                self.record_checkpoint(line_end, grapheme.visual_pos.col + grapheme.width());
            }
            if grapheme.raw == Grapheme::Newline {
                self.physical_line_start = self.char_pos;
                self.physical_row_start = self.visual_pos.row;
                self.next_checkpoint = self.char_pos.saturating_add(CHECKPOINT_INTERVAL);
                self.rendered_indent = None;
                self.previous_doc = (self.char_pos, self.visual_pos.row);
            }
        }
        if grapheme.raw == Grapheme::Newline {
            self.rendered_indent = None;
        }
        Some(grapheme)
    }
}
