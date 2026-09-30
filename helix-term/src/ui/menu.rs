use std::collections::BTreeMap;

use crate::{
    compositor::{Callback, Component, Compositor, Context, Event, EventResult},
    ctrl, key, shift,
};
use tui::{buffer::Buffer as Surface, widgets::Table};

pub use tui::widgets::{Cell, Row};

use helix_view::{editor::SmartTabConfig, graphics::Rect, Editor};
use tui::layout::Constraint;

pub trait Item: Sync + Send + 'static {
    /// Additional editor state that is used for label calculation.
    type Data: Sync + Send + 'static;

    fn format(&self, data: &Self::Data) -> Row<'_>;
}

pub type MenuCallback<T> = Box<dyn Fn(&mut Editor, Option<&T>, MenuEvent)>;

#[derive(Clone, Copy)]
struct RowMetrics {
    height: u16,
    total_height: u16,
}

pub struct Menu<T: Item> {
    options: Vec<T>,
    editor_data: T::Data,

    cursor: Option<usize>,

    /// (index, score)
    matches: Vec<(u32, u32)>,

    widths: Vec<Constraint>,
    row_metrics: Vec<RowMetrics>,
    option_widths: Vec<Vec<u16>>,
    column_widths: Vec<BTreeMap<u16, usize>>,
    metrics_dirty: bool,

    callback_fn: MenuCallback<T>,

    scroll: usize,
    size: (u16, u16),
    viewport: (u16, u16),
    recalculate: bool,
    auto_close: bool,
}

impl<T: Item> Menu<T> {
    const LEFT_PADDING: usize = 1;

    // TODO: it's like a slimmed down picker, share code? (picker = menu + prompt with different
    // rendering)
    pub fn new(
        options: Vec<T>,
        editor_data: <T as Item>::Data,
        callback_fn: impl Fn(&mut Editor, Option<&T>, MenuEvent) + 'static,
    ) -> Self {
        let matches = (0..options.len() as u32).map(|i| (i, 0)).collect();
        Self {
            options,
            editor_data,
            matches,
            cursor: None,
            widths: Vec::new(),
            row_metrics: Vec::new(),
            option_widths: Vec::new(),
            column_widths: Vec::new(),
            metrics_dirty: true,
            callback_fn: Box::new(callback_fn),
            scroll: 0,
            size: (0, 0),
            viewport: (0, 0),
            recalculate: true,
            auto_close: false,
        }
    }

    pub fn reset_cursor(&mut self) {
        self.cursor = None;
        self.scroll = 0;
        self.recalculate = true;
    }

    pub fn update_options(&mut self) -> (&mut Vec<(u32, u32)>, &mut Vec<T>) {
        self.metrics_dirty = true;
        self.recalculate = true;
        (&mut self.matches, &mut self.options)
    }

    /// Filtering changes row order and height, but cannot change candidate labels.
    pub fn update_matches(&mut self) -> (&mut Vec<(u32, u32)>, &[T]) {
        self.recalculate = true;
        (&mut self.matches, &self.options)
    }

    pub fn ensure_cursor_in_bounds(&mut self) {
        if self.matches.is_empty() {
            self.cursor = None;
            self.scroll = 0;
        } else {
            self.scroll = 0;
            self.recalculate = true;
            if let Some(cursor) = &mut self.cursor {
                *cursor = (*cursor).min(self.matches.len() - 1)
            }
        }
    }

    pub fn clear(&mut self) {
        self.matches.clear();
        self.recalculate = true;

        // reset cursor position
        self.cursor = None;
        self.scroll = 0;
    }

    pub fn move_up(&mut self) {
        let len = self.matches.len();
        let max_index = len.saturating_sub(1);
        let pos = self.cursor.map_or(max_index, |i| (i + max_index) % len) % len;
        self.cursor = Some(pos);
        self.adjust_scroll();
    }

    pub fn move_half_page_up(&mut self) {
        let len = self.matches.len();
        let max_index = len.saturating_sub((self.size.1 as usize / 2).max(1));
        let pos = self.cursor.map_or(max_index, |i| (i + max_index) % len) % len;
        self.cursor = Some(pos);
        self.adjust_scroll();
    }

    pub fn move_down(&mut self) {
        let len = self.matches.len();
        let pos = self.cursor.map_or(0, |i| i + 1) % len;
        self.cursor = Some(pos);
        self.adjust_scroll();
    }

