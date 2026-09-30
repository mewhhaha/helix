use helix_core::diagnostic::Severity;
use helix_core::doc_formatter::{FormattedGrapheme, TextFormat};
use helix_core::text_annotations::LineAnnotation;
use helix_core::{Diagnostic, Position};
use serde::{Deserialize, Serialize};

use crate::Document;
use std::{
    any::Any,
    hash::{Hash, Hasher},
    sync::Arc,
};

/// Describes the severity level of a [`Diagnostic`].
#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord)]
pub enum DiagnosticFilter {
    Disable,
    Enable(Severity),
}

impl Hash for DiagnosticFilter {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        match self {
            Self::Disable => 0u8.hash(hasher),
            Self::Enable(severity) => {
                1u8.hash(hasher);
                (*severity as u8).hash(hasher);
            }
        }
    }
}

impl<'de> Deserialize<'de> for DiagnosticFilter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match &*String::deserialize(deserializer)? {
            "disable" => Ok(DiagnosticFilter::Disable),
            "hint" => Ok(DiagnosticFilter::Enable(Severity::Hint)),
            "info" => Ok(DiagnosticFilter::Enable(Severity::Info)),
            "warning" => Ok(DiagnosticFilter::Enable(Severity::Warning)),
            "error" => Ok(DiagnosticFilter::Enable(Severity::Error)),
            variant => Err(serde::de::Error::unknown_variant(
                variant,
                &["disable", "hint", "info", "warning", "error"],
            )),
        }
    }
}

impl Serialize for DiagnosticFilter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let filter = match self {
            DiagnosticFilter::Disable => "disable",
            DiagnosticFilter::Enable(Severity::Hint) => "hint",
            DiagnosticFilter::Enable(Severity::Info) => "info",
            DiagnosticFilter::Enable(Severity::Warning) => "warning",
            DiagnosticFilter::Enable(Severity::Error) => "error",
        };
        filter.serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct InlineDiagnosticsConfig {
    pub cursor_line: DiagnosticFilter,
    pub other_lines: DiagnosticFilter,
    pub min_diagnostic_width: u16,
    pub prefix_len: u16,
    pub max_wrap: u16,
    pub max_diagnostics: usize,
}

impl InlineDiagnosticsConfig {
    pub fn disabled(&self) -> bool {
        matches!(
            self,
            Self {
                cursor_line: DiagnosticFilter::Disable,
                other_lines: DiagnosticFilter::Disable,
                ..
            }
        )
    }

    pub fn prepare(&self, width: u16, enable_cursor_line: bool) -> Self {
        let mut config = self.clone();
        if width < self.min_diagnostic_width + self.prefix_len {
            config.cursor_line = DiagnosticFilter::Disable;
            config.other_lines = DiagnosticFilter::Disable;
        } else if !enable_cursor_line {
            config.cursor_line = self.cursor_line.min(self.other_lines);
        }
        config
    }

    pub fn max_diagnostic_start(&self, width: u16) -> u16 {
        width - self.min_diagnostic_width - self.prefix_len
    }

    pub fn text_fmt(&self, anchor_col: u16, width: u16) -> TextFormat {
        let width = if anchor_col > self.max_diagnostic_start(width) {
            self.min_diagnostic_width
        } else {
            width - anchor_col - self.prefix_len
        };

        TextFormat {
            soft_wrap: true,
            tab_width: 4,
            max_wrap: self.max_wrap.min(width / 4),
            max_indent_retain: 0,
            wrap_indicator: "".into(),
            wrap_indicator_highlight: None,
            viewport_width: width,
            soft_wrap_at_text_width: true,
            checkpoint_cache: None,
        }
    }
}

impl Default for InlineDiagnosticsConfig {
    fn default() -> Self {
        InlineDiagnosticsConfig {
            cursor_line: DiagnosticFilter::Enable(Severity::Warning),
            other_lines: DiagnosticFilter::Disable,
            min_diagnostic_width: 40,
            prefix_len: 1,
            max_wrap: 20,
            max_diagnostics: 10,
        }
    }
}

pub struct InlineDiagnosticAccumulator<'a> {
    idx: usize,
    doc: &'a Document,
    pub stack: Vec<(&'a Diagnostic, u16)>,
    pub config: InlineDiagnosticsConfig,
    cursor: usize,
    cursor_line: bool,
}

impl<'a> InlineDiagnosticAccumulator<'a> {
    pub fn new(cursor: usize, doc: &'a Document, config: InlineDiagnosticsConfig) -> Self {
        InlineDiagnosticAccumulator {
            idx: 0,
            doc,
            stack: Vec::new(),
            config,
            cursor,
            cursor_line: false,
        }
    }

