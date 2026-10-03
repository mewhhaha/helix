//! Small LRU caches bounded by both entry count and retained payload bytes.

use std::collections::VecDeque;

#[derive(Debug)]
struct Entry<T> {
    value: T,
    bytes: usize,
}

/// Keys, payload measurements, and invalidation remain the caller's responsibility.
/// An entry's payload must not change size while it is stored in the cache.
#[derive(Debug)]
pub struct BoundedCache<T> {
    entries: VecDeque<Entry<T>>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl<T> BoundedCache<T> {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.entries.iter().map(|entry| &entry.value)
    }

    /// Find an entry and promote it to the most recently used position.
    pub fn get(&mut self, mut matches: impl FnMut(&T) -> bool) -> Option<&T> {
        let index = self
            .entries
            .iter()
            .position(|entry| matches(&entry.value))?;
        let entry = self.entries.remove(index)?;
        self.entries.push_back(entry);
        self.entries.back().map(|entry| &entry.value)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.entries.retain(|entry| {
            let retained = keep(&entry.value);
            if !retained {
                self.bytes -= entry.bytes;
            }
            retained
        });
    }

    pub fn admits(&self, bytes: usize) -> bool {
        self.max_entries != 0 && bytes <= self.max_bytes
    }

    /// Admit a payload, evicting the least recently used entries as necessary.
    /// Oversized entries leave existing entries intact.
    pub fn insert(&mut self, value: T, bytes: usize) -> bool {
        if !self.admits(bytes) {
            return false;
        }
        while self.entries.len() >= self.max_entries || bytes > self.max_bytes - self.bytes {
            let Some(oldest) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= oldest.bytes;
        }
        self.entries.push_back(Entry { value, bytes });
        self.bytes += bytes;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promotion_and_both_limits_choose_the_oldest_eligible_entry() {
        let mut cache = BoundedCache::new(3, 10);
        for (value, bytes) in [(1, 2), (2, 4), (3, 3)] {
            assert!(cache.insert(value, bytes));
        }
        assert_eq!(cache.get(|value| *value == 1), Some(&1));
        assert!(cache.insert(4, 2));
        assert_eq!(cache.iter().copied().collect::<Vec<_>>(), [3, 1, 4]);
        assert_eq!(cache.bytes(), 7);
        assert!(cache.insert(5, 1));
        assert_eq!(cache.iter().copied().collect::<Vec<_>>(), [1, 4, 5]);
        assert_eq!(cache.bytes(), 5);
    }

    #[test]
    fn rejected_entries_and_misses_preserve_the_working_set() {
        let mut cache = BoundedCache::new(2, 3);
        assert!(cache.insert(1, 2));
        assert!(cache.insert(2, 1));
        assert!(!cache.insert(3, 4));
        assert_eq!(cache.get(|value| *value == 9), None);
        assert_eq!(cache.iter().copied().collect::<Vec<_>>(), [1, 2]);
        assert_eq!(cache.bytes(), 3);
        assert!(!BoundedCache::new(0, usize::MAX).insert(1, 0));
    }

    #[test]
    fn invalidation_and_clear_release_budget_for_replacement_entries() {
        let mut cache = BoundedCache::new(4, 10);
        cache.insert(1, 6);
        cache.insert(2, 4);
        cache.retain(|value| *value != 1);
        assert!(cache.insert(3, 6));
        assert_eq!(cache.iter().copied().collect::<Vec<_>>(), [2, 3]);
        assert_eq!(cache.bytes(), 10);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
        assert!(cache.insert(4, 10));
    }
}
