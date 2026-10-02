use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::BTreeMap,
    ops::{Add, AddAssign, Sub, SubAssign},
};

use helix_stdx::rope::RopeSliceExt;
use parking_lot::Mutex;

use crate::{
    chars::char_is_line_ending,
    doc_formatter::{DocumentFormatter, TextFormat},
    graphemes::{ensure_grapheme_boundary_prev, grapheme_width, prev_grapheme_boundary},
    line_ending::line_end_char_index,
    text_annotations::TextAnnotations,
    RopeSlice,
};

/// Represents a single point in a text buffer. Zero indexed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub row: usize,
    pub col: usize,
}

impl AddAssign for Position {
    fn add_assign(&mut self, rhs: Self) {
        self.row += rhs.row;
        self.col += rhs.col;
    }
}

impl SubAssign for Position {
    fn sub_assign(&mut self, rhs: Self) {
        self.row -= rhs.row;
        self.col -= rhs.col;
    }
}

impl Sub for Position {
    type Output = Position;

    fn sub(mut self, rhs: Self) -> Self::Output {
        self -= rhs;
        self
    }
}

impl Add for Position {
    type Output = Position;

    fn add(mut self, rhs: Self) -> Self::Output {
        self += rhs;
        self
    }
}

impl Position {
    pub const fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }

    pub const fn is_zero(self) -> bool {
        self.row == 0 && self.col == 0
    }

    // TODO: generalize
    pub fn traverse(self, text: &crate::Tendril) -> Self {
        let Self { mut row, mut col } = self;
        // TODO: there should be a better way here
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if char_is_line_ending(ch) && !(ch == '\r' && chars.peek() == Some(&'\n')) {
                row += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        Self { row, col }
    }
}

impl From<(usize, usize)> for Position {
    fn from(tuple: (usize, usize)) -> Self {
        Self {
            row: tuple.0,
            col: tuple.1,
        }
    }
}

/// Convert a character index to (line, column) coordinates.
///
/// column in `char` count which can be used for row:column display in
/// status line. See [`visual_coords_at_pos`] for a visual one.
pub fn coords_at_pos(text: RopeSlice, pos: usize) -> Position {
    let line = text.char_to_line(pos);

    let line_start = text.line_to_char(line);
    let pos = ensure_grapheme_boundary_prev(text, pos);
    let col = text.slice(line_start..pos).graphemes().count();

    Position::new(line, col)
}

const COORD_CHECKPOINT_INTERVAL: usize = 1024;
const MAX_COORD_LINES: usize = 8;
const MAX_COORD_CHECKPOINTS_PER_LINE: usize = 512;

#[derive(Debug, Default)]
struct GraphemeColumns {
    checkpoints: BTreeMap<usize, usize>,
    last: Option<(usize, usize)>,
    uncached: bool,
    used: u64,
}

#[derive(Debug, Default)]
struct GraphemeCoordinates {
    len: Option<usize>,
    lines: BTreeMap<usize, GraphemeColumns>,
    last: Option<(usize, Position)>,
    clock: u64,
}

/// Bounded raw grapheme-column checkpoints for one document.
///
/// Unlike visual layout, these coordinates are independent of wrapping, tabs'
/// display width and annotations. Call [`Self::invalidate_after_change`] when
/// text changes, including replacements that keep its length unchanged.
#[derive(Debug, Default)]
pub struct GraphemePositionCache(Mutex<GraphemeCoordinates>);

impl GraphemePositionCache {
    pub fn clear(&self) {
        *self.0.lock() = GraphemeCoordinates::default();
    }

    /// Retain the unaffected prefix, including checkpoints on the edited line.
    /// Dropping the previous grapheme also handles combining characters, CRLF,
    /// regional indicators and other edits that join an existing cluster.
    pub fn invalidate_after_change(
        &self,
        old_text: RopeSlice,
        first_change: usize,
        new_len: usize,
    ) {
        let mut cache = self.0.lock();
        if cache.len != Some(old_text.len_chars()) {
            *cache = GraphemeCoordinates::default();
        } else {
            let safe_pos = prev_grapheme_boundary(old_text, first_change);
            let line_start = old_text.line_to_char(old_text.char_to_line(safe_pos));
            cache.lines.retain(|start, line| {
                if *start > line_start {
                    return false;
                }
                if *start == line_start {
                    line.checkpoints.retain(|pos, _| *pos <= safe_pos);
                    if line.last.is_some_and(|(pos, _)| pos > safe_pos) {
                        line.last = None;
                    }
                }
                true
            });
            cache.last = None;
        }
        cache.len = Some(new_len);
    }

