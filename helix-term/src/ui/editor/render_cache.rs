use std::{collections::VecDeque, sync::Arc};

use helix_core::{syntax, text_annotations::TextAnnotations, Position};
use helix_view::{
    document::Mode,
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

#[derive(Default)]
pub(super) struct ViewRenderCache {
    entries: VecDeque<Entry>,
    configs: VecDeque<Arc<Config>>,
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
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.view == view && entry.key == *key)
        else {
            return false;
        };
        let entry = self.entries.remove(index).unwrap();
        copy_region(&entry.cells, surface, entry.cells.area);
        if focused {
            cursor.set(entry.cursor);
        }
        self.entries.push_back(entry);
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
        const MAX_BYTES: usize = 8 * 1024 * 1024;
        let bytes = usize::from(area.width)
            * usize::from(area.height)
            * std::mem::size_of::<tui::buffer::Cell>();
        self.entries.retain(|entry| entry.view != view);
        if bytes > MAX_BYTES {
            return;
        }
        while self.entries.len() >= 8
            || self
                .entries
                .iter()
                .map(|entry| entry.cells.content.len() * std::mem::size_of::<tui::buffer::Cell>())
                .sum::<usize>()
                + bytes
                > MAX_BYTES
        {
            self.entries.pop_front();
        }
        let mut cells = Buffer::empty(area);
        copy_region(surface, &mut cells, area);
        self.entries.push_back(Entry {
            view,
            key,
            cells,
            cursor,
        });
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
mod tests {
    use super::*;
    use crate::{application::Application, args::Args, config, keymap::Keymaps};
    use helix_core::{text_annotations::InlineAnnotation, Selection, Transaction};
    use helix_view::{
        document::{DocumentInlayHints, DocumentInlayHintsId},
        editor::Action,
    };

    fn application() -> anyhow::Result<Application> {
        let mut config = config::Config::default();
        config.editor.lsp.enable = false;
        config.editor.word_completion.enable = false;
        config.editor.lsp.auto_document_highlight = true;
        config.editor.lsp.display_color_swatches = false;
        config.editor.lsp.display_color_values = true;
        Application::new(
            Args::default(),
            config,
            syntax::Loader::default(),
            helix_loader::workspace_trust::WorkspaceTrust::fully_trusted(),
        )
    }

    fn assert_matches_fresh(component: &EditorView, editor: &Editor) {
        let area = Rect::new(0, 0, 80, 24);
        let mut cached = Buffer::empty(area);
        let mut fresh = Buffer::empty(area);
        let reference = EditorView::new(Keymaps::default());
        cached.set_style(area, editor.theme.get("ui.background"));
        fresh.set_style(area, editor.theme.get("ui.background"));
        for (view, focused) in editor.tree.views() {
            let doc = editor.document(view.doc).unwrap();
            editor.cursor_cache.reset();
            component.render_view(editor, doc, view, area, &mut cached, focused);
            let cached_cursor = focused.then(|| editor.cursor_cache.get(view, doc));
            editor.cursor_cache.reset();
            reference.render_view(editor, doc, view, area, &mut fresh, focused);
            assert_eq!(
                cached_cursor,
                focused.then(|| editor.cursor_cache.get(view, doc))
            );
        }
        assert_eq!(cached, fresh);
    }

    #[tokio::test]
    async fn cached_cells_follow_edits_selection_scroll_annotations_and_theme() -> anyhow::Result<()>
    {
        let mut app = application()?;
        let editor = &mut app.editor;
        editor.resize(Rect::new(0, 0, 80, 23));
        editor.theme = toml::from_str(
            r##"
            "ui.background" = { bg = "#202020" }
            "ui.text" = { fg = "#eeeeee" }
            "ui.selection" = { bg = "#303030" }
            "ui.cursor" = { bg = "#ffffff" }
            "ui.highlight" = { fg = "#00ff00" }
            "markup.link.url" = { fg = "#ff0000" }
        "##,
        )?;
        let view = editor.tree.focus;
        let doc_id = editor.tree.get(view).doc;
        let doc = editor.document_mut(doc_id).unwrap();
        let transaction = Transaction::change(
            doc.text(),
            [(
                0,
                doc.text().len_chars(),
                Some("alpha beta\n    gamma delta\nlast\n".into()),
            )]
            .into_iter(),
        );
        doc.apply(&transaction, view);
        let component = EditorView::new(Keymaps::default());
        assert_matches_fresh(&component, editor);
        assert_matches_fresh(&component, editor);
        assert_eq!(component.render_cache.borrow().entries.len(), 1);

        editor
            .document_mut(doc_id)
            .unwrap()
            .set_selection(view, Selection::point(7));
        assert_matches_fresh(&component, editor);
        editor.document_mut(doc_id).unwrap().set_view_offset(
            view,
            ViewPosition {
                anchor: 11,
                horizontal_offset: 1,
                vertical_offset: 0,
            },
        );
        assert_matches_fresh(&component, editor);
        let doc = editor.document_mut(doc_id).unwrap();
        let mut hints = DocumentInlayHints::empty_with_id(DocumentInlayHintsId {
            first_line: 0,
            last_line: 4,
            version: doc.version(),
            server_id: Default::default(),
            length_limit: None,
        });
        hints
            .type_inlay_hints
            .push(InlineAnnotation::new(20, ": hint"));
        doc.set_inlay_hints(view, hints);
        assert_matches_fresh(&component, editor);
        let doc = editor.document_mut(doc_id).unwrap();
        let transaction =
            Transaction::change(doc.text(), [(15, 20, Some("changed".into()))].into_iter());
        doc.apply(&transaction, view);
        assert_matches_fresh(&component, editor);
        editor
            .document_mut(doc_id)
            .unwrap()
            .set_document_highlights(view, std::iter::once(15..22).collect());
        assert_matches_fresh(&component, editor);
        editor
            .document_mut(doc_id)
            .unwrap()
            .clear_document_highlights(view);
        assert_matches_fresh(&component, editor);
        for color in [
            helix_view::Theme::rgb_highlight(255, 0, 0),
            helix_view::Theme::rgb_highlight(0, 0, 255),
        ] {
            editor.document_mut(doc_id).unwrap().color_swatches =
                Some(helix_view::document::DocumentColorSwatches {
                    color_ranges: Arc::new(vec![(color, 15..22)]),
                    ..Default::default()
                });
            assert_matches_fresh(&component, editor);
        }
        let doc = editor.document_mut(doc_id).unwrap();
        let mut config = (*doc.config.load()).clone();
        config.whitespace = toml::from_str("render = 'all'")?;
        doc.config = Arc::new(arc_swap::ArcSwap::from_pointee(config));
        assert_matches_fresh(&component, editor);
        editor.theme = toml::from_str(
            "\"ui.selection\" = { bg = \"#0000ff\" }\n\"ui.text\" = { fg = \"#00ff00\" }",
        )?;
        assert_matches_fresh(&component, editor);
        // A fresh target buffer models a removed popup: cached cells must fill it.
        assert_matches_fresh(&component, editor);
        Ok(())
    }

    #[tokio::test]
    async fn split_views_reuse_content_and_refresh_statuslines_independently() -> anyhow::Result<()>
    {
        let mut app = application()?;
        let editor = &mut app.editor;
        editor.resize(Rect::new(0, 0, 80, 23));
        let first = editor.tree.focus;
        editor.new_file(Action::VerticalSplit);
        let component = EditorView::new(Keymaps::default());
        assert_matches_fresh(&component, editor);
        assert_matches_fresh(&component, editor);
        assert_eq!(component.render_cache.borrow().entries.len(), 2);
        editor.tree.focus = first;
        assert_matches_fresh(&component, editor);
        editor.close(first);
        component
            .render_cache
            .borrow_mut()
            .retain(|id| editor.tree.try_get(id).is_some());
        assert_matches_fresh(&component, editor);
        assert_eq!(component.render_cache.borrow().entries.len(), 1);
        Ok(())
    }
}
