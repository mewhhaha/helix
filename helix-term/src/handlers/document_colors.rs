use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::Path,
    sync::Arc,
    time::Duration,
};

use futures_util::{stream::FuturesUnordered, StreamExt};
use helix_core::{
    syntax::config::LanguageServerFeature, text_annotations::InlineAnnotation, Assoc, ChangeSet,
    Rope,
};
use helix_event::{cancelable_future, register_hook, TaskHandle};
use helix_lsp::{lsp, OffsetEncoding};
use helix_view::{
    document::DocumentColorSwatches,
    editor::LspConfig,
    events::{
        ConfigDidChange, DocumentDidChange, DocumentDidOpen, LanguageServerExited,
        LanguageServerInitialized,
    },
    handlers::{lsp::DocumentColorsEvent, Handlers},
    Document, DocumentId, Editor, Theme,
};
use tokio::time::Instant;

use crate::job;

#[derive(Default)]
pub(super) struct DocumentColorsHandler {
    docs: HashMap<DocumentId, i32>,
}

const DOCUMENT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(250);

struct DocumentColor {
    range: Range<usize>,
    color: lsp::Color,
    server_order: usize,
}

fn document_colors_enabled(config: &LspConfig) -> bool {
    config.display_color_swatches || config.display_color_values
}

impl helix_event::AsyncHook for DocumentColorsHandler {
    type Event = DocumentColorsEvent;

    fn handle_event(&mut self, event: Self::Event, _timeout: Option<Instant>) -> Option<Instant> {
        let DocumentColorsEvent(doc_id, version) = event;
        self.docs.insert(doc_id, version);
        Some(Instant::now() + DOCUMENT_CHANGE_DEBOUNCE)
    }

    fn finish_debounce(&mut self) {
        let docs = std::mem::take(&mut self.docs);

        job::dispatch_blocking(move |editor, _compositor| {
            for (doc_id, version) in docs {
                // A completion preview can change the text after a real edit
                // queued this request. The server has not seen that preview.
                if editor
                    .document(doc_id)
                    .is_some_and(|doc| doc.version() == version)
                {
                    request_document_colors(editor, doc_id);
                }
            }
        });
    }
}

fn request_document_colors(editor: &mut Editor, doc_id: DocumentId) {
    if !document_colors_enabled(&editor.config().lsp) {
        return;
    }

    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };

    let Some(path) = doc.path().map(Path::to_path_buf) else {
        return;
    };
    let version = doc.version();
    let cancel = doc.color_swatch_controller.restart();

    let mut seen_language_servers = HashSet::new();
    let mut futures: FuturesUnordered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::DocumentColors)
        .filter(|ls| seen_language_servers.insert(ls.id()))
        .enumerate()
        .map(|(server_order, language_server)| {
            let text = doc.text().clone();
            let offset_encoding = language_server.offset_encoding();
            let future = language_server
                .text_document_document_color(doc.identifier(), None)
                .unwrap();

            async move {
                let colors =
                    document_color_ranges(&text, offset_encoding, server_order, future.await?);
                anyhow::Ok(colors)
            }
        })
        .collect();

    if futures.is_empty() {
        doc.color_swatches = None;
        return;
    }

    tokio::spawn(async move {
        let mut all_colors = Vec::new();
        loop {
            match cancelable_future(futures.next(), &cancel).await {
                Some(Some(Ok(items))) => all_colors.extend(items),
                Some(Some(Err(err))) => log::error!("document color request failed: {err}"),
                Some(None) => break,
                // The request was cancelled.
                None => return,
            }
        }
        job::dispatch(move |editor, _| {
            if !document_colors_enabled(&editor.config().lsp) {
                return;
            }
            if let Some(doc) = editor.document_mut(doc_id) {
                attach_document_colors(doc, version, &path, &cancel, all_colors);
            }
        })
        .await;
    });
}

fn document_color_ranges(
    text: &Rope,
    offset_encoding: OffsetEncoding,
    server_order: usize,
    colors: Vec<lsp::ColorInformation>,
) -> Vec<DocumentColor> {
    colors
        .into_iter()
        .filter_map(|info| {
            let color = info.color;
            if ![color.red, color.green, color.blue, color.alpha]
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
                || info.range.start >= info.range.end
                || info.range.start.line as usize >= text.len_lines()
                || info.range.end.line as usize >= text.len_lines()
            {
                return None;
            }
            let range = helix_lsp::util::lsp_range_to_range(text, info.range, offset_encoding)?;
            // Position conversion normally clamps invalid columns. A color must
            // not tint a different token when a server uses an oversized column
            // or splits a UTF-8 character / UTF-16 surrogate pair.
            if helix_lsp::util::pos_to_lsp_pos(text, range.from(), offset_encoding)
                != info.range.start
                || helix_lsp::util::pos_to_lsp_pos(text, range.to(), offset_encoding)
                    != info.range.end
            {
                return None;
            }
            (range.from() < range.to()).then_some(DocumentColor {
                range: range.from()..range.to(),
                color,
                server_order,
            })
        })
        .collect()
}