    pub fn coords_at_pos(&self, text: RopeSlice, pos: usize) -> Position {
        let mut cache = self.0.lock();
        if cache.len != Some(text.len_chars()) {
            *cache = GraphemeCoordinates {
                len: Some(text.len_chars()),
                ..GraphemeCoordinates::default()
            };
        }
        if let Some((last_pos, coords)) = cache.last {
            if last_pos == pos {
                return coords;
            }
        }

        let row = text.char_to_line(pos);
        let line_start = text.line_to_char(row);
        let target = ensure_grapheme_boundary_prev(text, pos);
        if !cache.lines.contains_key(&line_start) && cache.lines.len() == MAX_COORD_LINES {
            let oldest = *cache
                .lines
                .iter()
                .min_by_key(|(_, line)| line.used)
                .unwrap()
                .0;
            cache.lines.remove(&oldest);
        }
        cache.clock = cache.clock.wrapping_add(1);
        let used = cache.clock;
        let line = cache.lines.entry(line_start).or_default();
        line.used = used;
        if line.uncached {
            let coords = coords_at_pos(text, pos);
            cache.last = Some((pos, coords));
            return coords;
        }
        let (mut char_pos, mut col) = line
            .checkpoints
            .range(..=target)
            .next_back()
            .map(|(&pos, &col)| (pos, col))
            .unwrap_or((line_start, 0));
        let mut last_checkpoint = char_pos;
        if let Some((last_pos, last_col)) = line.last {
            if last_pos <= target && last_pos > char_pos {
                (char_pos, col) = (last_pos, last_col);
            }
        }
        // Keep the full rope as segmentation context. Slicing at a checkpoint
        // could change regional-indicator pairing or other Unicode boundaries.
        let mut graphemes = text.graphemes_at(text.char_to_byte(char_pos));
        while char_pos < target {
            let grapheme = graphemes.next().unwrap();
            let chars = grapheme.len_chars();
            char_pos += chars;
            // The rope's forward segmenter and boundary lookup can disagree
            // on long regional-indicator runs crossing chunk seams. Keep the
            // existing raw-coordinate result instead of resuming inconsistent
            // checkpoints. Unchanged queries are still memoized in this case.
            let regional_indicator = grapheme
                .chars()
                .next()
                .is_some_and(|ch| ('\u{1f1e6}'..='\u{1f1ff}').contains(&ch));
            if char_pos > target
                || (regional_indicator
                    && (chars > 2
                        || (chars == 1
                            && char_pos < text.len_chars()
                            && ('\u{1f1e6}'..='\u{1f1ff}').contains(&text.char(char_pos)))))
            {
                line.uncached = true;
                line.checkpoints.clear();
                line.last = None;
                let coords = coords_at_pos(text, pos);
                cache.last = Some((pos, coords));
                return coords;
            }
            col += 1;
            if char_pos - last_checkpoint >= COORD_CHECKPOINT_INTERVAL {
                line.checkpoints.insert(char_pos, col);
                if line.checkpoints.len() > MAX_COORD_CHECKPOINTS_PER_LINE {
                    line.checkpoints.pop_first();
                }
                last_checkpoint = char_pos;
            }
        }
        line.last = Some((target, col));
        let coords = Position::new(row, col);
        cache.last = Some((pos, coords));
        coords
    }
}

/// Convert a character index to (line, column) coordinates visually.
///
/// Takes \t, double-width characters (CJK) into account as well as text
/// not in the document in the future.
/// See [`coords_at_pos`] for an "objective" one.
///
/// This function should be used very rarely. Usually `visual_offset_from_anchor`
/// or `visual_offset_from_block` is preferable. However when you want to compute the
/// actual visual row/column in the text (not what is actually shown on screen)
/// then you should use this function. For example aligning text should ignore virtual
/// text and softwrap.
#[deprecated = "Doesn't account for softwrap or decorations, use visual_offset_from_anchor instead"]
pub fn visual_coords_at_pos(text: RopeSlice, pos: usize, tab_width: usize) -> Position {
    let line = text.char_to_line(pos);

    let line_start = text.line_to_char(line);
    let pos = ensure_grapheme_boundary_prev(text, pos);

    let mut col = 0;

    for grapheme in text.slice(line_start..pos).graphemes() {
        if grapheme == "\t" {
            col += tab_width - (col % tab_width);
        } else {
            let grapheme = Cow::from(grapheme);
            col += grapheme_width(&grapheme);
        }
    }

    Position::new(line, col)
}

/// Returns the visual offset from the start of the first visual line
/// in the block that contains anchor.
/// Text is always wrapped at blocks, they usually correspond to
/// actual line breaks. Cached layout checkpoints avoid rescanning a long
/// physical line for repeated queries.
///
/// Usually you want to use `visual_offset_from_anchor` instead but this function
/// can be useful (and faster) if
/// * You already know the visual position of the block
/// * You only care about the horizontal offset (column) and not the vertical offset (row)
pub fn visual_offset_from_block(
    text: RopeSlice,
    anchor: usize,
    pos: usize,
    text_fmt: &TextFormat,
    annotations: &TextAnnotations,
) -> (Position, usize) {
    let mut last_pos = Position::default();
    let target = if pos <= text.len_chars()
        && text.char_to_line(pos) == text.char_to_line(anchor.min(text.len_chars()))
    {
        pos
    } else {
        anchor
    };
    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text, text_fmt, annotations, target);
    let block_start = formatter.block_start();

    while let Some(grapheme) = formatter.next() {
        last_pos = grapheme.visual_pos;
        if formatter.next_char_pos() > pos {
            return (grapheme.visual_pos, block_start);
        }
    }

    (last_pos, block_start)
}

