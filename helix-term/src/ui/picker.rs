mod handlers;
mod query;
mod row_cache;

use crate::{
    alt,
    compositor::{self, Component, Compositor, Context, Event, EventResult},
    ctrl, key, shift,
    ui::{
        self,
        document::{render_document, LinePos, TextRenderer},
        picker::query::PickerQuery,
        text_decorations::DecorationManager,
        EditorView,
    },
};
use futures_util::future::BoxFuture;
use helix_event::AsyncHook;
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Nucleo};
use thiserror::Error;
use tokio::sync::mpsc::Sender;
use tui::{
    buffer::Buffer as Surface,
    layout::Constraint,
    text::{Span, Spans},
    widgets::{Block, BorderType, Cell, Row, Table},
};

use tui::widgets::Widget;

use std::{
    borrow::Cow,
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{self, AtomicUsize},
        Arc,
    },
    time::{Duration, Instant},
};

use crate::ui::{Prompt, PromptEvent};
use helix_core::{
    char_idx_at_visual_offset, fuzzy::MATCHER, movement::Direction,
    text_annotations::TextAnnotations, unicode::segmentation::UnicodeSegmentation, Position,
};
use helix_view::{
    editor::Action,
    graphics::{CursorKind, Margin, Modifier, Rect},
    theme::Style,
    view::ViewPosition,
    Document, DocumentId, Editor,
};

pub(crate) use self::handlers::LatestBlockingWorker;
use self::handlers::{
    DynamicQueryChange, DynamicQueryHandler, PreviewLoadHandler, PreviewLoadStatus, PreviewRequest,
};

pub const ID: &str = "picker";

pub const MIN_AREA_WIDTH_FOR_PREVIEW: u16 = 72;
/// Biggest file size to preview in bytes
pub const MAX_FILE_SIZE_FOR_PREVIEW: u64 = 10 * 1024 * 1024;

const MAX_CACHED_PREVIEWS: usize = 32;
const MAX_CACHED_PREVIEW_BYTES: usize = 32 * 1024 * 1024;
const MAX_DIRECTORY_PREVIEW_BYTES: usize = 2 * 1024 * 1024;
const PREVIEW_RETRY_DELAY: Duration = Duration::from_millis(250);
const TRUNCATED_DIRECTORY_PREVIEW: &str = "… <directory preview truncated>";

fn bounded_directory_preview(
    entries: impl IntoIterator<Item = (String, bool)>,
    max_bytes: usize,
) -> Vec<(String, bool)> {
    let entry_bytes = std::mem::size_of::<(String, bool)>();
    let marker_bytes = TRUNCATED_DIRECTORY_PREVIEW.len() + entry_bytes;
    let mut retained = 0;
    let mut bounded = Vec::new();
    for (name, is_dir) in entries {
        let bytes = name.capacity() + entry_bytes;
        if retained + bytes > max_bytes.saturating_sub(marker_bytes) {
            if marker_bytes <= max_bytes {
                bounded.push((TRUNCATED_DIRECTORY_PREVIEW.to_owned(), false));
            }
            break;
        }
        retained += bytes;
        bounded.push((name, is_dir));
    }
    // Do not retain the growth capacity of a much larger directory listing.
    bounded.into_boxed_slice().into_vec()
}

#[derive(PartialEq, Eq, Hash)]
pub enum PathOrId<'a> {
    Id(DocumentId),
    Path(&'a Path),
}

impl<'a> From<&'a Path> for PathOrId<'a> {
    fn from(path: &'a Path) -> Self {
        Self::Path(path)
    }
}

impl From<DocumentId> for PathOrId<'_> {
    fn from(v: DocumentId) -> Self {
        Self::Id(v)
    }
}

type FileCallback<T> = Box<dyn for<'a> Fn(&'a Editor, &'a T) -> Option<FileLocation<'a>>>;

/// File path and range of lines (used to align and highlight lines)
pub type FileLocation<'a> = (PathOrId<'a>, Option<(usize, usize)>);

pub enum CachedPreview {
    Document(Box<Document>),
    Directory(Vec<(String, bool)>),
    Binary,
    LargeFile,
    NotFound,
}

impl CachedPreview {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Document(doc) => doc.text().len_bytes(),
            Self::Directory(entries) => {
                entries.capacity() * std::mem::size_of::<(String, bool)>()
                    + entries
                        .iter()
                        .map(|(name, _)| name.capacity())
                        .sum::<usize>()
            }
            _ => std::mem::size_of::<Self>(),
        }
    }
}

struct CachedPreviewEntry {
    preview: CachedPreview,
    used: u64,
    bytes: usize,
}

/// Bound both retained decoded text and the number of syntax trees / directory
/// listings. The byte budget excludes opaque tree-sitter allocations.
#[derive(Default)]
struct PreviewCache {
    entries: HashMap<Arc<Path>, CachedPreviewEntry>,
    bytes: usize,
    clock: u64,
}

impl PreviewCache {
    fn contains_key(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    fn get(&mut self, path: &Path) -> Option<&CachedPreview> {
        let entry = self.entries.get_mut(path)?;
        self.clock = self.clock.wrapping_add(1);
        entry.used = self.clock;
        Some(&entry.preview)
    }

    fn insert(&mut self, path: Arc<Path>, preview: CachedPreview) {
        let preview = if preview.retained_bytes() > MAX_CACHED_PREVIEW_BYTES {
            match preview {
                CachedPreview::Directory(entries) => CachedPreview::Directory(
                    bounded_directory_preview(entries, MAX_DIRECTORY_PREVIEW_BYTES),
                ),
                CachedPreview::Document(_) => CachedPreview::LargeFile,
                preview => preview,
            }
        } else {
            preview
        };
        if let Some(previous) = self.entries.remove(&path) {
            self.bytes -= previous.bytes;
        }
        self.clock = self.clock.wrapping_add(1);
        let bytes = preview.retained_bytes();
        self.bytes += bytes;
        self.entries.insert(
            path.clone(),
            CachedPreviewEntry {
                preview,
                bytes,
                used: self.clock,
            },
        );
        while self.entries.len() > MAX_CACHED_PREVIEWS || self.bytes > MAX_CACHED_PREVIEW_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .filter(|(key, _)| *key != &path)
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            let entry = self.entries.remove(&oldest).unwrap();
            self.bytes -= entry.bytes;
        }
    }
}

struct PendingPreview {
    generation: usize,
    status: Arc<PreviewLoadStatus>,
}

