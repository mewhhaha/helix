use std::{collections::HashSet, ops::Range, time::Duration};

use futures_util::{stream::FuturesOrdered, StreamExt};
use helix_core::{
    syntax::{config::LanguageServerFeature, OverlayHighlights},
    text_annotations::InlineAnnotation,
    Assoc,
};
use helix_event::{cancelable_future, register_hook};
use helix_lsp::lsp;
use helix_view::{
    document::DocumentColors,
    editor::DocumentColorMode,
    events::{DocumentDidChange, DocumentDidOpen, LanguageServerExited, LanguageServerInitialized},
    handlers::{lsp::DocumentColorsEvent, Handlers},
    DocumentId, Editor, Theme,
};
use tokio::time::Instant;

use crate::job;

#[derive(Default)]
pub(super) struct DocumentColorsHandler {
    docs: HashSet<DocumentId>,
}

const DOCUMENT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
struct DocumentColorSpan {
    anchor: usize,
    range: Range<usize>,
    color: lsp::Color,
}

impl helix_event::AsyncHook for DocumentColorsHandler {
    type Event = DocumentColorsEvent;

    fn handle_event(&mut self, event: Self::Event, _timeout: Option<Instant>) -> Option<Instant> {
        let DocumentColorsEvent(doc_id) = event;
        self.docs.insert(doc_id);
        Some(Instant::now() + DOCUMENT_CHANGE_DEBOUNCE)
    }

    fn finish_debounce(&mut self) {
        let docs = std::mem::take(&mut self.docs);

        job::dispatch_blocking(move |editor, _compositor| {
            for doc in docs {
                request_document_colors(editor, doc);
            }
        });
    }
}

fn request_document_colors(editor: &mut Editor, doc_id: DocumentId) {
    if !editor.config().lsp.display_color_swatches {
        return;
    }

    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };

    let cancel = doc.color_swatch_controller.restart();

    let mut seen_language_servers = HashSet::new();
    let mut futures: FuturesOrdered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::DocumentColors)
        .filter(|ls| seen_language_servers.insert(ls.id()))
        .map(|language_server| {
            let text = doc.text().clone();
            let offset_encoding = language_server.offset_encoding();
            let future = language_server
                .text_document_document_color(doc.identifier(), None)
                .unwrap();

            async move {
                let colors: Vec<_> = future
                    .await?
                    .into_iter()
                    .filter_map(|color_info| {
                        let range = helix_lsp::util::lsp_range_to_range(
                            &text,
                            color_info.range,
                            offset_encoding,
                        )?;
                        let range = range.from()..range.to();
                        Some(DocumentColorSpan {
                            anchor: range.start,
                            range,
                            color: color_info.color,
                        })
                    })
                    .collect();
                anyhow::Ok(colors)
            }
        })
        .collect();

    if futures.is_empty() {
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
        job::dispatch(move |editor, _| attach_document_colors(editor, doc_id, all_colors)).await;
    });
}

fn attach_document_colors(
    editor: &mut Editor,
    doc_id: DocumentId,
    doc_colors: Vec<DocumentColorSpan>,
) {
    if !editor.config().lsp.display_color_swatches {
        return;
    }

    let document_color_mode = editor.config().lsp.document_color_mode;

    let Some(doc) = editor.documents.get_mut(&doc_id) else {
        return;
    };

    doc.document_colors = build_document_colors(doc_colors, document_color_mode);
}

