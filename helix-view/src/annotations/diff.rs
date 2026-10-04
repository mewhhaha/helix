use std::{
    any::Any,
    cell::RefCell,
    hash::{Hash, Hasher},
    ops::Range,
    sync::Arc,
};

use helix_core::{
    char_idx_at_visual_offset,
    doc_formatter::{FormattedGrapheme, TextFormat},
    graphemes::prev_grapheme_boundary,
    syntax::{config::LanguageConfiguration, Loader, Syntax},
    text_annotations::{LineAnnotation, TextAnnotations},
    visual_offset_from_block, Position, Range as SelectionRange, Rope, RopeSlice, Selection,
};
use helix_vcs::Hunk;

use super::review_comments::{review_blocks, ReviewBlock, ReviewBlockContent};
use crate::{document::ReviewKey, Document, DocumentId, ViewId};

/// Review display state belongs to a view, independently of the editable text.
#[derive(Clone, Default)]
pub struct DiffMode {
    enabled: bool,
    scroll: Option<ReviewScroll>,
    cursor: Option<DiffCursor>,
    comment_cursor: Option<CommentCursor>,
    cache: RefCell<Option<Arc<DiffDisplay>>>,
    base_syntax: RefCell<Option<BaseSyntax>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ReviewScroll {
    document: DocumentId,
    version: i32,
    selection: u64,
}

impl ReviewScroll {
    fn new(doc: &Document, view: ViewId) -> Self {
        Self {
            document: doc.id(),
            version: doc.version(),
            selection: doc.selection_generation(view),
        }
    }
}

#[derive(Clone)]
struct BaseSyntax {
    document: DocumentId,
    base: Rope,
    language: Arc<LanguageConfiguration>,
    loader: Arc<Loader>,
    syntax: Option<Arc<Syntax>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffCursor {
    pub before: Range<u32>,
    pub row: usize,
    pub column: usize,
    /// Character offsets within this deletion, separate from editable selections.
    pub range: SelectionRange,
    document: DocumentId,
    version: i32,
    selection: u64,
    diff_key: ReviewKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentCursor {
    pub id: u64,
    pub range: SelectionRange,
    pub ranges: Selection,
    document: DocumentId,
    version: i32,
    selection: u64,
}

pub struct Deletion {
    pub before: Range<u32>,
    /// The newline immediately before the changed text, or EOF.
    pub anchor: usize,
    pub at_start: bool,
}

impl Deletion {
    pub fn height(&self) -> usize {
        self.before.len()
    }
}

pub struct DiffDisplay {
    document: DocumentId,
    version: i32,
    diff_key: ReviewKey,
    comments_generation: u64,
    pub layout_key: u64,
    pub base: Rope,
    pub hunks: Vec<Hunk>,
    pub deletions: Vec<Deletion>,
    pub blocks: Vec<ReviewBlock>,
}

impl DiffDisplay {
    pub fn deleted_row_offset(&self, before: &Range<u32>, row: usize) -> Option<usize> {
        let line = before.start + row as u32;
        self.blocks.iter().find_map(|block| match &block.content {
            ReviewBlockContent::Deleted { before, .. } if before.contains(&line) => {
                Some(block.offset + (line - before.start) as usize)
            }
            _ => None,
        })
    }

    pub fn comment_block(&self, id: u64) -> Option<&ReviewBlock> {
        self.blocks.iter().find(|block| matches!(&block.content, ReviewBlockContent::Comment { id: found, .. } if *found == id))
    }

    pub fn deleted_text(&self, before: &Range<u32>) -> RopeSlice<'_> {
        self.base.slice(
            self.base.line_to_char(before.start as usize)
                ..self.base.line_to_char(before.end as usize),
        )
    }
}

fn deleted_format(doc: &Document) -> TextFormat {
    TextFormat {
        tab_width: doc.tab_width() as u16,
        ..TextFormat::default()
    }
}

impl DiffMode {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn set_enabled(&mut self, doc: &mut Document, view: ViewId, enabled: bool) {
        if enabled {
            let generation = doc.review_comments_generation();
            if let Err(error) = doc.load_review_comments() {
                log::warn!("Cannot load review comments: {error:#}");
            }
            if doc.review_comments_generation() != generation {
                self.clear_cursor();
            }
        } else {
            self.clear_cursor();
        }
        self.enabled = enabled;
        doc.set_view_diff_mode(view, enabled);
    }