    pub fn move_half_page_down(&mut self) {
        let len = self.matches.len();
        let pos = self
            .cursor
            .map_or(0, |i| i + (self.size.1 as usize / 2).max(1))
            % len;
        self.cursor = Some(pos);
        self.adjust_scroll();
    }

    pub fn auto_close(mut self, auto_close: bool) -> Self {
        self.auto_close = auto_close;
        self
    }

    fn refresh_metrics(&mut self) {
        if !self.metrics_dirty {
            return;
        }
        self.row_metrics.clear();
        self.option_widths.clear();
        self.column_widths.clear();
        for option in &self.options {
            let row = option.format(&self.editor_data);
            self.row_metrics.push(RowMetrics {
                height: row.row_height(),
                total_height: row.total_height(),
            });
            let widths: Vec<_> = row
                .cells
                .iter()
                .map(|cell| cell.content.width().min(u16::MAX as usize) as u16)
                .collect();
            self.column_widths
                .resize_with(widths.len().max(self.column_widths.len()), BTreeMap::new);
            for (column, &width) in self.column_widths.iter_mut().zip(&widths) {
                *column.entry(width).or_default() += 1;
            }
            self.option_widths.push(widths);
        }
        self.refresh_column_widths();
        self.metrics_dirty = false;
    }

    fn refresh_column_widths(&mut self) {
        while self.column_widths.last().is_some_and(BTreeMap::is_empty) {
            self.column_widths.pop();
        }
        self.widths = self
            .column_widths
            .iter()
            .map(|column| {
                Constraint::Length(column.last_key_value().map_or(0, |(&width, _)| width))
            })
            .collect();
    }

    fn recalculate_size(&mut self, viewport: (u16, u16)) {
        self.refresh_metrics();
        let n = self.widths.len();

        let height = self.matches.len().min(10).min(viewport.1 as usize);
        // do all the matches fit on a single screen?
        let fits = self.matches.len() <= height;

        let mut len = self
            .widths
            .iter()
            .map(|width| match width {
                Constraint::Length(width) => *width as usize,
                _ => unreachable!(),
            })
            .sum::<usize>()
            + n;

        if !fits {
            len += 1; // +1: reserve some space for scrollbar
        }

        len += Self::LEFT_PADDING;
        let width = len.min(viewport.0 as usize);

        self.size = (width as u16, height as u16);
        self.viewport = viewport;

        // adjust scroll offsets if size changed
        self.adjust_scroll();
        self.recalculate = false;
    }

    fn adjust_scroll(&mut self) {
        self.refresh_metrics();
        self.scroll = self.visible_rows(self.size.1).start;
    }

    fn visible_rows(&self, max_height: u16) -> std::ops::Range<usize> {
        if self.matches.is_empty() || max_height == 0 {
            return 0..0;
        }
        let metrics = |index: usize| self.row_metrics[self.matches[index].0 as usize];
        let mut start = self.scroll.min(self.matches.len() - 1);
        let mut end = start;
        let mut height = 0u16;
        while end < self.matches.len() {
            let row = metrics(end);
            if height.saturating_add(row.height) > max_height {
                break;
            }
            height = height.saturating_add(row.total_height);
            end += 1;
        }
        let selected = self.cursor.unwrap_or(0).min(self.matches.len() - 1);
        while selected >= end {
            height = height.saturating_add(metrics(end).total_height);
            end += 1;
            while height > max_height && start < end {
                height = height.saturating_sub(metrics(start).total_height);
                start += 1;
            }
        }
        while selected < start {
            start -= 1;
            height = height.saturating_add(metrics(start).total_height);
            while height > max_height && end > start {
                end -= 1;
                height = height.saturating_sub(metrics(end).total_height);
            }
        }
        start..end
    }

