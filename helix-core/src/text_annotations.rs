use std::any::Any;
use std::cell::Cell;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::ptr::NonNull;
use std::sync::Arc;

use crate::doc_formatter::FormattedGrapheme;
use crate::syntax::{Highlight, OverlayHighlights};
use crate::{Position, Tendril};

/// An inline annotation is continuous text shown
/// on the screen before the grapheme that starts at
/// `char_idx`
#[derive(Debug, Clone)]
pub struct InlineAnnotation {
    pub text: Tendril,
    pub char_idx: usize,
}

impl InlineAnnotation {
    pub fn new(char_idx: usize, text: impl Into<Tendril>) -> Self {
        Self {
            char_idx,
            text: text.into(),
        }
    }

    /// Fingerprint immutable annotation positions and text once at their source.
    pub fn layout_key(annotations: &[Self]) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        annotations.len().hash(&mut hasher);
        for annotation in annotations {
            annotation.char_idx.hash(&mut hasher);
            annotation.text.as_bytes().hash(&mut hasher);
        }
        hasher.finish()
    }
}

/// Represents a **single Grapheme** that is part of the document
/// that start at `char_idx` that will be replaced with
/// a different `grapheme`.
/// If `grapheme` contains multiple graphemes the text
/// will render incorrectly.
/// If you want to overlay multiple graphemes simply
/// use multiple `Overlays`.
///
/// # Examples
///
/// The following examples are valid overlays for the following text:
///
/// `aX͎̊͢͜͝͡bc`
///
/// ```
/// use helix_core::text_annotations::Overlay;
///
/// // replaces a
/// Overlay::new(0, "X");
///
/// // replaces X͎̊͢͜͝͡
/// Overlay::new(1, "\t");
///
/// // replaces b
/// Overlay::new(6, "X̢̢̟͖̲͌̋̇͑͝");
/// ```
///
/// The following examples are invalid uses
///
/// ```
/// use helix_core::text_annotations::Overlay;
///
/// // overlay is not aligned at grapheme boundary
/// Overlay::new(3, "x");
///
/// // overlay contains multiple graphemes
/// Overlay::new(0, "xy");
/// ```
#[derive(Debug, Clone)]
pub struct Overlay {
    pub char_idx: usize,
    pub grapheme: Tendril,
}

impl Overlay {
    pub fn new(char_idx: usize, grapheme: impl Into<Tendril>) -> Self {
        Self {
            char_idx,
            grapheme: grapheme.into(),
        }
    }
}

/// Line annotations allow inserting virtual text lines between normal text
/// lines.  These lines can be filled with text in the rendering code as their
/// contents have no effect beyond visual appearance.
///
/// The height of virtual text is usually not known ahead of time as virtual
/// text often requires softwrapping. Furthermore the height of some virtual
/// text like side-by-side diffs depends on the height of the text (again
/// influenced by softwrap) and other virtual text. Therefore line annotations
/// are computed on the fly instead of ahead of time like other annotations.
///
/// The core of this trait `insert_virtual_lines` function. It is called at the
/// end of every  visual line and allows the `LineAnnotation` to insert empty
/// virtual lines. Apart from that the `LineAnnotation` trait has multiple
/// methods that allow it to track anchors in the document.
///
/// When a new traversal of a document starts `reset_pos` is called. Afterwards
/// the other functions are called with indices that are larger then the
/// one passed to `reset_pos`. This allows performing a binary search (use
/// `partition_point`) in `reset_pos` once and then to only look at the next
/// anchor during each method call.
///
/// The `reset_pos`, `skip_conceal` and `process_anchor` functions all return a
/// `char_idx` anchor. This anchor is stored when transversing the document and
/// when the grapheme at the anchor is traversed the `process_anchor` function
/// is called.
///
/// # Note
///
/// All functions only receive immutable references to `self`.
/// `LineAnnotation`s that want to store an internal position or
/// state of some kind should use `Cell`. Using interior mutability for
/// caches is preferable as otherwise a lot of lifetimes become invariant
/// which complicates APIs a lot.
pub trait LineAnnotation {
    /// Stable identity of the annotation's layout inputs. Opaque annotations
    /// keep the default and use ordinary traversal.
    fn checkpoint_key(&self) -> Option<u64> {
        None
    }

