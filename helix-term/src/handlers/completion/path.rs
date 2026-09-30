use std::{
    borrow::Cow,
    fs,
    ops::Range,
    path::{Path, PathBuf},
    str::FromStr as _,
    sync::Arc,
};

use helix_core::{self as core, completion::CompletionProvider, Selection, Transaction};
use helix_event::TaskHandle;
use helix_stdx::path::{self, canonicalize, fold_home_dir, get_path_suffix};
use helix_stdx::Url;
use helix_view::{document::SavePoint, handlers::completion::ResponseContext, Document};

use crate::handlers::completion::{item::CompletionResponse, CompletionItem, CompletionItems};

pub(crate) fn path_completion(
    selection: Selection,
    doc: &Document,
    handle: TaskHandle,
    savepoint: Arc<SavePoint>,
) -> Option<impl FnOnce() -> CompletionResponse> {
    if !doc.path_completion_enabled() {
        return None;
    }

    let text = doc.text().clone();
    let cursor = selection.primary().cursor(text.slice(..));
    let (dir_path, primary_edit) = path_at_cursor(&text, cursor, doc)?;
    let mut edits = vec![primary_edit];
    for (index, range) in selection.iter().enumerate() {
        if index == selection.primary_index() {
            continue;
        }
        let Some((directory, edit)) = path_at_cursor(&text, range.cursor(text.slice(..)), doc)
        else {
            continue;
        };
        // The primary cursor's candidates only apply to paths in the same directory.
        // Prioritize its edit when several cursors occupy the same filename.
        if directory == dir_path
            && !edits
                .iter()
                .any(|other| edit.start <= other.end && other.start <= edit.end)
        {
            edits.push(edit);
        }
    }
    edits.sort_unstable_by_key(|edit| edit.start);

    if handle.is_canceled() {
        return None;
    }

    // TODO: handle properly in the future
    const PRIORITY: i8 = 1;
    let future = move || {
        let Ok(read_dir) = std::fs::read_dir(&dir_path) else {
            return CompletionResponse {
                items: CompletionItems::Other(Vec::new()),
                provider: CompletionProvider::Path,
                context: ResponseContext {
                    is_incomplete: false,
                    priority: PRIORITY,
                    savepoint,
                },
            };
        };

        let res: Vec<_> = read_dir
            .filter_map(Result::ok)
            .filter_map(|dir_entry| {
                dir_entry
                    .metadata()
                    .ok()
                    .and_then(|md| Some((dir_entry.file_name().into_string().ok()?, md)))
            })
            .map_while(|(file_name, md)| {
                if handle.is_canceled() {
                    return None;
                }

                let kind = path_kind(&md);
                let documentation = path_documentation(&md, &dir_path.join(&file_name), kind);

                let transaction = Transaction::change(
                    &text,
                    edits
                        .iter()
                        .map(|edit| (edit.start, edit.end, Some((&file_name).into()))),
                );

                Some(CompletionItem::Other(core::CompletionItem {
                    kind: Cow::Borrowed(kind),
                    label: file_name.into(),
                    transaction,
                    documentation: Some(documentation),
                    provider: CompletionProvider::Path,
                }))
            })
            .collect();
        CompletionResponse {
            items: CompletionItems::Other(res),
            provider: CompletionProvider::Path,
            context: ResponseContext {
                is_incomplete: false,
                priority: PRIORITY,
                savepoint,
            },
        }
    };

    Some(future)
}

fn path_at_cursor(
    text: &core::Rope,
    cursor: usize,
    doc: &Document,
) -> Option<(PathBuf, Range<usize>)> {
    let cur_line = text.char_to_line(cursor);
    let start = text.line_to_char(cur_line).max(cursor.saturating_sub(1000));
    let line_until_cursor = text.slice(start..cursor);

    let (dir_path, filename_len) =
        get_path_suffix(line_until_cursor, false).and_then(|matched_path| {
            let matched_path = Cow::from(matched_path);
            let path: Cow<_> = if matched_path.starts_with("file://") {
                Url::from_str(&matched_path)
                    .ok()
                    .and_then(|url| url.to_file_path().ok())?
                    .into()
            } else {
                Path::new(&*matched_path).into()
            };
            let path = path::expand(&path);
            let parent_dir = doc.path().and_then(|dp| dp.parent());
            let path = match parent_dir {
                Some(parent_dir) if path.is_relative() => parent_dir.join(&path),
                _ => path.into_owned(),
            };
            // Measure the source spelling, which can differ from an expanded or decoded path.
            let filename_len = source_filename_len(&matched_path);

            if filename_len == 0 {
                Some((PathBuf::from(path.as_path()), 0))
            } else {
                path.parent()
                    .map(|parent_path| (PathBuf::from(parent_path), filename_len))
            }
        })?;
    Some((
        canonicalize(dir_path),
        cursor.checked_sub(filename_len)?..cursor,
    ))
}

