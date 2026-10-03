use helix_event::status::StatusMessage;
use helix_event::{runtime_local, send_blocking};
use helix_view::Editor;
use once_cell::sync::OnceCell;

use crate::compositor::Compositor;

use futures_util::future::{BoxFuture, Future, FutureExt};
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::mpsc::{channel, Receiver, Sender};

const MAX_CALLBACKS_PER_BATCH: usize = 32;
const CALLBACK_BATCH_BUDGET: std::time::Duration = std::time::Duration::from_millis(2);

pub type EditorCompositorCallback = Box<dyn FnOnce(&mut Editor, &mut Compositor) + Send>;
pub type EditorCallback = Box<dyn FnOnce(&mut Editor) + Send>;
pub type EditorCallbackFollowup = Box<dyn FnOnce(&mut Editor) -> Option<Job> + Send>;

runtime_local! {
    static JOB_QUEUE: OnceCell<Sender<Callback>> = OnceCell::new();
}

pub async fn dispatch_callback(job: Callback) {
    let _ = JOB_QUEUE.wait().send(job).await;
}

pub async fn dispatch(job: impl FnOnce(&mut Editor, &mut Compositor) + Send + 'static) {
    let _ = JOB_QUEUE
        .wait()
        .send(Callback::EditorCompositor(Box::new(job)))
        .await;
}

pub fn dispatch_blocking(job: impl FnOnce(&mut Editor, &mut Compositor) + Send + 'static) {
    let jobs = JOB_QUEUE.wait();
    send_blocking(jobs, Callback::EditorCompositor(Box::new(job)))
}

pub enum Callback {
    EditorCompositor(EditorCompositorCallback),
    Editor(EditorCallback),
    Followup(EditorCallbackFollowup),
}

pub type JobFuture = BoxFuture<'static, anyhow::Result<Option<Callback>>>;

pub struct Job {
    pub future: BoxFuture<'static, anyhow::Result<Option<Callback>>>,
    /// Do we need to wait for this job to finish before exiting?
    pub wait: bool,
}

pub struct Jobs {
    /// jobs that need to complete before we exit.
    pub wait_futures: FuturesUnordered<JobFuture>,
    pub callbacks: Receiver<Callback>,
    pub status_messages: Receiver<StatusMessage>,
    pub(crate) poll_wait_futures_first: bool,
}

impl Job {
    pub fn new<F: Future<Output = anyhow::Result<()>> + Send + 'static>(f: F) -> Self {
        Self {
            future: f.map(|r| r.map(|()| None)).boxed(),
            wait: false,
        }
    }

    pub fn with_callback<F: Future<Output = anyhow::Result<Callback>> + Send + 'static>(
        f: F,
    ) -> Self {
        Self {
            future: f.map(|r| r.map(Some)).boxed(),
            wait: false,
        }
    }

    pub fn wait_before_exiting(mut self) -> Self {
        self.wait = true;
        self
    }
}