    /// Capture owned traversal state for a formatter checkpoint.
    fn checkpoint(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }

    /// Restore a state captured with matching layout inputs.
    fn restore_checkpoint(&mut self, _state: &(dyn Any + Send + Sync)) -> bool {
        false
    }

    /// Validate traversal state when inputs that only affect a portion of the
    /// document (such as the cursor's visual row) have changed.
    fn checkpoint_is_valid(&self, _state: &(dyn Any + Send + Sync), _char_idx: usize) -> bool {
        true
    }

    /// Recompute an anchor whose position depends on inputs excluded from the
    /// layout key. Other annotations retain their recorded anchor.
    fn checkpoint_next_anchor(&self, _char_idx: usize) -> Option<usize> {
        None
    }

    /// Resets the internal position to `char_idx`. This function is called
    /// when a new traversal of a document starts.
    ///
    /// All `char_idx` passed to `insert_virtual_lines` are strictly monotonically increasing
    /// with the first `char_idx` greater or equal to the `char_idx`
    /// passed to this function.
    ///
    /// # Returns
    ///
    /// The `char_idx` of the next anchor this `LineAnnotation` is interested in,
    /// replaces the currently registered anchor. Return `usize::MAX` to ignore
    fn reset_pos(&mut self, _char_idx: usize) -> usize {
        usize::MAX
    }

    /// Called when a text is concealed that contains an anchor registered by this `LineAnnotation`.
    /// In this case the line decorations  **must** ensure that virtual text anchored within that
    /// char range is skipped.
    ///
    /// # Returns
    ///
    /// The `char_idx` of the next anchor this `LineAnnotation` is interested in,
    /// **after the end of conceal_end_char_idx**
    /// replaces the currently registered anchor. Return `usize::MAX` to ignore
    fn skip_concealed_anchors(&mut self, conceal_end_char_idx: usize) -> usize {
        self.reset_pos(conceal_end_char_idx)
    }

    /// Process an anchor (horizontal position is provided) and returns the next anchor.
    ///
    /// # Returns
    ///
    /// The `char_idx` of the next anchor this `LineAnnotation` is interested in,
    /// replaces the currently registered anchor. Return `usize::MAX` to ignore
    fn process_anchor(&mut self, _grapheme: &FormattedGrapheme) -> usize {
        usize::MAX
    }

    /// This function is called at the end of a visual line to insert virtual text
    ///
    /// # Returns
    ///
    /// The number of additional virtual lines to reserve
    ///
    /// # Note
    ///
    /// The `line_end_visual_pos` parameter indicates the visual vertical distance
    /// from the start of block where the traversal starts.  This includes the offset
    /// from other `LineAnnotations`. This allows inline annotations to consider
    /// the height of the text and "align" two different documents (like for side
    /// by side diffs).  These annotations that want to "align" two documents should
    /// therefore be added last so that other virtual text is also considered while aligning
    fn insert_virtual_lines(
        &mut self,
        line_end_char_idx: usize,
        line_end_visual_pos: Position,
        doc_line: usize,
    ) -> Position;
}

#[derive(Debug)]
struct Layer<'a, A, M> {
    annotations: &'a [A],
    current_index: Cell<usize>,
    metadata: M,
    content_key: Option<u64>,
    highlights_key: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct LineAnnotationCheckpoint {
    next_anchor: usize,
    state: Arc<dyn Any + Send + Sync>,
}

impl Debug for LineAnnotationCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LineAnnotationCheckpoint")
            .field("next_anchor", &self.next_anchor)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
