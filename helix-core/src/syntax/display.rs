//! Bounded syntax-highlight boundaries for repeated viewport rendering.
use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
    sync::Arc,
};

use super::{Highlight, Highlighter, Loader, Syntax};
use crate::RopeSlice;

const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 4;
const MIN_RANGE_BYTES: u32 = 8192;
const MAX_RANGE_BYTES: u32 = 4 * 1024 * 1024;

#[derive(Debug)]
struct Boundaries {
    events: Vec<(u32, u32)>,
    stacks: Vec<Box<[Highlight]>>,
    bytes: usize,
    // The first boundary that has not been consumed. MAX denotes a complete
    // traversal; every smaller value is an exclusive coverage limit.
    covered_until: u32,
}

#[derive(Debug)]
struct Entry {
    range: Range<u32>,
    scopes: Arc<Vec<String>>,
    boundaries: Arc<Boundaries>,
}

#[derive(Debug, Default)]
pub(super) struct DisplayCache(VecDeque<Entry>);

impl DisplayCache {
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    fn get(&mut self, range: &Range<u32>, scopes: &Arc<Vec<String>>) -> Option<Arc<Boundaries>> {
        let index = self
            .0
            .iter()
            .position(|entry| entry.range == *range && Arc::ptr_eq(&entry.scopes, scopes))?;
        let entry = self.0.remove(index).unwrap();
        let result = entry.boundaries.clone();
        self.0.push_back(entry);
        Some(result)
    }

    fn insert(&mut self, entry: Entry) {
        if let Some(index) = self.0.iter().position(|existing| {
            existing.range == entry.range && Arc::ptr_eq(&existing.scopes, &entry.scopes)
        }) {
            if self.0[index].boundaries.covered_until >= entry.boundaries.covered_until {
                return;
            }
            self.0.remove(index);
        }
        while self.0.len() >= MAX_ENTRIES
            || self
                .0
                .iter()
                .map(|entry| entry.boundaries.bytes)
                .sum::<usize>()
                + entry.boundaries.bytes
                > MAX_CACHE_BYTES
        {
            if self.0.pop_front().is_none() {
                break;
            }
        }
        self.0.push_back(entry);
    }
}

#[derive(Default)]
struct Recorder {
    events: Vec<(u32, u32)>,
    stacks: Vec<Box<[Highlight]>>,
    interned: HashMap<Vec<Highlight>, u32>,
    highlight_bytes: usize,
}

impl Recorder {
    fn record(&mut self, offset: u32, active: &[Highlight]) -> bool {
        let index = if let Some(&index) = self.interned.get(active) {
            index
        } else {
            let index = self.stacks.len() as u32;
            self.highlight_bytes += std::mem::size_of_val(active);
            self.stacks.push(active.into());
            self.interned.insert(active.to_vec(), index);
            index
        };
        match self.events.last_mut() {
            Some(last) if last.0 == offset => last.1 = index,
            Some(last) if last.1 == index => {}
            _ => self.events.push((offset, index)),
        }
        self.retained_bytes() <= MAX_CACHE_BYTES
    }

    fn retained_bytes(&self) -> usize {
        self.events.capacity() * std::mem::size_of::<(u32, u32)>()
            + self.stacks.capacity() * std::mem::size_of::<Box<[Highlight]>>()
            + self.highlight_bytes
    }

    fn finish(self, covered_until: u32) -> Boundaries {
        let bytes = self.retained_bytes();
        Boundaries {
            events: self.events,
            stacks: self.stacks,
            bytes,
            covered_until,
        }
    }
}

enum Source<'a> {
    Cached {
        boundaries: Arc<Boundaries>,
        next: usize,
    },
    Streaming {
        highlighter: Highlighter<'a>,
        recorder: Option<Box<Recorder>>,
    },
}

/// Syntax highlights that can seek across invisible text in a warm viewport.
/// Boundaries are recorded during normal traversal, without scanning ahead.
/// Large or short ranges retain the ordinary streaming behavior.
pub struct DisplayHighlighter<'a> {
    syntax: &'a Syntax,
    text: RopeSlice<'a>,
    loader: &'a Loader,
    range: Range<u32>,
    scopes: Option<Arc<Vec<String>>>,
    source: Source<'a>,
    active: Vec<Highlight>,
}

impl<'a> DisplayHighlighter<'a> {
    pub(super) fn new(
        syntax: &'a Syntax,
        source: RopeSlice<'a>,
        loader: &'a Loader,
        range: Range<u32>,
    ) -> Self {
        let length = range.end.saturating_sub(range.start);
        let scopes = (MIN_RANGE_BYTES..=MAX_RANGE_BYTES)
            .contains(&length)
            .then(|| Arc::clone(&loader.scopes()));
        let cached = scopes
            .as_ref()
            .and_then(|scopes| syntax.display_cache.lock().get(&range, scopes));
        let highlighter = match cached {
            Some(boundaries) => Source::Cached {
                boundaries,
                next: 0,
            },
            None => Source::Streaming {
                highlighter: syntax.highlighter(source, loader, range.clone()),
                recorder: scopes.as_ref().map(|_| Box::default()),
            },
        };
        Self {
            syntax,
            text: source,
            loader,
            range,
            scopes,
            source: highlighter,
            active: Vec::new(),
        }
    }