impl Jobs {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let (tx, rx) = channel(1024);
        let _ = JOB_QUEUE.set(tx);
        let status_messages = helix_event::status::setup();
        Self {
            wait_futures: FuturesUnordered::new(),
            callbacks: rx,
            status_messages,
            poll_wait_futures_first: true,
        }
    }

    pub fn spawn<F: Future<Output = anyhow::Result<()>> + Send + 'static>(&mut self, f: F) {
        self.add(Job::new(f));
    }

    pub fn callback<F: Future<Output = anyhow::Result<Callback>> + Send + 'static>(
        &mut self,
        f: F,
    ) {
        self.add(Job::with_callback(f));
    }

    pub fn handle_callback(
        &self,
        editor: &mut Editor,
        compositor: &mut Compositor,
        call: anyhow::Result<Option<Callback>>,
    ) -> Option<Job> {
        match call {
            Ok(None) => None,
            Ok(Some(call)) => match call {
                Callback::EditorCompositor(call) => {
                    call(editor, compositor);
                    None
                }
                Callback::Editor(call) => {
                    call(editor);
                    None
                }
                Callback::Followup(call) => call(editor),
            },
            Err(e) => {
                editor.set_error(format!("Async job failed: {}", e));
                None
            }
        }
    }

    /// Apply a bounded burst before the application paints its next frame.
    /// The count and time limits give input a chance to run between bursts.
    pub fn handle_callback_batch(
        &mut self,
        editor: &mut Editor,
        compositor: &mut Compositor,
        first: anyhow::Result<Option<Callback>>,
    ) {
        let started = std::time::Instant::now();
        if let Some(job) = self.handle_callback(editor, compositor, first) {
            self.add(job);
        }
        for _ in 1..MAX_CALLBACKS_PER_BATCH {
            if started.elapsed() >= CALLBACK_BATCH_BUDGET {
                break;
            }
            let Some(callback) = self.next_ready_callback() else {
                break;
            };
            if let Some(job) = self.handle_callback(editor, compositor, callback) {
                self.add(job);
            }
        }
    }

    /// Share source priority between the first callback and later callbacks in
    /// a batch, so a callback that uses the whole budget cannot starve wait jobs.
    pub async fn next_callback(
        callbacks: &mut Receiver<Callback>,
        wait_futures: &mut FuturesUnordered<JobFuture>,
        poll_wait_futures_first: &mut bool,
    ) -> Option<anyhow::Result<Option<Callback>>> {
        let callback = if *poll_wait_futures_first {
            tokio::select! {
                biased;
                Some(callback) = wait_futures.next() => Some(callback),
                Some(callback) = callbacks.recv() => Some(Ok(Some(callback))),
                else => None,
            }
        } else {
            tokio::select! {
                biased;
                Some(callback) = callbacks.recv() => Some(Ok(Some(callback))),
                Some(callback) = wait_futures.next() => Some(callback),
                else => None,
            }
        };
        if callback.is_some() {
            *poll_wait_futures_first = !*poll_wait_futures_first;
        }
        callback
    }

    fn next_ready_callback(&mut self) -> Option<anyhow::Result<Option<Callback>>> {
        Self::next_callback(
            &mut self.callbacks,
            &mut self.wait_futures,
            &mut self.poll_wait_futures_first,
        )
        .now_or_never()
        .flatten()
    }

    pub fn add(&self, j: Job) {
        if j.wait {
            self.wait_futures.push(j.future);
        } else {
            tokio::spawn(async move {
                match j.future.await {
                    Ok(Some(cb)) => dispatch_callback(cb).await,
                    Ok(None) => (),
                    Err(err) => helix_event::status::report(err).await,
                }
            });
        }
    }

    /// Blocks until all the jobs that need to be waited on are done.
    pub async fn finish(
        &mut self,
        editor: &mut Editor,
        mut compositor: Option<&mut Compositor>,
    ) -> anyhow::Result<()> {
        log::debug!("waiting on jobs...");
        let mut wait_futures = std::mem::take(&mut self.wait_futures);

        while let (Some(job), tail) = wait_futures.into_future().await {
            match job {
                Ok(callback) => {
                    wait_futures = tail;

                    if let Some(callback) = callback {
                        if let Some(job) = match callback {
                            Callback::EditorCompositor(call) => {
                                if let Some(compositor) = &mut compositor {
                                    call(editor, compositor);
                                }
                                None
                            }
                            Callback::Editor(call) => {
                                call(editor);
                                None
                            }
                            Callback::Followup(call) => call(editor),
                        } {
                            if job.wait {
                                wait_futures.push(job.future);
                            }
                        }
                    }
                }
                Err(e) => {
                    self.wait_futures = tail;
                    return Err(e);
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use arc_swap::{access::Map, ArcSwap};
    use helix_core::syntax;
    use helix_view::{graphics::Rect, theme};

    use super::*;

    #[tokio::test]
    async fn callback_batches_preserve_order_without_dropping_the_next_callback() {
        let config = Arc::new(ArcSwap::from_pointee(crate::config::Config::default()));
        let handlers = crate::handlers::setup_for_test(config.clone());
        let area = Rect::new(0, 0, 80, 24);
        let mut editor = Editor::new(
            area,
            Arc::new(theme::Loader::new(&[])),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
            Arc::new(Map::new(config, |config: &crate::config::Config| {
                &config.editor
            })),
            handlers,
            helix_loader::workspace_trust::WorkspaceTrust::fully_trusted(),
        );
        let mut compositor = Compositor::new(area);
        let (tx, callbacks) = channel(128);
        let (_status_tx, status_messages) = channel(1);
        let mut jobs = Jobs {
            callbacks,
            status_messages,
            wait_futures: FuturesUnordered::new(),
            poll_wait_futures_first: true,
        };
        let observed = Arc::new(Mutex::new(Vec::new()));
        let total = MAX_CALLBACKS_PER_BATCH * 2 + 3;
        for index in 0..total {
            let observed = observed.clone();
            tx.send(Callback::Editor(Box::new(move |_| {
                observed.lock().unwrap().push(index)
            })))
            .await
            .unwrap();
        }
        let first = jobs.callbacks.recv().await.unwrap();
        jobs.handle_callback_batch(&mut editor, &mut compositor, Ok(Some(first)));
        let processed = observed.lock().unwrap().len();
        assert!((1..=MAX_CALLBACKS_PER_BATCH).contains(&processed));
        assert_eq!(jobs.callbacks.len(), total - processed);
        while let Ok(first) = jobs.callbacks.try_recv() {
            jobs.handle_callback_batch(&mut editor, &mut compositor, Ok(Some(first)));
        }
        assert_eq!(*observed.lock().unwrap(), (0..total).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn ready_callbacks_do_not_wait_for_unfinished_jobs() {
        let (tx, callbacks) = channel(1);
        let (_status_tx, status_messages) = channel(1);
        let mut jobs = Jobs {
            callbacks,
            status_messages,
            wait_futures: FuturesUnordered::new(),
            poll_wait_futures_first: true,
        };
        jobs.wait_futures.push(std::future::pending().boxed());
        tx.send(Callback::Editor(Box::new(|_| {}))).await.unwrap();
        assert!(matches!(
            jobs.next_ready_callback(),
            Some(Ok(Some(Callback::Editor(_))))
        ));
        assert!(jobs.next_ready_callback().is_none());
        assert_eq!(jobs.wait_futures.len(), 1);
    }

    #[tokio::test]
    async fn ready_wait_jobs_are_not_starved_by_callback_bursts() {
        let (tx, callbacks) = channel(8);
        let (_status_tx, status_messages) = channel(1);
        let mut jobs = Jobs {
            callbacks,
            status_messages,
            wait_futures: FuturesUnordered::new(),
            poll_wait_futures_first: true,
        };
        for _ in 0..8 {
            tx.send(Callback::Editor(Box::new(|_| {}))).await.unwrap();
        }
        jobs.wait_futures.push(async { Ok(None) }.boxed());
        assert!(matches!(jobs.next_ready_callback(), Some(Ok(None))));
        assert_eq!(jobs.callbacks.len(), 8);
        assert!(matches!(
            jobs.next_ready_callback(),
            Some(Ok(Some(Callback::Editor(_))))
        ));
        jobs.wait_futures.push(async { Ok(None) }.boxed());
        assert!(matches!(jobs.next_ready_callback(), Some(Ok(None))));
        assert_eq!(jobs.callbacks.len(), 7);
    }

    #[tokio::test]
    async fn a_first_callback_exhausting_the_budget_does_not_starve_wait_jobs() {
        let config = Arc::new(ArcSwap::from_pointee(crate::config::Config::default()));
        let handlers = crate::handlers::setup_for_test(config.clone());
        let area = Rect::new(0, 0, 80, 24);
        let mut editor = Editor::new(
            area,
            Arc::new(theme::Loader::new(&[])),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
            Arc::new(Map::new(config, |config: &crate::config::Config| {
                &config.editor
            })),
            handlers,
            helix_loader::workspace_trust::WorkspaceTrust::fully_trusted(),
        );
        let mut compositor = Compositor::new(area);
        let (tx, callbacks) = channel(8);
        let (_status_tx, status_messages) = channel(1);
        let mut jobs = Jobs {
            callbacks,
            status_messages,
            wait_futures: FuturesUnordered::new(),
            poll_wait_futures_first: false,
        };
        let observed = Arc::new(Mutex::new(Vec::new()));
        for index in 0..4 {
            let observed = observed.clone();
            tx.send(Callback::Editor(Box::new(move |_| {
                observed.lock().unwrap().push(index);
                std::thread::sleep(CALLBACK_BATCH_BUDGET + std::time::Duration::from_millis(1));
            })))
            .await
            .unwrap();
        }
        let wait_observed = observed.clone();
        jobs.wait_futures.push(
            async move {
                Ok(Some(Callback::Editor(Box::new(move |_| {
                    wait_observed.lock().unwrap().push(99);
                }))))
            }
            .boxed(),
        );

        let first = Jobs::next_callback(
            &mut jobs.callbacks,
            &mut jobs.wait_futures,
            &mut jobs.poll_wait_futures_first,
        )
        .await
        .unwrap();
        jobs.handle_callback_batch(&mut editor, &mut compositor, first);
        assert_eq!(*observed.lock().unwrap(), [0]);
        assert_eq!(jobs.callbacks.len(), 3);
        assert_eq!(jobs.wait_futures.len(), 1);

        let next = Jobs::next_callback(
            &mut jobs.callbacks,
            &mut jobs.wait_futures,
            &mut jobs.poll_wait_futures_first,
        )
        .await
        .unwrap();
        jobs.handle_callback_batch(&mut editor, &mut compositor, next);
        assert_eq!(&observed.lock().unwrap()[..2], [0, 99]);
        assert!(jobs.wait_futures.is_empty());
    }
}
