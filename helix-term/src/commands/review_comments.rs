//! Route comment editing through ordinary writable buffers; source stays read-only.

use helix_view::document::review_comments::{CommentAnchor, CommentSide};

use super::*;

pub(super) fn create(cx: &mut Context) -> anyhow::Result<()> {
    if cx.editor.focused_review_thread().is_some() {
        cx.editor.begin_review_reply(None)?;
        MappableCommand::insert_mode.execute(cx);
        return Ok(());
    }
    let mode = cx.editor.mode();
    let (view, doc) = current!(cx.editor);
    doc.load_review_comments()?;
    let anchor = if let Some(cursor) = view.diff_mode.cursor(doc, view.id) {
        let display = view
            .diff_mode
            .display(doc)
            .context("Review baseline is still loading")?;
        let text = display.deleted_text(&cursor.before);
        let start = display.base.line_to_char(cursor.before.start as usize);
        let range = if mode == Mode::Select {
            let range = cursor.range.min_width_1(text);
            range.from()..range.to()
        } else {
            text.line_to_char(cursor.row)..text.line_to_char((cursor.row + 1).min(text.len_lines()))
        };
        CommentAnchor::new(
            CommentSide::Base,
            Some(doc.review_diff_reference().unwrap_or("HEAD").into()),
            display.base.slice(..),
            start + range.start..start + range.end,
        )
    } else {
        let text = doc.text().slice(..);
        let range = doc.selection(view.id).primary();
        let range = if mode == Mode::Select {
            let range = range.min_width_1(text);
            range.from()..range.to()
        } else {
            let line = range.cursor_line(text);
            text.line_to_char(line)..text.line_to_char((line + 1).min(text.len_lines()))
        };
        CommentAnchor::new(CommentSide::Current, None, text, range)
    };
    let was_dirty = doc.review_comments_dirty();
    let id = doc.add_review_comment(anchor)?;
    view.diff_mode.clear_cursor();
    view.diff_mode
        .set_comment_cursor(doc, view.id, id, Range::point(0));
    cx.editor.enter_review_comment(id, Some(was_dirty))?;
    MappableCommand::insert_mode.execute(cx);
    Ok(())
}

/// Return true only for actions handled at the source/comment boundary.
pub(super) fn execute(command: &MappableCommand, cx: &mut Context) -> bool {
    let name = command.name();
    let active = doc!(cx.editor).review_comment_target().is_some();
    if active {
        if matches!(
            name,
            "move_line_up" | "move_line_down" | "move_visual_line_up" | "move_visual_line_down"
        ) {
            let count = cx.count();
            return match cx.editor.move_review_comment(name.ends_with("down"), count) {
                Ok(handled) => handled,
                Err(error) => {
                    cx.editor
                        .set_error(format!("Cannot save review comment: {error:#}"));
                    true
                }
            };
        }
        if name == "toggle_comments" {
            if let Err(error) = create(cx) {
                cx.editor
                    .set_error(format!("Cannot create review comment: {error:#}"));
            }
            return true;
        }
        // View and review navigation use the surrounding source view.
        if matches!(
            name,
            "toggle_review_mode"
                | "goto_first_diag"
                | "goto_last_diag"
                | "goto_next_diag"
                | "goto_prev_diag"
                | "jump_backward"
                | "jump_forward"
        ) || name.starts_with("rotate_view")
            || name.starts_with("jump_view")
            || name.starts_with("hsplit")
            || name.starts_with("vsplit")
            || name.starts_with("close_view")
        {
            if let Err(error) = cx.editor.leave_review_comment(cx.editor.tree.focus) {
                cx.editor
                    .set_error(format!("Cannot save review comment: {error:#}"));
                return true;
            }
        }
        return false;
    }
    let (view, doc) = current_ref!(cx.editor);
    let Some(cursor) = view.diff_mode.comment_cursor(doc, view.id) else {
        return false;
    };
    if matches!(
        name,
        "move_line_up"
            | "move_line_down"
            | "move_visual_line_up"
            | "move_visual_line_down"
            | "toggle_review_mode"
            | "goto_first_diag"
            | "goto_last_diag"
            | "goto_next_diag"
            | "goto_prev_diag"
    ) {
        return false;
    }
    let id = cursor.id;
    if let Err(error) = cx.editor.enter_review_comment(id, None) {
        cx.editor
            .set_error(format!("Cannot edit review comment: {error:#}"));
        return true;
    }
    false
}