    pub fn cursor(&self, doc: &Document, view: ViewId) -> Option<&DiffCursor> {
        self.cursor.as_ref().filter(|cursor| {
            self.enabled
                && cursor.document == doc.id()
                && cursor.version == doc.version()
                && cursor.selection == doc.selection_generation(view)
                && doc.review_diff_key() == Some(cursor.diff_key)
        })
    }

    pub fn comment_cursor(&self, doc: &Document, view: ViewId) -> Option<&CommentCursor> {
        self.comment_cursor.as_ref().filter(|cursor| {
            self.enabled
                && cursor.document == doc.id()
                && cursor.version == doc.version()
                && cursor.selection == doc.selection_generation(view)
                && doc.review_comment(cursor.id).is_some()
                && self.display(doc).is_some_and(|display| display.comment_block(cursor.id).is_some_and(|block|
                    matches!(&block.content, ReviewBlockContent::Comment { text, .. } if cursor.ranges.iter().all(|range| range.to() <= text.len_chars()))))
        })
    }

    pub fn set_comment_cursor(
        &mut self,
        doc: &Document,
        view: ViewId,
        id: u64,
        range: SelectionRange,
    ) {
        self.set_comment_selection(doc, view, id, Selection::new(vec![range].into(), 0));
    }

    pub fn set_comment_selection(
        &mut self,
        doc: &Document,
        view: ViewId,
        id: u64,
        ranges: Selection,
    ) {
        if doc.review_comment(id).is_none() {
            return;
        }
        self.cursor = None;
        self.comment_cursor = Some(CommentCursor {
            id,
            range: ranges.primary(),
            ranges,
            document: doc.id(),
            version: doc.version(),
            selection: doc.selection_generation(view),
        });
        self.release_scroll();
    }

    pub fn select_comment(
        &mut self,
        doc: &Document,
        view: ViewId,
        select: impl FnOnce(RopeSlice, SelectionRange) -> SelectionRange,
    ) -> bool {
        let Some(cursor) = self.comment_cursor(doc, view).cloned() else {
            return false;
        };
        let text = Rope::from_str(&doc.review_comment(cursor.id).unwrap().text);
        let mut range = select(text.slice(..), cursor.range);
        range.anchor = range.anchor.min(text.len_chars());
        range.head = range.head.min(text.len_chars());
        range = range.grapheme_aligned(text.slice(..));
        self.set_comment_cursor(doc, view, cursor.id, range);
        true
    }

    pub fn set_cursor(
        &mut self,
        doc: &Document,
        view: ViewId,
        before: Range<u32>,
        row: usize,
        column: usize,
    ) {
        let Some(display) = self.display(doc) else {
            self.clear_cursor();
            return;
        };
        if !display.deletions.iter().any(|d| d.before == before) {
            self.clear_cursor();
            return;
        }
        let text = display.deleted_text(&before);
        let row = row.min(before.len().saturating_sub(1));
        let format = deleted_format(doc);
        let pos = char_idx_at_visual_offset(
            text,
            text.line_to_char(row),
            0,
            column,
            &format,
            &TextAnnotations::default(),
        )
        .0;
        let actual_column =
            visual_offset_from_block(text, pos, pos, &format, &TextAnnotations::default())
                .0
                .col;
        let mut range = SelectionRange::point(pos);
        range.old_visual_position = Some((0, column as u32));
        self.comment_cursor = None;
        self.cursor = Some(DiffCursor {
            before,
            row,
            column: actual_column,
            range,
            document: doc.id(),
            version: doc.version(),
            selection: doc.selection_generation(view),
            diff_key: display.diff_key,
        });
        self.release_scroll();
    }

