use crate::doc_formatter::{DocumentFormatter, TextFormat};
use crate::text_annotations::{InlineAnnotation, Overlay, TextAnnotations};

impl TextFormat {
    fn new_test(softwrap: bool) -> Self {
        TextFormat {
            soft_wrap: softwrap,
            tab_width: 2,
            max_wrap: 3,
            max_indent_retain: 4,
            wrap_indicator: ".".into(),
            wrap_indicator_highlight: None,
            // use a prime number to allow lining up too often with repeat
            viewport_width: 17,
            soft_wrap_at_text_width: false,
            checkpoint_cache: None,
        }
    }
}

impl<'t> DocumentFormatter<'t> {
    fn collect_to_str(&mut self) -> String {
        use std::fmt::Write;
        let mut res = String::new();
        let viewport_width = self.text_fmt.viewport_width;
        let soft_wrap_at_text_width = self.text_fmt.soft_wrap_at_text_width;
        let mut line = 0;

        for grapheme in self {
            if grapheme.visual_pos.row != line {
                line += 1;
                assert_eq!(grapheme.visual_pos.row, line);
                write!(res, "\n{}", ".".repeat(grapheme.visual_pos.col)).unwrap();
            }
            if !soft_wrap_at_text_width {
                assert!(
                    grapheme.visual_pos.col <= viewport_width as usize,
                    "softwrapped failed {}<={viewport_width}",
                    grapheme.visual_pos.col
                );
            }
            write!(res, "{}", grapheme.raw).unwrap();
        }

        res
    }
}

fn softwrap_text(text: &str) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(true),
        &TextAnnotations::default(),
        0,
    )
    .collect_to_str()
}

#[test]
fn basic_softwrap() {
    assert_eq!(
        softwrap_text(&"foo ".repeat(10)),
        "foo foo foo foo \n.foo foo foo foo \n.foo foo  "
    );
    assert_eq!(
        softwrap_text(&"fooo ".repeat(10)),
        "fooo fooo fooo \n.fooo fooo fooo \n.fooo fooo fooo \n.fooo  "
    );

    // check that we don't wrap unnecessarily
    assert_eq!(softwrap_text("\t\txxxx1xxxx2xx\n"), "    xxxx1xxxx2xx \n ");
}

#[test]
fn softwrap_indentation() {
    assert_eq!(
        softwrap_text("\t\tfoo1 foo2 foo3 foo4 foo5 foo6\n"),
        "    foo1 foo2 \n.....foo3 foo4 \n.....foo5 foo6 \n "
    );
    assert_eq!(
        softwrap_text("\t\t\tfoo1 foo2 foo3 foo4 foo5 foo6\n"),
        "      foo1 foo2 \n.foo3 foo4 foo5 \n.foo6 \n "
    );
}

#[test]
fn long_word_softwrap() {
    assert_eq!(
        softwrap_text("\t\txxxx1xxxx2xxxx3xxxx4xxxx5xxxx6xxxx7xxxx8xxxx9xxx\n"),
        "    xxxx1xxxx2xxx\n.....x3xxxx4xxxx5\n.....xxxx6xxxx7xx\n.....xx8xxxx9xxx \n "
    );
    assert_eq!(
        softwrap_text("xxxxxxxx1xxxx2xxx\n"),
        "xxxxxxxx1xxxx2xxx\n. \n "
    );
    assert_eq!(
        softwrap_text("\t\txxxx1xxxx 2xxxx3xxxx4xxxx5xxxx6xxxx7xxxx8xxxx9xxx\n"),
        "    xxxx1xxxx \n.....2xxxx3xxxx4x\n.....xxx5xxxx6xxx\n.....x7xxxx8xxxx9\n.....xxx \n "
    );
    assert_eq!(
        softwrap_text("\t\txxxx1xxx 2xxxx3xxxx4xxxx5xxxx6xxxx7xxxx8xxxx9xxx\n"),
        "    xxxx1xxx 2xxx\n.....x3xxxx4xxxx5\n.....xxxx6xxxx7xx\n.....xx8xxxx9xxx \n "
    );
}

