use std::{borrow::Cow, ops::Range};

use helix_view::graphics::Color;

fn color_function(name: &str) -> bool {
    [
        "rgb", "rgba", "hsl", "hsla", "hwb", "lab", "lch", "oklab", "oklch",
    ]
    .iter()
    .any(|function| name.eq_ignore_ascii_case(function))
}

/// Parse CSS colors, excluding the parser's non-CSS, unprefixed hex syntax.
/// Terminals cannot display alpha: composite over a known RGB background, or
/// show the uncomposited RGB when the terminal's background is unknown.
pub(crate) fn parse_css_color(value: &str, background: Option<Color>) -> Option<Color> {
    let value = value.trim();
    let css_syntax = value.starts_with('#')
        || value
            .split_once('(')
            .is_some_and(|(name, _)| color_function(name))
        || value.eq_ignore_ascii_case("transparent")
        || csscolorparser::NAMED_COLORS.get(value.into()).is_some();
    if !css_syntax {
        return None;
    }
    // csscolorparser's parameter lexer recognizes only literal spaces. CSS
    // also permits tabs, newlines, carriage returns, and form feeds between
    // parameters. Normalize only the parser input; rendered source ranges
    // continue to refer to the original text.
    let normalized = if value
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() && byte != b' ')
    {
        Cow::Owned(
            value
                .chars()
                .map(|c| if c.is_ascii_whitespace() { ' ' } else { c })
                .collect::<String>(),
        )
    } else {
        Cow::Borrowed(value)
    };
    let color = csscolorparser::parse(&normalized).ok()?.clamp();
    let [r, g, b, alpha] = color.to_array();
    let rgb = match background {
        Some(Color::Rgb(br, bg, bb)) => [
            r * alpha + br as f32 / 255.0 * (1.0 - alpha),
            g * alpha + bg as f32 / 255.0 * (1.0 - alpha),
            b * alpha + bb as f32 / 255.0 * (1.0 - alpha),
        ],
        _ => [r, g, b],
    };
    Some(Color::Rgb(
        (rgb[0] * 255.0).round() as u8,
        (rgb[1] * 255.0).round() as u8,
        (rgb[2] * 255.0).round() as u8,
    ))
}

fn identifier(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '\\') || !c.is_ascii()
}

fn word_end(text: &str, start: usize) -> usize {
    text[start..]
        .char_indices()
        .find(|(_, c)| !identifier(*c))
        .map_or(text.len(), |(offset, _)| start + offset)
}

fn quoted_end(text: &str, start: usize) -> usize {
    let quote = text.as_bytes()[start];
    let mut chars = text[start + 1..].char_indices();
    while let Some((offset, c)) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c as u32 == quote as u32 {
            return start + offset + 2;
        }
    }
    text.len()
}

fn comment_end(text: &str, start: usize) -> usize {
    text[start + 2..]
        .find("*/")
        .map_or(text.len(), |offset| start + offset + 4)
}

fn function_end(text: &str, open: usize) -> Option<usize> {
    let mut depth = 1;
    let mut pos = open + 1;
    while pos < text.len() {
        match text.as_bytes()[pos] {
            b'\'' | b'"' => pos = quoted_end(text, pos),
            b'/' if text[pos..].starts_with("/*") => pos = comment_end(text, pos),
            b'(' => {
                depth += 1;
                pos += 1;
            }
            b')' => {
                depth -= 1;
                pos += 1;
                if depth == 0 {
                    return Some(pos);
                }
            }
            _ => pos += text[pos..].chars().next().unwrap().len_utf8(),
        }
    }
    None
}

fn token_boundary(text: &str, pos: usize) -> bool {
    text[pos..].chars().next().is_none_or(|c| !identifier(c))
}

