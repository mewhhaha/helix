use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_core::{syntax, Rope, Transaction};

use super::*;
use crate::{editor::Config, ViewId};

fn document(path: &Path, text: &str) -> Document {
    let mut doc = Document::from(
        Rope::from_str(text),
        None,
        Arc::new(ArcSwap::from_pointee(Config::default())),
        Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
    );
    doc.set_path(Some(path));
    doc.load_review_comments().unwrap();
    doc
}

#[test]
fn sidecar_round_trip_preserves_unicode_ranges_and_never_writes_source() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("example.rs");
    let original = "keep\nlet café = 🦀;\n";
    std::fs::write(&source, original).unwrap();
    let mut doc = document(&source, original);
    let id = doc
        .add_review_comment(CommentAnchor::new(
            CommentSide::Current,
            None,
            doc.text().slice(..),
            9..13,
        ))
        .unwrap();
    doc.set_review_comment_text(id, "Why café?\nA second line.".into());
    doc.save_review_comments().unwrap();
    assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
    let reopened = document(&source, original);
    assert_eq!(reopened.review_comments()[0].anchor.range, 9..13);
    assert_eq!(
        reopened.review_comments()[0].text,
        "Why café?\nA second line."
    );
    let shifted = Rope::from_str(&format!("new\n{original}"));
    assert_eq!(
        reopened.review_comments()[0]
            .anchor
            .locate(shifted.slice(..)),
        Some(13..17)
    );
    doc.remove_review_comment(id).unwrap();
    assert!(!sidecar_path(&source).exists());
}

#[test]
fn large_selections_keep_the_full_range_with_a_small_anchor_excerpt() {
    let text = Rope::from_str(&format!(
        "begin\n{}end\n",
        "long selected row\n".repeat(10_000)
    ));
    let anchor = CommentAnchor::new(
        CommentSide::Current,
        None,
        text.slice(..),
        0..text.len_chars(),
    );
    assert_eq!(anchor.locate(text.slice(..)), Some(0..text.len_chars()));
    assert!(anchor.quote.len() < 4096);
    let shifted = Rope::from_str(&format!("prologue\n{text}"));
    assert_eq!(
        anchor.locate(shifted.slice(..)),
        Some(9..9 + text.len_chars())
    );
}

#[test]
fn external_comment_updates_and_invalid_sidecars_are_not_overwritten() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let mut doc = document(&source, "old\n");
    let id = doc
        .add_review_comment(CommentAnchor::new(
            CommentSide::Current,
            None,
            doc.text().slice(..),
            0..3,
        ))
        .unwrap();
    doc.set_review_comment_text(id, "local".into());
    let path = sidecar_path(&source);
    let external = br#"{"version":1,"comments":[]}"#;
    std::fs::write(&path, external).unwrap();
    assert!(doc.save_review_comments().is_err());
    assert!(doc.remove_review_comment(id).is_err());
    assert_eq!(doc.review_comment(id).unwrap().text, "local");
    assert_eq!(std::fs::read(&path).unwrap(), external);
    std::fs::write(&path, b"invalid JSON").unwrap();
    let mut reopened = Document::from(
        Rope::from_str("old\n"),
        None,
        Arc::new(ArcSwap::from_pointee(Config::default())),
        Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
    );
    reopened.set_path(Some(&source));
    assert!(reopened.load_review_comments().is_err());
    assert!(reopened.save_review_comments().is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"invalid JSON");
}

#[tokio::test]
async fn source_edits_map_comment_ranges_without_changing_comment_text() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let mut doc = document(&source, "keep\nold\n");
    let id = doc
        .add_review_comment(CommentAnchor::new(
            CommentSide::Current,
            None,
            doc.text().slice(..),
            5..8,
        ))
        .unwrap();
    doc.set_review_comment_text(id, "note".into());
    doc.save_review_comments().unwrap();
    let previous = doc.text().clone();
    let sidecar = sidecar_path(&source);
    let saved = std::fs::read(&sidecar).unwrap();
    let view = ViewId::default();
    doc.ensure_view_init(view);
    let change = Transaction::change(doc.text(), [(0, 0, Some("new\n".into()))].into_iter());
    assert!(doc.apply(&change, view));
    assert_eq!(doc.review_comment(id).unwrap().anchor.range, 9..12);
    assert_eq!(doc.review_comment(id).unwrap().anchor.quote, "old");
    assert_eq!(doc.review_comment(id).unwrap().text, "note");
    doc.save_review_comments_after_source_write(&previous, &source)
        .unwrap();
    assert_eq!(std::fs::read(&sidecar).unwrap(), saved);
    let current = doc.text().clone();
    doc.set_view_diff_mode(view, true);
    doc.save_review_comments_after_source_write(&current, &source)
        .unwrap();
    assert_eq!(std::fs::read(&sidecar).unwrap(), saved);
    doc.set_view_diff_mode(view, false);
    doc.save_review_comments_after_source_write(&current, &source)
        .unwrap();
    let reopened = document(&source, "new\nkeep\nold\n");
    assert_eq!(reopened.review_comment(id).unwrap().anchor.range, 9..12);
    let thread = reopened.review_comment_thread(id).unwrap();
    assert_eq!(thread.original_anchor.range, 5..8);
    assert_ne!(
        thread.snapshot.content_hash,
        content_hash(reopened.text().slice(..))
    );
}

