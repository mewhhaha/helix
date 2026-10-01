use super::*;
use helix_core::Transaction;
use helix_view::{document::Mode, editor::CursorSmearConfig};

const CELL_SIZE: CellSize = CellSize {
    width: 8,
    height: 16,
};

async fn application(duration: u64, graphics: bool) -> anyhow::Result<Application> {
    let mut config = Config::default();
    config.editor.cursor_smear = CursorSmearConfig {
        enabled: true,
        duration,
        ..Default::default()
    };
    config.editor.cursor_shape =
        toml::from_str("normal = 'block'\ninsert = 'bar'\nselect = 'underline'\n")?;
    config.editor.lsp.enable = false;
    config.editor.word_completion.enable = false;
    config.editor.auto_completion = false;
    config.editor.idle_timeout = Duration::from_millis(50);
    let loader = syntax::Loader::new(helix_loader::config::default_lang_config().try_into()?)?;
    let mut app = Application::new(
        Args::default(),
        config,
        loader,
        helix_loader::workspace_trust::WorkspaceTrust::fully_trusted(),
    )?;
    // Apply the startup theme notification before measuring an animation.
    let event = app.editor.wait_event().await;
    assert!(matches!(
        event,
        EditorEvent::ConfigEvent(ConfigEvent::ThemeChanged)
    ));
    app.handle_editor_event(event).await;
    app.terminal
        .backend_mut()
        .set_cursor_graphics_cell_size(graphics.then_some(CELL_SIZE));
    app.editor.theme = toml::from_str(
        r##"
        "ui.background" = { bg = "#202020" }
        "ui.text" = { fg = "#eeeeee" }
        "ui.selection" = { bg = "#303030" }
        "ui.cursor" = { bg = "#404040", fg = "#ffffff" }
        "ui.cursor.primary" = { bg = "#eeeeee", fg = "#202020" }
        "ui.cursor.smear" = { bg = "#70b0ff" }
        "##,
    )?;
    replace_text(
        &mut app,
        "abcdefghijklmnopqrstuvwxyz\n",
        Selection::point(0),
    );
    app.render().await;
    assert!(app.cursor_smear_frame.is_none(), "initial cursor is static");
    assert_eq!(app.terminal.backend().cursor_image().is_some(), graphics);
    Ok(app)
}

fn replace_text(app: &mut Application, text: &str, selection: Selection) {
    let (view, doc) = current!(app.editor);
    let transaction = Transaction::change(
        doc.text(),
        [(0, doc.text().len_chars(), Some(text.into()))].into_iter(),
    )
    .with_selection(selection);
    doc.apply(&transaction, view.id);
}

fn set_selection(app: &mut Application, selection: Selection) {
    let (view, doc) = current!(app.editor);
    doc.set_selection(view.id, selection);
}

async fn moving_cursor(duration: u64) -> anyhow::Result<Application> {
    let mut app = application(duration, true).await?;
    set_selection(&mut app, Selection::point(12));
    app.render().await;
    assert!(app.cursor_smear_frame.is_some(), "movement must animate");
    Ok(app)
}