enum InlineHighlights<'a> {
    Homogeneous(Option<Highlight>),
    Heterogeneous(&'a [Highlight]),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_fingerprints_match_content_and_track_replacements() {
        let inline = [InlineAnnotation::new(10_000, "■")];
        let colors = [Highlight::new(2)];
        let mut raw = TextAnnotations::default();
        raw.add_inline_annotations_with_highlights(&inline, &colors);
        let mut prepared = TextAnnotations::default();
        prepared.add_inline_annotations_with_highlights_cached(
            &inline,
            &colors,
            Some((
                InlineAnnotation::layout_key(&inline),
                TextAnnotations::highlights_key(&colors),
            )),
        );
        assert_eq!(raw.layout_key(), prepared.layout_key());
        prepared.reset_pos(10_000);
        prepared.next_inline_annotation_at(10_000);
        assert_eq!(raw.layout_key(), prepared.layout_key());

        let replacement = [InlineAnnotation::new(10_001, "■ ")];
        let changed_colors = [Highlight::new(3)];
        let mut changed = TextAnnotations::default();
        changed.add_inline_annotations_with_highlights_cached(
            &replacement,
            &changed_colors,
            Some((
                InlineAnnotation::layout_key(&replacement),
                TextAnnotations::highlights_key(&changed_colors),
            )),
        );
        assert_ne!(changed.layout_key(), prepared.layout_key());
    }

    #[test]
    fn heterogeneous_inline_stream_preserves_styles_and_same_position_order() {
        let before = [InlineAnnotation::new(1, "前")];
        let colors = [
            InlineAnnotation::new(0, "skipped"),
            InlineAnnotation::new(1, "■"),
            InlineAnnotation::new(1, "é"),
            InlineAnnotation::new(3, "later"),
        ];
        let highlights = [
            Highlight::new(1),
            Highlight::new(2),
            Highlight::new(3),
            Highlight::new(4),
        ];
        let after = [InlineAnnotation::new(1, " ")];
        let mut annotations = TextAnnotations::default();
        annotations
            .add_inline_annotations(&before, Some(Highlight::new(0)))
            .add_inline_annotations_with_highlights(&colors, &highlights)
            .add_inline_annotations(&after, None);
        assert_eq!(annotations.inline_annotations.len(), 3);
        annotations.reset_pos(1);
        let mut actual = Vec::new();
        while let Some((annotation, highlight)) = annotations.next_inline_annotation_at(1) {
            actual.push((annotation.text.to_string(), highlight));
        }
        assert_eq!(
            actual,
            [
                ("前".into(), Some(Highlight::new(0))),
                ("■".into(), Some(Highlight::new(2))),
                ("é".into(), Some(Highlight::new(3))),
                (" ".into(), None)
            ]
        );
        assert!(annotations.next_inline_annotation_at(2).is_none());
        assert_eq!(
            annotations.next_inline_annotation_at(3).unwrap().1,
            Some(Highlight::new(4))
        );
        annotations.reset_pos(1);
        assert_eq!(
            annotations
                .next_inline_annotation_at(1)
                .unwrap()
                .0
                .text
                .as_str(),
            "前"
        );
    }

    #[test]
    fn sparse_overlay_collection_preserves_last_layer_and_unstyled_precedence() {
        let first = [
            Overlay::new(1, "α"),
            Overlay::new(3, "β"),
            Overlay::new(1_000_000, "γ"),
        ];
        let unstyled = [Overlay::new(1, "x")];
        let last = [
            Overlay::new(3, "y"),
            Overlay::new(3, "z"),
            Overlay::new(8, "q"),
        ];
        let mut annotations = TextAnnotations::default();
        annotations
            .add_overlay(&first, Some(Highlight::new(1)))
            .add_overlay(&unstyled, None)
            .add_overlay(&last, Some(Highlight::new(2)));
        let OverlayHighlights::Heterogenous { highlights } =
            annotations.collect_overlay_highlights(1..8)
        else {
            unreachable!()
        };
        assert_eq!(highlights, [(Highlight::new(2), 3..4)]);
        let OverlayHighlights::Heterogenous { highlights } =
            annotations.collect_overlay_highlights(8..2_000_000)
        else {
            unreachable!()
        };
        assert_eq!(
            highlights,
            [
                (Highlight::new(2), 8..9),
                (Highlight::new(1), 1_000_000..1_000_001)
            ]
        );
        // Collection does not disturb traversal or consume duplicate overlays.
        annotations.reset_pos(3);
        assert_eq!(annotations.overlay_at(3).unwrap().0.grapheme.as_str(), "z");
    }

    #[test]
    fn layout_fingerprint_tracks_content_and_ignores_traversal() {
        let inline = [InlineAnnotation::new(2, "é")];
        let color = [Highlight::new(1)];
        let overlay = [Overlay::new(0, "x")];
        let mut annotations = TextAnnotations::default();
        let empty = annotations.layout_key();
        annotations.add_inline_annotations_with_highlights(&inline, &color);
        let populated = annotations.layout_key();
        assert_ne!(empty, populated);
        annotations.reset_pos(2);
        annotations.next_inline_annotation_at(2);
        assert_eq!(annotations.layout_key(), populated);
        let different = [InlineAnnotation::new(2, "α")];
        let mut other = TextAnnotations::default();
        other.add_inline_annotations_with_highlights(&different, &color);
        assert_ne!(other.layout_key(), populated);
        annotations.add_overlay(&overlay, None);
        assert_ne!(annotations.layout_key(), populated);
        assert!(!annotations.has_line_annotations());
    }
}

