use helix_core::Rope;
use tokio::task::JoinHandle;

use crate::diff::{DiffHandle, Hunk};

use super::{spawn_diff_notification, RenderLock};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;

#[tokio::test]
async fn async_redraw_observes_completion_before_task_starts() {
    let notify = Arc::new(Notify::new());
    let task = spawn_diff_notification(None, notify.clone());
    notify.notify_waiters();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn blocking_render_lock_observes_completion_before_task_starts() {
    let lock = Box::leak(Box::new(parking_lot::RwLock::new(())));
    let notify = Arc::new(Notify::new());
    let task = spawn_diff_notification(
        Some(RenderLock {
            lock: lock.read(),
            timeout: None,
        }),
        notify.clone(),
    );
    notify.notify_waiters();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert!(lock.try_write().is_some());
}

#[tokio::test]
async fn timed_out_render_lock_still_waits_for_diff_completion() {
    let lock = Box::leak(Box::new(parking_lot::RwLock::new(())));
    let notify = Arc::new(Notify::new());
    let task = spawn_diff_notification(
        Some(RenderLock {
            lock: lock.read(),
            timeout: Some(tokio::time::Instant::now()),
        }),
        notify.clone(),
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while lock.try_write().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!task.is_finished());
    notify.notify_waiters();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn oversized_document_clears_existing_hunks() {
    let (differ, handle) = DiffHandle::new_test("base\n", "doc\n");
    differ.update_document(
        Rope::from_str(&"\n".repeat(crate::diff::MAX_DIFF_LINES)),
        false,
    );
    assert!(differ.into_diff(handle).await.is_empty());
}

#[tokio::test]
async fn initially_oversized_document_resumes_diffing_when_small() {
    let (differ, handle) =
        DiffHandle::new_test("base\n", &"\n".repeat(crate::diff::MAX_DIFF_LINES));
    differ.update_document(Rope::from_str("doc\n"), false);
    assert_eq!(
        differ.into_diff(handle).await,
        vec![Hunk {
            before: 0..1,
            after: 0..1
        }]
    );
}

impl DiffHandle {
    fn new_test(diff_base: &str, doc: &str) -> (DiffHandle, JoinHandle<()>) {
        DiffHandle::new_with_handle(Rope::from_str(diff_base), Rope::from_str(doc))
    }
    async fn into_diff(self, handle: JoinHandle<()>) -> Vec<Hunk> {
        let diff = self.diff;
        // dropping the channel terminates the task
        drop(self.channel);
        handle.await.unwrap();
        let diff = diff.read();
        Vec::clone(&diff.hunks)
    }
}

#[tokio::test]
async fn append_line() {
    let (differ, handle) = DiffHandle::new_test("foo\n", "foo\nbar\n");
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[Hunk {
            before: 1..1,
            after: 1..2
        }]
    )
}

#[tokio::test]
async fn prepend_line() {
    let (differ, handle) = DiffHandle::new_test("foo\n", "bar\nfoo\n");
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[Hunk {
            before: 0..0,
            after: 0..1
        }]
    )
}

#[tokio::test]
async fn modify() {
    let (differ, handle) = DiffHandle::new_test("foo\nbar\n", "foo bar\nbar\n");
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[Hunk {
            before: 0..1,
            after: 0..1
        }]
    )
}

#[tokio::test]
async fn delete_line() {
    let (differ, handle) = DiffHandle::new_test("foo\nfoo bar\nbar\n", "foo\nbar\n");
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[Hunk {
            before: 1..2,
            after: 1..1
        }]
    )
}

#[tokio::test]
async fn delete_line_and_modify() {
    let (differ, handle) = DiffHandle::new_test("foo\nbar\ntest\nfoo", "foo\ntest\nfoo bar");
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[
            Hunk {
                before: 1..2,
                after: 1..1
            },
            Hunk {
                before: 3..4,
                after: 2..3
            },
        ]
    )
}

#[tokio::test]
async fn add_use() {
    let (differ, handle) = DiffHandle::new_test(
        "use ropey::Rope;\nuse tokio::task::JoinHandle;\n",
        "use ropey::Rope;\nuse ropey::RopeSlice;\nuse tokio::task::JoinHandle;\n",
    );
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[Hunk {
            before: 1..1,
            after: 1..2
        },]
    )
}

#[tokio::test]
async fn update_document() {
    let (differ, handle) = DiffHandle::new_test("foo\nbar\ntest\nfoo", "foo\nbar\ntest\nfoo");
    differ.update_document(Rope::from_str("foo\ntest\nfoo bar"), false);
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[
            Hunk {
                before: 1..2,
                after: 1..1
            },
            Hunk {
                before: 3..4,
                after: 2..3
            },
        ]
    )
}

#[tokio::test]
async fn update_base() {
    let (differ, handle) = DiffHandle::new_test("foo\ntest\nfoo bar", "foo\ntest\nfoo bar");
    differ.update_diff_base(Rope::from_str("foo\nbar\ntest\nfoo"));
    let line_diffs = differ.into_diff(handle).await;
    assert_eq!(
        &line_diffs,
        &[
            Hunk {
                before: 1..2,
                after: 1..1
            },
            Hunk {
                before: 3..4,
                after: 2..3
            },
        ]
    )
}