pub(super) fn register_hooks(handlers: &Handlers) {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        // when a document is initially opened, request colors for it
        request_document_colors(event.editor, event.doc);

        Ok(())
    });

    let tx = handlers.document_colors.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        let update_inline_annotations = |annotations: &mut Vec<InlineAnnotation>| {
            event.changes.update_positions(
                annotations
                    .iter_mut()
                    .map(|annotation| (&mut annotation.char_idx, Assoc::After)),
            );
        };

        let update_overlay_highlights = |overlay_highlights: &mut Vec<OverlayHighlights>| {
            for highlights in overlay_highlights.iter_mut() {
                match highlights {
                    OverlayHighlights::Homogeneous { ranges, .. } => {
                        event.changes.update_positions(
                            ranges
                                .iter_mut()
                                .map(|range| (&mut range.start, Assoc::After)),
                        );
                        event.changes.update_positions(
                            ranges
                                .iter_mut()
                                .map(|range| (&mut range.end, Assoc::Before)),
                        );
                        ranges.retain(|range| range.start < range.end);
                    }
                    OverlayHighlights::Heterogenous { highlights } => {
                        event.changes.update_positions(
                            highlights
                                .iter_mut()
                                .map(|(_, range)| (&mut range.start, Assoc::After)),
                        );
                        event.changes.update_positions(
                            highlights
                                .iter_mut()
                                .map(|(_, range)| (&mut range.end, Assoc::Before)),
                        );
                        highlights.retain(|(_, range)| range.start < range.end);
                    }
                }
            }
            overlay_highlights.retain(|highlights| !highlights.is_empty());
        };

        if let Some(DocumentColors {
            inline_annotations,
            inline_annotation_highlights: _,
            inline_padding_before,
            overlay_highlights,
        }) = &mut event.doc.document_colors
        {
            update_inline_annotations(inline_annotations);
            update_inline_annotations(inline_padding_before);
            update_overlay_highlights(overlay_highlights);
        }

        // Avoid re-requesting document colors if the change is a ghost transaction (completion)
        // because the language server will not know about the updates to the document and will
        // give out-of-date locations.
        if !event.ghost_transaction {
            // Cancel the ongoing request, if present.
            event.doc.color_swatch_controller.cancel();
            helix_event::send_blocking(&tx, DocumentColorsEvent(event.doc.id()));
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
                doc.document_colors.take();
            }
        }

        let doc_ids: Vec<_> = event.editor.documents().map(|doc| doc.id()).collect();

        for doc_id in doc_ids {
            request_document_colors(event.editor, doc_id);
        }

        Ok(())
    });
}

fn build_document_colors(
    mut document_colors: Vec<DocumentColorSpan>,
    mode: DocumentColorMode,
) -> Option<DocumentColors> {
    if document_colors.is_empty() {
        return None;
    }

    document_colors.sort_by_key(|document_color| {
        (
            document_color.anchor,
            document_color.range.end,
            document_color_to_highlight(document_color.color, mode).get(),
        )
    });

    match mode {
        DocumentColorMode::Virtual => {
            let mut inline_annotations = Vec::with_capacity(document_colors.len());
            let mut inline_padding_before = Vec::with_capacity(document_colors.len());
            let mut inline_annotation_highlights = Vec::with_capacity(document_colors.len());

            for document_color in document_colors {
                inline_padding_before.push(InlineAnnotation::new(document_color.anchor, " "));
                inline_annotations.push(InlineAnnotation::new(document_color.anchor, "■"));
                inline_annotation_highlights
                    .push(document_color_to_highlight(document_color.color, mode));
            }

            Some(DocumentColors {
                inline_annotations,
                inline_annotation_highlights,
                inline_padding_before,
                overlay_highlights: Vec::new(),
            })
        }
        DocumentColorMode::Foreground | DocumentColorMode::Background => {
            let mut highlights: Vec<_> = document_colors
                .into_iter()
                .filter(|document_color| document_color.range.start < document_color.range.end)
                .map(|document_color| {
                    (
                        document_color_to_highlight(document_color.color, mode),
                        document_color.range,
                    )
                })
                .collect();

            if highlights.is_empty() {
                return None;
            }

            highlights.sort_by_key(|(highlight, range)| (range.start, range.end, highlight.get()));
            highlights.dedup_by(
                |(left_highlight, left_range), (right_highlight, right_range)| {
                    left_highlight == right_highlight && left_range == right_range
                },
            );

            Some(DocumentColors {
                inline_annotations: Vec::new(),
                inline_annotation_highlights: Vec::new(),
                inline_padding_before: Vec::new(),
                overlay_highlights: pack_overlay_highlights(highlights),
            })
        }
    }
}

