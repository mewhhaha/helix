use std::{collections::VecDeque, sync::Arc};

use helix_core::{syntax, text_annotations::TextAnnotations, Position};
use helix_stdx::cache::BoundedCache;
use helix_view::{
    document::{Mode, ReviewKey},
    editor::{Config, CursorCache, GutterConfig},
    graphics::Rect,
    view::ViewPosition,
    Document, DocumentId, Editor, View, ViewId,
};
use tui::buffer::Buffer;

use super::EditorView;

type ColorRanges = Vec<(syntax::Highlight, std::ops::Range<usize>)>;

struct Identity<T>(Arc<T>);
impl<T> PartialEq for Identity<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl<T> Eq for Identity<T> {}

#[derive(PartialEq, Eq)]
pub(super) struct RenderKey {
    document: DocumentId,
    version: i32,
    selection: u64,
    position: ViewPosition,
    area: Rect,
    viewport: Rect,
    focused: bool,
    terminal_focused: bool,
    graphics_cursor: bool,
    mode: Mode,
    theme: usize,
    editor_config: Identity<Config>,
    document_config: Identity<Config>,
    language: Option<Identity<syntax::config::LanguageConfiguration>>,
    loader: Identity<syntax::Loader>,
    scopes: Identity<Vec<String>>,
    annotations: u64,
    diagnostics: u64,
    diagnostic_servers: Vec<helix_lsp::LanguageServerId>,
    cursorline_diagnostics: bool,
    links: Identity<Vec<std::ops::Range<usize>>>,
    references: Option<Identity<Vec<std::ops::Range<usize>>>>,
    colors: Option<Identity<ColorRanges>>,
    code_actions: bool,
    diff: Option<(u64, bool)>,
    review_diff: Option<ReviewKey>,
    diff_mode: bool,
    diff_cursor: Option<helix_view::annotations::diff::DiffCursor>,
    comment_cursor: Option<helix_view::annotations::diff::CommentCursor>,
    comments_generation: u64,
    gutters: GutterConfig,
    tab_width: usize,
    indent_width: usize,
    text_width: usize,
    syntax_enabled: bool,
}

impl RenderKey {
    pub(super) fn new(
        component: &EditorView,
        editor: &Editor,
        doc: &Document,
        view: &View,
        viewport: Rect,
        focused: bool,
        annotations: &TextAnnotations,
    ) -> Option<Self> {
        // These decorations can depend on mutable debugger/snippet state that
        // has no published revision. Keep their ordinary rendering path.
        if doc.active_snippet.is_some()
            || editor.debug_adapters.get_active_client().is_some()
            || !editor.breakpoints.is_empty()
        {
            return None;
        }
        let editor_config = editor.config();
        let document_config = doc.config.load();
        let mut cache = component.render_cache.borrow_mut();
        Some(Self {
            document: doc.id(),
            version: doc.version(),
            selection: doc.selection_generation(view.id),
            position: doc.view_offset(view.id),
            area: view.area,
            viewport,
            focused,
            terminal_focused: component.terminal_focused,
            graphics_cursor: component.graphics_cursor,
            mode: editor.mode(),
            theme: editor.theme.cache_key(),
            editor_config: Identity(cache.config(&editor_config)),
            document_config: Identity(cache.config(&document_config)),
            language: doc.language.clone().map(Identity),
            loader: Identity(Arc::clone(&editor.syn_loader.load())),
            scopes: Identity(Arc::clone(&editor.syn_loader.load().scopes())),
            annotations: annotations.layout_key(),
            diagnostics: doc.diagnostics_generation(),
            diagnostic_servers: if doc.diagnostics().is_empty() {
                Vec::new()
            } else {
                doc.language_servers_with_feature(
                    syntax::config::LanguageServerFeature::Diagnostics,
                )
                .map(|server| server.id())
                .collect()
            },
            cursorline_diagnostics: view
                .diagnostics_handler
                .show_cursorline_diagnostics(doc, view.id),
            links: Identity(doc.document_link_ranges().clone()),
            references: doc
                .document_highlight_ranges(view.id)
                .cloned()
                .map(Identity),
            colors: doc
                .color_swatches
                .as_ref()
                .map(|colors| Identity(colors.color_ranges.clone())),
            code_actions: doc.code_action_hints(view.id),
            diff: doc.diff_handle().map(|diff| diff.render_key()),
            diff_mode: view.diff_mode.enabled(),
            diff_cursor: view.diff_mode.cursor(doc, view.id).cloned(),
            comment_cursor: view.diff_mode.comment_cursor(doc, view.id).cloned(),
            comments_generation: doc.review_comments_generation(),
            review_diff: view
                .diff_mode
                .enabled()
                .then(|| doc.review_diff_key())
                .flatten(),
            gutters: view.gutters.clone(),
            tab_width: doc.tab_width(),
            indent_width: doc.indent_style.indent_width(doc.tab_width()),
            text_width: doc.text_width(),
            syntax_enabled: doc.syntax().is_some(),
        })
    }
}

