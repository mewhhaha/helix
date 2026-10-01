use std::future::Future;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

/// Run CPU work on the shared, bounded pool without occupying a Tokio worker.
/// Panics propagate to the awaiting task, as with a spawned Tokio task. Dropping
/// the future does not stop running work; closures should check cancellation.
pub fn spawn_cpu<F, T>(work: F) -> impl Future<Output = T> + Send
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (send, receive) = tokio::sync::oneshot::channel();
    helix_stdx::cpu::pool().spawn(move || {
        if send.is_closed() {
            return;
        }
        let result = catch_unwind(AssertUnwindSafe(work));
        let _ = send.send(result);
    });
    async move {
        match receive.await.expect("CPU worker dropped its result") {
            Ok(result) => result,
            Err(panic) => resume_unwind(panic),
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn a_panicking_job_does_not_kill_the_pool() {
        let job = tokio::spawn(super::spawn_cpu(|| panic!("test panic")));
        assert!(job.await.unwrap_err().is_panic());
        assert_eq!(super::spawn_cpu(|| 42).await, 42);
    }
}
