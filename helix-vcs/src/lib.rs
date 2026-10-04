//! `helix_vcs` provides types for working with diffs from a Version Control System (VCS).
//! Currently `git` is the only supported provider for diffs, but this architecture allows
//! for other providers to be added in the future.

use anyhow::{anyhow, bail, Result};
use arc_swap::ArcSwap;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

#[cfg(feature = "git")]
mod git;

mod diff;

pub use diff::{DiffHandle, Hunk};

mod status;

pub use status::FileChange;

#[derive(Debug, Default)]
pub struct PreparedVcs {
    pub diff_base: Option<Vec<u8>>,
    pub head: Option<Arc<ArcSwap<Box<str>>>>,
    pub review_revision: Option<ReviewRevision>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewRevision {
    pub target_commit: String,
    pub base_commit: String,
    pub head_commit: String,
    pub branch: Option<String>,
}

pub struct RevisionDiffBase {
    pub bytes: Vec<u8>,
    pub commit: String,
    pub revision: ReviewRevision,
}

/// Contains all active diff providers. Diff providers are compiled in via features. Currently
/// only `git` is supported.
#[derive(Clone)]
pub struct DiffProviderRegistry {
    providers: Vec<DiffProvider>,
    workers: Arc<tokio::sync::Semaphore>,
}

impl DiffProviderRegistry {
    /// Non-interactive review tools share Git resolution and trust rules with
    /// the editor. These synchronous calls belong on a worker or CLI thread.
    pub fn review_base(
        &self,
        file: &Path,
        reference: &str,
        trust_full: bool,
        cancel: &helix_event::TaskHandle,
    ) -> Result<RevisionDiffBase> {
        self.providers
            .first()
            .copied()
            .unwrap_or(DiffProvider::None)
            .get_review_base(file, reference, trust_full, cancel)
    }

    pub fn revision_file(
        &self,
        file: &Path,
        commit: &str,
        trust_full: bool,
        cancel: &helix_event::TaskHandle,
    ) -> Result<Vec<u8>> {
        #[cfg(feature = "git")]
        if self.providers.contains(&DiffProvider::Git) {
            return git::get_revision_file(file, commit, trust_full, cancel);
        }
        let _ = (file, commit, trust_full, cancel);
        bail!("Git support is unavailable")
    }

