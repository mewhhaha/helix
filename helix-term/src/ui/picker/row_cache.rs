use helix_stdx::cache::BoundedCache;

use tui::widgets::Cell;

struct Entry {
    item: usize,
    column: usize,
    source: Cell<'static>,
    painted: Cell<'static>,
    width: usize,
    generation: u64,
}

#[derive(Default, PartialEq, Eq)]
struct RowContext {
    version: usize,
    theme: usize,
    paths: bool,
}

pub(super) struct RowCache {
    context: RowContext,
    generation: u64,
    entries: BoundedCache<Entry>,
}

impl Default for RowCache {
    fn default() -> Self {
        Self {
            context: RowContext::default(),
            generation: 0,
            entries: BoundedCache::new(512, 2 * 1024 * 1024),
        }
    }
}

impl RowCache {
    pub(super) fn prepare(&mut self, version: usize, theme: usize, paths: bool, changed: bool) {
        let context = RowContext {
            version,
            theme,
            paths,
        };
        if self.context != context {
            self.entries.clear();
            self.context = context;
        }
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    pub(super) fn formatted(&self, item: usize, column: usize) -> Option<Cell<'static>> {
        self.entries
            .iter()
            .find(|entry| entry.item == item && entry.column == column)
            .map(|entry| entry.source.clone())
    }

    pub(super) fn get(
        &mut self,
        item: usize,
        column: usize,
        source: Option<&Cell<'_>>,
    ) -> Option<(Cell<'static>, usize)> {
        let entry = self.entries.get(|entry| {
            entry.item == item
                && entry.column == column
                && entry.generation == self.generation
                && source.is_none_or(|source| entry.source == *source)
        })?;
        Some((entry.painted.clone(), entry.width))
    }

    pub(super) fn insert(
        &mut self,
        item: usize,
        column: usize,
        source: Cell<'static>,
        painted: Cell<'static>,
        width: usize,
    ) {
        let cell_bytes = |cell: &Cell<'_>| {
            cell.content.lines.capacity() * std::mem::size_of::<tui::text::Spans>()
                + cell
                    .content
                    .lines
                    .iter()
                    .map(|line| {
                        line.0.capacity() * std::mem::size_of::<tui::text::Span>()
                            + line
                                .0
                                .iter()
                                .map(|span| match &span.content {
                                    std::borrow::Cow::Owned(text) => text.capacity(),
                                    std::borrow::Cow::Borrowed(_) => 0,
                                })
                                .sum::<usize>()
                    })
                    .sum::<usize>()
        };
        let bytes = cell_bytes(&source) + cell_bytes(&painted) + std::mem::size_of::<Entry>();
        self.entries
            .retain(|entry| entry.item != item || entry.column != column);
        self.entries.insert(
            Entry {
                item,
                column,
                source,
                painted,
                width,
                generation: self.generation,
            },
            bytes,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_patterns_keep_formatting_but_refresh_highlights() {
        let mut cache = RowCache::default();
        cache.prepare(1, 1, false, true);
        cache.insert(1, 0, "source".into(), "highlighted".into(), 6);
        assert_eq!(cache.get(1, 0, None).unwrap().1, 6);
        assert!(cache.get(1, 0, Some(&Cell::from("changed"))).is_none());
        cache.prepare(1, 1, false, true);
        assert!(cache.get(1, 0, None).is_none());
        assert_eq!(cache.formatted(1, 0).unwrap(), Cell::from("source"));
        cache.prepare(2, 1, false, false);
        assert!(cache.formatted(1, 0).is_none());
        cache.insert(1, 0, "source".into(), "highlighted".into(), 6);
        cache.prepare(2, 2, false, false);
        assert!(cache.formatted(1, 0).is_none());
    }
}
