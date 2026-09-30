//! This modules encapsulates a tiny bit of unsafe code that
//! makes diffing significantly faster and more ergonomic to implement.
//! This code is necessary because diffing requires quick random
//! access to the lines of the text that is being diffed.
//!
//! Therefore it is best to collect the `Rope::lines` iterator into a vec
//! first because access to the vec is `O(1)` where `Rope::line` is `O(log N)`.
//! However this process can allocate a (potentially quite large) vector.
//!
//! To avoid reallocation for every diff, the vector is reused.
//! However the RopeSlice references the original rope and therefore forms a self-referential data structure.
//! A transmute is used to change the lifetime of the slice to static to circumvent that project.
use std::mem::transmute;

use helix_core::{Rope, RopeSlice};
use imara_diff::{InternedInput, Interner};

use super::{MAX_DIFF_BYTES, MAX_DIFF_LINES};

/// A cache that stores the `lines` of a rope as a vector.
/// It allows safely reusing the allocation of the vec when updating the rope
pub(crate) struct InternedRopeLines {
    diff_base: Box<Rope>,
    doc: Box<Rope>,
    num_tokens_diff_base: Option<u32>,
    interned: InternedInput<RopeSlice<'static>>,
}

impl InternedRopeLines {
    pub fn new(diff_base: Rope, doc: Rope) -> InternedRopeLines {
        let mut res = InternedRopeLines {
            interned: InternedInput {
                before: Vec::new(),
                after: Vec::new(),
                interner: Interner::new(0),
            },
            diff_base: Box::new(diff_base),
            doc: Box::new(doc),
            // will be populated by update_diff_base_impl
            num_tokens_diff_base: None,
        };
        if !res.is_too_large() {
            res.interned
                .reserve(res.diff_base.len_lines() as u32, res.doc.len_lines() as u32);
            res.update_diff_base_impl();
        }
        res
    }

    pub fn doc(&self) -> Rope {
        Rope::clone(&*self.doc)
    }

    pub fn diff_base(&self) -> Rope {
        Rope::clone(&*self.diff_base)
    }

    /// Updates the `diff_base` and optionally the document if `doc` is not None
    pub fn update_diff_base(&mut self, diff_base: Rope, doc: Option<Rope>) {
        self.interned.clear();
        self.num_tokens_diff_base = None;
        *self.diff_base = diff_base;
        if let Some(doc) = doc {
            *self.doc = doc
        }
        if !self.is_too_large() {
            self.update_diff_base_impl();
        }
    }

    /// Updates the `doc` without reinterning the `diff_base`, this function
    /// is therefore significantly faster than `update_diff_base` when only the document changes.
    pub fn update_doc(&mut self, doc: Rope) {
        // Safety: we clear any tokens that were added after
        // the interning of `self.diff_base` finished so
        // all lines that refer to `self.doc` have been purged.

        if let Some(num_tokens) = self.num_tokens_diff_base {
            self.interned.interner.erase_tokens_after(num_tokens.into());
        }

        *self.doc = doc;
        if self.is_too_large() {
            self.interned.after.clear();
        } else if self.num_tokens_diff_base.is_some() {
            self.update_doc_impl();
        } else {
            self.update_diff_base_impl();
        }
    }

    fn update_diff_base_impl(&mut self) {
        // Safety: This transmute is safe because it only transmutes a lifetime, which has no effect.
        // The backing storage for the RopeSlices referred to by the lifetime is stored in `self.diff_base`.
        // Therefore as long as `self.diff_base` is not dropped/replaced this memory remains valid.
        // `self.diff_base` is only changed in `self.update_diff_base`, which clears the interner.
        // When the interned lines are exposed to consumer in `self.diff_input`, the lifetime is bounded to a reference to self.
        // That means that on calls to update there exist no references to `self.interned`.
        let before = self
            .diff_base
            .lines()
            .map(|line: RopeSlice| -> RopeSlice<'static> { unsafe { transmute(line) } });
        self.interned.update_before(before);
        self.num_tokens_diff_base = Some(self.interned.interner.num_tokens());
        // the has to be interned again because the interner was fully cleared
        self.update_doc_impl()
    }

    fn update_doc_impl(&mut self) {
        // Safety: This transmute is save because it only transmutes a lifetime, which has no effect.
        // The backing storage for the RopeSlices referred to by the lifetime is stored in `self.doc`.
        // Therefore as long as `self.doc` is not dropped/replaced this memory remains valid.
        // `self.doc` is only changed in `self.update_doc`, which clears the interner.
        // When the interned lines are exposed to consumer in `self.diff_input`, the lifetime is bounded to a reference to self.
        // That means that on calls to update there exist no references to `self.interned`.
        let after = self
            .doc
            .lines()
            .map(|line: RopeSlice| -> RopeSlice<'static> { unsafe { transmute(line) } });
        self.interned.update_after(after);
    }

    fn is_too_large(&self) -> bool {
        // bound both lines and bytes to avoid huge files with few (but huge) lines
        // or huge file with tiny lines. While this makes no difference to
        // diff itself (the diff performance only depends on the number of tokens)
        // the interning runtime depends mostly on filesize and is actually dominant
        // for large files
        self.doc.len_lines() > MAX_DIFF_LINES
            || self.diff_base.len_lines() > MAX_DIFF_LINES
            || self.doc.len_bytes() > MAX_DIFF_BYTES
            || self.diff_base.len_bytes() > MAX_DIFF_BYTES
    }

    /// Returns the `InternedInput` for performing the diff.
    /// If `diff_base` or `doc` is so large that performing a diff could slow the editor
    /// this function returns `None`.
    pub fn interned_lines(&self) -> Option<&InternedInput<RopeSlice<'_>>> {
        if self.is_too_large() {
            None
        } else {
            Some(&self.interned)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oversized() -> Rope {
        Rope::from_str(&"\n".repeat(MAX_DIFF_LINES))
    }

    fn assert_interned(cache: &InternedRopeLines, base: &str, doc: &str) {
        let input = cache.interned_lines().unwrap();
        let lines = |tokens: &[imara_diff::Token]| {
            tokens
                .iter()
                .map(|token| input.interner[*token].to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            lines(&input.before),
            Rope::from_str(base)
                .lines()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            lines(&input.after),
            Rope::from_str(doc)
                .lines()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn oversized_base_can_be_updated_and_replaced() {
        let mut cache = InternedRopeLines::new(Rope::from_str("base\n"), Rope::from_str("doc\n"));
        cache.update_diff_base(oversized(), None);
        cache.update_doc(Rope::from_str("edited\n"));
        assert!(cache.interned_lines().is_none());

        cache.update_diff_base(Rope::from_str("new base\n"), None);
        assert_interned(&cache, "new base\n", "edited\n");
    }

    #[test]
    fn base_changed_while_document_is_oversized_is_rebuilt() {
        let mut cache = InternedRopeLines::new(Rope::from_str("base\n"), Rope::from_str("doc\n"));
        cache.update_doc(oversized());
        cache.update_diff_base(Rope::from_str("new base\n"), None);
        cache.update_doc(Rope::from_str("edited\n"));

        assert_interned(&cache, "new base\n", "edited\n");
    }

    #[test]
    fn initially_oversized_document_is_not_interned_and_can_shrink() {
        let mut cache = InternedRopeLines::new(Rope::from_str("base\n"), oversized());
        assert!(cache.interned.before.is_empty());
        assert!(cache.interned.after.is_empty());
        assert_eq!(cache.interned.interner.num_tokens(), 0);
        cache.update_doc(Rope::from_str("doc\n"));

        assert_interned(&cache, "base\n", "doc\n");
    }
}