impl<A, M: Clone> Clone for Layer<'_, A, M> {
    fn clone(&self) -> Self {
        Layer {
            annotations: self.annotations,
            current_index: self.current_index.clone(),
            metadata: self.metadata.clone(),
            content_key: self.content_key,
            highlights_key: self.highlights_key,
        }
    }
}

impl<A, M> Layer<'_, A, M> {
    pub fn reset_pos(&self, char_idx: usize, get_char_idx: impl Fn(&A) -> usize) {
        let new_index = self
            .annotations
            .partition_point(|annot| get_char_idx(annot) < char_idx);
        self.current_index.set(new_index);
    }

    pub fn consume(&self, char_idx: usize, get_char_idx: impl Fn(&A) -> usize) -> Option<&A> {
        let annot = self.annotations.get(self.current_index.get())?;
        debug_assert!(get_char_idx(annot) >= char_idx);
        if get_char_idx(annot) == char_idx {
            self.current_index.set(self.current_index.get() + 1);
            Some(annot)
        } else {
            None
        }
    }
}

impl<'a, A, M> From<(&'a [A], M)> for Layer<'a, A, M> {
    fn from((annotations, metadata): (&'a [A], M)) -> Layer<'a, A, M> {
        Layer {
            annotations,
            current_index: Cell::new(0),
            metadata,
            content_key: None,
            highlights_key: None,
        }
    }
}

fn reset_pos<A, M>(layers: &[Layer<A, M>], pos: usize, get_pos: impl Fn(&A) -> usize) {
    for layer in layers {
        layer.reset_pos(pos, &get_pos)
    }
}

/// Safety: We store LineAnnotation in a NonNull pointer. This is necessary to work
/// around an unfortunate inconsistency in rusts variance system that unnnecesarily
/// makes the lifetime invariant if implemented with safe code. This makes the
/// DocFormatter API very cumbersome/basically impossible to work with.
///
/// Normally object types `dyn Foo + 'a` are covariant so if we used `Box<dyn LineAnnotation + 'a>` below
/// everything would be alright. However we want to use `Cell<Box<dyn LineAnnotation + 'a>>`
/// to be able to call the mutable function on `LineAnnotation`. The problem is that
/// some types like `Cell` make all their arguments invariant. This is important for soundness
/// normally for the same reasons that `&'a mut T` is invariant over `T`
/// (see <https://doc.rust-lang.org/nomicon/subtyping.html>). However for `&'a mut` (`dyn Foo + 'b`)
/// there is a specical rule in the language to make `'b` covariant (otherwise trait objects would be
/// super annoying to use). See  <https://users.rust-lang.org/t/solved-variance-of-dyn-trait-a> for
/// why this is sound. Sadly that rule doesn't apply to `Cell<Box<(dyn Foo + 'a)>`
/// (or other invariant types like `UnsafeCell` or `*mut (dyn Foo + 'a)`).
///
/// We sidestep the problem by using `NonNull` which is covariant. In the
/// special case of trait objects this is sound (easily checked by adding a
/// `PhantomData<&'a mut Foo + 'a)>` field). We don't need an explicit `Cell`
/// type here because we never hand out any refereces to the trait objects. That
/// means any reference to the pointer can create a valid multable reference
/// that is covariant over `'a` (or in other words it's a raw pointer, as long as
/// we don't hand out references we are free to do whatever we want).
struct RawBox<T: ?Sized>(NonNull<T>);

impl<T: ?Sized> RawBox<T> {
    /// Safety: Only a single mutable reference
    /// created by this function may exist at a given time.
    #[allow(clippy::mut_from_ref)]
    unsafe fn get(&self) -> &mut T {
        &mut *self.0.as_ptr()
    }
}
impl<T: ?Sized> From<Box<T>> for RawBox<T> {
    fn from(box_: Box<T>) -> Self {
        // obviously safe because Box::into_raw never returns null
        unsafe { Self(NonNull::new_unchecked(Box::into_raw(box_))) }
    }
}

impl<T: ?Sized> Drop for RawBox<T> {
    fn drop(&mut self) {
        unsafe { drop(Box::from_raw(self.0.as_ptr())) }
    }
}

/// Annotations that change that is displayed when the document is render.
/// Also commonly called virtual text.
#[derive(Default)]
pub struct TextAnnotations<'a> {
    inline_annotations: Vec<Layer<'a, InlineAnnotation, InlineHighlights<'a>>>,
    overlays: Vec<Layer<'a, Overlay, Option<Highlight>>>,
    line_annotations: Vec<(Cell<usize>, RawBox<dyn LineAnnotation + 'a>)>,
    layout_key: Cell<Option<u64>>,
}

impl Debug for TextAnnotations<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextAnnotations")
            .field("inline_annotations", &self.inline_annotations)
            .field("overlays", &self.overlays)
            .finish_non_exhaustive()
    }
}

