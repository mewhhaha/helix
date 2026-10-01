use std::{collections::VecDeque, ops::Range, sync::Arc};

use super::{Highlight, Loader, OverlayHighlights, Syntax};
use crate::RopeSlice;
use helix_stdx::rope::RopeSliceExt as _;

const MAX_ENTRIES: usize = 4;
const MAX_BYTES: usize = 2 * 1024 * 1024;
type Highlights = Arc<Vec<(Highlight, Range<usize>)>>;

#[derive(Debug)]
struct Entry {
    range: Range<u32>,
    palette: usize,
    scopes: Arc<Vec<String>>,
    highlights: Highlights,
}

#[derive(Debug, Default)]
pub(super) struct RainbowCache(VecDeque<Entry>);

impl RainbowCache {
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    fn get(
        &mut self,
        range: &Range<u32>,
        palette: usize,
        scopes: &Arc<Vec<String>>,
    ) -> Option<Highlights> {
        let index = self.0.iter().position(|entry| {
            entry.range == *range && entry.palette == palette && Arc::ptr_eq(&entry.scopes, scopes)
        })?;
        let entry = self.0.remove(index).unwrap();
        let highlights = entry.highlights.clone();
        self.0.push_back(entry);
        Some(highlights)
    }

    fn insert(&mut self, entry: Entry) {
        let bytes = |entry: &Entry| {
            entry.highlights.capacity() * std::mem::size_of::<(Highlight, Range<usize>)>()
        };
        if bytes(&entry) > MAX_BYTES {
            return;
        }
        while self.0.len() >= MAX_ENTRIES
            || self.0.iter().map(bytes).sum::<usize>() + bytes(&entry) > MAX_BYTES
        {
            self.0.pop_front();
        }
        self.0.push_back(entry);
    }
}

impl Syntax {
    pub fn rainbow_highlights(
        &self,
        source: RopeSlice,
        palette: usize,
        loader: &Loader,
        range: Range<u32>,
    ) -> OverlayHighlights {
        let visible = source.byte_to_char(source.floor_char_boundary(range.start as usize))
            ..source.byte_to_char(source.ceil_char_boundary(range.end as usize));
        if palette == 0 || range.is_empty() {
            return OverlayHighlights::Heterogenous {
                highlights: Vec::new(),
            };
        }
        let scopes = Arc::clone(&loader.scopes());
        let cached = self.rainbow_cache.lock().get(&range, palette, &scopes);
        let highlights = cached.unwrap_or_else(|| {
            let mut highlights =
                self.rainbow_highlights_uncached(source, palette, loader, range.clone());
            highlights.retain(|(_, highlight)| {
                highlight.end > visible.start && highlight.start < visible.end
            });
            let highlights = Arc::new(highlights);
            self.rainbow_cache.lock().insert(Entry {
                range,
                palette,
                scopes,
                highlights: highlights.clone(),
            });
            highlights
        });
        OverlayHighlights::shared_heterogenous(highlights, visible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Rope, Transaction};

    fn loader() -> Loader {
        Loader::new(
            helix_loader::config::default_lang_config()
                .try_into()
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn bounded_queries_preserve_enclosing_scopes_and_injections() {
        let loader = loader();
        for (language, source) in [
            (
                "rust",
                format!(
                    "fn f() {{ if true {{\n{}}}\n}}",
                    "let x = (1, [2, (3)]);\n".repeat(100)
                ),
            ),
            (
                "html",
                "<div><script>function f(){ let x = [1, (2)]; }</script></div>".into(),
            ),
            ("python", "def f():\n    x = [(1, (2, [3]))]\n".into()),
        ] {
            let text = Rope::from_str(&source);
            let syntax = Syntax::new(
                text.slice(..),
                loader.language_for_name(language).unwrap(),
                &loader,
            )
            .unwrap();
            for start in (0..text.len_bytes()).step_by(17) {
                let end = (start + 44).min(text.len_bytes());
                let visible = start..end; // fixtures are ASCII
                let mut expected =
                    syntax.rainbow_highlights_uncached(text.slice(..), 6, &loader, 0..end as u32);
                expected
                    .retain(|(_, range)| range.end > visible.start && range.start < visible.end);
                for _ in 0..2 {
                    let OverlayHighlights::SharedHeterogenous {
                        highlights,
                        indices,
                    } = syntax.rainbow_highlights(
                        text.slice(..),
                        6,
                        &loader,
                        start as u32..end as u32,
                    )
                    else {
                        panic!()
                    };
                    assert_eq!(highlights[indices], expected, "{language} at {start}");
                }
            }
            assert!(syntax.rainbow_cache.lock().0.len() <= MAX_ENTRIES);
        }
    }

    #[test]
    fn edits_and_palette_changes_invalidate_rainbow_colors() {
        let loader = loader();
        let text = Rope::from_str("fn f() { let x = [1, (2)]; }");
        let mut syntax = Syntax::new(
            text.slice(..),
            loader.language_for_name("rust").unwrap(),
            &loader,
        )
        .unwrap();
        for palette in [1, 6] {
            syntax.rainbow_highlights(text.slice(..), palette, &loader, 0..text.len_bytes() as u32);
        }
        assert_eq!(syntax.rainbow_cache.lock().0.len(), 2);
        let transaction = Transaction::change(&text, [(9, 9, Some("{ ".into()))].into_iter());
        let mut edited = text.clone();
        transaction.apply(&mut edited);
        syntax
            .update(
                text.slice(..),
                edited.slice(..),
                transaction.changes(),
                &loader,
            )
            .unwrap();
        assert!(syntax.rainbow_cache.lock().0.is_empty());
        let OverlayHighlights::SharedHeterogenous { highlights, .. } =
            syntax.rainbow_highlights(edited.slice(..), 6, &loader, 0..edited.len_bytes() as u32)
        else {
            panic!()
        };
        assert_eq!(
            *highlights,
            syntax.rainbow_highlights_uncached(
                edited.slice(..),
                6,
                &loader,
                0..edited.len_bytes() as u32
            )
        );
    }
}