    pub fn next_event_offset(&self) -> u32 {
        match &self.source {
            Source::Cached { boundaries, next } => boundaries
                .events
                .get(*next)
                .map_or(boundaries.covered_until, |&(offset, _)| offset),
            Source::Streaming { highlighter, .. } => highlighter.next_event_offset(),
        }
    }

    /// Restore the complete active highlight stack at a monotonically increasing
    /// byte position, including overlapping captures and language injections.
    pub fn seek_to(&mut self, byte: u32) -> &[Highlight] {
        if matches!(&self.source, Source::Cached { boundaries, .. }
            if boundaries.covered_until != u32::MAX && byte >= boundaries.covered_until)
        {
            // Replaying from the original range preserves tree/query context.
            // Record only the prefix reached by this viewport, never its tail.
            self.source = Source::Streaming {
                highlighter: self
                    .syntax
                    .highlighter(self.text, self.loader, self.range.clone()),
                recorder: Some(Box::default()),
            };
        }
        match &mut self.source {
            Source::Cached { boundaries, next } => {
                *next = boundaries
                    .events
                    .partition_point(|&(offset, _)| offset <= byte);
                next.checked_sub(1).map_or(&[], |index| {
                    boundaries.stacks[boundaries.events[index].1 as usize].as_ref()
                })
            }
            Source::Streaming {
                highlighter,
                recorder,
            } => {
                while highlighter.next_event_offset() <= byte
                    && highlighter.next_event_offset() != u32::MAX
                {
                    let offset = highlighter.next_event_offset();
                    highlighter.advance();
                    if let Some(recording) = recorder {
                        self.active.clear();
                        self.active.extend(highlighter.active_highlights());
                        if !recording.record(offset, &self.active) {
                            *recorder = None;
                        }
                    }
                }
                self.active.clear();
                self.active.extend(highlighter.active_highlights());
                &self.active
            }
        }
    }
}

