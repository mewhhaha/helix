use helix_core::Rope;
use helix_view::{
    current_ref,
    document::review_comments::{sidecar_path, CommentAnchor, CommentSide, ReviewComment},
};

use super::helpers::{assert_status_not_error, test_config, test_key_sequences, AppBuilder};

#[tokio::test(flavor = "multi_thread")]
async fn saved_comments_open_without_a_git_baseline() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source.txt");
    let text = Rope::from_str("source stays read-only\n");
    std::fs::write(&source, text.to_string())?;
    let sidecar = sidecar_path(&source);
    std::fs::write(
        &sidecar,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "comments": [ReviewComment {
                id: 1,
                anchor: CommentAnchor::new(CommentSide::Current, None, text.slice(..), 0..6),
                text: "Saved note".into(),
            }],
        }))?,
    )?;
    let mut app = AppBuilder::new().with_file(&source, None).build()?;
    let (_, doc) = current_ref!(app.editor);
    assert!(doc.review_diff_handle().is_none());
    assert!(doc.review_comments().is_empty());
    test_key_sequences(
        &mut app,
        vec![
            (
                Some("zr"),
                Some(&|app| {
                    let (view, doc) = current_ref!(app.editor);
                    assert!(view.diff_mode.enabled());
                    assert!(doc.is_diff_mode_read_only());
                    assert_eq!(doc.review_comments()[0].text, "Saved note");
                    assert!(view
                        .diff_mode
                        .display(doc)
                        .unwrap()
                        .comment_block(1)
                        .is_some());
                    assert_status_not_error(&app.editor);
                }),
            ),
            (
                Some("kA edited<esc>"),
                Some(&|app| {
                    let view = app.editor.tree.get(app.editor.tree.focus).review_view();
                    let doc = app.editor.document(view.doc).unwrap();
                    assert!(view.diff_mode.comment_cursor(doc, view.id).is_some());
                    assert_eq!(doc.review_comments()[0].text, "Saved note edited");
                    assert!(!doc.is_modified());
                    assert_eq!(doc.text(), &text);
                    assert_status_not_error(&app.editor);
                }),
            ),
        ],
        false,
    )
    .await?;
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&sidecar)?)?;
    assert_eq!(
        saved["threads"][0]["messages"][0]["text"],
        "Saved note edited"
    );
    assert_eq!(std::fs::read_to_string(source)?, text.to_string());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn normal_editing_commands_change_only_the_comment_and_keep_its_history() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source.txt");
    let source_text = "source stays read-only\n";
    std::fs::write(&source, source_text)?;
    let sidecar = sidecar_path(&source);
    let original = "alpha beta\nsecond line";
    std::fs::write(
        &sidecar,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "comments": [ReviewComment {
                id: 1,
                anchor: CommentAnchor::new(CommentSide::Current, None, Rope::from_str(source_text).slice(..), 0..6),
                text: original.into(),
            }],
        }))?,
    )?;
    let mut config = test_config();
    config.keys = toml::from_str(
        "[normal]\nQ = ['select_all', 'change_selection']\n[insert]\nC-e = 'normal_mode'\n",
    )?;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(&source, None)
        .build()?;
    let verify = |app: &helix_term::application::Application, expected: &str| {
        let doc = app.editor.document_by_path(&source).unwrap();
        assert_eq!(doc.review_comments()[0].text, expected);
        assert_eq!(doc.text().to_string(), source_text);
        assert!(!doc.is_modified());
        assert!(doc.is_diff_mode_read_only());
        assert_eq!(
            app.editor.documents().count(),
            1,
            "comment appeared in the buffer list"
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
        assert_eq!(saved["threads"][0]["messages"][0]["text"], expected);
        assert_status_not_error(&app.editor);
    };
    test_key_sequences(
        &mut app,
        vec![
            (Some("zrk%d"), Some(&|app| verify(app, ""))),
            (Some("u"), Some(&|app| verify(app, original))),
            (Some("U"), Some(&|app| verify(app, ""))),
            (Some("p"), Some(&|app| verify(app, original))),
            (
                Some("Qchanged note<C-e>"),
                Some(&|app| verify(app, "changed note")),
            ),
            (Some("%rX"), Some(&|app| verify(app, "XXXXXXXXXXXXX"))),
            (Some("u"), Some(&|app| verify(app, "changed note"))),
            (
                Some("Onew line<esc>"),
                Some(&|app| verify(app, "new line\nchanged note")),
            ),
            (
                Some("%sline<ret>cword<esc>"),
                Some(&|app| verify(app, "new word\nchanged note")),
            ),
            (
                Some(":goto 1<ret>d"),
                Some(&|app| {
                    let (view, doc) = current_ref!(app.editor);
                    assert!(view.review_source.is_none());
                    assert_eq!(doc.text().to_string(), source_text);
                    assert!(app
                        .editor
                        .status_msg
                        .as_ref()
                        .unwrap()
                        .0
                        .contains("read-only"));
                }),
            ),
            (
                Some("ku:w<ret>"),
                Some(&|app| verify(app, "new line\nchanged note")),
            ),
            (
                Some("%calpha alpha<esc>%salpha<ret>cBETA<esc>"),
                Some(&|app| {
                    verify(app, "BETA BETA");
                    let (view, doc) = current_ref!(app.editor);
                    assert_eq!(doc.selection(view.id).len(), 2);
                    let parent = view.review_view();
                    let source = app.editor.document(parent.doc).unwrap();
                    assert_eq!(
                        parent
                            .diff_mode
                            .comment_cursor(source, parent.id)
                            .unwrap()
                            .ranges
                            .len(),
                        2
                    );
                }),
            ),
            (Some("u"), Some(&|app| verify(app, "alpha alpha"))),
        ],
        false,
    )
    .await?;
    assert_eq!(std::fs::read_to_string(source)?, source_text);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn threads_reply_resolve_reopen_and_cancel_without_touching_source() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source.txt");
    let text = "source stays read-only\n";
    std::fs::write(&source, text)?;
    std::fs::write(
        sidecar_path(&source),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "comments": [ReviewComment { id: 1, anchor: CommentAnchor::new(CommentSide::Current, None, Rope::from_str(text).slice(..), 0..6), text: "Root note".into() }]
        }))?,
    )?;
    let mut app = AppBuilder::new().with_file(&source, None).build()?;
    let verify = |app: &helix_term::application::Application, resolved: bool| {
        let view = app.editor.tree.get(app.editor.tree.focus).review_view();
        let doc = app.editor.document(view.doc).unwrap();
        let thread = doc.review_store().thread(1).unwrap();
        assert_eq!(thread.resolved, resolved);
        assert_eq!(thread.messages.len(), 2);
        assert_eq!(thread.messages[0].text, "Root note");
        assert_eq!(thread.messages[1].text, "Reply body");
        assert_eq!(thread.original_anchor.range, 0..6);
        let display = view.diff_mode.display(doc).unwrap();
        assert_eq!(display.comment_block(1).is_none(), resolved);
        assert_eq!(display.comment_block(2).is_none(), resolved);
        assert!(doc.is_diff_mode_read_only());
        assert!(!doc.is_modified());
        assert_eq!(doc.text().to_string(), text);
        assert_status_not_error(&app.editor);
    };
    test_key_sequences(
        &mut app,
        vec![
            (
                Some("zrk cReply body<esc>"),
                Some(&|app| verify(app, false)),
            ),
            (
                Some(" cAnother reply<esc>"),
                Some(&|app| {
                    let source = app.editor.document_by_path(&source).unwrap();
                    let thread = source.review_store().thread(1).unwrap();
                    assert_eq!(thread.messages.len(), 3);
                    assert_eq!(thread.messages[0].text, "Root note");
                    assert_eq!(thread.messages[1].text, "Reply body");
                    assert_eq!(thread.messages[2].text, "Another reply");
                    assert_eq!(source.text().to_string(), text);
                    assert!(!source.is_modified());
                    assert_status_not_error(&app.editor);
                }),
            ),
            (Some(":review-delete<ret>"), Some(&|app| verify(app, false))),
            (Some("k:review-info<ret>"), Some(&|app| verify(app, false))),
            (Some(":review-resolve<ret>"), Some(&|app| verify(app, true))),
            (
                Some(":review-reopen 1<ret>"),
                Some(&|app| verify(app, false)),
            ),
            (
                Some(":review-reply 1<ret>discarded<C-c>"),
                Some(&|app| verify(app, false)),
            ),
            (
                Some(":review-comments<ret>"),
                Some(&|app| verify(app, false)),
            ),
            (Some("<esc>"), Some(&|app| verify(app, false))),
            (
                Some(":review-delete<ret>"),
                Some(&|app| {
                    let source = app.editor.document_by_path(&source).unwrap();
                    assert_eq!(source.review_store().thread(1).unwrap().messages.len(), 2);
                    assert_eq!(source.text().to_string(), text);
                    assert!(app
                        .editor
                        .status_msg
                        .as_ref()
                        .unwrap()
                        .0
                        .contains("not focused"));
                }),
            ),
            (
                Some("kk:review-delete<ret>"),
                Some(&|app| {
                    let doc = app.editor.document_by_path(&source).unwrap();
                    let thread = doc.review_store().thread(1).unwrap();
                    assert_eq!(thread.messages.len(), 1);
                    assert_eq!(thread.messages[0].text, "Reply body");
                    assert_eq!(thread.original_anchor.range, 0..6);
                    assert!(doc.review_comment(1).is_none());
                    assert!(doc.is_diff_mode_read_only());
                    assert_eq!(doc.text().to_string(), text);
                    let saved: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(sidecar_path(&source)).unwrap())
                            .unwrap();
                    assert_eq!(saved["threads"][0]["messages"].as_array().unwrap().len(), 1);
                    assert_status_not_error(&app.editor);
                }),
            ),
            (
                Some("k:review-delete<ret>"),
                Some(&|app| {
                    let source = app.editor.document_by_path(&source).unwrap();
                    assert!(source.review_comments().is_empty());
                    assert!(source.review_store().thread(1).is_none());
                    assert!(source.is_diff_mode_read_only());
                    assert!(!source.is_modified());
                    assert_eq!(source.text().to_string(), text);
                    assert_status_not_error(&app.editor);
                }),
            ),
        ],
        false,
    )
    .await?;
    assert!(!sidecar_path(&source).exists());
    assert_eq!(std::fs::read_to_string(&source)?, text);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_updates_reload_while_drafts_keep_conflicting_writes() -> anyhow::Result<()> {
    use helix_view::document::review_comments::ReviewStore;
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source.txt");
    let text = "source stays read-only\n";
    std::fs::write(&source, text)?;
    std::fs::write(
        sidecar_path(&source),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "comments": [ReviewComment { id: 1, anchor: CommentAnchor::new(CommentSide::Current, None, Rope::from_str(text).slice(..), 0..6), text: "Root note".into() }]
        }))?,
    )?;
    let mut app = AppBuilder::new().with_file(&source, None).build()?;
    async fn send_keys(
        app: &mut helix_term::application::Application,
        keys: &str,
    ) -> anyhow::Result<()> {
        #[cfg(windows)]
        use crossterm::event::{Event, KeyEvent};
        #[cfg(not(windows))]
        use termina::event::{Event, KeyEvent};
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for key in helix_view::input::parse_macro(keys)? {
            tx.send(Ok(Event::Key(KeyEvent::from(key))))?;
        }
        let mut stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        assert!(app.event_loop_until_idle(&mut stream).await);
        Ok(())
    }
    send_keys(&mut app, "zr").await?;
    ReviewStore::transaction(&source, |store| {
        store.reply(1, "codex".into(), "Agent reply".into())?;
        Ok(())
    })?;
    assert!(app.editor.reload_review_comments());
    {
        let (view, doc) = current_ref!(app.editor);
        assert!(view
            .diff_mode
            .display(doc)
            .unwrap()
            .comment_block(2)
            .is_some());
        assert_eq!(
            doc.review_store().thread(1).unwrap().messages[1].author,
            "codex"
        );
    }
    // Enter a real comment buffer, then let an agent save a concurrent edit.
    app.editor.focus_review_message(1)?;
    send_keys(&mut app, "A local draft").await?;
    ReviewStore::transaction(&source, |store| {
        assert!(store.set_text(1, "Agent changed root".into()));
        Ok(())
    })?;
    assert!(!app.editor.reload_review_comments());
    test_key_sequences(
        &mut app,
        vec![
            (
                Some("<esc>"),
                Some(&|app| {
                    assert!(app
                        .editor
                        .status_msg
                        .as_ref()
                        .unwrap()
                        .0
                        .contains("changed outside Helix"));
                    let data: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(sidecar_path(&source)).unwrap())
                            .unwrap();
                    assert_eq!(
                        data["threads"][0]["messages"][0]["text"],
                        "Agent changed root"
                    );
                }),
            ),
            (
                Some("i<C-c>"),
                Some(&|app| {
                    let view = app.editor.tree.get(app.editor.tree.focus).review_view();
                    let doc = app.editor.document(view.doc).unwrap();
                    assert_eq!(doc.review_comment(1).unwrap().text, "Agent changed root");
                    assert!(!doc.review_comments_dirty());
                    assert_eq!(doc.text().to_string(), text);
                }),
            ),
        ],
        false,
    )
    .await?;
    assert_eq!(std::fs::read_to_string(&source)?, text);
    Ok(())
}

