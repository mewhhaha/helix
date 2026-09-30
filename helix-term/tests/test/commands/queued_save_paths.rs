use std::{fs, path::PathBuf, time::SystemTime};

use helix_core::Transaction;
use helix_term::application::Application;
use helix_view::{document::DocumentSavedEvent, editor::EditorEvent, DocumentId};

use super::helpers::AppBuilder;

fn replace_text(app: &mut Application, doc_id: DocumentId, content: &str) {
    let view_id = app.editor.get_synced_view_id(doc_id);
    let view = app.editor.tree.get_mut(view_id);
    let doc = app.editor.documents.get_mut(&doc_id).unwrap();
    let transaction = Transaction::change(
        doc.text(),
        [(0, doc.text().len_chars(), Some(content.into()))].into_iter(),
    );
    assert!(doc.apply(&transaction, view_id));
    doc.append_changes_to_history(view);
}

async fn next_save(app: &mut Application) -> anyhow::Result<DocumentSavedEvent> {
    loop {
        match app.editor.wait_event().await {
            EditorEvent::DocumentSaved(result) => return result,
            event => {
                app.handle_editor_event(event).await;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_saves_keep_their_baseline_when_the_first_save_assigns_a_path() -> anyhow::Result<()>
{
    for save_as in [false, true] {
        let dir = tempfile::tempdir()?;
        let root = helix_stdx::path::canonicalize(dir.path());
        let original = root.join("original.txt");
        let destination = root.join("destination.txt");
        let mut builder = AppBuilder::new();
        if save_as {
            fs::write(&original, "original\n")?;
            builder = builder.with_file(&original, None);
        }
        let mut app = builder.build()?;
        let doc_id = app.editor.tree.get(app.editor.tree.focus).doc;

        replace_text(&mut app, doc_id, "first\n");
        app.editor.save(doc_id, Some(destination.clone()), false)?;
        replace_text(&mut app, doc_id, "second\n");
        app.editor.save(doc_id, Some(destination.clone()), false)?;

        let first = next_save(&mut app).await?;
        // Path reassignment must not use this older disk timestamp to detach the
        // shared baseline. Subsequent writes then have distinct mtimes even on
        // filesystems with coarse timestamp resolution.
        fs::File::open(&destination)?
            .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))?;
        app.handle_document_write(Ok(first));
        assert_eq!(
            app.editor.document(doc_id).unwrap().path(),
            Some(destination.as_path())
        );

        replace_text(&mut app, doc_id, "third\n");
        app.editor.save::<PathBuf>(doc_id, None, false)?;
        let second = next_save(&mut app).await?;
        app.handle_document_write(Ok(second));
        let third = next_save(&mut app).await?;
        app.handle_document_write(Ok(third));

        assert_eq!(
            fs::read_to_string(&destination)?,
            "third\n",
            "save_as={save_as}"
        );
        assert!(!app.editor.document(doc_id).unwrap().is_modified());
        if save_as {
            assert_eq!(fs::read_to_string(&original)?, "original\n");
        }
        let errors = app.close().await;
        assert!(errors.is_empty(), "{errors:?}");
    }
    Ok(())
}