fn attach_document_colors(
    doc: &mut Document,
    version: i32,
    path: &Path,
    cancel: &TaskHandle,
    doc_colors: Vec<DocumentColor>,
) {
    // Cancellation must be checked in the dispatched callback too: an edit or
    // another request may have happened after the response was queued.
    if cancel.is_canceled() || doc.version() != version || doc.path() != Some(path) {
        return;
    }

    doc.color_swatches = color_annotations(doc_colors);
}

fn color_annotations(mut doc_colors: Vec<DocumentColor>) -> Option<DocumentColorSwatches> {
    if doc_colors.is_empty() {
        return None;
    }

    // Preserve configured server priority for matching starts instead of using
    // the order in which the concurrent requests completed.
    doc_colors.sort_by_key(|item| (item.range.start, item.server_order, item.range.end));

    let mut color_swatches = Vec::with_capacity(doc_colors.len());
    let mut color_swatches_padding = Vec::with_capacity(doc_colors.len());
    let mut colors = Vec::with_capacity(doc_colors.len());
    let mut color_ranges: Vec<(helix_core::syntax::Highlight, Range<usize>)> =
        Vec::with_capacity(doc_colors.len());

    for DocumentColor { range, color, .. } in doc_colors {
        let pos = range.start;
        color_swatches_padding.push(InlineAnnotation::new(pos, " "));
        color_swatches.push(InlineAnnotation::new(pos, "■"));
        let highlight = Theme::rgb_highlight(
            (color.red * 255.).round() as u8,
            (color.green * 255.).round() as u8,
            (color.blue * 255.).round() as u8,
        );
        colors.push(highlight);
        // OverlayHighlights requires disjoint ranges. Keep the first range
        // when servers report overlapping colors, consistently across replies.
        if color_ranges
            .last()
            .is_none_or(|(_, previous)| previous.end <= range.start)
        {
            color_ranges.push((
                Theme::rgb_background_highlight(
                    (color.red * 255.).round() as u8,
                    (color.green * 255.).round() as u8,
                    (color.blue * 255.).round() as u8,
                ),
                range,
            ));
        }
    }

    let mut annotations = DocumentColorSwatches {
        color_swatches,
        colors,
        color_swatches_padding,
        color_ranges: Arc::new(color_ranges),
        layout_keys: None,
    };
    annotations.refresh_layout_keys();
    Some(annotations)
}

fn update_color_ranges(colors: &mut DocumentColorSwatches, changes: &ChangeSet) {
    let color_ranges = Arc::make_mut(&mut colors.color_ranges);
    let mut edits = changes.changes_iter().peekable();
    color_ranges.retain(|(_, range)| {
        // Both the ranges and edits are sorted, so each edit is scanned once.
        while let Some((from, to, _)) = edits.peek() {
            if *to < range.start || (*to == range.start && from != to) {
                edits.next();
            } else {
                break;
            }
        }
        let affected = edits.peek().is_some_and(|(from, to, _)| {
            if from == to {
                range.start <= *from && *from <= range.end
            } else {
                *from < range.end && range.start < *to
            }
        });
        !affected
    });
    changes.update_positions(color_ranges.iter_mut().flat_map(|(_, range)| {
        [
            (&mut range.start, Assoc::After),
            (&mut range.end, Assoc::Before),
        ]
    }));
}

