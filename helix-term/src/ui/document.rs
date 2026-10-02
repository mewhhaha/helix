use std::cmp::min;

use helix_core::doc_formatter::{DocumentFormatter, FormattedGrapheme, GraphemeSource, TextFormat};
use helix_core::graphemes::Grapheme;
use helix_core::str_utils::char_to_byte_idx;
use helix_core::syntax::{self, DisplayHighlighter, HighlightEvent, OverlayHighlights};
use helix_core::text_annotations::TextAnnotations;
use helix_core::{visual_offset_from_block, Position, RopeSlice};
use helix_stdx::rope::RopeSliceExt;
use helix_view::editor::{WhitespaceConfig, WhitespaceRenderValue};
use helix_view::graphics::Rect;
use helix_view::theme::Style;
use helix_view::view::ViewPosition;
use helix_view::{Document, Theme};
use tui::buffer::Buffer as Surface;

use crate::ui::text_decorations::DecorationManager;

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct LinePos {
    /// Indicates whether the given visual line
    /// is the first visual line of the given document line
    pub first_visual_line: bool,
    /// The line index of the document line that contains the given visual line
    pub doc_line: usize,
    /// Vertical offset from the top of the inner view area
    pub visual_line: usize,
}

#[allow(clippy::too_many_arguments)]
pub fn render_document(
    surface: &mut Surface,
    viewport: Rect,
    doc: &Document,
    offset: ViewPosition,
    doc_annotations: &TextAnnotations,
    syntax_highlighter: Option<DisplayHighlighter<'_>>,
    overlay_highlights: Vec<syntax::OverlayHighlights>,
    theme: &Theme,
    decorations: DecorationManager,
) {
    let mut renderer = TextRenderer::new(
        surface,
        doc,
        theme,
        Position::new(offset.vertical_offset, offset.horizontal_offset),
        viewport,
    );
    render_text(
        &mut renderer,
        doc.text().slice(..),
        offset.anchor,
        &doc.text_format(viewport.width, Some(theme)),
        doc_annotations,
        syntax_highlighter,
        overlay_highlights,
        theme,
        decorations,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_text(
    renderer: &mut TextRenderer,
    text: RopeSlice<'_>,
    anchor: usize,
    text_fmt: &TextFormat,
    text_annotations: &TextAnnotations,
    syntax_highlighter: Option<DisplayHighlighter<'_>>,
    overlay_highlights: Vec<syntax::OverlayHighlights>,
    theme: &Theme,
    mut decorations: DecorationManager,
) {
    let row_off = visual_offset_from_block(text, anchor, anchor, text_fmt, text_annotations)
        .0
        .row
        - if anchor == 0 {
            text_annotations.leading_virtual_lines()
        } else {
            0
        };

    let mut formatter = DocumentFormatter::new_at_visual_checkpoint(
        text,
        text_fmt,
        text_annotations,
        anchor,
        Position::new(
            row_off,
            if text_fmt.soft_wrap {
                0
            } else {
                renderer.offset.col
            },
        ),
    );
    let mut syntax_highlighter =
        SyntaxHighlighter::new(syntax_highlighter, text, theme, renderer.text_style);
    let mut overlay_highlighter = OverlayHighlighter::new(overlay_highlights, theme);

    let mut last_line_pos = LinePos {
        first_visual_line: false,
        doc_line: usize::MAX,
        visual_line: usize::MAX,
    };
    let mut last_line_end = 0;
    let resumed_indent = if text_fmt.soft_wrap {
        None
    } else {
        formatter.rendered_indent()
    };
    let mut is_in_indent_area = resumed_indent.is_none();
    let mut last_line_indent_level = resumed_indent.unwrap_or(0);
    let mut reached_view_top = false;
    let mut tail_checked_line = usize::MAX;

    if anchor == 0 {
        decorations.render_leading_lines(renderer);
    }

    loop {
        let Some(mut grapheme) = formatter.next() else {
            break;
        };

        // skip any graphemes on visual lines before the block start
        if grapheme.visual_pos.row < row_off {
            continue;
        }
        grapheme.visual_pos.row -= row_off;
        if !reached_view_top {
            decorations.prepare_for_rendering(grapheme.char_idx);
            reached_view_top = true;
        }

        // if the end of the viewport is reached stop rendering
        if grapheme.visual_pos.row >= renderer.viewport.height as usize + renderer.offset.row {
            break;
        }

        // apply decorations before rendering a new line
        if grapheme.visual_pos.row != last_line_pos.visual_line {
            // we initiate doc_line with usize::MAX because no file
            // can reach that size (memory allocations are limited to isize::MAX)
            // initially there is no "previous" line (so doc_line is set to usize::MAX)
            // in that case we don't need to draw indent guides/virtual text
            if last_line_pos.doc_line != usize::MAX {
                // draw indent guides for the last line
                renderer.draw_indent_guides(last_line_indent_level, last_line_pos.visual_line);
                is_in_indent_area = true;
                decorations.render_virtual_lines(renderer, last_line_pos, last_line_end)
            }
            last_line_pos = LinePos {
                first_visual_line: grapheme.line_idx != last_line_pos.doc_line,
                doc_line: grapheme.line_idx,
                visual_line: grapheme.visual_pos.row,
            };
            renderer.line_style = Style::default();
            decorations.decorate_line(renderer, last_line_pos);
        }

        // acquire the correct grapheme style
        if grapheme.char_idx >= syntax_highlighter.pos {
            syntax_highlighter.seek_to(grapheme.char_idx);
        }
        while grapheme.char_idx >= overlay_highlighter.pos {
            overlay_highlighter.advance();
        }

        let grapheme_style = if let GraphemeSource::VirtualText {
            highlight,
            inherit_background,
        } = grapheme.source
        {
            let mut style = renderer.text_style;
            if let Some(highlight) = highlight {
                style = style.patch(theme.highlight(highlight));
            }
            GraphemeStyle {
                syntax_style: style,
                overlay_style: Style::default(),
                reference_style: inherit_background.then(|| {
                    syntax_highlighter
                        .style
                        .patch(renderer.line_style)
                        .patch(overlay_highlighter.style)
                }),
            }
        } else {
            GraphemeStyle {
                syntax_style: syntax_highlighter.style,
                overlay_style: overlay_highlighter.style,
                reference_style: None,
            }
        };
        decorations.decorate_grapheme(renderer, &grapheme);

        let virt = grapheme.is_virtual();
        let grapheme_width = renderer.draw_grapheme(
            &grapheme,
            grapheme_style,
            virt,
            &mut last_line_indent_level,
            &mut is_in_indent_area,
            grapheme.visual_pos,
        );
        last_line_end = grapheme.visual_pos.col + grapheme_width;
        if !text_fmt.soft_wrap
            && tail_checked_line != grapheme.line_idx
            && grapheme.visual_pos.col >= renderer.offset.col + renderer.viewport.width as usize
            && grapheme.raw != Grapheme::Newline
        {
            tail_checked_line = grapheme.line_idx;
            if let Some(end) = formatter.cached_line_end() {
                let callback_end = end.char_idx + usize::from(end.includes_eof);
                if decorations.can_skip_graphemes_until(callback_end)
                    && formatter.skip_to_line_end()
                {
                    last_line_end = end.width;
                    if let Some(indent) = end.indent_level {
                        last_line_indent_level = indent;
                    }
                }
            }
        }
    }

    if last_line_pos.doc_line != usize::MAX {
        renderer.draw_indent_guides(last_line_indent_level, last_line_pos.visual_line);
        decorations.render_virtual_lines(renderer, last_line_pos, last_line_end)
    }
}

#[derive(Debug)]
pub struct TextRenderer<'a> {
    pub(super) surface: &'a mut Surface,
    pub text_style: Style,
    pub whitespace_style: Style,
    pub indent_guide_char: String,
    pub indent_guide_style: Style,
    pub newline: String,
    pub nbsp: String,
    pub nnbsp: String,
    pub space: String,
    pub tab: String,
    pub virtual_tab: String,
    pub indent_width: u16,
    pub starting_indent: usize,
    pub draw_indent_guides: bool,
    pub viewport: Rect,
    pub offset: Position,
    pub line_style: Style,
}

pub struct GraphemeStyle {
    syntax_style: Style,
    overlay_style: Style,
    reference_style: Option<Style>,
}

impl<'a> TextRenderer<'a> {
    pub fn new(
        surface: &'a mut Surface,
        doc: &Document,
        theme: &Theme,
        offset: Position,
        viewport: Rect,
    ) -> TextRenderer<'a> {
        let editor_config = doc.config.load();
        let WhitespaceConfig {
            render: ws_render,
            characters: ws_chars,
        } = &editor_config.whitespace;

        let tab_width = doc.tab_width();
        let tab = if ws_render.tab() == WhitespaceRenderValue::All {
            std::iter::once(ws_chars.tab)
                .chain(std::iter::repeat_n(ws_chars.tabpad, tab_width - 1))
                .collect()
        } else {
            " ".repeat(tab_width)
        };
        let virtual_tab = " ".repeat(tab_width);
        let newline = if ws_render.newline() == WhitespaceRenderValue::All {
            ws_chars.newline.into()
        } else {
            " ".to_owned()
        };

        let space = if ws_render.space() == WhitespaceRenderValue::All {
            ws_chars.space.into()
        } else {
            " ".to_owned()
        };
        let nbsp = if ws_render.nbsp() == WhitespaceRenderValue::All {
            ws_chars.nbsp.into()
        } else {
            " ".to_owned()
        };
        let nnbsp = if ws_render.nnbsp() == WhitespaceRenderValue::All {
            ws_chars.nnbsp.into()
        } else {
            " ".to_owned()
        };

        let text_style = theme.get("ui.text");

        let indent_width = doc.indent_style.indent_width(tab_width) as u16;

        TextRenderer {
            surface,
            indent_guide_char: editor_config.indent_guides.character.into(),
            newline,
            nbsp,
            nnbsp,
            space,
            tab,
            virtual_tab,
            whitespace_style: theme.get("ui.virtual.whitespace"),
            indent_width,
            starting_indent: offset.col / indent_width as usize
                + !offset.col.is_multiple_of(indent_width as usize) as usize
                + editor_config.indent_guides.skip_levels as usize,
            indent_guide_style: text_style.patch(
                theme
                    .try_get("ui.virtual.indent-guide")
                    .unwrap_or_else(|| theme.get("ui.virtual.whitespace")),
            ),
            text_style,
            draw_indent_guides: editor_config.indent_guides.render,
            viewport,
            offset,
            line_style: Style::default(),
        }
    }
    /// Draws a single `grapheme` at the current render position with a specified `style`.
    pub fn draw_decoration_grapheme(
        &mut self,
        grapheme: Grapheme,
        mut style: Style,
        row: usize,
        col: u16,
    ) -> bool {
        let Some(row) = self.viewport_row(row) else {
            return false;
        };
        if col >= self.viewport.width {
            return false;
        }
        // TODO is it correct to apply the whitspace style to all unicode white spaces?
        if grapheme.is_whitespace() {
            style = style.patch(self.whitespace_style);
        }

        let grapheme = match grapheme {
            Grapheme::Tab { width } => {
                let grapheme_tab_width = char_to_byte_idx(&self.virtual_tab, width);
                &self.virtual_tab[..grapheme_tab_width]
            }
            Grapheme::Other { ref g } if g == "\u{00A0}" => " ",
            Grapheme::Other { ref g } => g,
            Grapheme::Newline => " ",
        };

        self.surface
            .set_string(self.viewport.x + col, row, grapheme, style);
        true
    }

    /// Draws a single `grapheme` at the current render position with a specified `style`.
    pub fn draw_grapheme(
        &mut self,
        grapheme: &FormattedGrapheme,
        grapheme_style: GraphemeStyle,
        is_virtual: bool,
        last_indent_level: &mut usize,
        is_in_indent_area: &mut bool,
        mut position: Position,
    ) -> usize {
        if position.row < self.offset.row {
            return 0;
        }
        position.row -= self.offset.row;
        let cut_off_start = self.offset.col.saturating_sub(position.col);
        let is_whitespace = grapheme.is_whitespace();

        // TODO is it correct to apply the whitespace style to all unicode white spaces?
        let mut style = grapheme_style.syntax_style;
        if is_whitespace {
            style = style.patch(self.whitespace_style);
        }
        style = style
            .patch(self.line_style)
            .patch(grapheme_style.overlay_style);

        if let Some(reference_style) = grapheme_style.reference_style {
            // Line decorations (e.g. cursorline) may already have styled the
            // surface. Resolve that background with syntax, diff and selection
            // styles, while keeping the swatch's own foreground color.
            let x = position.col.saturating_sub(self.offset.col);
            if x < self.viewport.width as usize {
                if let Some(cell) = self.surface.get(
                    self.viewport.x + x as u16,
                    self.viewport.y + position.row as u16,
                ) {
                    style = cell.style().patch(reference_style).color_swatch(
                        grapheme_style
                            .syntax_style
                            .fg
                            .unwrap_or(helix_view::graphics::Color::Reset),
                    );
                }
            }
        }

        let width = grapheme.width();
        let mut is_tab = false;
        let space = if is_virtual { " " } else { &self.space };
        let nbsp = if is_virtual { " " } else { &self.nbsp };
        let nnbsp = if is_virtual { " " } else { &self.nnbsp };
        let tab = if is_virtual {
            &self.virtual_tab
        } else {
            &self.tab
        };
        let grapheme = match grapheme.raw {
            Grapheme::Tab { width } => {
                is_tab = true;
                let grapheme_tab_width = char_to_byte_idx(tab, width);
                &tab[..grapheme_tab_width]
            }
            // TODO special rendering for other whitespaces?
            Grapheme::Other { ref g } if g == " " && !grapheme.source.is_eof() => space,
            Grapheme::Other { ref g } if g == "\u{00A0}" => nbsp,
            Grapheme::Other { ref g } if g == "\u{202F}" => nnbsp,
            Grapheme::Other { ref g } => g,
            Grapheme::Newline => &self.newline,
        };

        let in_bounds = self.column_in_bounds(position.col, width);

        if in_bounds {
            let x = self.viewport.x + (position.col - self.offset.col) as u16;
            let y = self.viewport.y + position.row as u16;
            if is_tab {
                // A tab expands to `width` single-column cells; writing them
                // individually keeps background styles (selection, cursorline)
                // across the whole tab and avoids the redraw diff clipping
                // `render-whitespace` pads. A single `set_grapheme` would pack
                // them into one wide cell and leave the rest unstyled.
                self.surface.set_tab(x, y, grapheme, style);
            } else {
                self.surface.set_grapheme(x, y, grapheme, width, style);
            }
        } else if cut_off_start != 0 && cut_off_start < width {
            // partially on screen
            let rect = Rect::new(
                self.viewport.x,
                self.viewport.y + position.row as u16,
                (width - cut_off_start) as u16,
                1,
            );
            self.surface.set_style(rect, style);
        }
        if *is_in_indent_area && !is_whitespace {
            *last_indent_level = position.col;
            *is_in_indent_area = false;
        }

        width
    }

    pub fn column_in_bounds(&self, colum: usize, width: usize) -> bool {
        self.offset.col <= colum && colum + width <= self.offset.col + self.viewport.width as usize
    }

    /// Overlay indentation guides ontop of a rendered line
    /// The indentation level is computed in `draw_lines`.
    /// Therefore this function must always be called afterwards.
    pub fn draw_indent_guides(&mut self, indent_level: usize, row: usize) {
        if !self.draw_indent_guides {
            return;
        }
        let Some(y) = self.viewport_row(row) else {
            return;
        };

        // Don't draw indent guides outside of view
        let end_indent = min(
            indent_level,
            // Add indent_width - 1 to round up, since the first visible
            // indent might be a bit after offset.col
            self.offset.col + self.viewport.width as usize + (self.indent_width as usize - 1),
        ) / self.indent_width as usize;

        for i in self.starting_indent..end_indent {
            let x = (self.viewport.x as usize + (i * self.indent_width as usize) - self.offset.col)
                as u16;
            debug_assert!(self.surface.in_bounds(x, y));
            self.surface.set_string(
                x,
                y,
                &self.indent_guide_char,
                self.indent_guide_style.patch(self.line_style),
            );
        }
    }

    fn viewport_row(&self, row: usize) -> Option<u16> {
        let row = row.checked_sub(self.offset.row)?;
        (row < self.viewport.height as usize).then(|| self.viewport.y + row as u16)
    }

    pub fn set_string(&mut self, x: u16, row: usize, string: &str, style: Style) {
        if let Some(y) = self.viewport_row(row) {
            self.surface.set_string(x, y, string, style);
        }
    }

    pub fn set_stringn(&mut self, x: u16, row: usize, string: &str, width: usize, style: Style) {
        if let Some(y) = self.viewport_row(row) {
            self.surface.set_stringn(x, y, string, width, style);
        }
    }

    pub fn set_row_style(&mut self, x: u16, row: usize, width: u16, style: Style) {
        if let Some(y) = self.viewport_row(row) {
            self.surface.set_style(Rect::new(x, y, width, 1), style);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_string_truncated(
        &mut self,
        x: u16,
        row: usize,
        string: &str,
        width: usize,
        style: impl Fn(usize) -> Style, // Map a grapheme's string offset to a style
        ellipsis: bool,
        truncate_start: bool,
    ) -> (u16, u16) {
        let Some(y) = self.viewport_row(row) else {
            return (x, self.viewport.y);
        };
        self.surface
            .set_string_truncated(x, y, string, width, style, ellipsis, truncate_start)
    }
}

pub(super) struct SyntaxHighlighter<'h, 'r, 't> {
    inner: Option<DisplayHighlighter<'h>>,
    text: RopeSlice<'r>,
    /// The character index of the next highlight event, or `usize::MAX` if the highlighter is
    /// finished.
    pos: usize,
    theme: &'t Theme,
    text_style: Style,
    style: Style,
}

impl<'h, 'r, 't> SyntaxHighlighter<'h, 'r, 't> {
    pub(super) fn new(
        inner: Option<DisplayHighlighter<'h>>,
        text: RopeSlice<'r>,
        theme: &'t Theme,
        text_style: Style,
    ) -> Self {
        let mut highlighter = Self {
            inner,
            text,
            pos: 0,
            theme,
            style: text_style,
            text_style,
        };
        highlighter.update_pos();
        highlighter
    }

    fn update_pos(&mut self) {
        self.pos = self
            .inner
            .as_ref()
            .and_then(|highlighter| {
                let next_byte_idx = highlighter.next_event_offset();
                (next_byte_idx != u32::MAX).then(|| {
                    // Move the byte index to the nearest character boundary (rounding up) and
                    // convert it to a character index.
                    self.text
                        .byte_to_char(self.text.ceil_char_boundary(next_byte_idx as usize))
                })
            })
            .unwrap_or(usize::MAX);
    }

    fn seek_to(&mut self, char_idx: usize) {
        let Some(highlighter) = self.inner.as_mut() else {
            return;
        };

        let highlights = highlighter.seek_to(self.text.char_to_byte(char_idx) as u32);
        self.style = highlights
            .iter()
            .copied()
            .fold(self.text_style, |acc, highlight| {
                acc.patch(self.theme.highlight(highlight))
            });
        self.update_pos();
    }

    pub(super) fn style_at(&mut self, char_idx: usize) -> Style {
        if char_idx >= self.pos {
            self.seek_to(char_idx);
        }
        self.style
    }
}

struct OverlayHighlighter<'t> {
    inner: syntax::OverlayHighlighter,
    pos: usize,
    theme: &'t Theme,
    style: Style,
}