impl<'a> TextAnnotations<'a> {
    /// Prepare the TextAnnotations for iteration starting at char_idx
    pub fn reset_pos(&self, char_idx: usize) {
        reset_pos(&self.inline_annotations, char_idx, |annot| annot.char_idx);
        reset_pos(&self.overlays, char_idx, |annot| annot.char_idx);
        for (next_anchor, layer) in &self.line_annotations {
            next_anchor.set(unsafe { layer.get().reset_pos(char_idx) });
        }
    }

    pub fn collect_overlay_highlights(&self, char_range: Range<usize>) -> OverlayHighlights {
        let mut candidates = Vec::new();
        for layer in &self.overlays {
            let start = layer
                .annotations
                .partition_point(|annotation| annotation.char_idx < char_range.start);
            for annotation in layer.annotations[start..]
                .iter()
                .take_while(|annotation| annotation.char_idx < char_range.end)
            {
                // Preserve both layer precedence and duplicate ordering. A final
                // unstyled overlay also masks an earlier styled overlay.
                candidates.push((annotation.char_idx, candidates.len(), layer.metadata));
            }
        }
        candidates.sort_unstable_by_key(|&(pos, order, _)| (pos, order));
        let highlights = candidates
            .iter()
            .enumerate()
            .filter_map(|(index, &(pos, _, highlight))| {
                if candidates.get(index + 1).is_some_and(|next| next.0 == pos) {
                    return None;
                }
                // The renderer aligns this character range to its grapheme.
                highlight.map(|highlight| (highlight, pos..pos + 1))
            })
            .collect();

        OverlayHighlights::Heterogenous { highlights }
    }