    pub fn reset_pos(&mut self, char_idx: usize) -> usize {
        self.idx = 0;
        self.clear();
        self.skip_concealed(char_idx)
    }

    pub fn skip_concealed(&mut self, conceal_end_char_idx: usize) -> usize {
        let diagnostics = &self.doc.diagnostics[self.idx..];
        let idx = diagnostics.partition_point(|diag| diag.range.start < conceal_end_char_idx);
        self.idx += idx;
        self.next_anchor(conceal_end_char_idx)
    }

    pub fn next_anchor(&self, current_char_idx: usize) -> usize {
        let next_diag_start = self
            .doc
            .diagnostics
            .get(self.idx)
            .map_or(usize::MAX, |diag| diag.range.start);
        if (current_char_idx..next_diag_start).contains(&self.cursor) {
            self.cursor
        } else {
            next_diag_start
        }
    }

    pub fn clear(&mut self) {
        self.cursor_line = false;
        self.stack.clear();
    }

    fn process_anchor_impl(
        &mut self,
        grapheme: &FormattedGrapheme,
        width: u16,
        horizontal_off: usize,
    ) -> bool {
        // TODO: doing the cursor tracking here works well but is somewhat
        // duplicate effort/tedious maybe centralize this somewhere?
        // In the DocFormatter?
        if grapheme.char_idx == self.cursor {
            self.cursor_line = true;
            if self
                .doc
                .diagnostics
                .get(self.idx)
                .is_none_or(|diag| diag.range.start != grapheme.char_idx)
            {
                return false;
            }
        }

        let Some(anchor_col) = grapheme.visual_pos.col.checked_sub(horizontal_off) else {
            return true;
        };
        if anchor_col >= width as usize {
            return true;
        }

        for diag in &self.doc.diagnostics[self.idx..] {
            if diag.range.start != grapheme.char_idx {
                break;
            }
            self.stack.push((diag, anchor_col as u16));
            self.idx += 1;
        }
        false
    }

    pub fn proccess_anchor(
        &mut self,
        grapheme: &FormattedGrapheme,
        width: u16,
        horizontal_off: usize,
    ) -> usize {
        if self.process_anchor_impl(grapheme, width, horizontal_off) {
            self.idx += self.doc.diagnostics[self.idx..]
                .iter()
                .take_while(|diag| diag.range.start == grapheme.char_idx)
                .count();
        }
        self.next_anchor(grapheme.char_idx + 1)
    }

    pub fn filter(&self) -> DiagnosticFilter {
        if self.cursor_line {
            self.config.cursor_line
        } else {
            self.config.other_lines
        }
    }

    pub fn compute_line_diagnostics(&mut self) {
        let filter = if self.cursor_line {
            self.cursor_line = false;
            self.config.cursor_line
        } else {
            self.config.other_lines
        };
        let DiagnosticFilter::Enable(filter) = filter else {
            self.stack.clear();
            return;
        };
        self.stack.retain(|(diag, _)| diag.severity() >= filter);
        self.stack.truncate(self.config.max_diagnostics)
    }

    pub fn has_multi(&self, width: u16) -> bool {
        self.stack
            .last()
            .is_some_and(|&(_, anchor)| anchor > self.config.max_diagnostic_start(width))
    }
}

pub(crate) struct InlineDiagnostics<'a> {
    state: InlineDiagnosticAccumulator<'a>,
    width: u16,
    horizontal_off: usize,
    visual_row_start: usize,
    cursor_row: Option<std::ops::Range<usize>>,
}

#[derive(Debug)]
struct DiagnosticCheckpoint {
    idx: usize,
    cursor_line: bool,
    stack: Vec<(usize, u16)>,
    cursor: usize,
    visual_row_start: usize,
    cursor_row: Option<std::ops::Range<usize>>,
}

impl<'a> InlineDiagnostics<'a> {
    #[allow(clippy::new_ret_no_self)]
    pub(crate) fn new(
        doc: &'a Document,
        cursor: usize,
        width: u16,
        horizontal_off: usize,
        config: InlineDiagnosticsConfig,
    ) -> Box<dyn LineAnnotation + 'a> {
        Box::new(InlineDiagnostics {
            state: InlineDiagnosticAccumulator::new(cursor, doc, config),
            width,
            horizontal_off,
            visual_row_start: 0,
            cursor_row: None,
        })
    }
}

