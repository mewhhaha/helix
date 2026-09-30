use helix_loader::workspace_trust::TrustQuery;
use helix_view::editor::{ConfigEvent, ImplicitTrustLevelConfig};

use super::helpers::AppBuilder;

#[cfg(feature = "git")]
fn git(root: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=helix-test",
            "-c",
            "user.email=test@helix.org",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_TERMINAL_PROMPT", "false")
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(feature = "git")]
async fn wait_for_baseline(
    app: &mut helix_term::application::Application,
    document: helix_view::DocumentId,
    expected: &str,
) -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if app
                .editor
                .document(document)
                .unwrap()
                .diff_handle()
                .is_some_and(|diff| diff.load().diff_base() == expected)
            {
                break;
            }
            let event = app.editor.wait_event().await;
            app.handle_editor_event(event).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_trust_updates_apply_levels_and_globs_and_refresh_git_inputs() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let root = helix_stdx::path::canonicalize(directory.path());
    let path = root.join("tracked.txt");
    std::fs::write(&path, "initial\n")?;
    #[cfg(feature = "git")]
    {
        git(&root, &["init", "-b", "main"])?;
        git(&root, &["add", "tracked.txt"])?;
        git(&root, &["commit", "-m", "initial"])?;
    }
    let mut app = AppBuilder::new().with_file(&path, None).build()?;
    let document = app.editor.tree.get(app.editor.tree.focus).doc;
    assert!(app
        .editor
        .workspace_trust
        .query(&root, TrustQuery::Git)
        .is_trusted());
    #[cfg(feature = "git")]
    {
        wait_for_baseline(&mut app, document, "initial\n").await?;
        std::fs::write(&path, "new commit\n")?;
        git(&root, &["add", "tracked.txt"])?;
        git(&root, &["commit", "-m", "updated"])?;
    }

    let mut config = (*app.editor.config()).clone();
    config.workspace_trust.level = ImplicitTrustLevelConfig::None;
    config.workspace_trust.trusted.clear();
    app.handle_config_events(ConfigEvent::Update(Box::new(config.clone())));
    assert!(!app
        .editor
        .workspace_trust
        .query(&root, TrustQuery::Git)
        .is_trusted());
    #[cfg(feature = "git")]
    wait_for_baseline(&mut app, document, "new commit\n").await?;
    assert_eq!(app.editor.document(document).unwrap().text(), "initial\n");

    // A matching glob grants Git trust independently of the implicit level.
    config.workspace_trust.trusted.push("**".into());
    app.handle_config_events(ConfigEvent::Update(Box::new(config.clone())));
    assert!(app
        .editor
        .workspace_trust
        .query(&root, TrustQuery::Git)
        .is_trusted());
    config.workspace_trust.trusted.clear();
    app.handle_config_events(ConfigEvent::Update(Box::new(config.clone())));
    assert!(!app
        .editor
        .workspace_trust
        .query(&root, TrustQuery::Git)
        .is_trusted());
    config.workspace_trust.level = ImplicitTrustLevelConfig::Insecure;
    app.handle_config_events(ConfigEvent::Update(Box::new(config)));
    assert!(app
        .editor
        .workspace_trust
        .query(&root, TrustQuery::Git)
        .is_trusted());

    let errors = app.close().await;
    assert!(errors.is_empty(), "{errors:?}");
    Ok(())
}
