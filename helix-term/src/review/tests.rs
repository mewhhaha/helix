use std::{path::Path, process::Command};

use super::*;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
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
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn run(source: &Path, command: &str, options: &[&str]) -> Result<Value> {
    execute(
        [command.to_owned(), source.to_str().unwrap().to_owned()]
            .into_iter()
            .chain(options.iter().map(|arg| (*arg).to_owned())),
    )
}

#[test]
fn agent_review_preserves_pr_snapshots_scopes_threads_and_reports_outdated_code() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repo = directory.path();
    let source = repo.join("source.rs");
    git(repo, &["init", "-b", "main"]);
    std::fs::write(&source, "common\nlet café = old;\n")?;
    git(repo, &["add", "source.rs"]);
    git(repo, &["commit", "-m", "common"]);
    let merge_base = git(repo, &["rev-parse", "HEAD"]);
    git(repo, &["branch", "feature"]);
    std::fs::write(&source, "main-only\nlet café = old;\n")?;
    git(repo, &["commit", "-am", "target moved"]);
    let target_commit = git(repo, &["rev-parse", "HEAD"]);
    git(repo, &["checkout", "feature"]);
    let reviewed = "common\nlet café = new;\n";
    std::fs::write(&source, reviewed)?;
    git(repo, &["commit", "-am", "feature"]);
    let head_commit = git(repo, &["rev-parse", "HEAD"]);
    let pr = "https://github.com/example/repository/pull/42";
    let started = run(&source, "start", &["--target", "main", "--pr", pr])?;
    let review_id = started["review"]["id"].as_str().unwrap();
    let snapshot = &started["review"]["snapshot"];
    assert_eq!(snapshot["target_commit"], target_commit);
    assert_eq!(snapshot["base_commit"], merge_base);
    assert_eq!(snapshot["head_commit"], head_commit);
    assert_eq!(started["review"]["pr"], pr);
    assert_ne!(target_commit, merge_base);
    assert!(run(
        &source,
        "add",
        &[
            "--line",
            "2",
            "--quote",
            "wrong code",
            "--body",
            "bad anchor"
        ]
    )
    .is_err());
    let finding = run(
        &source,
        "add",
        &[
            "--line",
            "2",
            "--start-column",
            "5",
            "--end-column",
            "9",
            "--quote",
            "café",
            "--author",
            "codex",
            "--body",
            "Why rename this?",
        ],
    )?;
    assert_eq!(finding["thread"]["anchor"]["quote"], "café");
    assert_eq!(finding["thread"]["messages"][0]["author"], "codex");
    run(
        &source,
        "reply",
        &[
            "--thread",
            "1",
            "--author",
            "developer",
            "--body",
            "For consistency.\nSee the caller.",
        ],
    )?;
    let removed = run(
        &source,
        "add",
        &[
            "--side",
            "base",
            "--line",
            "2",
            "--quote",
            "old",
            "--body",
            "Old text note",
        ],
    )?;
    assert_eq!(removed["thread"]["anchor"]["quote"], "let café = old;\n");
    run(&source, "resolve", &["--thread", "1"])?;
    assert_eq!(
        run(&source, "list", &[])?["threads"][0]["status"],
        "resolved"
    );
    run(&source, "reopen", &["--thread", "1"])?;
    let local = run(&source, "start", &["--target", "HEAD"])?;
    assert_ne!(local["review"]["id"], review_id);
    run(&source, "add", &["--line", "1", "--body", "Local note"])?;
    assert_eq!(
        run(&source, "list", &[])?["threads"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let first = run(&source, "list", &["--review", review_id])?;
    assert_eq!(first["threads"].as_array().unwrap().len(), 2);
    assert_eq!(
        first["threads"][0]["thread"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let original_anchor = first["threads"][0]["thread"]["original_anchor"].clone();

    std::fs::write(&source, format!("inserted\n{reviewed}"))?;
    git(repo, &["commit", "-am", "new head"]);
    assert!(run(
        &source,
        "add",
        &[
            "--review",
            review_id,
            "--line",
            "3",
            "--body",
            "stale finding"
        ]
    )
    .is_err());
    let shifted = run(&source, "list", &["--review", review_id])?;
    assert_eq!(shifted["threads"][0]["status"], "open");
    assert_eq!(shifted["threads"][0]["relocated"], true);
    assert_eq!(shifted["threads"][0]["location"]["start_line"], 3);
    let refreshed = run(&source, "start", &["--target", "main", "--pr", pr])?;
    assert_eq!(refreshed["review"]["id"], review_id);
    let refreshed = run(&source, "list", &[])?;
    assert_eq!(
        refreshed["threads"][0]["thread"]["snapshot"]["head_commit"],
        head_commit
    );
    assert_eq!(
        refreshed["threads"][0]["thread"]["original_anchor"],
        original_anchor
    );
    std::fs::write(&source, "inserted\ncommon\nlet tea = new;\n")?;
    assert_eq!(
        run(&source, "list", &[])?["threads"][0]["status"],
        "outdated"
    );
    assert_eq!(
        run(&source, "list", &["--all"])?["threads"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        std::fs::read_to_string(&source)?,
        "inserted\ncommon\nlet tea = new;\n"
    );
    Ok(())
}

#[test]
fn deleted_files_can_be_reviewed_and_pr_identity_survives_branch_renames() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repo = directory.path();
    let source = repo.join("deleted.rs");
    git(repo, &["init", "-b", "main"]);
    std::fs::write(&source, "let removed = true;\n")?;
    git(repo, &["add", "deleted.rs"]);
    git(repo, &["commit", "-m", "base"]);
    git(repo, &["checkout", "-b", "feature"]);
    std::fs::remove_file(&source)?;
    git(repo, &["commit", "-am", "delete file"]);
    let pr = "https://github.com/example/repository/pull/7";
    let started = run(&source, "start", &["--target", "main", "--pr", pr])?;
    let finding = run(
        &source,
        "add",
        &[
            "--side",
            "base",
            "--line",
            "1",
            "--body",
            "Is deleting this intended?",
        ],
    )?;
    assert_eq!(
        finding["thread"]["anchor"]["quote"],
        "let removed = true;\n"
    );
    git(repo, &["branch", "-m", "renamed-feature"]);
    let renamed = run(&source, "start", &["--target", "main", "--pr", pr])?;
    assert_eq!(renamed["review"]["id"], started["review"]["id"]);
    assert_eq!(renamed["review"]["branch"], "renamed-feature");
    assert_eq!(
        run(&source, "list", &[])?["threads"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(!source.exists(), "review recreated a deleted source file");
    Ok(())
}