    /// Use the same grapheme-aware selection operations as source text, bounded
    /// to one deleted block. Updating a review selection never changes Document.
    pub fn select(
        &mut self,
        doc: &Document,
        view: ViewId,
        select: impl FnOnce(RopeSlice, SelectionRange) -> SelectionRange,
    ) -> bool {
        let Some(mut cursor) = self.cursor(doc, view).cloned() else {
            return false;
        };
        let Some(display) = self.display(doc) else {
            return false;
        };
        if !display.deletions.iter().any(|d| d.before == cursor.before) {
            return false;
        }
        let text = display.deleted_text(&cursor.before);
        let mut range = select(text, cursor.range);
        range.anchor = range.anchor.min(text.len_chars());
        range.head = range.head.min(text.len_chars());
        range = range.grapheme_aligned(text);
        if range.is_empty() && range.head == text.len_chars() {
            range = SelectionRange::point(prev_grapheme_boundary(text, range.head));
        }
        let pos = range.cursor(text);
        cursor.row = text.char_to_line(pos);
        cursor.column = visual_offset_from_block(
            text,
            pos,
            pos,
            &deleted_format(doc),
            &TextAnnotations::default(),
        )
        .0
        .col;
        cursor.range = range;
        self.cursor = Some(cursor);
        self.release_scroll();
        true
    }

    pub fn selected_text(&self, doc: &Document, view: ViewId) -> Option<String> {
        let cursor = self.cursor(doc, view)?;
        let display = self.display(doc)?;
        display
            .deletions
            .iter()
            .find(|d| d.before == cursor.before)?;
        let text = display.deleted_text(&cursor.before);
        Some(cursor.range.min_width_1(text).fragment(text).into_owned())
    }

    pub fn clear_cursor(&mut self) {
        self.cursor = None;
        self.comment_cursor = None;
        self.release_scroll();
    }

    /// Review rows can fill the viewport without an editable cursor. Keep that
    /// position until the source or selection changes, instead of snapping past
    /// a long deletion to put the cursor back on screen.
    pub fn hold_scroll(&mut self, doc: &Document, view: ViewId) {
        self.scroll = Some(ReviewScroll::new(doc, view));
    }

    pub fn release_scroll(&mut self) {
        self.scroll = None;
    }

    pub fn preserves_scroll(&self, doc: &Document, view: ViewId) -> bool {
        self.enabled && self.scroll == Some(ReviewScroll::new(doc, view))
    }

    /// Parse the complete original file so deleted fragments retain syntax and
    /// injection context. Source edits reuse this tree while the base is unchanged.
    pub fn base_syntax(
        &self,
        doc: &Document,
        display: &DiffDisplay,
        loader: &Arc<Loader>,
    ) -> Option<Arc<Syntax>> {
        let mut cache = self.base_syntax.borrow_mut();
        let Some(language) = doc.language.as_ref().filter(|_| doc.syntax().is_some()) else {
            *cache = None;
            return None;
        };
        if let Some(cached) = cache.as_ref().filter(|cached| {
            cached.document == doc.id()
                && cached.base.is_instance(&display.base)
                && Arc::ptr_eq(&cached.language, language)
                && Arc::ptr_eq(&cached.loader, loader)
        }) {
            return cached.syntax.clone();
        }
        let syntax = Syntax::new(display.base.slice(..), language.language(), loader)
            .map_err(|err| {
                if err != helix_core::syntax::HighlighterError::NoRootConfig {
                    log::warn!(
                        "Error building diff base syntax for '{}': {err}",
                        doc.display_name()
                    );
                }
            })
            .ok()
            .map(Arc::new);
        *cache = Some(BaseSyntax {
            document: doc.id(),
            base: display.base.clone(),
            language: language.clone(),
            loader: loader.clone(),
            syntax: syntax.clone(),
        });
        syntax
    }

