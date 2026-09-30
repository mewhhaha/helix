use crate::{
    buffer::Buffer,
    layout::Alignment,
    text::{Span, Spans, StyledGrapheme, Text},
    widgets::{
        reflow::{LineComposer, LineTruncator, WordWrapper},
        Block, Widget,
    },
};
use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};
use std::{iter, sync::Arc};

const MAX_LAYOUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutKey {
    width: u16,
    trim: Option<bool>,
    horizontal_offset: u16,
}

#[derive(Debug)]
struct PreparedRow {
    spans: Spans<'static>,
    width: u16,
    leading_blank: Option<Style>,
}

#[derive(Debug)]
struct PreparedRows {
    rows: Vec<PreparedRow>,
    bytes: usize,
    width: u16,
    complete: bool,
}

/// Owned content makes validation correct even when callers directly mutate Text::lines.
/// Two recently used widths cover sizing followed by rendering without retaining an
/// unbounded collection of layouts.
#[derive(Debug, Default)]
pub(crate) struct ParagraphCache {
    source: Vec<Spans<'static>>,
    source_bytes: usize,
    layouts: Vec<(LayoutKey, Arc<PreparedRows>)>,
}

fn content_matches(source: &[Spans<'_>], current: &[Spans<'_>]) -> bool {
    source.len() == current.len()
        && source.iter().zip(current).all(|(a, b)| {
            a.0.len() == b.0.len()
                && a.0
                    .iter()
                    .zip(&b.0)
                    .all(|(a, b)| a.content == b.content && a.style == b.style)
        })
}

