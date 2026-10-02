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
    visual_offset_from_block, Position, Range as SelectionRange, Rope, RopeSlice,
};
use helix_vcs::Hunk;

use crate::{Document, DocumentId, ViewId};

/// Review display state belongs to a view, independently of the editable text.
#[derive(Clone, Default)]
pub struct DiffMode {
    pub enabled: bool,
    scroll: Option<(DocumentId, i32, u64)>,
    cursor: Option<DiffCursor>,
    cache: RefCell<Option<Arc<DiffDisplay>>>,
    base_syntax: RefCell<Option<BaseSyntax>>,
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
    diff_key: (u64, bool),
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
    diff_key: (u64, bool),
    pub layout_key: u64,
    pub base: Rope,
    pub hunks: Vec<Hunk>,
    pub deletions: Vec<Deletion>,
}

impl DiffDisplay {
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
    pub fn cursor(&self, doc: &Document, view: ViewId) -> Option<&DiffCursor> {
        self.cursor.as_ref().filter(|cursor| {
            self.enabled
                && cursor.document == doc.id()
                && cursor.version == doc.version()
                && cursor.selection == doc.selection_generation(view)
                && doc.diff_handle().map(|diff| diff.render_key()) == Some(cursor.diff_key)
        })
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
        self.release_scroll();
    }

    /// Review rows can fill the viewport without an editable cursor. Keep that
    /// position until the source or selection changes, instead of snapping past
    /// a long deletion to put the cursor back on screen.
    pub fn hold_scroll(&mut self, doc: &Document, view: ViewId) {
        self.scroll = Some((doc.id(), doc.version(), doc.selection_generation(view)));
    }

    pub fn release_scroll(&mut self) {
        self.scroll = None;
    }

