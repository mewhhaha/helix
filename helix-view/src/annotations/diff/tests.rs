use super::*;
use crate::{editor::Config, graphics::Rect, View};
use arc_swap::ArcSwap;
use helix_core::{
    char_idx_at_visual_offset, doc_formatter::DocumentFormatter, syntax, visual_offset_from_anchor,
    Selection, Transaction,
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
    let mut ids = slotmap::SlotMap::<ViewId, ()>::with_key();
    view.id = ids.insert(());
    doc.ensure_view_init(view.id);
    doc.set_selection(view.id, Selection::point(0));
    doc.set_diff_base(base.as_bytes().to_vec());
    wait_diff(&doc).await;
    view.set_diff_mode(&mut doc, true);
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
async fn named_review_base_invalidates_caches_and_tracks_edits() {
    let (mut doc, mut view) = fixture("keep\nold\nsame\n", "keep\nnew\nsame\n").await;
    let old = view.diff_mode.display(&doc).unwrap();
    let before = old.deletions[0].before.clone();
    view.diff_mode
        .set_cursor(&doc, view.id, before.clone(), 0, 0);
    doc.set_review_diff_base("main".into(), Rope::from_str("keep\nmain\nsame\n"));
    async fn wait_review(doc: &Document) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let ready = {
                    let diff = doc.review_diff_handle().unwrap().load();
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
    wait_review(&doc).await;
    let review = view.diff_mode.display(&doc).unwrap();
    assert_eq!(review.deleted_text(&before).to_string(), "main\n");
    assert!(!Arc::ptr_eq(&old, &review));
    assert!(view.diff_mode.cursor(&doc, view.id).is_none());
    assert_eq!(
        doc.diff_handle().unwrap().load().diff_base().to_string(),
        "keep\nold\nsame\n"
    );
    let change = Transaction::change(doc.text(), [(5, 8, Some("unsaved".into()))].into_iter());
    view.set_diff_mode(&mut doc, false);
    assert!(doc.apply(&change, view.id));
    view.set_diff_mode(&mut doc, true);
    wait_review(&doc).await;
    assert_eq!(
        doc.review_diff_handle().unwrap().load().doc().to_string(),
        "keep\nunsaved\nsame\n"
    );
    assert_eq!(
        view.diff_mode.display(&doc).unwrap().base.to_string(),
        "keep\nmain\nsame\n"
    );
    view.set_diff_mode(&mut doc, false);
    assert!(view.diff_mode.display(&doc).is_none());
    assert_eq!(doc.review_diff_reference(), Some("main"));
    view.set_diff_mode(&mut doc, true);
    assert_eq!(
        view.diff_mode.display(&doc).unwrap().base.to_string(),
        "keep\nmain\nsame\n"
    );
    doc.set_path(Some(std::path::Path::new("renamed.txt")));
    assert!(doc.review_diff_reference().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn review_selection_invalidates_when_the_base_changes_without_a_source_edit() {
    let (mut doc, mut view) = fixture("keep\na long deleted line\nsame\n", "keep\nsame\n").await;
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
    let (mut doc, mut view) = fixture("one\ntwo\nthree\nfour\nsame\nlast\n", "same\nlast\n").await;
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
        assert!(!doc.apply(&edit, view.id));
        view.set_diff_mode(&mut doc, false);
        assert!(doc.apply(&edit, view.id));
        view.set_diff_mode(&mut doc, true);
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
    let (mut doc, mut view) = fixture("same\nold1\nold2\nold3\nlast\n", "same\nnew\nlast\n").await;
    let mut other = view.clone();
    let mut ids = slotmap::SlotMap::<ViewId, ()>::with_key();
    ids.insert(());
    other.id = ids.insert(());
    doc.ensure_view_init(other.id);
    other.set_diff_mode(&mut doc, false);
    assert_eq!(
        view.diff_mode.display(&doc).unwrap().deletions[0].height(),
        3
    );
    assert!(other.diff_mode.display(&doc).is_none());
    let edit = Transaction::change(doc.text(), [(0, 0, Some("added\n".into()))].into_iter());
    view.set_diff_mode(&mut doc, false);
    assert!(doc.apply(&edit, view.id));
    view.set_diff_mode(&mut doc, true);
    // The old snapshot is inapplicable until the worker publishes this edit.
    assert!(view.diff_mode.display(&doc).is_none());
    wait_diff(&doc).await;
    assert_eq!(
        view.diff_mode.display(&doc).unwrap().deletions[0].height(),
        3
    );
    assert!(other.diff_mode.display(&doc).is_none());
    other.set_diff_mode(&mut doc, true);
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
        DocumentFormatter::new_at_prev_checkpoint(doc.text().slice(..), &cached, &annotations, 0)
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