// We don't store this enum in the cache so as to avoid lifetime constraints
// from borrowing a document already opened in the editor.
pub enum Preview<'picker, 'editor> {
    Cached(&'picker CachedPreview),
    EditorDocument(&'editor Document),
    Loading,
}

impl Preview<'_, '_> {
    fn document(&self) -> Option<&Document> {
        match self {
            Preview::EditorDocument(doc) => Some(doc),
            Preview::Cached(CachedPreview::Document(doc)) => Some(doc),
            _ => None,
        }
    }

    fn dir_content(&self) -> Option<&Vec<(String, bool)>> {
        match self {
            Preview::Cached(CachedPreview::Directory(dir_content)) => Some(dir_content),
            _ => None,
        }
    }

    /// Alternate text to show for the preview.
    fn placeholder(&self) -> &str {
        match *self {
            Self::EditorDocument(_) => "<Invalid file location>",
            Self::Loading => "<Loading preview…>",
            Self::Cached(preview) => match preview {
                CachedPreview::Document(_) => "<Invalid file location>",
                CachedPreview::Directory(_) => "<Invalid directory location>",
                CachedPreview::Binary => "<Binary file>",
                CachedPreview::LargeFile => "<File too large to preview>",
                CachedPreview::NotFound => "<File not found>",
            },
        }
    }
}

fn inject_nucleo_item<T, D>(
    injector: &nucleo::Injector<T>,
    columns: &[Column<T, D>],
    item: T,
    editor_data: &D,
) {
    injector.push(item, |item, dst| {
        for (column, text) in columns.iter().filter(|column| column.filter).zip(dst) {
            *text = column.format_text(item, editor_data).into()
        }
    });
}

pub struct Injector<T, D> {
    dst: nucleo::Injector<T>,
    columns: Arc<[Column<T, D>]>,
    editor_data: Arc<D>,
    version: usize,
    picker_version: Arc<AtomicUsize>,
    /// Requests a redraw when the injector drops, including on blocking workers.
    /// This causes the "running" indicator to disappear when a background job
    /// providing items is finished and drops. This could be wrapped in an [Arc] to ensure
    /// that the redraw is only requested when all Injectors drop for a Picker (which removes
    /// the "running" indicator) but the redraw handle is debounced so this is unnecessary.
    redraw: Arc<dyn Fn() + Send + Sync>,
}

impl<I, D> Clone for Injector<I, D> {
    fn clone(&self) -> Self {
        Injector {
            dst: self.dst.clone(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version,
            picker_version: self.picker_version.clone(),
            redraw: self.redraw.clone(),
        }
    }
}

#[derive(Error, Debug)]
#[error("picker has been shut down")]
pub struct InjectorShutdown;

impl<T, D> Injector<T, D> {
    pub fn cancellation(&self) -> PickerCancellation {
        PickerCancellation {
            version: self.version,
            current: self.picker_version.clone(),
        }
    }

    pub fn push(&self, item: T) -> Result<(), InjectorShutdown> {
        if self.version != self.picker_version.load(atomic::Ordering::Relaxed) {
            return Err(InjectorShutdown);
        }

        inject_nucleo_item(&self.dst, &self.columns, item, &self.editor_data);
        Ok(())
    }
}

impl<T, D> Drop for Injector<T, D> {
    fn drop(&mut self) {
        (self.redraw)();
    }
}

#[derive(Clone)]
pub struct PickerCancellation {
    version: usize,
    current: Arc<AtomicUsize>,
}

impl PickerCancellation {
    pub fn is_canceled(&self) -> bool {
        self.version != self.current.load(atomic::Ordering::Relaxed)
    }

    pub fn reader<'a, R: std::io::Read + 'a>(&'a self, reader: R) -> impl std::io::Read + 'a {
        struct Reader<'a, R> {
            inner: R,
            cancellation: &'a PickerCancellation,
        }
        impl<R: std::io::Read> std::io::Read for Reader<'_, R> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.cancellation.is_canceled() {
                    // Interrupted is retried by read_to_end, so cancellation
                    // needs an error that stops the searcher's reader loop.
                    return Err(std::io::Error::other("picker request canceled"));
                }
                let len = buffer.len().min(64 * 1024);
                self.inner.read(&mut buffer[..len])
            }
        }
        Reader {
            inner: reader,
            cancellation: self,
        }
    }
}

type ColumnFormatFn<T, D> = for<'a> fn(&'a T, &'a D) -> Cell<'a>;

pub struct Column<T, D> {
    name: Arc<str>,
    format: ColumnFormatFn<T, D>,
    /// Whether the column should be passed to nucleo for matching and filtering.
    /// `DynamicPicker` uses this so that the dynamic column (for example regex in
    /// global search) is not used for filtering twice.
    filter: bool,
    hidden: bool,
    cache_format: bool,
}

impl<T, D> Column<T, D> {
    pub fn new(name: impl Into<Arc<str>>, format: ColumnFormatFn<T, D>) -> Self {
        Self {
            name: name.into(),
            format,
            filter: true,
            hidden: false,
            cache_format: false,
        }
    }

    /// A column which does not display any contents
    pub fn hidden(name: impl Into<Arc<str>>) -> Self {
        let format = |_: &T, _: &D| unreachable!();

        Self {
            name: name.into(),
            format,
            filter: false,
            hidden: true,
            cache_format: true,
        }
    }

    /// Cache formatting for immutable item/data snapshots. Dynamic formatters
    /// keep the default and are evaluated before reusing match highlights.
    pub fn cached(mut self) -> Self {
        self.cache_format = true;
        self
    }

    pub fn without_filtering(mut self) -> Self {
        self.filter = false;
        self
    }

    fn format<'a>(&self, item: &'a T, data: &'a D) -> Cell<'a> {
        (self.format)(item, data)
    }

    fn format_text<'a>(&self, item: &'a T, data: &'a D) -> Cow<'a, str> {
        let text: String = self.format(item, data).content.into();
        text.into()
    }
}

/// Returns a new list of options to replace the contents of the picker
/// when called with the current picker query,
type DynQueryCallback<T, D> =
    fn(&str, &mut Editor, Arc<D>, &Injector<T, D>) -> BoxFuture<'static, anyhow::Result<()>>;

pub struct Picker<T: 'static + Send + Sync, D: 'static> {
    columns: Arc<[Column<T, D>]>,
    primary_column: usize,
    editor_data: Arc<D>,
    version: Arc<AtomicUsize>,
    matcher: Nucleo<T>,

    /// Current height of the completions box
    completion_height: u16,

    cursor: u32,
    prompt: Prompt,
    query: PickerQuery,

    /// Whether to show the preview panel (default true)
    show_preview: bool,
    /// Constraints for tabular formatting
    widths: Vec<Constraint>,
    row_cache: row_cache::RowCache,

    callback_fn: PickerCallback<T>,
    default_action: Action,

