use std::cmp::Ordering;

use helix_core::diagnostic::Severity;
use helix_core::doc_formatter::{DocumentFormatter, FormattedGrapheme};
use helix_core::graphemes::Grapheme;
use helix_core::text_annotations::TextAnnotations;
use helix_core::{Diagnostic, Position};
use helix_view::annotations::diagnostics::{
    DiagnosticFilter, InlineDiagnosticAccumulator, InlineDiagnosticsConfig,
};

use helix_view::theme::Style;
use helix_view::{Document, Theme};

use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::text_decorations::Decoration;

#[derive(Debug)]
struct Styles {
    hint: Style,
    info: Style,
    warning: Style,
    error: Style,
}

impl Styles {
    fn new(theme: &Theme) -> Styles {
        Styles {
            hint: theme.get("hint.diagnostic.inline"),
            info: theme.get("info.diagnostic.inline"),
            warning: theme.get("warning.diagnostic.inline"),
            error: theme.get("error.diagnostic.inline"),
        }
    }

    fn severity_style(&self, severity: Severity) -> Style {
        match severity {
            Severity::Hint => self.hint,
            Severity::Info => self.info,
            Severity::Warning => self.warning,
            Severity::Error => self.error,
        }
    }
}

pub struct InlineDiagnostics<'a> {
    state: InlineDiagnosticAccumulator<'a>,
    doc: &'a Document,
    eol_diagnostics: DiagnosticFilter,
    styles: Styles,
}

impl<'a> InlineDiagnostics<'a> {
    pub fn new(
        doc: &'a Document,
        theme: &Theme,
        cursor: usize,
        config: InlineDiagnosticsConfig,
        eol_diagnostics: DiagnosticFilter,
    ) -> Self {
        InlineDiagnostics {
            state: InlineDiagnosticAccumulator::new(cursor, doc, config),
            doc,
            styles: Styles::new(theme),
            eol_diagnostics,
        }
    }
}

const BL_CORNER: &str = "┘";
const TR_CORNER: &str = "┌";
const BR_CORNER: &str = "└";
const STACK: &str = "├";
const MULTI: &str = "┴";
const HOR_BAR: &str = "─";
const VER_BAR: &str = "│";

struct Renderer<'a, 'b> {
    renderer: &'a mut TextRenderer<'b>,
    first_row: usize,
    row: usize,
    doc: &'a Document,
    config: &'a InlineDiagnosticsConfig,
    styles: &'a Styles,
}

impl Renderer<'_, '_> {
    fn draw_decoration(&mut self, g: &'static str, severity: Severity, col: u16) {
        self.draw_decoration_at(g, severity, col, self.row)
    }

    fn visible_rows(&self, rows: std::ops::Range<usize>) -> std::ops::Range<usize> {
        rows.start.max(self.renderer.offset.row)
            ..rows.end.min(
                self.renderer
                    .offset
                    .row
                    .saturating_add(self.renderer.viewport.height as usize),
            )
    }

    fn draw_decoration_at(&mut self, g: &'static str, severity: Severity, col: u16, row: usize) {
        if !self.visible_rows(row..row.saturating_add(1)).contains(&row) {
            return;
        }
        self.renderer.draw_decoration_grapheme(
            Grapheme::new_decoration(g),
            self.styles.severity_style(severity),
            row,
            col,
        );
    }

    fn draw_eol_diagnostic(&mut self, diag: &Diagnostic, row: usize, col: usize) -> u16 {
        let style = self.styles.severity_style(diag.severity());
        let width = self.renderer.viewport.width;
        let start_col = (col - self.renderer.offset.col) as u16;
        let mut end_col = start_col;
        let mut draw_col = (col + 1) as u16;

        for line in diag.message.lines() {
            if !self.renderer.column_in_bounds(draw_col as usize, 1) {
                break;
            }

            (end_col, _) = self.renderer.set_string_truncated(
                self.renderer.viewport.x + draw_col,
                row,
                line,
                width.saturating_sub(draw_col) as usize,
                |_| style,
                true,
                false,
            );

            draw_col = end_col - self.renderer.viewport.x + 2; // double space between lines
        }

        end_col - start_col
    }