    fn rows(&self, visible: std::ops::Range<usize>) -> impl Iterator<Item = Row<'_>> {
        self.matches[visible]
            .iter()
            .map(move |&(index, _)| self.options[index as usize].format(&self.editor_data))
    }

    pub(crate) fn selected_option_index(&self) -> Option<usize> {
        self.cursor
            .and_then(|cursor| self.matches.get(cursor))
            .map(|&(index, _)| index as usize)
    }

    pub fn selection(&self) -> Option<&T> {
        self.cursor.and_then(|cursor| {
            self.matches
                .get(cursor)
                .map(|(index, _score)| &self.options[*index as usize])
        })
    }

    pub fn selection_mut(&mut self) -> Option<&mut T> {
        self.metrics_dirty = true;
        self.recalculate = true;
        self.selection_mut_untracked()
    }

    /// Callers must invalidate candidates if they change anything used by `Item::format`.
    pub(crate) fn selection_mut_untracked(&mut self) -> Option<&mut T> {
        self.cursor.and_then(|cursor| {
            self.matches
                .get(cursor)
                .map(|(index, _score)| &mut self.options[*index as usize])
        })
    }

    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    pub fn len(&self) -> usize {
        self.matches.len()
    }
}

impl<T: Item + PartialEq> Menu<T> {
    pub fn replace_option(&mut self, old_option: &impl PartialEq<T>, new_option: T) {
        let Some(index) = self.options.iter().position(|option| old_option == option) else {
            return;
        };
        self.options[index] = new_option;
        if self.metrics_dirty {
            return;
        }
        let row = self.options[index].format(&self.editor_data);
        let metrics = RowMetrics {
            height: row.row_height(),
            total_height: row.total_height(),
        };
        let widths: Vec<_> = row
            .cells
            .iter()
            .map(|cell| cell.content.width().min(u16::MAX as usize) as u16)
            .collect();
        let old = self.row_metrics[index];
        let geometry_changed = old.height != metrics.height
            || old.total_height != metrics.total_height
            || self.option_widths[index] != widths;
        self.row_metrics[index] = metrics;
        if !geometry_changed {
            return;
        }
        for (column, width) in self
            .column_widths
            .iter_mut()
            .zip(&self.option_widths[index])
        {
            let count = column.get_mut(width).unwrap();
            *count -= 1;
            if *count == 0 {
                column.remove(width);
            }
        }
        self.column_widths
            .resize_with(widths.len().max(self.column_widths.len()), BTreeMap::new);
        for (column, &width) in self.column_widths.iter_mut().zip(&widths) {
            *column.entry(width).or_default() += 1;
        }
        self.option_widths[index] = widths;
        self.refresh_column_widths();
        self.recalculate = true;
    }
}

use super::PromptEvent as MenuEvent;

