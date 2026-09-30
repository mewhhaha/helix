use helix_core::{
    syntax::{config::Configuration, Loader, Syntax},
    textobject::{textobject_treesitter, TextObject},
    Range, Rope, Transaction,
};

fn reference(
    text: &Rope,
    range: Range,
    object: &str,
    kind: TextObject,
    syntax: &Syntax,
    loader: &Loader,
) -> Range {
    let slice = text.slice(..);
    let byte = slice.char_to_byte(range.cursor(slice));
    let layer = syntax.layer_for_byte_range(byte as u32, byte as u32);
    let root = syntax
        .tree_for_byte_range(byte as u32, byte as u32)
        .root_node();
    let expected = (|| {
        let query = loader.textobject_query(syntax.layer(layer).language)?;
        let capture = format!("{object}.{kind}");
        let node = query
            .capture_nodes(&capture, &root, slice)?
            .filter(|node| node.byte_range().contains(&byte))
            .min_by_key(|node| node.byte_range().len())?;
        if node.start_byte() >= slice.len_bytes() || node.end_byte() >= slice.len_bytes() {
            return None;
        }
        Some(Range::new(
            slice.byte_to_char(node.start_byte()),
            slice.byte_to_char(node.end_byte()),
        ))
    })();
    expected.unwrap_or(range)
}

fn compare(text: &Rope, syntax: &Syntax, loader: &Loader) {
    for object in ["function", "class", "parameter", "comment", "entry"] {
        for kind in [TextObject::Inside, TextObject::Around] {
            // Repeat queries to exercise the interval cache, including groups
            // of comment nodes with a cursor in the gap between them.
            for _ in 0..2 {
                for pos in (0..=text.len_chars()).step_by(7) {
                    let range = Range::point(pos);
                    let expected = reference(text, range, object, kind, syntax, loader);
                    let actual = textobject_treesitter(
                        text.slice(..),
                        range,
                        kind,
                        object,
                        syntax,
                        loader,
                        1,
                    );
                    assert_eq!(actual, expected, "{object}.{kind} at {pos}");
                }
            }
        }
    }
}

#[test]
fn cached_text_objects_match_full_queries_for_nested_unicode_and_reparsed_source() {
    let config: Configuration = toml::from_str(
        r#"
[[language]]
name = "javascript"
scope = "source.js"
file-types = ["js"]
[[language]]
name = "css"
scope = "source.css"
injection-regex = "css"
file-types = ["css"]
"#,
    )
    .unwrap();
    let loader = Loader::new(config).unwrap();
    let language = loader.language_for_name("javascript").unwrap();
    let mut text = Rope::from_str(
        "// 界 first\n// second\nclass Example { method(a, b) { function inner(x) { return x; } return inner(a); } }\nconst style = css`.界 { color: red; }`;\n",
    );
    let mut syntax = Syntax::new(text.slice(..), language, &loader).unwrap();
    compare(&text, &syntax, &loader);
    let old = text.clone();
    let transaction = Transaction::change(
        &old,
        [(0, 0, Some("function added() { return '😀'; }\n".into()))].into_iter(),
    );
    assert!(transaction.apply(&mut text));
    syntax
        .update(
            old.slice(..),
            text.slice(..),
            transaction.changes(),
            &loader,
        )
        .unwrap();
    compare(&text, &syntax, &loader);
}