    pub fn preserves_scroll(&self, doc: &Document, view: ViewId) -> bool {
        self.enabled
            && self.scroll == Some((doc.id(), doc.version(), doc.selection_generation(view)))
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
        let diff = doc.diff_handle()?.load();
        // A worker can briefly lag behind edits. Its old line numbers must not
        // be used to insert virtual rows into the newer text.
        if !diff.doc().is_instance(doc.text()) {
            return None;
        }
        let diff_key = diff.render_key();
        let mut cache = self.cache.borrow_mut();
        if let Some(display) = cache.as_ref().filter(|display| {
            display.document == doc.id()
                && display.version == doc.version()
                && display.diff_key == diff_key
        }) {
            return Some(display.clone());
        }
        // Rope includes an empty sentinel line after a final newline. It is
        // useful to the editor, but is not an extra added/deleted source row.
        let source_lines = |text: &Rope| {
            text.len_lines() as u32 - u32::from(text.line(text.len_lines() - 1).len_chars() == 0)
        };
        let before_lines = source_lines(diff.diff_base());
        let after_lines = source_lines(doc.text());
        let hunks: Vec<_> = (0..diff.len())
            .filter_map(|n| {
                let mut hunk = diff.nth_hunk(n);
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
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        doc.id().hash(&mut hasher);
        deletions.len().hash(&mut hasher);
        for deletion in &deletions {
            deletion.anchor.hash(&mut hasher);
            deletion.at_start.hash(&mut hasher);
            deletion.height().hash(&mut hasher);
        }
        let display = Arc::new(DiffDisplay {
            document: doc.id(),
            version: doc.version(),
            diff_key,
            layout_key: hasher.finish(),
            base: diff.diff_base().clone(),
            hunks,
            deletions,
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
            .deletions
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
            .deletions
            .first()
            .filter(|d| d.at_start)
            .map_or(0, Deletion::height)
    }

    fn reset_pos(&mut self, char_idx: usize) -> usize {
        self.next = self
            .display
            .deletions
            .partition_point(|d| d.at_start || d.anchor < char_idx);
        self.pending = 0;
        self.next_anchor()
    }

    fn process_anchor(&mut self, _grapheme: &FormattedGrapheme) -> usize {
        self.pending += self.display.deletions[self.next].height();
        self.next += 1;
        self.next_anchor()
    }

    fn insert_virtual_lines(&mut self, _char_idx: usize, _pos: Position, _line: usize) -> Position {
        Position::new(std::mem::take(&mut self.pending), 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{editor::Config, graphics::Rect, View};
    use arc_swap::ArcSwap;
    use helix_core::{
        char_idx_at_visual_offset, doc_formatter::DocumentFormatter, syntax,
        visual_offset_from_anchor, Selection, Transaction,
    };

    async fn fixture(base: &str, text: &str) -> (Document, View) {
        let config = Config::default();
        let mut doc = Document::from(
            Rope::from_str(text),
            None,
            Arc::new(ArcSwap::from_pointee(config.clone())),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );
        let mut view = View::new(doc.id(), config.gutters);
        view.area = Rect::new(0, 0, 80, 24);
        view.diff_mode.enabled = true;
        doc.ensure_view_init(view.id);
        doc.set_selection(view.id, Selection::point(0));
        doc.set_diff_base(base.as_bytes().to_vec());
        wait_diff(&doc).await;
        (doc, view)
    }

    async fn wait_diff(doc: &Document) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let ready = {
                    let diff = doc.diff_handle().unwrap().load();
                    diff.render_key().0 != 0 && diff.doc().is_instance(doc.text())
                };
                if ready {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn review_selection_invalidates_when_the_base_changes_without_a_source_edit() {
        let (mut doc, mut view) =
            fixture("keep\na long deleted line\nsame\n", "keep\nsame\n").await;
        assert!(view.move_diff_cursor(&mut doc, true, 1));
        assert!(view
            .diff_mode
            .select(&doc, view.id, |text, _| SelectionRange::new(
                0,
                text.len_chars()
            )));
        assert_eq!(
            view.diff_mode.selected_text(&doc, view.id).unwrap(),
            "a long deleted line\n"
        );
        let version = doc.version();
        let original = doc.text().clone();
        let generation = doc.diff_handle().unwrap().render_key();
        doc.set_diff_base(b"keep\nx\nsame\n".to_vec());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while doc.diff_handle().unwrap().render_key() == generation {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert!(view.diff_mode.cursor(&doc, view.id).is_none());
        assert!(view.diff_mode.selected_text(&doc, view.id).is_none());
        assert_eq!(doc.version(), version);
        assert!(doc.text().is_instance(&original));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn review_cursor_navigates_all_deletions_at_the_start() {
        let (mut doc, mut view) =
            fixture("one\ntwo\nthree\nfour\nsame\nlast\n", "same\nlast\n").await;
        let original = doc.text().clone();
        assert!(view.move_diff_cursor(&mut doc, false, 1));
        assert_eq!(view.diff_mode.cursor(&doc, view.id).unwrap().row, 3);
        assert_eq!(view.diff_cursor_screen_coords(&doc).unwrap().row, 3);
        assert_eq!(
            view.diff_mode.display(&doc).unwrap().deletions[0].height(),
            4
        );
        assert!(view.move_diff_cursor(&mut doc, false, 2));
        assert_eq!(view.diff_mode.cursor(&doc, view.id).unwrap().row, 1);
        assert!(view.move_diff_cursor(&mut doc, true, 3));
        assert!(view.diff_mode.cursor(&doc, view.id).is_none());
        assert_eq!(
            doc.selection(view.id)
                .primary()
                .cursor(doc.text().slice(..)),
            0
        );
        assert!(view.move_diff_cursor(&mut doc, false, 100));
        assert_eq!(view.diff_mode.cursor(&doc, view.id).unwrap().row, 0);
        assert!(doc.text().is_instance(&original));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_deleted_rows_are_visible_and_navigable() {
        let (mut doc, mut view) = fixture("first\nold1\nold2\nlast\n", "first\nlast\n").await;
        assert_eq!(
            view.diff_mode.display(&doc).unwrap().deletions[0].height(),
            2
        );
        for row in [0, 1] {
            assert!(view.move_diff_cursor(&mut doc, true, 1));
            assert_eq!(view.diff_mode.cursor(&doc, view.id).unwrap().row, row);
            assert_eq!(view.diff_cursor_screen_coords(&doc).unwrap().row, row + 1);
        }
        assert!(view.move_diff_cursor(&mut doc, true, 1));
        assert!(view.diff_mode.cursor(&doc, view.id).is_none());
        assert_eq!(
            doc.selection(view.id)
                .primary()
                .cursor(doc.text().slice(..)),
            6
        );
        assert!(view.move_diff_cursor(&mut doc, false, 1));
        assert_eq!(view.diff_mode.cursor(&doc, view.id).unwrap().row, 1);
        assert!(view.move_diff_cursor(&mut doc, false, 2));
        assert!(view.diff_mode.cursor(&doc, view.id).is_none());
        assert_eq!(
            doc.selection(view.id)
                .primary()
                .cursor(doc.text().slice(..)),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn review_cursor_scrolls_through_large_deletions_without_changing_source() {
        let base = format!("first\n{}last\n", "old\n".repeat(70_000));
        let (mut doc, mut view) = fixture(&base, "first\nlast\n").await;
        assert!(view.move_diff_cursor(&mut doc, true, 1));
        assert!(view.move_diff_cursor(&mut doc, true, 65_540));
        assert_eq!(view.diff_mode.cursor(&doc, view.id).unwrap().row, 65_540);
        view.ensure_cursor_in_view(&mut doc, 3);
        let pos = view.diff_cursor_screen_coords(&doc).unwrap();
        assert!(pos.row >= 3 && pos.row < view.inner_height() - 3);
        assert!(doc.view_offset(view.id).vertical_offset > 65_000);
        view.diff_mode.release_scroll();
        view.ensure_cursor_in_view(&mut doc, 3);
        assert!(view.diff_cursor_screen_coords(&doc).is_some());
        assert_eq!(doc.text().to_string(), "first\nlast\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn leaving_large_leading_deletions_keeps_the_source_cursor_visible() {
        let base = format!("{}same\n", "old\n".repeat(70_000));
        let (mut doc, mut view) = fixture(&base, "same\n").await;
        assert!(view.move_diff_cursor(&mut doc, false, 70_000));
        assert_eq!(view.diff_mode.cursor(&doc, view.id).unwrap().row, 0);
        assert!(view.move_diff_cursor(&mut doc, true, 70_000));
        assert!(view.diff_mode.cursor(&doc, view.id).is_none());
        view.ensure_cursor_in_view(&mut doc, 3);
        assert!(view
            .screen_coords_at_pos(&doc, doc.text().slice(..), 0)
            .is_some());
        assert!(doc.view_offset(view.id).vertical_offset > 69_000);
        assert_eq!(doc.text().to_string(), "same\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn review_cursor_handles_empty_documents_crlf_and_invalidates_on_edits() {
        for (base, text) in [
            ("old\nmore\nlast\n", ""),
            ("first\r\nold\r\nmore\r\nlast\r\n", "first\r\nlast\r\n"),
        ] {
            let (mut doc, mut view) = fixture(base, text).await;
            assert!(view.move_diff_cursor(&mut doc, !text.is_empty(), 1));
            assert!(view.diff_mode.cursor(&doc, view.id).is_some());
            view.ensure_cursor_in_view(&mut doc, 2);
            assert!(view.diff_cursor_screen_coords(&doc).is_some());
            let edit = Transaction::change(doc.text(), [(0, 0, Some("new\n".into()))].into_iter());
            assert!(doc.apply(&edit, view.id));
            assert!(view.diff_mode.cursor(&doc, view.id).is_none());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleted_rows_before_first_line_and_between_hunks_have_consistent_positions() {
        let (doc, view) = fixture("gone\nsame\nold\nlast\n", "same\nnew\nlast\n").await;
        let original = doc.text().clone();
        for _ in 0..2 {
            let display = view.diff_mode.display(&doc).unwrap();
            assert!(Arc::ptr_eq(
                &display,
                &view.diff_mode.display(&doc).unwrap()
            ));
            let annotations = view.text_annotations(&doc, None);
            let format = doc.text_format(80, None);
            let first_row = 1;
            assert_eq!(
                visual_offset_from_anchor(doc.text().slice(..), 0, 0, &format, &annotations, 20)
                    .unwrap()
                    .0
                    .row,
                first_row
            );
            let positions: Vec<_> = DocumentFormatter::new_at_prev_checkpoint(
                doc.text().slice(..),
                &format,
                &annotations,
                0,
            )
            .filter(|g| !g.is_virtual() && g.visual_pos.col == 0)
            .map(|g| g.visual_pos.row)
            .collect();
            assert_eq!(positions, vec![1, 3, 4, 5]);
            assert_eq!(
                char_idx_at_visual_offset(
                    doc.text().slice(..),
                    0,
                    first_row as isize,
                    0,
                    &format,
                    &annotations
                ),
                (0, 0)
            );
            let inner = view.inner_area(&doc);
            assert_eq!(
                view.pos_at_screen_coords(&doc, inner.y, inner.x, true),
                None
            );
            assert_eq!(
                view.diff_deletion_at_screen_coords(&doc, inner.y, inner.x),
                Some(0..1)
            );
            let marker = inner.y + first_row as u16 + 1;
            assert_eq!(view.pos_at_screen_coords(&doc, marker, inner.x, true), None);
            assert_eq!(
                view.diff_deletion_at_screen_coords(&doc, marker, inner.x),
                Some(2..3)
            );
        }
        assert!(doc.text().is_instance(&original));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn diff_visibility_is_per_view_and_stays_complete_after_edits() {
        let (mut doc, view) = fixture("same\nold1\nold2\nold3\nlast\n", "same\nnew\nlast\n").await;
        let mut other = view.clone();
        other.diff_mode.enabled = false;
        assert_eq!(
            view.diff_mode.display(&doc).unwrap().deletions[0].height(),
            3
        );
        assert!(other.diff_mode.display(&doc).is_none());
        let edit = Transaction::change(doc.text(), [(0, 0, Some("added\n".into()))].into_iter());
        assert!(doc.apply(&edit, view.id));
        // The old snapshot is inapplicable until the worker publishes this edit.
        assert!(view.diff_mode.display(&doc).is_none());
        wait_diff(&doc).await;
        assert_eq!(
            view.diff_mode.display(&doc).unwrap().deletions[0].height(),
            3
        );
        assert!(other.diff_mode.display(&doc).is_none());
        other.diff_mode.enabled = true;
        assert_eq!(
            other.diff_mode.display(&doc).unwrap().deletions[0].height(),
            3
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrapped_checkpoints_preserve_deletion_anchors_and_cursor_mapping() {
        let long = "界e\u{301}\tword ".repeat(900);
        let (doc, view) = fixture(
            &format!("{long}\nremoved\nlast\n"),
            &format!("{long}\nlast\n"),
        )
        .await;
        for width in [40, 80] {
            let annotations = view.text_annotations(&doc, None);
            let mut cached = doc.text_format(width, None);
            cached.soft_wrap = true;
            let mut plain = cached.clone();
            plain.checkpoint_cache = None;
            DocumentFormatter::new_at_prev_checkpoint(
                doc.text().slice(..),
                &cached,
                &annotations,
                0,
            )
            .for_each(drop);
            let target = doc.text().line_to_char(1) - 50;
            let resumed = DocumentFormatter::new_at_prev_checkpoint(
                doc.text().slice(..),
                &cached,
                &annotations,
                target,
            );
            assert!(resumed.next_char_pos() > target - 2048);
            let actual: Vec<_> = resumed.map(|g| (g.char_idx, g.visual_pos)).collect();
            let expected: Vec<_> = DocumentFormatter::new_at_prev_checkpoint(
                doc.text().slice(..),
                &plain,
                &annotations,
                0,
            )
            .filter(|g| g.char_idx >= actual[0].0)
            .map(|g| (g.char_idx, g.visual_pos))
            .collect();
            assert_eq!(actual, expected);
        }
    }
}