#[cfg(feature = "git")]
#[tokio::test(flavor = "multi_thread")]
async fn agent_pr_threads_resume_in_helix_and_follow_the_selected_target() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source.txt");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(directory.path())
            .args(args)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
            .env("GIT_CONFIG_VALUE_0", "false")
            .env("GIT_AUTHOR_NAME", "review-test")
            .env("GIT_AUTHOR_EMAIL", "review@example.com")
            .env("GIT_COMMITTER_NAME", "review-test")
            .env("GIT_COMMITTER_EMAIL", "review@example.com")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    std::fs::write(&source, "old row\n")?;
    git(&["add", "source.txt"]);
    git(&["commit", "-m", "base"]);
    git(&["checkout", "-b", "feature"]);
    std::fs::write(&source, "new row\n")?;
    git(&["commit", "-am", "feature"]);
    let cli = |command: &str, options: &[&str]| {
        helix_term::review::execute(
            [command.to_owned(), source.to_str().unwrap().to_owned()]
                .into_iter()
                .chain(options.iter().map(|value| (*value).to_owned())),
        )
    };
    let pr = "https://github.com/example/repository/pull/42";
    let started = cli("start", &["--target", "main", "--pr", pr])?;
    cli(
        "add",
        &[
            "--side",
            "base",
            "--line",
            "1",
            "--author",
            "codex",
            "--body",
            "Why remove this?",
        ],
    )?;
    cli(
        "add",
        &[
            "--line",
            "1",
            "--author",
            "codex",
            "--body",
            "Please check this.",
        ],
    )?;
    let mut app = AppBuilder::new().with_file(&source, None).build()?;
    let verify = |app: &helix_term::application::Application, visible: bool| {
        let (view, doc) = current_ref!(app.editor);
        let display = view.diff_mode.display(doc).unwrap();
        assert_eq!(display.comment_block(1).is_some(), visible);
        assert_eq!(display.comment_block(2).is_some(), visible);
        assert!(doc.is_diff_mode_read_only());
        assert!(!doc.is_modified());
        assert_eq!(doc.text().to_string(), "new row\n");
        if visible {
            let review = doc.review_session().unwrap();
            assert_eq!(review.id, started["review"]["id"].as_str().unwrap());
            assert_eq!(review.pr.as_deref(), Some(pr));
            assert_eq!(
                review.snapshot.head_commit.as_deref(),
                started["review"]["snapshot"]["head_commit"].as_str()
            );
        }
        assert_status_not_error(&app.editor);
    };
    test_key_sequences(
        &mut app,
        vec![
            (Some("zr"), Some(&|app| verify(app, true))),
            (
                Some(":review-mode HEAD<ret>"),
                Some(&|app| verify(app, false)),
            ),
            (
                Some(":review-mode main --pr https://github.com/example/repository/pull/42<ret>"),
                Some(&|app| verify(app, true)),
            ),
        ],
        false,
    )
    .await?;
    cli("start", &["--target", "HEAD"])?;
    let local = cli("add", &["--line", "1", "--body", "Saved HEAD review"])?;
    let message = local["thread"]["messages"][0]["id"].as_u64().unwrap();
    let mut reopened = AppBuilder::new().with_file(&source, None).build()?;
    test_key_sequences(
        &mut reopened,
        vec![(
            Some("zr"),
            Some(&|app| {
                let (view, doc) = current_ref!(app.editor);
                assert!(view
                    .diff_mode
                    .display(doc)
                    .unwrap()
                    .comment_block(message)
                    .is_some());
                assert_eq!(doc.review_diff_reference(), Some("HEAD"));
                assert_eq!(
                    doc.review_session()
                        .unwrap()
                        .snapshot
                        .head_commit
                        .as_deref(),
                    started["review"]["snapshot"]["head_commit"].as_str()
                );
                assert_status_not_error(&app.editor);
            }),
        )],
        false,
    )
    .await?;
    assert_eq!(std::fs::read_to_string(&source)?, "new row\n");
    Ok(())
}