#[test]
fn softwrap_multichar_grapheme() {
    assert_eq!(
        softwrap_text("xxxx xxxx xxx a\u{0301}bc\n"),
        "xxxx xxxx xxx \n.ábc \n "
    )
}

fn softwrap_text_at_text_width(text: &str) -> String {
    let mut text_fmt = TextFormat::new_test(true);
    text_fmt.soft_wrap_at_text_width = true;
    let annotations = TextAnnotations::default();
    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text.into(), &text_fmt, &annotations, 0);
    formatter.collect_to_str()
}
#[test]
fn long_word_softwrap_text_width() {
    assert_eq!(
        softwrap_text_at_text_width("xxxxxxxx1xxxx2xxx\nxxxxxxxx1xxxx2xxx"),
        "xxxxxxxx1xxxx2xxx \nxxxxxxxx1xxxx2xxx "
    );
}

fn overlay_text(text: &str, char_pos: usize, softwrap: bool, overlays: &[Overlay]) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(softwrap),
        TextAnnotations::default().add_overlay(overlays, None),
        char_pos,
    )
    .collect_to_str()
}

#[test]
fn overlay() {
    assert_eq!(
        overlay_text(
            "foobar",
            0,
            false,
            &[Overlay::new(0, "X"), Overlay::new(2, "\t")],
        ),
        "Xo  bar "
    );
    assert_eq!(
        overlay_text(
            &"foo ".repeat(10),
            0,
            true,
            &[
                Overlay::new(2, "\t"),
                Overlay::new(5, "\t"),
                Overlay::new(16, "X"),
            ]
        ),
        "fo   f  o foo \n.foo Xoo foo foo \n.foo foo foo  "
    );
}

fn annotate_text(text: &str, softwrap: bool, annotations: &[InlineAnnotation]) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(softwrap),
        TextAnnotations::default().add_inline_annotations(annotations, None),
        0,
    )
    .collect_to_str()
}

#[test]
fn annotation() {
    assert_eq!(
        annotate_text("bar", false, &[InlineAnnotation::new(0, "foo")]),
        "foobar "
    );
    assert_eq!(
        annotate_text(
            &"foo ".repeat(10),
            true,
            &[InlineAnnotation::new(0, "foo ")]
        ),
        "foo foo foo foo \n.foo foo foo foo \n.foo foo foo  "
    );
}

#[test]
fn annotation_and_overlay() {
    let annotations = [InlineAnnotation {
        char_idx: 0,
        text: "fooo".into(),
    }];
    let overlay = [Overlay {
        char_idx: 0,
        grapheme: "\t".into(),
    }];
    assert_eq!(
        DocumentFormatter::new_at_prev_checkpoint(
            "bbar".into(),
            &TextFormat::new_test(false),
            TextAnnotations::default()
                .add_inline_annotations(annotations.as_slice(), None)
                .add_overlay(overlay.as_slice(), None),
            0,
        )
        .collect_to_str(),
        "fooo  bar "
    );
}

fn layout(formatter: DocumentFormatter<'_>) -> Vec<(String, bool, usize, usize, crate::Position)> {
    formatter
        .map(|g| {
            (
                g.raw.to_string(),
                g.is_virtual(),
                g.char_idx,
                g.line_idx,
                g.visual_pos,
            )
        })
        .collect()
}