#[tokio::test]
async fn pixel_frames_preserve_text_redraw_requests_and_settle_without_a_timer(
) -> anyhow::Result<()> {
    let mut app = moving_cursor(120).await?;
    let baseline = app.terminal.backend().buffer().clone();
    let initial_image = app.terminal.backend().cursor_image().unwrap().clone();
    let draw_calls = app.terminal.backend().draw_calls();
    let cursor_position = app.terminal.backend().cursor_position();
    let (view, doc) = current_ref!(app.editor);
    let document = doc.text().clone();
    let selection = doc.selection(view.id).clone();

    helix_event::request_redraw();
    app.editor.needs_redraw = true;
    app.paint_cursor_smear(
        tokio::time::Instant::now() + Duration::from_millis(30),
        app.terminal.backend().window_metrics().unwrap(),
    );
    assert!(app.editor.needs_redraw);
    tokio::time::timeout(Duration::from_millis(50), helix_event::redraw_requested()).await?;
    assert_eq!(app.terminal.backend().buffer().content, baseline.content);
    assert_eq!(app.terminal.backend().draw_calls(), draw_calls);
    assert_eq!(app.terminal.backend().cursor_position(), cursor_position);
    assert_ne!(
        app.terminal.backend().cursor_image().unwrap(),
        &initial_image
    );
    let (view, doc) = current_ref!(app.editor);
    assert_eq!(doc.text(), &document);
    assert_eq!(doc.selection(view.id), &selection);

    app.paint_cursor_smear(
        tokio::time::Instant::now() + Duration::from_secs(2),
        app.terminal.backend().window_metrics().unwrap(),
    );
    assert_eq!(app.terminal.backend().buffer().content, baseline.content);
    assert!(app.cursor_smear_frame.is_none(), "idle cursor has no timer");
    let image = app.terminal.backend().cursor_image().unwrap();
    assert_eq!(image.width, u32::from(CELL_SIZE.width));
    assert_eq!(image.height, u32::from(CELL_SIZE.height));
    assert!(image.rgba.chunks_exact(4).any(|pixel| pixel[3] == 255));
    assert!(!app.terminal.backend().cursor_visible());
    assert!(app.terminal.backend().graphics_frames_synchronized());
    assert!(app.close().await.is_empty());
    app.restore_term()?;
    assert!(app.terminal.backend().cursor_image().is_none());
    assert!(app.terminal.backend().cursor_visible());
    Ok(())
}