    /// Resolve a pull-request baseline and decode it on the bounded Git worker.
    pub async fn prepare_review_with<T: Send + 'static>(
        &self,
        file: PathBuf,
        reference: String,
        trust: impl FnOnce() -> bool + Send + 'static,
        cancel: helix_event::TaskHandle,
        finish: impl FnOnce(RevisionDiffBase, &helix_event::TaskHandle, bool) -> T + Send + 'static,
    ) -> Result<Option<T>> {
        let Some(permit) =
            helix_event::cancelable_future(self.workers.clone().acquire_owned(), &cancel)
                .await
                .transpose()?
        else {
            return Ok(None);
        };
        let provider = self
            .providers
            .first()
            .copied()
            .unwrap_or(DiffProvider::None);
        let worker_cancel = cancel.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if worker_cancel.is_canceled() {
                return Ok(None);
            }
            let trust_full = trust();
            let prepared =
                provider.get_review_base(&file, &reference, trust_full, &worker_cancel)?;
            if worker_cancel.is_canceled() {
                return Ok(None);
            }
            Ok(Some(finish(prepared, &worker_cancel, trust_full)))
        })
        .await;
        if cancel.is_canceled() {
            return Ok(None);
        }
        result.map_err(|err| anyhow!("Git review worker failed: {err}"))?
    }

    /// Prepare both gutter inputs together without blocking the editor. The
    /// permit belongs to the blocking closure, so canceling an async caller
    /// cannot start more workers while its filesystem operation is still busy.
    pub async fn prepare_vcs(
        &self,
        file: PathBuf,
        trust_full: bool,
        cancel: helix_event::TaskHandle,
    ) -> Option<PreparedVcs> {
        self.prepare_vcs_with(file, move || trust_full, cancel, |prepared, _, _| prepared)
            .await
    }

    /// Decode or otherwise prepare the result on the same bounded worker.
    pub async fn prepare_vcs_with<T: Send + 'static>(
        &self,
        file: PathBuf,
        trust: impl FnOnce() -> bool + Send + 'static,
        cancel: helix_event::TaskHandle,
        finish: impl FnOnce(PreparedVcs, &helix_event::TaskHandle, bool) -> T + Send + 'static,
    ) -> Option<T> {
        let permit = helix_event::cancelable_future(self.workers.clone().acquire_owned(), &cancel)
            .await?
            .ok()?;
        let providers = self.providers.clone();
        let worker_cancel = cancel.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if worker_cancel.is_canceled() {
                return None;
            }
            // Resolve trust after waiting for the worker slot. Revoking trust
            // while an operation is queued must disable external Git filters.
            let trust_full = trust();
            if worker_cancel.is_canceled() {
                return None;
            }
            let prepared = providers
                .iter()
                .find_map(|provider| {
                    provider
                        .prepare_vcs(&file, trust_full, &worker_cancel)
                        .map_err(|err| {
                            log::debug!("Preparing VCS data for {}: {err:#}", file.display())
                        })
                        .ok()
                })
                .unwrap_or_default();
            if worker_cancel.is_canceled() {
                None
            } else {
                Some(finish(prepared, &worker_cancel, trust_full))
            }
        })
        .await;
        if cancel.is_canceled() {
            return None;
        }
        match result {
            Ok(prepared) => prepared,
            Err(err) => {
                log::error!("VCS worker failed: {err}");
                None
            }
        }
    }

    /// Get the given file from the VCS. This provides the unedited document as a "base"
    /// for a diff to be created.
    pub fn get_diff_base(&self, file: &Path, trust_full: bool) -> Option<Vec<u8>> {
        self.providers
            .iter()
            .find_map(|provider| match provider.get_diff_base(file, trust_full) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to open diff base for {}", file.display());
                    None
                }
            })
    }

    /// Get the current name of the current [HEAD](https://stackoverflow.com/questions/2304087/what-is-head-in-git).
    pub fn get_current_head_name(
        &self,
        file: &Path,
        trust_full: bool,
    ) -> Option<Arc<ArcSwap<Box<str>>>> {
        self.providers.iter().find_map(|provider| {
            match provider.get_current_head_name(file, trust_full) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to obtain current head name for {}", file.display());
                    None
                }
            }
        })
    }

    /// Iterate changed files within the same worker limit as baseline preparation.
    /// Cancellation interrupts Git's walk even when it has yielded no changes.
    pub async fn for_each_changed_file(
        self,
        cwd: PathBuf,
        trust: impl FnOnce() -> bool + Send + 'static,
        cancel: helix_event::TaskHandle,
        f: impl Fn(Result<FileChange>) -> bool + Send + 'static,
    ) {
        let providers = self.providers;
        let scan_cancel = cancel.clone();
        run_changed_file_worker(self.workers, cancel, move |interrupt| {
            if scan_cancel.is_canceled() || interrupt.load(Ordering::Relaxed) {
                return;
            }
            let trust_full = trust();
            if scan_cancel.is_canceled() || interrupt.load(Ordering::Relaxed) {
                return;
            }
            let on_change = |change| !scan_cancel.is_canceled() && f(change);
            if providers
                .iter()
                .find_map(|provider| {
                    provider
                        .for_each_changed_file(&cwd, trust_full, interrupt.clone(), on_change)
                        .ok()
                })
                .is_none()
                && !scan_cancel.is_canceled()
                && !interrupt.load(Ordering::Relaxed)
            {
                f(Err(anyhow!("no diff provider returns success")));
            }
        })
        .await;
    }
}

struct InterruptOnDrop(Arc<AtomicBool>);
impl Drop for InterruptOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn run_changed_file_worker(
    workers: Arc<tokio::sync::Semaphore>,
    cancel: helix_event::TaskHandle,
    work: impl FnOnce(Arc<AtomicBool>) + Send + 'static,
) {
    let Some(Ok(permit)) = helix_event::cancelable_future(workers.acquire_owned(), &cancel).await
    else {
        return;
    };
    if cancel.is_canceled() {
        return;
    }
    let interrupt = Arc::new(AtomicBool::new(false));
    let _interrupt_on_drop = InterruptOnDrop(interrupt.clone());
    let worker_interrupt = interrupt.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work(worker_interrupt);
    });
    let result = tokio::select! {
        biased;
        _ = cancel.canceled() => {
            interrupt.store(true, Ordering::Relaxed);
            // Keep supervising the blocking worker; its permit is released only
            // after the interrupted walk has actually returned.
            worker.await
        }
        result = &mut worker => result,
    };
    if let Err(err) = result {
        log::error!("Git status worker failed: {err}");
    }
}