    pub truncate_start: bool,
    /// Caches paths to documents
    preview_cache: PreviewCache,
    preview_path: Option<Arc<Path>>,
    preview_version: Arc<AtomicUsize>,
    preview_pending: Option<PendingPreview>,
    preview_retry_at: Option<Instant>,
    /// Given an item in the picker, return the file path and line number to display.
    file_fn: Option<FileCallback<T>>,
    /// Debounced, bounded background loading for the currently previewed file.
    preview_load_handler: Sender<PreviewRequest>,
    dynamic_query_handler: Option<Sender<DynamicQueryChange>>,
    background_task: helix_event::TaskController,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> Picker<T, D> {
    pub fn stream(
        columns: impl IntoIterator<Item = Column<T, D>>,
        editor_data: D,
    ) -> (Nucleo<T>, Injector<T, D>) {
        let columns: Arc<[_]> = columns.into_iter().collect();
        let matcher_columns = columns.iter().filter(|col| col.filter).count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::redraw_callback()),
            Some(helix_stdx::cpu::auxiliary_worker_count()),
            matcher_columns,
        );
        let streamer = Injector {
            dst: matcher.injector(),
            columns,
            editor_data: Arc::new(editor_data),
            version: 0,
            picker_version: Arc::new(AtomicUsize::new(0)),
            redraw: Arc::new(helix_event::redraw_callback()),
        };
        (matcher, streamer)
    }

    pub fn new<C, O, F>(
        columns: C,
        primary_column: usize,
        options: O,
        editor_data: D,
        callback_fn: F,
    ) -> Self
    where
        C: IntoIterator<Item = Column<T, D>>,
        O: IntoIterator<Item = T>,
        F: Fn(&mut Context, &T, Action) + 'static,
    {
        let columns: Arc<[_]> = columns.into_iter().collect();
        let matcher_columns = columns
            .iter()
            .filter(|col: &&Column<T, D>| col.filter)
            .count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::redraw_callback()),
            Some(helix_stdx::cpu::auxiliary_worker_count()),
            matcher_columns,
        );
        let injector = matcher.injector();
        for item in options {
            inject_nucleo_item(&injector, &columns, item, &editor_data);
        }
        Self::with(
            matcher,
            columns,
            primary_column,
            Arc::new(editor_data),
            Arc::new(AtomicUsize::new(0)),
            callback_fn,
        )
    }

    pub fn with_stream(
        matcher: Nucleo<T>,
        primary_column: usize,
        injector: Injector<T, D>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        Self::with(
            matcher,
            injector.columns.clone(),
            primary_column,
            injector.editor_data.clone(),
            injector.picker_version.clone(),
            callback_fn,
        )
    }

    fn with(
        matcher: Nucleo<T>,
        columns: Arc<[Column<T, D>]>,
        default_column: usize,
        editor_data: Arc<D>,
        version: Arc<AtomicUsize>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        assert!(!columns.is_empty());

        let prompt = Prompt::new(
            "".into(),
            None,
            ui::completers::none,
            |_editor: &mut Context, _pattern: &str, _event: PromptEvent| {},
        );

        let widths = columns
            .iter()
            .map(|column| Constraint::Length(column.name.chars().count() as u16))
            .collect();

        let query = PickerQuery::new(columns.iter().map(|col| &col.name).cloned(), default_column);

        Self {
            columns,
            primary_column: default_column,
            matcher,
            editor_data,
            version,
            cursor: 0,
            prompt,
            query,
            truncate_start: true,
            show_preview: true,
            callback_fn: Box::new(callback_fn),
            default_action: Action::Replace,
            completion_height: 0,
            widths,
            row_cache: Default::default(),
            preview_cache: PreviewCache::default(),
            preview_path: None,
            preview_version: Arc::new(AtomicUsize::new(0)),
            preview_pending: None,
            preview_retry_at: None,
            file_fn: None,
            preview_load_handler: PreviewLoadHandler::<T, D>::default().spawn(),
            dynamic_query_handler: None,
            background_task: helix_event::TaskController::new(),
        }
    }

    /// Start an operation that must stop when this picker closes.
    pub fn cancel_background_task(&mut self) {
        self.background_task.cancel();
    }

    pub fn background_task(&mut self) -> helix_event::TaskHandle {
        self.background_task.restart()
    }

    pub fn injector(&self) -> Injector<T, D> {
        Injector {
            dst: self.matcher.injector(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version.load(atomic::Ordering::Relaxed),
            picker_version: self.version.clone(),
            redraw: Arc::new(helix_event::redraw_callback()),
        }
    }

    pub fn truncate_start(mut self, truncate_start: bool) -> Self {
        self.truncate_start = truncate_start;
        self
    }

    pub fn with_preview(
        mut self,
        preview_fn: impl for<'a> Fn(&'a Editor, &'a T) -> Option<FileLocation<'a>> + 'static,
    ) -> Self {
        self.file_fn = Some(Box::new(preview_fn));
        // assumption: if we have a preview we are matching paths... If this is ever
        // not true this could be a separate builder function
        self.matcher.update_config(Config::DEFAULT.match_paths());
        self
    }

    pub fn with_history_register(mut self, history_register: Option<char>) -> Self {
        self.prompt.with_history_register(history_register);
        self
    }

    pub fn with_initial_cursor(mut self, cursor: u32) -> Self {
        self.cursor = cursor;
        self
    }

    pub fn with_dynamic_query(
        mut self,
        callback: DynQueryCallback<T, D>,
        debounce_ms: Option<u64>,
    ) -> Self {
        let handler = DynamicQueryHandler::new(callback, debounce_ms).spawn();
        let event = DynamicQueryChange {
            query: self.primary_query(),
            // Treat the initial query as a paste.
            is_paste: true,
            generation: self.version.load(atomic::Ordering::Relaxed),
            version: self.version.clone(),
        };
        helix_event::send_blocking(&handler, event);
        self.dynamic_query_handler = Some(handler);
        self
    }

    pub fn with_default_action(mut self, action: Action) -> Self {
        self.default_action = action;
        self
    }

    /// Move the cursor by a number of lines, either down (`Forward`) or up (`Backward`)
    pub fn move_by(&mut self, amount: u32, direction: Direction) {
        let len = self.matcher.snapshot().matched_item_count();

        if len == 0 {
            // No results, can't move.
            return;
        }

        match direction {
            Direction::Forward => {
                self.cursor = self.cursor.saturating_add(amount) % len;
            }
            Direction::Backward => {
                self.cursor = self.cursor.saturating_add(len).saturating_sub(amount) % len;
            }
        }
    }

    /// Move the cursor down by exactly one page. After the last page comes the first page.
    pub fn page_up(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Backward);
    }

    /// Move the cursor up by exactly one page. After the first page comes the last page.
    pub fn page_down(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Forward);
    }

    /// Move the cursor to the first entry
    pub fn to_start(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the last entry
    pub fn to_end(&mut self) {
        self.cursor = self
            .matcher
            .snapshot()
            .matched_item_count()
            .saturating_sub(1);
    }

    pub fn selection(&self) -> Option<&T> {
        self.matcher
            .snapshot()
            .get_matched_item(self.cursor)
            .map(|item| item.data)
    }

    fn primary_query(&self) -> Arc<str> {
        self.query
            .get(&self.columns[self.primary_column].name)
            .cloned()
            .unwrap_or_else(|| "".into())
    }

    fn header_height(&self) -> u16 {
        if self.columns.len() > 1 {
            1
        } else {
            0
        }
    }

    pub fn toggle_preview(&mut self) {
        self.show_preview = !self.show_preview;
        if !self.show_preview {
            self.cancel_preview_load();
        }
    }

    fn cancel_preview_load(&mut self) {
        if self.preview_path.take().is_some() {
            self.preview_version.fetch_add(1, atomic::Ordering::Relaxed);
        }
        self.preview_pending = None;
        self.preview_retry_at = None;
    }

    fn retry_preview_load(&mut self) -> bool {
        if let Some(pending) = &self.preview_pending {
            if !pending.status.failed() {
                return false;
            }
            self.preview_pending = None;
            self.preview_retry_at = Some(Instant::now() + PREVIEW_RETRY_DELAY);
            if tokio::runtime::Handle::try_current().is_ok() {
                tokio::spawn(async {
                    tokio::time::sleep(PREVIEW_RETRY_DELAY).await;
                    helix_event::request_redraw();
                });
            }
        }
        self.preview_retry_at
            .is_none_or(|retry_at| Instant::now() >= retry_at)
    }

    fn prompt_handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if let EventResult::Consumed(_) = self.prompt.handle_event(event, cx) {
            self.handle_prompt_change(matches!(event, Event::Paste(_)));
        }
        EventResult::Consumed(None)
    }

    fn handle_prompt_change(&mut self, is_paste: bool) {
        // TODO: better track how the pattern has changed
        let old_primary_query = self.primary_query();
        let line = self.prompt.line();
        let old_query = self.query.parse(line);
        if self.query == old_query {
            return;
        }
        // If the query has meaningfully changed, reset the cursor to the top of the results.
        self.cursor = 0;
        // Have nucleo reparse each changed column.
        for (i, column) in self
            .columns
            .iter()
            .filter(|column| column.filter)
            .enumerate()
        {
            let pattern = self
                .query
                .get(&column.name)
                .map(|f| &**f)
                .unwrap_or_default();
            let old_pattern = old_query
                .get(&column.name)
                .map(|f| &**f)
                .unwrap_or_default();
            // Fastlane: most columns will remain unchanged after each edit.
            if pattern == old_pattern {
                continue;
            }
            let is_append = pattern.starts_with(old_pattern);
            self.matcher.pattern.reparse(
                i,
                pattern,
                CaseMatching::Smart,
                Normalization::Smart,
                is_append,
            );
        }
        // If this is a dynamic picker, notify the query hook that the primary
        // query might have been updated.
        if let Some(handler) = &self.dynamic_query_handler {
            let query = self.primary_query();
            if query != old_primary_query {
                // Stop obsolete scans during the debounce before their
                // successors start, including queries that produce no matches.
                self.version.fetch_add(1, atomic::Ordering::Relaxed);
            }
            let event = DynamicQueryChange {
                query,
                is_paste,
                generation: self.version.load(atomic::Ordering::Relaxed),
                version: self.version.clone(),
            };
            helix_event::send_blocking(handler, event);
        }
    }

    /// Get (cached) preview for the currently selected item. If a document corresponding
    /// to the path is already open in the editor, it is used instead.
    fn get_preview<'picker, 'editor>(
        &'picker mut self,
        editor: &'editor Editor,
    ) -> Option<(Preview<'picker, 'editor>, Option<(usize, usize)>)> {
        let Some(current) = self.selection() else {
            self.cancel_preview_load();
            return None;
        };
        let Some((path_or_id, range)) = (self.file_fn.as_ref()?)(editor, current) else {
            self.cancel_preview_load();
            return None;
        };

        match path_or_id {
            PathOrId::Path(path) => {
                let path: Arc<Path> = self
                    .preview_path
                    .as_ref()
                    .filter(|current| current.as_ref() == path)
                    .cloned()
                    .unwrap_or_else(|| path.into());
                let changed = self.preview_path.as_deref() != Some(path.as_ref());
                if changed {
                    self.preview_version.fetch_add(1, atomic::Ordering::Relaxed);
                    self.preview_path = Some(path.clone());
                    self.preview_pending = None;
                    self.preview_retry_at = None;
                }
                if let Some(doc) = editor.document_by_path(&path) {
                    return Some((Preview::EditorDocument(doc), range));
                }
                if self.preview_cache.contains_key(&path) {
                    return Some((
                        Preview::Cached(self.preview_cache.get(&path).unwrap()),
                        range,
                    ));
                }
                if self.retry_preview_load() {
                    let generation = self
                        .preview_version
                        .fetch_add(1, atomic::Ordering::Relaxed)
                        .wrapping_add(1);
                    let status = Arc::new(PreviewLoadStatus::default());
                    self.preview_pending = Some(PendingPreview {
                        generation,
                        status: status.clone(),
                    });
                    self.preview_retry_at = None;
                    let request = PreviewRequest {
                        path,
                        generation,
                        version: self.preview_version.clone(),
                        status: status.clone(),
                        config: Arc::new(arc_swap::ArcSwap::from_pointee(editor.config().clone())),
                        syn_loader: editor.syn_loader.clone(),
                    };
                    // Retry a saturated or closed handler later instead of blocking
                    // rendering or assuming that a dropped event is still pending.
                    if let Err(error) = self.preview_load_handler.try_send(request) {
                        if matches!(error, tokio::sync::mpsc::error::TrySendError::Closed(_)) {
                            self.preview_load_handler =
                                PreviewLoadHandler::<T, D>::default().spawn();
                        }
                        status.fail();
                        helix_event::request_redraw();
                    }
                }
                Some((Preview::Loading, range))
            }
            PathOrId::Id(id) => {
                self.cancel_preview_load();
                let doc = editor.documents.get(&id)?;
                Some((Preview::EditorDocument(doc), range))
            }
        }
    }

    fn render_picker(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let status = self.matcher.tick(10);
        let snapshot = self.matcher.snapshot();
        self.row_cache.prepare(
            self.version.load(atomic::Ordering::Relaxed),
            cx.editor.theme.cache_key(),
            self.file_fn.is_some(),
            status.changed,
        );
        if status.changed {
            self.cursor = self
                .cursor
                .min(snapshot.matched_item_count().saturating_sub(1))
        }

        let text_style = cx.editor.theme.get("ui.text");
        let selected = cx.editor.theme.get("ui.text.focus");
        let highlight_style = cx.editor.theme.get("special").add_modifier(Modifier::BOLD);

        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(area, background);

        const BLOCK: Block<'_> = Block::bordered();

        // calculate the inner area inside the box
        let inner = BLOCK.inner(area);

        BLOCK.render(area, surface);

        // -- Render the input bar:

        let count = format!(
            "{}{}/{}",
            if status.running || self.matcher.active_injectors() > 0 {
                "(running) "
            } else {
                ""
            },
            snapshot.matched_item_count(),
            snapshot.item_count(),
        );

        let area = inner.clip_left(1).with_height(1);
        let line_area = area.clip_right(count.len() as u16 + 1);

        // render the prompt first since it will clear its background
        self.prompt.render(line_area, surface, cx);

        surface.set_stringn(
            (area.x + area.width).saturating_sub(count.len() as u16 + 1),
            area.y,
            &count,
            (count.len()).min(area.width as usize),
            text_style,
        );

        // -- Separator
        let sep_style = cx.editor.theme.get("ui.background.separator");
        let borders = BorderType::line_symbols(BorderType::Plain);
        for x in inner.left()..inner.right() {
            if let Some(cell) = surface.get_mut(x, inner.y + 1) {
                cell.set_symbol(borders.horizontal).set_style(sep_style);
            }
        }

        // -- Render the contents:
        // subtract area of prompt from top
        let inner = inner.clip_top(2);
        let rows = inner.height.saturating_sub(self.header_height()) as u32;
        let offset = self.cursor - (self.cursor % std::cmp::max(1, rows));
        let cursor = self.cursor.saturating_sub(offset);
        let end = offset
            .saturating_add(rows)
            .min(snapshot.matched_item_count());
        let mut indices = Vec::new();
        let mut matcher = MATCHER.lock();
        matcher.config = Config::DEFAULT;
        if self.file_fn.is_some() {
            matcher.config.set_match_paths()
        }

        let options = snapshot.matched_items(offset..end).map(|item| {
            let mut widths = self.widths.iter_mut();
            let mut matcher_index = 0;

            let item_id = item.matcher_columns.as_ptr() as usize;
            Row::new(
                self.columns
                    .iter()
                    .enumerate()
                    .map(|(column_index, column)| {
                        if column.hidden {
                            return Cell::default();
                        }

                        let Some(Constraint::Length(max_width)) = widths.next() else {
                            unreachable!();
                        };
                        let cached = column
                            .cache_format
                            .then(|| self.row_cache.get(item_id, column_index, None))
                            .flatten();
                        if let Some((cell, width)) = cached {
                            *max_width = (*max_width).max(width.min(u16::MAX as usize) as u16);
                            if column.filter {
                                matcher_index += 1;
                            }
                            return cell;
                        }
                        let source = if column.cache_format {
                            self.row_cache
                                .formatted(item_id, column_index)
                                .unwrap_or_else(|| {
                                    column.format(item.data, &self.editor_data).into_owned()
                                })
                        } else {
                            column.format(item.data, &self.editor_data).into_owned()
                        };
                        if let Some((cell, width)) =
                            self.row_cache.get(item_id, column_index, Some(&source))
                        {
                            *max_width = (*max_width).max(width.min(u16::MAX as usize) as u16);
                            if column.filter {
                                matcher_index += 1;
                            }
                            return cell;
                        }
                        let mut cell = source.clone();
                        let width = if column.filter {
                            snapshot.pattern().column_pattern(matcher_index).indices(
                                item.matcher_columns[matcher_index].slice(..),
                                &mut matcher,
                                &mut indices,
                            );
                            indices.sort_unstable();
                            indices.dedup();
                            let mut indices = indices.drain(..);
                            let mut next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                            let mut span_list = Vec::new();
                            let mut current_span = String::new();
                            let mut current_style = Style::default();
                            let mut grapheme_idx = 0u32;
                            let mut width = 0;

                            let spans: &[Span] =
                                cell.content.lines.first().map_or(&[], |it| it.0.as_slice());
                            for span in spans {
                                // this looks like a bug on first glance, we are iterating
                                // graphemes but treating them as char indices. The reason that
                                // this is correct is that nucleo will only ever consider the first char
                                // of a grapheme (and discard the rest of the grapheme) so the indices
                                // returned by nucleo are essentially grapheme indecies
                                for grapheme in span.content.graphemes(true) {
                                    let style = if grapheme_idx == next_highlight_idx {
                                        next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                                        span.style.patch(highlight_style)
                                    } else {
                                        span.style
                                    };
                                    if style != current_style {
                                        if !current_span.is_empty() {
                                            span_list
                                                .push(Span::styled(current_span, current_style))
                                        }
                                        current_span = String::new();
                                        current_style = style;
                                    }
                                    current_span.push_str(grapheme);
                                    grapheme_idx += 1;
                                }
                                width += span.width();
                            }

                            span_list.push(Span::styled(current_span, current_style));
                            cell = Cell::from(Spans::from(span_list));
                            matcher_index += 1;
                            width
                        } else {
                            cell.content
                                .lines
                                .first()
                                .map(|line| line.width())
                                .unwrap_or_default()
                        };

                        if width as u16 > *max_width {
                            *max_width = width as u16;
                        }

                        self.row_cache
                            .insert(item_id, column_index, source, cell.clone(), width);
                        cell
                    }),
            )
        });

        let mut table = Table::new(options)
            .style(text_style)
            .highlight_style(selected)
            .highlight_symbol(" > ")
            .column_spacing(1)
            .widths(&self.widths);

        // -- Header
        if self.columns.len() > 1 {
            let active_column = self.query.active_column(self.prompt.position());
            let header_style = cx.editor.theme.get("ui.picker.header");
            let header_column_style = cx.editor.theme.get("ui.picker.header.column");

            table = table.header(
                Row::new(self.columns.iter().map(|column| {
                    if column.hidden {
                        Cell::default()
                    } else {
                        let style =
                            if active_column.is_some_and(|name| Arc::ptr_eq(name, &column.name)) {
                                cx.editor.theme.get("ui.picker.header.column.active")
                            } else {
                                header_column_style
                            };

                        Cell::from(Span::styled(Cow::from(&*column.name), style))
                    }
                }))
                .style(header_style),
            );
        }

        use tui::widgets::TableState;

        table.render_table(
            inner,
            surface,
            &mut TableState {
                offset: 0,
                selected: Some(cursor as usize),
            },
            self.truncate_start,
        );
    }

    fn render_preview(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        let text = cx.editor.theme.get("ui.text");
        let directory = cx.editor.theme.get("ui.text.directory");
        surface.clear_with(area, background);

        const BLOCK: Block<'_> = Block::bordered();

        // calculate the inner area inside the box
        let inner = BLOCK.inner(area);
        // 1 column gap on either side
        let margin = Margin::horizontal(1);
        let inner = inner.inner(margin);
        BLOCK.render(area, surface);

        if let Some((preview, range)) = self.get_preview(cx.editor) {
            let doc = match preview.document() {
                Some(doc)
                    if range.is_none_or(|(start, end)| {
                        start <= end && end <= doc.text().len_lines()
                    }) =>
                {
                    doc
                }
                _ => {
                    if let Some(dir_content) = preview.dir_content() {
                        for (i, (path, is_dir)) in
                            dir_content.iter().take(inner.height as usize).enumerate()
                        {
                            let style = if *is_dir { directory } else { text };
                            surface.set_stringn(
                                inner.x,
                                inner.y + i as u16,
                                path,
                                inner.width as usize,
                                style,
                            );
                        }
                        return;
                    }

                    let alt_text = preview.placeholder();
                    let x = inner.x + inner.width.saturating_sub(alt_text.len() as u16) / 2;
                    let y = inner.y + inner.height / 2;
                    surface.set_stringn(x, y, alt_text, inner.width as usize, text);
                    return;
                }
            };

            let mut offset = ViewPosition::default();
            if let Some((start_line, end_line)) = range {
                let height = end_line - start_line;
                let text = doc.text().slice(..);
                let start = text.line_to_char(start_line);
                let middle = text.line_to_char(start_line + height / 2);
                if height < inner.height as usize {
                    let text_fmt = doc.text_format(inner.width, None);
                    let annotations = TextAnnotations::default();
                    (offset.anchor, offset.vertical_offset) = char_idx_at_visual_offset(
                        text,
                        middle,
                        // align to middle
                        -(inner.height as isize / 2),
                        0,
                        &text_fmt,
                        &annotations,
                    );
                    if start < offset.anchor {
                        offset.anchor = start;
                        offset.vertical_offset = 0;
                    }
                } else {
                    offset.anchor = start;
                }
            }

            let loader = cx.editor.syn_loader.load();
            let config = cx.editor.config();

            let syntax_highlighter =
                EditorView::doc_syntax_highlighter(doc, offset.anchor, area.height, &loader);
            let mut overlay_highlights = Vec::new();
            if doc
                .language_config()
                .and_then(|config| config.rainbow_brackets)
                .unwrap_or(config.rainbow_brackets)
            {
                if let Some(overlay) = EditorView::doc_rainbow_highlights(
                    doc,
                    offset.anchor,
                    area.height,
                    &cx.editor.theme,
                    &loader,
                ) {
                    overlay_highlights.push(overlay);
                }
            }

            EditorView::doc_diagnostics_highlights_into(
                doc,
                &cx.editor.theme,
                offset.anchor,
                area.height,
                &mut overlay_highlights,
            );

            let mut decorations = DecorationManager::default();

            if let Some((start, end)) = range {
                let style = cx
                    .editor
                    .theme
                    .try_get("ui.highlight")
                    .unwrap_or_else(|| cx.editor.theme.get("ui.selection"));
                let draw_highlight = move |renderer: &mut TextRenderer, pos: LinePos| {
                    if (start..=end).contains(&pos.doc_line) {
                        let area = Rect::new(
                            renderer.viewport.x,
                            pos.visual_line,
                            renderer.viewport.width,
                            1,
                        );
                        renderer.set_style(area, style)
                    }
                };
                decorations.add_decoration(draw_highlight);
            }

            render_document(
                surface,
                inner,
                doc,
                offset,
                // TODO: compute text annotations asynchronously here (like inlay hints)
                &TextAnnotations::default(),
                syntax_highlighter,
                overlay_highlights,
                &cx.editor.theme,
                decorations,
            );
        }
    }
}