fn pack_overlay_highlights(
    highlights: Vec<(helix_core::syntax::Highlight, Range<usize>)>,
) -> Vec<OverlayHighlights> {
    let mut layers: Vec<(Vec<(helix_core::syntax::Highlight, Range<usize>)>, usize)> = Vec::new();

    for (highlight, range) in highlights {
        if let Some((layer, last_end)) = layers
            .iter_mut()
            .find(|(_layer, last_end)| range.start >= *last_end)
        {
            *last_end = range.end;
            layer.push((highlight, range));
        } else {
            layers.push((vec![(highlight, range.clone())], range.end));
        }
    }

    layers
        .into_iter()
        .map(|(highlights, _)| OverlayHighlights::Heterogenous { highlights })
        .collect()
}

fn document_color_to_highlight(
    color: lsp::Color,
    mode: DocumentColorMode,
) -> helix_core::syntax::Highlight {
    let [red, green, blue] = [color.red, color.green, color.blue]
        .map(|channel| (channel.clamp(0., 1.) * 255.).round() as u8);

    match mode {
        DocumentColorMode::Background => Theme::rgb_background_highlight(red, green, blue),
        DocumentColorMode::Foreground | DocumentColorMode::Virtual => {
            Theme::rgb_highlight(red, green, blue)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::graphics::{Color, Style};
    use helix_view::theme::Theme;

    fn color(red: f32, green: f32, blue: f32) -> lsp::Color {
        lsp::Color {
            red,
            green,
            blue,
            alpha: 1.0,
        }
    }

    fn span(anchor: usize, range: Range<usize>, color: lsp::Color) -> DocumentColorSpan {
        DocumentColorSpan {
            anchor,
            range,
            color,
        }
    }

    #[test]
    fn virtual_mode_builds_inline_annotations() {
        let document_colors = build_document_colors(
            vec![span(4, 4..11, color(0.1, 0.2, 0.3))],
            DocumentColorMode::Virtual,
        )
        .expect("expected document colors");

        assert_eq!(document_colors.inline_padding_before.len(), 1);
        assert_eq!(document_colors.inline_padding_before[0].char_idx, 4);
        assert_eq!(&*document_colors.inline_padding_before[0].text, " ");
        assert_eq!(document_colors.inline_annotations.len(), 1);
        assert_eq!(document_colors.inline_annotations[0].char_idx, 4);
        assert_eq!(&*document_colors.inline_annotations[0].text, "■");
        assert_eq!(document_colors.overlay_highlights.len(), 0);
        assert_eq!(
            Theme::default().highlight(document_colors.inline_annotation_highlights[0]),
            Style::default().fg(Color::Rgb(26, 51, 77))
        );
    }

    #[test]
    fn foreground_mode_builds_overlay_highlights() {
        let document_colors = build_document_colors(
            vec![span(0, 1..8, color(0.25, 0.5, 0.75))],
            DocumentColorMode::Foreground,
        )
        .expect("expected document colors");

        assert!(document_colors.inline_annotations.is_empty());
        assert_eq!(document_colors.overlay_highlights.len(), 1);
        let OverlayHighlights::Heterogenous { highlights } = &document_colors.overlay_highlights[0]
        else {
            panic!("expected heterogenous highlights");
        };
        assert_eq!(highlights.len(), 1);
        assert_eq!(highlights[0].1, 1..8);
        assert_eq!(
            Theme::default().highlight(highlights[0].0),
            Style::default().fg(Color::Rgb(64, 128, 191))
        );
    }

    #[test]
    fn background_mode_builds_overlay_highlights() {
        let document_colors = build_document_colors(
            vec![span(0, 1..8, color(0.95, 0.9, 0.2))],
            DocumentColorMode::Background,
        )
        .expect("expected document colors");

        let OverlayHighlights::Heterogenous { highlights } = &document_colors.overlay_highlights[0]
        else {
            panic!("expected heterogenous highlights");
        };
        assert_eq!(
            Theme::default().highlight(highlights[0].0),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(242, 230, 51))
        );
    }

    #[test]
    fn overlapping_ranges_are_split_into_separate_layers() {
        let document_colors = build_document_colors(
            vec![
                span(0, 0..7, color(1.0, 0.0, 0.0)),
                span(0, 0..7, color(0.0, 1.0, 0.0)),
            ],
            DocumentColorMode::Foreground,
        )
        .expect("expected document colors");

        assert_eq!(document_colors.overlay_highlights.len(), 2);
    }
}