#[test]
fn cached_unicode_wrapping_and_position_queries_match_full_traversal() {
    use super::FormatterCache;
    use crate::position::char_idx_at_visual_block_offset;
    use crate::{visual_offset_from_block, Rope};
    use std::sync::Arc;

    // Includes long unbroken words, regional indicators at rope chunk boundaries,
    // combining clusters, wide characters, retained indentation and tabs.
    let text = Rope::from_str(&format!(
        "\t  {}\n{}",
        "a界e\u{301}🇵🇱\t word ".repeat(600),
        "x".repeat(12000)
    ));
    let inline = [
        InlineAnnotation::new(2048, "virtual\n\ttext"),
        InlineAnnotation::new(6000, "色"),
    ];
    let overlays = [Overlay::new(3000, "界"), Overlay::new(7500, "\t")];
    let mut annotations = TextAnnotations::default();
    annotations
        .add_inline_annotations(&inline, None)
        .add_overlay(&overlays, None);
    for wrapped in [false, true] {
        let plain = TextFormat::new_test(wrapped);
        let mut cached = plain.clone();
        cached.checkpoint_cache = Some(Arc::new(FormatterCache::default()));
        let expected = layout(DocumentFormatter::new_at_prev_checkpoint(
            text.slice(..),
            &plain,
            &annotations,
            0,
        ));
        assert_eq!(
            layout(DocumentFormatter::new_at_prev_checkpoint(
                text.slice(..),
                &cached,
                &annotations,
                0
            )),
            expected
        );
        for target in [2047, 2048, 4096, 6001, 8200, 12000, text.len_chars() - 1] {
            let formatter = DocumentFormatter::new_at_prev_checkpoint(
                text.slice(..),
                &cached,
                &annotations,
                target,
            );
            if target - formatter.block_start() >= super::CHECKPOINT_INTERVAL + 64 {
                assert!(
                    formatter.next_char_pos() > formatter.block_start(),
                    "no checkpoint near {target}, wrapped={wrapped}"
                );
            }
            assert!(target - formatter.next_char_pos() < super::CHECKPOINT_INTERVAL + 64);
            let actual = layout(formatter);
            let base_row = expected
                .iter()
                .find(|g| g.3 == text.char_to_line(target))
                .unwrap()
                .4
                .row;
            let start = expected
                .iter()
                .position(|g| {
                    let mut normalized = g.clone();
                    normalized.4.row = normalized.4.row.saturating_sub(base_row);
                    Some(&normalized) == actual.first()
                })
                .unwrap();
            let mut expected_suffix = expected[start..].to_vec();
            for grapheme in &mut expected_suffix {
                grapheme.4.row -= base_row;
            }
            assert_eq!(actual, expected_suffix);
            let expected_pos =
                visual_offset_from_block(text.slice(..), target, target, &plain, &annotations);
            assert_eq!(
                visual_offset_from_block(text.slice(..), target, target, &cached, &annotations),
                expected_pos
            );
            for column in [
                0,
                expected_pos.0.col,
                expected_pos.0.col.saturating_sub(1),
                expected_pos.0.col + 1,
            ] {
                assert_eq!(
                    char_idx_at_visual_block_offset(
                        text.slice(..),
                        target,
                        expected_pos.0.row,
                        column,
                        &cached,
                        &annotations
                    ),
                    char_idx_at_visual_block_offset(
                        text.slice(..),
                        target,
                        expected_pos.0.row,
                        column,
                        &plain,
                        &annotations
                    ),
                    "wrapped={wrapped}, target={target}, column={column}"
                );
            }
        }
    }
}

#[test]
fn cached_layout_changes_and_explicit_text_invalidation() {
    use super::FormatterCache;
    use crate::{visual_offset_from_block, Rope};
    use std::sync::Arc;

    let mut text = Rope::from_str(&"a\t界 ".repeat(1500));
    let cache = Arc::new(FormatterCache::default());
    let mut format = TextFormat::new_test(true);
    format.checkpoint_cache = Some(cache.clone());
    let annotations = TextAnnotations::default();
    layout(DocumentFormatter::new_at_prev_checkpoint(
        text.slice(..),
        &format,
        &annotations,
        0,
    ));
    for (tab_width, width, indicator) in [(4, 23, "→ "), (8, 37, "\t"), (2, 17, ".")] {
        format.tab_width = tab_width;
        format.viewport_width = width;
        format.wrap_indicator = indicator.into();
        let mut plain = format.clone();
        plain.checkpoint_cache = None;
        let target = text.len_chars() - 1;
        assert_eq!(
            visual_offset_from_block(text.slice(..), target, target, &format, &annotations),
            visual_offset_from_block(text.slice(..), target, target, &plain, &annotations)
        );
        // Populate checkpoints for the next mutation, including same-position annotations.
        layout(DocumentFormatter::new_at_prev_checkpoint(
            text.slice(..),
            &format,
            &annotations,
            0,
        ));
    }
    let inline = [InlineAnnotation::new(1, "\nchanged\t")];
    let mut changed_annotations = TextAnnotations::default();
    changed_annotations.add_inline_annotations(&inline, None);
    let mut plain = format.clone();
    plain.checkpoint_cache = None;
    let target = text.len_chars() - 1;
    assert_eq!(
        visual_offset_from_block(
            text.slice(..),
            target,
            target,
            &format,
            &changed_annotations
        ),
        visual_offset_from_block(text.slice(..), target, target, &plain, &changed_annotations)
    );
    text.remove(100..101);
    text.insert(100, "\t"); // Same character count; length alone cannot invalidate it.
    cache.clear();
    assert_eq!(
        visual_offset_from_block(text.slice(..), target, target, &format, &annotations),
        visual_offset_from_block(text.slice(..), target, target, &plain, &annotations)
    );
}