fn source_filename_len(path: &str) -> usize {
    let file_uri = path.starts_with("file://");
    let mut filename_start = 0;
    for (index, ch) in path.char_indices() {
        if std::path::is_separator(ch) {
            filename_start = index + ch.len_utf8();
        } else if file_uri && ch == '%' {
            let separator = match path.as_bytes().get(index + 1..index + 3) {
                Some([b'2', b'F' | b'f']) => Some('/'),
                Some([b'5', b'C' | b'c']) => Some('\\'),
                _ => None,
            };
            if separator.is_some_and(std::path::is_separator) {
                filename_start = index + 3;
            }
        }
    }
    path[filename_start..].chars().count()
}

#[cfg(unix)]
fn path_documentation(md: &fs::Metadata, full_path: &Path, kind: &str) -> String {
    let full_path = fold_home_dir(canonicalize(full_path));
    let full_path_name = full_path.to_string_lossy();

    use std::os::unix::prelude::PermissionsExt;
    let mode = md.permissions().mode();

    let perms = [
        (libc::S_IRUSR, 'r'),
        (libc::S_IWUSR, 'w'),
        (libc::S_IXUSR, 'x'),
        (libc::S_IRGRP, 'r'),
        (libc::S_IWGRP, 'w'),
        (libc::S_IXGRP, 'x'),
        (libc::S_IROTH, 'r'),
        (libc::S_IWOTH, 'w'),
        (libc::S_IXOTH, 'x'),
    ]
    .into_iter()
    .fold(String::with_capacity(9), |mut acc, (p, s)| {
        // This cast is necessary on some platforms such as macos as `mode_t` is u16 there
        #[allow(clippy::unnecessary_cast)]
        acc.push(if mode & (p as u32) > 0 { s } else { '-' });
        acc
    });

    // TODO it would be great to be able to individually color the documentation,
    // but this will likely require a custom doc implementation (i.e. not `lsp::Documentation`)
    // and/or different rendering in completion.rs
    format!(
        "type: `{kind}`\n\
         permissions: `[{perms}]`\n\
         full path: `{full_path_name}`",
    )
}

#[cfg(not(unix))]
fn path_documentation(_md: &fs::Metadata, full_path: &Path, kind: &str) -> String {
    let full_path = fold_home_dir(canonicalize(full_path));
    let full_path_name = full_path.to_string_lossy();
    format!("type: `{kind}`\nfull path: `{full_path_name}`",)
}

#[cfg(unix)]
fn path_kind(md: &fs::Metadata) -> &'static str {
    if md.is_symlink() {
        "link"
    } else if md.is_dir() {
        "folder"
    } else {
        use std::os::unix::fs::FileTypeExt;
        if md.file_type().is_block_device() {
            "block"
        } else if md.file_type().is_socket() {
            "socket"
        } else if md.file_type().is_char_device() {
            "char_device"
        } else if md.file_type().is_fifo() {
            "fifo"
        } else {
            "file"
        }
    }
}