pub(super) fn register_hooks(handlers: &Handlers) {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        // when a document is initially opened, request colors for it
        request_document_colors(event.editor, event.doc);

        Ok(())
    });

    let tx = handlers.document_colors.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        // Update the color swatch' positions, helping ensure they are displayed in the
        // proper place.
        let apply_color_swatch_changes = |annotations: &mut Vec<InlineAnnotation>| {
            event.changes.update_positions(
                annotations
                    .iter_mut()
                    .map(|annotation| (&mut annotation.char_idx, helix_core::Assoc::After)),
            );
        };

        if let Some(colors) = &mut event.doc.color_swatches {
            apply_color_swatch_changes(&mut colors.color_swatches);
            apply_color_swatch_changes(&mut colors.color_swatches_padding);
            update_color_ranges(colors, event.changes);
            colors.refresh_layout_keys();
        }

        // Ghost transactions also change the document version and positions.
        event.doc.color_swatch_controller.cancel();
        // Avoid re-requesting document colors if the change is a ghost transaction (completion)
        // because the language server will not know about the updates to the document and will
        // give out-of-date locations.
        if !event.ghost_transaction {
            helix_event::send_blocking(
                &tx,
                DocumentColorsEvent(event.doc.id(), event.doc.version()),
            );
        }

        Ok(())
    });

    register_hook!(move |event: &mut LanguageServerInitialized<'_>| {
        let doc_ids: Vec<_> = event.editor.documents().map(|doc| doc.id()).collect();

        for doc_id in doc_ids {
            request_document_colors(event.editor, doc_id);
        }

        Ok(())
    });

    register_hook!(move |event: &mut LanguageServerExited<'_>| {
        // Clear and re-request all color swatches when a server exits.
        for doc in event.editor.documents_mut() {
            if doc.supports_language_server(event.server_id) {
                doc.color_swatches.take();
            }
        }

        let doc_ids: Vec<_> = event.editor.documents().map(|doc| doc.id()).collect();

        for doc_id in doc_ids {
            request_document_colors(event.editor, doc_id);
        }

        Ok(())
    });

    register_hook!(move |event: &mut ConfigDidChange<'_>| {
        let was_enabled = document_colors_enabled(&event.old.lsp);
        let is_enabled = document_colors_enabled(&event.new.lsp);
        if was_enabled && !is_enabled {
            for doc in event.editor.documents_mut() {
                doc.color_swatch_controller.cancel();
                doc.color_swatches = None;
            }
        } else if !was_enabled && is_enabled {
            let doc_ids: Vec<_> = event.editor.documents().map(|doc| doc.id()).collect();
            for doc_id in doc_ids {
                request_document_colors(event.editor, doc_id);
            }
        }
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use helix_core::{syntax, Selection, Transaction};
    use helix_view::{editor::Config, graphics::Color, ViewId};

    use super::*;

    fn red() -> lsp::Color {
        lsp::Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        }
    }

    fn blue() -> lsp::Color {
        lsp::Color {
            red: 0.0,
            green: 0.0,
            blue: 1.0,
            alpha: 0.5,
        }
    }

    fn color(range: Range<usize>, color: lsp::Color, server_order: usize) -> DocumentColor {
        DocumentColor {
            range,
            color,
            server_order,
        }
    }

    fn document() -> Document {
        let mut doc = Document::from(
            Rope::from_str("#fff\n"),
            None,
            Arc::new(ArcSwap::from_pointee(Config::default())),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );
        doc.set_selection(ViewId::default(), Selection::single(0, 0));
        doc.set_path(Some(Path::new("/tmp/helix-color-test.css")));
        doc
    }

    #[test]
    fn document_colors_are_requested_for_either_display_option() {
        for (swatches, values, enabled) in [
            (true, true, true),
            (true, false, true),
            (false, true, true),
            (false, false, false),
        ] {
            let config = LspConfig {
                display_color_swatches: swatches,
                display_color_values: values,
                ..LspConfig::default()
            };
            assert_eq!(document_colors_enabled(&config), enabled);
        }
    }

    #[test]
    fn unicode_color_ranges_use_utf16_and_reject_invalid_positions() {
        let text = Rope::from_str("😀 bg-red-500\n");
        let info = |start, end| lsp::ColorInformation {
            range: lsp::Range::new(lsp::Position::new(0, start), lsp::Position::new(0, end)),
            color: red(),
        };
        let colors = document_color_ranges(
            &text,
            OffsetEncoding::Utf16,
            0,
            vec![
                info(3, 13),
                info(3, 3),
                info(13, 3),
                info(1, 3), // Split the emoji's surrogate pair.
                info(3, 999),
                lsp::ColorInformation {
                    range: lsp::Range::new(lsp::Position::new(5, 0), lsp::Position::new(6, 0)),
                    color: red(),
                },
            ],
        );
        assert_eq!(colors.len(), 1);
        assert_eq!(colors[0].range, 2..12);
        assert_eq!(
            text.slice(colors[0].range.clone()).to_string(),
            "bg-red-500"
        );

        assert!(document_color_ranges(&text, OffsetEncoding::Utf8, 0, vec![info(1, 5)]).is_empty());
        let mut invalid = info(3, 13);
        invalid.color.red = f64::NAN as _;
        assert!(document_color_ranges(&text, OffsetEncoding::Utf16, 0, vec![invalid]).is_empty());
    }

    #[test]
    fn overlapping_responses_keep_deterministic_disjoint_value_ranges() {
        let colors = color_annotations(vec![
            color(0..6, blue(), 1),
            color(2..5, blue(), 0),
            color(10..12, red(), 0),
            color(4..8, blue(), 0),
            color(0..4, red(), 0),
        ])
        .unwrap();

        assert_eq!(colors.color_swatches.len(), 5);
        assert_eq!(
            *colors.color_ranges,
            vec![
                (Theme::rgb_background_highlight(255, 0, 0), 0..4),
                (Theme::rgb_background_highlight(0, 0, 255), 4..8),
                (Theme::rgb_background_highlight(255, 0, 0), 10..12),
            ]
        );
        assert_eq!(
            Theme::default().highlight(colors.color_ranges[1].0).bg,
            Some(Color::Rgb(0, 0, 255))
        );
        assert_eq!(
            Theme::default().highlight(colors.color_ranges[1].0).fg,
            Some(Color::Rgb(255, 255, 255))
        );
    }

    #[test]
    fn color_channels_round_to_the_nearest_display_byte() {
        let colors = color_annotations(vec![color(
            0..10,
            lsp::Color {
                red: 0.9826614,
                green: 44.0 / 255.0,
                blue: 54.0 / 255.0,
                alpha: 1.0,
            },
            0,
        )])
        .unwrap();
        assert_eq!(
            Theme::default().highlight(colors.colors[0]).fg,
            Some(Color::Rgb(251, 44, 54))
        );
    }

    #[test]
    fn edits_remove_changed_values_and_map_unaffected_adjacent_ranges() {
        let text = Rope::from_str("x#f00#0f0#00f\n");
        let mut colors = color_annotations(vec![
            color(1..5, red(), 0),
            color(5..9, blue(), 0),
            color(9..13, red(), 0),
        ])
        .unwrap();
        let prior_ranges = Arc::clone(&colors.color_ranges);
        let insertion = Transaction::change(&text, [(0, 0, Some("😀".into()))].into_iter());
        update_color_ranges(&mut colors, insertion.changes());
        assert_eq!(
            prior_ranges
                .iter()
                .map(|(_, range)| range.clone())
                .collect::<Vec<_>>(),
            [1..5, 5..9, 9..13]
        );
        assert_eq!(
            colors
                .color_ranges
                .iter()
                .map(|(_, range)| range.clone())
                .collect::<Vec<_>>(),
            vec![2..6, 6..10, 10..14]
        );

        let mut updated_text = text.clone();
        insertion.apply(&mut updated_text);
        // Insertion at a shared boundary can change either color token.
        let boundary = Transaction::change(&updated_text, [(6, 6, Some("a".into()))].into_iter());
        update_color_ranges(&mut colors, boundary.changes());
        assert_eq!(colors.color_ranges[0].1, 11..15);
        assert_eq!(colors.color_ranges.len(), 1);

        boundary.apply(&mut updated_text);
        let replacement =
            Transaction::change(&updated_text, [(12, 13, Some("b".into()))].into_iter());
        update_color_ranges(&mut colors, replacement.changes());
        assert!(colors.color_ranges.is_empty());
    }

    #[tokio::test]
    async fn stale_color_callback_cannot_overwrite_newer_response() {
        let mut doc = document();
        let path = doc.path().unwrap().to_owned();
        let version = doc.version();
        let stale = doc.color_swatch_controller.restart();
        let current = doc.color_swatch_controller.restart();
        attach_document_colors(
            &mut doc,
            version,
            &path,
            &current,
            vec![color(0..4, blue(), 0)],
        );
        attach_document_colors(
            &mut doc,
            version,
            &path,
            &stale,
            vec![color(0..4, red(), 0)],
        );
        assert_eq!(
            doc.color_swatches.as_ref().unwrap().colors[0],
            Theme::rgb_highlight(0, 0, 255)
        );
    }

    #[tokio::test]
    async fn stale_color_callback_is_rejected_after_edit_or_path_change() {
        let mut doc = document();
        let path = doc.path().unwrap().to_owned();
        let version = doc.version();
        let stale = doc.color_swatch_controller.restart();
        let edit = Transaction::change(doc.text(), [(0, 0, Some(" ".into()))].into_iter());
        doc.apply(&edit, ViewId::default());
        attach_document_colors(
            &mut doc,
            version,
            &path,
            &stale,
            vec![color(0..4, red(), 0)],
        );
        assert!(doc.color_swatches.is_none());

        let version = doc.version();
        let stale = doc.color_swatch_controller.restart();
        doc.set_path(Some(Path::new("/tmp/helix-other-color-test.css")));
        doc.set_path(Some(&path));
        attach_document_colors(
            &mut doc,
            version,
            &path,
            &stale,
            vec![color(1..5, red(), 0)],
        );
        assert!(doc.color_swatches.is_none());
    }

    #[tokio::test]
    async fn changing_language_clears_colors_and_rejects_queued_response() {
        let mut doc = document();
        let path = doc.path().unwrap().to_owned();
        let version = doc.version();
        let stale = doc.color_swatch_controller.restart();
        doc.color_swatches = color_annotations(vec![color(0..4, red(), 0)]);
        doc.set_language(None, &syntax::Loader::default());
        assert!(doc.color_swatches.is_none());
        attach_document_colors(
            &mut doc,
            version,
            &path,
            &stale,
            vec![color(0..4, red(), 0)],
        );
        assert!(doc.color_swatches.is_none());
    }
}
