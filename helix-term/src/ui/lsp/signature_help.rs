use std::{
    cell::{Cell, OnceCell, RefCell},
    sync::Arc,
};

use arc_swap::ArcSwap;
use helix_core::syntax::{self, OverlayHighlights};
use helix_view::graphics::{Margin, Rect, Style};
use helix_view::input::Event;
use helix_view::Theme;
use tui::buffer::Buffer;
use tui::layout::Alignment;
use tui::text::Text;
use tui::widgets::{BorderType, Paragraph, Widget, Wrap};

use crate::compositor::{Component, Compositor, Context, EventResult};

use crate::alt;
use crate::ui::Markdown;

use crate::ui::Popup;

pub struct Signature {
    pub signature: String,
    pub signature_doc: Option<String>,
    /// Part of signature text
    pub active_param_range: Option<(usize, usize)>,
}

pub struct SignatureHelp {
    language: String,
    config_loader: Arc<ArcSwap<syntax::Loader>>,
    active_signature: usize,
    lsp_signature: Option<usize>,
    signatures: Vec<Signature>,
    prepared: Vec<OnceCell<PreparedSignature>>,
}

struct PreparedSignature {
    layout: Arc<Text<'static>>,
    document: Option<Markdown>,
    measured: Cell<Option<(u16, (u16, u16))>>,
    styled: RefCell<Option<StyledSignature>>,
}

struct StyledSignature {
    theme: usize,
    loader: Arc<syntax::Loader>,
    scopes: Arc<Vec<String>>,
    active_param: Option<(usize, usize)>,
    text: Arc<Text<'static>>,
}

impl PreparedSignature {
    fn dimensions(&self, width: u16) -> (u16, u16) {
        if let Some((cached_width, size)) = self.measured.get() {
            if cached_width == width {
                return size;
            }
        }
        let size = Paragraph::new(&self.layout)
            .wrap(Wrap { trim: false })
            .required_size(width);
        self.measured.set(Some((width, size)));
        size
    }
}

impl SignatureHelp {
    pub const ID: &'static str = "signature-help";

    pub fn new(
        language: String,
        config_loader: Arc<ArcSwap<syntax::Loader>>,
        active_signature: usize,
        lsp_signature: Option<usize>,
        signatures: Vec<Signature>,
    ) -> Self {
        Self {
            language,
            config_loader,
            active_signature,
            lsp_signature,
            prepared: (0..signatures.len()).map(|_| OnceCell::new()).collect(),
            signatures,
        }
    }

    pub fn active_signature(&self) -> usize {
        self.active_signature
    }

    pub fn lsp_signature(&self) -> Option<usize> {
        self.lsp_signature
    }

    pub fn visible_popup(compositor: &mut Compositor) -> Option<&mut Popup<Self>> {
        compositor.find_id::<Popup<Self>>(Self::ID)
    }

    fn signature_index(&self) -> String {
        format!("({}/{})", self.active_signature + 1, self.signatures.len())
    }

    fn current_signature(&self) -> (&Signature, &PreparedSignature) {
        let index = if self.active_signature < self.signatures.len() {
            self.active_signature
        } else {
            0
        };
        let signature = &self.signatures[index];
        let prepared = self.prepared[index].get_or_init(|| PreparedSignature {
            layout: Arc::new(crate::ui::markdown::highlighted_code_block(
                &signature.signature,
                &self.language,
                None,
                &self.config_loader.load(),
                None,
            )),
            document: signature
                .signature_doc
                .as_ref()
                .map(|doc| Markdown::new(doc.clone(), self.config_loader.clone())),
            measured: Cell::new(None),
            styled: RefCell::new(None),
        });
        (signature, prepared)
    }