#[test]
fn cached_line_end_skips_only_to_the_exact_next_line_or_eof() {
    use super::FormatterCache;
    use crate::Rope;
    use std::sync::Arc;

    let text = Rope::from_str(&format!(
        "\t{}\n{}",
        "a界\t".repeat(1000),
        "e\u{301}".repeat(2000)
    ));
    let mut format = TextFormat::new_test(false);
    format.checkpoint_cache = Some(Arc::new(FormatterCache::default()));
    let annotations = TextAnnotations::default();
    let expected = layout(DocumentFormatter::new_at_prev_checkpoint(
        text.slice(..),
        &format,
        &annotations,
        0,
    ));
    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text.slice(..), &format, &annotations, 0);
    assert!(formatter.skip_to_line_end());
    let actual = layout(formatter);
    let start = expected.iter().position(|g| g.3 == 1).unwrap();
    assert_eq!(actual, expected[start..]);
    let mut formatter = DocumentFormatter::new_at_prev_checkpoint(
        text.slice(..),
        &format,
        &annotations,
        text.len_chars() - 1,
    );
    assert!(formatter.skip_to_line_end());
    assert!(formatter.next().is_none());
}

#[test]
fn stateful_line_annotations_retain_full_traversal_and_callbacks() {
    use super::{FormattedGrapheme, FormatterCache};
    use crate::{text_annotations::LineAnnotation, Position, Rope};
    use std::{cell::Cell, rc::Rc, sync::Arc};
    struct Track(Rc<Cell<usize>>);
    impl LineAnnotation for Track {
        fn reset_pos(&mut self, _pos: usize) -> usize {
            0
        }
        fn process_anchor(&mut self, grapheme: &FormattedGrapheme) -> usize {
            self.0.set(self.0.get() + 1);
            grapheme.char_idx + grapheme.doc_chars().max(1)
        }
        fn insert_virtual_lines(
            &mut self,
            _char_idx: usize,
            _position: Position,
            _line: usize,
        ) -> Position {
            Position::new(1, 0)
        }
    }
    let text = Rope::from_str(&format!("{}\nend", "word ".repeat(1000)));
    let mut format = TextFormat::new_test(true);
    format.checkpoint_cache = Some(Arc::new(FormatterCache::default()));
    let calls = Rc::new(Cell::new(0));
    let mut annotations = TextAnnotations::default();
    annotations.add_line_annotation(Box::new(Track(calls.clone())));
    let expected = layout(DocumentFormatter::new_at_prev_checkpoint(
        text.slice(..),
        &format,
        &annotations,
        0,
    ));
    let first_calls = calls.replace(0);
    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text.slice(..), &format, &annotations, 4000);
    assert_eq!(formatter.next_char_pos(), 0);
    assert!(!formatter.skip_to_line_end());
    assert_eq!(layout(formatter), expected);
    assert_eq!(calls.get(), first_calls);
}

#[test]
fn checkpoint_cache_is_bounded_and_send() {
    use super::FormatterCache;
    use crate::Rope;
    use std::sync::Arc;
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FormatterCache>();
    let text = Rope::from_str(&format!("{}\n", "word ".repeat(500)).repeat(40));
    let cache = Arc::new(FormatterCache::default());
    let mut format = TextFormat::new_test(false);
    format.checkpoint_cache = Some(cache.clone());
    layout(DocumentFormatter::new_at_prev_checkpoint(
        text.slice(..),
        &format,
        &TextAnnotations::default(),
        0,
    ));
    let state = cache.0.lock();
    assert!(state.entries.len() <= super::MAX_CACHED_LAYOUTS);
    for layout in &state.entries {
        assert!(layout.lines.len() <= super::MAX_CACHED_LINES);
        assert!(layout.count <= super::MAX_CHECKPOINTS);
    }
}