impl<'t> OverlayHighlighter<'t> {
    fn new(overlays: Vec<OverlayHighlights>, theme: &'t Theme) -> Self {
        let inner = syntax::OverlayHighlighter::new(overlays);
        let mut highlighter = Self {
            inner,
            pos: 0,
            theme,
            style: Style::default(),
        };
        highlighter.update_pos();
        highlighter
    }

    fn update_pos(&mut self) {
        self.pos = self.inner.next_event_offset();
    }

    fn advance(&mut self) {
        let (event, highlights) = self.inner.advance();
        let base = match event {
            HighlightEvent::Refresh => Style::default(),
            HighlightEvent::Push => self.style,
        };

        self.style = highlights.fold(base, |acc, highlight| {
            acc.patch(self.theme.highlight(highlight))
        });
        self.update_pos();
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use arc_swap::ArcSwap;
    use helix_core::doc_formatter::FormatterCache;
    use helix_core::text_annotations::InlineAnnotation;
    use helix_core::Rope;
    use helix_view::editor::Config;
    use std::{cell::Cell, rc::Rc, sync::Arc};

    fn document(text: Rope) -> Document {
        let mut config = Config::default();
        config.indent_guides.render = true;
        Document::from(
            text,
            None,
            Arc::new(ArcSwap::from_pointee(config)),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        )
    }

    fn render(
        doc: &Document,
        format: &TextFormat,
        annotations: &TextAnnotations,
        anchor: usize,
        offset: Position,
        decorations: DecorationManager,
    ) -> Surface {
        let viewport = Rect::new(0, 0, 80, 4);
        let mut surface = Surface::empty(viewport);
        let theme = Theme::default();
        let mut renderer = TextRenderer::new(&mut surface, doc, &theme, offset, viewport);
        render_text(
            &mut renderer,
            doc.text().slice(..),
            anchor,
            format,
            annotations,
            None,
            Vec::new(),
            &theme,
            decorations,
        );
        surface
    }

    #[test]
    fn color_swatches_and_padding_follow_diff_selection_and_cursorline_backgrounds() {
        use helix_view::graphics::{Color, Modifier};
        let doc = document(Rope::from_str("color: #f00;\n"));
        let swatches = [InlineAnnotation::new(7, "■").with_inherited_background()];
        let padding = [InlineAnnotation::new(7, " ").with_inherited_background()];
        let colors = [Theme::rgb_highlight(255, 0, 0)];
        let mut annotations = TextAnnotations::default();
        annotations
            .add_inline_annotations_with_highlights(&swatches, &colors)
            .add_inline_annotations(&padding, None);
        let theme: Theme = toml::from_str(
            "\"ui.text\" = { fg = \"#eeeeee\", bg = \"#121212\" }\n\
             \"ui.selection\" = { fg = \"#ffffff\", bg = \"#223344\" }\n\
             \"ui.selection.primary\" = { fg = \"#563412\", bg = \"#010203\", modifiers = [\"reversed\", \"dim\"] }",
        ).unwrap();
        for (diff, selection, explicit_text_bg) in [
            (false, None, false),
            (false, None, true),
            (true, None, true),
            (true, Some("ui.selection"), true),
            (true, Some("ui.selection.primary"), true),
        ] {
            let viewport = Rect::new(0, 0, 40, 2);
            let mut surface = Surface::empty(viewport);
            let mut renderer =
                TextRenderer::new(&mut surface, &doc, &theme, Position::default(), viewport);
            if !explicit_text_bg {
                renderer.text_style.bg = None;
            }
            let mut decorations = DecorationManager::default();
            decorations.add_decoration(|renderer: &mut TextRenderer, pos: LinePos| {
                renderer.set_row_style(
                    0,
                    pos.visual_line,
                    40,
                    Style::default().bg(Color::Rgb(40, 40, 40)),
                );
            });
            if diff {
                decorations.add_decoration(|renderer: &mut TextRenderer, pos: LinePos| {
                    renderer.line_style = Style::default().bg(Color::Rgb(28, 57, 37));
                    renderer.set_row_style(0, pos.visual_line, 40, renderer.line_style);
                });
            }
            let overlays = selection
                .into_iter()
                .map(|scope| {
                    OverlayHighlights::single(theme.find_highlight_exact(scope).unwrap(), 7..11)
                })
                .collect();
            render_text(
                &mut renderer,
                doc.text().slice(..),
                0,
                &TextFormat::default(),
                &annotations,
                None,
                overlays,
                &theme,
                decorations,
            );
            let value = surface.get(9, 0).unwrap();
            assert_eq!(value.symbol.as_str(), "#");
            let background = if value.modifier.contains(Modifier::REVERSED) {
                value.fg
            } else {
                value.bg
            };
            for x in [7, 8] {
                let swatch = surface.get(x, 0).unwrap();
                assert_eq!(
                    swatch.bg, background,
                    "diff={diff}, selection={selection:?}, x={x}"
                );
                assert!(!swatch
                    .modifier
                    .intersects(Modifier::DIM | Modifier::REVERSED));
            }
            assert_eq!(surface.get(7, 0).unwrap().fg, Color::Rgb(255, 0, 0));
        }
    }

    #[test]
    fn warm_checkpoints_preserve_rendered_tabs_indent_guides_and_inline_rows() {
        let doc = document(Rope::from_str(&format!(
            "{}{}\n{}\nend",
            "\t ".repeat(700),
            "界e\u{301}\tword ".repeat(800),
            "next ".repeat(700)
        )));
        let inline = [InlineAnnotation::new(3000, "\nvisible virtual text\n")];
        let mut annotations = TextAnnotations::default();
        annotations.add_inline_annotations(&inline, None);
        for wrapped in [false, true] {
            let plain = TextFormat {
                soft_wrap: wrapped,
                viewport_width: 80,
                ..TextFormat::default()
            };
            let mut cached = plain.clone();
            cached.checkpoint_cache = Some(Arc::new(FormatterCache::default()));
            DocumentFormatter::new_at_prev_checkpoint(
                doc.text().slice(..),
                &cached,
                &annotations,
                0,
            )
            .for_each(drop);
            for (anchor, offset) in [
                (0, Position::new(0, 0)),
                (0, Position::new(0, if wrapped { 0 } else { 4190 })),
                (5000, Position::new(1, 0)),
            ] {
                assert_eq!(
                    render(
                        &doc,
                        &cached,
                        &annotations,
                        anchor,
                        offset,
                        DecorationManager::default()
                    ),
                    render(
                        &doc,
                        &plain,
                        &annotations,
                        anchor,
                        offset,
                        DecorationManager::default()
                    ),
                    "wrapped={wrapped}, anchor={anchor}, offset={offset:?}"
                );
            }
        }
    }

    #[test]
    fn cached_tail_keeps_pending_decoration_at_synthetic_eof() {
        struct AtEof {
            anchor: usize,
            calls: Rc<Cell<usize>>,
        }
        impl crate::ui::text_decorations::Decoration for AtEof {
            fn reset_pos(&mut self, pos: usize) -> usize {
                if pos <= self.anchor {
                    self.anchor
                } else {
                    usize::MAX
                }
            }
            fn decorate_grapheme(
                &mut self,
                _renderer: &mut TextRenderer,
                _grapheme: &FormattedGrapheme,
            ) -> usize {
                self.calls.set(self.calls.get() + 1);
                usize::MAX
            }
        }
        let doc = document(Rope::from_str(&"x".repeat(5000)));
        let annotations = TextAnnotations::default();
        let format = TextFormat {
            checkpoint_cache: Some(Arc::new(FormatterCache::default())),
            ..TextFormat::default()
        };
        DocumentFormatter::new_at_prev_checkpoint(doc.text().slice(..), &format, &annotations, 0)
            .for_each(drop);
        let calls = Rc::new(Cell::new(0));
        let mut decorations = DecorationManager::default();
        decorations.add_decoration(AtEof {
            anchor: doc.text().len_chars(),
            calls: calls.clone(),
        });
        render(
            &doc,
            &format,
            &annotations,
            0,
            Position::default(),
            decorations,
        );
        assert_eq!(calls.get(), 1);
    }
}
