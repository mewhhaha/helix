use std::ops::Range;
use std::time::Instant;

use helix_stdx::rope::RopeSliceExt;
use imara_diff::{Algorithm, Diff, Hunk, IndentHeuristic, IndentLevel, InternedInput};
use ropey::RopeSlice;

use crate::{ChangeSet, Rope, Transaction};

struct ChangeSetBuilder<'a> {
    res: ChangeSet,
    before: RopeSlice<'a>,
    after: RopeSlice<'a>,
    file: &'a InternedInput<RopeSlice<'a>>,
    current_hunk: InternedInput<char>,
    char_diff: Diff,
    pos: u32,
}

impl ChangeSetBuilder<'_> {
    fn process_hunk(&mut self, before: Range<u32>, after: Range<u32>) {
        let len = self.file.before[self.pos as usize..before.start as usize]
            .iter()
            .map(|&it| self.file.interner[it].len_chars())
            .sum();
        self.res.retain(len);
        self.pos = before.end;

        // Line counts cannot bound character diffs: a single minified line
        // can contain megabytes. Keep equal edges out of both the interner and
        // Myers' working set, then bound the remaining character input.
        let before = self
            .before
            .slice(line_start(self.before, before.start)..line_start(self.before, before.end));
        let after = self
            .after
            .slice(line_start(self.after, after.start)..line_start(self.after, after.end));
        let prefix_bytes = common_edge_bytes(before.chunks(), after.chunks(), false);
        let prefix = before.byte_to_char(before.floor_char_boundary(prefix_bytes));
        self.res.retain(prefix);
        let before = before.slice(prefix..);
        let after = after.slice(prefix..);
        let mut before_chunks = before.chunks_at_byte(before.len_bytes()).0;
        let mut after_chunks = after.chunks_at_byte(after.len_bytes()).0;
        before_chunks.reverse();
        after_chunks.reverse();
        let suffix_bytes = common_edge_bytes(before_chunks, after_chunks, true);
        let suffix = before.len_chars()
            - before.byte_to_char(before.ceil_char_boundary(before.len_bytes() - suffix_bytes));
        let before = before.slice(..before.len_chars() - suffix);
        let after = after.slice(..after.len_chars() - suffix);

        if before.len_chars() == 0
            || after.len_chars() == 0
            || before.len_chars().saturating_add(after.len_chars()) > MAX_CHAR_DIFF_CHARS
        {
            self.res.delete(before.len_chars());
            self.res.insert(after.chunks().collect());
        } else {
            self.current_hunk.update_before(before.chars());
            self.current_hunk.update_after(after.chars());
            // Repeated character tokens suit Myers better than Histogram.
            self.char_diff.compute_with(
                Algorithm::Myers,
                &self.current_hunk.before,
                &self.current_hunk.after,
                self.current_hunk.interner.num_tokens(),
            );
            let mut pos = 0;
            for Hunk { before, after } in self.char_diff.hunks() {
                self.res.retain((before.start - pos) as usize);
                self.res.delete(before.len());
                pos = before.end;
                let fragment = self.current_hunk.after[after.start as usize..after.end as usize]
                    .iter()
                    .map(|&token| self.current_hunk.interner[token])
                    .collect();
                self.res.insert(fragment);
            }
            self.res
                .retain(self.current_hunk.before.len() - pos as usize);
            self.current_hunk.clear();
        }
        self.res.retain(suffix);
    }

    fn finish(mut self) -> ChangeSet {
        let len = self.file.before[self.pos as usize..]
            .iter()
            .map(|&it| self.file.interner[it].len_chars())
            .sum();

        self.res.retain(len);
        self.res
    }
}

// This bounds character token storage and the worst-case Myers working set.
const MAX_CHAR_DIFF_CHARS: usize = 16 * 1024;

fn line_start(text: RopeSlice, line: u32) -> usize {
    if line as usize == text.len_lines() {
        text.len_chars()
    } else {
        text.line_to_char(line as usize)
    }
}

fn common_edge_bytes<'a>(
    mut left: impl Iterator<Item = &'a str>,
    mut right: impl Iterator<Item = &'a str>,
    reverse: bool,
) -> usize {
    let mut a = &[][..];
    let mut b = &[][..];
    let mut equal = 0;
    loop {
        if a.is_empty() {
            let Some(chunk) = left.next() else {
                return equal;
            };
            a = chunk.as_bytes();
        }
        if b.is_empty() {
            let Some(chunk) = right.next() else {
                return equal;
            };
            b = chunk.as_bytes();
        }
        let len = a.len().min(b.len());
        let (a_edge, b_edge) = if reverse {
            (&a[a.len() - len..], &b[b.len() - len..])
        } else {
            (&a[..len], &b[..len])
        };
        if a_edge != b_edge {
            let matched = if reverse {
                a_edge
                    .iter()
                    .rev()
                    .zip(b_edge.iter().rev())
                    .take_while(|(a, b)| a == b)
                    .count()
            } else {
                a_edge
                    .iter()
                    .zip(b_edge.iter())
                    .take_while(|(a, b)| a == b)
                    .count()
            };
            return equal + matched;
        }
        equal += len;
        if reverse {
            a = &a[..a.len() - len];
            b = &b[..b.len() - len];
        } else {
            a = &a[len..];
            b = &b[len..];
        }
    }
}