impl Drop for DisplayHighlighter<'_> {
    fn drop(&mut self) {
        if let Source::Streaming {
            highlighter,
            recorder,
        } = &mut self.source
        {
            if let (Some(recorder), Some(scopes)) = (recorder.take(), &self.scopes) {
                let boundaries = Arc::new((*recorder).finish(highlighter.next_event_offset()));
                self.syntax.display_cache.lock().insert(Entry {
                    range: self.range.clone(),
                    scopes: scopes.clone(),
                    boundaries,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(range: Range<u32>, scopes: &Arc<Vec<String>>, coverage: u32, bytes: usize) -> Entry {
        Entry {
            range,
            scopes: scopes.clone(),
            boundaries: Arc::new(Boundaries {
                events: Vec::new(),
                stacks: Vec::new(),
                bytes,
                covered_until: coverage,
            }),
        }
    }

    #[test]
    fn recorder_coalesces_boundaries_and_accounts_for_retained_capacity() {
        let outer = Highlight::new(1);
        let inner = Highlight::new(2);
        let mut recorder = Recorder::default();
        for (offset, stack) in [
            (0, vec![]),
            (10, vec![outer]),
            (10, vec![outer, inner]),
            (10, vec![outer]),
            (20, vec![outer]),
            (30, vec![outer, inner]),
            (40, vec![]),
        ] {
            assert!(recorder.record(offset, &stack));
        }
        let boundaries = recorder.finish(50);
        assert_eq!(boundaries.events, [(0, 0), (10, 1), (30, 2), (40, 0)]);
        assert_eq!(boundaries.stacks.len(), 3);
        assert_eq!(boundaries.covered_until, 50);
        for (byte, expected) in [
            (0, vec![]),
            (9, vec![]),
            (10, vec![outer]),
            (29, vec![outer]),
            (30, vec![outer, inner]),
            (39, vec![outer, inner]),
            (40, vec![]),
            (49, vec![]),
        ] {
            let next = boundaries
                .events
                .partition_point(|&(offset, _)| offset <= byte);
            let active = next.checked_sub(1).map_or(&[][..], |index| {
                boundaries.stacks[boundaries.events[index].1 as usize].as_ref()
            });
            assert_eq!(active, expected, "byte {byte}");
        }

        let mut recorder = Recorder {
            events: Vec::with_capacity(64),
            stacks: Vec::with_capacity(32),
            ..Recorder::default()
        };
        assert!(recorder.record(10, &[Highlight::new(1), Highlight::new(2)]));
        let expected = recorder.events.capacity() * std::mem::size_of::<(u32, u32)>()
            + recorder.stacks.capacity() * std::mem::size_of::<Box<[Highlight]>>()
            + 2 * std::mem::size_of::<Highlight>();
        assert_eq!(recorder.retained_bytes(), expected);
        assert_eq!(recorder.finish(u32::MAX).bytes, expected);

        let mut oversized = Recorder {
            events: Vec::with_capacity(MAX_CACHE_BYTES / std::mem::size_of::<(u32, u32)>()),
            ..Recorder::default()
        };
        assert!(!oversized.record(0, &[Highlight::new(1)]));
        assert!(oversized.retained_bytes() > MAX_CACHE_BYTES);
    }

    #[test]
    fn cache_only_replaces_a_prefix_with_greater_coverage() {
        let scopes = Arc::new(vec!["keyword".into()]);
        let range = 0..MIN_RANGE_BYTES;
        let mut cache = DisplayCache::default();
        let original = entry(range.clone(), &scopes, 100, 8);
        let original_boundaries = original.boundaries.clone();
        cache.insert(original);
        cache.insert(entry(range.clone(), &scopes, 50, 8));
        cache.insert(entry(range.clone(), &scopes, 100, 8));
        assert!(Arc::ptr_eq(
            &cache.get(&range, &scopes).unwrap(),
            &original_boundaries
        ));
        assert_eq!(cache.0.len(), 1);

        cache.insert(entry(range.clone(), &scopes, 200, 8));
        assert_eq!(cache.get(&range, &scopes).unwrap().covered_until, 200);
        let complete = entry(range.clone(), &scopes, u32::MAX, 8);
        let complete_boundaries = complete.boundaries.clone();
        cache.insert(complete);
        cache.insert(entry(range.clone(), &scopes, 300, 8));
        cache.insert(entry(range.clone(), &scopes, u32::MAX, 8));
        assert!(Arc::ptr_eq(
            &cache.get(&range, &scopes).unwrap(),
            &complete_boundaries
        ));
        assert_eq!(cache.0.len(), 1);

        // Reconfigured scopes must not reuse an earlier highlight stack even
        // when the new scope names and requested byte range compare equal.
        let new_scopes = Arc::new(scopes.as_ref().clone());
        assert!(cache.get(&range, &new_scopes).is_none());
        cache.insert(entry(range.clone(), &new_scopes, 10, 8));
        assert_eq!(cache.0.len(), 2);
        assert_eq!(cache.get(&range, &new_scopes).unwrap().covered_until, 10);
        assert_eq!(cache.get(&range, &scopes).unwrap().covered_until, u32::MAX);
    }

    #[test]
    fn cache_eviction_respects_lru_entry_and_byte_limits() {
        let scopes = Arc::new(Vec::new());
        let ranges: Vec<_> = (0..=MAX_ENTRIES as u32)
            .map(|start| start..start + MIN_RANGE_BYTES)
            .collect();
        let mut cache = DisplayCache::default();
        for range in &ranges[..MAX_ENTRIES] {
            cache.insert(entry(range.clone(), &scopes, 100, 8));
        }
        assert!(cache.get(&ranges[0], &scopes).is_some());
        cache.insert(entry(ranges[MAX_ENTRIES].clone(), &scopes, 100, 8));
        assert_eq!(cache.0.len(), MAX_ENTRIES);
        assert!(cache.get(&ranges[1], &scopes).is_none());
        for index in [0, 2, 3, MAX_ENTRIES] {
            assert!(cache.get(&ranges[index], &scopes).is_some());
        }
        cache.clear();
        assert!(cache.0.is_empty());

        // The fixtures specify retained sizes independently of the recorder;
        // its accounting is exercised above without allocating each cache.
        let half_budget = MAX_CACHE_BYTES / 2;
        cache.insert(entry(0..MIN_RANGE_BYTES, &scopes, 100, half_budget));
        cache.insert(entry(1..MIN_RANGE_BYTES + 1, &scopes, 100, half_budget));
        assert_eq!(cache.0.len(), 2);
        assert!(cache.get(&(0..MIN_RANGE_BYTES), &scopes).is_some());
        cache.insert(entry(2..MIN_RANGE_BYTES + 2, &scopes, 100, 1));
        assert_eq!(cache.0.len(), 2);
        assert!(cache.get(&(1..MIN_RANGE_BYTES + 1), &scopes).is_none());
        assert!(cache.get(&(0..MIN_RANGE_BYTES), &scopes).is_some());
        assert!(cache.get(&(2..MIN_RANGE_BYTES + 2), &scopes).is_some());
        assert!(
            cache
                .0
                .iter()
                .map(|entry| entry.boundaries.bytes)
                .sum::<usize>()
                <= MAX_CACHE_BYTES
        );
    }
}