    fn rendered_signature(&self, theme: &Theme) -> Arc<Text<'static>> {
        let (signature, prepared) = self.current_signature();
        let loader = self.config_loader.load_full();
        let scopes = Arc::clone(&loader.scopes());
        if let Some(cached) = prepared.styled.borrow().as_ref() {
            if cached.theme == theme.cache_key()
                && cached.active_param == signature.active_param_range
                && Arc::ptr_eq(&cached.loader, &loader)
                && Arc::ptr_eq(&cached.scopes, &scopes)
            {
                return cached.text.clone();
            }
        }
        let active_param = signature.active_param_range.map(|(start, end)| {
            let highlight = theme.find_highlight_exact("ui.selection").unwrap();
            OverlayHighlights::single(highlight, start..end)
        });
        let text = Arc::new(crate::ui::markdown::highlighted_code_block(
            &signature.signature,
            &self.language,
            Some(theme),
            &loader,
            active_param,
        ));
        *prepared.styled.borrow_mut() = Some(StyledSignature {
            theme: theme.cache_key(),
            loader,
            scopes,
            active_param: signature.active_param_range,
            text: text.clone(),
        });
        text
    }
}

impl Component for SignatureHelp {
    fn handle_event(&mut self, event: &Event, _cx: &mut Context) -> EventResult {
        let Event::Key(event) = event else {
            return EventResult::Ignored(None);
        };

        if self.signatures.len() <= 1 {
            return EventResult::Ignored(None);
        }

        match event {
            alt!('p') => {
                self.active_signature = self
                    .active_signature
                    .checked_sub(1)
                    .unwrap_or(self.signatures.len() - 1);
                EventResult::Consumed(None)
            }
            alt!('n') => {
                self.active_signature = (self.active_signature + 1) % self.signatures.len();
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored(None),
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Buffer, cx: &mut Context) {
        let margin = Margin::all(1);
        let area = area.inner(margin);

        let (_, prepared) = self.current_signature();
        let sig_text = self.rendered_signature(&cx.editor.theme);

        if self.signatures.len() > 1 {
            let signature_index = self.signature_index();
            let text = Text::from(signature_index);
            let paragraph = Paragraph::new(&text).alignment(Alignment::Right);
            paragraph.render(area.with_height(1).clip_right(1), surface);
        }

        let sig_text_para = Paragraph::new(&sig_text)
            .wrap(Wrap { trim: false })
            .scroll((cx.scroll.unwrap_or_default() as u16, 0));
        let (_, sig_text_height) = prepared.dimensions(area.width);
        let sig_text_area = area.with_height(sig_text_height.min(area.height));
        let sig_text_area = sig_text_area.intersection(surface.area);
        sig_text_para.render(sig_text_area, surface);

        let Some(document) = &prepared.document else {
            return;
        };

        let sep_style = Style::default();
        let borders = BorderType::line_symbols(BorderType::Plain);
        for x in sig_text_area.left()..sig_text_area.right() {
            if let Some(cell) = surface.get_mut(x, sig_text_area.bottom()) {
                cell.set_symbol(borders.horizontal).set_style(sep_style);
            }
        }

        let sig_doc = document.parse(Some(&cx.editor.theme));
        let sig_doc_area = area
            .clip_top(sig_text_area.height + 2)
            .clip_bottom(u16::from(cx.editor.popup_border()));
        let sig_doc_para = Paragraph::new(&sig_doc)
            .wrap(Wrap { trim: false })
            .scroll((cx.scroll.unwrap_or_default() as u16, 0));
        sig_doc_para.render(sig_doc_area, surface);
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        const PADDING: u16 = 2;
        const SEPARATOR_HEIGHT: u16 = 1;

        let (_, prepared) = self.current_signature();
        let max_text_width = viewport.0.saturating_sub(PADDING).clamp(10, 120);
        let (sig_width, sig_height) = prepared.dimensions(max_text_width);
        let (width, height) = match &prepared.document {
            Some(document) => {
                let (doc_width, doc_height) = document.dimensions(max_text_width);
                (
                    sig_width.max(doc_width),
                    sig_height + SEPARATOR_HEIGHT + doc_height,
                )
            }
            None => (sig_width, sig_height),
        };

        let sig_index_width = if self.signatures.len() > 1 {
            self.signature_index().len() + 1
        } else {
            0
        };

        Some((width + PADDING + sig_index_width as u16, height + PADDING))
    }
}

#[cfg(test)]
mod prepared_signature_tests {
    use super::*;

    fn help() -> SignatureHelp {
        SignatureHelp::new(
            "unknown".into(),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
            0,
            None,
            vec![
                Signature {
                    signature: "f(界,\te\u{301})".into(),
                    signature_doc: Some("**First** documentation.".into()),
                    active_param_range: Some((2, 5)),
                },
                Signature {
                    signature: "f(second)".into(),
                    signature_doc: None,
                    active_param_range: None,
                },
            ],
        )
    }

    fn theme(color: &str) -> Theme {
        toml::from_str(&format!(
            "\"ui.selection\" = {{ bg = \"#123456\" }}\n\"markup.raw.inline\" = {{ fg = \"{color}\" }}"
        ))
        .unwrap()
    }

    #[test]
    fn signature_and_document_caches_survive_switching_and_invalidate_styles() {
        let mut help = help();
        let first_theme = theme("#ff0000");
        let first = help.rendered_signature(&first_theme);
        let first_doc = help
            .current_signature()
            .1
            .document
            .as_ref()
            .unwrap()
            .parse(Some(&first_theme));
        assert!(Arc::ptr_eq(
            &first,
            &help.rendered_signature(&first_theme.clone())
        ));
        help.active_signature = 1;
        let second = help.rendered_signature(&first_theme);
        assert!(!Arc::ptr_eq(&first, &second));
        help.active_signature = 0;
        assert!(Arc::ptr_eq(&first, &help.rendered_signature(&first_theme)));
        assert!(Arc::ptr_eq(
            &first_doc,
            &help
                .current_signature()
                .1
                .document
                .as_ref()
                .unwrap()
                .parse(Some(&first_theme))
        ));

        help.signatures[0].active_param_range = Some((6, 9));
        let changed_param = help.rendered_signature(&first_theme);
        assert!(!Arc::ptr_eq(&first, &changed_param));
        let second_theme = theme("#00ff00");
        let changed_theme = help.rendered_signature(&second_theme);
        assert!(!Arc::ptr_eq(&changed_param, &changed_theme));
        assert_ne!(
            first.lines[0].0[0].style.fg,
            changed_theme.lines[0].0[0].style.fg
        );
        help.config_loader
            .load()
            .set_scopes(vec!["ui.selection".into()]);
        let changed_scopes = help.rendered_signature(&second_theme);
        assert!(!Arc::ptr_eq(&changed_theme, &changed_scopes));
        help.config_loader
            .store(Arc::new(syntax::Loader::default()));
        assert!(!Arc::ptr_eq(
            &changed_scopes,
            &help.rendered_signature(&second_theme)
        ));
    }

    #[test]
    fn cached_sizing_preserves_unicode_tabs_and_signature_fallback() {
        let mut help = help();
        let theme = theme("#ff0000");
        let first = help.rendered_signature(&theme);
        let layout = &help.current_signature().1.layout;
        assert_eq!(String::from(layout.as_ref()), "f(界,    e\u{301})");
        for width in [5, 20, 5] {
            let expected = Paragraph::new(&first)
                .wrap(Wrap { trim: false })
                .required_size(width);
            let prepared = help.current_signature().1;
            assert_eq!(prepared.dimensions(width), expected);
            assert_eq!(prepared.measured.get(), Some((width, expected)));
        }
        let size = help.required_size((80, 24));
        assert_eq!(size, help.required_size((80, 24)));
        help.active_signature = 100;
        assert!(std::ptr::eq(
            help.current_signature().0,
            &help.signatures[0]
        ));
        assert!(Arc::ptr_eq(&first, &help.rendered_signature(&theme)));
    }
}