impl<I: 'static + Send + Sync, D: 'static + Send + Sync> Component for Picker<I, D> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // +---------+ +---------+
        // |prompt   | |preview  |
        // +---------+ |         |
        // |picker   | |         |
        // |         | |         |
        // +---------+ +---------+

        let render_preview =
            self.show_preview && self.file_fn.is_some() && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };

        let picker_area = area.with_width(picker_width);
        self.render_picker(picker_area, surface, cx);

        if render_preview {
            let preview_area = area.clip_left(picker_width);
            self.render_preview(preview_area, surface, cx);
        } else {
            self.cancel_preview_load();
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        // TODO: keybinds for scrolling preview

        let key_event = match event {
            Event::Key(event) => *event,
            Event::Paste(..) => return self.prompt_handle_event(event, ctx),
            Event::Resize(..) => return EventResult::Consumed(None),
            // Picker is a modal and should consume mouse events so clicks don't fall
            // through to the editor underneath
            Event::Mouse(_) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        let close_fn = |picker: &mut Self| {
            picker.background_task.cancel();
            picker
                .preview_version
                .fetch_add(1, atomic::Ordering::Relaxed);
            picker.preview_path = None;
            // if the picker is very large don't store it as last_picker to avoid
            // excessive memory consumption
            let callback: compositor::Callback =
                if picker.matcher.snapshot().item_count() > 1_000_000 {
                    Box::new(|compositor: &mut Compositor, _ctx| {
                        // remove the layer
                        compositor.pop();
                    })
                } else {
                    // stop streaming in new items in the background, really we should
                    // be restarting the stream somehow once the picker gets
                    // reopened instead (like for an FS crawl) that would also remove the
                    // need for the special case above but that is pretty tricky
                    picker.version.fetch_add(1, atomic::Ordering::Relaxed);
                    Box::new(|compositor: &mut Compositor, _ctx| {
                        // remove the layer
                        compositor.last_picker = compositor.pop();
                    })
                };
            EventResult::Consumed(Some(callback))
        };

        match key_event {
            shift!(Tab) | key!(Up) | ctrl!('p') => {
                self.move_by(1, Direction::Backward);
            }
            key!(Tab) | key!(Down) | ctrl!('n') => {
                self.move_by(1, Direction::Forward);
            }
            key!(PageDown) | ctrl!('d') => {
                self.page_down();
            }
            key!(PageUp) | ctrl!('u') => {
                self.page_up();
            }
            key!(Home) => {
                self.to_start();
            }
            key!(End) => {
                self.to_end();
            }
            key!(Esc) | ctrl!('c') => return close_fn(self),
            alt!(Enter) => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, self.default_action);
                }
            }
            key!(Enter) => {
                // If the prompt has a history completion and is empty, use enter to accept
                // that completion
                if let Some(completion) = self
                    .prompt
                    .first_history_completion(ctx.editor)
                    .filter(|_| self.prompt.line().is_empty())
                {
                    // The percent character is used by the query language and needs to be
                    // escaped with a backslash.
                    let completion = if completion.contains('%') {
                        completion.replace('%', "\\%")
                    } else {
                        completion.into_owned()
                    };
                    self.prompt.set_line(completion, ctx.editor);

                    // Inserting from the history register is a paste.
                    self.handle_prompt_change(true);
                } else {
                    if let Some(option) = self.selection() {
                        (self.callback_fn)(ctx, option, self.default_action);
                    }
                    if let Some(history_register) = self.prompt.history_register() {
                        if let Err(err) = ctx
                            .editor
                            .registers
                            .push(history_register, self.primary_query().to_string())
                        {
                            ctx.editor.set_error(err.to_string());
                        }
                    }
                    return close_fn(self);
                }
            }
            ctrl!('s') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::HorizontalSplit);
                }
                return close_fn(self);
            }
            ctrl!('v') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::VerticalSplit);
                }
                return close_fn(self);
            }
            ctrl!('t') => {
                self.toggle_preview();
            }
            _ => {
                self.prompt_handle_event(event, ctx);
            }
        }

        EventResult::Consumed(None)
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let block = Block::bordered();
        // calculate the inner area inside the box
        let inner = block.inner(area);

        // prompt area
        let render_preview =
            self.show_preview && self.file_fn.is_some() && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };
        let area = inner.clip_left(1).with_height(1).with_width(picker_width);

        self.prompt.cursor(area, editor)
    }

    fn required_size(&mut self, (width, height): (u16, u16)) -> Option<(u16, u16)> {
        self.completion_height = height.saturating_sub(4 + self.header_height());
        Some((width, height))
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}
impl<T: 'static + Send + Sync, D> Drop for Picker<T, D> {
    fn drop(&mut self) {
        // ensure we cancel any ongoing background threads streaming into the picker
        self.version.fetch_add(1, atomic::Ordering::Relaxed);
        self.preview_version.fetch_add(1, atomic::Ordering::Relaxed);
    }
}

