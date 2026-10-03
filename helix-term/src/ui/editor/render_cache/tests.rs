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
async fn cached_cells_follow_edits_selection_scroll_annotations_and_theme() -> anyhow::Result<()> {
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
        helix_view::Theme::rgb_background_highlight(255, 0, 0),
        helix_view::Theme::rgb_background_highlight(0, 0, 255),
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
async fn split_views_reuse_content_and_refresh_statuslines_independently() -> anyhow::Result<()> {
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

fn typed(editor: &mut Editor, name: &str, arguments: &str) -> anyhow::Result<()> {
    use crate::{
        commands::typed::TYPABLE_COMMAND_MAP, compositor::Context, job::Jobs, ui::PromptEvent,
    };
    let command = TYPABLE_COMMAND_MAP.get(name).unwrap();
    let args = helix_core::command_line::Args::parse(arguments, command.signature, true, |token| {
        Ok(token.content)
    })
    .map_err(|err| anyhow::anyhow!("{err}"))?;
    let mut jobs = Jobs::new();
    (command.fun)(
        &mut Context {
            editor,
            jobs: &mut jobs,
            scroll: None,
        },
        args,
        PromptEvent::Validate,
    )
}

async fn diff_application(base: &str, text: &str) -> anyhow::Result<Application> {
    let mut app = application()?;
    let mut config = (*app.editor.config()).clone();
    config.lsp.display_color_swatches = true;
    config.lsp.display_color_values = false;
    app.handle_config_events(helix_view::editor::ConfigEvent::Update(Box::new(config)));
    app.editor.resize(Rect::new(0, 0, 80, 23));
    app.editor.theme = toml::from_str(
        r##"
        "ui.background" = { bg = "#202020" }
        "ui.text" = { fg = "#eeeeee" }
        "ui.diff.added" = { bg = "#204020" }
        "ui.diff.deleted" = { bg = "#402020" }
        "ui.selection" = { bg = "#303030" }
        "ui.cursor" = { bg = "#ffffff" }
    "##,
    )?;
    let view = app.editor.tree.focus;
    let doc_id = app.editor.tree.get(view).doc;
    let doc = app.editor.document_mut(doc_id).unwrap();
    let transaction = Transaction::change(
        doc.text(),
        [(0, doc.text().len_chars(), Some(text.into()))].into_iter(),
    );
    assert!(doc.apply(&transaction, view));
    doc.set_selection(view, Selection::point(0));
    doc.set_diff_base(base.as_bytes().to_vec());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let ready = {
                let diff = doc.diff_handle().unwrap().load();
                diff.render_key().0 != 0 && diff.doc().is_instance(doc.text())
            };
            if ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await?;
    Ok(app)
}

fn rendered(component: &EditorView, editor: &Editor) -> Buffer {
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    buffer.set_style(area, editor.theme.get("ui.background"));
    let view = editor.tree.get(editor.tree.focus);
    component.render_view(
        editor,
        editor.document(view.doc).unwrap(),
        view,
        area,
        &mut buffer,
        true,
    );
    buffer
}

fn diff_language(editor: &mut Editor, name: &str) -> anyhow::Result<Arc<syntax::Loader>> {
    let config = toml::from_str(
        r#"
        [[language]]
        name = "javascript"
        scope = "source.js"
        file-types = ["js"]
        [[language]]
        name = "css"
        scope = "source.css"
        injection-regex = "css"
        file-types = ["css"]
    "#,
    )?;
    let loader = Arc::new(syntax::Loader::new(config)?);
    editor.theme = toml::from_str(
        r##"
        "ui.background" = { bg = "#202020" }
        "ui.text" = { fg = "#eeeeee" }
        "ui.diff.added" = { bg = "#204020" }
        "ui.diff.deleted" = { bg = "#402020" }
        "ui.selection" = { bg = "#303030" }
        "ui.cursor" = { bg = "#ffffff" }
        "string" = "#aa3377"
        "keyword" = "#eeaa00"
        "comment" = "#889988"
        "variable.other.member" = "#22cc99"
    "##,
    )?;
    loader.set_scopes(editor.theme.scopes().to_vec());
    editor.syn_loader.store(loader.clone());
    let language = loader.language_for_name(name).unwrap();
    let doc_id = editor.tree.get(editor.tree.focus).doc;
    let doc = editor.document_mut(doc_id).unwrap();
    doc.set_language(Some(loader.language(language).config().clone()), &loader);
    assert!(doc.syntax().is_some());
    Ok(loader)
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_syntax_keeps_original_multiline_context_and_selection_backgrounds(
) -> anyhow::Result<()> {
    use crate::commands::MappableCommand as Command;
    use helix_view::graphics::Color;
    for (base, text, color) in [
        (
            "const before = `start\nconst value = 1;\nend`;\nconst same = 0;\n",
            "const before = `start\nend`;\nconst value = 2;\nconst same = 0;\n",
            Color::Rgb(170, 51, 119),
        ),
        (
            "/* begin\nconst value = 1;\n*/\nconst same = 0;\n",
            "/* begin\n*/\nconst value = 2;\nconst same = 0;\n",
            Color::Rgb(136, 153, 136),
        ),
    ] {
        let mut app = diff_application(base, text).await?;
        let editor = &mut app.editor;
        let id = editor.tree.focus;
        let doc_id = editor.tree.get(id).doc;
        diff_language(editor, "javascript")?;
        typed(editor, "review-mode", "on")?;
        let component = EditorView::new(Keymaps::default());
        let inner = editor
            .tree
            .get(id)
            .inner_area(editor.document(doc_id).unwrap());
        let buffer = rendered(&component, editor);
        assert_eq!(row(&buffer, 1, inner.x), "const value = 1;");
        assert_eq!(row(&buffer, 3, inner.x), "const value = 2;");
        assert_eq!(buffer[(inner.x + 1, 1)].fg, color);
        assert_eq!(buffer[(inner.x + 1, 3)].fg, Color::Rgb(238, 170, 0));
        for x in 0..inner.right() {
            assert_eq!(buffer[(x, 1)].bg, Color::Rgb(64, 32, 32));
            assert_eq!(buffer[(x, 3)].bg, Color::Rgb(32, 64, 32));
        }
        assert_matches_fresh(&component, editor);
        command(editor, Command::goto_first_diag);
        command(editor, Command::select_mode);
        command(editor, Command::extend_next_word_end);
        let buffer = rendered(&component, editor);
        assert_eq!(register_text(editor, '.'), "const");
        assert_eq!(buffer[(inner.x + 1, 1)].fg, color);
        assert_eq!(buffer[(inner.x + 1, 1)].bg, Color::Rgb(48, 48, 48));
        assert_eq!(buffer[(inner.x + 7, 1)].bg, Color::Rgb(64, 32, 32));
        assert_eq!(editor.document(doc_id).unwrap().text().to_string(), text);
        assert_matches_fresh(&component, editor);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_syntax_preserves_injections_reuses_the_base_and_invalidates_on_changes(
) -> anyhow::Result<()> {
    use helix_view::graphics::Color;
    let base = "const style = css`\n.old { color: red; }\n`;\nconst same = 0;\n";
    let text = "const style = css`\n.new { color: blue; }\n`;\nconst same = 0;\n";
    let mut app = diff_application(base, text).await?;
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    let loader = diff_language(editor, "javascript")?;
    typed(editor, "review-mode", "on")?;
    let view = editor.tree.get(id);
    let doc = editor.document(doc_id).unwrap();
    let display = view.diff_mode.display(doc).unwrap();
    let syntax = view.diff_mode.base_syntax(doc, &display, &loader).unwrap();
    let byte = base.find("color").unwrap() as u32;
    assert!(syntax.layers_for_byte_range(byte, byte + 1).count() > 1);
    assert!(Arc::ptr_eq(
        &syntax,
        &view.diff_mode.base_syntax(doc, &display, &loader).unwrap()
    ));
    let component = EditorView::new(Keymaps::default());
    let inner = view.inner_area(doc);
    let buffer = rendered(&component, editor);
    assert_eq!(row(&buffer, 1, inner.x), ".old { color: red; }");
    assert_eq!(row(&buffer, 2, inner.x), ".new { color: blue; }");
    assert_eq!(buffer[(inner.x + 7, 1)].fg, Color::Rgb(34, 204, 153));
    assert_eq!(buffer[(inner.x + 7, 2)].fg, Color::Rgb(34, 204, 153));
    assert_eq!(buffer[(inner.x + 7, 1)].bg, Color::Rgb(64, 32, 32));
    assert_eq!(buffer[(inner.x + 7, 2)].bg, Color::Rgb(32, 64, 32));
    assert_matches_fresh(&component, editor);

    // Editing the current file must not reparse the unchanged original file.
    typed(editor, "review-mode", "off")?;
    let doc = editor.document_mut(doc_id).unwrap();
    let start = doc.text().len_chars() - 3;
    let change = Transaction::change(
        doc.text(),
        [(start, start + 1, Some("1".into()))].into_iter(),
    );
    assert!(doc.apply(&change, id));
    typed(editor, "review-mode", "on")?;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while editor
            .tree
            .get(id)
            .diff_mode
            .display(editor.document(doc_id).unwrap())
            .is_none()
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await?;
    let doc = editor.document(doc_id).unwrap();
    let display = editor.tree.get(id).diff_mode.display(doc).unwrap();
    assert!(Arc::ptr_eq(
        &syntax,
        &editor
            .tree
            .get(id)
            .diff_mode
            .base_syntax(doc, &display, &loader)
            .unwrap()
    ));
    assert_matches_fresh(&component, editor);

    let generation = doc.diff_handle().unwrap().render_key();
    editor
        .document_mut(doc_id)
        .unwrap()
        .set_diff_base(format!("// original\n{base}").into_bytes());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while editor
            .document(doc_id)
            .unwrap()
            .diff_handle()
            .unwrap()
            .render_key()
            == generation
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await?;
    let doc = editor.document(doc_id).unwrap();
    let display = editor.tree.get(id).diff_mode.display(doc).unwrap();
    let changed = editor
        .tree
        .get(id)
        .diff_mode
        .base_syntax(doc, &display, &loader)
        .unwrap();
    assert!(!Arc::ptr_eq(&syntax, &changed));
    let new_loader = diff_language(editor, "css")?;
    let doc = editor.document(doc_id).unwrap();
    let display = editor.tree.get(id).diff_mode.display(doc).unwrap();
    let changed_language = editor
        .tree
        .get(id)
        .diff_mode
        .base_syntax(doc, &display, &new_loader)
        .unwrap();
    assert!(!Arc::ptr_eq(&changed, &changed_language));
    assert_eq!(
        changed_language.root_language(),
        new_loader.language_for_name("css").unwrap()
    );
    assert_matches_fresh(&component, editor);
    editor
        .document_mut(doc_id)
        .unwrap()
        .set_language(None, &new_loader);
    assert!(editor
        .tree
        .get(id)
        .diff_mode
        .base_syntax(editor.document(doc_id).unwrap(), &display, &new_loader)
        .is_none());
    assert_matches_fresh(&component, editor);
    Ok(())
}

fn row(buffer: &Buffer, y: u16, x: u16) -> String {
    let mut x = x;
    let mut text = String::new();
    while x < buffer.area.right() {
        let cell = &buffer[(x, y)];
        text.push_str(&cell.symbol);
        x += cell.width().max(1) as u16;
    }
    text.trim_end().to_owned()
}

fn command(editor: &mut Editor, command: crate::commands::MappableCommand) {
    command_with_count(editor, command, None);
}

fn command_with_count(
    editor: &mut Editor,
    command: crate::commands::MappableCommand,
    count: Option<std::num::NonZeroUsize>,
) {
    let mut jobs = crate::job::Jobs::new();
    command.execute(&mut crate::commands::Context {
        editor,
        jobs: &mut jobs,
        register: None,
        count,
        callback: Vec::new(),
        on_next_key_callback: None,
    });
    editor.ensure_cursor_in_view(editor.tree.focus);
}

fn register_text(editor: &Editor, name: char) -> String {
    editor
        .registers
        .read(name, editor)
        .unwrap()
        .collect::<Vec<_>>()
        .join("")
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_source_rows_reject_edits_but_keep_navigation_selection_and_yanking(
) -> anyhow::Result<()> {
    use crate::commands::MappableCommand as Command;

    let mut app = diff_application("same\nold\nlast\n", "same\nnew\nlast\n").await?;
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    {
        let (view, doc) = helix_view::current!(editor);
        doc.append_changes_to_history(view);
    }
    let original = editor.document(doc_id).unwrap().text().clone();
    let version = editor.document(doc_id).unwrap().version();
    editor
        .registers
        .write('"', vec!["preserve register".into()])?;
    typed(editor, "review-mode", "on")?;
    for line in [0, 1, 2] {
        let doc = editor.document_mut(doc_id).unwrap();
        doc.set_selection(id, Selection::point(doc.text().line_to_char(line)));
        for edit in [
            Command::insert_mode,
            Command::append_mode,
            Command::open_above,
            Command::open_below,
            Command::delete_selection,
            Command::change_selection,
            Command::replace,
            Command::paste_after,
            Command::indent,
            Command::toggle_comments,
            Command::undo,
            Command::redo,
            Command::rename_symbol,
            Command::code_action,
            Command::format_selections,
            Command::shell_pipe,
        ] {
            editor.clear_status();
            command(editor, edit);
            assert!(editor.is_err());
            assert_eq!(editor.mode, Mode::Normal);
            let doc = editor.document(doc_id).unwrap();
            assert!(doc.text().is_instance(&original));
            assert_eq!(doc.version(), version);
            assert_eq!(register_text(editor, '"'), "preserve register");
        }
    }
    for edit in [
        ":sort",
        ":reflow",
        ":format",
        ":diffget",
        ":read missing.txt",
        ":line-ending crlf",
        ":encoding windows-1252",
        ":write",
        ":write!",
        ":write-all",
        ":write-all!",
        ":reload",
        ":earlier",
        ":later",
        ":insert-output printf changed",
    ] {
        editor.clear_status();
        command(editor, edit.parse()?);
        assert!(editor.is_err(), "{edit}");
        assert!(
            editor
                .document(doc_id)
                .unwrap()
                .text()
                .is_instance(&original),
            "{edit}"
        );
        assert_eq!(
            editor.document(doc_id).unwrap().version(),
            version,
            "{edit}"
        );
    }
    command(editor, Command::goto_file_start);
    command(editor, Command::select_all);
    command(editor, Command::yank);
    assert_eq!(register_text(editor, '"'), "same\nnew\nlast\n");
    command(editor, Command::goto_last_diag);
    assert!(editor
        .tree
        .get(id)
        .diff_mode
        .cursor(editor.document(doc_id).unwrap(), id)
        .is_some());
    command(editor, Command::extend_line_below);
    command(editor, Command::yank);
    assert_eq!(register_text(editor, '"'), "old\n");
    typed(editor, "review-mode", "off")?;
    command(editor, Command::delete_selection);
    assert!(!editor
        .document(doc_id)
        .unwrap()
        .text()
        .is_instance(&original));
    command(editor, Command::undo);
    assert_eq!(
        editor.document(doc_id).unwrap().text().to_string(),
        original.to_string()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostic_navigation_merges_diff_hunks_and_real_diagnostics() -> anyhow::Result<()> {
    use crate::commands::MappableCommand as Command;
    use helix_core::diagnostic::{Diagnostic, DiagnosticProvider, Severity};
    let base = "old head\nkeep a\nold one\nold two\nold three\nkeep b\nold replace\nkeep c\nkeep d\nold tail\n";
    let text = "keep a\nkeep b\nnew replace\nkeep c\nadded\nkeep d\n";
    let mut app = diff_application(base, text).await?;
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    let doc = editor.document_mut(doc_id).unwrap();
    let positions = [
        1,
        doc.text().line_to_char(1) + 1,
        doc.text().line_to_char(2),
        doc.text().line_to_char(4),
        doc.text().line_to_char(5) + 1,
    ];
    // Two LSP diagnostics coincide with the added hunk's start. Navigation
    // must make one stop there, without skipping the next deletion.
    let diagnostics: Vec<_> = positions
        .iter()
        .chain(std::iter::once(&positions[3]))
        .map(|&start| Diagnostic {
            // A diagnostic can span later changes; those must stay reachable.
            range: helix_core::diagnostic::Range {
                start,
                end: if start == positions[0] {
                    doc.text().len_chars() - 1
                } else {
                    start
                },
            },
            ends_at_word: false,
            starts_at_word: false,
            zero_width: start != positions[0],
            line: doc.text().char_to_line(start),
            message: "Real diagnostic".into(),
            severity: Some(Severity::Warning),
            code: None,
            provider: DiagnosticProvider::Lsp {
                server_id: Default::default(),
                identifier: None,
            },
            tags: Vec::new(),
            source: None,
            data: None,
        })
        .collect();
    doc.replace_diagnostics(diagnostics, &[], None);
    let version = doc.version();
    let diagnostics_generation = doc.diagnostics_generation();
    typed(editor, "review-mode", "on")?;
    assert_eq!(
        editor
            .tree
            .get(id)
            .diff_mode
            .display(editor.document(doc_id).unwrap())
            .unwrap()
            .hunks
            .len(),
        5
    );

    #[derive(Clone, Copy)]
    enum Stop {
        Deleted(&'static str),
        Source(usize),
    }
    let component = EditorView::new(Keymaps::default());
    let assert_stop = |editor: &Editor, stop: Stop| {
        let doc = editor.document(doc_id).unwrap();
        let view = editor.tree.get(id);
        match stop {
            Stop::Deleted(old) => {
                let cursor = view.diff_mode.cursor(doc, id).unwrap();
                assert_eq!(cursor.row, 0);
                assert_eq!(cursor.column, 0);
                assert_eq!(
                    view.diff_mode
                        .display(doc)
                        .unwrap()
                        .deleted_text(&cursor.before)
                        .to_string(),
                    old
                );
                assert!(view.diff_cursor_screen_coords(doc).is_some());
            }
            Stop::Source(pos) => {
                assert!(view.diff_mode.cursor(doc, id).is_none());
                assert_eq!(doc.selection(id).primary().from(), pos);
            }
        }
        assert_eq!(doc.text().to_string(), text);
        assert_eq!(doc.version(), version);
        assert_eq!(doc.diagnostics_generation(), diagnostics_generation);
        assert_eq!(doc.diagnostics().len(), 6);
        assert_matches_fresh(&component, editor);
    };
    let stops = [
        Stop::Deleted("old head\n"),
        Stop::Source(positions[0]),
        Stop::Deleted("old one\nold two\nold three\n"),
        Stop::Source(positions[1]),
        Stop::Deleted("old replace\n"),
        Stop::Source(positions[2]),
        Stop::Source(positions[3]),
        Stop::Source(positions[4]),
        Stop::Deleted("old tail\n"),
    ];
    command(editor, Command::goto_first_diag);
    assert_stop(editor, stops[0]);
    // The diagnostic jump really focuses old text, including its read-only
    // guard, instead of selecting the editable row at its anchor.
    assert_eq!(register_text(editor, '.'), "o");
    command(editor, Command::delete_selection);
    assert_stop(editor, stops[0]);
    for &stop in &stops[1..] {
        command(editor, Command::goto_next_diag);
        assert_stop(editor, stop);
    }
    command(editor, Command::goto_next_diag);
    assert_stop(editor, *stops.last().unwrap());
    for &stop in stops[..stops.len() - 1].iter().rev() {
        command(editor, Command::goto_prev_diag);
        assert_stop(editor, stop);
    }
    command(editor, Command::goto_prev_diag);
    assert_stop(editor, stops[0]);

    command_with_count(
        editor,
        Command::goto_next_diag,
        std::num::NonZeroUsize::new(3),
    );
    assert_stop(editor, stops[3]);
    command(editor, Command::repeat_last_motion);
    assert_stop(editor, stops[6]);
    command(editor, Command::repeat_last_motion);
    assert_stop(editor, stops[8]);
    command(editor, Command::repeat_last_motion);
    assert_stop(editor, stops[8]);
    command_with_count(
        editor,
        Command::goto_prev_diag,
        std::num::NonZeroUsize::new(3),
    );
    assert_stop(editor, stops[5]);
    command_with_count(
        editor,
        Command::goto_prev_diag,
        std::num::NonZeroUsize::new(usize::MAX),
    );
    assert_stop(editor, stops[0]);
    command(editor, Command::goto_last_diag);
    assert_stop(editor, stops[8]);

    // Switching review mode off restores diagnostic-only navigation.
    typed(editor, "review-mode", "off")?;
    command(editor, Command::goto_first_diag);
    assert_stop(editor, Stop::Source(positions[0]));
    command(editor, Command::goto_next_diag);
    assert_stop(editor, Stop::Source(positions[0]));
    command(editor, Command::flip_selections);
    command(editor, Command::goto_next_diag);
    assert_stop(editor, Stop::Source(positions[1]));
    command(editor, Command::goto_last_diag);
    assert_stop(editor, Stop::Source(positions[4]));
    command(editor, Command::goto_prev_diag);
    assert_stop(editor, Stop::Source(positions[3]));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostic_jumps_keep_a_visible_cursor_with_deleted_rows_and_inline_errors(
) -> anyhow::Result<()> {
    use crate::commands::MappableCommand as Command;
    use helix_core::diagnostic::{Diagnostic, DiagnosticProvider, Severity};
    use helix_view::{annotations::diagnostics::DiagnosticFilter, graphics::Color};
    for soft_wrap in [false, true] {
        for deleted in [0, 3, 40] {
            for prefix in ["", "keep\n"] {
                let old = "removed\n".repeat(deleted);
                let long = "\t界e\u{301}word ".repeat(45);
                let text = format!("{prefix}error one\n{long}\n\nlast error");
                let base = format!("{prefix}{old}old error\n{long}\n\nlast error");
                let mut app = diff_application(&base, &text).await?;
                let mut config = (*app.editor.config()).clone();
                config.soft_wrap.enable = Some(soft_wrap);
                config.inline_diagnostics.cursor_line = DiagnosticFilter::Enable(Severity::Error);
                config.inline_diagnostics.other_lines = DiagnosticFilter::Enable(Severity::Error);
                config.end_of_line_diagnostics = DiagnosticFilter::Enable(Severity::Hint);
                app.handle_config_events(helix_view::editor::ConfigEvent::Update(Box::new(config)));
                let editor = &mut app.editor;
                let id = editor.tree.focus;
                let doc_id = editor.tree.get(id).doc;
                let doc = editor.document_mut(doc_id).unwrap();
                let line = usize::from(!prefix.is_empty());
                let short = doc.text().line_to_char(line);
                let wrapped = doc.text().line_to_char(line + 1);
                let blank = doc.text().line_to_char(line + 2);
                let last = doc.text().line_to_char(line + 3);
                let ranges = [
                    (short, short + 5),
                    (short + 5, wrapped),
                    (wrapped + 300, wrapped + 315),
                    (blank, blank),
                    (last, doc.text().len_chars()),
                ];
                let diagnostics = ranges
                    .into_iter()
                    .map(|(start, end)| Diagnostic {
                        range: helix_core::diagnostic::Range { start, end },
                        ends_at_word: false,
                        starts_at_word: false,
                        zero_width: start == end,
                        line: doc.text().char_to_line(start),
                        message: "Error details\n".repeat(20),
                        severity: Some(Severity::Error),
                        code: None,
                        provider: DiagnosticProvider::Lsp {
                            server_id: Default::default(),
                            identifier: None,
                        },
                        tags: Vec::new(),
                        source: None,
                        data: None,
                    })
                    .collect::<Vec<_>>();
                doc.replace_diagnostics(diagnostics, &[], None);
                let component = EditorView::new(Keymaps::default());
                for diff in [false, true] {
                    typed(editor, "review-mode", if diff { "on" } else { "off" })?;
                    for command_name in [Command::goto_first_diag, Command::goto_last_diag] {
                        command(editor, command_name);
                        for direction in [Command::goto_next_diag, Command::goto_prev_diag] {
                            for _ in 0..8 {
                                command(editor, direction.clone());
                                editor.cursor_cache.reset();
                                let buffer = rendered(&component, editor);
                                let view = editor.tree.get(id);
                                let doc = editor.document(doc_id).unwrap();
                                let position = editor.cursor_cache.get(view, doc).unwrap_or_else(|| panic!(
                                    "hidden cursor: wrap={soft_wrap} deleted={deleted} prefix={prefix:?} diff={diff}, selection={:?}, offset={:?}",
                                    doc.selection(id), doc.view_offset(id)
                                ));
                                let inner = view.inner_area(doc);
                                assert!(position.row < inner.height as usize);
                                assert!(position.col < inner.width as usize);
                                assert_eq!(
                                    buffer[(inner.x + position.col as u16, inner.y + position.row as u16)].bg,
                                    Color::Rgb(255, 255, 255),
                                    "undrawn cursor: wrap={soft_wrap} deleted={deleted} prefix={prefix:?} diff={diff}, position={position:?}, selection={:?}, offset={:?}",
                                    doc.selection(id), doc.view_offset(id)
                                );
                                assert_matches_fresh(&component, editor);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostic_jumps_from_horizontally_scrolled_deleted_text_keep_the_cursor_visible(
) -> anyhow::Result<()> {
    use crate::commands::MappableCommand as Command;
    use helix_core::diagnostic::{Diagnostic, DiagnosticProvider, Severity};
    use helix_view::{annotations::diagnostics::DiagnosticFilter, graphics::Color};

    let text = "first error\nprefix\nnew error\nlast\n";
    let base = format!("first error\nprefix\n{}\nlast\n", "old word ".repeat(60));
    for soft_wrap in [false, true] {
        let mut app = diff_application(&base, text).await?;
        let mut config = (*app.editor.config()).clone();
        config.soft_wrap.enable = Some(soft_wrap);
        config.inline_diagnostics.cursor_line = DiagnosticFilter::Enable(Severity::Error);
        config.inline_diagnostics.other_lines = DiagnosticFilter::Enable(Severity::Error);
        config.end_of_line_diagnostics = DiagnosticFilter::Enable(Severity::Hint);
        app.handle_config_events(helix_view::editor::ConfigEvent::Update(Box::new(config)));
        let editor = &mut app.editor;
        let id = editor.tree.focus;
        let doc_id = editor.tree.get(id).doc;
        let doc = editor.document_mut(doc_id).unwrap();
        let original = doc.text().clone();
        let diagnostics = [0, 2]
            .into_iter()
            .map(|line| {
                let start = doc.text().line_to_char(line);
                Diagnostic {
                    range: helix_core::diagnostic::Range {
                        start,
                        end: start + 5,
                    },
                    ends_at_word: false,
                    starts_at_word: false,
                    zero_width: false,
                    line,
                    message: "Error details\n".repeat(20),
                    severity: Some(Severity::Error),
                    code: None,
                    provider: DiagnosticProvider::Lsp {
                        server_id: Default::default(),
                        identifier: None,
                    },
                    tags: Vec::new(),
                    source: None,
                    data: None,
                }
            })
            .collect::<Vec<_>>();
        doc.replace_diagnostics(diagnostics, &[], None);
        typed(editor, "review-mode", "on")?;
        let component = EditorView::new(Keymaps::default());
        for command_name in [
            Command::goto_first_diag,
            Command::goto_next_diag,
            Command::goto_line_end,
            Command::goto_line_start,
            Command::goto_line_end,
            Command::goto_next_diag,
            Command::goto_prev_diag,
            Command::goto_line_end,
            Command::goto_next_diag,
        ] {
            let name = command_name.name().to_owned();
            command(editor, command_name);
            editor.cursor_cache.reset();
            let buffer = rendered(&component, editor);
            let view = editor.tree.get(id);
            let doc = editor.document(doc_id).unwrap();
            let position = editor.cursor_cache.get(view, doc).unwrap_or_else(|| {
                panic!(
                    "hidden cursor after {name}: wrap={soft_wrap}, offset={:?}, review={:?}",
                    doc.view_offset(id),
                    view.diff_mode.cursor(doc, id)
                )
            });
            let inner = view.inner_area(doc);
            assert!(position.row < inner.height as usize);
            assert!(position.col < inner.width as usize);
            assert_eq!(
                buffer[(inner.x + position.col as u16, inner.y + position.row as u16)].bg,
                Color::Rgb(255, 255, 255),
                "undrawn cursor after {name}: wrap={soft_wrap}"
            );
            if name == "goto_line_end" {
                assert!(doc.view_offset(id).horizontal_offset > 0);
            } else if name == "goto_next_diag" || name == "goto_prev_diag" {
                assert_eq!(doc.view_offset(id).horizontal_offset, 0);
            }
            assert!(doc.text().is_instance(&original));
            assert_matches_fresh(&component, editor);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_cursor_from_wrapped_viewport_anchor() -> anyhow::Result<()> {
    use crate::commands::MappableCommand as Command;
    use helix_view::graphics::Color;

    let prefix = "wrapped source ".repeat(20);
    let deleted = "            visual_pos: Position::default(),";
    let base = format!("{prefix}\n            annotations,\n{deleted}\nkeep\n");
    let text = format!(
        "{prefix}\n            annotations,\n            visual_pos: Position::new(0, 0),\nkeep\n"
    );
    let mut app = diff_application(&base, &text).await?;
    let mut config = (*app.editor.config()).clone();
    config.soft_wrap.enable = Some(true);
    app.handle_config_events(helix_view::editor::ConfigEvent::Update(Box::new(config)));
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    typed(editor, "review-mode", "on")?;
    let component = EditorView::new(Keymaps::default());
    for navigation in [
        Command::goto_next_diag,
        Command::goto_first_diag,
        Command::goto_last_diag,
    ] {
        editor.tree.get_mut(id).diff_mode.clear_cursor();
        let doc = editor.document_mut(doc_id).unwrap();
        doc.set_selection(id, Selection::point(100));
        doc.set_view_offset(
            id,
            ViewPosition {
                anchor: 100,
                horizontal_offset: 0,
                vertical_offset: 0,
            },
        );
        // The viewport starts on a continuation of the wrapped source line.
        command(editor, navigation);
        editor.cursor_cache.reset();
        let buffer = rendered(&component, editor);
        let view = editor.tree.get(id);
        let doc = editor.document(doc_id).unwrap();
        let cursor = view
            .diff_mode
            .cursor(doc, id)
            .expect("deleted text focused");
        assert_eq!(
            view.diff_mode
                .display(doc)
                .unwrap()
                .deleted_text(&cursor.before)
                .to_string(),
            format!("{deleted}\n")
        );
        let position = editor
            .cursor_cache
            .get(view, doc)
            .expect("deleted cursor visible");
        let inner = view.inner_area(doc);
        assert_eq!(
            row(&buffer, inner.y + position.row as u16, inner.x),
            deleted
        );
        assert_eq!(
            buffer[(inner.x + position.col as u16, inner.y + position.row as u16)].bg,
            Color::Rgb(255, 255, 255)
        );
        assert_eq!(doc.text().to_string(), text);
        assert_matches_fresh(&component, editor);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostic_navigation_visits_added_hunks_without_an_lsp() -> anyhow::Result<()> {
    use crate::commands::MappableCommand as Command;
    let text = "head\nfirst\nsecond\nmiddle\nlast\ntail\n";
    let mut app = diff_application("head\nmiddle\ntail\n", text).await?;
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    let first = editor.document(doc_id).unwrap().text().line_to_char(1);
    let last = editor.document(doc_id).unwrap().text().line_to_char(4);
    typed(editor, "review-mode", "on")?;
    let assert_source = |editor: &Editor, pos| {
        let doc = editor.document(doc_id).unwrap();
        assert!(editor.tree.get(id).diff_mode.cursor(doc, id).is_none());
        assert!(doc.diagnostics().is_empty());
        assert_eq!(
            doc.selection(id).primary().cursor(doc.text().slice(..)),
            pos
        );
        assert_eq!(doc.text().to_string(), text);
    };
    command(editor, Command::goto_next_diag);
    assert_source(editor, first);
    command(editor, Command::goto_next_diag);
    assert_source(editor, last);
    command(editor, Command::goto_next_diag);
    assert_source(editor, last);
    command(editor, Command::goto_prev_diag);
    assert_source(editor, first);
    command(editor, Command::goto_prev_diag);
    assert_source(editor, first);
    command(editor, Command::goto_last_diag);
    assert_source(editor, last);
    command(editor, Command::goto_first_diag);
    assert_source(editor, first);
    typed(editor, "review-mode", "off")?;
    command(editor, Command::goto_last_diag);
    assert_source(editor, first);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_text_selection_yanks_old_words_and_lines_and_rejects_edits() -> anyhow::Result<()>
{
    use crate::commands::{self, MappableCommand as Command};
    let old = "alpha beta\nsecond line\nlast old\n";
    let mut app = diff_application(&format!("keep\n{old}same\n"), "keep\nsame\n").await?;
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    typed(editor, "review-mode", "on")?;
    command(editor, Command::move_visual_line_down);
    let original = editor.document(doc_id).unwrap().text().clone();
    let selection = editor.document(doc_id).unwrap().selection(id).clone();
    let version = editor.document(doc_id).unwrap().version();
    command(editor, Command::select_mode);
    assert_eq!(editor.mode, Mode::Select);
    assert_eq!(
        editor
            .tree
            .get(id)
            .diff_mode
            .display(editor.document(doc_id).unwrap())
            .unwrap()
            .deletions[0]
            .height(),
        3
    );
    command(editor, Command::extend_next_word_end);
    assert_eq!(register_text(editor, '.'), "alpha");
    let component = EditorView::new(Keymaps::default());
    let doc = editor.document(doc_id).unwrap();
    let inner = editor.tree.get(id).inner_area(doc);
    let buffer = rendered(&component, editor);
    assert_eq!(
        buffer[(inner.x + 1, 1)].bg,
        helix_view::graphics::Color::Rgb(48, 48, 48)
    );
    assert_eq!(
        buffer[(inner.x + 7, 1)].bg,
        helix_view::graphics::Color::Rgb(64, 32, 32)
    );
    assert_matches_fresh(&component, editor);
    command(editor, Command::yank);
    assert_eq!(register_text(editor, '"'), "alpha");
    assert_eq!(editor.mode, Mode::Normal);
    command(editor, Command::extend_line_below);
    command(editor, Command::extend_line_below);
    command(editor, Command::yank_joined);
    assert_eq!(register_text(editor, '"'), "alpha beta\nsecond line\n");
    command(editor, Command::select_all);
    commands::yank_main_selection_to_register(editor, 'a');
    assert_eq!(register_text(editor, 'a'), old);
    for edit in [
        Command::delete_selection,
        Command::change_selection,
        Command::replace,
        Command::insert_mode,
        Command::paste_after,
        Command::undo,
        Command::redo,
        Command::shell_pipe,
    ] {
        command(editor, edit);
        assert_eq!(register_text(editor, '.'), old);
    }
    let mut jobs = crate::job::Jobs::new();
    commands::paste_bracketed_value(
        &mut commands::Context {
            editor,
            jobs: &mut jobs,
            register: None,
            count: None,
            callback: Vec::new(),
            on_next_key_callback: None,
        },
        "pasted".into(),
    );
    command(editor, ":sort".parse().unwrap());
    assert!(editor.status_msg.as_ref().unwrap().0.contains("read-only"));
    let doc = editor.document(doc_id).unwrap();
    assert!(doc.text().is_instance(&original));
    assert_eq!(doc.selection(id), &selection);
    assert_eq!(doc.version(), version);
    assert_matches_fresh(&component, editor);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_text_selection_preserves_graphemes_tabs_crlf_and_reverse_ranges(
) -> anyhow::Result<()> {
    use crate::commands::MappableCommand as Command;
    let old = "\t界e\u{301}🦀 old\r\n";
    let mut app = diff_application(&format!("keep\r\n{old}last\r\n"), "keep\r\nlast\r\n").await?;
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    typed(editor, "review-mode", "on")?;
    command(editor, Command::move_visual_line_down);
    command(editor, Command::move_char_right);
    command(editor, Command::select_mode);
    command(editor, Command::extend_char_right);
    assert_eq!(register_text(editor, '.'), "界e\u{301}");
    let cursor = editor
        .tree
        .get(id)
        .diff_cursor_screen_coords(editor.document(doc_id).unwrap())
        .unwrap();
    assert_eq!(cursor.col, 6);
    command(editor, Command::extend_char_right);
    assert_eq!(register_text(editor, '.'), "界e\u{301}🦀");
    command(editor, Command::flip_selections);
    command(editor, Command::extend_char_left);
    command(editor, Command::yank);
    assert_eq!(register_text(editor, '"'), "\t界e\u{301}🦀");
    command(editor, Command::select_all);
    command(editor, Command::yank);
    assert_eq!(register_text(editor, '"'), old);
    assert_eq!(
        editor.document(doc_id).unwrap().text().to_string(),
        "keep\r\nlast\r\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_text_mouse_selection_copies_old_text_and_stays_in_its_block() -> anyhow::Result<()>
{
    use helix_view::{
        input::{MouseButton, MouseEvent, MouseEventKind},
        keyboard::KeyModifiers,
    };
    let mut app = diff_application(
        "keep\nalpha beta\nsecond line\nlast old\nsame\n",
        "keep\nsame\n",
    )
    .await?;
    let mut config = (*app.editor.config()).clone();
    config.middle_click_paste = true;
    config.mouse_yank_register = 'a';
    app.handle_config_events(helix_view::editor::ConfigEvent::Update(Box::new(config)));
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    typed(editor, "review-mode", "on")?;
    let x = editor
        .tree
        .get(id)
        .inner_area(editor.document(doc_id).unwrap())
        .x;
    let mut component = EditorView::new(Keymaps::default());
    let mut jobs = crate::job::Jobs::new();
    let mut cx = crate::commands::Context {
        editor,
        jobs: &mut jobs,
        register: None,
        count: None,
        callback: Vec::new(),
        on_next_key_callback: None,
    };
    for (kind, row, column) in [
        (MouseEventKind::Down(MouseButton::Left), 1, x + 1),
        (MouseEventKind::Drag(MouseButton::Left), 2, x + 4),
        (MouseEventKind::Drag(MouseButton::Left), 4, x),
        (MouseEventKind::Up(MouseButton::Left), 2, x + 4),
    ] {
        component.handle_mouse_event(
            &MouseEvent {
                kind,
                row,
                column,
                modifiers: KeyModifiers::empty(),
            },
            &mut cx,
        );
    }
    assert_eq!(register_text(cx.editor, 'a'), "lpha beta\nsecon");
    assert_eq!(register_text(cx.editor, '.'), "lpha beta\nsecon");
    assert_eq!(
        cx.editor
            .tree
            .get(id)
            .diff_mode
            .display(cx.editor.document(doc_id).unwrap())
            .unwrap()
            .deletions[0]
            .height(),
        3
    );
    component.handle_mouse_event(
        &MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Middle),
            row: 2,
            column: x,
            modifiers: KeyModifiers::ALT,
        },
        &mut cx,
    );
    assert_eq!(
        cx.editor.document(doc_id).unwrap().text().to_string(),
        "keep\nsame\n"
    );
    assert_matches_fresh(&component, cx.editor);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_text_selection_keeps_anchor_while_paging_and_stops_at_block_boundary(
) -> anyhow::Result<()> {
    use crate::commands::MappableCommand as Command;
    let mut app = diff_application(
        &format!("keep\n{}same\n", "old\n".repeat(100)),
        "keep\nsame\n",
    )
    .await?;
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    typed(editor, "review-mode", "on")?;
    command(editor, Command::move_visual_line_down);
    command(editor, Command::select_mode);
    command(editor, Command::page_cursor_half_down);
    let cursor = editor
        .tree
        .get(id)
        .diff_mode
        .cursor(editor.document(doc_id).unwrap(), id)
        .unwrap();
    assert_eq!(cursor.range.anchor, 0);
    assert_eq!(cursor.row, editor.tree.get(id).inner_height() / 2);
    command(editor, Command::page_down);
    let view = editor.tree.get(id);
    let doc = editor.document(doc_id).unwrap();
    assert_eq!(view.diff_mode.cursor(doc, id).unwrap().range.anchor, 0);
    assert_eq!(
        view.diff_cursor_screen_coords(doc).unwrap().row,
        editor.config().scrolloff
    );
    for _ in 0..10 {
        command(editor, Command::page_cursor_down);
    }
    let cursor = editor
        .tree
        .get(id)
        .diff_mode
        .cursor(editor.document(doc_id).unwrap(), id)
        .unwrap();
    assert_eq!(cursor.row, 99);
    assert_eq!(cursor.range.anchor, 0);
    command(editor, Command::yank);
    assert!(!register_text(editor, '"').contains("same"));
    assert_eq!(
        editor.document(doc_id).unwrap().text().to_string(),
        "keep\nsame\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn paging_keeps_deleted_row_cursor_at_viewport_edge_without_snapping_back(
) -> anyhow::Result<()> {
    use crate::commands::MappableCommand as Command;

    for prefix in ["", "keep\n"] {
        let base = format!("{prefix}{}same\n", "removed\n".repeat(70_000));
        let text = format!("{prefix}same\n");
        let mut app = diff_application(&base, &text).await?;
        let editor = &mut app.editor;
        let id = editor.tree.focus;
        let doc_id = editor.tree.get(id).doc;
        let original = editor.document(doc_id).unwrap().text().clone();
        typed(editor, "review-mode", "on")?;
        command(editor, Command::goto_first_diag);

        let component = EditorView::new(Keymaps::default());
        for _ in 0..3 {
            command(editor, Command::page_down);
            let view = editor.tree.get(id);
            let doc = editor.document(doc_id).unwrap();
            let margin = editor
                .config()
                .scrolloff
                .min(view.inner_height().saturating_sub(1) / 2);
            assert_eq!(view.diff_cursor_screen_coords(doc).unwrap().row, margin);
            let cursor = view.diff_mode.cursor(doc, id).unwrap().row;
            let offset = doc.view_offset(id);
            command(editor, Command::move_visual_line_down);
            let view = editor.tree.get(id);
            let doc = editor.document(doc_id).unwrap();
            assert_eq!(view.diff_mode.cursor(doc, id).unwrap().row, cursor + 1);
            assert_eq!(doc.view_offset(id), offset);
            assert_matches_fresh(&component, editor);
        }
        command(editor, Command::page_up);
        let view = editor.tree.get(id);
        let doc = editor.document(doc_id).unwrap();
        let margin = editor
            .config()
            .scrolloff
            .min(view.inner_height().saturating_sub(1) / 2);
        assert_eq!(
            view.diff_cursor_screen_coords(doc).unwrap().row,
            view.inner_height() - margin - 1
        );
        let cursor = view.diff_mode.cursor(doc, id).unwrap().row;
        let offset = doc.view_offset(id);
        command(editor, Command::move_visual_line_up);
        let view = editor.tree.get(id);
        let doc = editor.document(doc_id).unwrap();
        assert_eq!(view.diff_mode.cursor(doc, id).unwrap().row, cursor - 1);
        assert_eq!(doc.view_offset(id), offset);
        assert!(doc.text().is_instance(&original));
        assert_matches_fresh(&component, editor);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_paging_counts_deleted_rows_and_preserves_the_review_cursor() -> anyhow::Result<()> {
    use crate::commands::MappableCommand as Command;

    let mut app = diff_application(
        &format!("keep\n{}same\n", "removed\n".repeat(100)),
        "keep\nsame\n",
    )
    .await?;
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    typed(editor, "review-mode", "on")?;
    command(editor, Command::move_visual_line_down);
    let count = editor.tree.get(id).inner_height() / 2;
    for _ in 0..3 {
        let before = editor
            .tree
            .get(id)
            .diff_mode
            .cursor(editor.document(doc_id).unwrap(), id)
            .unwrap()
            .row;
        command(editor, Command::page_cursor_half_down);
        let view = editor.tree.get(id);
        let doc = editor.document(doc_id).unwrap();
        assert_eq!(view.diff_mode.cursor(doc, id).unwrap().row, before + count);
        assert!(view.diff_cursor_screen_coords(doc).is_some());
        command(editor, Command::page_cursor_half_up);
        let view = editor.tree.get(id);
        assert_eq!(
            view.diff_mode
                .cursor(editor.document(doc_id).unwrap(), id)
                .unwrap()
                .row,
            before
        );
    }
    let count = editor.tree.get(id).inner_height();
    command(editor, Command::page_cursor_down);
    assert_eq!(
        editor
            .tree
            .get(id)
            .diff_mode
            .cursor(editor.document(doc_id).unwrap(), id)
            .unwrap()
            .row,
        count
    );
    command(editor, Command::page_cursor_up);
    assert_eq!(
        editor
            .tree
            .get(id)
            .diff_mode
            .cursor(editor.document(doc_id).unwrap(), id)
            .unwrap()
            .row,
        0
    );
    assert_eq!(
        editor.document(doc_id).unwrap().text().to_string(),
        "keep\nsame\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_paging_keeps_source_cursor_at_edge_when_there_are_no_deletions() -> anyhow::Result<()>
{
    use crate::commands::MappableCommand as Command;

    let text = format!("keep\n{}", "added\n".repeat(100));
    let mut app = diff_application("keep\n", &text).await?;
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    typed(editor, "review-mode", "on")?;
    for _ in 0..3 {
        command(editor, Command::page_down);
        let view = editor.tree.get(id);
        let doc = editor.document(doc_id).unwrap();
        let cursor = doc.selection(id).primary().cursor(doc.text().slice(..));
        assert_eq!(
            view.screen_coords_at_pos(doc, doc.text().slice(..), cursor)
                .unwrap()
                .row,
            editor.config().scrolloff
        );
        let offset = doc.view_offset(id);
        command(editor, Command::move_visual_line_down);
        assert_eq!(editor.document(doc_id).unwrap().view_offset(id), offset);
    }
    command(editor, Command::page_up);
    let view = editor.tree.get(id);
    let doc = editor.document(doc_id).unwrap();
    let cursor = doc.selection(id).primary().cursor(doc.text().slice(..));
    assert_eq!(
        view.screen_coords_at_pos(doc, doc.text().slice(..), cursor)
            .unwrap()
            .row,
        view.inner_height() - editor.config().scrolloff - 1
    );
    assert_eq!(doc.text().to_string(), text);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_toggle_shows_all_deleted_rows_and_keeps_source_unchanged() -> anyhow::Result<()> {
    let mut app = diff_application(
        "gone1\ngone2\ngone3\nsame\nold\nlast\n",
        "same\nnew\nlast\n",
    )
    .await?;
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    let original = editor.document(doc_id).unwrap().text().clone();
    let component = EditorView::new(Keymaps::default());
    let before = rendered(&component, editor);
    typed(editor, "review-mode", "")?;
    let inner = editor
        .tree
        .get(id)
        .inner_area(editor.document(doc_id).unwrap());
    let buffer = rendered(&component, editor);
    assert_eq!(row(&buffer, 0, inner.x), "gone1");
    assert_eq!(row(&buffer, 1, inner.x), "gone2");
    assert_eq!(row(&buffer, 2, inner.x), "gone3");
    assert_eq!(row(&buffer, 3, inner.x), "same");
    assert_eq!(row(&buffer, 4, inner.x), "old");
    assert_eq!(row(&buffer, 5, inner.x), "new");
    assert_eq!(buffer[(0, 0)].symbol.as_str(), "-");
    assert_eq!(buffer[(0, 5)].symbol.as_str(), "+");
    for x in 0..inner.right() {
        assert_eq!(
            buffer[(x, 5)].bg,
            helix_view::graphics::Color::Rgb(32, 64, 32)
        );
    }
    assert_matches_fresh(&component, editor);
    let buffer = rendered(&component, editor);
    assert_eq!(row(&buffer, 0, inner.x), "gone1");
    assert_eq!(row(&buffer, 1, inner.x), "gone2");
    assert_eq!(row(&buffer, 2, inner.x), "gone3");
    assert_eq!(
        buffer[(inner.x, 1)].bg,
        helix_view::graphics::Color::Rgb(64, 32, 32)
    );
    assert_matches_fresh(&component, editor);
    typed(editor, "diff-mode", "off")?;
    assert_eq!(rendered(&component, editor), before);
    assert!(editor
        .document(doc_id)
        .unwrap()
        .text()
        .is_instance(&original));
    assert!(typed(editor, "review-mode", "invalid").is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_handles_eof_empty_buffers_tabs_horizontal_scroll_and_no_configured_gutters(
) -> anyhow::Result<()> {
    for (base, text, marker_row, deleted) in [
        ("keep\nremoved\n", "keep\n", 1, "removed"),
        (
            "keep\r\nremoved\r\nlast\r\n",
            "keep\r\nlast\r\n",
            1,
            "removed",
        ),
        ("removed", "", 0, "removed"),
        ("old\n", "new", 0, "old"),
        (
            "keep\n\t界e\u{301}old\nlast\n",
            "keep\nlast\n",
            1,
            "    界e\u{301}old",
        ),
    ] {
        let mut app = diff_application(base, text).await?;
        let editor = &mut app.editor;
        let id = editor.tree.focus;
        editor.tree.get_mut(id).gutters.layout.clear();
        typed(editor, "review-mode", "on")?;
        let doc_id = editor.tree.get(id).doc;
        // Review deleted text without moving the editable cursor or source.
        editor
            .document_mut(doc_id)
            .unwrap()
            .set_view_offset(id, ViewPosition::default());
        let component = EditorView::new(Keymaps::default());
        let inner = editor
            .tree
            .get(id)
            .inner_area(editor.document(doc_id).unwrap());
        assert_eq!(inner.x, 1);
        let buffer = rendered(&component, editor);
        assert_eq!(row(&buffer, marker_row, inner.x), deleted);
        assert_eq!(buffer[(0, marker_row)].symbol.as_str(), "-");
        assert_matches_fresh(&component, editor);
        editor.document_mut(doc_id).unwrap().set_view_offset(
            id,
            ViewPosition {
                horizontal_offset: 4,
                ..ViewPosition::default()
            },
        );
        let scrolled = rendered(&component, editor);
        if deleted.starts_with("    ") {
            assert_eq!(row(&scrolled, marker_row, inner.x), "界e\u{301}old");
        } else {
            assert_eq!(
                row(&scrolled, marker_row, inner.x),
                deleted.chars().skip(4).collect::<String>()
            );
        }
        assert_matches_fresh(&component, editor);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_additions_keep_color_previews_and_selection_priority() -> anyhow::Result<()> {
    let mut app = diff_application("same\n", "same\ncolor\n").await?;
    let editor = &mut app.editor;
    let id = editor.tree.focus;
    let doc_id = editor.tree.get(id).doc;
    typed(editor, "review-mode", "on")?;
    let component = EditorView::new(Keymaps::default());
    let inner = editor
        .tree
        .get(id)
        .inner_area(editor.document(doc_id).unwrap());
    let buffer = rendered(&component, editor);
    assert_eq!(row(&buffer, 0, inner.x), "same");
    assert_eq!(row(&buffer, 1, inner.x), "color");
    assert_eq!(buffer[(0, 1)].symbol.as_str(), "+");
    assert_eq!(
        buffer[(inner.x + 1, 1)].bg,
        helix_view::graphics::Color::Rgb(32, 64, 32)
    );
    let doc = editor.document_mut(doc_id).unwrap();
    doc.color_swatches = Some(helix_view::document::DocumentColorSwatches {
        color_swatches: vec![
            helix_core::text_annotations::InlineAnnotation::new(5, "■").with_inherited_background()
        ],
        colors: vec![helix_view::Theme::rgb_highlight(255, 0, 0)],
        color_swatches_padding: vec![
            helix_core::text_annotations::InlineAnnotation::new(5, " ").with_inherited_background()
        ],
        color_ranges: Arc::new(vec![(
            helix_view::Theme::rgb_background_highlight(255, 0, 0),
            5..10,
        )]),
        ..Default::default()
    });
    let buffer = rendered(&component, editor);
    assert_eq!(
        buffer[(inner.x + 1, 1)].bg,
        helix_view::graphics::Color::Rgb(32, 64, 32)
    );
    assert_eq!(row(&buffer, 1, inner.x), "■ color");
    assert_eq!(
        buffer[(inner.x, 1)].fg,
        helix_view::graphics::Color::Rgb(255, 0, 0)
    );
    assert_eq!(buffer[(inner.x, 1)].bg, buffer[(inner.x + 2, 1)].bg);
    editor
        .document_mut(doc_id)
        .unwrap()
        .set_selection(id, Selection::single(5, 10));
    let buffer = rendered(&component, editor);
    assert_eq!(
        buffer[(inner.x + 1, 1)].bg,
        helix_view::graphics::Color::Rgb(48, 48, 48)
    );
    assert_eq!(buffer[(inner.x, 1)].bg, buffer[(inner.x + 2, 1)].bg);
    assert_eq!(
        buffer[(inner.x, 1)].fg,
        helix_view::graphics::Color::Rgb(255, 0, 0)
    );
    assert_matches_fresh(&component, editor);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn large_deletions_scroll_to_old_text_and_back_to_editable_rows() -> anyhow::Result<()> {
    for prefix in ["", "keep\n"] {
        let base = format!("{prefix}{}same\n", "removed\n".repeat(70_000));
        let text = format!("{prefix}same\n");
        let mut app = diff_application(&base, &text).await?;
        let editor = &mut app.editor;
        let id = editor.tree.focus;
        let doc_id = editor.tree.get(id).doc;
        typed(editor, "review-mode", "on")?;
        let before_selection = editor.document(doc_id).unwrap().selection(id).clone();
        let mut jobs = crate::job::Jobs::new();
        let mut context = crate::commands::Context {
            editor,
            jobs: &mut jobs,
            register: None,
            count: None,
            callback: Vec::new(),
            on_next_key_callback: None,
        };
        crate::commands::scroll(
            &mut context,
            69_999 + usize::from(!prefix.is_empty()),
            helix_core::movement::Direction::Forward,
            crate::commands::ScrollCursor::Preserve,
        );
        editor.ensure_cursor_in_view(id);
        assert_eq!(
            editor.document(doc_id).unwrap().selection(id),
            &before_selection
        );
        let component = EditorView::new(Keymaps::default());
        let inner = editor
            .tree
            .get(id)
            .inner_area(editor.document(doc_id).unwrap());
        let buffer = rendered(&component, editor);
        assert_eq!(row(&buffer, 0, inner.x), "removed");
        assert_eq!(row(&buffer, 1, inner.x), "same");
        assert_matches_fresh(&component, editor);
        editor
            .document_mut(doc_id)
            .unwrap()
            .set_selection(id, Selection::point(0));
        assert!(!editor
            .tree
            .get(id)
            .diff_mode
            .preserves_scroll(editor.document(doc_id).unwrap(), id));
        typed(editor, "review-mode", "off")?;
        let buffer = rendered(&component, editor);
        let inner = editor
            .tree
            .get(id)
            .inner_area(editor.document(doc_id).unwrap());
        assert_eq!(
            row(&buffer, 0, inner.x),
            if prefix.is_empty() { "same" } else { "keep" }
        );
    }
    Ok(())
}
