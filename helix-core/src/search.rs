use crate::movement::Direction;
use crate::RopeSlice;
use helix_stdx::rope::{Regex, RopeSliceExt};
use regex_cursor::regex_automata::Match;
use std::collections::VecDeque;

const MAX_REVERSE_MATCHES: usize = 4096;

/// Reuse a forward match scan for repeated reverse searches on one unchanged
/// text and regex. Clear the cache before changing either, or after searching a
/// suffix for wraparound: suffix searches can have a different match alignment.
pub struct ReverseSearchCache {
    matches: VecDeque<Match>,
    end: Option<usize>,
    capacity: usize,
}

impl ReverseSearchCache {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.clamp(2, MAX_REVERSE_MATCHES);
        Self {
            matches: VecDeque::with_capacity(capacity),
            end: None,
            capacity,
        }
    }

    pub fn clear(&mut self) {
        self.matches.clear();
        self.end = None;
    }

    /// Equivalent to `regex.find_iter(text.regex_input_at_bytes(..end)).last()`.
    pub fn find(&mut self, regex: &Regex, text: RopeSlice, end: usize) -> Option<Match> {
        if self.end == Some(end) {
            return self.matches.back().copied();
        }
        let seed = self
            .end
            .filter(|&previous_end| end < previous_end)
            .and_then(|_| self.matches.iter().rposition(|mat| mat.end() <= end));
        let (start, expected_seed) = if let Some(index) = seed {
            let mat = self.matches[index];
            self.matches.truncate(index);
            (mat.start(), Some(mat))
        } else {
            self.matches.clear();
            (0, None)
        };

        // Re-run from a preceding match rather than just popping a match list.
        // A shorter prefix can truncate a greedy match or introduce an empty
        // boundary match. Including the seed also restores find_iter's rule
        // that suppresses an empty match adjacent to the preceding match.
        // The input retains the complete rope as anchor/word-boundary context.
        let mut matches = regex.find_iter(text.regex_input_at_bytes(start..end));
        let first = matches.next();
        if expected_seed.is_some() && first != expected_seed {
            // Keep an exact fallback if replay cannot reproduce its seed.
            self.clear();
            return self.find(regex, text, end);
        }
        for mat in first.into_iter().chain(matches) {
            if self.matches.len() == self.capacity {
                self.matches.pop_front();
            }
            self.matches.push_back(mat);
        }
        self.end = Some(end);
        self.matches.back().copied()
    }
}

// TODO: switch to std::str::Pattern when it is stable.
pub trait CharMatcher {
    fn char_match(&self, ch: char) -> bool;
}

impl CharMatcher for char {
    fn char_match(&self, ch: char) -> bool {
        *self == ch
    }
}

impl<F: Fn(&char) -> bool> CharMatcher for F {
    fn char_match(&self, ch: char) -> bool {
        (*self)(&ch)
    }
}

