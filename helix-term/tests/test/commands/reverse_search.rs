use std::cell::RefCell;

use helix_core::Selection;

use super::helpers::{test_config, test_key_sequence, AppBuilder};

async fn search_selection(
    text: &str,
    pattern: &str,
    count: usize,
    counted: bool,
    wrap_around: bool,
    extend: bool,
) -> anyhow::Result<Selection> {
    let mut config = test_config();
    config.editor.search.wrap_around = wrap_around;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_input_text(format!("#[|]#{text}"))
        .build()?;
    let keys = if counted {
        format!("/{pattern}<ret>ge{}{count}N", if extend { "v" } else { "" })
    } else {
        format!(
            "/{pattern}<ret>ge{}{}",
            if extend { "v" } else { "" },
            "N".repeat(count)
        )
    };
    let observed = RefCell::new(None);
    test_key_sequence(
        &mut app,
        Some(&keys),
        Some(&|app| {
            let (view, doc) = helix_view::current_ref!(app.editor);
            *observed.borrow_mut() = Some(doc.selection(view.id).clone());
        }),
        false,
    )
    .await?;
    Ok(observed.into_inner().unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn counted_reverse_search_matches_single_steps_with_wrap_greediness_and_unicode(
) -> anyhow::Result<()> {
    for (text, pattern, count, wrap, extend) in [
        ("needle x needle\nneedle\n", "needle", 12, true, false),
        ("aaaaa b aaaaaaaaa\n", "a{1,4}", 7, true, false),
        ("baa baab aaaaaa\n", "a*|b", 8, true, false),
        ("abc\r\ndef\nabc\n", "(?m)^|$", 12, false, false),
        (
            "αβ e\u{301} 🇵🇱 α e\u{301}\n",
            "α|e\\p{M}*|🇵🇱",
            12,
            true,
            true,
        ),
        ("aaaaa\n", "aa", 9, true, true),
    ] {
        let expected = search_selection(text, pattern, count, false, wrap, extend).await?;
        let actual = search_selection(text, pattern, count, true, wrap, extend).await?;
        assert_eq!(
            actual, expected,
            "{text:?}, {pattern:?}, wrap={wrap}, extend={extend}"
        );
    }
    Ok(())
}