    /// Copy a published diff once. Rendering and positioning share this snapshot
    /// without holding the worker's lock or repeatedly copying its hunks.
    pub fn display(&self, doc: &Document) -> Option<Arc<DiffDisplay>> {
        if !self.enabled {
            return None;
        }
        let diff = doc.review_diff_handle().map(|handle| handle.load());
        // A worker can briefly lag behind edits. Its old line numbers must not
        // be used to insert virtual rows into the newer text.
        let current = diff
            .as_ref()
            .is_some_and(|diff| diff.doc().is_instance(doc.text()));
        if !current && doc.review_comments().is_empty() {
            return None;
        }
        let (revision, inverted) = diff.as_ref().map_or((0, false), |diff| diff.render_key());
        let diff_key = ReviewKey {
            revision,
            inverted,
            base_generation: doc.review_diff_generation(),
        };
        let mut cache = self.cache.borrow_mut();
        if let Some(display) = cache.as_ref().filter(|display| {
            display.document == doc.id()
                && display.version == doc.version()
                && display.diff_key == diff_key
                && display.comments_generation == doc.review_comments_generation()
        }) {
            return Some(display.clone());
        }
        // Rope includes an empty sentinel line after a final newline. It is
        // useful to the editor, but is not an extra added/deleted source row.
        let source_lines = |text: &Rope| {
            text.len_lines() as u32 - u32::from(text.line(text.len_lines() - 1).len_chars() == 0)
        };
        let base = diff
            .as_ref()
            .map_or_else(|| doc.text().clone(), |diff| diff.diff_base().clone());
        let before_lines = source_lines(&base);
        let after_lines = source_lines(doc.text());
        let hunks: Vec<_> = (0..diff
            .as_ref()
            .filter(|_| current)
            .map_or(0, |diff| diff.len()))
            .filter_map(|n| {
                let mut hunk = diff.as_ref().unwrap().nth_hunk(n);
                hunk.before =
                    hunk.before.start.min(before_lines)..hunk.before.end.min(before_lines);
                hunk.after = hunk.after.start.min(after_lines)..hunk.after.end.min(after_lines);
                (!hunk.before.is_empty() || !hunk.after.is_empty()).then_some(hunk)
            })
            .collect();
        let deletions: Vec<Deletion> = hunks
            .iter()
            .filter(|hunk| !hunk.before.is_empty())
            .map(|hunk| {
                let at_start = hunk.after.start == 0;
                let line_start = doc
                    .text()
                    .try_line_to_char(hunk.after.start as usize)
                    .unwrap_or(doc.text().len_chars());
                Deletion {
                    before: hunk.before.clone(),
                    anchor: if line_start > 0
                        && helix_core::chars::char_is_line_ending(doc.text().char(line_start - 1))
                    {
                        prev_grapheme_boundary(doc.text().slice(..), line_start)
                    } else {
                        line_start
                    },
                    at_start,
                }
            })
            .collect();
        let blocks = review_blocks(doc, &base, &deletions);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        doc.id().hash(&mut hasher);
        blocks.len().hash(&mut hasher);
        for block in &blocks {
            block.anchor.hash(&mut hasher);
            block.at_start.hash(&mut hasher);
            block.height().hash(&mut hasher);
        }
        let display = Arc::new(DiffDisplay {
            document: doc.id(),
            version: doc.version(),
            diff_key,
            comments_generation: doc.review_comments_generation(),
            layout_key: hasher.finish(),
            base,
            hunks,
            deletions,
            blocks,
        });
        *cache = Some(display.clone());
        Some(display)
    }
}

pub struct DiffAnnotation {
    display: Arc<DiffDisplay>,
    next: usize,
    pending: usize,
}

impl DiffAnnotation {
    pub fn new(display: Arc<DiffDisplay>) -> Box<Self> {
        Box::new(Self {
            display,
            next: 0,
            pending: 0,
        })
    }

    fn next_anchor(&self) -> usize {
        self.display
            .blocks
            .get(self.next)
            .map_or(usize::MAX, |d| d.anchor)
    }
}

impl LineAnnotation for DiffAnnotation {
    fn checkpoint_key(&self) -> Option<u64> {
        Some(self.display.layout_key)
    }

    fn checkpoint(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        Some(Arc::new((self.next, self.pending)))
    }

    fn restore_checkpoint(&mut self, state: &(dyn Any + Send + Sync)) -> bool {
        let Some(&(next, pending)) = state.downcast_ref::<(usize, usize)>() else {
            return false;
        };
        self.next = next;
        self.pending = pending;
        true
    }

    fn leading_virtual_lines(&self) -> usize {
        self.display
            .blocks
            .iter()
            .take_while(|block| block.at_start)
            .map(ReviewBlock::height)
            .sum()
    }

    fn reset_pos(&mut self, char_idx: usize) -> usize {
        self.next = self
            .display
            .blocks
            .partition_point(|d| d.at_start || d.anchor < char_idx);
        self.pending = 0;
        self.next_anchor()
    }

    fn process_anchor(&mut self, _grapheme: &FormattedGrapheme) -> usize {
        self.pending += self.display.blocks[self.next].height();
        self.next += 1;
        self.next_anchor()
    }

    fn insert_virtual_lines(&mut self, _char_idx: usize, _pos: Position, _line: usize) -> Position {
        Position::new(std::mem::take(&mut self.pending), 0)
    }
}

#[cfg(test)]
mod tests;
