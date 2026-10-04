use std::sync::Arc;

use helix_core::{
    doc_formatter::FormattedGrapheme,
    graphemes::Grapheme,
    syntax::{Loader, Syntax},
    unicode::segmentation::UnicodeSegmentation,
    Position,
};
use helix_view::{
    annotations::{
        diff::{CommentCursor, DiffCursor, DiffDisplay},
        review_comments::{ReviewBlock, ReviewBlockContent},
    },
    document::{review_comments::CommentSide, Mode},
    graphics::{Color, Rect},
    theme::Style,
    Theme,
};

use super::Decoration;
use crate::ui::document::{LinePos, SyntaxHighlighter, TextRenderer};

pub struct ReviewCursors {
    pub deleted: Option<DiffCursor>,
    pub comment: Option<CommentCursor>,
    pub mode: Mode,
}

struct RowPaint<'a> {
    selection: &'a [helix_core::Range],
    cursors: &'a [usize],
    background: Style,
    deleted: bool,
    side: CommentSide,
}

pub struct DiffDecoration<'a> {
    display: Arc<DiffDisplay>,
    syntax: Option<Arc<Syntax>>,
    loader: &'a Loader,
    theme: &'a Theme,
    added: Style,
    deleted: Style,
    added_gutter: Style,
    deleted_gutter: Style,
    selection_style: Style,
    comment_selection_style: Style,
    secondary_cursor_style: Style,
    cursor: Option<DiffCursor>,
    comment_cursor: Option<CommentCursor>,
    comment: Style,
    active_comment: Style,
    reference: Option<(CommentSide, std::ops::Range<usize>)>,
    reference_style: Style,
    gutter_width: u16,
    tab_width: u16,
    next: usize,
    pending: Option<std::ops::Range<usize>>,
}

impl<'a> DiffDecoration<'a> {
    pub fn new(
        display: Arc<DiffDisplay>,
        theme: &'a Theme,
        gutter_width: u16,
        tab_width: u16,
        cursors: ReviewCursors,
        syntax: Option<Arc<Syntax>>,
        loader: &'a Loader,
    ) -> Self {
        let light = match theme.get("ui.background").bg {
            Some(Color::Rgb(r, g, b)) => (r as u32 + g as u32 + b as u32) > 384,
            _ => false,
        };
        let reference = cursors
            .comment
            .as_ref()
            .and_then(|cursor| display.comment_block(cursor.id))
            .and_then(|block| match &block.content {
                ReviewBlockContent::Comment { side, range, .. } => Some((*side, range.clone())),
                _ => None,
            });
        let comment = theme.try_get_exact("ui.review.comment").unwrap_or_else(|| {
            let text = theme.get("ui.background").patch(theme.get("ui.text"));
            Style {
                fg: text.bg,
                bg: text.fg,
                ..Style::default()
            }
        });
        let reference_style = theme
            .try_get_exact("ui.review.reference")
            .unwrap_or_else(|| theme.get("ui.selection.primary"));
        let active_comment = theme
            .try_get_exact("ui.review.comment.active")
            .unwrap_or(comment);
        Self {
            display,
            syntax,
            loader,
            theme,
            added: theme.try_get("ui.diff.added").unwrap_or_else(|| {
                Style::default().bg(if light {
                    Color::Rgb(214, 242, 214)
                } else {
                    Color::Rgb(28, 57, 37)
                })
            }),
            deleted: theme.try_get("ui.diff.deleted").unwrap_or_else(|| {
                Style::default().bg(if light {
                    Color::Rgb(250, 218, 218)
                } else {
                    Color::Rgb(65, 32, 36)
                })
            }),
            added_gutter: theme.get("diff.plus.gutter"),
            deleted_gutter: theme.get("diff.minus.gutter"),
            selection_style: theme.get("ui.selection.primary"),
            comment_selection_style: Style::reset()
                .patch(theme.get("ui.background"))
                .patch(theme.get("ui.text"))
                .patch(theme.get("ui.selection.primary")),
            secondary_cursor_style: theme.get(match cursors.mode {
                Mode::Insert => "ui.cursor.insert",
                Mode::Select => "ui.cursor.select",
                Mode::Normal => "ui.cursor.normal",
            }),
            cursor: cursors.deleted,
            comment_cursor: cursors.comment,
            comment,
            active_comment,
            reference,
            reference_style,
            gutter_width,
            tab_width,
            next: 0,
            pending: None,
        }
    }

    fn next_anchor(&self) -> usize {
        self.display
            .blocks
            .get(self.next)
            .map_or(usize::MAX, |d| d.anchor)
    }

