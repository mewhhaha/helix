use std::ops::DerefMut;

use nucleo::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo::Config;
use parking_lot::Mutex;

pub struct LazyMutex<T> {
    inner: Mutex<Option<T>>,
    init: fn() -> T,
}

impl<T> LazyMutex<T> {
    pub const fn new(init: fn() -> T) -> Self {
        Self {
            inner: Mutex::new(None),
            init,
        }
    }

    pub fn lock(&self) -> impl DerefMut<Target = T> + '_ {
        parking_lot::MutexGuard::map(self.inner.lock(), |val| val.get_or_insert_with(self.init))
    }
}

pub static MATCHER: LazyMutex<nucleo::Matcher> = LazyMutex::new(nucleo::Matcher::default);

/// convenience function to easily fuzzy match
/// on a (relatively small list of inputs). This is not recommended for building a full tui
/// application that can match large numbers of matches as all matching is done on the current
/// thread, effectively blocking the UI
pub fn fuzzy_match<T: AsRef<str>>(
    pattern: &str,
    items: impl IntoIterator<Item = T>,
    path: bool,
) -> Vec<(T, u16)> {
    let mut matcher = MATCHER.lock();
    matcher.config = Config::DEFAULT;
    if path {
        matcher.config.set_match_paths();
    }
    let pattern = Atom::new(
        pattern,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );
    pattern.match_list(items, &mut matcher)
}

/// Match a background scan with its own matcher, stopping obsolete work without
/// holding the matcher used by interactive menus.
pub fn fuzzy_match_cancelable<T: AsRef<str>>(
    pattern: &str,
    items: impl IntoIterator<Item = T>,
    mut is_canceled: impl FnMut() -> bool,
) -> Option<Vec<(T, u16)>> {
    let mut matcher = nucleo::Matcher::new(Config::DEFAULT);
    let pattern = Atom::new(
        pattern,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );
    let mut buffer = Vec::new();
    let mut matches = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        if i % 64 == 0 && is_canceled() {
            return None;
        }
        if let Some(score) = pattern.score(
            nucleo::Utf32Str::new(item.as_ref(), &mut buffer),
            &mut matcher,
        ) {
            matches.push((item, score));
        }
    }
    (!is_canceled()).then_some(matches)
}

/// Score large immutable candidate sets in parallel. Each chunk owns its
/// matcher, and collection preserves input order, including equal scores.
pub fn fuzzy_match_parallel<'a, T: AsRef<str> + Sync>(
    pattern: &str,
    items: &'a [T],
    is_canceled: impl Fn() -> bool + Sync,
) -> Option<Vec<(&'a T, u16)>> {
    use rayon::prelude::*;

    const PARALLEL_THRESHOLD: usize = 32_768;
    const CHUNK: usize = 4096;
    if items.len() < PARALLEL_THRESHOLD || helix_stdx::cpu::worker_count() == 1 {
        return fuzzy_match_cancelable(pattern, items, is_canceled);
    }
    let chunks: Option<Vec<_>> = helix_stdx::cpu::pool().install(|| {
        items
            .par_chunks(CHUNK)
            .map(|chunk| fuzzy_match_cancelable(pattern, chunk, &is_canceled))
            .collect()
    });
    if is_canceled() {
        return None;
    }
    Some(chunks?.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn parallel_scores_and_order_match_the_serial_scan() {
        let words: Vec<_> = (0..40_000)
            .map(|i| {
                format!(
                    "parse_{}_buffer_{i}",
                    if i % 2 == 0 { "é" } else { "ascii" }
                )
            })
            .collect();
        for pattern in ["", "pbf", "Pb", "é", "xyz"] {
            assert_eq!(
                super::fuzzy_match_parallel(pattern, &words, || false),
                super::fuzzy_match_cancelable(pattern, &words, || false)
            );
        }
    }

    #[test]
    fn canceled_parallel_scans_discard_all_partial_results() {
        let checks = AtomicUsize::new(0);
        let words = vec!["word"; 100_000];
        assert!(super::fuzzy_match_parallel("w", &words, || {
            checks.fetch_add(1, Ordering::Relaxed) >= 10
        })
        .is_none());
    }
}