impl Default for DiffProviderRegistry {
    fn default() -> Self {
        // currently only git is supported
        // TODO make this configurable when more providers are added
        let providers = vec![
            #[cfg(feature = "git")]
            DiffProvider::Git,
            DiffProvider::None,
        ];
        DiffProviderRegistry {
            providers,
            workers: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
}

#[cfg(test)]
mod preparation_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn changed_file_cancellation_before_worker_slot_skips_all_work() {
        let workers = Arc::new(tokio::sync::Semaphore::new(1));
        let busy = workers.clone().acquire_owned().await.unwrap();
        let mut controller = helix_event::TaskController::new();
        let cancel = controller.restart();
        let task = tokio::spawn(run_changed_file_worker(workers.clone(), cancel, |_| {
            panic!("canceled scan started")
        }));
        tokio::task::yield_now().await;
        controller.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        drop(busy);
        assert_eq!(workers.available_permits(), 1);
    }

    #[tokio::test]
    async fn changed_file_cancellation_interrupts_before_any_result_and_retains_permit() {
        let workers = Arc::new(tokio::sync::Semaphore::new(1));
        let mut controller = helix_event::TaskController::new();
        let cancel = controller.restart();
        let (started, start) = tokio::sync::oneshot::channel();
        let (interrupted, interrupt) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let task = tokio::spawn(run_changed_file_worker(
            workers.clone(),
            cancel,
            move |flag| {
                started.send(()).unwrap();
                while !flag.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                interrupted.send(()).unwrap();
                released.recv().unwrap();
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), start)
            .await
            .unwrap()
            .unwrap();
        controller.cancel();
        tokio::time::timeout(Duration::from_secs(1), interrupt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            workers.available_permits(),
            0,
            "canceled blocking work still owns its slot"
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(workers.available_permits(), 1);
    }

    #[tokio::test]
    async fn queued_vcs_work_resolves_trust_after_obtaining_its_worker_slot() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let registry = DiffProviderRegistry {
            providers: Vec::new(),
            workers: Arc::new(tokio::sync::Semaphore::new(2)),
        };
        let busy = registry
            .workers
            .clone()
            .acquire_many_owned(2)
            .await
            .unwrap();
        let trusted = Arc::new(AtomicBool::new(true));
        let trust_queried = Arc::new(AtomicBool::new(false));
        let worker_trust = trusted.clone();
        let worker_queried = trust_queried.clone();
        let mut controller = helix_event::TaskController::new();
        let cancel = controller.restart();
        let task = tokio::spawn(async move {
            registry
                .prepare_vcs_with(
                    PathBuf::new(),
                    move || {
                        worker_queried.store(true, Ordering::Relaxed);
                        worker_trust.load(Ordering::Relaxed)
                    },
                    cancel,
                    |_, _, trust| trust,
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(!trust_queried.load(Ordering::Relaxed));
        trusted.store(false, Ordering::Relaxed);
        drop(busy);
        assert_eq!(task.await.unwrap(), Some(false));
        assert!(trust_queried.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn dropping_callers_keeps_blocking_vcs_work_within_the_shared_limit() {
        let registry = DiffProviderRegistry {
            providers: Vec::new(),
            workers: Arc::new(tokio::sync::Semaphore::new(2)),
        };
        let mut controllers = Vec::new();
        let mut tasks = Vec::new();
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
        let mut releases = Vec::new();
        for index in 0..2 {
            let (release, blocked) = std::sync::mpsc::channel();
            releases.push(release);
            let mut controller = helix_event::TaskController::new();
            let cancel = controller.restart();
            controllers.push(controller);
            let registry = registry.clone();
            let started = started.clone();
            tasks.push(tokio::spawn(async move {
                registry
                    .prepare_vcs_with(
                        PathBuf::new(),
                        || false,
                        cancel,
                        move |_, _, _| {
                            started.send(index).unwrap();
                            blocked.recv().unwrap();
                        },
                    )
                    .await
            }));
        }
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(5), starts.recv())
                .await
                .unwrap()
                .unwrap();
        }
        for task in &tasks {
            task.abort();
        }
        for controller in &mut controllers {
            controller.cancel();
        }
        assert_eq!(registry.workers.available_permits(), 0);

        let mut controller = helix_event::TaskController::new();
        let cancel = controller.restart();
        let third = tokio::spawn(async move {
            registry
                .prepare_vcs_with(
                    PathBuf::new(),
                    || false,
                    cancel,
                    move |_, _, _| {
                        started.send(2).unwrap();
                    },
                )
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), starts.recv())
                .await
                .is_err()
        );
        releases[0].send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), starts.recv())
                .await
                .unwrap(),
            Some(2)
        );
        releases[1].send(()).unwrap();
        assert!(third.await.unwrap().is_some());
    }

    #[tokio::test]
    async fn queued_review_cancellation_skips_git_and_preserves_worker_slots() {
        let registry = DiffProviderRegistry::default();
        let busy = registry
            .workers
            .clone()
            .acquire_many_owned(2)
            .await
            .unwrap();
        let worker = registry.clone();
        let mut controller = helix_event::TaskController::new();
        let cancel = controller.restart();
        let task = tokio::spawn(async move {
            worker
                .prepare_review_with::<()>(
                    PathBuf::from("missing/file.txt"),
                    "main".into(),
                    || panic!("canceled review queried trust"),
                    cancel,
                    |_, _, _| panic!("canceled review started decoding"),
                )
                .await
        });
        tokio::task::yield_now().await;
        controller.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_none());
        drop(busy);
        assert_eq!(registry.workers.available_permits(), 2);
    }