fn spans_bytes(lines: &[Spans<'_>]) -> usize {
    std::mem::size_of_val(lines)
        + lines
            .iter()
            .map(|line| {
                line.0.capacity() * std::mem::size_of::<Span>()
                    + line
                        .0
                        .iter()
                        .map(|span| match &span.content {
                            std::borrow::Cow::Owned(s) => s.capacity(),
                            std::borrow::Cow::Borrowed(s) => s.len(),
                        })
                        .sum::<usize>()
            })
            .sum::<usize>()
}

fn compose_rows(
    source: &[Spans<'static>],
    key: LayoutKey,
    budget: usize,
    requested: Option<usize>,
) -> Option<PreparedRows> {
    // These addresses are used only while composing this owned source, never as
    // persistent identities. Contiguous symbols from the same span can be stored
    // as one owned span while preserving original grapheme boundaries.
    let mut spans: Vec<_> = source
        .iter()
        .flat_map(|line| &line.0)
        .map(|span| (span.content.as_ptr() as usize, span.content.len()))
        .collect();
    spans.sort_unstable();
    let mut styled = source.iter().flat_map(|line| {
        line.0
            .iter()
            .flat_map(|span| span.styled_graphemes(Style::default()))
            .chain(iter::once(StyledGrapheme {
                symbol: "\n",
                style: Style::default(),
            }))
    });
    let mut composer: Box<dyn LineComposer> = if let Some(trim) = key.trim {
        Box::new(WordWrapper::new(&mut styled, key.width, trim))
    } else {
        let mut composer = LineTruncator::new(&mut styled, key.width);
        composer.set_horizontal_offset(key.horizontal_offset);
        Box::new(composer)
    };
    let mut rows = Vec::new();
    let mut retained = 0;
    let mut complete = true;
    while let Some((line, width)) = composer.next_line() {
        let mut output: Vec<Span<'static>> = Vec::new();
        let mut previous = None;
        let mut leading_blank = None;
        for grapheme in line {
            if grapheme.symbol.is_empty() {
                if output.is_empty() {
                    leading_blank = Some(grapheme.style);
                }
                continue;
            }
            let address = grapheme.symbol.as_ptr() as usize;
            let source = spans
                .partition_point(|&(start, _)| start <= address)
                .saturating_sub(1);
            let contiguous = previous == Some((source, address))
                && output
                    .last()
                    .is_some_and(|span| span.style == grapheme.style);
            if contiguous {
                output
                    .last_mut()
                    .unwrap()
                    .content
                    .to_mut()
                    .push_str(grapheme.symbol);
            } else {
                output.push(Span::styled(grapheme.symbol.to_owned(), grapheme.style));
            }
            previous = Some((source, address + grapheme.symbol.len()));
        }
        for span in &mut output {
            span.content.to_mut().shrink_to_fit();
        }
        output.shrink_to_fit();
        retained += std::mem::size_of::<PreparedRow>()
            + output.capacity() * std::mem::size_of::<Span>()
            + output.iter().map(|span| span.content.len()).sum::<usize>();
        if retained > budget {
            return None;
        }
        rows.push(PreparedRow {
            spans: Spans(output),
            width,
            leading_blank,
        });
        if requested.is_some_and(|limit| rows.len() >= limit) {
            complete = false;
            break;
        }
    }
    rows.shrink_to_fit();
    let width = rows.iter().map(|row| row.width).max().unwrap_or(0);
    Some(PreparedRows {
        rows,
        bytes: retained,
        width,
        complete,
    })
}

fn get_line_offset(line_width: u16, text_area_width: u16, alignment: Alignment) -> u16 {
    match alignment {
        Alignment::Center => (text_area_width / 2).saturating_sub(line_width / 2),
        Alignment::Right => text_area_width.saturating_sub(line_width),
        Alignment::Left => 0,
    }
}

/// A widget to display some text.
///
/// # Examples
///
/// ```
/// # use helix_tui::text::{Text, Spans, Span};
/// # use helix_tui::widgets::{Block, Borders, Paragraph, Wrap};
/// # use helix_tui::layout::{Alignment};
/// # use helix_view::graphics::{Style, Color, Modifier};
/// let text = Text::from(vec![
///     Spans::from(vec![
///         Span::raw("First"),
///         Span::styled("line",Style::default().add_modifier(Modifier::ITALIC)),
///         Span::raw("."),
///     ]),
///     Spans::from(Span::styled("Second line", Style::default().fg(Color::Red))),
/// ]);
/// Paragraph::new(&text)
///     .block(Block::bordered().title("Paragraph"))
///     .style(Style::default().fg(Color::White).bg(Color::Black))
///     .alignment(Alignment::Center)
///     .wrap(Wrap { trim: true });
/// ```
#[derive(Debug, Clone)]
pub struct Paragraph<'a> {
    /// A block to wrap the widget in
    block: Option<Block<'a>>,
    /// Widget style
    style: Style,
    /// How to wrap the text
    wrap: Option<Wrap>,
    /// The text to display
    text: &'a Text<'a>,
    /// Scroll
    scroll: (u16, u16),
    /// Alignment of the text
    alignment: Alignment,
}

/// Describes how to wrap text across lines.
///
/// ## Examples
///
/// ```
/// # use helix_tui::widgets::{Paragraph, Wrap};
/// # use helix_tui::text::Text;
/// let bullet_points = Text::from(r#"Some indented points:
///     - First thing goes here and is long so that it wraps
///     - Here is another point that is long enough to wrap"#);
///
/// // With leading spaces trimmed (window width of 30 chars):
/// Paragraph::new(&bullet_points).wrap(Wrap { trim: true });
/// // Some indented points:
/// // - First thing goes here and is
/// // long so that it wraps
/// // - Here is another point that
/// // is long enough to wrap
///
/// // But without trimming, indentation is preserved:
/// Paragraph::new(&bullet_points).wrap(Wrap { trim: false });
/// // Some indented points:
/// //     - First thing goes here
/// // and is long so that it wraps
/// //     - Here is another point
/// // that is long enough to wrap
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Wrap {
    /// Should leading whitespace be trimmed
    pub trim: bool,
}

impl<'a> Paragraph<'a> {
    pub fn new(text: &'a Text) -> Paragraph<'a> {
        Paragraph {
            block: None,
            style: Default::default(),
            wrap: None,
            text,
            scroll: (0, 0),
            alignment: Alignment::Left,
        }
    }

    pub fn block(mut self, block: Block<'a>) -> Paragraph<'a> {
        self.block = Some(block);
        self
    }

    pub fn style(mut self, style: Style) -> Paragraph<'a> {
        self.style = style;
        self
    }

    pub fn wrap(mut self, wrap: Wrap) -> Paragraph<'a> {
        self.wrap = Some(wrap);
        self
    }

    pub fn scroll(mut self, offset: (u16, u16)) -> Paragraph<'a> {
        self.scroll = offset;
        self
    }

    pub fn alignment(mut self, alignment: Alignment) -> Paragraph<'a> {
        self.alignment = alignment;
        self
    }

    fn prepared_rows(&self, width: u16) -> Option<Arc<PreparedRows>> {
        self.prepared_rows_for(width, None)
    }

    fn prepared_rows_for(
        &self,
        width: u16,
        mut requested: Option<usize>,
    ) -> Option<Arc<PreparedRows>> {
        let key = LayoutKey {
            width,
            trim: self.wrap.map(|wrap| wrap.trim),
            horizontal_offset: if self.wrap.is_none() && self.alignment == Alignment::Left {
                self.scroll.1
            } else {
                0
            },
        };
        let mut cache = self
            .text
            .layout_cache
            .get_or_init(Default::default)
            .lock()
            .unwrap();
        if !content_matches(&cache.source, &self.text.lines) {
            // Check size before duplicating a very large transient paragraph.
            if spans_bytes(&self.text.lines) > MAX_LAYOUT_BYTES / 2 {
                *cache = ParagraphCache::default();
                return None;
            }
            cache.source = self
                .text
                .lines
                .iter()
                .map(|line| {
                    Spans(
                        line.0
                            .iter()
                            .map(|span| Span::styled(span.content.to_string(), span.style))
                            .collect(),
                    )
                })
                .collect();
            cache.source.shrink_to_fit();
            cache.source_bytes = spans_bytes(&cache.source);
            cache.layouts.clear();
        }
        if let Some(index) = cache.layouts.iter().position(|(cached, _)| *cached == key) {
            let (_, rows) = &cache.layouts[index];
            if rows.complete || requested.is_some_and(|requested| requested <= rows.rows.len()) {
                let (_, rows) = cache.layouts.remove(index);
                cache.layouts.push((key, rows.clone()));
                return Some(rows);
            }
            // Grow geometrically during scrolling instead of repeatedly rebuilding
            // the same prefix for each newly exposed row.
            requested = requested.map(|requested| requested.max(rows.rows.len().saturating_mul(2)));
        }
        let rows = Arc::new(compose_rows(
            &cache.source,
            key,
            MAX_LAYOUT_BYTES.saturating_sub(cache.source_bytes),
            requested,
        )?);
        cache.layouts.retain(|(cached, _)| *cached != key);
        while cache.layouts.len() >= 2
            || cache.source_bytes
                + rows.bytes
                + cache
                    .layouts
                    .iter()
                    .map(|(_, rows)| rows.bytes)
                    .sum::<usize>()
                > MAX_LAYOUT_BYTES
        {
            cache.layouts.remove(0);
        }
        cache.layouts.push((key, rows.clone()));
        Some(rows)
    }

    pub fn required_size(&self, max_text_width: u16) -> (u16, u16) {
        if let Some(rows) = self.prepared_rows(max_text_width) {
            return (rows.width, rows.rows.len().min(u16::MAX as usize) as u16);
        }
        let style = self.style;
        let mut styled = self.text.lines.iter().flat_map(|spans| {
            spans
                .0
                .iter()
                .flat_map(|span| span.styled_graphemes(style))
                // Required given the way composers work but might be refactored out if we change
                // composers to operate on lines instead of a stream of graphemes.
                .chain(iter::once(StyledGrapheme {
                    symbol: "\n",
                    style: self.style,
                }))
        });
        let mut line_composer: Box<dyn LineComposer> = if let Some(Wrap { trim }) = self.wrap {
            Box::new(WordWrapper::new(&mut styled, max_text_width, trim))
        } else {
            let mut line_composer = Box::new(LineTruncator::new(&mut styled, max_text_width));
            if self.alignment == Alignment::Left {
                line_composer.set_horizontal_offset(self.scroll.1);
            }
            line_composer
        };
        let mut text_width = 0;
        let mut text_height = 0u16;
        while let Some((_, line_width)) = line_composer.next_line() {
            text_width = line_width.max(text_width);
            text_height = text_height.saturating_add(1);
        }
        (text_width, text_height)
    }
}