fn property_name(text: &str) -> Option<Cow<'_, str>> {
    let name = if text.contains("/*") {
        let mut name = String::with_capacity(text.len());
        let mut remaining = text;
        while let Some(start) = remaining.find("/*") {
            name.push_str(&remaining[..start]);
            name.push(' ');
            let end = remaining[start + 2..].find("*/")?;
            remaining = &remaining[start + end + 4..];
        }
        name.push_str(remaining);
        Cow::Owned(name)
    } else {
        Cow::Borrowed(text)
    };
    let mut chars = name.trim().chars();
    let first = chars.next()?;
    if (first.is_ascii_alphabetic() || matches!(first, '-' | '_' | '$' | '@'))
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        Some(name)
    } else {
        None
    }
}

fn named_color_property(property: &str) -> bool {
    let name = property.trim().to_ascii_lowercase();
    if name.starts_with("--") || name.starts_with(['$', '@']) || name.ends_with("-color") {
        return true;
    }
    let name = ["-webkit-", "-moz-", "-ms-", "-o-"]
        .iter()
        .find_map(|prefix| name.strip_prefix(*prefix))
        .unwrap_or(&name);
    matches!(
        name,
        "color"
            | "background"
            | "background-image"
            | "border"
            | "border-top"
            | "border-right"
            | "border-bottom"
            | "border-left"
            | "border-block"
            | "border-block-start"
            | "border-block-end"
            | "border-inline"
            | "border-inline-start"
            | "border-inline-end"
            | "border-image"
            | "border-image-source"
            | "outline"
            | "fill"
            | "stroke"
            | "text-stroke"
            | "box-shadow"
            | "text-shadow"
            | "text-decoration"
            | "text-emphasis"
            | "column-rule"
            | "filter"
            | "backdrop-filter"
            | "mask"
            | "mask-image"
            | "list-style"
            | "list-style-image"
    )
}

// A bare element selector can resemble a declaration (e.g. `red:blue`).
// An opening rule brace before a value terminator identifies that context.
fn declaration_value(text: &str, mut pos: usize) -> bool {
    while pos < text.len() {
        match text.as_bytes()[pos] {
            b'\'' | b'"' => pos = quoted_end(text, pos),
            b'/' if text[pos..].starts_with("/*") => pos = comment_end(text, pos),
            b'(' => match function_end(text, pos) {
                Some(end) => pos = end,
                None => return false,
            },
            b'{' => return false,
            b';' | b'}' => return true,
            _ => pos += text[pos..].chars().next().unwrap().len_utf8(),
        }
    }
    true
}

fn comment_colors(
    text: &str,
    start: usize,
    end: usize,
    background: Option<Color>,
    colors: &mut Vec<(Range<usize>, Color)>,
) {
    let mut pos = start;
    while pos < end {
        let c = text[pos..].chars().next().unwrap();
        let before = text[..pos].chars().next_back();
        if c == '#' && before.is_none_or(|c| !identifier(c) && c != '#') {
            let next = word_end(text, pos + 1).min(end);
            if let Some(color) = parse_css_color(&text[pos..next], background) {
                colors.push((pos..next, color));
            }
            pos = next;
        } else if identifier(c) {
            let next = word_end(text, pos);
            if next < end && text.as_bytes()[next] == b'(' && color_function(&text[pos..next]) {
                if let Some(close) = function_end(text, next).filter(|&close| close <= end) {
                    if token_boundary(text, close) {
                        if let Some(color) = parse_css_color(&text[pos..close], background) {
                            colors.push((pos..close, color));
                        }
                    }
                    pos = close;
                    continue;
                }
            }
            pos = next;
        } else {
            pos += c.len_utf8();
        }
    }
}

fn hidden_comments(
    text: &str,
    mut pos: usize,
    end: usize,
    background: Option<Color>,
    colors: &mut Vec<(Range<usize>, Color)>,
) {
    while pos < end {
        match text.as_bytes()[pos] {
            b'\'' | b'"' => pos = quoted_end(text, pos),
            b'/' if text[pos..].starts_with("/*") => {
                let close = comment_end(text, pos).min(end);
                comment_colors(text, pos + 2, close, background, colors);
                pos = close;
            }
            _ => pos += text[pos..].chars().next().unwrap().len_utf8(),
        }
    }
}