    #[tokio::test]
    async fn canceled_vcs_request_never_runs_its_preparation_callback() {
        let registry = DiffProviderRegistry::default();
        let mut controller = helix_event::TaskController::new();
        let cancel = controller.restart();
        controller.cancel();
        let prepared = registry
            .prepare_vcs_with(
                PathBuf::new(),
                || false,
                cancel,
                |_, _, _| panic!("canceled request began blocking work"),
            )
            .await;
        assert!(prepared.is_none());
    }
}

/// A union type that includes all types that implement [DiffProvider]. We need this type to allow
/// cloning [DiffProviderRegistry] as `Clone` cannot be used in trait objects.
///
/// `Copy` is simply to ensure the `clone()` call is the simplest it can be.
#[derive(Copy, Clone, PartialEq, Eq)]
enum DiffProvider {
    #[cfg(feature = "git")]
    Git,
    None,
}

impl DiffProvider {
    fn get_review_base(
        &self,
        file: &Path,
        reference: &str,
        trust_full: bool,
        cancel: &helix_event::TaskHandle,
    ) -> Result<RevisionDiffBase> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_review_base(file, reference, trust_full, cancel),
            Self::None => {
                let _ = (file, reference, trust_full, cancel);
                bail!("Git diff support is not available")
            }
        }
    }

    fn prepare_vcs(
        &self,
        file: &Path,
        trust_full: bool,
        cancel: &helix_event::TaskHandle,
    ) -> Result<PreparedVcs> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::prepare_vcs(file, trust_full, cancel),
            Self::None => {
                let _ = (file, trust_full, cancel);
                bail!("No diff support compiled in")
            }
        }
    }

    fn get_diff_base(&self, file: &Path, trust_full: bool) -> Result<Vec<u8>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_diff_base(file, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn get_current_head_name(
        &self,
        file: &Path,
        trust_full: bool,
    ) -> Result<Arc<ArcSwap<Box<str>>>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_current_head_name(file, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn for_each_changed_file(
        &self,
        cwd: &Path,
        trust_full: bool,
        interrupt: Arc<AtomicBool>,
        f: impl Fn(Result<FileChange>) -> bool,
    ) -> Result<()> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::for_each_changed_file(cwd, trust_full, interrupt, f),
            Self::None => {
                let _ = (cwd, trust_full, interrupt, f);
                bail!("No diff support compiled in")
            }
        }
    }
}