impl<T: Item + 'static> Component for Menu<T> {
    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let event = match event {
            Event::Key(event) => *event,
            // Menu is a modal and should consume mouse events so clicks don't fall
            // through to the editor underneath
            Event::Mouse(_) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        let close_fn: Option<Callback> = Some(Box::new(|compositor: &mut Compositor, _| {
            // remove the layer
            compositor.pop();
        }));

        // Ignore tab key when supertab is turned on in order not to interfere
        // with it. (Is there a better way to do this?)
        if (event == key!(Tab) || event == shift!(Tab))
            && cx.editor.config().auto_completion
            && matches!(
                cx.editor.config().smart_tab,
                Some(SmartTabConfig {
                    enable: true,
                    supersede_menu: true,
                })
            )
        {
            return EventResult::Ignored(None);
        }

        match event {
            // esc or ctrl-c aborts the completion and closes the menu
            key!(Esc) | ctrl!('c') => {
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Abort);
                return EventResult::Consumed(close_fn);
            }
            // arrow up/ctrl-p/shift-tab prev completion choice (including updating the doc)
            shift!(Tab) | key!(Up) | ctrl!('p') => {
                self.move_up();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(Tab) | key!(Down) | ctrl!('n') => {
                // arrow down/ctrl-n/tab advances completion choice (including updating the doc)
                self.move_down();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(PageUp) | ctrl!('u') => {
                // page up moves back in the completion choice (including updating the doc)
                self.move_half_page_up();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(PageDown) | ctrl!('d') => {
                // page down advances completion choice (including updating the doc)
                self.move_half_page_down();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(Enter) => {
                if let Some(selection) = self.selection() {
                    (self.callback_fn)(cx.editor, Some(selection), MenuEvent::Validate);
                    return EventResult::Consumed(close_fn);
                } else {
                    return EventResult::Ignored(close_fn);
                }
            }
            // KeyEvent {
            //     code: KeyCode::Char(c),
            //     modifiers: KeyModifiers::NONE,
            // } => {
            //     self.insert_char(c);
            //     (self.callback_fn)(cx.editor, &self.line, MenuEvent::Update);
            // }

            // / -> edit_filter?
            //
            // enter confirms the match and closes the menu
            // typing filters the menu
            // if we run out of options the menu closes itself
            _ if self.auto_close => {
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Abort);
                return EventResult::Ignored(close_fn);
            }
            _ => (),
        }
        // for some events, we want to process them but send ignore, specifically all input except
        // tab/enter/ctrl-k or whatever will confirm the selection/ ctrl-n/ctrl-p for scroll.
        // EventResult::Consumed(None)
        EventResult::Ignored(None)
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        if viewport != self.viewport || self.recalculate {
            self.recalculate_size(viewport);
        }

        Some(self.size)
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let theme = &cx.editor.theme;
        let style = theme
            .try_get("ui.menu")
            .unwrap_or_else(|| theme.get("ui.text"));
        let selected = theme.get("ui.menu.selected");

        surface.clear_with(area, style);

        self.refresh_metrics();
        let visible = self.visible_rows(area.height);
        self.scroll = visible.start;
        let scroll = self.scroll;
        let len = self.matches.len();

        let win_height = area.height as usize;

        let rows = self.rows(visible);
        let table = Table::new(rows)
            .style(style)
            .highlight_style(selected)
            .column_spacing(1)
            .widths(&self.widths);

        use tui::widgets::TableState;

        table.render_table(
            area.clip_left(Self::LEFT_PADDING as u16).clip_right(1),
            surface,
            &mut TableState {
                offset: 0,
                selected: self.cursor.and_then(|cursor| cursor.checked_sub(scroll)),
            },
            false,
        );

        let render_borders = cx.editor.menu_border();

        if !render_borders {
            if let Some(cursor) = self.cursor.filter(|cursor| *cursor >= scroll) {
                let offset_from_top: u16 = self.matches[scroll..cursor]
                    .iter()
                    .map(|&(index, _)| self.row_metrics[index as usize].total_height)
                    .fold(0u16, u16::saturating_add);
                if offset_from_top < area.height {
                    let left = &mut surface[(area.left(), area.y + offset_from_top)];
                    left.set_style(selected);
                    let right =
                        &mut surface[(area.right().saturating_sub(1), area.y + offset_from_top)];
                    right.set_style(selected);
                }
            }
        }

        let fits = len <= win_height;

        let scroll_style = theme.get("ui.menu.scroll");
        if !fits {
            let scroll_height = win_height.pow(2).div_ceil(len).min(win_height);
            let scroll_line = (win_height - scroll_height) * scroll
                / std::cmp::max(1, len.saturating_sub(win_height));

            let mut cell;
            for i in 0..win_height {
                cell = &mut surface[(area.right() - 1, area.top() + i as u16)];

                let half_block = if render_borders { "▌" } else { "▐" };

                if scroll_line <= i && i < scroll_line + scroll_height {
                    // Draw scroll thumb
                    cell.set_symbol(half_block);
                    cell.set_fg(scroll_style.fg.unwrap_or(helix_view::theme::Color::Reset));
                } else if !render_borders {
                    // Draw scroll track
                    cell.set_symbol(half_block);
                    cell.set_fg(scroll_style.bg.unwrap_or(helix_view::theme::Color::Reset));
                }
            }
        }
    }
}

#[cfg(test)]
mod metric_tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use tui::widgets::TableState;

    use super::*;

    #[derive(PartialEq)]
    struct Candidate {
        label: String,
        height: u16,
        margin: u16,
    }

    impl Item for Candidate {
        type Data = Arc<AtomicUsize>;

        fn format(&self, calls: &Self::Data) -> Row<'_> {
            calls.fetch_add(1, Ordering::Relaxed);
            Row::new([self.label.as_str()])
                .height(self.height)
                .bottom_margin(self.margin)
        }
    }

    fn candidate(label: &str, height: u16, margin: u16) -> Candidate {
        Candidate {
            label: label.into(),
            height,
            margin,
        }
    }

    #[test]
    fn stable_sizing_and_filtering_do_not_reformat_candidates() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut menu = Menu::new(
            (0..10_000)
                .map(|index| candidate(&format!("候補{index}"), 1, 0))
                .collect(),
            calls.clone(),
            |_, _, _| {},
        );
        assert_eq!(menu.required_size((80, 24)), Some((11, 10)));
        assert_eq!(calls.load(Ordering::Relaxed), 10_000);
        assert_eq!(menu.required_size((80, 24)), Some((11, 10)));
        assert_eq!(menu.required_size((40, 12)), Some((11, 10)));
        assert_eq!(calls.load(Ordering::Relaxed), 10_000);

        menu.update_matches().0.truncate(3);
        assert_eq!(menu.required_size((40, 12)), Some((10, 3)));
        assert_eq!(calls.load(Ordering::Relaxed), 10_000);
        let visible = menu.visible_rows(3);
        assert_eq!(menu.rows(visible).count(), 3);
        assert_eq!(calls.load(Ordering::Relaxed), 10_003);

        let old = candidate("候補0", 1, 0);
        menu.replace_option(&old, candidate("a much longer resolved label", 1, 0));
        assert_eq!(menu.required_size((80, 24)), Some((30, 3)));
        assert_eq!(calls.load(Ordering::Relaxed), 10_004);
        menu.replace_option(
            &candidate("a much longer resolved label", 1, 0),
            candidate("short", 2, 1),
        );
        assert_eq!(menu.required_size((80, 24)), Some((10, 3)));
        assert_eq!(calls.load(Ordering::Relaxed), 10_005);
        assert_eq!(menu.row_metrics[0].height, 2);
        assert_eq!(menu.row_metrics[0].total_height, 3);
    }

    #[test]
    fn resolution_with_unchanged_geometry_formats_only_the_replaced_candidate() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut menu = Menu::new(
            (0..10_000).map(|_| candidate("same", 1, 0)).collect(),
            calls.clone(),
            |_, _, _| {},
        );
        let original = menu.required_size((80, 24));
        let old = candidate("same", 1, 0);
        menu.replace_option(&old, candidate("same", 1, 0));
        assert!(!menu.recalculate);
        assert_eq!(menu.required_size((80, 24)), original);
        assert_eq!(calls.load(Ordering::Relaxed), 10_001);
    }

    #[test]
    fn replacing_a_unique_maximum_updates_and_removes_column_widths() {
        #[derive(PartialEq)]
        struct Columns(Vec<String>);
        impl Item for Columns {
            type Data = ();
            fn format(&self, _: &()) -> Row<'_> {
                Row::new(self.0.iter().map(String::as_str))
            }
        }
        let old = Columns(vec!["long label".into(), "extra".into()]);
        let mut menu = Menu::new(
            vec![Columns(old.0.clone()), Columns(vec!["short".into()])],
            (),
            |_, _, _| {},
        );
        menu.required_size((80, 24));
        assert_eq!(
            menu.widths,
            vec![Constraint::Length(10), Constraint::Length(5)]
        );
        menu.replace_option(&old, Columns(vec!["tiny".into()]));
        menu.required_size((80, 24));
        assert_eq!(menu.widths, vec![Constraint::Length(5)]);
    }

    #[test]
    fn visible_rows_preserve_table_scrolling_heights_margins_and_styles() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut menu = Menu::new(
            vec![
                candidate("one", 1, 1),
                candidate("two\nsecond", 2, 0),
                candidate("three", 1, 1),
                candidate("four", 1, 0),
                candidate("five\nsecond", 2, 0),
            ],
            calls,
            |_, _, _| {},
        );
        menu.refresh_metrics();
        let area = Rect::new(0, 0, 16, 5);
        let selected_style =
            helix_view::graphics::Style::default().fg(helix_view::graphics::Color::Red);
        for cursor in [0, 1, 2, 3, 4, 3, 0] {
            menu.cursor = Some(cursor);
            let previous_scroll = menu.scroll;
            let mut expected = Surface::empty(area);
            let mut state = TableState {
                offset: previous_scroll,
                selected: Some(cursor),
            };
            Table::new(menu.rows(0..menu.matches.len()))
                .widths(&menu.widths)
                .highlight_style(selected_style)
                .render_table(area, &mut expected, &mut state, false);

            let visible = menu.visible_rows(area.height);
            assert_eq!(visible.start, state.offset);
            menu.scroll = visible.start;
            let mut actual = Surface::empty(area);
            Table::new(menu.rows(visible))
                .widths(&menu.widths)
                .highlight_style(selected_style)
                .render_table(
                    area,
                    &mut actual,
                    &mut TableState {
                        offset: 0,
                        selected: cursor.checked_sub(menu.scroll),
                    },
                    false,
                );
            assert_eq!(actual, expected, "selected candidate {cursor}");
        }
    }
}