impl LineAnnotation for InlineDiagnostics<'_> {
    fn checkpoint_key(&self) -> Option<u64> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.state
            .doc
            .diagnostics_layout_generation()
            .hash(&mut hasher);
        self.state.config.hash(&mut hasher);
        self.width.hash(&mut hasher);
        self.horizontal_off.hash(&mut hasher);
        Some(hasher.finish())
    }

    fn checkpoint(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        // The rendering decoration also accumulates anchors from the visible
        // traversal. Resume only before pending diagnostics/cursor-line state
        // so that both traversals still observe those anchors themselves.
        if !self.state.stack.is_empty() || self.state.cursor_line {
            return None;
        }
        // Entries refer to this immutable diagnostic slice. Store indices,
        // rather than borrowed diagnostics, so checkpoints can outlive a view.
        let diagnostics = self.state.doc.diagnostics();
        let stack = self
            .state
            .stack
            .iter()
            .map(|&(diagnostic, anchor)| {
                let address = diagnostic as *const Diagnostic as usize;
                diagnostics
                    .binary_search_by_key(&address, |diag| diag as *const Diagnostic as usize)
                    .ok()
                    .map(|index| (index, anchor))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Arc::new(DiagnosticCheckpoint {
            idx: self.state.idx,
            cursor_line: self.state.cursor_line,
            stack,
            cursor: self.state.cursor,
            visual_row_start: self.visual_row_start,
            cursor_row: self.cursor_row.clone(),
        }))
    }

    fn checkpoint_is_valid(&self, state: &(dyn Any + Send + Sync), char_idx: usize) -> bool {
        let Some(state) = state.downcast_ref::<DiagnosticCheckpoint>() else {
            return false;
        };
        state.cursor == self.state.cursor
            || state
                .cursor_row
                .as_ref()
                .is_some_and(|row| row.contains(&self.state.cursor))
            || (state.cursor >= char_idx && self.state.cursor >= char_idx)
            || self.state.config.cursor_line == self.state.config.other_lines
    }

    fn checkpoint_next_anchor(&self, char_idx: usize) -> Option<usize> {
        Some(self.state.next_anchor(char_idx))
    }

    fn restore_checkpoint(&mut self, state: &(dyn Any + Send + Sync)) -> bool {
        let Some(state) = state.downcast_ref::<DiagnosticCheckpoint>() else {
            return false;
        };
        let diagnostics = self.state.doc.diagnostics();
        if state.idx > diagnostics.len()
            || state
                .stack
                .iter()
                .any(|&(index, _)| index >= diagnostics.len())
        {
            return false;
        }
        self.state.idx = state.idx;
        self.state.cursor_line = state.cursor_line;
        self.visual_row_start = state.visual_row_start;
        self.cursor_row = state
            .cursor_row
            .clone()
            .filter(|row| row.contains(&self.state.cursor));
        self.state.stack.clear();
        self.state.stack.extend(
            state
                .stack
                .iter()
                .map(|&(index, anchor)| (&diagnostics[index], anchor)),
        );
        true
    }

    fn reset_pos(&mut self, char_idx: usize) -> usize {
        self.visual_row_start = char_idx;
        self.cursor_row = None;
        self.state.reset_pos(char_idx)
    }

    fn skip_concealed_anchors(&mut self, conceal_end_char_idx: usize) -> usize {
        self.state.skip_concealed(conceal_end_char_idx)
    }

    fn process_anchor(&mut self, grapheme: &FormattedGrapheme) -> usize {
        self.state
            .proccess_anchor(grapheme, self.width, self.horizontal_off)
    }

    fn insert_virtual_lines(
        &mut self,
        line_end_char_idx: usize,
        _line_end_visual_pos: Position,
        _doc_line: usize,
    ) -> Position {
        if self.state.cursor_line {
            self.cursor_row = Some(self.visual_row_start..line_end_char_idx);
        }
        self.visual_row_start = line_end_char_idx;
        self.state.compute_line_diagnostics();
        let multi = self.state.has_multi(self.width);
        let doc = self.state.doc;
        let diagostic_height: usize = self
            .state
            .stack
            .drain(..)
            .map(|(diag, anchor)| {
                let text_fmt = self.state.config.text_fmt(anchor, self.width);
                doc.diagnostic_message_dimensions(diag.message.as_str().trim(), &text_fmt)
                    .0
            })
            .sum();
        Position::new(multi as usize + diagostic_height, 0)
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use crate::{editor::Config, View};
    use arc_swap::ArcSwap;
    use helix_core::{
        diagnostic::{DiagnosticProvider, LanguageServerId},
        doc_formatter::DocumentFormatter,
        syntax,
        text_annotations::TextAnnotations,
        Rope, Selection, Transaction,
    };

    fn document() -> Document {
        let mut config = Config::default();
        config.soft_wrap.enable = Some(true);
        Document::from(
            Rope::from_str(&format!("{}\nlater line\n", "word ".repeat(4000))),
            None,
            Arc::new(ArcSwap::from_pointee(config)),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        )
    }

    fn diagnostic(start: usize, end: usize) -> Diagnostic {
        Diagnostic {
            range: helix_core::diagnostic::Range { start, end },
            starts_at_word: false,
            ends_at_word: false,
            zero_width: false,
            line: 0,
            message: "warning on cursor row".into(),
            severity: Some(Severity::Warning),
            provider: DiagnosticProvider::Lsp {
                server_id: LanguageServerId::default(),
                identifier: None,
            },
            code: None,
            tags: Vec::new(),
            source: None,
            data: None,
        }
    }

    fn annotations(doc: &Document, cursor: usize, width: u16) -> TextAnnotations<'_> {
        let mut annotations = TextAnnotations::default();
        annotations.add_line_annotation(InlineDiagnostics::new(
            doc,
            cursor,
            width,
            0,
            InlineDiagnosticsConfig {
                max_diagnostics: 1,
                ..InlineDiagnosticsConfig::default()
            },
        ));
        annotations
    }

    fn assert_layout(doc: &Document, cursor: usize, width: u16, target: usize, expect_warm: bool) {
        let annotations = annotations(doc, cursor, width);
        let format = doc.text_format(width, None);
        let formatter = DocumentFormatter::new_at_prev_checkpoint(
            doc.text().slice(..),
            &format,
            &annotations,
            target,
        );
        if expect_warm {
            assert!(
                formatter.next_char_pos() > target - 1500,
                "lost compatible checkpoints"
            );
        }
        let actual: Vec<_> = formatter
            .map(|g| (g.char_idx, g.raw.to_string(), g.visual_pos))
            .collect();
        let mut plain = format.clone();
        plain.checkpoint_cache = None;
        let expected: Vec<_> = DocumentFormatter::new_at_prev_checkpoint(
            doc.text().slice(..),
            &plain,
            &annotations,
            0,
        )
        .filter(|g| g.char_idx >= actual[0].0)
        .map(|g| (g.char_idx, g.raw.to_string(), g.visual_pos))
        .collect();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn cursor_row_compatibility_split_widths_and_unrelated_edits_preserve_layout() {
        let mut doc = document();
        let view = View::new(doc.id(), Config::default().gutters);
        doc.set_selection(view.id, Selection::point(0));
        doc.replace_diagnostics([diagnostic(0, 1), diagnostic(20001, 20002)], &[], None);
        for width in [80, 120] {
            assert_layout(&doc, 0, width, 18000, false);
        }
        for width in [80, 120] {
            assert_layout(&doc, 1, width, 18000, true);
        }
        // Moving into another wrapped row changes the cursor-line filter and must
        // rebuild affected geometry; subsequent seeks can reuse that new state.
        assert_layout(&doc, 100, 80, 18000, false);
        assert_layout(&doc, 101, 80, 18000, true);
        let end = doc.text().len_chars() - 1;
        let edit = Transaction::change(doc.text(), [(end, end, Some("tail".into()))].into_iter());
        assert!(doc.apply(&edit, view.id));
        assert_layout(&doc, 101, 80, 18000, true);
        // A diagnostic anchored before the edit can disappear when its range is
        // deleted. Its earlier virtual rows must be invalidated as well.
        let edit = Transaction::change(doc.text(), [(0, 150, None)].into_iter());
        assert!(doc.apply(&edit, view.id));
        assert_layout(&doc, 0, 80, 17800, false);
    }

    #[tokio::test]
    async fn remapping_that_reorders_same_anchor_diagnostics_invalidates_earlier_rows() {
        let mut doc = document();
        let view = View::new(doc.id(), Config::default().gutters);
        doc.set_selection(view.id, Selection::point(0));
        let mut first = diagnostic(0, 20002);
        first.ends_at_word = true;
        first.message = "short".into();
        let mut second = diagnostic(0, 20007);
        second.message = "long wrapped warning ".repeat(50);
        doc.replace_diagnostics([first, second], &[], None);
        assert_layout(&doc, 0, 80, 18000, false);
        assert_layout(&doc, 1, 80, 18000, true);
        let edit = Transaction::change(
            doc.text(),
            [(20002, 20009, Some("word ".into()))].into_iter(),
        );
        assert!(doc.apply(&edit, view.id));
        assert!(doc.diagnostics()[0].message.starts_with("long"));
        assert_layout(&doc, 1, 80, 18000, false);
    }
}