    fn draw_diagnostic(&mut self, diag: &Diagnostic, col: u16, next_severity: Option<Severity>) {
        let severity = diag.severity();
        let (sym, sym_severity) = if let Some(next_severity) = next_severity {
            (STACK, next_severity.max(severity))
        } else {
            (BR_CORNER, severity)
        };
        self.draw_decoration(sym, sym_severity, col);
        for i in 0..self.config.prefix_len {
            self.draw_decoration(HOR_BAR, severity, col + i + 1);
        }

        let text_col = col + self.config.prefix_len + 1;
        let text_fmt = self.config.text_fmt(text_col, self.renderer.viewport.width);
        let height = self
            .doc
            .diagnostic_message_dimensions(diag.message.as_str().trim(), &text_fmt)
            .0;
        let end = self.row.saturating_add(height);
        let visible = self.visible_rows(self.row..end);
        if !visible.is_empty() {
            let annotations = TextAnnotations::default();
            let formatter = DocumentFormatter::new_at_prev_checkpoint(
                diag.message.as_str().trim().into(),
                &text_fmt,
                &annotations,
                0,
            );
            let style = self.styles.severity_style(severity);
            for grapheme in formatter {
                let row = self.row.saturating_add(grapheme.visual_pos.row);
                if row >= visible.end {
                    break;
                }
                if row < visible.start {
                    continue;
                }
                let Ok(col) = u16::try_from(text_col as usize + grapheme.visual_pos.col) else {
                    continue;
                };
                self.renderer
                    .draw_decoration_grapheme(grapheme.raw, style, row, col);
            }
        }
        if let Some(next_severity) = next_severity {
            for row in self.visible_rows(self.row.saturating_add(1)..end) {
                self.draw_decoration_at(VER_BAR, next_severity, col, row);
            }
        }
        // Layout must still reserve the entire message, including invisible rows.
        self.row = end;
    }

    fn draw_multi_diagnostics(&mut self, stack: &mut Vec<(&Diagnostic, u16)>) {
        let Some(&(last_diag, last_anchor)) = stack.last() else {
            return;
        };
        let start = self
            .config
            .max_diagnostic_start(self.renderer.viewport.width);

        if last_anchor <= start {
            return;
        }
        let mut severity = last_diag.severity();
        let mut last_anchor = last_anchor;
        self.draw_decoration(BL_CORNER, severity, last_anchor);
        let mut stacked_diagnostics = 1;
        for &(diag, anchor) in stack.iter().rev().skip(1) {
            let sym = match anchor.cmp(&start) {
                Ordering::Less => break,
                Ordering::Equal => STACK,
                Ordering::Greater => MULTI,
            };
            stacked_diagnostics += 1;
            severity = severity.max(diag.severity());
            let old_severity = severity;
            if anchor == last_anchor && severity == old_severity {
                continue;
            }
            for col in (anchor + 1)..last_anchor {
                self.draw_decoration(HOR_BAR, old_severity, col)
            }
            self.draw_decoration(sym, severity, anchor);
            last_anchor = anchor;
        }

        // if no diagnostic anchor was found exactly at the start of the
        // diagnostic text  draw an upwards corner and ensure the last piece
        // of the line is not missing
        if last_anchor != start {
            for col in (start + 1)..last_anchor {
                self.draw_decoration(HOR_BAR, severity, col)
            }
            self.draw_decoration(TR_CORNER, severity, start)
        }
        self.row += 1;
        let stacked_diagnostics = &stack[stack.len() - stacked_diagnostics..];

        for (i, (diag, _)) in stacked_diagnostics.iter().rev().enumerate() {
            let next_severity = stacked_diagnostics[..stacked_diagnostics.len() - i - 1]
                .iter()
                .map(|(diag, _)| diag.severity())
                .max();
            self.draw_diagnostic(diag, start, next_severity);
        }

        stack.truncate(stack.len() - stacked_diagnostics.len());
    }

    fn draw_diagnostics(&mut self, stack: &mut Vec<(&Diagnostic, u16)>) {
        let mut stack = stack.drain(..).rev().peekable();
        let mut last_anchor = self.renderer.viewport.width;
        while let Some((diag, anchor)) = stack.next() {
            if anchor != last_anchor {
                for row in self.visible_rows(self.first_row..self.row) {
                    self.draw_decoration_at(VER_BAR, diag.severity(), anchor, row);
                }
            }
            let next_severity = stack.peek().and_then(|&(diag, next_anchor)| {
                (next_anchor == anchor).then_some(diag.severity())
            });
            self.draw_diagnostic(diag, anchor, next_severity);
            last_anchor = anchor;
        }
    }
}