struct RopeLines<'a>(RopeSlice<'a>);

impl<'a> imara_diff::TokenSource for RopeLines<'a> {
    type Token = RopeSlice<'a>;
    type Tokenizer = ropey::iter::Lines<'a>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.lines()
    }

    fn estimate_tokens(&self) -> u32 {
        // we can provide a perfect estimate which is very nice for performance
        self.0.len_lines() as u32
    }
}

/// Compares `old` and `new` to generate a [`Transaction`] describing
/// the steps required to get from `old` to `new`.
pub fn compare_ropes(before: &Rope, after: &Rope) -> Transaction {
    let start = Instant::now();
    let res = ChangeSet::with_capacity(32);
    let after = after.slice(..);
    let file = InternedInput::new(RopeLines(before.slice(..)), RopeLines(after));
    let mut builder = ChangeSetBuilder {
        res,
        file: &file,
        before: before.slice(..),
        after,
        pos: 0,
        current_hunk: InternedInput::default(),
        char_diff: Diff::default(),
    };
    let mut diff = Diff::compute(Algorithm::Histogram, &file);
    diff.postprocess_with_heuristic(
        &file,
        IndentHeuristic::new(|token| IndentLevel::for_ascii_line(file.interner[token].bytes(), 4)),
    );
    for hunk in diff.hunks() {
        builder.process_hunk(hunk.before, hunk.after)
    }
    let res = builder.finish().into();

    log::debug!(
        "rope diff took {}s",
        Instant::now().duration_since(start).as_secs_f64()
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_identity(a: &str, b: &str) {
        let mut old = Rope::from(a);
        let new = Rope::from(b);
        compare_ropes(&old, &new).apply(&mut old);
        assert_eq!(old, new);
    }

    #[test]
    fn large_unicode_line_keeps_equal_edges_outside_the_replacement() {
        let prefix = "界e\u{301}".repeat(20_000);
        let suffix = "😀\r\n".repeat(1000);
        let old = Rope::from(format!("{prefix}à{suffix}"));
        let new = Rope::from(format!("{prefix}á{suffix}"));
        let transaction = compare_ropes(&old, &new);
        let pos = prefix.chars().count();
        assert_eq!(
            transaction.changes_iter().collect::<Vec<_>>(),
            [(pos, pos + 1, Some("á".into()))]
        );
        let mut actual = old;
        assert!(transaction.apply(&mut actual));
        assert_eq!(actual, new);
    }

    #[test]
    fn bounded_character_diff_preserves_prefix_suffix_and_exact_output() {
        let old = Rope::from(format!("prefix{}suffix", "x".repeat(MAX_CHAR_DIFF_CHARS)));
        let new = Rope::from(format!("prefix{}suffix", "界".repeat(MAX_CHAR_DIFF_CHARS)));
        let transaction = compare_ropes(&old, &new);
        let changes: Vec<_> = transaction.changes_iter().collect();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, 6);
        assert_eq!(changes[0].1, 6 + MAX_CHAR_DIFF_CHARS);
        let mut actual = old.clone();
        assert!(transaction.apply(&mut actual));
        assert_eq!(actual, new);
        assert!(transaction.invert(&old).apply(&mut actual));
        assert_eq!(actual, old);
    }

    #[test]
    fn trimming_handles_partial_utf8_edges_crlf_and_separated_changes() {
        for (before, after) in [
            ("à😀\r\nold\r\nend", "á😁\r\nnew\r\nend"),
            ("\u{400}tail", "\u{500}tail"),
            ("one\r\ntwo\r\nthree", "ONE\r\ntwo\nTHREE"),
            ("e\u{301}", "e"),
        ] {
            test_identity(before, after);
        }
    }

    quickcheck::quickcheck! {
        fn test_compare_ropes(a: String, b: String) -> bool {
            let mut old = Rope::from(a);
            let new = Rope::from(b);
            compare_ropes(&old, &new).apply(&mut old);
            old == new
        }
    }

    #[test]
    fn equal_files() {
        test_identity("foo", "foo");
    }

    #[test]
    fn trailing_newline() {
        test_identity("foo\n", "foo");
        test_identity("foo", "foo\n");
    }

    #[test]
    fn new_file() {
        test_identity("", "foo");
    }

    #[test]
    fn deleted_file() {
        test_identity("foo", "");
    }
}