type PickerCallback<T> = Box<dyn Fn(&mut Context, &T, Action)>;

#[cfg(test)]
mod preview_cache_tests {
    use arc_swap::{access::Map, ArcSwap};
    use helix_core::{syntax, Rope};
    use helix_view::theme;
    use tokio::sync::mpsc::{channel, Receiver};

    use super::*;

    fn path(index: usize) -> Arc<Path> {
        std::path::PathBuf::from(format!("/preview/{index}")).into()
    }

    fn editor() -> Editor {
        let config = Arc::new(ArcSwap::from_pointee(crate::config::Config::default()));
        let handlers = crate::handlers::setup_for_test(config.clone());
        Editor::new(
            Rect::new(0, 0, 80, 24),
            Arc::new(theme::Loader::new(&[])),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
            Arc::new(Map::new(config, |config: &crate::config::Config| {
                &config.editor
            })),
            handlers,
            helix_loader::workspace_trust::WorkspaceTrust::fully_trusted(),
        )
    }

    #[tokio::test]
    async fn cached_picker_rows_reuse_formatting_and_refresh_query_highlights() {
        let mut editor = editor();
        let formats = Arc::new(AtomicUsize::new(0));
        let options = || ["alpha", "alphabet", "beta"].map(String::from);
        let column = || {
            Column::new("name", |item: &String, formats: &Arc<AtomicUsize>| {
                formats.fetch_add(1, atomic::Ordering::Relaxed);
                item.as_str().into()
            })
            .cached()
        };
        let mut picker = Picker::new([column()], 0, options(), formats.clone(), |_, _, _| {});
        let mut jobs = crate::job::Jobs::new();
        let area = Rect::new(0, 0, 80, 24);
        let mut surface = Surface::empty(area);
        let mut cx = Context {
            editor: &mut editor,
            jobs: &mut jobs,
            scroll: None,
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while picker.matcher.snapshot().matched_item_count() != 3 {
            assert!(Instant::now() < deadline);
            picker.render_picker(area, &mut surface, &mut cx);
            tokio::task::yield_now().await;
        }
        picker.render_picker(area, &mut surface, &mut cx);
        let before = formats.load(atomic::Ordering::Relaxed);
        picker.move_by(1, Direction::Forward);
        picker.render_picker(area, &mut surface, &mut cx);
        assert_eq!(formats.load(atomic::Ordering::Relaxed), before);
        picker.prompt.set_line("alp".into(), cx.editor);
        picker.handle_prompt_change(false);
        while picker.matcher.snapshot().matched_item_count() != 2 {
            assert!(Instant::now() < deadline);
            picker.render_picker(area, &mut surface, &mut cx);
            tokio::task::yield_now().await;
        }
        picker.render_picker(area, &mut surface, &mut cx);
        assert_eq!(formats.load(atomic::Ordering::Relaxed), before);

        let mut fresh = Picker::new(
            [column()],
            0,
            options(),
            Arc::new(AtomicUsize::new(0)),
            |_, _, _| {},
        );
        fresh.prompt.set_line("alp".into(), cx.editor);
        fresh.handle_prompt_change(false);
        let mut expected = Surface::empty(area);
        while fresh.matcher.snapshot().matched_item_count() != 2 {
            assert!(Instant::now() < deadline);
            fresh.render_picker(area, &mut expected, &mut cx);
            tokio::task::yield_now().await;
        }
        fresh.render_picker(area, &mut expected, &mut cx);
        assert_eq!(surface, expected);
    }

    fn picker(path: &Path) -> (Picker<std::path::PathBuf, ()>, Receiver<PreviewRequest>) {
        let mut picker = Picker::new(
            [Column::new("path", |path: &std::path::PathBuf, _: &()| {
                Cell::from(path.to_string_lossy().into_owned())
            })],
            0,
            [path.to_path_buf()],
            (),
            |_, _, _| {},
        )
        .with_preview(|_, path| Some((path.as_path().into(), None)));
        picker.matcher.tick(1000);
        picker.matcher.tick(1000);
        assert!(picker.selection().is_some());
        let (sender, receiver) = channel(1);
        picker.preview_load_handler = sender;
        (picker, receiver)
    }

    #[test]
    fn canceled_multiline_search_stops_reading_before_any_match() {
        use std::io::Read;

        struct CancelAfterRead {
            version: Arc<AtomicUsize>,
            bytes_read: usize,
        }
        impl Read for CancelAfterRead {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                // A multiline search fills its input before producing matches.
                // Cancel during that fill, even for a file with no matches.
                assert!(buffer.len() <= 64 * 1024);
                buffer.fill(b'a');
                self.bytes_read += buffer.len();
                self.version.fetch_add(1, atomic::Ordering::Relaxed);
                Ok(buffer.len())
            }
        }
        let version = Arc::new(AtomicUsize::new(1));
        let cancellation = PickerCancellation {
            version: 1,
            current: version.clone(),
        };
        let mut source = CancelAfterRead {
            version,
            bytes_read: 0,
        };
        let matcher = grep_regex::RegexMatcherBuilder::new()
            .multi_line(true)
            .build("absent\\npattern")
            .unwrap();
        let mut searcher = grep_searcher::SearcherBuilder::new()
            .multi_line(true)
            .build();
        let result = searcher.search_reader(
            &matcher,
            cancellation.reader(&mut source),
            grep_searcher::sinks::UTF8(|_, _| panic!("canceled scan produced a match")),
        );
        assert!(result.is_err());
        assert!(source.bytes_read > 0);
        assert!(source.bytes_read <= 64 * 1024);
    }