    /// Fingerprint the immutable annotation content used by formatter checkpoints.
    /// This is computed lazily once per annotation set, excluding traversal state.
    pub fn layout_key(&self) -> u64 {
        if let Some(key) = self.layout_key.get() {
            return key;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.inline_annotations.len().hash(&mut hasher);
        for layer in &self.inline_annotations {
            layer
                .content_key
                .unwrap_or_else(|| InlineAnnotation::layout_key(layer.annotations))
                .hash(&mut hasher);
            match &layer.metadata {
                InlineHighlights::Homogeneous(highlight) => {
                    0u8.hash(&mut hasher);
                    highlight.map(|highlight| highlight.get()).hash(&mut hasher);
                }
                InlineHighlights::Heterogeneous(highlights) => {
                    1u8.hash(&mut hasher);
                    layer
                        .highlights_key
                        .unwrap_or_else(|| Self::highlights_key(highlights))
                        .hash(&mut hasher);
                }
            }
        }
        self.overlays.len().hash(&mut hasher);
        for layer in &self.overlays {
            layer.annotations.len().hash(&mut hasher);
            layer
                .metadata
                .map(|highlight| highlight.get())
                .hash(&mut hasher);
            for annotation in layer.annotations {
                annotation.char_idx.hash(&mut hasher);
                annotation.grapheme.as_bytes().hash(&mut hasher);
            }
        }
        self.line_annotations.len().hash(&mut hasher);
        for (_, layer) in &self.line_annotations {
            unsafe { layer.get().checkpoint_key() }.hash(&mut hasher);
        }
        let key = hasher.finish();
        self.layout_key.set(Some(key));
        key
    }

    /// Stateful line annotations cannot yet be restored from a formatter checkpoint.
    pub fn has_line_annotations(&self) -> bool {
        !self.line_annotations.is_empty()
    }

    pub(crate) fn can_checkpoint(&self) -> bool {
        self.line_annotations
            .iter()
            .all(|(_, layer)| unsafe { layer.get().checkpoint_key().is_some() })
    }

    pub(crate) fn checkpoint(&self) -> Option<Vec<LineAnnotationCheckpoint>> {
        self.line_annotations
            .iter()
            .map(|(anchor, layer)| {
                Some(LineAnnotationCheckpoint {
                    next_anchor: anchor.get(),
                    state: unsafe { layer.get().checkpoint()? },
                })
            })
            .collect()
    }

    pub(crate) fn checkpoint_is_valid(
        &self,
        checkpoints: &[LineAnnotationCheckpoint],
        char_idx: usize,
    ) -> bool {
        self.line_annotations.len() == checkpoints.len()
            && self.line_annotations.iter().zip(checkpoints).all(
                |((_, layer), checkpoint)| unsafe {
                    layer
                        .get()
                        .checkpoint_is_valid(checkpoint.state.as_ref(), char_idx)
                },
            )
    }

    pub(crate) fn restore_checkpoint(
        &self,
        checkpoints: &[LineAnnotationCheckpoint],
        char_idx: usize,
    ) -> bool {
        self.line_annotations.len() == checkpoints.len()
            && self
                .line_annotations
                .iter()
                .zip(checkpoints)
                .all(|((anchor, layer), checkpoint)| {
                    if unsafe { layer.get().restore_checkpoint(checkpoint.state.as_ref()) } {
                        anchor.set(
                            unsafe { layer.get().checkpoint_next_anchor(char_idx) }
                                .unwrap_or(checkpoint.next_anchor),
                        );
                        true
                    } else {
                        false
                    }
                })
    }

    pub fn highlights_key(highlights: &[Highlight]) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        highlights.len().hash(&mut hasher);
        for highlight in highlights {
            highlight.get().hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Add a layer whose immutable content fingerprint was prepared at mutation.
    pub fn add_inline_annotations_cached(
        &mut self,
        layer: &'a [InlineAnnotation],
        highlight: Option<Highlight>,
        content_key: Option<u64>,
    ) -> &mut Self {
        self.add_inline_annotations(layer, highlight);
        if !layer.is_empty() {
            self.inline_annotations.last_mut().unwrap().content_key = content_key;
        }
        self
    }

    /// Cached counterpart of `add_inline_annotations_with_highlights`.
    pub fn add_inline_annotations_with_highlights_cached(
        &mut self,
        layer: &'a [InlineAnnotation],
        highlights: &'a [Highlight],
        keys: Option<(u64, u64)>,
    ) -> &mut Self {
        self.add_inline_annotations_with_highlights(layer, highlights);
        if let Some((content, colors)) = keys.filter(|_| !layer.is_empty()) {
            let layer = self.inline_annotations.last_mut().unwrap();
            layer.content_key = Some(content);
            layer.highlights_key = Some(colors);
        }
        self
    }

    /// Add new inline annotations.
    ///
    /// The annotations grapheme will be rendered with `highlight`
    /// patched on top of `ui.text`.
    ///
    /// The annotations **must be sorted** by their `char_idx`.
    /// Multiple annotations with the same `char_idx` are allowed,
    /// they will be display in the order that they are present in the layer.
    ///
    /// If multiple layers contain annotations at the same position
    /// the annotations that belong to the layers added first will be shown first.
    pub fn add_inline_annotations(
        &mut self,
        layer: &'a [InlineAnnotation],
        highlight: Option<Highlight>,
    ) -> &mut Self {
        if !layer.is_empty() {
            self.layout_key.set(None);
            self.inline_annotations
                .push((layer, InlineHighlights::Homogeneous(highlight)).into());
        }
        self
    }

    /// Add one sorted annotation stream with a highlight for each item.
    /// Its order relative to other streams, and duplicate positions within the
    /// stream, follow the same rules as `add_inline_annotations`.
    pub fn add_inline_annotations_with_highlights(
        &mut self,
        layer: &'a [InlineAnnotation],
        highlights: &'a [Highlight],
    ) -> &mut Self {
        assert_eq!(layer.len(), highlights.len());
        if !layer.is_empty() {
            self.layout_key.set(None);
            self.inline_annotations
                .push((layer, InlineHighlights::Heterogeneous(highlights)).into());
        }
        self
    }

    /// Add new grapheme overlays.
    ///
    /// The overlaid grapheme will be rendered with `highlight`
    /// patched on top of `ui.text`.
    ///
    /// The overlays **must be sorted** by their `char_idx`.
    /// Multiple overlays with the same `char_idx` **are allowed**.
    ///
    /// If multiple layers contain overlay at the same position
    /// the overlay from the layer added last will be show.
    pub fn add_overlay(&mut self, layer: &'a [Overlay], highlight: Option<Highlight>) -> &mut Self {
        if !layer.is_empty() {
            self.layout_key.set(None);
            self.overlays.push((layer, highlight).into());
        }
        self
    }

    /// Add new annotation lines.
    ///
    /// The line annotations **must be sorted** by their `char_idx`.
    /// Multiple line annotations with the same `char_idx` **are not allowed**.
    pub fn add_line_annotation(&mut self, layer: Box<dyn LineAnnotation + 'a>) -> &mut Self {
        self.layout_key.set(None);
        self.line_annotations
            .push((Cell::new(usize::MAX), layer.into()));
        self
    }

    /// Removes all line annotations, useful for vertical motions
    /// so that virtual text lines are automatically skipped.
    pub fn clear_line_annotations(&mut self) {
        self.layout_key.set(None);
        self.line_annotations.clear();
    }

    pub(crate) fn next_inline_annotation_at(
        &self,
        char_idx: usize,
    ) -> Option<(&InlineAnnotation, Option<Highlight>)> {
        self.inline_annotations.iter().find_map(|layer| {
            let index = layer.current_index.get();
            let annotation = layer.consume(char_idx, |annot| annot.char_idx)?;
            let highlight = match layer.metadata {
                InlineHighlights::Homogeneous(highlight) => highlight,
                InlineHighlights::Heterogeneous(highlights) => Some(highlights[index]),
            };
            Some((annotation, highlight))
        })
    }

    pub(crate) fn overlay_at(&self, char_idx: usize) -> Option<(&Overlay, Option<Highlight>)> {
        let mut overlay = None;
        for layer in &self.overlays {
            while let Some(new_overlay) = layer.consume(char_idx, |annot| annot.char_idx) {
                overlay = Some((new_overlay, layer.metadata));
            }
        }
        overlay
    }

    pub(crate) fn process_virtual_text_anchors(&self, grapheme: &FormattedGrapheme) {
        for (next_anchor, layer) in &self.line_annotations {
            loop {
                match next_anchor.get().cmp(&grapheme.char_idx) {
                    Ordering::Less => next_anchor
                        .set(unsafe { layer.get().skip_concealed_anchors(grapheme.char_idx) }),
                    Ordering::Equal => {
                        next_anchor.set(unsafe { layer.get().process_anchor(grapheme) })
                    }
                    Ordering::Greater => break,
                };
            }
        }
    }

    pub(crate) fn virtual_lines_at(
        &self,
        char_idx: usize,
        line_end_visual_pos: Position,
        doc_line: usize,
    ) -> usize {
        let mut virt_off = Position::new(0, 0);
        for (_, layer) in &self.line_annotations {
            virt_off += unsafe {
                layer
                    .get()
                    .insert_virtual_lines(char_idx, line_end_visual_pos + virt_off, doc_line)
            };
        }
        virt_off.row
    }
}