struct Entry {
    view: ViewId,
    key: RenderKey,
    cells: Buffer,
    cursor: Option<Position>,
}

pub(super) struct ViewRenderCache {
    entries: BoundedCache<Entry>,
    configs: VecDeque<Arc<Config>>,
}

impl Default for ViewRenderCache {
    fn default() -> Self {
        Self {
            entries: BoundedCache::new(8, 8 * 1024 * 1024),
            configs: VecDeque::new(),
        }
    }
}

impl ViewRenderCache {
    fn config(&mut self, config: &Config) -> Arc<Config> {
        if let Some(cached) = self.configs.iter().find(|cached| ***cached == *config) {
            return cached.clone();
        }
        let config = Arc::new(config.clone());
        if self.configs.len() == 8 {
            self.configs.pop_front();
        }
        self.configs.push_back(config.clone());
        config
    }

    pub(super) fn retain(&mut self, mut open: impl FnMut(ViewId) -> bool) {
        self.entries.retain(|entry| open(entry.view));
    }

    pub(super) fn restore(
        &mut self,
        view: ViewId,
        key: &RenderKey,
        surface: &mut Buffer,
        cursor: &CursorCache,
        focused: bool,
    ) -> bool {
        let Some(entry) = self
            .entries
            .get(|entry| entry.view == view && entry.key == *key)
        else {
            return false;
        };
        copy_region(&entry.cells, surface, entry.cells.area);
        if focused {
            cursor.set(entry.cursor);
        }
        true
    }

    pub(super) fn store(
        &mut self,
        view: ViewId,
        key: RenderKey,
        surface: &Buffer,
        area: Rect,
        cursor: Option<Position>,
    ) {
        let bytes = usize::from(area.width)
            .saturating_mul(usize::from(area.height))
            .saturating_mul(std::mem::size_of::<tui::buffer::Cell>());
        self.entries.retain(|entry| entry.view != view);
        if !self.entries.admits(bytes) {
            return;
        }
        let mut cells = Buffer::empty(area);
        copy_region(surface, &mut cells, area);
        self.entries.insert(
            Entry {
                view,
                key,
                cells,
                cursor,
            },
            bytes,
        );
    }
}

fn copy_region(source: &Buffer, target: &mut Buffer, area: Rect) {
    let area = area.intersection(source.area).intersection(target.area);
    if area.width == 0 {
        return;
    }
    for y in area.top()..area.bottom() {
        let source_start = source.index_of(area.x, y);
        let target_start = target.index_of(area.x, y);
        target.content[target_start..target_start + area.width as usize]
            .clone_from_slice(&source.content[source_start..source_start + area.width as usize]);
    }
}

#[cfg(all(test, feature = "integration"))]
mod tests;