#[cfg(not(unix))]
fn path_kind(md: &fs::Metadata) -> &'static str {
    if md.is_symlink() {
        "link"
    } else if md.is_dir() {
        "folder"
    } else {
        "file"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use helix_core::{syntax, Rope};
    use helix_event::TaskController;
    use helix_view::{editor::Config, View};

    fn complete(
        directory: &Path,
        source: &str,
        cursors: &[usize],
        primary: usize,
        filename: &str,
    ) -> String {
        fs::write(directory.join(filename), "").unwrap();
        let config = Config {
            path_completion: true,
            ..Config::default()
        };
        let gutters = config.gutters.clone();
        let mut doc = Document::from(
            Rope::from_str(source),
            None,
            Arc::new(ArcSwap::from_pointee(config)),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );
        doc.set_path(Some(&directory.join("buffer.txt")));
        let view = View::new(doc.id(), gutters);
        let selection = Selection::new(
            cursors
                .iter()
                .map(|&cursor| core::Range::point(cursor))
                .collect(),
            primary,
        );
        doc.set_selection(view.id, selection.clone());
        let savepoint = doc.savepoint(&view);
        let mut controller = TaskController::new();
        let response = path_completion(selection, &doc, controller.restart(), savepoint).unwrap()();
        let CompletionItems::Other(items) = response.items else {
            panic!("expected path completion items");
        };
        let item = items
            .into_iter()
            .find(|item| matches!(item, CompletionItem::Other(item) if item.label == filename))
            .unwrap();
        let CompletionItem::Other(item) = item else {
            unreachable!()
        };
        let mut text = doc.text().clone();
        assert!(item.transaction.apply(&mut text));
        text.to_string()
    }

    #[tokio::test]
    async fn other_cursor_at_start_is_not_changed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(complete(dir.path(), "./fo\n", &[0, 4], 1, "foo"), "./foo\n");
    }

    #[tokio::test]
    async fn each_cursor_replaces_its_own_filename_prefix() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            complete(dir.path(), "./f\n./fo\n", &[3, 8], 1, "foo"),
            "./foo\n./foo\n"
        );
    }

    #[tokio::test]
    async fn nonpath_text_and_other_directories_are_not_changed() {
        let dir = tempfile::tempdir().unwrap();
        let source = "plain\nother/ba\n./fo\n";
        assert_eq!(
            complete(dir.path(), source, &[5, 14, 19], 2, "foo"),
            "plain\nother/ba\n./foo\n"
        );
    }

    #[tokio::test]
    async fn overlapping_prefixes_only_replace_the_primary_filename() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            complete(dir.path(), "./foo\n", &[3, 5], 1, "foobar"),
            "./foobar\n"
        );
    }

    #[tokio::test]
    async fn unicode_prefixes_use_character_offsets() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            complete(dir.path(), "./é\n./éx\n", &[3, 8], 1, "éxample"),
            "./éxample\n./éxample\n"
        );
    }

    #[tokio::test]
    async fn trailing_slash_inserts_without_deleting_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(complete(dir.path(), "./\n", &[2], 0, "foo"), "./foo\n");
    }

    #[tokio::test]
    async fn file_uri_replaces_the_encoded_source_filename() {
        let dir = tempfile::tempdir().unwrap();
        let uri = Url::from_file_path(dir.path().join("file"))
            .unwrap()
            .to_string();
        let prefix = uri.strip_suffix("file").unwrap();
        let source = format!("{prefix}%66o\n");
        let cursor = source.chars().count() - 1;
        assert_eq!(
            complete(dir.path(), &source, &[cursor], 0, "foo"),
            format!("{prefix}foo\n")
        );
    }

    #[tokio::test]
    async fn file_uri_preserves_an_encoded_directory_separator() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let prefix = Url::from_directory_path(dir.path()).unwrap().to_string();

        for separator in ["%2F", "%2f"] {
            let source = format!("{prefix}sub{separator}fo\n");
            let cursor = source.chars().count() - 1;
            assert_eq!(
                complete(&sub, &source, &[cursor], 0, "foo"),
                format!("{prefix}sub{separator}foo\n")
            );
        }
    }

    #[tokio::test]
    async fn file_uri_ending_with_an_encoded_separator_inserts_in_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let prefix = Url::from_directory_path(dir.path()).unwrap().to_string();
        let source = format!("{prefix}sub%2F\n");
        let cursor = source.chars().count() - 1;

        assert_eq!(
            complete(&sub, &source, &[cursor], 0, "foo"),
            format!("{prefix}sub%2Ffoo\n")
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn file_uri_preserves_an_encoded_backslash_separator() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let prefix = Url::from_directory_path(dir.path()).unwrap().to_string();

        for separator in ["%5C", "%5c"] {
            let source = format!("{prefix}sub{separator}fo\n");
            let cursor = source.chars().count() - 1;
            assert_eq!(
                complete(&sub, &source, &[cursor], 0, "foo"),
                format!("{prefix}sub{separator}foo\n")
            );
        }
    }

    #[tokio::test]
    async fn ordinary_paths_keep_percent_sequences_as_filename_characters() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            complete(dir.path(), "./sub%2Ffo\n", &[10], 0, "sub%2Ffoo"),
            "./sub%2Ffoo\n"
        );
    }

    #[tokio::test]
    async fn tilde_keeps_the_source_prefix_and_resolves_the_home_directory() {
        let Ok(home) = path::home_dir() else {
            return;
        };
        let doc = Document::default(
            Arc::new(ArcSwap::from_pointee(Config::default())),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );
        let (directory, edit) = path_at_cursor(&Rope::from_str("~/fo"), 4, &doc).unwrap();
        assert_eq!(directory, canonicalize(home));
        assert_eq!(edit, 2..4);
    }
}
