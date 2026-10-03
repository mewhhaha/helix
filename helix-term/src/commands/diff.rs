//! Diff navigation and selection on read-only deleted text. Review selections
//! use old-text offsets; source selections stay at the deletion anchor.

use super::*;

pub(super) enum DiagnosticNavigation {
    First,
    Last,
    Next(usize),
    Previous(usize),
}

/// Merge review stops with diagnostics without putting synthetic errors in the
/// document. Deleted blocks sort before the source row which follows them.
pub(super) fn goto_diagnostic(editor: &mut Editor, navigation: DiagnosticNavigation) -> bool {
    let (view, doc) = current!(editor);
    if !view.diff_mode.enabled() {
        return false;
    }
    let display = view.diff_mode.display(doc);
    let hunks = display.as_ref().map_or(&[][..], |display| &display.hunks);
    let text = doc.text().slice(..);
    let hunk_position = |hunk: &Hunk| {
        (
            text.line_to_char(hunk.after.start as usize),
            hunk.before.is_empty(),
        )
    };
    let selection = doc.selection(view.id).primary();
    // Diagnostic selections put the cursor at their end when moving forward.
    // Use their start so long ranges neither skip hunks inside them nor visit
    // the same diagnostic again when moving backward.
    let source_cursor = doc
        .diagnostics()
        .iter()
        .find(|diagnostic| {
            diagnostic.range.start == selection.from() && diagnostic.range.end == selection.to()
        })
        .map_or_else(
            || selection.cursor(text),
            |diagnostic| diagnostic.range.start,
        );
    let cursor = view
        .diff_mode
        .cursor(doc, view.id)
        .and_then(|cursor| hunks.iter().find(|hunk| hunk.before == cursor.before))
        .map(hunk_position)
        .unwrap_or((source_cursor, true));
    let reverse = matches!(
        navigation,
        DiagnosticNavigation::Last | DiagnosticNavigation::Previous(_)
    );
    let mut remaining = match navigation {
        DiagnosticNavigation::Next(count) | DiagnosticNavigation::Previous(count) => count,
        _ => 1,
    };
    let mut diagnostics = doc.diagnostics().iter();
    let mut changes = hunks.iter();
    let mut diagnostic = if reverse {
        diagnostics.next_back()
    } else {
        diagnostics.next()
    };
    let mut change = if reverse {
        changes.next_back()
    } else {
        changes.next()
    };
    let mut selected = None;
    let mut previous = None;
    while diagnostic.is_some() || change.is_some() {
        let take_diagnostic = match (diagnostic, change) {
            (Some(diagnostic), Some(hunk)) => {
                let ordering = (diagnostic.range.start, true).cmp(&hunk_position(hunk));
                // A source diagnostic at an added hunk's start is a single stop.
                if ordering == Ordering::Equal {
                    change = if reverse {
                        changes.next_back()
                    } else {
                        changes.next()
                    };
                }
                if reverse {
                    ordering != Ordering::Less
                } else {
                    ordering != Ordering::Greater
                }
            }
            (Some(_), None) => true,
            _ => false,
        };
        let (position, target) = if take_diagnostic {
            let target = diagnostic.unwrap();
            diagnostic = if reverse {
                diagnostics.next_back()
            } else {
                diagnostics.next()
            };
            (
                (target.range.start, true),
                (Some(Range::new(target.range.start, target.range.end)), None),
            )
        } else {
            let target = change.unwrap();
            change = if reverse {
                changes.next_back()
            } else {
                changes.next()
            };
            (hunk_position(target), (None, Some(target)))
        };
        if previous == Some(position) {
            continue;
        }
        previous = Some(position);
        if match navigation {
            DiagnosticNavigation::Next(_) => position <= cursor,
            DiagnosticNavigation::Previous(_) => position >= cursor,
            _ => false,
        } {
            continue;
        }
        selected = Some(target);
        remaining -= 1;
        if remaining == 0 {
            break;
        }
    }
    // Counts stop at the last available target, as change navigation does.
    let Some((diagnostic, change)) = selected else {
        return true;
    };
    let deletion = change.and_then(|hunk| {
        display
            .as_ref()?
            .deletions
            .iter()
            .find(|deletion| deletion.before == hunk.before)
    });
    let before = deletion.map(|deletion| deletion.before.clone());
    let selection = if let Some(range) = diagnostic {
        let direction = if matches!(navigation, DiagnosticNavigation::Previous(_)) {
            Direction::Backward
        } else {
            Direction::Forward
        };
        let range = range.with_direction(direction);
        Selection::single(range.anchor, range.head)
    } else {
        Selection::point(deletion.map_or_else(
            || hunk_position(change.unwrap()).0,
            |deletion| deletion.anchor,
        ))
    };
    push_jump(view, doc);
    view.diff_mode.clear_cursor();
    doc.set_selection(view.id, selection);
    if let Some(before) = before {
        view.diff_mode.set_cursor(doc, view.id, before, 0, 0);
    } else if diagnostic.is_some() {
        view.diagnostics_handler
            .immediately_show_diagnostic(doc, view.id);
    }
    true
}