    fn virtual_row(
        &self,
        renderer: &mut TextRenderer,
        row: usize,
        text: &str,
        mut char_idx: usize,
        highlighter: &mut SyntaxHighlighter<'_, '_, '_>,
        paint: RowPaint<'_>,
    ) {
        if row < renderer.offset.row
            || row >= renderer.offset.row + renderer.viewport.height as usize
        {
            return;
        }
        let y = renderer.viewport.y + (row - renderer.offset.row) as u16;
        let x = renderer.viewport.x - self.gutter_width;
        let style = renderer.text_style.patch(paint.background);
        renderer.surface.set_style(
            Rect::new(x, y, renderer.viewport.width + self.gutter_width, 1),
            style,
        );
        if self.gutter_width != 0 && paint.deleted {
            renderer
                .surface
                .set_string(x, y, "-", style.patch(self.deleted_gutter));
        }
        // Deleted source rows are clipped horizontally, just like unwrapped
        // editor text. Traverse graphemes so tabs and wide characters align.
        let mut col = 0;
        for raw in text.graphemes(true) {
            let grapheme = Grapheme::new(raw.into(), col, self.tab_width);
            let selected = paint.selection.iter().any(|range| range.contains(char_idx));
            let mut style = highlighter.style_at(char_idx).patch(paint.background);
            if paint.deleted
                && self
                    .reference
                    .as_ref()
                    .is_some_and(|(reference_side, range)| {
                        *reference_side == paint.side && range.contains(&char_idx)
                    })
            {
                style = style.patch(self.reference_style);
            }
            let mut grapheme_style = if selected {
                style.patch(if paint.deleted {
                    self.selection_style
                } else {
                    self.comment_selection_style
                })
            } else {
                style
            };
            if paint.cursors.contains(&char_idx) {
                grapheme_style = grapheme_style.patch(self.secondary_cursor_style);
            }
            if grapheme == Grapheme::Newline {
                if (selected || paint.cursors.contains(&char_idx))
                    && renderer.column_in_bounds(col, 1)
                {
                    renderer.surface.set_string(
                        renderer.viewport.x + (col - renderer.offset.col) as u16,
                        y,
                        " ",
                        grapheme_style,
                    );
                }
                break;
            }
            let width = grapheme.width();
            if col >= renderer.offset.col + renderer.viewport.width as usize {
                break;
            }
            if renderer.column_in_bounds(col, width) {
                let x = renderer.viewport.x + (col - renderer.offset.col) as u16;
                if matches!(grapheme, Grapheme::Tab { .. }) {
                    renderer.surface.set_stringn(
                        x,
                        y,
                        &renderer.virtual_tab,
                        width,
                        grapheme_style,
                    );
                } else {
                    renderer
                        .surface
                        .set_grapheme(x, y, raw, width, grapheme_style);
                }
            } else if matches!(grapheme, Grapheme::Tab { .. }) && col < renderer.offset.col {
                let visible = (col + width)
                    .saturating_sub(renderer.offset.col)
                    .min(renderer.viewport.width as usize);
                renderer.surface.set_stringn(
                    renderer.viewport.x,
                    y,
                    &renderer.virtual_tab,
                    visible,
                    grapheme_style,
                );
            }
            col += width;
            char_idx += raw.chars().count();
        }
        if paint.cursors.contains(&char_idx) && renderer.column_in_bounds(col, 1) {
            renderer.surface.set_style(
                Rect::new(
                    renderer.viewport.x + (col - renderer.offset.col) as u16,
                    y,
                    1,
                    1,
                ),
                self.secondary_cursor_style,
            );
        }
    }

    fn deletion(
        &self,
        renderer: &mut TextRenderer,
        index: usize,
        before: &std::ops::Range<u32>,
        row: usize,
    ) -> usize {
        let deletion = &self.display.deletions[index];
        let height = before.len();
        let start = self
            .display
            .base
            .line_to_char(deletion.before.start as usize);
        let selection = self
            .cursor
            .as_ref()
            .filter(|cursor| cursor.before == deletion.before && !cursor.range.is_empty())
            .map(|cursor| {
                helix_core::Range::new(cursor.range.anchor + start, cursor.range.head + start)
            });
        // Only materialize old text that is actually visible in this view.
        let first = renderer.offset.row.saturating_sub(row).min(height);
        let end = (renderer.offset.row + renderer.viewport.height as usize)
            .saturating_sub(row)
            .min(height);
        if first >= end {
            return height;
        }
        let base = self.display.base.slice(..);
        let range = self
            .display
            .base
            .line_to_byte(before.start as usize + first) as u32
            ..self.display.base.line_to_byte(before.start as usize + end) as u32;
        let highlighter = self
            .syntax
            .as_ref()
            .map(|syntax| syntax.display_highlighter(base, self.loader, range));
        let mut highlighter =
            SyntaxHighlighter::new(highlighter, base, self.theme, renderer.text_style);
        for i in first..end {
            let line = before.start as usize + i;
            let text = self.display.base.line(line).to_string();
            self.virtual_row(
                renderer,
                row + i,
                &text,
                self.display.base.line_to_char(line),
                &mut highlighter,
                RowPaint {
                    selection: selection.as_slice(),
                    cursors: &[],
                    background: self.deleted,
                    deleted: true,
                    side: CommentSide::Base,
                },
            );
        }
        height
    }