impl Widget for Paragraph<'_> {
    fn render(mut self, area: Rect, buf: &mut Buffer) {
        buf.set_style(area, self.style);
        let text_area = match self.block.take() {
            Some(b) => {
                let inner_area = b.inner(area);
                b.render(area, buf);
                inner_area
            }
            None => area,
        };

        if text_area.height < 1 {
            return;
        }

        if let Some(rows) = self.prepared_rows_for(
            text_area.width,
            Some(self.scroll.0 as usize + text_area.height as usize),
        ) {
            for (y, row) in rows
                .rows
                .iter()
                .skip(self.scroll.0 as usize)
                .take(text_area.height as usize)
                .enumerate()
            {
                let mut x = get_line_offset(row.width, text_area.width, self.alignment);
                let y = text_area.top() + y as u16;
                if let Some(style) = row.leading_blank.filter(|_| x < text_area.width) {
                    buf[(text_area.left() + x, y)]
                        .set_symbol(" ")
                        .set_style(self.style.patch(style));
                }
                for span in &row.spans.0 {
                    for grapheme in span.styled_graphemes(self.style) {
                        if x >= text_area.width {
                            break;
                        }
                        buf[(text_area.left() + x, y)]
                            .set_symbol(grapheme.symbol)
                            .set_style(grapheme.style);
                        x += grapheme.symbol.width() as u16;
                    }
                }
            }
            return;
        }

        let style = self.style;
        let mut styled = self.text.lines.iter().flat_map(|spans| {
            spans
                .0
                .iter()
                .flat_map(|span| span.styled_graphemes(style))
                // Required given the way composers work but might be refactored out if we change
                // composers to operate on lines instead of a stream of graphemes.
                .chain(iter::once(StyledGrapheme {
                    symbol: "\n",
                    style: self.style,
                }))
        });

        let mut line_composer: Box<dyn LineComposer> = if let Some(Wrap { trim }) = self.wrap {
            Box::new(WordWrapper::new(&mut styled, text_area.width, trim))
        } else {
            let mut line_composer = Box::new(LineTruncator::new(&mut styled, text_area.width));
            if self.alignment == Alignment::Left {
                line_composer.set_horizontal_offset(self.scroll.1);
            }
            line_composer
        };
        let mut y = 0;
        while let Some((current_line, current_line_width)) = line_composer.next_line() {
            if y >= self.scroll.0 as usize {
                let mut x = get_line_offset(current_line_width, text_area.width, self.alignment);
                for StyledGrapheme { symbol, style } in current_line {
                    buf[(
                        text_area.left() + x,
                        text_area.top() + (y - self.scroll.0 as usize) as u16,
                    )]
                        .set_symbol(if symbol.is_empty() {
                            // If the symbol is empty, the last char which rendered last time will
                            // leave on the line. It's a quick fix.
                            " "
                        } else {
                            symbol
                        })
                        .set_style(*style);
                    x += symbol.width() as u16;
                }
            }
            y += 1;
            if y >= text_area.height as usize + self.scroll.0 as usize {
                break;
            }
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use helix_view::graphics::{Color, Modifier};

    fn reference(paragraph: Paragraph<'_>, area: Rect) -> Buffer {
        let mut buffer = Buffer::empty(area);
        buffer.set_style(area, paragraph.style);
        let mut styled = paragraph.text.lines.iter().flat_map(|line| {
            line.0
                .iter()
                .flat_map(|span| span.styled_graphemes(paragraph.style))
                .chain(iter::once(StyledGrapheme {
                    symbol: "\n",
                    style: paragraph.style,
                }))
        });
        let mut composer: Box<dyn LineComposer> = if let Some(wrap) = paragraph.wrap {
            Box::new(WordWrapper::new(&mut styled, area.width, wrap.trim))
        } else {
            let mut composer = LineTruncator::new(&mut styled, area.width);
            if paragraph.alignment == Alignment::Left {
                composer.set_horizontal_offset(paragraph.scroll.1);
            }
            Box::new(composer)
        };
        let mut row = 0usize;
        while let Some((line, width)) = composer.next_line() {
            if row >= paragraph.scroll.0 as usize {
                let mut x = get_line_offset(width, area.width, paragraph.alignment);
                for grapheme in line {
                    buffer[(
                        area.x + x,
                        area.y + (row - paragraph.scroll.0 as usize) as u16,
                    )]
                        .set_symbol(if grapheme.symbol.is_empty() {
                            " "
                        } else {
                            grapheme.symbol
                        })
                        .set_style(grapheme.style);
                    x += grapheme.symbol.width() as u16;
                }
            }
            row += 1;
            if row >= paragraph.scroll.0 as usize + area.height as usize {
                break;
            }
        }
        buffer
    }

    #[test]
    fn cached_rows_preserve_wrap_unicode_styles_alignment_and_horizontal_scroll() {
        let text = Text::from(vec![
            Spans::from(vec![
                Span::raw("  abc "),
                Span::styled("界a\u{301} xyz", Style::default().fg(Color::Red)),
            ]),
            Spans::from(vec![
                Span::styled(" ", Style::default().bg(Color::Blue)),
                Span::raw("more words here"),
            ]),
            Spans::from(""),
            Spans::from("fin"),
        ]);
        let area = Rect::new(0, 0, 7, 3);
        for trim in [None, Some(false), Some(true)] {
            for alignment in [Alignment::Left, Alignment::Center, Alignment::Right] {
                for scroll in [(0, 0), (1, 0), (0, 4), (2, 30)] {
                    let mut paragraph = Paragraph::new(&text)
                        .alignment(alignment)
                        .scroll(scroll)
                        .style(
                            Style::default()
                                .bg(Color::Black)
                                .add_modifier(Modifier::BOLD),
                        );
                    paragraph.wrap = trim.map(|trim| Wrap { trim });
                    let expected = reference(paragraph.clone(), area);
                    let mut actual = Buffer::empty(area);
                    paragraph.render(area, &mut actual);
                    assert_eq!(
                        actual, expected,
                        "trim={trim:?}, alignment={alignment:?}, scroll={scroll:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn prepared_rows_reuse_scrolling_and_detect_direct_content_and_style_mutations() {
        let mut text = Text::raw("one two three\nsecond");
        let first = Paragraph::new(&text)
            .wrap(Wrap { trim: true })
            .prepared_rows(6)
            .unwrap();
        let warm = Paragraph::new(&text)
            .wrap(Wrap { trim: true })
            .scroll((2, 0))
            .prepared_rows(6)
            .unwrap();
        assert!(Arc::ptr_eq(&first, &warm));
        let other_width = Paragraph::new(&text)
            .wrap(Wrap { trim: true })
            .prepared_rows(10)
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &other_width));
        assert!(Arc::ptr_eq(
            &first,
            &Paragraph::new(&text)
                .wrap(Wrap { trim: true })
                .prepared_rows(6)
                .unwrap()
        ));
        text.lines[0].0[0]
            .content
            .to_mut()
            .replace_range(0..3, "new");
        let changed = Paragraph::new(&text)
            .wrap(Wrap { trim: true })
            .prepared_rows(6)
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &changed));
        assert_eq!(changed.rows[0].spans.0[0].content, "new");
        text.lines[0].0[0].style = Style::default().fg(Color::Green);
        let styled = Paragraph::new(&text)
            .wrap(Wrap { trim: true })
            .prepared_rows(6)
            .unwrap();
        assert_eq!(styled.rows[0].spans.0[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn cold_render_prepares_only_visible_prefix_and_extends_when_scrolled() {
        let text = Text::raw("line\n".repeat(10_000));
        let area = Rect::new(0, 0, 80, 25);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(&text).render(area, &mut buffer);
        {
            let cache = text.layout_cache.get().unwrap().lock().unwrap();
            assert_eq!(cache.layouts[0].1.rows.len(), 25);
            assert!(!cache.layouts[0].1.complete);
        }
        let first = Paragraph::new(&text)
            .prepared_rows_for(80, Some(25))
            .unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &Paragraph::new(&text)
                .prepared_rows_for(80, Some(20))
                .unwrap()
        ));
        Paragraph::new(&text)
            .scroll((25, 0))
            .render(area, &mut buffer);
        assert_eq!(
            text.layout_cache.get().unwrap().lock().unwrap().layouts[0]
                .1
                .rows
                .len(),
            50
        );
        assert_eq!(Paragraph::new(&text).required_size(80), (4, 10_000));
        assert!(
            text.layout_cache.get().unwrap().lock().unwrap().layouts[0]
                .1
                .complete
        );
    }

    #[test]
    fn retained_layouts_have_a_hard_byte_bound() {
        let text = Text::raw("words words words\n".repeat(10_000));
        for width in [20, 30, 40, 50] {
            Paragraph::new(&text)
                .wrap(Wrap { trim: true })
                .prepared_rows(width)
                .unwrap();
            let cache = text.layout_cache.get().unwrap().lock().unwrap();
            assert!(cache.layouts.len() <= 2);
            assert!(
                cache.source_bytes
                    + cache
                        .layouts
                        .iter()
                        .map(|(_, rows)| rows.bytes)
                        .sum::<usize>()
                    <= MAX_LAYOUT_BYTES
            );
        }
        let oversized = Text::raw("x".repeat(MAX_LAYOUT_BYTES));
        assert!(Paragraph::new(&oversized).prepared_rows(80).is_none());
        assert!(oversized
            .layout_cache
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .source
            .is_empty());
    }
}