/// Returns the height of the given text when softwrapping
pub fn softwrapped_dimensions(text: RopeSlice, text_fmt: &TextFormat) -> (usize, u16) {
    let last_pos =
        visual_offset_from_block(text, 0, usize::MAX, text_fmt, &TextAnnotations::default()).0;
    if last_pos.row == 0 {
        (1, last_pos.col as u16)
    } else {
        (last_pos.row + 1, text_fmt.viewport_width)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum VisualOffsetError {
    PosBeforeAnchorRow,
    PosAfterMaxRow,
}

/// Returns the visual offset from the start of the visual line
/// that contains anchor.
pub fn visual_offset_from_anchor(
    text: RopeSlice,
    anchor: usize,
    pos: usize,
    text_fmt: &TextFormat,
    annotations: &TextAnnotations,
    max_rows: usize,
) -> Result<(Position, usize), VisualOffsetError> {
    if pos >= anchor
        && pos <= text.len_chars()
        && text.char_to_line(pos) == text.char_to_line(anchor.min(text.len_chars()))
        && !annotations.has_line_annotations()
    {
        let target_formatter =
            DocumentFormatter::new_at_prev_checkpoint(text, text_fmt, annotations, pos);
        if target_formatter.next_char_pos() > target_formatter.block_start() {
            let (anchor_pos, block_start) =
                visual_offset_from_block(text, anchor, anchor, text_fmt, annotations);
            let (mut target_pos, _) =
                visual_offset_from_block(text, anchor, pos, text_fmt, annotations);
            target_pos.row -= anchor_pos.row;
            if target_pos.row < max_rows || pos == anchor {
                return Ok((target_pos, block_start));
            }
        }
    }
    let start = if pos < anchor
        && text.char_to_line(pos) == text.char_to_line(anchor.min(text.len_chars()))
    {
        pos
    } else {
        anchor
    };
    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text, text_fmt, annotations, start);
    // At BOF the viewport includes virtual rows preceding the first character.
    let mut anchor_line = (anchor == 0 && annotations.leading_virtual_lines() != 0).then_some(0);
    let mut found_pos = None;
    let mut last_pos = Position::default();

    let block_start = formatter.block_start();
    if pos < block_start {
        return Err(VisualOffsetError::PosBeforeAnchorRow);
    }

    while let Some(grapheme) = formatter.next() {
        last_pos = grapheme.visual_pos;

        if formatter.next_char_pos() > pos {
            if let Some(anchor_line) = anchor_line {
                last_pos.row -= anchor_line;
                return Ok((last_pos, block_start));
            } else {
                found_pos = Some(last_pos);
            }
        }
        if formatter.next_char_pos() > anchor && anchor_line.is_none() {
            if let Some(mut found_pos) = found_pos {
                return if found_pos.row == last_pos.row {
                    found_pos.row = 0;
                    Ok((found_pos, block_start))
                } else {
                    Err(VisualOffsetError::PosBeforeAnchorRow)
                };
            } else {
                anchor_line = Some(last_pos.row);
            }
        }

        if let Some(anchor_line) = anchor_line {
            // Compare relative rows so usize::MAX can request an unbounded scan.
            if grapheme.visual_pos.row.saturating_sub(anchor_line) >= max_rows {
                return Err(VisualOffsetError::PosAfterMaxRow);
            }
        }
    }

    let anchor_line = anchor_line.unwrap_or(last_pos.row);
    last_pos.row -= anchor_line;

    Ok((last_pos, block_start))
}

/// Convert (line, column) coordinates to a character index.
///
/// If the `line` coordinate is beyond the end of the file, the EOF
/// position will be returned.
///
/// If the `column` coordinate is past the end of the given line, the
/// line-end position will be returned.  What constitutes the "line-end
/// position" depends on the parameter `limit_before_line_ending`.  If it's
/// `true`, the line-end position will be just *before* the line ending
/// character.  If `false` it will be just *after* the line ending
/// character--on the border between the current line and the next.
///
/// Usually you only want `limit_before_line_ending` to be `true` if you're working
/// with left-side block-cursor positions, as this prevents the the block cursor
/// from jumping to the next line.  Otherwise you typically want it to be `false`,
/// such as when dealing with raw anchor/head positions.
pub fn pos_at_coords(text: RopeSlice, coords: Position, limit_before_line_ending: bool) -> usize {
    let Position { mut row, col } = coords;
    if limit_before_line_ending {
        let lines = text.len_lines() - 1;

        row = row.min(if crate::line_ending::get_line_ending(&text).is_some() {
            // if the last line is empty, don't jump to it
            lines - 1
        } else {
            lines
        });
    };
    let line_start = text.line_to_char(row);
    let line_end = if limit_before_line_ending {
        line_end_char_index(&text, row)
    } else {
        text.line_to_char((row + 1).min(text.len_lines()))
    };

    let mut col_char_offset = 0;
    for (i, g) in text.slice(line_start..line_end).graphemes().enumerate() {
        if i == col {
            break;
        }
        col_char_offset += g.chars().count();
    }

    line_start + col_char_offset
}

/// Convert visual (line, column) coordinates to a character index.
///
/// If the `line` coordinate is beyond the end of the file, the EOF
/// position will be returned.
///
/// If the `column` coordinate is past the end of the given line, the
/// line-end position (in this case, just before the line ending
/// character) will be returned.
/// This function should be used very rarely. Usually `char_idx_at_visual_offset` is preferable.
/// However when you want to compute a char position from the visual row/column in the text
/// (not what is actually shown on screen) then you should use this function.
/// For example aligning text should ignore virtual text and softwrap.
#[deprecated = "Doesn't account for softwrap or decorations, use char_idx_at_visual_offset instead"]
pub fn pos_at_visual_coords(text: RopeSlice, coords: Position, tab_width: usize) -> usize {
    let Position { mut row, col } = coords;
    row = row.min(text.len_lines() - 1);
    let line_start = text.line_to_char(row);
    let line_end = line_end_char_index(&text, row);

    let mut col_char_offset = 0;
    let mut cols_remaining = col;
    for grapheme in text.slice(line_start..line_end).graphemes() {
        let grapheme_width = if grapheme == "\t" {
            tab_width - ((col - cols_remaining) % tab_width)
        } else {
            let grapheme = Cow::from(grapheme);
            grapheme_width(&grapheme)
        };

        // If pos is in the middle of a wider grapheme (tab for example)
        // return the starting offset.
        if grapheme_width > cols_remaining {
            break;
        }

        cols_remaining -= grapheme_width;
        col_char_offset += grapheme.chars().count();
    }

    line_start + col_char_offset
}

/// Returns the char index on the visual line `row_offset` below the visual line of
/// the provided char index `anchor` that is closest to the supplied visual `column`.
///
/// If the targeted visual line is entirely covered by virtual text the last
/// char position before the virtual text and a virtual offset is returned instead.
///
/// If no (text) grapheme starts at exactly at the specified column the
/// start of the grapheme to the left is returned. If there is no grapheme
/// to the left (for example if the line starts with virtual text) then the positioning
/// of the next grapheme to the right is returned.
///
/// If the `line` coordinate is beyond the end of the file, the EOF
/// position will be returned.
///
/// If the `column` coordinate is past the end of the given line, the
/// line-end position (in this case, just before the line ending
/// character) will be returned.
///
/// # Returns
///
/// `(real_char_idx, virtual_lines)`
///
/// The nearest character idx "closest" (see above) to the specified visual offset
/// on the visual line is returned if the visual line contains any text:
/// If the visual line at the specified offset is a virtual line generated by a `LineAnnotation`
/// the previous char_index is returned, together with the remaining vertical offset (`virtual_lines`)
pub fn char_idx_at_visual_offset(
    text: RopeSlice,
    mut anchor: usize,
    mut row_offset: isize,
    column: usize,
    text_fmt: &TextFormat,
    annotations: &TextAnnotations,
) -> (usize, usize) {
    let mut pos = anchor;
    // convert row relative to visual line containing anchor to row relative to a block containing anchor (anchor may change)
    loop {
        let (visual_pos_in_block, block_char_offset) =
            visual_offset_from_block(text, anchor, pos, text_fmt, annotations);
        row_offset += visual_pos_in_block.row as isize
            - if anchor == 0 && pos == 0 {
                annotations.leading_virtual_lines() as isize
            } else {
                0
            };
        anchor = block_char_offset;
        if row_offset >= 0 {
            break;
        }

        if block_char_offset == 0 {
            row_offset = 0;
            break;
        }
        // the row_offset is negative so we need to look at the previous block
        // set the anchor to the last char before the current block so that we can compute
        // the distance of this block from the start of the previous block
        pos = anchor;
        anchor -= 1;
    }

    char_idx_at_visual_block_offset(
        text,
        anchor,
        row_offset as usize,
        column,
        text_fmt,
        annotations,
    )
}

/// This function behaves the same as `char_idx_at_visual_offset`, except that
/// the vertical offset `row` is always computed relative to the block that contains `anchor`
/// instead of the visual line that contains `anchor`.
/// Usually `char_idx_at_visual_offset` is more useful but this function can be
/// used in some situations as an optimization when `visual_offset_from_block` was used
///
/// # Returns
///
/// `(real_char_idx, virtual_lines)`
///
/// See `char_idx_at_visual_offset` for details
pub fn char_idx_at_visual_block_offset(
    text: RopeSlice,
    anchor: usize,
    row: usize,
    column: usize,
    text_fmt: &TextFormat,
    annotations: &TextAnnotations,
) -> (usize, usize) {
    if anchor == 0 && row < annotations.leading_virtual_lines() {
        return (0, row);
    }
    let mut formatter = DocumentFormatter::new_at_visual_checkpoint(
        text,
        text_fmt,
        annotations,
        anchor,
        Position::new(row, column),
    );
    let (mut last_char_idx, mut last_row) = formatter.previous_document_position();
    let mut found_non_virtual_on_row =
        formatter.next_char_pos() > formatter.block_start() && last_row == row;
    for grapheme in &mut formatter {
        match grapheme.visual_pos.row.cmp(&row) {
            Ordering::Equal => {
                if grapheme.visual_pos.col + grapheme.width() > column {
                    if !grapheme.is_virtual() {
                        return (grapheme.char_idx, 0);
                    } else if found_non_virtual_on_row {
                        return (last_char_idx, 0);
                    }
                } else if !grapheme.is_virtual() {
                    found_non_virtual_on_row = true;
                    last_char_idx = grapheme.char_idx;
                }
            }
            Ordering::Greater if found_non_virtual_on_row => return (last_char_idx, 0),
            Ordering::Greater => return (last_char_idx, row - last_row),
            Ordering::Less => {
                if !grapheme.is_virtual() {
                    last_row = grapheme.visual_pos.row;
                    last_char_idx = grapheme.char_idx;
                }
            }
        }
    }

    (formatter.next_char_pos(), 0)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::text_annotations::InlineAnnotation;
    use crate::Rope;

    #[test]
    fn visual_offset_from_wrapped_anchor_accepts_unbounded_row_limit() {
        let text = Rope::from_str(&format!("{}\ntarget\n", "wrapped source ".repeat(20)));
        let text = text.slice(..);
        let format = TextFormat {
            soft_wrap: true,
            viewport_width: 40,
            ..TextFormat::default()
        };
        let annotations = TextAnnotations::default();
        let anchor = 100;
        let target = text.line_to_char(1);
        let (anchor_pos, block) =
            visual_offset_from_block(text, anchor, anchor, &format, &annotations);
        assert!(anchor_pos.row > 0);
        let (mut target_pos, _) =
            visual_offset_from_block(text, anchor, target, &format, &annotations);
        target_pos.row -= anchor_pos.row;
        assert_eq!(
            visual_offset_from_anchor(text, anchor, target, &format, &annotations, usize::MAX),
            Ok((target_pos, block))
        );
        assert_eq!(
            visual_offset_from_anchor(
                text,
                anchor,
                target,
                &format,
                &annotations,
                target_pos.row - 1
            ),
            Err(VisualOffsetError::PosAfterMaxRow)
        );
        assert_eq!(
            visual_offset_from_anchor(
                text,
                anchor,
                target,
                &format,
                &annotations,
                target_pos.row + 1
            ),
            Ok((target_pos, block))
        );
    }

    #[test]
    fn test_ordering() {
        // (0, 5) is less than (1, 0)
        assert!(Position::new(0, 5) < Position::new(1, 0));
    }

    #[test]
    fn cached_raw_coordinates_match_unicode_graphemes_in_both_directions() {
        let text = Rope::from_str(&format!(
            "{}\r\n{}\n{}",
            "界e\u{301}🇵🇱\t👩\u{200d}💻".repeat(500),
            "🇦🇧🇨".repeat(1200),
            "किमपि".repeat(600),
        ));
        let cache = GraphemePositionCache::default();
        let mut positions: Vec<_> = (0..text.len_chars()).step_by(137).collect();
        positions.extend([0, 1, 2, 3, 4, 5, text.len_chars()]);
        positions.extend((1000..1028).chain(4090..4105));
        for pos in positions
            .iter()
            .copied()
            .chain(positions.iter().copied().rev())
        {
            assert_eq!(
                cache.coords_at_pos(text.slice(..), pos),
                coords_at_pos(text.slice(..), pos),
                "{pos}"
            );
            assert_eq!(
                cache.coords_at_pos(text.slice(..), pos),
                coords_at_pos(text.slice(..), pos),
                "repeated {pos}"
            );
        }
        assert!(cache
            .0
            .lock()
            .lines
            .values()
            .any(|line| !line.checkpoints.is_empty()));
    }

    #[test]
    fn raw_coordinate_edits_retain_only_safe_prefix_checkpoints() {
        use crate::ChangeSet;
        let original = Rope::from_str(&format!(
            "{}🇦🇧🇨🇩e\u{301}\r\n{}",
            "a".repeat(5000),
            "b".repeat(3000),
        ));
        for change in [
            (4096, 4096, Some("\u{301}".into())),
            (4096, 4097, Some("界".into())),
            (5001, 5002, Some("🇿".into())),
            (5004, 5005, Some("👩\u{200d}💻".into())),
            (5006, 5007, None),
            (4096, 5008, Some("\n".into())),
        ] {
            let cache = GraphemePositionCache::default();
            cache.coords_at_pos(original.slice(..), 5006);
            cache.coords_at_pos(original.slice(..), original.len_chars());
            let changes = ChangeSet::from_change(&original, change.clone());
            let mut edited = original.clone();
            assert!(changes.apply(&mut edited));
            cache.invalidate_after_change(original.slice(..), change.0, edited.len_chars());
            {
                let state = cache.0.lock();
                assert_eq!(state.lines.len(), 1);
                let safe_pos = prev_grapheme_boundary(original.slice(..), change.0);
                let checkpoints = &state.lines[&0].checkpoints;
                assert!(!checkpoints.is_empty());
                assert!(checkpoints.keys().all(|&pos| pos <= safe_pos));
            }
            for pos in (0..edited.len_chars())
                .step_by(173)
                .chain([edited.len_chars()])
            {
                assert_eq!(
                    cache.coords_at_pos(edited.slice(..), pos),
                    coords_at_pos(edited.slice(..), pos),
                    "{change:?}, {pos}"
                );
            }
        }
    }

    #[test]
    fn raw_coordinate_cache_bounds_retained_lines_and_checkpoints() {
        let cache = GraphemePositionCache::default();
        let text = Rope::from_str(&format!(
            "{}\n{}",
            "x".repeat(600_000),
            "short\n".repeat(20)
        ));
        cache.coords_at_pos(text.slice(..), 600_000);
        assert_eq!(
            cache.0.lock().lines[&0].checkpoints.len(),
            MAX_COORD_CHECKPOINTS_PER_LINE
        );
        for line in 1..text.len_lines() {
            cache.coords_at_pos(text.slice(..), text.line_to_char(line));
        }
        assert_eq!(cache.0.lock().lines.len(), MAX_COORD_LINES);
        assert!(!cache.0.lock().lines.contains_key(&0));
    }

    #[test]
    fn small_cursor_moves_still_record_sparse_raw_checkpoints() {
        let text = Rope::from_str(&"a".repeat(4000));
        let cache = GraphemePositionCache::default();
        for pos in (0..4000).step_by(7) {
            assert_eq!(
                cache.coords_at_pos(text.slice(..), pos),
                Position::new(0, pos)
            );
        }
        assert!(cache.0.lock().lines[&0].checkpoints.len() >= 3);
        for pos in (0..4000).step_by(7).rev() {
            assert_eq!(
                cache.coords_at_pos(text.slice(..), pos),
                Position::new(0, pos)
            );
        }
    }

    #[test]
    fn test_coords_at_pos() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ");
        let slice = text.slice(..);
        assert_eq!(coords_at_pos(slice, 0), (0, 0).into());
        assert_eq!(coords_at_pos(slice, 5), (0, 5).into()); // position on \n
        assert_eq!(coords_at_pos(slice, 6), (1, 0).into()); // position on w
        assert_eq!(coords_at_pos(slice, 7), (1, 1).into()); // position on o
        assert_eq!(coords_at_pos(slice, 10), (1, 4).into()); // position on d

        // Test with wide characters.
        let text = Rope::from("今日はいい\n");
        let slice = text.slice(..);
        assert_eq!(coords_at_pos(slice, 0), (0, 0).into());
        assert_eq!(coords_at_pos(slice, 1), (0, 1).into());
        assert_eq!(coords_at_pos(slice, 2), (0, 2).into());
        assert_eq!(coords_at_pos(slice, 3), (0, 3).into());
        assert_eq!(coords_at_pos(slice, 4), (0, 4).into());
        assert_eq!(coords_at_pos(slice, 5), (0, 5).into());
        assert_eq!(coords_at_pos(slice, 6), (1, 0).into());

        // Test with grapheme clusters.
        let text = Rope::from("a̐éö̲\r\n");
        let slice = text.slice(..);
        assert_eq!(coords_at_pos(slice, 0), (0, 0).into());
        assert_eq!(coords_at_pos(slice, 2), (0, 1).into());
        assert_eq!(coords_at_pos(slice, 4), (0, 2).into());
        assert_eq!(coords_at_pos(slice, 7), (0, 3).into());
        assert_eq!(coords_at_pos(slice, 9), (1, 0).into());

        // Test with wide-character grapheme clusters.
        let text = Rope::from("किमपि\n");
        let slice = text.slice(..);
        assert_eq!(coords_at_pos(slice, 0), (0, 0).into());
        assert_eq!(coords_at_pos(slice, 2), (0, 1).into());
        assert_eq!(coords_at_pos(slice, 3), (0, 2).into());
        assert_eq!(coords_at_pos(slice, 5), (0, 3).into());
        assert_eq!(coords_at_pos(slice, 6), (1, 0).into());

        // Test with tabs.
        let text = Rope::from("\tHello\n");
        let slice = text.slice(..);
        assert_eq!(coords_at_pos(slice, 0), (0, 0).into());
        assert_eq!(coords_at_pos(slice, 1), (0, 1).into());
        assert_eq!(coords_at_pos(slice, 2), (0, 2).into());
    }

    #[test]
    #[allow(deprecated)]
    fn test_visual_coords_at_pos() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ");
        let slice = text.slice(..);
        assert_eq!(visual_coords_at_pos(slice, 0, 8), (0, 0).into());
        assert_eq!(visual_coords_at_pos(slice, 5, 8), (0, 5).into()); // position on \n
        assert_eq!(visual_coords_at_pos(slice, 6, 8), (1, 0).into()); // position on w
        assert_eq!(visual_coords_at_pos(slice, 7, 8), (1, 1).into()); // position on o
        assert_eq!(visual_coords_at_pos(slice, 10, 8), (1, 4).into()); // position on d

        // Test with wide characters.
        let text = Rope::from("今日はいい\n");
        let slice = text.slice(..);
        assert_eq!(visual_coords_at_pos(slice, 0, 8), (0, 0).into());
        assert_eq!(visual_coords_at_pos(slice, 1, 8), (0, 2).into());
        assert_eq!(visual_coords_at_pos(slice, 2, 8), (0, 4).into());
        assert_eq!(visual_coords_at_pos(slice, 3, 8), (0, 6).into());
        assert_eq!(visual_coords_at_pos(slice, 4, 8), (0, 8).into());
        assert_eq!(visual_coords_at_pos(slice, 5, 8), (0, 10).into());
        assert_eq!(visual_coords_at_pos(slice, 6, 8), (1, 0).into());

        // Test with grapheme clusters.
        let text = Rope::from("a̐éö̲\r\n");
        let slice = text.slice(..);
        assert_eq!(visual_coords_at_pos(slice, 0, 8), (0, 0).into());
        assert_eq!(visual_coords_at_pos(slice, 2, 8), (0, 1).into());
        assert_eq!(visual_coords_at_pos(slice, 4, 8), (0, 2).into());
        assert_eq!(visual_coords_at_pos(slice, 7, 8), (0, 3).into());
        assert_eq!(visual_coords_at_pos(slice, 9, 8), (1, 0).into());

        // Test with wide-character grapheme clusters.
        // TODO: account for cluster.
        let text = Rope::from("किमपि\n");
        let slice = text.slice(..);
        assert_eq!(visual_coords_at_pos(slice, 0, 8), (0, 0).into());
        assert_eq!(visual_coords_at_pos(slice, 2, 8), (0, 2).into());
        assert_eq!(visual_coords_at_pos(slice, 3, 8), (0, 3).into());
        assert_eq!(visual_coords_at_pos(slice, 5, 8), (0, 5).into());
        assert_eq!(visual_coords_at_pos(slice, 6, 8), (1, 0).into());

        // Test with tabs.
        let text = Rope::from("\tHello\n");
        let slice = text.slice(..);
        assert_eq!(visual_coords_at_pos(slice, 0, 8), (0, 0).into());
        assert_eq!(visual_coords_at_pos(slice, 1, 8), (0, 8).into());
        assert_eq!(visual_coords_at_pos(slice, 2, 8), (0, 9).into());
    }

    #[test]
    fn test_visual_off_from_block() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ");
        let slice = text.slice(..);
        let annot = TextAnnotations::default();
        let text_fmt = TextFormat::default();
        assert_eq!(
            visual_offset_from_block(slice, 0, 0, &text_fmt, &annot).0,
            (0, 0).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 5, &text_fmt, &annot).0,
            (0, 5).into()
        ); // position on \n
        assert_eq!(
            visual_offset_from_block(slice, 0, 6, &text_fmt, &annot).0,
            (1, 0).into()
        ); // position on w
        assert_eq!(
            visual_offset_from_block(slice, 0, 7, &text_fmt, &annot).0,
            (1, 1).into()
        ); // position on o
        assert_eq!(
            visual_offset_from_block(slice, 0, 10, &text_fmt, &annot).0,
            (1, 4).into()
        ); // position on d

        // Test with wide characters.
        let text = Rope::from("今日はいい\n");
        let slice = text.slice(..);
        assert_eq!(
            visual_offset_from_block(slice, 0, 0, &text_fmt, &annot).0,
            (0, 0).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 1, &text_fmt, &annot).0,
            (0, 2).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 2, &text_fmt, &annot).0,
            (0, 4).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 3, &text_fmt, &annot).0,
            (0, 6).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 4, &text_fmt, &annot).0,
            (0, 8).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 5, &text_fmt, &annot).0,
            (0, 10).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 6, &text_fmt, &annot).0,
            (1, 0).into()
        );

        // Test with grapheme clusters.
        let text = Rope::from("a̐éö̲\r\n");
        let slice = text.slice(..);
        assert_eq!(
            visual_offset_from_block(slice, 0, 0, &text_fmt, &annot).0,
            (0, 0).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 2, &text_fmt, &annot).0,
            (0, 1).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 4, &text_fmt, &annot).0,
            (0, 2).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 7, &text_fmt, &annot).0,
            (0, 3).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 9, &text_fmt, &annot).0,
            (1, 0).into()
        );

        // Test with wide-character grapheme clusters.
        // TODO: account for cluster.
        let text = Rope::from("किमपि\n");
        let slice = text.slice(..);
        assert_eq!(
            visual_offset_from_block(slice, 0, 0, &text_fmt, &annot).0,
            (0, 0).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 2, &text_fmt, &annot).0,
            (0, 2).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 3, &text_fmt, &annot).0,
            (0, 3).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 5, &text_fmt, &annot).0,
            (0, 5).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 6, &text_fmt, &annot).0,
            (1, 0).into()
        );

        // Test with tabs.
        let text = Rope::from("\tHello\n");
        let slice = text.slice(..);
        assert_eq!(
            visual_offset_from_block(slice, 0, 0, &text_fmt, &annot).0,
            (0, 0).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 1, &text_fmt, &annot).0,
            (0, 4).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 2, &text_fmt, &annot).0,
            (0, 5).into()
        );
    }
    #[test]
    fn test_pos_at_coords() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ");
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (0, 0).into(), false), 0);
        assert_eq!(pos_at_coords(slice, (0, 5).into(), false), 5); // position on \n
        assert_eq!(pos_at_coords(slice, (0, 6).into(), false), 6); // position after \n
        assert_eq!(pos_at_coords(slice, (0, 6).into(), true), 5); // position after \n
        assert_eq!(pos_at_coords(slice, (1, 0).into(), false), 6); // position on w
        assert_eq!(pos_at_coords(slice, (1, 1).into(), false), 7); // position on o
        assert_eq!(pos_at_coords(slice, (1, 4).into(), false), 10); // position on d

        // Test with wide characters.
        // TODO: account for character width.
        let text = Rope::from("今日はいい\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (0, 0).into(), false), 0);
        assert_eq!(pos_at_coords(slice, (0, 1).into(), false), 1);
        assert_eq!(pos_at_coords(slice, (0, 2).into(), false), 2);
        assert_eq!(pos_at_coords(slice, (0, 3).into(), false), 3);
        assert_eq!(pos_at_coords(slice, (0, 4).into(), false), 4);
        assert_eq!(pos_at_coords(slice, (0, 5).into(), false), 5);
        assert_eq!(pos_at_coords(slice, (0, 6).into(), false), 6);
        assert_eq!(pos_at_coords(slice, (0, 6).into(), true), 5);
        assert_eq!(pos_at_coords(slice, (1, 0).into(), false), 6);

        // Test with grapheme clusters.
        let text = Rope::from("a̐éö̲\r\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (0, 0).into(), false), 0);
        assert_eq!(pos_at_coords(slice, (0, 1).into(), false), 2);
        assert_eq!(pos_at_coords(slice, (0, 2).into(), false), 4);
        assert_eq!(pos_at_coords(slice, (0, 3).into(), false), 7); // \r\n is one char here
        assert_eq!(pos_at_coords(slice, (0, 4).into(), false), 9);
        assert_eq!(pos_at_coords(slice, (0, 4).into(), true), 7);
        assert_eq!(pos_at_coords(slice, (1, 0).into(), false), 9);

        // Test with wide-character grapheme clusters.
        // TODO: account for character width.
        let text = Rope::from("किमपि");
        // 2 - 1 - 2 codepoints
        // TODO: delete handling as per https://news.ycombinator.com/item?id=20058454
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (0, 0).into(), false), 0);
        assert_eq!(pos_at_coords(slice, (0, 1).into(), false), 2);
        assert_eq!(pos_at_coords(slice, (0, 2).into(), false), 3);
        assert_eq!(pos_at_coords(slice, (0, 3).into(), false), 5);
        assert_eq!(pos_at_coords(slice, (0, 3).into(), true), 5);

        // Test with tabs.
        // Todo: account for tab stops.
        let text = Rope::from("\tHello\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (0, 0).into(), false), 0);
        assert_eq!(pos_at_coords(slice, (0, 1).into(), false), 1);
        assert_eq!(pos_at_coords(slice, (0, 2).into(), false), 2);

        // Test out of bounds.
        let text = Rope::new();
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (10, 0).into(), true), 0);
        assert_eq!(pos_at_coords(slice, (0, 10).into(), true), 0);
        assert_eq!(pos_at_coords(slice, (10, 10).into(), true), 0);
    }

    #[test]
    #[allow(deprecated)]
    fn test_pos_at_visual_coords() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ");
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (0, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 5).into(), 4), 5); // position on \n
        assert_eq!(pos_at_visual_coords(slice, (0, 6).into(), 4), 5); // position after \n
        assert_eq!(pos_at_visual_coords(slice, (1, 0).into(), 4), 6); // position on w
        assert_eq!(pos_at_visual_coords(slice, (1, 1).into(), 4), 7); // position on o
        assert_eq!(pos_at_visual_coords(slice, (1, 4).into(), 4), 10); // position on d

        // Test with wide characters.
        let text = Rope::from("今日はいい\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (0, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 1).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 2).into(), 4), 1);
        assert_eq!(pos_at_visual_coords(slice, (0, 3).into(), 4), 1);
        assert_eq!(pos_at_visual_coords(slice, (0, 4).into(), 4), 2);
        assert_eq!(pos_at_visual_coords(slice, (0, 5).into(), 4), 2);
        assert_eq!(pos_at_visual_coords(slice, (0, 6).into(), 4), 3);
        assert_eq!(pos_at_visual_coords(slice, (0, 7).into(), 4), 3);
        assert_eq!(pos_at_visual_coords(slice, (0, 8).into(), 4), 4);
        assert_eq!(pos_at_visual_coords(slice, (0, 9).into(), 4), 4);
        // assert_eq!(pos_at_visual_coords(slice, (0, 10).into(), 4, false), 5);
        // assert_eq!(pos_at_visual_coords(slice, (0, 10).into(), 4, true), 5);
        assert_eq!(pos_at_visual_coords(slice, (1, 0).into(), 4), 6);

        // Test with grapheme clusters.
        let text = Rope::from("a̐éö̲\r\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (0, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 1).into(), 4), 2);
        assert_eq!(pos_at_visual_coords(slice, (0, 2).into(), 4), 4);
        assert_eq!(pos_at_visual_coords(slice, (0, 3).into(), 4), 7); // \r\n is one char here
        assert_eq!(pos_at_visual_coords(slice, (0, 4).into(), 4), 7);
        assert_eq!(pos_at_visual_coords(slice, (1, 0).into(), 4), 9);

        // Test with wide-character grapheme clusters.
        let text = Rope::from("किमपि");
        // 2 - 1 - 2 codepoints
        // TODO: delete handling as per https://news.ycombinator.com/item?id=20058454
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (0, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 1).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 2).into(), 4), 2);
        assert_eq!(pos_at_visual_coords(slice, (0, 3).into(), 4), 3);

        // Test with tabs.
        let text = Rope::from("\tHello\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (0, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 1).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 2).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 3).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 4).into(), 4), 1);
        assert_eq!(pos_at_visual_coords(slice, (0, 5).into(), 4), 2);

        // Test out of bounds.
        let text = Rope::new();
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (10, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 10).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (10, 10).into(), 4), 0);
    }

    #[test]
    fn test_char_idx_at_visual_row_offset_inline_annotation() {
        let text = Rope::from("foo\nbar");
        let slice = text.slice(..);
        let mut text_fmt = TextFormat::default();
        let annotations = [InlineAnnotation::new(3, "x".repeat(100))];
        text_fmt.soft_wrap = true;

        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                0,
                1,
                0,
                &text_fmt,
                TextAnnotations::default().add_inline_annotations(&annotations, None)
            ),
            (2, 1)
        );
    }

    #[test]
    fn test_char_idx_at_visual_row_offset() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ\nfoo");
        let slice = text.slice(..);
        let mut text_fmt = TextFormat::default();
        for i in 0isize..3isize {
            for j in -2isize..=2isize {
                if !(0..3).contains(&(i + j)) {
                    continue;
                }
                println!("{i} {j}");
                assert_eq!(
                    char_idx_at_visual_offset(
                        slice,
                        slice.line_to_char(i as usize),
                        j,
                        3,
                        &text_fmt,
                        &TextAnnotations::default(),
                    )
                    .0,
                    slice.line_to_char((i + j) as usize) + 3
                );
            }
        }

        text_fmt.soft_wrap = true;
        let mut softwrapped_text = "foo ".repeat(10);
        softwrapped_text.push('\n');
        let last_char = softwrapped_text.len() - 1;

        let text = Rope::from(softwrapped_text.repeat(3));
        let slice = text.slice(..);
        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                last_char,
                0,
                0,
                &text_fmt,
                &TextAnnotations::default(),
            )
            .0,
            32
        );
        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                last_char,
                -1,
                0,
                &text_fmt,
                &TextAnnotations::default(),
            )
            .0,
            16
        );
        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                last_char,
                -2,
                0,
                &text_fmt,
                &TextAnnotations::default(),
            )
            .0,
            0
        );
        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                softwrapped_text.len() + last_char,
                -2,
                0,
                &text_fmt,
                &TextAnnotations::default(),
            )
            .0,
            softwrapped_text.len()
        );

        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                softwrapped_text.len() + last_char,
                -5,
                0,
                &text_fmt,
                &TextAnnotations::default(),
            )
            .0,
            0
        );
    }
}