/// Find literal colors in CSS declaration values and explicit color literals
/// in comments (including the resolved colors supplied by Tailwind's server).
/// Strings, selectors, URLs, and unresolved variable expressions stay untouched.
pub(crate) fn css_color_ranges(
    text: &str,
    background: Option<Color>,
) -> Vec<(Range<usize>, Color)> {
    let mut colors = Vec::new();
    let mut pos = 0;
    let mut statement_start = 0;
    let mut in_value = false;
    let mut named_colors = false;
    let mut parens = 0usize;
    while pos < text.len() {
        let c = text[pos..].chars().next().unwrap();
        match c {
            '\'' | '"' => pos = quoted_end(text, pos),
            '/' if text[pos..].starts_with("/*") => {
                let end = comment_end(text, pos);
                comment_colors(text, pos + 2, end, background, &mut colors);
                if text[statement_start..pos].trim().is_empty() {
                    statement_start = end;
                }
                pos = end;
            }
            '{' | '}' | ';' if parens == 0 => {
                in_value = false;
                named_colors = false;
                statement_start = pos + 1;
                pos += 1;
            }
            '\n' if parens == 0 => {
                statement_start = pos + 1;
                pos += 1;
            }
            ':' if parens == 0 => {
                let property = property_name(&text[statement_start..pos]);
                in_value = property.is_some() && declaration_value(text, pos + 1);
                named_colors = in_value && property.is_some_and(|name| named_color_property(&name));
                pos += 1;
            }
            '#' if in_value => {
                let before = text[..pos].chars().next_back();
                let end = word_end(text, pos + 1);
                if before.is_none_or(|c| !identifier(c) && c != '#') {
                    if let Some(color) = parse_css_color(&text[pos..end], background) {
                        colors.push((pos..end, color));
                    }
                }
                pos = end;
            }
            c if identifier(c) => {
                let end = word_end(text, pos);
                let name = &text[pos..end];
                if text.as_bytes().get(end) == Some(&b'(') {
                    if ["url", "var", "env", "attr"]
                        .iter()
                        .any(|function| name.eq_ignore_ascii_case(function))
                    {
                        let close = function_end(text, end).unwrap_or(text.len());
                        if !name.eq_ignore_ascii_case("url") {
                            hidden_comments(text, end + 1, close, background, &mut colors);
                        }
                        pos = close;
                        continue;
                    }
                    if in_value && color_function(name) {
                        if let Some(close) = function_end(text, end) {
                            if token_boundary(text, close) {
                                if let Some(color) = parse_css_color(&text[pos..close], background)
                                {
                                    colors.push((pos..close, color));
                                }
                            }
                            pos = close;
                            continue;
                        }
                    }
                } else if in_value && named_colors {
                    if let Some(color) = parse_css_color(name, background) {
                        colors.push((pos..end, color));
                    }
                }
                pos = end;
            }
            '(' => {
                parens += 1;
                pos += 1;
            }
            ')' => {
                parens = parens.saturating_sub(1);
                pos += 1;
            }
            _ => pos += c.len_utf8(),
        }
    }
    colors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(source: &str) -> Vec<&str> {
        css_color_ranges(source, None)
            .into_iter()
            .map(|(range, _)| &source[range])
            .collect()
    }

    #[test]
    fn standard_colors_and_alpha() {
        assert_eq!(
            parse_css_color("#abc", None),
            Some(Color::Rgb(170, 187, 204))
        );
        assert_eq!(
            parse_css_color("rgb(100% 0% 0%)", None),
            Some(Color::Rgb(255, 0, 0))
        );
        assert_eq!(
            parse_css_color("hsl(.5turn 100% 50%)", None),
            Some(Color::Rgb(0, 255, 255))
        );
        assert_eq!(
            parse_css_color("ReBeccaPurple", None),
            Some(Color::Rgb(102, 51, 153))
        );
        assert_eq!(
            parse_css_color("rgb(255 0 0 / 50%)", Some(Color::Rgb(0, 0, 255))),
            Some(Color::Rgb(128, 0, 128))
        );
        for color in [
            "oklch(63.7% 0.237 25.331)",
            "oklab(0.5 0.1 0.1)",
            "lab(50% 20 30)",
            "lch(50% 40 30)",
            "hwb(0 0% 0%)",
        ] {
            assert!(parse_css_color(color, None).is_some(), "{color}");
        }
        for value in [
            "bad",
            "beef",
            "ff0000",
            "currentColor",
            "var(--red)",
            "#abcde",
            "rgb(1 2)",
            "red blue",
            "hsv(0 1 1)",
        ] {
            assert_eq!(parse_css_color(value, None), None, "{value}");
        }
    }

    #[test]
    fn tailwind_resolved_comments_and_declarations() {
        let source = ".bg-red-500 { background-color: var(--color-red-500) /* oklch(63.7% 0.237 25.331) = #fb2c36 */; }\n.text-blue-600\\/50 { color: color-mix(in oklab, var(--color-blue-600) /* oklch(54.6% 0.245 262.881) = #155dfc */ 50%, transparent); }";
        assert_eq!(
            values(source),
            [
                "oklch(63.7% 0.237 25.331)",
                "#fb2c36",
                "oklch(54.6% 0.245 262.881)",
                "#155dfc",
                "transparent"
            ]
        );
        assert_eq!(values(".x { color: var(--red /* #f00 */); background: linear-gradient(red, rgb(0 0 255)); }"), ["#f00", "red", "rgb(0 0 255)"]);
    }

    #[test]
    fn selectors_strings_urls_and_identifiers_are_not_colors() {
        let source = "#abc, red:blue { content: '#fff red'; background: url(#abc); color: var(--red, #f00); --red-500: currentColor; border: red-500; animation: bad; color: #ff0000foo; }";
        assert!(values(source).is_empty());
        assert!(values("red:blue\n{ content: red-500; }").is_empty());
        assert!(values(".x { color: var(--red, #f00").is_empty());
        assert_eq!(
            values("#abc:hover { color: red; /* red is a keyword; #abcd is explicit */ }"),
            ["red", "#abcd"]
        );
    }

    #[test]
    fn unicode_and_multiline_values() {
        let source = ".é {\n --color: oklch(\n63.7% 0.237 25.331\n);\n color: #abc;\n}";
        assert_eq!(values(source), ["oklch(\n63.7% 0.237 25.331\n)", "#abc"]);
        assert_eq!(
            values(".x { content: \"escaped \\\" #abc\"; color: blue; }"),
            ["blue"]
        );
    }

    #[test]
    fn css_whitespace_is_normalized_without_changing_source_ranges() {
        let value = "rgb(\r255\t0\x0c0\n/\t50%)";
        assert_eq!(
            parse_css_color(value, Some(Color::Rgb(0, 0, 255))),
            Some(Color::Rgb(128, 0, 128))
        );
        let source = format!(".é {{ color: {value}; background: #abc; }}");
        assert_eq!(values(&source), [value, "#abc"]);
        assert_eq!(parse_css_color("rgb(1\n2)", None), None);
        assert_eq!(parse_css_color("#ab\nc", None), None);
    }

    #[test]
    fn named_colors_require_color_properties() {
        let source = ".x { animation-name: red; font-family: blue; border-style: red; background: red; border: 1px solid blue; outline-color: green; --custom: purple; fill: orange; box-shadow: 0 0 2px teal; background-image: linear-gradient(red, blue); filter: drop-shadow(0 0 2px navy); }";
        assert_eq!(
            values(source),
            ["red", "blue", "green", "purple", "orange", "teal", "red", "blue", "navy"]
        );
        // Explicit color syntax remains useful even for unfamiliar properties.
        assert_eq!(
            values(".x { extension: #f00 rgb(0 0 255); }"),
            ["#f00", "rgb(0 0 255)"]
        );
    }

    #[test]
    fn comments_separate_property_tokens() {
        assert_eq!(
            values(".x { color/*comment*/: #f00; background/**/: blue; co/**/lor: #0f0; }"),
            ["#f00", "blue"]
        );
    }
}