    fn block(&self, renderer: &mut TextRenderer, block: &ReviewBlock, row: usize) -> usize {
        match &block.content {
            ReviewBlockContent::Deleted { deletion, before } => {
                self.deletion(renderer, *deletion, before, row)
            }
            ReviewBlockContent::Comment {
                id,
                text,
                side,
                range,
            } => {
                let active =
                    self.reference
                        .as_ref()
                        .is_some_and(|(reference_side, reference_range)| {
                            side == reference_side
                                && (range == reference_range
                                    || range.start < reference_range.end
                                        && reference_range.start < range.end)
                        });
                let style = if active {
                    self.active_comment
                } else {
                    self.comment
                };
                let selection = self
                    .comment_cursor
                    .as_ref()
                    .filter(|cursor| cursor.id == *id)
                    .map_or(&[][..], |cursor| cursor.ranges.ranges());
                let cursors: Vec<_> = self
                    .comment_cursor
                    .as_ref()
                    .filter(|cursor| cursor.id == *id)
                    .into_iter()
                    .flat_map(|cursor| {
                        cursor
                            .ranges
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| *index != cursor.ranges.primary_index())
                            .map(|(_, range)| range.cursor(text.slice(..)))
                    })
                    .collect();
                let mut highlighter =
                    SyntaxHighlighter::new(None, text.slice(..), self.theme, renderer.text_style);
                let first = renderer
                    .offset
                    .row
                    .saturating_sub(row)
                    .min(text.len_lines());
                let end = (renderer.offset.row + renderer.viewport.height as usize)
                    .saturating_sub(row)
                    .min(text.len_lines());
                for line in first..end {
                    self.virtual_row(
                        renderer,
                        row + line,
                        &text.line(line).to_string(),
                        text.line_to_char(line),
                        &mut highlighter,
                        RowPaint {
                            selection,
                            cursors: &cursors,
                            background: style,
                            deleted: false,
                            side: *side,
                        },
                    );
                }
                text.len_lines()
            }
        }
    }
}

impl Decoration for DiffDecoration<'_> {
    fn render_leading_lines(&mut self, renderer: &mut TextRenderer, row: usize) -> usize {
        self.display
            .blocks
            .iter()
            .take_while(|block| block.at_start)
            .fold(0, |offset, block| {
                offset + self.block(renderer, block, row + offset)
            })
    }

    fn reset_pos(&mut self, pos: usize) -> usize {
        self.next = self
            .display
            .blocks
            .partition_point(|d| d.at_start || d.anchor < pos);
        self.pending = None;
        self.next_anchor()
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        _grapheme: &FormattedGrapheme,
    ) -> usize {
        let pending = self.pending.get_or_insert(self.next..self.next);
        pending.end += 1;
        self.next += 1;
        self.next_anchor()
    }

    fn decorate_line(&mut self, renderer: &mut TextRenderer, pos: LinePos) {
        let idx = self
            .display
            .hunks
            .partition_point(|h| h.after.end <= pos.doc_line as u32);
        if self
            .display
            .hunks
            .get(idx)
            .is_some_and(|h| h.after.contains(&(pos.doc_line as u32)))
        {
            renderer.line_style = self.added;
            let x = renderer.viewport.x - self.gutter_width;
            renderer.set_row_style(
                x,
                pos.visual_line,
                renderer.viewport.width + self.gutter_width,
                self.added,
            );
            if self.gutter_width != 0 && pos.first_visual_line {
                renderer.set_string(
                    x,
                    pos.visual_line,
                    "+",
                    renderer
                        .text_style
                        .patch(self.added)
                        .patch(self.added_gutter),
                );
            }
        }
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let Some(pending) = self.pending.take() else {
            return Position::default();
        };
        let height = self.display.blocks[pending]
            .iter()
            .fold(0, |offset, block| {
                offset + self.block(renderer, block, pos.visual_line + virt_off.row + offset)
            });
        Position::new(height, 0)
    }
}