#[tokio::test]
async fn animation_ticks_do_not_postpone_editor_idle() -> anyhow::Result<()> {
    let mut app = moving_cursor(1000).await?;
    let before = app.terminal.backend().graphics_frame_count();
    app.editor.reset_idle_timer();
    let mut input = futures_util::stream::pending();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(500),
            app.event_loop_until_idle(&mut input),
        )
        .await?
    );
    assert!(app.terminal.backend().graphics_frame_count() > before);
    assert!(
        app.cursor_smear_frame.is_some(),
        "idle occurs during movement"
    );
    assert!(app.close().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn live_disable_restores_the_cursor_and_resize_resets_pixel_geometry() -> anyhow::Result<()> {
    let mut app = moving_cursor(1000).await?;
    let deletes = app.terminal.backend().graphics_delete_count();
    let mut config = (*app.editor.config()).clone();
    config.cursor_smear.enabled = false;
    app.handle_config_events(ConfigEvent::Update(Box::new(config)));
    app.render_cursor_smear().await;
    assert!(app.cursor_smear_frame.is_none());
    assert!(app.terminal.backend().cursor_image().is_none());
    assert!(app.terminal.backend().graphics_delete_count() > deletes);
    let (col, row) = app.terminal.backend().cursor_position();
    assert_eq!(
        app.terminal.backend().buffer().get(col, row).unwrap().bg,
        Color::Rgb(0xee, 0xee, 0xee),
        "the ordinary block cursor must be composited again",
    );
    assert!(app.close().await.is_empty());

    let mut app = moving_cursor(1000).await?;
    let new_metrics = CellSize {
        width: 10,
        height: 20,
    };
    app.terminal.backend_mut().resize(80, 40);
    app.terminal
        .backend_mut()
        .set_cursor_graphics_cell_size(Some(new_metrics));
    app.render_cursor_smear().await;
    assert!(app.cursor_smear_frame.is_none());
    assert_eq!(
        app.terminal.backend().buffer().area,
        Rect::new(0, 0, 80, 40)
    );
    let image = app.terminal.backend().cursor_image().unwrap();
    assert_eq!((image.width, image.height), (10, 20));
    let (view, doc) = current_ref!(app.editor);
    let bounds = view.inner_area(doc);
    assert!(image.position.col >= usize::from(bounds.left()));
    assert!(image.position.col < usize::from(bounds.right()));
    assert!(image.position.row >= usize::from(bounds.top()));
    assert!(image.position.row < usize::from(bounds.bottom()));
    assert!(
        image.position.col as u32 * 10 + u32::from(image.offset_x) + image.width
            <= u32::from(bounds.right()) * 10
    );
    assert!(
        image.position.row as u32 * 20 + u32::from(image.offset_y) + image.height
            <= u32::from(bounds.bottom()) * 20
    );
    assert!(app.close().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn cursorless_overlays_and_focus_loss_restore_ordinary_cursors() -> anyhow::Result<()> {
    struct Overlay;
    impl crate::compositor::Component for Overlay {
        fn render(
            &mut self,
            _: Rect,
            _: &mut tui::buffer::Buffer,
            _: &mut crate::compositor::Context,
        ) {
        }
    }
    let mut app = moving_cursor(1000).await?;
    app.compositor.push(Box::new(Overlay));
    app.render_cursor_smear().await;
    assert!(app.cursor_smear_frame.is_none());
    assert!(app.terminal.backend().cursor_image().is_none());
    app.compositor.pop();
    app.render().await;
    assert!(
        app.cursor_smear_frame.is_none(),
        "closing a popup resets movement"
    );
    assert!(app.terminal.backend().cursor_image().is_some());
    set_selection(&mut app, Selection::point(16));
    app.render().await;
    assert!(app.cursor_smear_frame.is_some());
    app.compositor.handle_event(
        &Event::FocusLost,
        &mut crate::compositor::Context {
            editor: &mut app.editor,
            jobs: &mut app.jobs,
            scroll: None,
        },
    );
    app.render_cursor_smear().await;
    assert!(app.cursor_smear_frame.is_none());
    assert!(app.terminal.backend().cursor_image().is_none());
    assert!(app.terminal.backend().cursor_visible());
    assert!(app.close().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn unsupported_terminals_keep_the_configured_native_cursor_without_animation(
) -> anyhow::Result<()> {
    let mut app = application(120, false).await?;
    app.editor.mode = Mode::Insert;
    set_selection(&mut app, Selection::point(12));
    app.render().await;
    assert!(app.terminal.backend().cursor_image().is_none());
    assert!(app.cursor_smear_frame.is_none());
    assert_eq!(app.terminal.backend().graphics_frame_count(), 0);
    assert!(app.terminal.backend().cursor_visible());
    assert_eq!(app.terminal.cursor_kind(), CursorKind::Bar);
    assert!(app.close().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn graphics_cursor_follows_mode_shapes_wide_graphemes_and_eof() -> anyhow::Result<()> {
    let mut app = application(120, true).await?;
    replace_text(&mut app, "a界e\u{301}\n", Selection::point(1));
    app.render().await;
    app.paint_cursor_smear(
        tokio::time::Instant::now() + Duration::from_secs(2),
        app.terminal.backend().window_metrics().unwrap(),
    );
    for mode in [Mode::Normal, Mode::Insert, Mode::Select] {
        app.editor.mode = mode;
        app.render().await;
        assert!(
            app.cursor_smear_frame.is_none(),
            "mode changes reset movement"
        );
        let image = app.terminal.backend().cursor_image().unwrap();
        match mode {
            Mode::Normal => assert_eq!((image.width, image.height), (16, 16)),
            Mode::Insert => {
                assert!(image.width < u32::from(CELL_SIZE.width));
                assert_eq!(image.height, 16);
            }
            Mode::Select => {
                assert_eq!(image.width, 16);
                assert!(image.height < u32::from(CELL_SIZE.height));
            }
        }
        assert!(!app.terminal.backend().cursor_visible());
    }
    // Combining characters occupy one rendered cell, despite two scalar values.
    app.editor.mode = Mode::Normal;
    set_selection(&mut app, Selection::point(2));
    app.render().await;
    assert_eq!(app.terminal.backend().cursor_image().unwrap().width, 8);
    let eof = current_ref!(app.editor).1.text().len_chars();
    set_selection(&mut app, Selection::point(eof));
    app.render().await;
    app.paint_cursor_smear(
        tokio::time::Instant::now() + Duration::from_secs(2),
        app.terminal.backend().window_metrics().unwrap(),
    );
    let image = app.terminal.backend().cursor_image().unwrap();
    assert_eq!((image.width, image.height), (8, 16));
    assert_eq!(
        (image.position.col as u16, image.position.row as u16),
        app.terminal.backend().cursor_position(),
    );
    assert!(app.close().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn graphics_suppression_preserves_forward_reverse_selections_and_secondary_cursors(
) -> anyhow::Result<()> {
    let mut app = application(120, true).await?;
    for primary in [Range::new(2, 7), Range::new(7, 2)] {
        let selection = Selection::new(vec![primary, Range::new(15, 16)].into(), 0);
        let mut config = (*app.editor.config()).clone();
        config.cursor_smear.enabled = true;
        app.handle_config_events(ConfigEvent::Update(Box::new(config)));
        set_selection(&mut app, selection.clone());
        app.render().await;
        let graphics_cells = app.terminal.backend().buffer().clone();
        let primary_cursor = app.terminal.backend().cursor_position();

        let mut config = (*app.editor.config()).clone();
        config.cursor_smear.enabled = false;
        app.handle_config_events(ConfigEvent::Update(Box::new(config)));
        app.render().await;
        let ordinary_cells = app.terminal.backend().buffer();
        for row in 0..ordinary_cells.area.height {
            for col in 0..ordinary_cells.area.width {
                if (col, row) != primary_cursor {
                    assert_eq!(
                        graphics_cells.get(col, row),
                        ordinary_cells.get(col, row),
                        "selection/secondary cursor changed at ({col}, {row}) for {primary:?}",
                    );
                }
            }
        }
        let (view, doc) = current_ref!(app.editor);
        assert_eq!(doc.selection(view.id), &selection);
    }
    assert!(app.close().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn graphics_colors_preserve_reversed_cursor_contrast_and_explicit_override(
) -> anyhow::Result<()> {
    let mut app = moving_cursor(120).await?;
    app.editor.theme = toml::from_str(
        r##"
        "ui.background" = { bg = "#202020" }
        "ui.text" = { fg = "#eeeeee" }
        "ui.selection" = { bg = "#303030" }
        "ui.cursor.primary" = { fg = "#123456", bg = "#abcdef", modifiers = ["reversed"] }
        "##,
    )?;
    assert_eq!(
        app.cursor_smear_colors(None),
        (Color::Rgb(0x12, 0x34, 0x56), Color::Rgb(0xab, 0xcd, 0xef))
    );
    app.render().await;
    let (col, row) = app.terminal.backend().cursor_position();
    assert_eq!(
        app.terminal.backend().buffer().get(col, row).unwrap().fg,
        Color::Rgb(0xab, 0xcd, 0xef),
    );
    app.editor.theme = toml::from_str(
        r##"
        "ui.background" = { bg = "#202020" }
        "ui.text" = { fg = "#eeeeee" }
        "ui.selection" = { bg = "#303030" }
        "ui.cursor.primary" = { bg = "#abcdef" }
        "ui.cursor.smear" = { fg = "#fedcba" }
        "##,
    )?;
    assert_eq!(
        app.cursor_smear_colors(None).0,
        Color::Rgb(0xfe, 0xdc, 0xba)
    );
    assert!(app.close().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn reversed_background_only_cursor_inherits_the_actual_glyph_color() -> anyhow::Result<()> {
    let mut app = moving_cursor(120).await?;
    app.editor.theme = toml::from_str(
        r##"
        "ui.background" = { bg = "#101112" }
        "ui.text" = { fg = "#2468ac" }
        "ui.selection" = { bg = "#303030" }
        "ui.cursor.primary" = { bg = "#abcdef", modifiers = ["reversed"] }
        "##,
    )?;
    assert_eq!(
        app.cursor_smear_colors(None),
        (Color::Rgb(0x24, 0x68, 0xac), Color::Rgb(0xab, 0xcd, 0xef)),
    );
    let mut syntax_cell = tui::buffer::Cell::default();
    syntax_cell
        .set_fg(Color::Rgb(0x45, 0x67, 0x89))
        .set_bg(Color::Rgb(0x10, 0x11, 0x12));
    assert_eq!(
        app.cursor_smear_colors(Some(&syntax_cell)),
        (Color::Rgb(0x45, 0x67, 0x89), Color::Rgb(0xab, 0xcd, 0xef)),
        "inherited syntax foreground becomes the visible reversed cursor body",
    );
    app.render().await;
    let (col, row) = app.terminal.backend().cursor_position();
    assert_eq!(
        app.terminal.backend().buffer().get(col, row).unwrap().fg,
        Color::Rgb(0xab, 0xcd, 0xef),
    );
    let image = app.terminal.backend().cursor_image().unwrap();
    let pixel = image
        .rgba
        .chunks_exact(4)
        .find(|pixel| pixel[3] != 0)
        .unwrap();
    assert_eq!(&pixel[..3], &[0x24, 0x68, 0xac]);
    assert!(app.close().await.is_empty());
    Ok(())
}

async fn press_keys(app: &mut Application, keys: &str) -> anyhow::Result<()> {
    for key in helix_view::input::parse_macro(keys)? {
        app.handle_terminal_events(Ok(TerminalEvent::Key(key.into())))
            .await;
    }
    Ok(())
}

#[tokio::test]
async fn queued_keys_preserve_commands_and_reduce_redraws() -> anyhow::Result<()> {
    use futures_util::StreamExt;
    for keys in ["iabc<esc>hhx", "gwao", &"l".repeat(160)] {
        let text = "origin word word word word word word word word word word word word word word word word\n";
        let mut single = application(0, false).await?;
        replace_text(&mut single, text, Selection::point(0));
        let before = single.terminal.backend().draw_calls();
        for key in helix_view::input::parse_macro(keys)? {
            single
                .handle_terminal_events(Ok(TerminalEvent::Key(key.into())))
                .await;
            tokio::task::yield_now().await;
        }
        let draws = single.terminal.backend().draw_calls() - before;
        let (view, doc) = current_ref!(single.editor);
        let expected_text = doc.text().clone();
        let expected_selection = doc.selection(view.id).clone();
        assert!(single.close().await.is_empty());
        drop(single);

        let mut batched = application(0, false).await?;
        replace_text(&mut batched, text, Selection::point(0));
        let events = helix_view::input::parse_macro(keys)?
            .into_iter()
            .map(|key| Ok(TerminalEvent::Key(key.into())))
            .collect::<Vec<_>>();
        let mut input = futures_util::stream::iter(events);
        let before = batched.terminal.backend().draw_calls();
        while let Some(first) = input.next().await {
            batched.handle_terminal_event_batch(first, &mut input).await;
        }
        let (view, doc) = current_ref!(batched.editor);
        assert_eq!(&expected_text, doc.text(), "{keys}");
        assert_eq!(&expected_selection, doc.selection(view.id), "{keys}");
        assert!(
            batched.terminal.backend().draw_calls() - before < draws,
            "{keys}"
        );
        assert!(batched.close().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn cursor_frames_measure_geometry_once_and_observe_font_changes() -> anyhow::Result<()> {
    let mut app = moving_cursor(1000).await?;
    let before = app.terminal.backend().metrics_queries();
    app.render().await;
    assert_eq!(app.terminal.backend().metrics_queries() - before, 1);
    let before = app.terminal.backend().metrics_queries();
    app.render_cursor_smear().await;
    assert_eq!(app.terminal.backend().metrics_queries() - before, 1);
    app.terminal
        .backend_mut()
        .set_cursor_graphics_cell_size(Some(CellSize {
            width: 10,
            height: 20,
        }));
    app.render_cursor_smear().await;
    assert_eq!(
        app.cursor_smear_frame.as_ref().map(|frame| frame.cell_size),
        None
    );
    assert!(app.terminal.backend().cursor_image().is_some());
    assert!(app.close().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn word_label_jumps_animate_near_and_far_with_automatic_help_enabled() -> anyhow::Result<()> {
    for (label, word_index) in [("aa", 0), ("ao", 14)] {
        let mut app = application(120, true).await?;
        assert!(app.editor.config().auto_info);
        let text = format!("origin {}\n", "word ".repeat(20));
        replace_text(&mut app, &text, Selection::point(0));
        app.render().await;
        let origin = app.terminal.backend().cursor_position();
        let original_scroll = {
            let (view, doc) = current_ref!(app.editor);
            doc.view_offset(view.id)
        };

        press_keys(&mut app, "g").await?;
        assert!(app.editor.autoinfo.is_some(), "g opens automatic help");
        assert!(app.terminal.backend().cursor_image().is_none());
        assert!(app.cursor_smear_frame.is_none());
        press_keys(&mut app, "w").await?;
        assert!(app.editor.autoinfo.is_none());
        assert_eq!(app.terminal.backend().cursor_position(), origin);
        assert!(app.terminal.backend().cursor_image().is_some());

        press_keys(&mut app, &label[..1]).await?;
        assert_eq!(app.terminal.backend().cursor_position(), origin);
        press_keys(&mut app, &label[1..]).await?;
        let (view, doc) = current_ref!(app.editor);
        let word_start = 7 + word_index * 5;
        assert_eq!(
            doc.selection(view.id).primary(),
            Range::new(word_start, word_start + 4),
            "the real gw key sequence must select the requested label",
        );
        assert_eq!(doc.text().to_string(), text);
        assert_eq!(doc.view_offset(view.id), original_scroll);
        let destination = app.terminal.backend().cursor_position();
        let jump = origin.0.abs_diff(destination.0);
        if word_index == 14 {
            assert!(jump > app.editor.config().cursor_smear.max_distance);
        }
        assert!(app.cursor_smear_frame.is_some(), "gw{label} must animate");
        assert!(app.terminal.backend().cursor_image().is_some());
        assert!(!app.terminal.backend().cursor_visible());
        app.paint_cursor_smear(
            tokio::time::Instant::now() + Duration::from_secs(2),
            app.terminal.backend().window_metrics().unwrap(),
        );
        assert!(app.cursor_smear_frame.is_none());
        let image = app.terminal.backend().cursor_image().unwrap();
        assert_eq!(
            (image.position.col as u16, image.position.row as u16),
            destination,
        );
        assert!(app.close().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn direct_goto_keeps_its_origin_across_automatic_help() -> anyhow::Result<()> {
    let mut app = moving_cursor(120).await?;
    app.paint_cursor_smear(
        tokio::time::Instant::now() + Duration::from_secs(2),
        app.terminal.backend().window_metrics().unwrap(),
    );
    let origin = app.terminal.backend().cursor_position();
    press_keys(&mut app, "g").await?;
    assert!(app.editor.autoinfo.is_some());
    assert!(app.terminal.backend().cursor_image().is_none());
    assert!(app.cursor_smear_frame.is_none());
    press_keys(&mut app, "h").await?;
    let (view, doc) = current_ref!(app.editor);
    assert_eq!(
        doc.selection(view.id)
            .primary()
            .cursor(doc.text().slice(..)),
        0
    );
    assert_ne!(app.terminal.backend().cursor_position(), origin);
    assert!(
        app.cursor_smear_frame.is_some(),
        "gh must retain the cursor before g"
    );
    assert!(app.terminal.backend().cursor_image().is_some());
    assert!(app.close().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn word_label_jump_that_scrolls_still_animates_to_the_new_cursor() -> anyhow::Result<()> {
    let mut app = application(120, true).await?;
    app.terminal.backend_mut().resize(120, 10);
    replace_text(&mut app, &"word\n".repeat(20), Selection::point(0));
    app.render().await;
    let original_scroll = {
        let (view, doc) = current_ref!(app.editor);
        assert_eq!(view.inner_area(doc).height, 8);
        doc.view_offset(view.id)
    };
    // The seventh candidate is on the bottom visible row. Default scrolloff
    // moves the viewport when accepting it, although the label was on screen.
    press_keys(&mut app, "gwag").await?;
    let (view, doc) = current_ref!(app.editor);
    let cursor = doc
        .selection(view.id)
        .primary()
        .cursor(doc.text().slice(..));
    assert_eq!(doc.text().char_to_line(cursor), 7);
    assert_ne!(doc.view_offset(view.id), original_scroll);
    assert!(
        app.cursor_smear_frame.is_some(),
        "automatic scrolling must not hide a gw jump"
    );
    assert!(app.terminal.backend().cursor_image().is_some());
    let destination = app.terminal.backend().cursor_position();
    app.paint_cursor_smear(
        tokio::time::Instant::now() + Duration::from_secs(2),
        app.terminal.backend().window_metrics().unwrap(),
    );
    assert!(app.cursor_smear_frame.is_none());
    let image = app.terminal.backend().cursor_image().unwrap();
    assert_eq!(
        (image.position.col as u16, image.position.row as u16),
        destination,
    );
    assert!(app.close().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn scrolling_the_viewport_without_moving_the_document_cursor_resets_animation(
) -> anyhow::Result<()> {
    let mut app = application(120, true).await?;
    app.terminal.backend_mut().resize(120, 10);
    replace_text(&mut app, &"word\n".repeat(20), Selection::point(20));
    app.render().await;
    let original_position = app.terminal.backend().cursor_position();
    let original_selection = {
        let (view, doc) = current!(app.editor);
        let selection = doc.selection(view.id).clone();
        let mut offset = doc.view_offset(view.id);
        offset.anchor += 5;
        doc.set_view_offset(view.id, offset);
        selection
    };
    app.render().await;
    let (view, doc) = current_ref!(app.editor);
    assert_eq!(doc.selection(view.id), &original_selection);
    assert_ne!(app.terminal.backend().cursor_position(), original_position);
    assert!(
        app.cursor_smear_frame.is_none(),
        "viewport scrolling is not a cursor jump"
    );
    let image = app.terminal.backend().cursor_image().unwrap();
    assert_eq!(
        (image.position.col as u16, image.position.row as u16),
        app.terminal.backend().cursor_position(),
    );
    assert!(app.close().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn automatic_help_appearing_before_a_redraw_pauses_the_pending_pixel_frame(
) -> anyhow::Result<()> {
    let mut app = moving_cursor(120).await?;
    app.editor.autoinfo = Some(helix_view::info::Info::new("Help", &[("g", "Goto")]));
    app.editor.needs_redraw = true;
    app.render_cursor_smear().await;
    assert!(app.cursor_smear_frame.is_none());
    assert!(app.terminal.backend().cursor_image().is_none());
    app.editor.autoinfo = None;
    set_selection(&mut app, Selection::point(16));
    app.render().await;
    assert!(
        app.cursor_smear_frame.is_some(),
        "help retains a valid logical origin"
    );
    assert!(app.close().await.is_empty());
    Ok(())
}
