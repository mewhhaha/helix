use super::*;
use crate::editor::Config;
use arc_swap::ArcSwap;
use helix_core::{
    diagnostic::{DiagnosticProvider, LanguageServerId, Range, Severity},
    doc_formatter::DocumentFormatter,
    syntax, Diagnostic, Rope,
};
use std::sync::Arc;

fn warning(start: usize, line: usize) -> Diagnostic {
    Diagnostic {
        range: Range {
            start,
            end: start + 1,
        },
        ends_at_word: false,
        starts_at_word: false,
        zero_width: false,
        line,
        message: "warning with wrapped diagnostic text".into(),
        severity: Some(Severity::Warning),
        code: None,
        provider: DiagnosticProvider::Lsp {
            server_id: LanguageServerId::default(),
            identifier: None,
        },
        tags: Vec::new(),
        source: None,
        data: None,
    }
}

#[tokio::test]
async fn offscreen_diagnostics_reuse_checkpoints_without_losing_pending_anchors() {
    let length = 32_768;
    let mut doc = Document::from(
        Rope::from_str(&format!("{}\nnext\n", "word ".repeat(length / 5))),
        None,
        Arc::new(ArcSwap::from_pointee(Config::default())),
        Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
    );
    let line_end = doc.text().line_to_char(1);
    let target = line_end - 100;
    let mut view = View::new(doc.id(), GutterConfig::default());
    view.area = Rect::new(0, 0, 120, 40);
    doc.ensure_view_init(view.id);
    doc.set_selection(view.id, Selection::point(target));
    for wrapped in [false, true] {
        doc.replace_diagnostics([warning(line_end, 1)], &[], None);
        view.diagnostics_handler
            .immediately_show_diagnostic(&doc, view.id);
        let mut cached = doc.text_format(120, None);
        cached.soft_wrap = wrapped;
        let mut plain = cached.clone();
        plain.checkpoint_cache = None;
        {
            let annotations = view.text_annotations(&doc, None);
            DocumentFormatter::new_at_prev_checkpoint(
                doc.text().slice(..),
                &cached,
                &annotations,
                0,
            )
            .for_each(drop);
        }
        let annotations = view.text_annotations(&doc, None);
        let resumed = DocumentFormatter::new_at_prev_checkpoint(
            doc.text().slice(..),
            &cached,
            &annotations,
            target,
        );
        assert!(
            resumed.next_char_pos() > target - 2048,
            "offscreen diagnostics should allow a nearby checkpoint"
        );
        drop(resumed);
        assert_eq!(
            visual_offset_from_block(doc.text().slice(..), target, target, &cached, &annotations),
            visual_offset_from_block(doc.text().slice(..), target, target, &plain, &annotations)
        );
        drop(annotations);

        doc.replace_diagnostics([warning(100, 0)], &[], None);
        let annotations = view.text_annotations(&doc, None);
        DocumentFormatter::new_at_prev_checkpoint(doc.text().slice(..), &cached, &annotations, 0)
            .for_each(drop);
        let resumed = DocumentFormatter::new_at_prev_checkpoint(
            doc.text().slice(..),
            &cached,
            &annotations,
            target,
        );
        if wrapped {
            assert!(
                resumed.next_char_pos() > target - 2048,
                "diagnostics consumed on earlier visual lines should permit checkpoints"
            );
        } else {
            assert!(
                resumed.next_char_pos() <= 100,
                "pending diagnostic anchors must still be traversed by the decoration"
            );
        }
        drop(resumed);
        assert_eq!(
            visual_offset_from_block(doc.text().slice(..), target, target, &cached, &annotations),
            visual_offset_from_block(doc.text().slice(..), target, target, &plain, &annotations)
        );
    }
}