impl Decoration for InlineDiagnostics<'_> {
    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let mut col_off = 0;
        let filter = self.state.filter();
        let eol_diagnostic = match self.eol_diagnostics {
            DiagnosticFilter::Enable(eol_filter) => {
                let eol_diganogistcs = self
                    .state
                    .stack
                    .iter()
                    .filter(|(diag, _)| eol_filter <= diag.severity());
                match filter {
                    DiagnosticFilter::Enable(filter) => eol_diganogistcs
                        .filter(|(diag, _)| filter > diag.severity())
                        .max_by_key(|(diagnostic, _)| diagnostic.severity),
                    DiagnosticFilter::Disable => {
                        eol_diganogistcs.max_by_key(|(diagnostic, _)| diagnostic.severity)
                    }
                }
            }
            DiagnosticFilter::Disable => None,
        };
        if let Some((eol_diagnostic, _)) = eol_diagnostic {
            let mut renderer = Renderer {
                renderer,
                first_row: pos.visual_line,
                row: pos.visual_line,
                doc: self.doc,
                config: &self.state.config,
                styles: &self.styles,
            };
            col_off = renderer.draw_eol_diagnostic(eol_diagnostic, pos.visual_line, virt_off.col);
        }

        self.state.compute_line_diagnostics();
        let mut renderer = Renderer {
            renderer,
            first_row: pos.visual_line + virt_off.row,
            row: pos.visual_line + virt_off.row,
            doc: self.doc,
            config: &self.state.config,
            styles: &self.styles,
        };
        renderer.draw_multi_diagnostics(&mut self.state.stack);
        renderer.draw_diagnostics(&mut self.state.stack);
        let horizontal_off = renderer.row - renderer.first_row;
        Position::new(horizontal_off, col_off as usize)
    }

    fn reset_pos(&mut self, pos: usize) -> usize {
        self.state.reset_pos(pos)
    }

    fn skip_concealed_anchor(&mut self, conceal_end_char_idx: usize) -> usize {
        self.state.skip_concealed(conceal_end_char_idx)
    }

    fn decorate_grapheme(
        &mut self,
        renderer: &mut TextRenderer,
        grapheme: &FormattedGrapheme,
    ) -> usize {
        self.state
            .proccess_anchor(grapheme, renderer.viewport.width, renderer.offset.col)
    }
}

#[cfg(test)]
mod rendering_tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use helix_core::{diagnostic::DiagnosticProvider, syntax, Rope};
    use helix_view::{editor::Config, graphics::Rect};
    use tui::buffer::Buffer;

    use super::*;

    fn document() -> Document {
        Document::from(
            Rope::from_str("x\n"),
            None,
            Arc::new(ArcSwap::from_pointee(Config::default())),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        )
    }

    fn diagnostic(message: String) -> Diagnostic {
        Diagnostic {
            range: helix_core::diagnostic::Range { start: 0, end: 1 },
            starts_at_word: false,
            ends_at_word: false,
            zero_width: false,
            line: 0,
            message,
            severity: Some(Severity::Warning),
            code: None,
            provider: DiagnosticProvider::Lsp {
                server_id: helix_core::diagnostic::LanguageServerId::default(),
                identifier: None,
            },
            tags: Vec::new(),
            source: None,
            data: None,
        }
    }

    #[test]
    fn long_diagnostics_reserve_full_height_and_draw_only_visible_rows() {
        let doc = document();
        let theme = Theme::default();
        let config = InlineDiagnosticsConfig::default();
        let styles = Styles::new(&theme);
        let area = Rect::new(0, 0, 80, 3);
        let mut surface = Buffer::empty(area);
        {
            let mut text_renderer =
                TextRenderer::new(&mut surface, &doc, &theme, Position::new(0, 0), area);
            let mut renderer = Renderer {
                renderer: &mut text_renderer,
                first_row: 1,
                row: 1,
                doc: &doc,
                config: &config,
                styles: &styles,
            };
            let diagnostic = diagnostic("x\n".repeat(70_000));
            renderer.draw_diagnostic(&diagnostic, 0, Some(Severity::Error));
            assert_eq!(renderer.row, 70_001);
            assert_eq!(
                renderer.visible_rows(renderer.first_row..renderer.row),
                1..3
            );
            let previous = renderer.row;
            renderer.draw_diagnostic(&diagnostic, 0, None);
            assert_eq!(renderer.row, previous + 70_000);
        }
        assert_eq!(surface[(2, 1)].symbol.as_str(), "x");
        assert_eq!(surface[(2, 2)].symbol.as_str(), "x");
        assert_eq!(surface[(0, 2)].symbol.as_str(), VER_BAR);
    }

    #[test]
    fn vertically_offset_diagnostic_rows_use_the_entire_viewport() {
        let doc = document();
        let theme = Theme::default();
        let config = InlineDiagnosticsConfig::default();
        let styles = Styles::new(&theme);
        let area = Rect::new(0, 0, 80, 3);
        let mut surface = Buffer::empty(area);
        {
            let mut text_renderer =
                TextRenderer::new(&mut surface, &doc, &theme, Position::new(2, 0), area);
            let mut renderer = Renderer {
                renderer: &mut text_renderer,
                first_row: 1,
                row: 1,
                doc: &doc,
                config: &config,
                styles: &styles,
            };
            renderer.draw_diagnostic(&diagnostic("one\ntwo\nthree\nfour".into()), 0, None);
            assert_eq!(renderer.row, 5);
        }
        assert_eq!(surface[(2, 0)].symbol.as_str(), "t");
        assert_eq!(surface[(2, 1)].symbol.as_str(), "t");
        assert_eq!(surface[(2, 2)].symbol.as_str(), "f");
    }
}
