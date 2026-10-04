//! Inline review comments use ordinary documents, selections and undo history.

use helix_core::{Rope, RopeSlice};

use crate::{document::review_comments::ReviewCommentTarget, Document, DocumentId, View, ViewId};

use super::{Editor, Mode};

// Ordinary editing commands keep a terminal newline. It is an editing boundary,
// not an extra blank row in the inline comment or its saved body.
fn comment_body(text: &Rope) -> RopeSlice<'_> {
    let end = text.len_chars();
    let end = end - usize::from(end != 0 && text.char(end - 1) == '\n');
    text.slice(..end)
}

impl Editor {
    pub fn focused_review_thread(&self) -> Option<u64> {
        let view = self.tree.get(self.tree.focus).review_view();
        let source = self.documents.get(&view.doc)?;
        let cursor = view.diff_mode.comment_cursor(source, view.id)?;
        source
            .review_comment_thread(cursor.id)
            .map(|thread| thread.id)
    }

    pub fn focus_review_message(&mut self, id: u64) -> anyhow::Result<()> {
        self.leave_review_comment(self.tree.focus)?;
        let view = self.tree.get(self.tree.focus);
        let source = self.documents.get(&view.doc).unwrap();
        let anchor = view
            .diff_mode
            .display(source)
            .and_then(|display| display.comment_block(id).map(|block| block.anchor))
            .ok_or_else(|| {
                anyhow::anyhow!("Comment is resolved, outdated, or outside the current diff")
            })?;
        let view = self.tree.get_mut(self.tree.focus);
        let source = self.documents.get_mut(&view.doc).unwrap();
        source.set_selection(view.id, helix_core::Selection::point(anchor));
        view.diff_mode
            .set_comment_cursor(source, view.id, id, helix_core::Range::point(0));
        view.ensure_cursor_in_view(source, self.config.load().scrolloff);
        Ok(())
    }

    pub fn begin_review_reply(&mut self, thread: Option<u64>) -> anyhow::Result<()> {
        let thread = thread
            .or_else(|| self.focused_review_thread())
            .ok_or_else(|| anyhow::anyhow!("Focus a comment or specify a thread ID"))?;
        self.leave_review_comment(self.tree.focus)?;
        let view = self.tree.get(self.tree.focus);
        let source = &self.documents[&view.doc];
        let root = source
            .review_store()
            .thread(thread)
            .ok_or_else(|| anyhow::anyhow!("Unknown review thread"))?;
        anyhow::ensure!(
            root.messages.iter().any(|message| view
                .diff_mode
                .display(source)
                .is_some_and(|display| display.comment_block(message.id).is_some())),
            "Reopen or relocate the thread before replying inline"
        );
        let source_id = view.doc;
        let source = self.documents.get_mut(&source_id).unwrap();
        let was_dirty = source.review_comments_dirty();
        let id = source.reply_review_comment(thread)?;
        self.focus_review_message(id)?;
        self.enter_review_comment(id, Some(was_dirty))?;
        self.set_status(format!("Reply to review thread #{thread}"));
        Ok(())
    }

    pub fn set_review_thread_resolved(
        &mut self,
        thread: Option<u64>,
        resolved: bool,
    ) -> anyhow::Result<()> {
        let thread = thread
            .or_else(|| self.focused_review_thread())
            .ok_or_else(|| anyhow::anyhow!("Focus a comment or specify a thread ID"))?;
        self.leave_review_comment(self.tree.focus)?;
        let view = self.tree.get_mut(self.tree.focus);
        let source = self.documents.get_mut(&view.doc).unwrap();
        let root = source
            .review_store()
            .thread(thread)
            .ok_or_else(|| anyhow::anyhow!("Unknown review thread"))?;
        anyhow::ensure!(
            source
                .review_session()
                .is_some_and(|session| session.id == root.review_id),
            "Thread belongs to another review target"
        );
        source.resolve_review_thread(thread, resolved)?;
        view.diff_mode.clear_cursor();
        view.ensure_cursor_in_view(source, self.config.load().scrolloff);
        self.set_status(format!(
            "Review thread #{thread} {}",
            if resolved { "resolved" } else { "reopened" }
        ));
        Ok(())
    }

