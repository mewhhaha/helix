use std::{
    io::Read,
    path::Path,
    sync::{atomic, Arc},
    time::Duration,
};

use arc_swap::ArcSwap;
use helix_core::syntax;
use helix_event::AsyncHook;
use helix_view::{document::LoadedDocument, editor::Config, Document};
use tokio::{sync::watch, time::Instant};

use crate::{job, ui::overlay::Overlay};

use super::{
    bounded_directory_preview, CachedPreview, DynQueryCallback, Picker,
    MAX_DIRECTORY_PREVIEW_BYTES, MAX_FILE_SIZE_FOR_PREVIEW,
};

/// Run one blocking operation at a time, retaining only its latest successor.
/// The queued cell is taken by the worker, so a consumed request cannot keep
/// the producer alive through its own retained editor data.
pub(crate) struct LatestBlockingWorker<T> {
    sender: tokio::sync::mpsc::UnboundedSender<Arc<parking_lot::Mutex<Option<T>>>>,
    pending: parking_lot::Mutex<std::sync::Weak<parking_lot::Mutex<Option<T>>>>,
}

impl<T: Send + 'static> LatestBlockingWorker<T> {
    pub fn new(run: impl Fn(T) -> anyhow::Result<()> + Send + Sync + 'static) -> Self {
        let (sender, mut receiver) =
            tokio::sync::mpsc::unbounded_channel::<Arc<parking_lot::Mutex<Option<T>>>>();
        let run = Arc::new(run);
        tokio::spawn(async move {
            while let Some(request) = receiver.recv().await {
                let Some(request) = request.lock().take() else {
                    continue;
                };
                let run = run.clone();
                match tokio::task::spawn_blocking(move || run(request)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => helix_event::status::report(err).await,
                    Err(err) => helix_event::status::report(anyhow::Error::from(err)).await,
                }
            }
        });
        Self {
            sender,
            pending: parking_lot::Mutex::new(std::sync::Weak::new()),
        }
    }

    pub fn submit(&self, request: T) {
        let mut pending = self.pending.lock();
        if let Some(queued) = pending.upgrade() {
            let mut queued = queued.lock();
            if queued.is_some() {
                *queued = Some(request);
                return;
            }
        }
        let queued = Arc::new(parking_lot::Mutex::new(Some(request)));
        *pending = Arc::downgrade(&queued);
        let _ = self.sender.send(queued);
    }
}

#[derive(Default)]
pub(super) struct PreviewLoadStatus(atomic::AtomicBool);

impl PreviewLoadStatus {
    pub fn failed(&self) -> bool {
        self.0.load(atomic::Ordering::Relaxed)
    }

