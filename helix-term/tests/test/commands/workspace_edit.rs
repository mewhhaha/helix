use std::{fs, io::ErrorKind, path::PathBuf};

use helix_core::Transaction;
use helix_lsp::{lsp, OffsetEncoding};
use helix_view::{
    editor::Action,
    handlers::lsp::{ApplyEditError, ApplyEditErrorKind},
};

use super::helpers::AppBuilder;

type ResourceOptions = Option<(Option<bool>, Option<bool>)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExistingDestinationOutcome {
    Decline,
    Ignore,
    Overwrite,
}

fn existing_destination_cases() -> [(&'static str, ResourceOptions, ExistingDestinationOutcome); 8]
{
    use ExistingDestinationOutcome::*;
    [
        ("omitted options", None, Decline),
        ("default options", Some((None, None)), Decline),
        ("overwrite false", Some((Some(false), None)), Decline),
        ("both false", Some((Some(false), Some(false))), Decline),
        ("ignore", Some((None, Some(true))), Ignore),
        (
            "overwrite false and ignore",
            Some((Some(false), Some(true))),
            Ignore,
        ),
        ("overwrite", Some((Some(true), None)), Overwrite),
        (
            "overwrite wins over ignore",
            Some((Some(true), Some(true))),
            Overwrite,
        ),
    ]
}

fn create(path: &std::path::Path, options: ResourceOptions) -> lsp::ResourceOp {
    lsp::ResourceOp::Create(lsp::CreateFile {
        uri: lsp::Url::from_file_path(path).unwrap(),
        options: options.map(|(overwrite, ignore_if_exists)| lsp::CreateFileOptions {
            overwrite,
            ignore_if_exists,
        }),
        annotation_id: None,
    })
}

fn rename(
    old_path: &std::path::Path,
    new_path: &std::path::Path,
    options: ResourceOptions,
) -> lsp::ResourceOp {
    lsp::ResourceOp::Rename(lsp::RenameFile {
        old_uri: lsp::Url::from_file_path(old_path).unwrap(),
        new_uri: lsp::Url::from_file_path(new_path).unwrap(),
        options: options.map(|(overwrite, ignore_if_exists)| lsp::RenameFileOptions {
            overwrite,
            ignore_if_exists,
        }),
        annotation_id: None,
    })
}

fn workspace_edit(operations: Vec<lsp::ResourceOp>) -> lsp::WorkspaceEdit {
    lsp::WorkspaceEdit {
        document_changes: Some(lsp::DocumentChanges::Operations(
            operations
                .into_iter()
                .map(lsp::DocumentChangeOperation::Op)
                .collect(),
        )),
        ..Default::default()
    }
}

fn assert_existing_destination_result(
    result: Result<(), ApplyEditError>,
    outcome: ExistingDestinationOutcome,
    case: &str,
) {
    if outcome == ExistingDestinationOutcome::Decline {
        let error = result.expect_err(case);
        assert_eq!(error.failed_change_idx, 1, "{case}");
        match error.kind {
            ApplyEditErrorKind::IoError(error) => {
                assert_eq!(error.kind(), ErrorKind::AlreadyExists, "{case}");
            }
            error => panic!("{case}: unexpected error {error:?}"),
        }
    } else {
        result.unwrap_or_else(|error| panic!("{case}: {error:?}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_create_respects_existing_destination_options() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = helix_stdx::path::canonicalize(dir.path());
    let mut app = AppBuilder::new().build()?;

    for (index, (case, options, outcome)) in existing_destination_cases().into_iter().enumerate() {
        let case_dir = root.join(index.to_string());
        fs::create_dir(&case_dir)?;
        let destination = case_dir.join("destination.txt");
        let before = case_dir.join("before.txt");
        let after = case_dir.join("after.txt");
        fs::write(&destination, b"existing destination\n")?;

        let edit = workspace_edit(vec![
            create(&before, None),
            create(&destination, options),
            create(&after, None),
        ]);
        assert_existing_destination_result(
            app.editor
                .apply_workspace_edit(OffsetEncoding::Utf16, &edit),
            outcome,
            case,
        );

        assert!(before.exists(), "{case}");
        assert_eq!(
            after.exists(),
            outcome != ExistingDestinationOutcome::Decline,
            "{case}"
        );
        let expected: &[u8] = if outcome == ExistingDestinationOutcome::Overwrite {
            b""
        } else {
            b"existing destination\n"
        };
        assert_eq!(fs::read(&destination)?, expected, "{case}");
    }

    let errors = app.close().await;
    assert!(errors.is_empty(), "{errors:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_rename_respects_existing_destination_options() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = helix_stdx::path::canonicalize(dir.path());
    let mut app = AppBuilder::new().build()?;

    for (index, (case, options, outcome)) in existing_destination_cases().into_iter().enumerate() {
        let case_dir = root.join(index.to_string());
        fs::create_dir(&case_dir)?;
        let source = case_dir.join("source.txt");
        let destination = case_dir.join("destination.txt");
        let before = case_dir.join("before.txt");
        let after = case_dir.join("after.txt");
        fs::write(&source, b"source content\n")?;
        fs::write(&destination, b"existing destination\n")?;
        let doc_id = app.editor.open(&source, Action::Load)?;

        let edit = workspace_edit(vec![
            create(&before, None),
            rename(&source, &destination, options),
            create(&after, None),
        ]);
        assert_existing_destination_result(
            app.editor
                .apply_workspace_edit(OffsetEncoding::Utf16, &edit),
            outcome,
            case,
        );

        assert!(before.exists(), "{case}");
        assert_eq!(
            after.exists(),
            outcome != ExistingDestinationOutcome::Decline,
            "{case}"
        );
        let expected_path = if outcome == ExistingDestinationOutcome::Overwrite {
            assert!(!source.exists(), "{case}");
            assert_eq!(fs::read(&destination)?, b"source content\n", "{case}");
            &destination
        } else {
            assert_eq!(fs::read(&source)?, b"source content\n", "{case}");
            assert_eq!(fs::read(&destination)?, b"existing destination\n", "{case}");
            &source
        };
        assert_eq!(
            app.editor.document(doc_id).unwrap().path(),
            Some(expected_path.as_path()),
            "{case}"
        );
    }

    let errors = app.close().await;
    assert!(errors.is_empty(), "{errors:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_directory_rename_updates_open_descendants_and_save_paths() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let root = helix_stdx::path::canonicalize(dir.path());
    let source = root.join("old");
    let destination = root.join("new");
    let direct = source.join("direct.txt");
    let nested = source.join("nested/file.txt");
    let sibling = root.join("old-sibling/file.txt");
    fs::create_dir_all(nested.parent().unwrap())?;
    fs::create_dir_all(sibling.parent().unwrap())?;
    fs::write(&direct, b"direct content\n")?;
    fs::write(&nested, b"nested content\n")?;
    fs::write(&sibling, b"sibling content\n")?;

    let mut app = AppBuilder::new()
        .with_file(&direct, None)
        .with_file(&nested, None)
        .with_file(&sibling, None)
        .build()?;
    let direct_id = app.editor.document_by_path(&direct).unwrap().id();
    let nested_id = app.editor.document_by_path(&nested).unwrap().id();
    let sibling_id = app.editor.document_by_path(&sibling).unwrap().id();

    let edit = workspace_edit(vec![rename(&source, &destination, None)]);
    app.editor
        .apply_workspace_edit(OffsetEncoding::Utf16, &edit)
        .expect("directory rename should succeed");

    for (doc_id, relative_path, content) in [
        (direct_id, "direct.txt", "saved direct\n"),
        (nested_id, "nested/file.txt", "saved nested\n"),
    ] {
        let expected_path = destination.join(relative_path);
        assert_eq!(
            app.editor.document(doc_id).unwrap().path(),
            Some(expected_path.as_path())
        );
        assert_eq!(
            app.editor.document_by_path(&expected_path).unwrap().id(),
            doc_id
        );
        let view_id = app.editor.get_synced_view_id(doc_id);
        let doc = app.editor.document_mut(doc_id).unwrap();
        let transaction = Transaction::change(
            doc.text(),
            [(0, doc.text().len_chars(), Some(content.into()))].into_iter(),
        );
        assert!(doc.apply(&transaction, view_id));
        app.editor.save::<PathBuf>(doc_id, None, false)?;
    }
    app.editor.flush_writes().await?;

    assert!(!source.exists());
    assert!(app.editor.document_by_path(&direct).is_none());
    assert!(app.editor.document_by_path(&nested).is_none());
    assert_eq!(fs::read(destination.join("direct.txt"))?, b"saved direct\n");
    assert_eq!(
        fs::read(destination.join("nested/file.txt"))?,
        b"saved nested\n"
    );
    assert_eq!(
        app.editor.document(sibling_id).unwrap().path(),
        Some(sibling.as_path())
    );
    assert_eq!(fs::read(&sibling)?, b"sibling content\n");

    let errors = app.close().await;
    assert!(errors.is_empty(), "{errors:?}");
    Ok(())
}