    /// Metadata checks run once per second only while review views are visible.
    /// Drafts keep their original disk bytes so saving can detect conflicts.
    pub fn reload_review_comments(&mut self) -> bool {
        let sources: std::collections::BTreeSet<_> = self
            .tree
            .views()
            .filter_map(|(view, _)| {
                view.review_view()
                    .diff_mode
                    .enabled()
                    .then_some(view.review_view().doc)
            })
            .collect();
        let mut changed = false;
        for source in sources {
            let doc = &self.documents[&source];
            if doc.review_comments_dirty()
                || !doc.review_store().externally_changed().unwrap_or(false)
            {
                continue;
            }
            let editable: Vec<_> = self
                .tree
                .views()
                .filter(|(view, _)| {
                    view.review_source
                        .as_ref()
                        .is_some_and(|parent| parent.doc == source)
                })
                .map(|(view, focused)| (view.id, focused))
                .collect();
            if editable
                .iter()
                .any(|(_, focused)| *focused && self.mode != Mode::Normal)
            {
                continue;
            }
            let cursors: Vec<_> = self
                .tree
                .views()
                .filter_map(|(view, focused)| {
                    let view = view.review_view();
                    (view.doc == source)
                        .then(|| {
                            view.diff_mode
                                .comment_cursor(doc, view.id)
                                .cloned()
                                .map(|cursor| (view.id, cursor, focused))
                        })
                        .flatten()
                })
                .collect();
            for (view, _) in editable {
                if let Err(error) = self.leave_review_comment(view) {
                    self.set_error(format!("Cannot finish review comment: {error:#}"));
                    continue;
                }
            }
            let doc = self.documents.get_mut(&source).unwrap();
            let selected = doc.review_session().map(|review| review.id.clone());
            let generation = doc.review_comments_generation();
            match doc.load_review_comments() {
                Ok(()) => {
                    // Another CLI session must not change this view's target.
                    if let Some(id) = selected {
                        doc.restore_review_session(&id);
                    }
                    if generation != doc.review_comments_generation() {
                        changed = true;
                        for (view_id, cursor, focused) in cursors {
                            let view = self.tree.get_mut(view_id);
                            view.diff_mode.clear_cursor();
                            if let Some(comment) = doc
                                .review_comment(cursor.id)
                                .filter(|_| doc.review_comment_visible(cursor.id))
                            {
                                let end = comment.text.chars().count();
                                let ranges = cursor.ranges.transform(|mut range| {
                                    range.anchor = range.anchor.min(end);
                                    range.head = range.head.min(end);
                                    range
                                });
                                view.diff_mode
                                    .set_comment_selection(doc, view_id, cursor.id, ranges);
                            }
                            if focused {
                                view.ensure_cursor_in_view(doc, self.config.load().scrolloff);
                            }
                        }
                    }
                }
                Err(error) => self.set_error(format!("Cannot reload review comments: {error:#}")),
            }
        }
        changed
    }

    pub fn reset_review_comments_timer(&mut self) {
        let enabled = self
            .tree
            .views()
            .any(|(view, _)| view.review_view().diff_mode.enabled());
        let delay = if enabled {
            std::time::Duration::from_secs(1)
        } else {
            std::time::Duration::from_secs(86400 * 365 * 30)
        };
        self.review_comments_timer
            .as_mut()
            .reset(tokio::time::Instant::now() + delay);
    }