    #[test]
    fn cancellable_rope_search_keeps_multiline_unicode_buffer_contents() {
        let cancellation = PickerCancellation {
            version: 1,
            current: Arc::new(AtomicUsize::new(1)),
        };
        let rope = Rope::from_str("disk differs\n😀 unsaved\nβ suffix\n");
        let matcher = grep_regex::RegexMatcherBuilder::new()
            .multi_line(true)
            .build("😀 unsaved\\nβ")
            .unwrap();
        let mut searcher = grep_searcher::SearcherBuilder::new()
            .multi_line(true)
            .build();
        let mut found = Vec::new();
        searcher
            .search_reader(
                &matcher,
                cancellation.reader(helix_core::RopeReader::new(rope.slice(..))),
                grep_searcher::sinks::UTF8(|line, text| {
                    found.push((line, text.to_owned()));
                    Ok(true)
                }),
            )
            .unwrap();
        assert_eq!(found, vec![(2, "😀 unsaved\nβ suffix\n".to_owned())]);
    }

    #[tokio::test]
    async fn picker_injector_can_finish_on_a_plain_blocking_thread() {
        let (picker, _) = picker(Path::new("/preview/blocking-injector"));
        let injector = picker.injector();
        std::thread::spawn(move || drop(injector)).join().unwrap();
    }