    pub fn fail(&self) {
        self.0.store(true, atomic::Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub(super) struct PreviewRequest {
    pub path: Arc<Path>,
    pub generation: usize,
    pub version: Arc<atomic::AtomicUsize>,
    pub status: Arc<PreviewLoadStatus>,
    pub config: Arc<ArcSwap<Config>>,
    pub syn_loader: Arc<ArcSwap<syntax::Loader>>,
}

enum LoadedPreview {
    Document(Box<LoadedDocument>),
    Directory(Vec<(String, bool)>),
    Binary,
    LargeFile,
    NotFound,
}

impl LoadedPreview {
    fn into_cached(self, editor: &helix_view::Editor) -> CachedPreview {
        match self {
            Self::Document(loaded) => CachedPreview::Document(Box::new(Document::from_preview(
                *loaded,
                editor.config.clone(),
                editor.syn_loader.clone(),
            ))),
            Self::Directory(entries) => CachedPreview::Directory(entries),
            Self::Binary => CachedPreview::Binary,
            Self::LargeFile => CachedPreview::LargeFile,
            Self::NotFound => CachedPreview::NotFound,
        }
    }
}

impl PreviewRequest {
    fn is_current(&self) -> bool {
        self.version.load(atomic::Ordering::Relaxed) == self.generation
    }

    fn matches_picker(&self, version: &Arc<atomic::AtomicUsize>, path: Option<&Path>) -> bool {
        Arc::ptr_eq(version, &self.version) && self.is_current() && path == Some(self.path.as_ref())
    }
}

pub(super) struct PreviewLoadHandler<T: 'static + Send + Sync, D: 'static + Send + Sync> {
    trigger: Option<PreviewRequest>,
    worker: Option<watch::Sender<Option<PreviewRequest>>>,
    phantom_data: std::marker::PhantomData<(T, D)>,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> Default for PreviewLoadHandler<T, D> {
    fn default() -> Self {
        Self {
            trigger: None,
            worker: None,
            phantom_data: Default::default(),
        }
    }
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> AsyncHook for PreviewLoadHandler<T, D> {
    type Event = PreviewRequest;

    fn handle_event(&mut self, request: Self::Event, _timeout: Option<Instant>) -> Option<Instant> {
        self.trigger = Some(request);
        Some(Instant::now() + Duration::from_millis(75))
    }

    fn finish_debounce(&mut self) {
        let Some(request) = self.trigger.take().filter(PreviewRequest::is_current) else {
            return;
        };
        if self.worker.as_ref().is_none_or(watch::Sender::is_closed) {
            let (sender, receiver) = watch::channel(None);
            tokio::spawn(load_previews::<T, D>(receiver));
            self.worker = Some(sender);
        }
        // At most one load is running and one latest request is waiting. Slow
        // filesystem operations cannot create an unbounded queue of workers.
        self.worker.as_ref().unwrap().send_replace(Some(request));
    }
}

async fn load_previews<T: 'static + Send + Sync, D: 'static + Send + Sync>(
    mut requests: watch::Receiver<Option<PreviewRequest>>,
) {
    while requests.changed().await.is_ok() {
        let Some(request) = requests.borrow_and_update().clone() else {
            continue;
        };
        if !request.is_current() {
            continue;
        }
        let load_request = request.clone();
        let result = tokio::task::spawn_blocking(move || load_preview(&load_request)).await;
        let preview = match result {
            Ok(Some(preview)) if request.is_current() => preview,
            Ok(_) => continue,
            Err(err) => {
                log::info!("loading picker preview failed: {err}");
                request.status.fail();
                helix_event::request_redraw();
                continue;
            }
        };
        job::dispatch(move |editor, compositor| {
            let Some(Overlay {
                content: picker, ..
            }) = compositor.find::<Overlay<Picker<T, D>>>()
            else {
                return;
            };
            if !request.matches_picker(&picker.preview_version, picker.preview_path.as_deref()) {
                return;
            }
            if picker
                .preview_pending
                .as_ref()
                .is_none_or(|pending| pending.generation != request.generation)
            {
                return;
            }
            picker.preview_pending = None;
            picker.preview_retry_at = None;
            let mut preview = preview.into_cached(editor);
            if let CachedPreview::Document(doc) = &mut preview {
                let diagnostics = helix_view::Editor::doc_diagnostics(
                    &editor.language_servers,
                    &editor.diagnostics,
                    doc,
                );
                doc.replace_diagnostics(diagnostics, &[], None);
            }
            picker.preview_cache.insert(request.path, preview);
        })
        .await;
    }
}

fn load_preview(request: &PreviewRequest) -> Option<LoadedPreview> {
    if !request.is_current() {
        return None;
    }
    let load = || -> std::io::Result<LoadedPreview> {
        let path = &request.path;
        let metadata = std::fs::metadata(path)?;
        if metadata.is_dir() {
            let files = super::super::directory_content_with_config_and_cancel(
                path,
                &request.config.load(),
                || !request.is_current(),
            )?;
            let names = files.into_iter().filter_map(|(file_path, is_dir)| {
                let name = file_path
                    .strip_prefix(path)
                    .map(|path| Some(path.as_os_str()))
                    .unwrap_or_else(|_| file_path.file_name())?
                    .to_string_lossy();
                Some((
                    if is_dir {
                        format!("{name}/")
                    } else {
                        name.into_owned()
                    },
                    is_dir,
                ))
            });
            return Ok(LoadedPreview::Directory(bounded_directory_preview(
                names,
                MAX_DIRECTORY_PREVIEW_BYTES,
            )));
        }
        if !metadata.is_file() {
            return Ok(LoadedPreview::NotFound);
        }
        if metadata.len() > MAX_FILE_SIZE_FOR_PREVIEW {
            return Ok(LoadedPreview::LargeFile);
        }
        let mut sample = Vec::with_capacity(1024);
        std::fs::File::open(path)?
            .take(1024)
            .read_to_end(&mut sample)?;
        if crate::is_binary(&sample) {
            return Ok(LoadedPreview::Binary);
        }
        if !request.is_current() {
            return Ok(LoadedPreview::NotFound);
        }
        let mut doc = Document::open(
            path,
            None,
            false,
            request.config.clone(),
            request.syn_loader.clone(),
        )
        .map_err(std::io::Error::other)?;
        if request.is_current() {
            let loader = request.syn_loader.load();
            if let Some(language_config) = doc.detect_language_config(&loader) {
                let language = language_config.language();
                doc.language = Some(language_config);
                match helix_core::Syntax::new(doc.text().slice(..), language, &loader) {
                    Ok(syntax) => doc.syntax = Some(syntax),
                    Err(err) => log::info!("highlighting picker preview failed: {err}"),
                }
            }
        }
        Ok(LoadedPreview::Document(Box::new(doc.into_preview())))
    };
    let preview = load().unwrap_or(LoadedPreview::NotFound);
    request.is_current().then_some(preview)
}

pub(super) struct DynamicQueryChange {
    pub query: Arc<str>,
    pub is_paste: bool,
    pub generation: usize,
    pub version: Arc<atomic::AtomicUsize>,
}

impl DynamicQueryChange {
    fn matches_picker(&self, version: &Arc<atomic::AtomicUsize>) -> bool {
        Arc::ptr_eq(&self.version, version)
            && version.load(atomic::Ordering::Relaxed) == self.generation
    }
}

pub(super) struct DynamicQueryHandler<T: 'static + Send + Sync, D: 'static + Send + Sync> {
    callback: Arc<DynQueryCallback<T, D>>,
    // Duration used as a debounce.
    // Defaults to 100ms if not provided via `Picker::with_dynamic_query`. Callers may want to set
    // this higher if the dynamic query is expensive - for example global search.
    debounce: Duration,
    last_query: Arc<str>,
    query: Option<DynamicQueryChange>,
    last_generation: usize,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> DynamicQueryHandler<T, D> {
    pub(super) fn new(callback: DynQueryCallback<T, D>, duration_ms: Option<u64>) -> Self {
        Self {
            callback: Arc::new(callback),
            debounce: Duration::from_millis(duration_ms.unwrap_or(100)),
            last_query: "".into(),
            query: None,
            last_generation: 0,
        }
    }
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> AsyncHook for DynamicQueryHandler<T, D> {
    type Event = DynamicQueryChange;

    fn handle_event(&mut self, change: Self::Event, _timeout: Option<Instant>) -> Option<Instant> {
        if change.query == self.last_query && change.generation == self.last_generation {
            // A secondary-column edit can keep the primary query and its
            // generation unchanged; its running request can still be reused.
            self.query = None;
            None
        } else {
            let is_paste = change.is_paste;
            self.query = Some(change);
            if is_paste {
                self.finish_debounce();
                None
            } else {
                Some(Instant::now() + self.debounce)
            }
        }
    }

    fn finish_debounce(&mut self) {
        let Some(change) = self.query.take() else {
            return;
        };
        self.last_query = change.query.clone();
        self.last_generation = change.generation;
        let callback = self.callback.clone();

        job::dispatch_blocking(move |editor, compositor| {
            let Some(Overlay {
                content: picker, ..
            }) = compositor.find::<Overlay<Picker<T, D>>>()
            else {
                return;
            };
            if !change.matches_picker(&picker.version) {
                return;
            }
            picker.matcher.restart(false);
            let injector = picker.injector();
            let get_options =
                (callback)(&change.query, editor, picker.editor_data.clone(), &injector);
            tokio::spawn(async move {
                if let Err(err) = get_options.await {
                    log::info!("Dynamic request failed: {err}");
                }
                // NOTE: the Drop implementation of Injector will request a redraw when the
                // injector falls out of scope here, clearing the "running" indicator.
            });
        })
    }
}

#[cfg(test)]
mod preview_load_tests {
    use super::*;

    fn request(path: &Path) -> PreviewRequest {
        let mut config = Config {
            editor_config: false,
            ..Config::default()
        };
        config.file_explorer.hidden = false;
        config.file_explorer.flatten_dirs = false;
        PreviewRequest {
            path: path.into(),
            generation: 1,
            version: Arc::new(atomic::AtomicUsize::new(1)),
            status: Arc::new(PreviewLoadStatus::default()),
            config: Arc::new(ArcSwap::from_pointee(config)),
            syn_loader: Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        }
    }

    #[test]
    fn preview_results_require_current_selection_and_picker_instance() {
        let request = request(Path::new("/preview/a"));
        assert!(request.matches_picker(&request.version, Some(&request.path)));
        assert!(!request.matches_picker(&request.version, Some(Path::new("/preview/b"))));
        assert!(
            !request.matches_picker(&Arc::new(atomic::AtomicUsize::new(1)), Some(&request.path))
        );
        request.version.fetch_add(1, atomic::Ordering::Relaxed);
        assert!(!request.matches_picker(&request.version, Some(&request.path)));
        assert!(load_preview(&request).is_none());
    }

    #[test]
    fn background_preview_loader_preserves_text_and_file_placeholders() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("text.txt");
        std::fs::write(&text, "preview 😀 café\n").unwrap();
        let text_request = request(&text);
        let Some(LoadedPreview::Document(loaded)) = load_preview(&text_request) else {
            panic!("text preview must load as a document");
        };
        let doc = Document::from_preview(*loaded, text_request.config, text_request.syn_loader);
        assert_eq!(doc.text().to_string(), "preview 😀 café\n");

        let binary = dir.path().join("binary.bin");
        std::fs::write(&binary, [0u8; 1024]).unwrap();
        assert!(matches!(
            load_preview(&request(&binary)),
            Some(LoadedPreview::Binary)
        ));
        let large = dir.path().join("large.txt");
        std::fs::File::create(&large)
            .unwrap()
            .set_len(MAX_FILE_SIZE_FOR_PREVIEW + 1)
            .unwrap();
        assert!(matches!(
            load_preview(&request(&large)),
            Some(LoadedPreview::LargeFile)
        ));
        assert!(matches!(
            load_preview(&request(&dir.path().join("missing"))),
            Some(LoadedPreview::NotFound)
        ));
    }

    #[test]
    fn directory_previews_keep_explorer_order_and_directory_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("text.txt"), "text\n").unwrap();
        let Some(LoadedPreview::Directory(entries)) = load_preview(&request(dir.path())) else {
            panic!("directory preview must load its entries");
        };
        assert_eq!(
            entries,
            vec![
                ("../".into(), true),
                ("nested/".into(), true),
                ("text.txt".into(), false)
            ]
        );
    }

    #[test]
    fn directory_preview_truncation_counts_allocated_name_and_vector_capacity() {
        let names = vec![
            ("first/".to_owned(), true),
            (String::with_capacity(4096), false),
            ("last.txt".to_owned(), false),
        ];
        let entries = bounded_directory_preview(names, 256);
        assert_eq!(
            entries,
            vec![
                ("first/".into(), true),
                (super::super::TRUNCATED_DIRECTORY_PREVIEW.into(), false)
            ]
        );
        let preview = CachedPreview::Directory(entries);
        assert!(preview.retained_bytes() <= 256);
    }
}

#[cfg(test)]
mod blocking_worker_tests {
    use super::*;

    #[test]
    fn dynamic_query_dispatch_requires_its_original_picker_and_generation() {
        let version = Arc::new(atomic::AtomicUsize::new(3));
        let change = DynamicQueryChange {
            query: "query".into(),
            is_paste: false,
            generation: 3,
            version: version.clone(),
        };
        assert!(change.matches_picker(&version));
        assert!(!change.matches_picker(&Arc::new(atomic::AtomicUsize::new(3))));
        version.fetch_add(1, atomic::Ordering::Relaxed);
        assert!(!change.matches_picker(&version));
    }

    #[tokio::test]
    async fn slow_worker_runs_only_the_latest_pending_request() {
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let blocked = parking_lot::Mutex::new(blocked);
        let worker = LatestBlockingWorker::new(move |request: Arc<usize>| {
            events.send(("start", *request)).unwrap();
            if *request == 0 {
                blocked.lock().recv().unwrap();
            }
            events.send(("finish", *request)).unwrap();
            Ok(())
        });
        worker.submit(Arc::new(0));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), received.recv())
                .await
                .unwrap(),
            Some(("start", 0))
        );

        let superseded = Arc::new(1);
        let retained = Arc::downgrade(&superseded);
        worker.submit(superseded);
        for request in 2..=32 {
            worker.submit(Arc::new(request));
        }
        // The obsolete payload is released while the first operation remains
        // blocked, rather than waiting behind it in an unbounded work queue.
        assert!(retained.upgrade().is_none());
        assert!(received.try_recv().is_err());
        release.send(()).unwrap();
        for expected in [("finish", 0), ("start", 32), ("finish", 32)] {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), received.recv())
                    .await
                    .unwrap(),
                Some(expected)
            );
        }
        assert!(received.try_recv().is_err());
    }
}
