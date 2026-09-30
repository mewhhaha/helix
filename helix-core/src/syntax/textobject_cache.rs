use std::{collections::VecDeque, ops::Range, sync::Arc};

use super::Layer;

pub(super) const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 4;

#[derive(Debug)]
pub(super) struct CaptureRange {
    pub range: Range<usize>,
    pub order: usize,
}

#[derive(Debug)]
pub(super) struct CaptureRanges {
    ranges: Vec<CaptureRange>,
    prefix_max_end: Vec<usize>,
    bytes: usize,
}

impl CaptureRanges {
    pub fn new(mut ranges: Vec<CaptureRange>) -> Self {
        ranges.sort_unstable_by_key(|capture| (capture.range.start, capture.order));
        let mut max_end = 0;
        let prefix_max_end: Vec<_> = ranges
            .iter()
            .map(|capture| {
                max_end = max_end.max(capture.range.end);
                max_end
            })
            .collect();
        let bytes = ranges.capacity() * std::mem::size_of::<CaptureRange>()
            + prefix_max_end.capacity() * std::mem::size_of::<usize>();
        Self {
            ranges,
            prefix_max_end,
            bytes,
        }
    }

    pub fn containing(&self, pos: usize) -> Option<Range<usize>> {
        let mut index = self
            .ranges
            .partition_point(|capture| capture.range.start <= pos);
        let mut best: Option<&CaptureRange> = None;
        while index > 0 {
            index -= 1;
            if self.prefix_max_end[index] <= pos {
                break;
            }
            let capture = &self.ranges[index];
            if capture.range.contains(&pos)
                && best.is_none_or(|best| {
                    (capture.range.len(), capture.order) < (best.range.len(), best.order)
                })
            {
                best = Some(capture);
            }
        }
        best.map(|capture| capture.range.clone())
    }
}

#[derive(Debug)]
struct Entry {
    layer: Layer,
    query: u64,
    capture: String,
    ranges: Arc<CaptureRanges>,
}

#[derive(Debug, Default)]
pub(super) struct TextObjectCache(VecDeque<Entry>);

impl TextObjectCache {
    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn get(&mut self, layer: Layer, query: u64, capture: &str) -> Option<Arc<CaptureRanges>> {
        let index = self.0.iter().position(|entry| {
            entry.layer == layer && entry.query == query && entry.capture == capture
        })?;
        let entry = self.0.remove(index).unwrap();
        let ranges = entry.ranges.clone();
        self.0.push_back(entry);
        Some(ranges)
    }

    pub fn insert(&mut self, layer: Layer, query: u64, capture: &str, ranges: CaptureRanges) {
        let capture = String::from(capture);
        if ranges.bytes.saturating_add(capture.capacity()) > MAX_BYTES {
            return;
        }
        self.0.retain(|entry| {
            !(entry.layer == layer && entry.query == query && entry.capture == capture)
        });
        while self.0.len() >= MAX_ENTRIES
            || self
                .0
                .iter()
                .map(|entry| entry.ranges.bytes + entry.capture.capacity())
                .sum::<usize>()
                + ranges.bytes
                + capture.capacity()
                > MAX_BYTES
        {
            self.0.pop_front();
        }
        self.0.push_back(Entry {
            layer,
            query,
            capture,
            ranges: Arc::new(ranges),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_lookup_matches_capture_order_for_nested_and_overlapping_ranges() {
        // Grouped captures use their whole start-to-end span, including gaps.
        let captures = [0..200, 20..80, 30..40, 25..35, 30..40, 90..110, 110..110];
        let index = CaptureRanges::new(
            captures
                .iter()
                .cloned()
                .enumerate()
                .map(|(order, range)| CaptureRange { range, order })
                .collect(),
        );
        for pos in 0..=220 {
            let expected = captures
                .iter()
                .filter(|range| range.contains(&pos))
                .min_by_key(|range| range.len())
                .cloned();
            assert_eq!(index.containing(pos), expected, "position {pos}");
        }
    }

    #[test]
    fn interval_lookup_seeks_past_unrelated_captures() {
        let ranges = CaptureRanges::new(
            (0..10_000)
                .map(|order| CaptureRange {
                    range: order * 10..order * 10 + 5,
                    order,
                })
                .collect(),
        );
        assert_eq!(ranges.containing(99_993), Some(99_990..99_995));
        assert_eq!(ranges.containing(99_995), None);
        assert_eq!(ranges.containing(100_000), None);
        assert!(ranges.bytes <= MAX_BYTES);
    }
}
