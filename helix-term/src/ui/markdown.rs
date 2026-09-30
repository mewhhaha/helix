use crate::compositor::{Component, Context};
use arc_swap::ArcSwap;
use tui::{
    buffer::Buffer as Surface,
    text::{Span, Spans, Text},
};

use std::{
    cell::{Cell, OnceCell, RefCell},
    sync::Arc,
};

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use helix_core::{
    syntax::{self, HighlightEvent, OverlayHighlights},
    RopeSlice, Syntax,
};
use helix_view::{
    graphics::{Color, Margin, Rect, Style},
    theme::Modifier,
    Theme,
};

use super::color::{css_color_ranges, parse_css_color};

fn preview_background(theme: Option<&Theme>) -> Option<Color> {
    theme.and_then(|theme| {
        theme
            .get("ui.popup")
            .bg
            .filter(|color| matches!(color, Color::Rgb(..)))
            .or_else(|| {
                theme
                    .get("ui.background")
                    .bg
                    .filter(|color| matches!(color, Color::Rgb(..)))
            })
    })
}

/// Split the existing syntax spans rather than replacing their styles. A CSS
/// color frequently crosses span boundaries (e.g. the `#` and its hex digits).
fn decorate_colors<'a>(
    text: &mut Text<'a>,
    colors: &[(std::ops::Range<usize>, Color)],
    swatches: bool,
    values: bool,
) {
    if colors.is_empty() {
        return;
    }
    let mut pos = 0;
    let mut color_index = 0;
    for line in &mut text.lines {
        let mut output = Vec::new();
        for span in std::mem::take(&mut line.0) {
            let mut offset = 0;
            while offset < span.content.len() {
                while colors
                    .get(color_index)
                    .is_some_and(|(range, _)| range.end <= pos)
                {
                    color_index += 1;
                }
                let available = span.content.len() - offset;
                let (length, color) = match colors.get(color_index) {
                    Some((range, color)) if range.start <= pos => {
                        if swatches && range.start == pos {
                            output.push(Span::styled("■ ", span.style.fg(*color)));
                        }
                        ((range.end - pos).min(available), Some(*color))
                    }
                    Some((range, _)) => ((range.start - pos).min(available), None),
                    None => (available, None),
                };
                let style = match color.filter(|_| values) {
                    Some(color) => span.style.fg(color),
                    None => span.style,
                };
                output.push(Span::styled(
                    span.content[offset..offset + length].to_owned(),
                    style,
                ));
                offset += length;
                pos += length;
            }
        }
        line.0 = output;
        pos += 1; // The newline separating rendered lines.
    }
}