#[test]
fn split_widths_keep_independent_checkpoints_after_a_tail_edit() {
    use super::FormatterCache;
    use crate::{Rope, Transaction};
    use std::sync::Arc;
    let mut text = Rope::from_str(&format!("{}\nlater line\n", "words ".repeat(4000)));
    let cache = Arc::new(FormatterCache::default());
    let annotations = TextAnnotations::default();
    let formats: Vec<_> = [60, 90]
        .into_iter()
        .map(|width| {
            let mut format = TextFormat::new_test(true);
            format.viewport_width = width;
            format.checkpoint_cache = Some(cache.clone());
            format
        })
        .collect();
    for format in &formats {
        DocumentFormatter::new_at_prev_checkpoint(text.slice(..), format, &annotations, 0)
            .for_each(drop);
    }
    let old = text.clone();
    let transaction = Transaction::change(
        &text,
        [(text.len_chars() - 2, text.len_chars() - 1, Some("x".into()))].into_iter(),
    );
    assert!(transaction.apply(&mut text));
    cache.invalidate_after_change(old.slice(..), text.slice(..), transaction.changes());
    for _ in 0..3 {
        for format in &formats {
            let formatter = DocumentFormatter::new_at_prev_checkpoint(
                text.slice(..),
                format,
                &annotations,
                20_000,
            );
            assert!(formatter.char_pos > 18_000);
            let actual = layout(formatter);
            let mut plain = format.clone();
            plain.checkpoint_cache = None;
            let expected = layout(DocumentFormatter::new_at_prev_checkpoint(
                text.slice(..),
                &plain,
                &annotations,
                0,
            ));
            assert_eq!(
                actual,
                expected
                    .into_iter()
                    .filter(|grapheme| grapheme.2 >= actual[0].2)
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn cached_anchor_queries_preserve_row_limits_and_temporary_text_identity() {
    use super::FormatterCache;
    use crate::{visual_offset_from_anchor, visual_offset_from_block, Rope};
    use std::sync::Arc;
    let text = Rope::from_str(&"long word\t界 ".repeat(1500));
    let annotations = TextAnnotations::default();
    let plain = TextFormat::new_test(true);
    let mut cached = plain.clone();
    cached.checkpoint_cache = Some(Arc::new(FormatterCache::default()));
    layout(DocumentFormatter::new_at_prev_checkpoint(
        text.slice(..),
        &cached,
        &annotations,
        0,
    ));
    for anchor in [2048, 4000, 7000, text.len_chars()] {
        for pos in [
            0,
            anchor.saturating_sub(1),
            anchor,
            (anchor + 3).min(text.len_chars()),
            text.len_chars(),
        ] {
            for rows in [0, 1, 3, 100_000] {
                assert_eq!(
                    visual_offset_from_anchor(
                        text.slice(..),
                        anchor,
                        pos,
                        &cached,
                        &annotations,
                        rows
                    ),
                    visual_offset_from_anchor(
                        text.slice(..),
                        anchor,
                        pos,
                        &plain,
                        &annotations,
                        rows
                    ),
                    "anchor={anchor}, pos={pos}, rows={rows}"
                );
            }
        }
    }
    let other = Rope::from_str(&"\tword long色 ".repeat(1500));
    let target = other.len_chars() - 1;
    assert_eq!(
        visual_offset_from_block(other.slice(..), target, target, &cached, &annotations),
        visual_offset_from_block(other.slice(..), target, target, &plain, &annotations)
    );
}

#[test]
fn cached_invisible_tail_does_not_skip_inline_newlines_back_into_the_viewport() {
    use super::FormatterCache;
    use crate::Rope;
    use std::sync::Arc;
    let text = Rope::from_str(&format!("{}\nnext", "x".repeat(4000)));
    let inline = [InlineAnnotation::new(3000, "\nvisible virtual text")];
    let mut annotations = TextAnnotations::default();
    annotations.add_inline_annotations(&inline, None);
    let mut format = TextFormat::new_test(false);
    format.checkpoint_cache = Some(Arc::new(FormatterCache::default()));
    layout(DocumentFormatter::new_at_prev_checkpoint(
        text.slice(..),
        &format,
        &annotations,
        0,
    ));
    let formatter =
        DocumentFormatter::new_at_prev_checkpoint(text.slice(..), &format, &annotations, 100);
    assert!(formatter.cached_line_end().is_none());
    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text.slice(..), &format, &annotations, 3500);
    // After the inline newline has actually been visited, the remaining tail is safe.
    while formatter.next_char_pos() <= 3500 {
        formatter.next().unwrap();
    }
    assert!(formatter.cached_line_end().is_some());
}

#[test]
fn edited_layout_retains_only_safe_prefix_checkpoints() {
    use super::FormatterCache;
    use crate::{Rope, Transaction};
    use std::sync::Arc;
    for wrapped in [false, true] {
        for replacement in ["\u{301}", "\t", "\n", "👩\u{200d}💻"] {
            let mut text = Rope::from_str(&"ab e\u{301}\t界 ".repeat(1500));
            let annotations = TextAnnotations::default();
            let mut cached = TextFormat::new_test(wrapped);
            let cache = Arc::new(FormatterCache::default());
            cached.checkpoint_cache = Some(cache.clone());
            DocumentFormatter::new_at_prev_checkpoint(text.slice(..), &cached, &annotations, 0)
                .for_each(drop);
            let old = text.clone();
            let from = old.len_chars() - 3;
            let transaction = Transaction::change(
                &old,
                [(from, from + 1, Some(replacement.into()))].into_iter(),
            );
            assert!(transaction.apply(&mut text));
            cache.invalidate_after_change(old.slice(..), text.slice(..), transaction.changes());
            let target = text.len_chars() - 1;
            let formatter = DocumentFormatter::new_at_prev_checkpoint(
                text.slice(..),
                &cached,
                &annotations,
                target,
            );
            let resumed = formatter.next_char_pos();
            if !wrapped && replacement != "\n" {
                assert!(
                    resumed > 2000,
                    "an unchanged unwrapped prefix should survive"
                );
            }
            let actual = layout(formatter);
            let mut plain = cached.clone();
            plain.checkpoint_cache = None;
            let expected = layout(DocumentFormatter::new_at_prev_checkpoint(
                text.slice(..),
                &plain,
                &annotations,
                target,
            ));
            let start = expected
                .iter()
                .position(|grapheme| grapheme.2 == resumed)
                .unwrap();
            assert_eq!(
                actual,
                expected[start..],
                "wrapped={wrapped}, replacement={replacement:?}"
            );
        }
    }
}

#[test]
fn edits_at_line_boundaries_invalidate_preceding_cr_and_lookahead() {
    use super::FormatterCache;
    use crate::{Rope, Transaction};
    use std::sync::Arc;
    for ending in ["\r", "\n", "\r\n"] {
        for replacement in ["\n", "\u{301}", "x"] {
            let mut text = Rope::from_str(&format!("{}{ending}next", "x".repeat(5000)));
            let annotations = TextAnnotations::default();
            let mut format = TextFormat::new_test(false);
            let cache = Arc::new(FormatterCache::default());
            format.checkpoint_cache = Some(cache.clone());
            DocumentFormatter::new_at_prev_checkpoint(text.slice(..), &format, &annotations, 0)
                .for_each(drop);
            let old = text.clone();
            let from = old.line_to_char(1);
            let transaction =
                Transaction::change(&old, [(from, from, Some(replacement.into()))].into_iter());
            assert!(transaction.apply(&mut text));
            cache.invalidate_after_change(old.slice(..), text.slice(..), transaction.changes());
            let actual = layout(DocumentFormatter::new_at_prev_checkpoint(
                text.slice(..),
                &format,
                &annotations,
                0,
            ));
            format.checkpoint_cache = None;
            let expected = layout(DocumentFormatter::new_at_prev_checkpoint(
                text.slice(..),
                &format,
                &annotations,
                0,
            ));
            assert_eq!(
                actual, expected,
                "ending={ending:?}, replacement={replacement:?}"
            );
        }
    }
}