    pub fn enter_review_comment(
        &mut self,
        id: u64,
        draft_was_dirty: Option<bool>,
    ) -> anyhow::Result<()> {
        let new = draft_was_dirty.is_some();
        let view_id = self.tree.focus;
        let view = self.tree.get(view_id);
        if view.review_source.is_some() {
            return Ok(());
        }
        let source_id = view.doc;
        let source = &self.documents[&source_id];
        let cursor = view
            .diff_mode
            .comment_cursor(source, view_id)
            .filter(|cursor| cursor.id == id)
            .ok_or_else(|| anyhow::anyhow!("Review comment is not focused"))?;
        let selection = cursor.ranges.clone();
        let comment = source.review_comment(id).unwrap();
        let body = comment.text.clone();
        let was_dirty = draft_was_dirty.unwrap_or_else(|| source.review_comments_dirty());
        let key = (source_id, id);
        let cached = self.review_comment_buffers.get(&key).copied();
        let buffer = cached.filter(|buffer| {
            self.documents
                .get(buffer)
                .is_some_and(|doc| comment_body(doc.text()) == body)
        });
        let buffer = buffer.unwrap_or_else(|| {
            self.remove_review_comment_buffers(source_id, Some(id));
            let mut doc = Document::from(
                Rope::from_str(&format!("{body}\n")),
                None,
                self.config.clone(),
                self.syn_loader.clone(),
            );
            doc.review_comment_target = Some(ReviewCommentTarget {
                source: source_id,
                id,
                previous: (!new).then(|| body.clone()),
                was_dirty,
            });
            let buffer = self.new_document(doc);
            self.review_comment_buffers.insert(key, buffer);
            buffer
        });
        let doc = self.documents.get_mut(&buffer).unwrap();
        doc.ensure_view_init(view_id);
        doc.set_selection(view_id, selection);
        let target = doc.review_comment_target.as_mut().unwrap();
        target.previous = (!new).then_some(body);
        target.was_dirty = was_dirty;
        let mut editable = View::new(buffer, self.config().gutters.clone());
        let source = self.tree.get_mut(view_id);
        editable.id = source.id;
        editable.area = source.area;
        let source = std::mem::replace(source, editable);
        self.tree.get_mut(view_id).review_source = Some(Box::new(source));
        Ok(())
    }

    /// Copy only the comment body and selections back to the inline display.
    /// Source edits still go through the source document's read-only guard.
    pub fn sync_review_comment(&mut self, view_id: ViewId, save: bool) -> anyhow::Result<()> {
        let Some(view) = self.tree.try_get(view_id) else {
            return Ok(());
        };
        let buffer = view.doc;
        let Some((source_id, id)) = self.documents[&buffer].review_comment_target() else {
            return Ok(());
        };
        let text = self.documents[&buffer].text().clone();
        let body = comment_body(&text);
        let selection = self.documents[&buffer]
            .selection(view_id)
            .clone()
            .transform(|range| {
                let mut range = range;
                range.anchor = range.anchor.min(body.len_chars());
                range.head = range.head.min(body.len_chars());
                range
            });
        let source = self
            .documents
            .get_mut(&source_id)
            .ok_or_else(|| anyhow::anyhow!("Reviewed source was closed"))?;
        let comment = source
            .review_comment(id)
            .ok_or_else(|| anyhow::anyhow!("Review comment was removed"))?;
        if body != comment.text {
            source.set_review_comment_text(id, body.to_string());
        }
        let scrolloff = source.config.load().scrolloff;
        let view = self.tree.get_mut(view_id);
        if let Some(parent) = view.review_source.as_mut() {
            parent.area = view.area;
            if parent
                .diff_mode
                .comment_cursor(source, view_id)
                .is_none_or(|cursor| cursor.ranges != selection)
            {
                parent
                    .diff_mode
                    .set_comment_selection(source, view_id, id, selection);
            }
            parent.ensure_cursor_in_view(source, scrolloff);
        }
        if save && source.review_comments_dirty() {
            source.save_review_comments()?;
            let body = source.review_comment(id).unwrap().text.clone();
            let doc = self.documents.get_mut(&buffer).unwrap();
            doc.reset_modified();
            let target = doc.review_comment_target.as_mut().unwrap();
            target.previous = Some(body);
            target.was_dirty = false;
        }
        Ok(())
    }

    pub fn sync_review_comments(&mut self) {
        let views: Vec<_> = self
            .tree
            .views()
            .filter(|(view, _)| view.review_source.is_some())
            .map(|(view, focused)| (view.id, focused))
            .collect();
        for (view, focused) in views {
            if let Err(error) =
                self.sync_review_comment(view, !focused || self.mode != Mode::Insert)
            {
                self.set_error(format!("Cannot save review comment: {error:#}"));
            }
        }
    }

