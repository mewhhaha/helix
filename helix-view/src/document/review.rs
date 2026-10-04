//! Per-document review bases and protection shared by all of its views.

use helix_core::Rope;
use helix_vcs::DiffHandle;

use super::Document;
use crate::ViewId;

pub const DIFF_MODE_READ_ONLY: &str = "Review mode is read-only; use :review-mode off to edit";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReviewKey {
    pub revision: u64,
    pub inverted: bool,
    pub base_generation: u64,
}

pub(super) struct ReviewBase {
    pub(super) reference: String,
    pub(super) handle: DiffHandle,
    pub(super) revision: Option<helix_vcs::ReviewRevision>,
    pub(super) base_hash: String,
}

impl Document {
    /// Protect the buffer until its last diff view is disabled or closed.
    pub fn is_diff_mode_read_only(&self) -> bool {
        !self.diff_mode_views.is_empty()
    }

    pub(crate) fn set_view_diff_mode(&mut self, view: ViewId, enabled: bool) {
        if enabled {
            self.diff_mode_views.insert(view);
        } else {
            self.diff_mode_views.remove(&view);
        }
    }

    pub fn review_diff_handle(&self) -> Option<&DiffHandle> {
        self.review_diff
            .as_ref()
            .map(|review| &review.handle)
            .or_else(|| self.diff_handle())
    }

    pub fn review_diff_reference(&self) -> Option<&str> {
        self.review_diff
            .as_ref()
            .map(|review| review.reference.as_str())
    }

    pub fn review_diff_key(&self) -> Option<ReviewKey> {
        self.review_diff_handle().map(|handle| {
            let (revision, inverted) = handle.render_key();
            ReviewKey {
                revision,
                inverted,
                base_generation: self.review_diff_generation,
            }
        })
    }

    pub(crate) fn review_diff_generation(&self) -> u64 {
        self.review_diff_generation
    }

    pub(crate) fn set_review_diff_base(&mut self, reference: String, base: Rope) {
        self.review_diff_generation = self.review_diff_generation.wrapping_add(1);
        self.review_diff = Some(ReviewBase {
            reference,
            base_hash: super::review_comments::content_hash(base.slice(..)),
            handle: DiffHandle::new(base, self.text.clone()),
            revision: None,
        });
    }

    pub(crate) fn set_review_revision(&mut self, revision: helix_vcs::ReviewRevision) {
        if let Some(base) = self.review_diff.as_mut() {
            base.revision = Some(revision);
        }
    }

    pub(crate) fn clear_review_diff_base(&mut self) {
        self.review_diff_controller.cancel();
        self.review_diff_generation = self.review_diff_generation.wrapping_add(1);
        self.review_diff = None;
    }
}