pub(super) fn execute(command: &MappableCommand, cx: &mut Context) -> bool {
    let (view, doc) = current_ref!(cx.editor);
    if view.diff_mode.cursor(doc, view.id).is_none() {
        return false;
    }
    let name = command.name();
    if name == "normal_mode" {
        cx.editor.mode = Mode::Normal;
        return true;
    }
    if name == "select_mode" {
        let (view, doc) = current!(cx.editor);
        view.diff_mode.select(doc, view.id, |_, range| range);
        cx.editor.mode = Mode::Select;
        return true;
    }

    let extend = cx.editor.mode == Mode::Select || name.starts_with("extend_");
    let behavior = if extend {
        Movement::Extend
    } else {
        Movement::Move
    };
    let count = cx.count();
    let motion: Option<(MoveFn, Direction)> = match name {
        "move_char_left" | "extend_char_left" => Some((move_horizontally, Direction::Backward)),
        "move_char_right" | "extend_char_right" => Some((move_horizontally, Direction::Forward)),
        "extend_line_up" | "extend_visual_line_up" => Some((move_vertically, Direction::Backward)),
        "extend_line_down" | "extend_visual_line_down" => {
            Some((move_vertically, Direction::Forward))
        }
        // In normal mode, vertical movement can also leave the deleted block.
        "move_line_up" | "move_visual_line_up" if extend => {
            Some((move_vertically, Direction::Backward))
        }
        "move_line_down" | "move_visual_line_down" if extend => {
            Some((move_vertically, Direction::Forward))
        }
        _ => None,
    };
    if let Some((motion, direction)) = motion {
        let (view, doc) = current!(cx.editor);
        let format = TextFormat {
            tab_width: doc.tab_width() as u16,
            ..TextFormat::default()
        };
        return view.diff_mode.select(doc, view.id, |text, range| {
            let count = if name.contains("line") {
                let line = range.cursor_line(text);
                let last = text.len_lines()
                    - 1
                    - usize::from(text.line(text.len_lines() - 1).len_chars() == 0);
                count.min(if direction == Direction::Forward {
                    last.saturating_sub(line)
                } else {
                    line
                })
            } else {
                count
            };
            motion(
                text,
                range,
                direction,
                count,
                behavior,
                &format,
                &mut TextAnnotations::default(),
            )
        });
    }

    let word: Option<fn(RopeSlice, Range, usize) -> Range> = match name {
        "move_next_word_start" | "extend_next_word_start" => Some(movement::move_next_word_start),
        "move_next_word_end" | "extend_next_word_end" => Some(movement::move_next_word_end),
        "move_prev_word_start" | "extend_prev_word_start" => Some(movement::move_prev_word_start),
        "move_prev_word_end" | "extend_prev_word_end" => Some(movement::move_prev_word_end),
        "move_next_long_word_start" | "extend_next_long_word_start" => {
            Some(movement::move_next_long_word_start)
        }
        "move_next_long_word_end" | "extend_next_long_word_end" => {
            Some(movement::move_next_long_word_end)
        }
        "move_prev_long_word_start" | "extend_prev_long_word_start" => {
            Some(movement::move_prev_long_word_start)
        }
        "move_prev_long_word_end" | "extend_prev_long_word_end" => {
            Some(movement::move_prev_long_word_end)
        }
        "move_next_sub_word_start" | "extend_next_sub_word_start" => {
            Some(movement::move_next_sub_word_start)
        }
        "move_next_sub_word_end" | "extend_next_sub_word_end" => {
            Some(movement::move_next_sub_word_end)
        }
        "move_prev_sub_word_start" | "extend_prev_sub_word_start" => {
            Some(movement::move_prev_sub_word_start)
        }
        "move_prev_sub_word_end" | "extend_prev_sub_word_end" => {
            Some(movement::move_prev_sub_word_end)
        }
        _ => None,
    };
    if let Some(word) = word {
        let (view, doc) = current!(cx.editor);
        return view.diff_mode.select(doc, view.id, |text, range| {
            let next = word(text, range, count);
            if extend {
                range.put_cursor(text, next.cursor(text), true)
            } else {
                next
            }
        });
    }

    if !matches!(
        name,
        "select_all"
            | "collapse_selection"
            | "flip_selections"
            | "ensure_selections_forward"
            | "goto_line_start"
            | "extend_to_line_start"
            | "goto_line_end"
            | "extend_to_line_end"
            | "goto_line_end_newline"
            | "extend_to_line_end_newline"
            | "goto_first_nonwhitespace"
            | "goto_file_start"
            | "goto_last_line"
            | "goto_line"
            | "extend_line"
            | "extend_line_below"
            | "extend_line_above"
            | "select_line_below"
            | "select_line_above"
            | "extend_to_line_bounds"
            | "shrink_to_line_bounds"
    ) {
        return false;
    }
    let (view, doc) = current!(cx.editor);
    view.diff_mode.select(doc, view.id, |text, range| {
        let cursor = range.cursor(text);
        let line = text.char_to_line(cursor);
        let start = text.line_to_char(line);
        let end = line_end_char_index(&text, line);
        match name {
            "select_all" => Range::new(0, text.len_chars()),
            "collapse_selection" => Range::point(cursor),
            "flip_selections" => range.flip(),
            "ensure_selections_forward" => range.with_direction(Direction::Forward),
            "goto_line_start" | "extend_to_line_start" => range.put_cursor(text, start, extend),
            "goto_line_end" | "extend_to_line_end" => range.put_cursor(
                text,
                graphemes::prev_grapheme_boundary(text, end).max(start),
                extend,
            ),
            "goto_line_end_newline" | "extend_to_line_end_newline" => {
                range.put_cursor(text, end, extend)
            }
            "goto_first_nonwhitespace" => {
                let pos = text
                    .line(line)
                    .chars()
                    .position(|ch| !ch.is_whitespace())
                    .unwrap_or(0);
                range.put_cursor(text, start + pos, extend)
            }
            "goto_file_start" => range.put_cursor(text, 0, extend),
            "goto_last_line" => {
                let last = text.len_lines()
                    - 1
                    - usize::from(text.line(text.len_lines() - 1).len_chars() == 0);
                range.put_cursor(text, text.line_to_char(last), extend)
            }
            "goto_line" => range.put_cursor(
                text,
                text.line_to_char((count - 1).min(text.len_lines() - 1)),
                extend,
            ),
            _ => {
                let (first, last) = range.line_range(text);
                let first_char = text.line_to_char(first);
                let last_char = text.line_to_char((last + 1).min(text.len_lines()));
                if name == "shrink_to_line_bounds" {
                    if first == last {
                        return range;
                    }
                    let first = first + usize::from(range.from() != first_char);
                    let last = last + usize::from(range.to() == last_char);
                    return Range::new(text.line_to_char(first), text.line_to_char(last))
                        .with_direction(range.direction());
                }
                if name == "extend_to_line_bounds" {
                    return Range::new(first_char, last_char).with_direction(range.direction());
                }
                let above = name.ends_with("above")
                    || name == "extend_line" && range.direction() == Direction::Backward;
                let steps =
                    count - usize::from(range.from() != first_char || range.to() != last_char);
                if above {
                    Range::new(last_char, text.line_to_char(first.saturating_sub(steps)))
                } else {
                    Range::new(
                        first_char,
                        text.line_to_char((last + steps + 1).min(text.len_lines())),
                    )
                }
            }
        }
    })
}
