use std::sync::Arc;

use helix_core::{
    doc_formatter::FormattedGrapheme,
    graphemes::Grapheme,
    syntax::{Loader, Syntax},
    unicode::segmentation::UnicodeSegmentation,
    Position,
};
use helix_view::{
    annotations::diff::{Deletion, DiffCursor, DiffDisplay},
    graphics::{Color, Rect},
    theme::Style,
    Theme,
};

use super::Decoration;
use crate::ui::document::{LinePos, SyntaxHighlighter, TextRenderer};

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
    cursor: Option<DiffCursor>,
    gutter_width: u16,
    tab_width: u16,
    next: usize,
    pending: Option<usize>,
}

impl<'a> DiffDecoration<'a> {
    pub fn new(
        display: Arc<DiffDisplay>,
        theme: &'a Theme,
        gutter_width: u16,
        tab_width: u16,
        cursor: Option<DiffCursor>,
        syntax: Option<Arc<Syntax>>,
        loader: &'a Loader,
    ) -> Self {
        let light = match theme.get("ui.background").bg {
            Some(Color::Rgb(r, g, b)) => (r as u32 + g as u32 + b as u32) > 384,
            _ => false,
        };
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
            cursor,
            gutter_width,
            tab_width,
            next: 0,
            pending: None,
        }
    }

    fn next_anchor(&self) -> usize {
        self.display
            .deletions
            .get(self.next)
            .map_or(usize::MAX, |d| d.anchor)
    }

    fn deleted_row(
        &self,
        renderer: &mut TextRenderer,
        row: usize,
        text: &str,
        mut char_idx: usize,
        selection: Option<helix_core::Range>,
        highlighter: &mut SyntaxHighlighter<'_, '_, '_>,
    ) {
        if row < renderer.offset.row
            || row >= renderer.offset.row + renderer.viewport.height as usize
        {
            return;
        }
        let y = renderer.viewport.y + (row - renderer.offset.row) as u16;
        let x = renderer.viewport.x - self.gutter_width;
        let style = renderer.text_style.patch(self.deleted);
        renderer.surface.set_style(
            Rect::new(x, y, renderer.viewport.width + self.gutter_width, 1),
            style,
        );
        if self.gutter_width != 0 {
            renderer
                .surface
                .set_string(x, y, "-", style.patch(self.deleted_gutter));
        }
        // Deleted source rows are clipped horizontally, just like unwrapped
        // editor text. Traverse graphemes so tabs and wide characters align.
        let mut col = 0;
        for raw in text.graphemes(true) {
            let grapheme = Grapheme::new(raw.into(), col, self.tab_width);
            let selected = selection.is_some_and(|range| range.contains(char_idx));
            let style = highlighter.style_at(char_idx).patch(self.deleted);
            let grapheme_style = if selected {
                style.patch(self.selection_style)
            } else {
                style
            };
            if grapheme == Grapheme::Newline {
                if selected && renderer.column_in_bounds(col, 1) {
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
    }

    fn deletion(&self, renderer: &mut TextRenderer, deletion: &Deletion, row: usize) -> usize {
        let height = deletion.height();
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
            .line_to_byte(deletion.before.start as usize + first) as u32
            ..self
                .display
                .base
                .line_to_byte(deletion.before.start as usize + end) as u32;
        let highlighter = self
            .syntax
            .as_ref()
            .map(|syntax| syntax.display_highlighter(base, self.loader, range));
        let mut highlighter =
            SyntaxHighlighter::new(highlighter, base, self.theme, renderer.text_style);
        for i in first..end {
            let line = deletion.before.start as usize + i;
            let text = self.display.base.line(line).to_string();
            self.deleted_row(
                renderer,
                row + i,
                &text,
                self.display.base.line_to_char(line),
                selection,
                &mut highlighter,
            );
        }
        deletion.height()
    }
}

impl Decoration for DiffDecoration<'_> {
    fn render_leading_lines(&mut self, renderer: &mut TextRenderer, row: usize) -> usize {
        self.display
            .deletions
            .first()
            .filter(|d| d.at_start)
            .map_or(0, |d| self.deletion(renderer, d, row))
    }

    fn reset_pos(&mut self, pos: usize) -> usize {
        self.next = self
            .display
            .deletions
            .partition_point(|d| d.at_start || d.anchor < pos);
        self.pending = None;
        self.next_anchor()
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        _grapheme: &FormattedGrapheme,
    ) -> usize {
        self.pending = Some(self.next);
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
        let Some(idx) = self.pending.take() else {
            return Position::default();
        };
        Position::new(
            self.deletion(
                renderer,
                &self.display.deletions[idx],
                pos.visual_line + virt_off.row,
            ),
            0,
        )
    }
}