    #[tokio::test]
    async fn closing_an_open_document_starts_a_preview_for_the_same_selected_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preview.txt");
        let mut editor = editor();
        let id = editor.new_file(Action::VerticalSplit);
        editor.documents.get_mut(&id).unwrap().set_path(Some(&path));
        let (mut picker, mut requests) = picker(&path);
        assert!(matches!(
            picker.get_preview(&editor),
            Some((Preview::EditorDocument(_), _))
        ));
        assert!(requests.try_recv().is_err());
        assert!(picker.preview_pending.is_none());
        assert!(editor.close_document(id, true).is_ok());
        assert!(matches!(
            picker.get_preview(&editor),
            Some((Preview::Loading, _))
        ));
        let request = requests.try_recv().unwrap();
        assert_eq!(request.path.as_ref(), path);
        assert_eq!(
            picker.preview_pending.as_ref().unwrap().generation,
            request.generation
        );
        for _ in 0..8 {
            picker.get_preview(&editor);
        }
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn failed_preview_load_retries_once_after_a_cooldown() {
        let editor = editor();
        let (mut picker, mut requests) = picker(Path::new("/preview/retry"));
        picker.get_preview(&editor);
        let first = requests.try_recv().unwrap();
        for _ in 0..8 {
            picker.get_preview(&editor);
        }
        assert!(requests.try_recv().is_err());

        // A blocking-worker JoinError reports failure through this request's
        // status without mutating a newer selection's pending state.
        first.status.fail();
        for _ in 0..8 {
            picker.get_preview(&editor);
        }
        assert!(picker.preview_retry_at.is_some());
        assert!(requests.try_recv().is_err());
        picker.preview_retry_at = Some(Instant::now() - Duration::from_secs(1));
        picker.get_preview(&editor);
        let retry = requests.try_recv().unwrap();
        assert_ne!(first.generation, retry.generation);
        assert_eq!(
            retry.version.load(atomic::Ordering::Relaxed),
            retry.generation
        );
        for _ in 0..8 {
            picker.get_preview(&editor);
        }
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_dropped_preview_request_can_retry_without_a_frame_by_frame_send_loop() {
        let editor = editor();
        let (mut picker, mut requests) = picker(Path::new("/preview/full"));
        picker.get_preview(&editor);
        // Leave the first event in the bounded channel and simulate its worker
        // failing. The attempted replacement cannot be enqueued yet.
        picker.preview_pending.as_ref().unwrap().status.fail();
        picker.get_preview(&editor);
        picker.preview_retry_at = Some(Instant::now() - Duration::from_secs(1));
        picker.get_preview(&editor);
        assert!(picker.preview_pending.as_ref().unwrap().status.failed());
        let discarded = requests.try_recv().unwrap();
        for _ in 0..8 {
            picker.get_preview(&editor);
        }
        assert!(requests.try_recv().is_err());
        picker.preview_retry_at = Some(Instant::now() - Duration::from_secs(1));
        picker.get_preview(&editor);
        let retry = requests.try_recv().unwrap();
        assert_ne!(discarded.generation, retry.generation);
        assert!(!retry.status.failed());
    }

    #[test]
    fn preview_cache_evicts_least_recently_used_entries() {
        let mut cache = PreviewCache::default();
        for index in 0..MAX_CACHED_PREVIEWS {
            cache.insert(path(index), CachedPreview::Binary);
        }
        assert!(cache.get(&path(0)).is_some());
        cache.insert(path(MAX_CACHED_PREVIEWS), CachedPreview::LargeFile);
        assert_eq!(cache.entries.len(), MAX_CACHED_PREVIEWS);
        assert!(cache.contains_key(&path(0)));
        assert!(!cache.contains_key(&path(1)));
        assert!(cache.contains_key(&path(MAX_CACHED_PREVIEWS)));
    }

    #[test]
    fn preview_cache_bounds_bytes_and_accounts_for_replacements() {
        let mut cache = PreviewCache::default();
        for index in 0..8 {
            let name = String::with_capacity(8 * 1024 * 1024);
            cache.insert(path(index), CachedPreview::Directory(vec![(name, false)]));
            assert!(cache.bytes <= MAX_CACHED_PREVIEW_BYTES);
            assert!(cache.contains_key(&path(index)));
        }
        assert!(!cache.contains_key(&path(0)));
        let old_bytes = cache.bytes;
        cache.insert(path(7), CachedPreview::NotFound);
        assert!(cache.bytes < old_bytes);
        assert_eq!(
            cache.bytes,
            cache
                .entries
                .values()
                .map(|entry| entry.bytes)
                .sum::<usize>()
        );
    }

    #[test]
    fn oversized_single_directory_preview_is_truncated_with_a_hard_byte_limit() {
        let mut cache = PreviewCache::default();
        cache.insert(path(0), CachedPreview::Binary);
        cache.insert(
            path(1),
            CachedPreview::Directory(vec![
                ("visible.txt".into(), false),
                (String::with_capacity(MAX_CACHED_PREVIEW_BYTES + 1), false),
            ]),
        );
        let CachedPreview::Directory(entries) = cache.get(&path(1)).unwrap() else {
            panic!("large directory previews must remain readable");
        };
        assert_eq!(entries[0].0, "visible.txt");
        assert_eq!(entries.last().unwrap().0, TRUNCATED_DIRECTORY_PREVIEW);
        assert_eq!(entries.capacity(), entries.len());
        assert!(cache.bytes <= MAX_CACHED_PREVIEW_BYTES);
        assert!(cache.entries[&path(1)].bytes <= MAX_DIRECTORY_PREVIEW_BYTES);
    }

    #[tokio::test]
    async fn an_oversized_decoded_document_is_not_retained_in_the_cache() {
        let editor = editor();
        let doc = Document::from(
            Rope::from_str(&"x".repeat(MAX_CACHED_PREVIEW_BYTES + 1)),
            None,
            editor.config.clone(),
            editor.syn_loader.clone(),
        );
        let mut cache = PreviewCache::default();
        cache.insert(path(0), CachedPreview::Document(Box::new(doc)));
        assert!(matches!(
            cache.get(&path(0)),
            Some(CachedPreview::LargeFile)
        ));
        assert!(cache.bytes <= MAX_CACHED_PREVIEW_BYTES);
    }
}
