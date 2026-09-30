use helix_core::{
    syntax::{config::Configuration, Loader, Syntax},
    Rope, Transaction,
};
use std::ops::Range;

fn compare(syntax: &Syntax, text: &Rope, loader: &Loader) {
    compare_range(syntax, text, loader, 0..text.len_bytes() as u32);
}

fn compare_range(syntax: &Syntax, text: &Rope, loader: &Loader, range: Range<u32>) {
    // Streaming records the traversed boundaries; later passes reuse them.
    for _ in 0..2 {
        let mut display = syntax.display_highlighter(text.slice(..), loader, range.clone());
        let mut raw = syntax.highlighter(text.slice(..), loader, range.clone());
        let mut positions: Vec<_> = (text.byte_to_char(range.start as usize)
            ..text.byte_to_char(range.end as usize))
            .step_by(37)
            .map(|index| text.char_to_byte(index) as u32)
            .collect();
        positions.push(range.end);
        for byte in positions {
            while raw.next_event_offset() <= byte && raw.next_event_offset() != u32::MAX {
                raw.advance();
            }
            let expected: Vec<_> = raw.active_highlights().collect();
            assert_eq!(display.seek_to(byte), expected, "byte={byte}");
            assert!(
                display.next_event_offset() > byte,
                "a seek consumes all events at its boundary"
            );
        }
    }
}

#[test]
fn partial_cached_seeks_preserve_injected_languages_and_viewport_boundaries() {
    let config: Configuration = toml::from_str(
        r#"
[[language]]
name = "javascript"
scope = "source.js"
file-types = ["js"]
[[language]]
name = "regex"
scope = "source.regex"
injection-regex = "regex"
file-types = ["regex"]
[[language]]
name = "css"
scope = "source.css"
injection-regex = "css"
file-types = ["css"]
"#,
    )
    .unwrap();
    let loader = Loader::new(config).unwrap();
    loader.set_scopes(vec![
        "string".into(),
        "constant.numeric".into(),
        "keyword".into(),
        "operator".into(),
        "punctuation".into(),
        "function".into(),
    ]);
    let language = loader.language_for_name("javascript").unwrap();
    let text = Rope::from_str(
        &"const pattern = /(?<界>[a-z]+|\\d{2})/gu;\nconst style = css`.界 { color: #aabbcc; width: 12px }`;\n".repeat(250),
    );
    let syntax = Syntax::new(text.slice(..), language, &loader).unwrap();
    for fragment in ["[a-z]", "#aabbcc"] {
        let byte = text.to_string().find(fragment).unwrap() as u32;
        assert!(syntax.layers_for_byte_range(byte, byte + 1).count() > 1);
    }
    let full = 0..text.len_bytes() as u32;
    // A short first viewport must leave a partial cache. A later viewport can
    // cross its edge while preserving the complete injection highlight stack.
    {
        let mut display = syntax.display_highlighter(text.slice(..), &loader, full.clone());
        display.seek_to(text.char_to_byte(100) as u32);
    }
    compare(&syntax, &text, &loader);
    let start = text.line_to_byte(40) as u32;
    let end = text.line_to_byte(230) as u32;
    assert!(end - start >= 8192);
    compare_range(&syntax, &text, &loader, start..end);
}

#[test]
fn cached_syntax_seeks_preserve_nested_unicode_highlights_and_invalidation() {
    let config: Configuration = toml::from_str(
        r#"
[[language]]
name = "json"
scope = "source.json"
file-types = ["json"]
"#,
    )
    .unwrap();
    let loader = Loader::new(config).unwrap();
    loader.set_scopes(vec![
        "constant.numeric".into(),
        "string".into(),
        "punctuation.bracket".into(),
        "punctuation.delimiter".into(),
    ]);
    let language = loader.language_for_name("json").unwrap();
    let mut text = Rope::from_str(&format!("[{}null]", "{\"界é\": [1, 2, 3]},".repeat(600)));
    let mut syntax = Syntax::new(text.slice(..), language, &loader).unwrap();
    compare(&syntax, &text, &loader);
    loader.set_scopes(vec![
        "punctuation".into(),
        "string".into(),
        "constant".into(),
    ]);
    compare(&syntax, &text, &loader);
    let old = text.clone();
    let transaction =
        Transaction::change(&old, [(1, 1, Some("\"replacement\",".into()))].into_iter());
    assert!(transaction.apply(&mut text));
    syntax
        .update(
            old.slice(..),
            text.slice(..),
            transaction.changes(),
            &loader,
        )
        .unwrap();
    compare(&syntax, &text, &loader);
}