fn color_code_block(
    text: &mut Text<'_>,
    language: &str,
    theme: Option<&Theme>,
    swatches: bool,
    values: bool,
) {
    let language = language.split_whitespace().next().unwrap_or_default();
    let css = ["css", "scss", "sass", "less"]
        .iter()
        .any(|css| language.eq_ignore_ascii_case(css));
    let plain = language.is_empty() || language.eq_ignore_ascii_case("plaintext");
    if !css && !plain {
        return;
    }
    let background = preview_background(theme);
    if plain {
        let source = text
            .lines
            .iter()
            .map(|line| {
                line.0
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        if parse_css_color(&source, background).is_none() {
            return;
        }
    }
    // Syntax rendering expands tabs. Apply the same expansion when measuring
    // the popup without a theme, and when no grammar is installed.
    for span in text.lines.iter_mut().flat_map(|line| &mut line.0) {
        if span.content.contains('\t') {
            span.content = span.content.replace('\t', "    ").into();
        }
    }
    let source = text
        .lines
        .iter()
        .map(|line| {
            line.0
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let colors = if css {
        css_color_ranges(&source, background)
    } else {
        // Some servers return a resolved value without identifying it as CSS.
        parse_css_color(source.trim(), background)
            .map(|color| {
                let start = source.len() - source.trim_start().len();
                vec![(start..source.trim_end().len(), color)]
            })
            .unwrap_or_default()
    };
    decorate_colors(text, &colors, swatches, values);
}

fn styled_multiline_text<'a>(text: &str, style: Style) -> Text<'a> {
    let spans: Vec<_> = text
        .lines()
        .map(|line| Span::styled(line.replace('\t', "    "), style))
        .map(Spans::from)
        .collect();
    Text::from(spans)
}

pub fn highlighted_code_block<'a>(
    text: &str,
    language: &str,
    theme: Option<&Theme>,
    loader: &syntax::Loader,
    // Optional overlay highlights to mix in with the syntax highlights.
    //
    // Note that `OverlayHighlights` is typically used with char indexing but the only caller
    // which passes this parameter currently passes **byte indices** instead.
    additional_highlight_spans: Option<OverlayHighlights>,
) -> Text<'a> {
    let mut spans = Vec::new();
    let mut lines = Vec::new();

    let get_theme = |key: &str| -> Style { theme.map(|t| t.get(key)).unwrap_or_default() };
    let text_style = get_theme(Markdown::TEXT_STYLE);
    let code_style = get_theme(Markdown::BLOCK_STYLE);

    let theme = match theme {
        Some(t) => t,
        None => return styled_multiline_text(text, code_style),
    };

    let ropeslice = RopeSlice::from(text);
    let Some(syntax) = loader
        .language_for_match(RopeSlice::from(language))
        .and_then(|lang| Syntax::new(ropeslice, lang, loader).ok())
    else {
        return styled_multiline_text(text, code_style);
    };

    let mut syntax_highlighter = syntax.highlighter(ropeslice, loader, ..);
    let mut syntax_highlight_stack = Vec::new();
    let mut overlay_highlight_stack = Vec::new();
    let mut overlay_highlighter = syntax::OverlayHighlighter::new(additional_highlight_spans);
    let mut pos = 0;

    while pos < ropeslice.len_bytes() as u32 {
        if pos == syntax_highlighter.next_event_offset() {
            let (event, new_highlights) = syntax_highlighter.advance();
            if event == HighlightEvent::Refresh {
                syntax_highlight_stack.clear();
            }
            syntax_highlight_stack.extend(new_highlights);
        } else if pos == overlay_highlighter.next_event_offset() as u32 {
            let (event, new_highlights) = overlay_highlighter.advance();
            if event == HighlightEvent::Refresh {
                overlay_highlight_stack.clear();
            }
            overlay_highlight_stack.extend(new_highlights)
        }

        let start = pos;
        pos = syntax_highlighter
            .next_event_offset()
            .min(overlay_highlighter.next_event_offset() as u32);
        if pos == u32::MAX {
            pos = ropeslice.len_bytes() as u32;
        }
        if pos == start {
            continue;
        }
        // The highlighter should always move forward.
        // If the highlighter malfunctions, bail on syntax highlighting and log an error.
        debug_assert!(pos > start);
        if pos < start {
            log::error!("Failed to highlight '{language}': {text:?}");
            return styled_multiline_text(text, code_style);
        }

        let style = syntax_highlight_stack
            .iter()
            .chain(overlay_highlight_stack.iter())
            .fold(text_style, |acc, highlight| {
                acc.patch(theme.highlight(*highlight))
            });

        let mut slice = &text[start as usize..pos as usize];
        // TODO: do we need to handle all unicode line endings
        // here, or is just '\n' okay?
        while let Some(end) = slice.find('\n') {
            // emit span up to newline
            let text = &slice[..end];
            let text = text.replace('\t', "    "); // replace tabs
            let span = Span::styled(text, style);
            spans.push(span);

            // truncate slice to after newline
            slice = &slice[end + 1..];

            // make a new line
            let spans = std::mem::take(&mut spans);
            lines.push(Spans::from(spans));
        }

        if !slice.is_empty() {
            let span = Span::styled(slice.replace('\t', "    "), style);
            spans.push(span);
        }
    }

    if !spans.is_empty() {
        let spans = std::mem::take(&mut spans);
        lines.push(Spans::from(spans));
    }

    Text::from(lines)
}

pub struct Markdown {
    contents: String,

    config_loader: Arc<ArcSwap<syntax::Loader>>,
    color_swatches: bool,
    color_values: bool,
    layout_cache: OnceCell<Arc<Text<'static>>>,
    rendered_cache: RefCell<Option<RenderedMarkdown>>,
    measured_size: Cell<Option<(u16, (u16, u16))>>,
}

struct RenderedMarkdown {
    theme: usize,
    loader: Arc<syntax::Loader>,
    scopes: Arc<Vec<String>>,
    text: Arc<Text<'static>>,
}

fn owned_text(text: Text<'_>) -> Arc<Text<'static>> {
    Arc::new(Text::from(
        text.lines
            .into_iter()
            .map(|line| {
                Spans::from(
                    line.0
                        .into_iter()
                        .map(|span| Span::styled(span.content.into_owned(), span.style))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>(),
    ))
}

impl Markdown {
    const TEXT_STYLE: &'static str = "ui.text";
    const BLOCK_STYLE: &'static str = "markup.raw.inline";
    const RULE_STYLE: &'static str = "punctuation.special";
    const UNNUMBERED_LIST_STYLE: &'static str = "markup.list.unnumbered";
    const NUMBERED_LIST_STYLE: &'static str = "markup.list.numbered";
    const HEADING_STYLES: [&'static str; 6] = [
        "markup.heading.1",
        "markup.heading.2",
        "markup.heading.3",
        "markup.heading.4",
        "markup.heading.5",
        "markup.heading.6",
    ];
    const INDENT: &'static str = "  ";

    pub fn new(contents: String, config_loader: Arc<ArcSwap<syntax::Loader>>) -> Self {
        Self {
            contents,
            config_loader,
            color_swatches: false,
            color_values: false,
            layout_cache: OnceCell::new(),
            rendered_cache: RefCell::new(None),
            measured_size: Cell::new(None),
        }
    }

    pub fn with_color_previews(mut self, swatches: bool, values: bool) -> Self {
        self.color_swatches = swatches;
        self.color_values = values;
        self.layout_cache.take();
        self.rendered_cache.get_mut().take();
        self.measured_size.set(None);
        self
    }

    /// Reuse immutable rendered content until its theme or syntax configuration changes.
    pub fn parse(&self, theme: Option<&Theme>) -> Arc<Text<'static>> {
        let Some(theme) = theme else {
            return Arc::clone(self.layout_cache.get_or_init(|| {
                owned_text(self.parse_uncached(None, &self.config_loader.load()))
            }));
        };
        let loader = self.config_loader.load_full();
        let scopes = Arc::clone(&loader.scopes());
        if let Some(cached) = self.rendered_cache.borrow().as_ref() {
            if cached.theme == theme.cache_key()
                && Arc::ptr_eq(&cached.loader, &loader)
                && Arc::ptr_eq(&cached.scopes, &scopes)
            {
                return Arc::clone(&cached.text);
            }
        }
        let text = owned_text(self.parse_uncached(Some(theme), &loader));
        *self.rendered_cache.borrow_mut() = Some(RenderedMarkdown {
            theme: theme.cache_key(),
            loader,
            scopes,
            text: Arc::clone(&text),
        });
        text
    }

    pub fn dimensions(&self, max_width: u16) -> (u16, u16) {
        if let Some((width, dimensions)) = self.measured_size.get() {
            if width == max_width {
                return dimensions;
            }
        }
        let dimensions = super::text::required_size(&self.parse(None), max_width);
        self.measured_size.set(Some((max_width, dimensions)));
        dimensions
    }

    fn parse_uncached(&self, theme: Option<&Theme>, loader: &syntax::Loader) -> Text<'_> {
        fn push_line<'a>(spans: &mut Vec<Span<'a>>, lines: &mut Vec<Spans<'a>>) {
            let spans = std::mem::take(spans);
            if !spans.is_empty() {
                lines.push(Spans::from(spans));
            }
        }

        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        let parser = Parser::new_ext(&self.contents, options);

        // TODO: if possible, render links as terminal hyperlinks: https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda
        let mut tags = Vec::new();
        let mut spans = Vec::new();
        let mut lines = Vec::new();
        let mut list_stack = Vec::new();

        let get_indent = |level: usize| {
            if level < 1 {
                String::new()
            } else {
                Self::INDENT.repeat(level - 1)
            }
        };

        let get_theme = |key: &str| -> Style { theme.map(|t| t.get(key)).unwrap_or_default() };
        let text_style = get_theme(Self::TEXT_STYLE);
        let code_style = get_theme(Self::BLOCK_STYLE);
        if (self.color_swatches || self.color_values)
            && parse_css_color(&self.contents, preview_background(theme)).is_some()
        {
            let mut text = styled_multiline_text(&self.contents, text_style);
            color_code_block(
                &mut text,
                "plaintext",
                theme,
                self.color_swatches,
                self.color_values,
            );
            return text;
        }
        let numbered_list_style = get_theme(Self::NUMBERED_LIST_STYLE);
        let unnumbered_list_style = get_theme(Self::UNNUMBERED_LIST_STYLE);
        let rule_style = get_theme(Self::RULE_STYLE);
        let heading_styles: Vec<Style> = Self::HEADING_STYLES
            .iter()
            .map(|key| get_theme(key))
            .collect();

        // Transform text in `<code>` blocks into `Event::Code`
        let mut in_code = false;
        let parser = parser.filter_map(|event| match event {
            Event::Html(tag)
                if tag.starts_with("<code") && matches!(tag.chars().nth(5), Some(' ' | '>')) =>
            {
                in_code = true;
                None
            }
            Event::Html(tag) if *tag == *"</code>" => {
                in_code = false;
                None
            }
            Event::Text(text) if in_code => Some(Event::Code(text)),
            _ => Some(event),
        });

        for event in parser {
            match event {
                Event::Start(Tag::List(list)) => {
                    // if the list stack is not empty this is a sub list, in that
                    // case we need to push the current line before proceeding
                    if !list_stack.is_empty() {
                        push_line(&mut spans, &mut lines);
                    }

                    list_stack.push(list);
                }
                Event::End(TagEnd::List(_)) => {
                    list_stack.pop();

                    // whenever top-level list closes, empty line
                    if list_stack.is_empty() {
                        lines.push(Spans::default());
                    }
                }
                Event::Start(Tag::Item) => {
                    if list_stack.is_empty() {
                        log::warn!("markdown parsing error, list item without list");
                    }

                    tags.push(Tag::Item);

                    // get the appropriate bullet for the current list
                    let (bullet, bullet_style) = list_stack
                        .last()
                        .unwrap_or(&None) // use the '- ' bullet in case the list stack would be empty
                        .map_or((String::from("• "), unnumbered_list_style), |number| {
                            (format!("{}. ", number), numbered_list_style)
                        });

                    // increment the current list number if there is one
                    if let Some(v) = list_stack.last_mut().unwrap_or(&mut None).as_mut() {
                        *v += 1;
                    }

                    let prefix = get_indent(list_stack.len()) + bullet.as_str();
                    spans.push(Span::styled(prefix, bullet_style));
                }
                Event::Start(tag) => {
                    tags.push(tag);
                    if spans.is_empty() && !list_stack.is_empty() {
                        // TODO: could push indent + 2 or 3 spaces to align with
                        // the rest of the list.
                        spans.push(Span::from(get_indent(list_stack.len())));
                    }
                }
                Event::End(tag) => {
                    tags.pop();
                    match tag {
                        TagEnd::Heading(_)
                        | TagEnd::Paragraph
                        | TagEnd::CodeBlock
                        | TagEnd::Item => {
                            push_line(&mut spans, &mut lines);
                        }
                        _ => (),
                    }

                    // whenever heading, code block or paragraph closes, empty line
                    match tag {
                        TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::CodeBlock => {
                            lines.push(Spans::default());
                        }
                        _ => (),
                    }
                }
                Event::Text(text) => {
                    if let Some(Tag::CodeBlock(kind)) = tags.last() {
                        let language = match kind {
                            CodeBlockKind::Fenced(language) => language,
                            CodeBlockKind::Indented => "",
                        };
                        let mut tui_text =
                            highlighted_code_block(&text, language, theme, loader, None);
                        if self.color_swatches || self.color_values {
                            color_code_block(
                                &mut tui_text,
                                language,
                                theme,
                                self.color_swatches,
                                self.color_values,
                            );
                        }
                        lines.extend(tui_text.lines);
                    } else {
                        let style = match tags.last() {
                            Some(Tag::Heading { level, .. }) => match level {
                                HeadingLevel::H1 => heading_styles[0],
                                HeadingLevel::H2 => heading_styles[1],
                                HeadingLevel::H3 => heading_styles[2],
                                HeadingLevel::H4 => heading_styles[3],
                                HeadingLevel::H5 => heading_styles[4],
                                HeadingLevel::H6 => heading_styles[5],
                            },
                            Some(Tag::Emphasis) => text_style.add_modifier(Modifier::ITALIC),
                            Some(Tag::Strong) => text_style.add_modifier(Modifier::BOLD),
                            Some(Tag::Strikethrough) => {
                                text_style.add_modifier(Modifier::CROSSED_OUT)
                            }
                            _ => text_style,
                        };
                        spans.push(Span::styled(text, style));
                    }
                }
                Event::Code(text) => {
                    let color = (self.color_swatches || self.color_values)
                        .then(|| parse_css_color(&text, preview_background(theme)))
                        .flatten();
                    if let Some(color) = color.filter(|_| self.color_swatches) {
                        spans.push(Span::styled("■ ", code_style.fg(color)));
                    }
                    let style = color
                        .filter(|_| self.color_values)
                        .map_or(code_style, |color| code_style.fg(color));
                    spans.push(Span::styled(text, style));
                }
                Event::Html(text) => {
                    spans.push(Span::styled(text, code_style));
                }
                Event::SoftBreak | Event::HardBreak => {
                    push_line(&mut spans, &mut lines);
                    if !list_stack.is_empty() {
                        // TODO: could push indent + 2 or 3 spaces to align with
                        // the rest of the list.
                        spans.push(Span::from(get_indent(list_stack.len())));
                    }
                }
                Event::Rule => {
                    lines.push(Spans::from(Span::styled("───", rule_style)));
                    lines.push(Spans::default());
                }
                // TaskListMarker(bool) true if checked
                _ => {
                    log::warn!("unhandled markdown event {:?}", event);
                }
            }
            // build up a vec of Paragraph tui widgets
        }

        if !spans.is_empty() {
            lines.push(Spans::from(spans));
        }

        // if last line is empty, remove it
        if let Some(line) = lines.last() {
            if line.0.is_empty() {
                lines.pop();
            }
        }

        Text::from(lines)
    }
}

impl Component for Markdown {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        use tui::widgets::{Paragraph, Widget, Wrap};

        let text = self.parse(Some(&cx.editor.theme));

        let par = Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .scroll((cx.scroll.unwrap_or_default() as u16, 0));

        let margin = Margin::all(1);
        par.render(area.inner(margin), surface);
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let padding = 2;
        // TODO: account for tab width
        let max_text_width = (viewport.0.saturating_sub(padding)).min(120);
        let (width, height) = self.dimensions(max_text_width);

        Some((width + padding, height + padding))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn markdown(contents: &str) -> Markdown {
        let loader = syntax::Loader::new(syntax::config::Configuration {
            language: Vec::new(),
            language_server: Default::default(),
        })
        .unwrap();
        Markdown::new(contents.to_owned(), Arc::new(ArcSwap::from_pointee(loader)))
    }

    fn visible(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .map(|line| {
                line.0
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn colors_are_opt_in_and_preferences_are_independent() {
        let source = "```css\n.x { color: #f00; }\n```";
        let disabled_markdown = markdown(source);
        let disabled = disabled_markdown.parse(None);
        assert_eq!(visible(&disabled), ".x { color: #f00; }");
        for (swatches, values) in [(false, false), (true, false), (false, true), (true, true)] {
            let markdown = markdown(source).with_color_previews(swatches, values);
            let text = markdown.parse(None);
            assert_eq!(visible(&text).contains('■'), swatches);
            let value = text.lines[0].0.iter().find(|span| span.content == "#f00");
            if swatches || values {
                assert_eq!(
                    value.unwrap().style.fg,
                    values.then_some(Color::Rgb(255, 0, 0))
                );
            }
        }
    }

    #[test]
    fn tailwind_hover_comments_and_named_values() {
        let markdown = markdown("```css\n.bg-red-500 { background-color: var(--color-red-500) /* oklch(63.7% 0.237 25.331) = #fb2c36 */; color: rebeccapurple; }\n```")
            .with_color_previews(true, true);
        let text = markdown.parse(None);
        assert_eq!(visible(&text).matches('■').count(), 3);
        assert!(visible(&text).contains("/* ■ oklch(63.7% 0.237 25.331) = ■ #fb2c36 */"));
        assert!(text.lines[0].0.iter().any(
            |span| span.content == "#fb2c36" && span.style.fg == Some(Color::Rgb(251, 44, 54))
        ));
    }

    #[test]
    fn exact_inline_plaintext_and_bare_colors() {
        for source in [
            "`#abc`",
            "```plaintext\n#abc\n```",
            "```\n#abc\n```",
            "#abc",
            "red",
            "`oklab(0.5 0.1 0.1)`",
        ] {
            let markdown = markdown(source).with_color_previews(true, true);
            assert_eq!(
                visible(&markdown.parse(None)).matches('■').count(),
                1,
                "{source}"
            );
        }
        for source in [
            "plain #abc prose",
            "`color: #abc`",
            "```plaintext\ncolor: #abc;\n```",
            "```rust\nlet red = \"#abc\";\n```",
            "`bad`",
            "`var(--red)`",
        ] {
            let markdown = markdown(source).with_color_previews(true, true);
            assert!(!visible(&markdown.parse(None)).contains('■'), "{source}");
        }
    }

    #[test]
    fn decorations_preserve_split_syntax_styles_and_unicode() {
        let style = Style::default()
            .bg(Color::Rgb(1, 2, 3))
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD)
            .underline_color(Color::Green);
        let second = style.add_modifier(Modifier::ITALIC);
        let mut text = Text::from(Spans::from(vec![
            Span::styled(".é { color: ", style),
            Span::styled("#", style),
            Span::styled("ff00", second),
            Span::styled("00; }", style),
        ]));
        let source = visible(&text);
        let colors = css_color_ranges(&source, None);
        decorate_colors(&mut text, &colors, true, true);
        assert_eq!(visible(&text), ".é { color: ■ #ff0000; }");
        assert_eq!(text.lines[0].0[0].style, style);
        assert!(text.lines[0]
            .0
            .iter()
            .any(|span| span.content == "ff00" && span.style == second.fg(Color::Rgb(255, 0, 0))));
        assert_eq!(text.lines[0].0.last().unwrap().style, style);
    }

    #[test]
    fn multiline_values_and_tabs_have_the_same_measured_layout() {
        let markdown =
            markdown("```css\n.é {\n\tcolor: rgb(\n\t\t255 0 0\n\t);\n\tbackground: #00f;\n}\n```")
                .with_color_previews(true, true);
        let theme = Theme::default();
        let measured = markdown.parse(None);
        let rendered = markdown.parse(Some(&theme));
        assert_eq!(visible(&measured), visible(&rendered));
        assert_eq!(visible(&rendered).matches('■').count(), 2);
        assert!(visible(&rendered).contains("    color: ■ rgb(\n        255 0 0\n    );"));
        assert!(rendered.lines[2]
            .0
            .iter()
            .any(|span| span.content == "        255 0 0"
                && span.style.fg == Some(Color::Rgb(255, 0, 0))));
        assert_eq!(measured.width(), rendered.width());
    }

    #[test]
    fn alpha_uses_popup_then_editor_background() {
        let theme: Theme = toml::from_str(
            "\"ui.popup\" = { bg = \"#0000ff\" }\n\"ui.background\" = { bg = \"#00ff00\" }",
        )
        .unwrap();
        let markdown = markdown("`rgb(255 0 0 / 50%)`").with_color_previews(true, true);
        let text = markdown.parse(Some(&theme));
        assert!(text.lines[0]
            .0
            .iter()
            .all(|span| span.style.fg == Some(Color::Rgb(128, 0, 128))));
        let background: Theme = toml::from_str("\"ui.background\" = { bg = \"#00ff00\" }").unwrap();
        assert_eq!(
            preview_background(Some(&background)),
            Some(Color::Rgb(0, 255, 0))
        );
        let indexed_popup: Theme = toml::from_str(
            "\"ui.popup\" = { bg = \"blue\" }\n\"ui.background\" = { bg = \"#00ff00\" }",
        )
        .unwrap();
        assert_eq!(
            preview_background(Some(&indexed_popup)),
            Some(Color::Rgb(0, 255, 0))
        );
    }

    #[test]
    fn parsed_text_and_dimensions_are_reused_across_frames() {
        let markdown = markdown("**αβ** `#f00`\n\n```css\n.x { color: #0f0; }\n```")
            .with_color_previews(true, true);
        let theme = Theme::default();
        let first_layout = markdown.parse(None);
        let first_render = markdown.parse(Some(&theme));
        assert!(Arc::ptr_eq(&first_layout, &markdown.parse(None)));
        assert!(Arc::ptr_eq(&first_render, &markdown.parse(Some(&theme))));
        assert_eq!(visible(&first_layout), visible(&first_render));
        let wide = markdown.dimensions(120);
        assert_eq!(wide, markdown.dimensions(120));
        assert_eq!(markdown.measured_size.get(), Some((120, wide)));
        let narrow = markdown.dimensions(4);
        assert!(narrow.1 > wide.1);
        assert_eq!(markdown.measured_size.get(), Some((4, narrow)));
    }

    #[test]
    fn cached_styles_invalidate_for_theme_loader_and_scope_changes() {
        let markdown = markdown("`rgb(255 0 0 / 50%)`").with_color_previews(true, true);
        let blue: Theme = toml::from_str("\"ui.popup\" = { bg = \"#0000ff\" }").unwrap();
        let green: Theme = toml::from_str("\"ui.popup\" = { bg = \"#00ff00\" }").unwrap();
        let first = markdown.parse(Some(&blue));
        assert!(Arc::ptr_eq(&first, &markdown.parse(Some(&blue.clone()))));
        let changed = markdown.parse(Some(&green));
        assert!(!Arc::ptr_eq(&first, &changed));
        assert_eq!(first.lines[0].0[0].style.fg, Some(Color::Rgb(128, 0, 128)));
        assert_eq!(
            changed.lines[0].0[0].style.fg,
            Some(Color::Rgb(128, 128, 0))
        );
        markdown
            .config_loader
            .load()
            .set_scopes(vec!["ui.text".into()]);
        let scopes_changed = markdown.parse(Some(&green));
        assert!(!Arc::ptr_eq(&changed, &scopes_changed));
        markdown
            .config_loader
            .store(Arc::new(syntax::Loader::default()));
        assert!(!Arc::ptr_eq(&scopes_changed, &markdown.parse(Some(&green))));
    }

    #[test]
    fn changing_color_options_invalidates_layout_and_rendered_caches() {
        let markdown = markdown("`#f00`").with_color_previews(true, true);
        let theme = Theme::default();
        let old = markdown.parse(Some(&theme));
        let old_size = markdown.dimensions(120);
        let markdown = markdown.with_color_previews(false, false);
        let new = markdown.parse(Some(&theme));
        assert!(!Arc::ptr_eq(&old, &new));
        assert_eq!(visible(&new), "#f00");
        assert_eq!(markdown.dimensions(120).0 + 2, old_size.0);
    }
}
