use std::{future::poll_fn, pin::Pin, task::Poll};

use futures_util::Stream;

/// Poll once without waiting, preserving the application's actual task waker.
/// `now_or_never` instead supplies a noop waker, which can prevent terminal
/// adapters that latch a pending poll's waker from waking on the next key.
pub(super) async fn next_ready<S: Stream + Unpin>(stream: &mut S) -> Option<S::Item> {
    poll_fn(|cx| {
        Poll::Ready(match Pin::new(&mut *stream).poll_next(cx) {
            Poll::Ready(item) => item,
            Poll::Pending => None,
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Mutex},
        task::{Context, Waker},
        time::Duration,
    };

    use futures_util::StreamExt;

    #[derive(Default)]
    struct State {
        item: Option<u8>,
        waker: Option<Waker>,
    }

    /// Like Termina's helper thread, keep the first pending poll's waker until
    /// new input arrives, rather than replacing it on every poll.
    struct LatchedWakeStream(Arc<Mutex<State>>);

    impl Stream for LatchedWakeStream {
        type Item = u8;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<u8>> {
            let mut state = self.0.lock().unwrap();
            if let Some(item) = state.item.take() {
                Poll::Ready(Some(item))
            } else {
                state.waker.get_or_insert_with(|| cx.waker().clone());
                Poll::Pending
            }
        }
    }

    #[tokio::test]
    async fn draining_ready_events_preserves_wakeup_for_later_input() {
        let state = Arc::new(Mutex::new(State {
            item: Some(1),
            ..State::default()
        }));
        let mut stream = LatchedWakeStream(state.clone());
        assert_eq!(next_ready(&mut stream).await, Some(1));
        assert_eq!(next_ready(&mut stream).await, None);

        let started = tokio::time::Instant::now();
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let waker = {
                let mut state = state.lock().unwrap();
                state.item = Some(2);
                state.waker.take().unwrap()
            };
            waker.wake();
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .unwrap(),
            Some(2)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        sender.await.unwrap();
    }
}