#[test]
fn concurrent_agent_transactions_preserve_all_messages_and_ids() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.rs");
    std::fs::write(&source, "source\n").unwrap();
    let review = ReviewSession::new(
        "main".into(),
        Some("feature".into()),
        None,
        ReviewSnapshot::default(),
    );
    let review_id = review.id.clone();
    ReviewStore::transaction(&source, |store| {
        store.select(review);
        Ok(())
    })
    .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    std::thread::scope(|scope| {
        for agent in 0..8 {
            let barrier = barrier.clone();
            let source = &source;
            let review_id = &review_id;
            scope.spawn(move || {
                barrier.wait();
                ReviewStore::transaction(source, |store| {
                    store.add(
                        review_id,
                        CommentAnchor::new(
                            CommentSide::Current,
                            None,
                            Rope::from_str("source\n").slice(..),
                            0..6,
                        ),
                        format!("agent-{agent}"),
                        "finding".into(),
                    )?;
                    Ok(())
                })
                .unwrap();
            });
        }
    });
    let mut store = ReviewStore::default();
    store.load(&source).unwrap();
    assert_eq!(store.data().threads.len(), 8);
    let ids: std::collections::HashSet<_> = store
        .data()
        .threads
        .iter()
        .map(|thread| thread.id)
        .collect();
    assert_eq!(ids.len(), 8);
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "source\n");
}

#[test]
fn migration_keeps_unknown_origins_and_cancel_restores_the_saved_session() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    std::fs::write(&source, "source\n").unwrap();
    let old = serde_json::json!({ "version": 1, "comments": [{ "id": 9, "anchor": { "side": "current", "range": { "start": 0, "end": 6 }, "quote": "source", "prefix": "", "suffix": "\n" }, "text": "existing" }] });
    std::fs::write(sidecar_path(&source), serde_json::to_vec(&old).unwrap()).unwrap();
    let mut doc = document(&source, "source\n");
    let thread = doc.review_comment_thread(9).unwrap();
    assert_eq!(thread.messages[0].author, "unknown");
    assert_eq!(thread.snapshot.head_commit, None);
    let original = thread.original_anchor.clone();
    doc.set_review_comment_text(9, "edited".into());
    doc.save_review_comments().unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(sidecar_path(&source)).unwrap()).unwrap();
    assert_eq!(saved["version"], 2);
    assert_eq!(
        doc.review_comment_thread(9).unwrap().original_anchor,
        original
    );
    doc.set_review_comment_text(9, "draft".into());
    doc.cancel_review_comment_edit(9, Some("edited".into()), false);
    assert_eq!(doc.review_comment(9).unwrap().text, "edited");
    assert!(!doc.review_comments_dirty());

    let source = directory.path().join("empty");
    std::fs::write(&source, "source\n").unwrap();
    let review = ReviewSession::new("HEAD".into(), None, None, ReviewSnapshot::default());
    let review_id = review.id.clone();
    ReviewStore::transaction(&source, |store| {
        store.select(review);
        Ok(())
    })
    .unwrap();
    let mut doc = document(&source, "source\n");
    let id = doc
        .add_review_comment(CommentAnchor::new(
            CommentSide::Current,
            None,
            doc.text().slice(..),
            0..6,
        ))
        .unwrap();
    doc.cancel_review_comment_edit(id, None, false);
    assert_eq!(doc.review_session().unwrap().id, review_id);
    assert!(doc.review_comments().is_empty());
}

#[test]
fn changed_or_ambiguous_code_does_not_acquire_an_unrelated_comment() {
    let text = Rope::from_str("prefix\nselected\nsuffix\n");
    let mut anchor = CommentAnchor::new(CommentSide::Current, None, text.slice(..), 7..15);
    assert_eq!(
        anchor.locate(Rope::from_str("new\nprefix\nselected\nsuffix\n").slice(..)),
        Some(11..19)
    );
    assert!(anchor
        .locate(Rope::from_str("prefix\nmodified\nsuffix\n").slice(..))
        .is_none());
    anchor.range = 100..108;
    let duplicated = Rope::from_str("prefix\nselected\nsuffix\nprefix\nselected\nsuffix\n");
    assert!(anchor.locate(duplicated.slice(..)).is_none());

    let original = Rope::from_str("first()\nreturn None\nfinish()\n");
    let anchor = CommentAnchor::new(CommentSide::Current, None, original.slice(..), 8..19);
    let different_function = Rope::from_str("other()\nreturn None\notherend()\n");
    assert!(anchor.locate(different_function.slice(..)).is_none());
}