    pub fn leave_review_comment(&mut self, view_id: ViewId) -> anyhow::Result<()> {
        if self
            .tree
            .try_get(view_id)
            .is_none_or(|view| view.review_source.is_none())
        {
            return Ok(());
        }
        {
            let view = self.tree.get_mut(view_id);
            let doc = self.documents.get_mut(&view.doc).unwrap();
            doc.append_changes_to_history(view);
        }
        self.sync_review_comment(view_id, true)?;
        let view = self.tree.get_mut(view_id);
        let mut source = *view.review_source.take().unwrap();
        source.area = view.area;
        *view = source;
        self.mode = Mode::Normal;
        Ok(())
    }

    pub fn cancel_review_comment(&mut self, view_id: ViewId) {
        let view = self.tree.get_mut(view_id);
        let buffer = view.doc;
        let doc = self.documents.get_mut(&buffer).unwrap();
        let Some(target) = doc.review_comment_target.take() else {
            return;
        };
        let mut source = *view.review_source.take().unwrap();
        source.area = view.area;
        source.diff_mode.clear_cursor();
        *view = source;
        self.documents
            .get_mut(&target.source)
            .unwrap()
            .cancel_review_comment_edit(target.id, target.previous, target.was_dirty);
        self.remove_review_comment_buffers(target.source, Some(target.id));
        self.mode = Mode::Normal;
    }

    pub fn remove_focused_review_comment(&mut self) -> anyhow::Result<()> {
        let view = self.tree.focus;
        self.leave_review_comment(view)?;
        let source = self.tree.get_mut(view);
        let doc = self.documents.get_mut(&source.doc).unwrap();
        let id = source
            .diff_mode
            .comment_cursor(doc, view)
            .ok_or_else(|| anyhow::anyhow!("Review comment is not focused"))?
            .id;
        doc.remove_review_comment(id)?;
        source.diff_mode.clear_cursor();
        let source_id = source.doc;
        self.remove_review_comment_buffers(source_id, Some(id));
        self.mode = Mode::Normal;
        Ok(())
    }

    /// Normal vertical motion can leave a comment; selections remain inside it.
    pub fn move_review_comment(&mut self, down: bool, count: usize) -> anyhow::Result<bool> {
        let view_id = self.tree.focus;
        let view = self.tree.get(view_id);
        let Some((source_id, id)) = self.documents[&view.doc].review_comment_target() else {
            return Ok(false);
        };
        if self.mode == Mode::Select || self.documents[&view.doc].selection(view_id).len() != 1 {
            return Ok(false);
        }
        self.sync_review_comment(view_id, true)?;
        let parent = self.tree.get_mut(view_id).review_source.as_mut().unwrap();
        let source = self.documents.get_mut(&source_id).unwrap();
        parent.move_diff_cursor(source, down, count);
        let selection = parent
            .diff_mode
            .comment_cursor(source, view_id)
            .filter(|cursor| cursor.id == id)
            .map(|cursor| cursor.ranges.clone());
        if let Some(selection) = selection {
            let buffer = self.tree.get(view_id).doc;
            self.documents
                .get_mut(&buffer)
                .unwrap()
                .set_selection(view_id, selection);
        } else {
            // Do not overwrite the destination with the old comment's cursor.
            let view = self.tree.get_mut(view_id);
            let mut source = *view.review_source.take().unwrap();
            source.area = view.area;
            *view = source;
        }
        Ok(true)
    }

    pub(super) fn remove_review_comment_buffers(&mut self, source: DocumentId, id: Option<u64>) {
        let buffers: Vec<_> = self
            .review_comment_buffers
            .iter()
            .filter(|((owner, comment), _)| *owner == source && id.is_none_or(|id| id == *comment))
            .map(|(key, buffer)| (*key, *buffer))
            .collect();
        for (key, buffer) in buffers {
            self.review_comment_buffers.remove(&key);
            self.saves.remove(&buffer);
            if let Some(doc) = self.documents.remove(&buffer) {
                helix_event::dispatch(crate::events::DocumentDidClose { editor: self, doc });
            }
        }
    }
}