// Finds the positions of the nth matching character in given direction
// starting from the pos gap-index (see Range struct for explanation)
pub fn find_nth_char<M: CharMatcher>(
    mut n: usize,
    text: RopeSlice,
    char_matcher: M,
    mut pos: usize,
    direction: Direction,
) -> Option<usize> {
    if n == 0 {
        return None;
    }

    let mut chars = text.get_chars_at(pos)?;

    match direction {
        Direction::Forward => loop {
            let c = chars.next()?;
            if char_matcher.char_match(c) {
                n -= 1;
                if n == 0 {
                    return Some(pos);
                }
            }
            pos += 1;
        },
        Direction::Backward => loop {
            let c = chars.prev()?;
            pos -= 1;
            if char_matcher.char_match(c) {
                n -= 1;
                if n == 0 {
                    return Some(pos);
                }
            }
        },
    };
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::movement::Direction;

    #[test]
    fn reverse_cache_matches_exact_prefix_searches_for_regex_edge_cases() {
        for text in [
            "aaaaaaaaa b aaa\nfoo foo\r\nαβ e\u{301} 🇵🇱\n",
            "baa baab aaaaaa",
            "abc\r\ndef\nabc\n",
        ] {
            let text = RopeSlice::from(text);
            for pattern in [
                "a{1,4}",
                "a+",
                "a*",
                "a*|b",
                "a+|",
                "",
                ".*",
                ".+?",
                "[a-z]+",
                "(?s).{0,3}",
                "(?m)^",
                "(?m)$",
                "\\A|\\z",
                "\\b",
                "\\b[a-z]+\\b",
                "(?m)^abc$",
                "(?:aa|a)",
                "(?:a|aa)",
                "α|e\\p{M}*|🇵🇱",
            ] {
                let regex = helix_stdx::rope::RegexBuilder::new()
                    .syntax(helix_stdx::rope::Config::new().crlf(true))
                    .build(pattern)
                    .unwrap();
                let mut cache = ReverseSearchCache::new(8);
                // Include arbitrary decreasing boundaries, unchanged boundaries
                // and increasing boundaries, rather than only literal matches.
                for pos in (0..=text.len_chars()).rev().chain(0..=text.len_chars()) {
                    let end = text.char_to_byte(pos);
                    let expected = regex.find_iter(text.regex_input_at_bytes(..end)).last();
                    assert_eq!(
                        cache.find(&regex, text, end),
                        expected,
                        "{pattern:?}, {pos}"
                    );
                    assert_eq!(
                        cache.find(&regex, text, end),
                        expected,
                        "repeated {pattern:?}, {pos}"
                    );
                }
            }
        }
    }

    #[test]
    fn reverse_cache_replays_truncated_greedy_and_empty_matches() {
        let text = RopeSlice::from("aa baaa baa");
        for pattern in ["a*", "a{1,1000}", "a*|b", "b|a*", "a+|$"] {
            let regex = Regex::new(pattern).unwrap();
            let mut cache = ReverseSearchCache::new(16);
            let mut end = text.len_bytes();
            for _ in 0..20 {
                let expected = regex.find_iter(text.regex_input_at_bytes(..end)).last();
                let actual = cache.find(&regex, text, end);
                assert_eq!(actual, expected, "{pattern:?}, {end}");
                if let Some(mat) = actual {
                    end = mat.start();
                }
            }
        }
    }

    #[test]
    fn reverse_cache_is_bounded_and_reset_after_suffix_search() {
        let text = crate::Rope::from_str(&"aa ".repeat(MAX_REVERSE_MATCHES + 10));
        let regex = Regex::new("aa").unwrap();
        let mut cache = ReverseSearchCache::new(usize::MAX);
        let mut end = text.len_bytes();
        for _ in 0..MAX_REVERSE_MATCHES + 20 {
            let expected = regex
                .find_iter(text.slice(..).regex_input_at_bytes(..end))
                .last();
            assert_eq!(cache.find(&regex, text.slice(..), end), expected);
            assert!(cache.matches.len() <= MAX_REVERSE_MATCHES);
            if let Some(mat) = expected {
                end = mat.start();
            }
        }
        // Searching from byte 1 changes the nonoverlapping alignment of "aa".
        let text = RopeSlice::from("aaaaa");
        cache.clear();
        assert_eq!(cache.find(&regex, text, 1), None);
        let wrapped = regex
            .find_iter(text.regex_input_at_bytes(1..))
            .last()
            .unwrap();
        assert_eq!(wrapped.start(), 3);
        cache.clear();
        assert_eq!(
            cache.find(&regex, text, wrapped.start()).unwrap().start(),
            0
        );
    }

    #[test]
    fn test_find_nth_char() {
        let text = RopeSlice::from("aa ⌚aa \r\n aa");

        // Forward direction
        assert_eq!(find_nth_char(1, text, 'a', 5, Direction::Forward), Some(5));
        assert_eq!(find_nth_char(2, text, 'a', 5, Direction::Forward), Some(10));
        assert_eq!(find_nth_char(3, text, 'a', 5, Direction::Forward), Some(11));
        assert_eq!(find_nth_char(4, text, 'a', 5, Direction::Forward), None);

        // Backward direction
        assert_eq!(find_nth_char(1, text, 'a', 5, Direction::Backward), Some(4));
        assert_eq!(find_nth_char(2, text, 'a', 5, Direction::Backward), Some(1));
        assert_eq!(find_nth_char(3, text, 'a', 5, Direction::Backward), Some(0));
        assert_eq!(find_nth_char(4, text, 'a', 5, Direction::Backward), None);

        // Edge cases
        assert_eq!(find_nth_char(0, text, 'a', 5, Direction::Forward), None); // n = 0
        assert_eq!(find_nth_char(1, text, 'x', 5, Direction::Forward), None); // Not found
        assert_eq!(find_nth_char(1, text, 'a', 20, Direction::Forward), None); // Beyond text
        assert_eq!(find_nth_char(1, text, 'a', 0, Direction::Backward), None); // At start going backward
    }
}
