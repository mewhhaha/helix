use std::{
    io::{self, Write as _},
    sync::{
        atomic::AtomicBool,
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use helix_view::{
    editor::KittyKeyboardProtocolConfig,
    graphics::{CursorKind, Rect, UnderlineStyle},
    theme::{self, Color, Modifier},
};
use termina::{
    escape::{
        csi::{self, Csi, SgrAttributes, SgrModifiers},
        dcs::{self, Dcs},
        osc::{self, Osc},
    },
    style::{CursorStyle, RgbColor},
    Event, OneBased, PlatformTerminal, Terminal as _, WindowSize,
};

use crate::{buffer::Cell, terminal::Config};

use super::{kitty, Backend, CellSize, CursorImage, WindowMetrics};

fn supports_cursor_graphics(term: Option<&str>, program: Option<&str>, multiplexed: bool) -> bool {
    !multiplexed
        && (matches!(term, Some("xterm-kitty" | "xterm-ghostty"))
            || matches!(program, Some("kitty" | "ghostty")))
}

fn cursor_graphics_compression(term: Option<&str>, program: Option<&str>) -> kitty::Compression {
    if term == Some("xterm-ghostty") || program == Some("ghostty") {
        // Ghostty 1.3.1 can segfault when inflating valid cursor frames. Raw RGBA
        // avoids its zlib decoder while retaining the same image and animation.
        kitty::Compression::None
    } else {
        kitty::Compression::Zlib
    }
}

fn cell_size(size: WindowSize) -> Option<CellSize> {
    let width = size.pixel_width?.checked_div(size.cols)?;
    let height = size.pixel_height?.checked_div(size.rows)?;
    (width != 0 && height != 0).then_some(CellSize { width, height })
}

// These macros are helpers to set/unset modes like bracketed paste or enter/exit the alternate
// screen.
macro_rules! decset {
    ($mode:ident) => {
        Csi::Mode(csi::Mode::SetDecPrivateMode(csi::DecPrivateMode::Code(
            csi::DecPrivateModeCode::$mode,
        )))
    };
}
macro_rules! decreset {
    ($mode:ident) => {
        Csi::Mode(csi::Mode::ResetDecPrivateMode(csi::DecPrivateMode::Code(
            csi::DecPrivateModeCode::$mode,
        )))
    };
}

fn term_program() -> Option<String> {
    // Some terminals don't set $TERM_PROGRAM
    match std::env::var("TERM_PROGRAM") {
        Err(_) => std::env::var("TERM").ok(),
        Ok(term_program) => Some(term_program),
    }
}
fn vte_version() -> Option<usize> {
    std::env::var("VTE_VERSION").ok()?.parse().ok()
}

#[derive(Debug, Default, Clone, Copy)]
struct Capabilities {
    kitty_keyboard: KittyKeyboardSupport,
    synchronized_output: bool,
    true_color: bool,
    extended_underlines: bool,
    /// OSC11 / OSC111 - change the terminal's background color.
    dynamic_background_color: bool,
    theme_mode: Option<theme::Mode>,
    /// Conservative advertised support: Termina cannot currently parse APC query replies.
    cursor_graphics: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum KittyKeyboardSupport {
    /// The terminal doesn't support the protocol.
    #[default]
    None,
    /// The terminal supports the protocol but we haven't checked yet whether it has full or
    /// partial support for the flags we require.
    Some,
    /// The terminal only supports some of the flags we require.
    Partial,
    /// The terminal supports all flags require.
    Full,
}

#[derive(Debug)]
pub struct TerminaBackend {
    terminal: PlatformTerminal,
    config: Config,
    capabilities: Capabilities,
    reset_cursor_command: String,
    is_synchronized_output_set: bool,
    /// The requested terminal background color, set in `Self::set_background_color`.
    background_color: Option<RgbColor>,
    /// The terminal emulator's background color. This is queried when claiming the terminal so
    /// that custom colors set outside of Helix with OSC11 are restored when Helix exits.
    original_background_color: Option<RgbColor>,
    cursor_image_id: u32,
    cursor_image_active: Arc<AtomicBool>,
    cursor_encoder: kitty::Encoder,
}

impl TerminaBackend {
    pub fn new(config: Config) -> io::Result<Self> {
        use std::time::{Duration, Instant};

        let mut terminal = PlatformTerminal::new()?;

        // Colibri "midnight"
        const TEST_COLOR: RgbColor = RgbColor::new(59, 34, 76);

        terminal.enter_raw_mode()?;

        let mut capabilities = Capabilities::default();
        let mut original_background_color = None;
        let term = std::env::var("TERM").ok();
        let program = std::env::var("TERM_PROGRAM").ok();
        capabilities.cursor_graphics = supports_cursor_graphics(
            term.as_deref(),
            program.as_deref(),
            ["TMUX", "STY", "ZELLIJ"]
                .iter()
                .any(|name| std::env::var_os(name).is_some()),
        );
        let start = Instant::now();

        // HACK: emitting OSC11 / OSC111 seems to break SGR and cause flickering in tmux.
        capabilities.dynamic_background_color = std::env::var_os("TMUX").is_none();

        capabilities.kitty_keyboard = match config.kitty_keyboard_protocol {
            KittyKeyboardProtocolConfig::Disabled => KittyKeyboardSupport::None,
            KittyKeyboardProtocolConfig::Enabled => KittyKeyboardSupport::Full,
            KittyKeyboardProtocolConfig::Auto => {
                write!(terminal, "{}", Csi::Keyboard(csi::Keyboard::QueryFlags))?;
                KittyKeyboardSupport::None
            }
        };

        // Many terminal extensions can be detected by querying the terminal for the state of the
        // extension and then sending a request for the primary device attributes (which is
        // consistently supported by all terminals). If we receive the status of the feature (for
        // example the current Kitty keyboard flags) then we know that the feature is supported.
        // If we only receive the device attributes then we know it is not.
        write!(
            terminal,
            "{}{}{}{}{}{}{}{}",
            // Synchronized output
            Csi::Mode(csi::Mode::QueryDecPrivateMode(csi::DecPrivateMode::Code(
                csi::DecPrivateModeCode::SynchronizedOutput
            ))),
            // Mode 2031 theme updates. Query the current theme.
            Csi::Mode(csi::Mode::QueryTheme),
            // True color and while we're at it, extended underlines:
            // <https://github.com/termstandard/colors?tab=readme-ov-file#querying-the-terminal>
            Csi::Sgr(csi::Sgr::Background(TEST_COLOR.into())),
            Csi::Sgr(csi::Sgr::UnderlineColor(TEST_COLOR.into())),
            Dcs::Request(dcs::DcsRequest::GraphicRendition),
            Csi::Sgr(csi::Sgr::Reset),
            Osc::ChangeDynamicColors(
                osc::DynamicColorNumber::TextBackgroundColor,
                vec![osc::ColorOrQuery::Query]
            ),
            // Finally request the primary device attributes
            Csi::Device(csi::Device::RequestPrimaryDeviceAttributes),
        )?;
        terminal.flush()?;

        let device_attributes = |event: &Event| {
            matches!(
                event,
                Event::Csi(Csi::Device(csi::Device::DeviceAttributes(_)))
            )
        };
        // TODO: tune this poll constant? Does it need to be longer when on an SSH connection?
        let poll_duration = Duration::from_millis(100);
        if terminal.poll(device_attributes, Some(poll_duration))? {
            while terminal.poll(Event::is_escape, Some(Duration::ZERO))? {
                match terminal.read(Event::is_escape)? {
                    Event::Csi(Csi::Keyboard(csi::Keyboard::ReportFlags(_))) => {
                        capabilities.kitty_keyboard = KittyKeyboardSupport::Some;
                    }
                    Event::Csi(Csi::Mode(csi::Mode::ReportDecPrivateMode {
                        mode: csi::DecPrivateMode::Code(csi::DecPrivateModeCode::SynchronizedOutput),
                        setting: csi::DecModeSetting::Set | csi::DecModeSetting::Reset,
                    })) => {
                        capabilities.synchronized_output = true;
                    }
                    Event::Csi(Csi::Mode(csi::Mode::ReportTheme(mode))) => {
                        capabilities.theme_mode = Some(mode.into());
                    }
                    Event::Osc(Osc::ChangeDynamicColors(
                        osc::DynamicColorNumber::TextBackgroundColor,
                        colors,
                    )) => {
                        if let Some(osc::ColorOrQuery::Color(color)) = colors.first() {
                            original_background_color = Some(*color);
                        }
                    }
                    Event::Dcs(dcs::Dcs::Response {
                        value: dcs::DcsResponse::GraphicRendition(sgrs),
                        ..
                    }) => {
                        capabilities.true_color =
                            sgrs.contains(&csi::Sgr::Background(TEST_COLOR.into()));
                        capabilities.extended_underlines =
                            sgrs.contains(&csi::Sgr::UnderlineColor(TEST_COLOR.into()));
                    }
                    _ => (),
                }
            }

            let end = Instant::now();
            log::debug!(
                "Detected terminal capabilities in {:?}: {capabilities:?}",
                end.duration_since(start)
            );
        } else {
            log::debug!("Failed to detect terminal capabilities within {poll_duration:?}. Using default capabilities only");
        }

        capabilities.extended_underlines |= config.force_enable_extended_underlines;

        let mut reset_cursor_command = String::new();
        if let Ok(t) = termini::TermInfo::from_env() {
            capabilities.extended_underlines |= t.extended_cap("Smulx").is_some()
                || t.extended_cap("Su").is_some()
                || vte_version() >= Some(5102)
                // HACK: once WezTerm can support DECRQSS/DECRPSS for SGR we can remove this line.
                // <https://github.com/wezterm/wezterm/pull/6856>
                || matches!(term_program().as_deref(), Some("WezTerm"));

            if let Some(termini::Value::Utf8String(se_str)) = t.extended_cap("Se") {
                reset_cursor_command.push_str(se_str);
            };
            reset_cursor_command.push_str(
                t.utf8_string_cap(termini::StringCapability::CursorNormal)
                    .unwrap_or(""),
            );
            log::debug!(
                "Cursor reset escape sequence detected from terminfo: {reset_cursor_command:?}"
            );
        } else {
            log::debug!("terminfo could not be read, using default cursor reset escape sequence: {reset_cursor_command:?}");
        }
        reset_cursor_command
            .push_str(&Csi::Cursor(csi::Cursor::CursorStyle(CursorStyle::Default)).to_string());

        terminal.enter_cooked_mode()?;

        // In the case of a panic, reset the terminal eagerly. If we didn't do this and instead
        // relied on `Drop`, the backtrace would be lost because it is printed before we would
        // clear and exit the alternate screen.
        let hook_reset_cursor_command = reset_cursor_command.clone();
        static NEXT_IMAGE: AtomicU32 = AtomicU32::new(1);
        let cursor_image_id = std::process::id()
            .wrapping_mul(0x9e37_79b9)
            .wrapping_add(NEXT_IMAGE.fetch_add(1, Ordering::Relaxed))
            .max(1);
        let cursor_image_active = Arc::new(AtomicBool::new(false));
        let hook_image_active = cursor_image_active.clone();
        terminal.set_panic_hook(move |term| {
            let _ = write!(term, "{}", decreset!(SynchronizedOutput));
            if hook_image_active.swap(false, Ordering::Relaxed) {
                let _ = kitty::delete_image(term, cursor_image_id);
            }
            let _ = write!(
                term,
                "{}{}{}{}{}{}{}{}{}{}{}{}",
                Csi::Keyboard(csi::Keyboard::PopFlags(1)),
                decreset!(MouseTracking),
                decreset!(ButtonEventMouse),
                decreset!(AnyEventMouse),
                decreset!(RXVTMouse),
                decreset!(SGRMouse),
                &hook_reset_cursor_command,
                decreset!(BracketedPaste),
                decreset!(FocusTracking),
                Osc::ResetDynamicColor(osc::DynamicColorNumber::TextBackgroundColor),
                Csi::Edit(csi::Edit::EraseInDisplay(csi::EraseInDisplay::EraseDisplay)),
                decreset!(ClearAndEnableAlternateScreen),
            );
        });

        Ok(Self {
            terminal,
            config,
            capabilities,
            reset_cursor_command,
            is_synchronized_output_set: false,
            background_color: None,
            original_background_color,
            cursor_image_id,
            cursor_image_active,
            cursor_encoder: kitty::Encoder::new(cursor_graphics_compression(
                term.as_deref(),
                program.as_deref(),
            )),
        })
    }

    pub fn terminal(&self) -> &PlatformTerminal {
        &self.terminal
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        if self.config.enable_mouse_capture {
            write!(
                self.terminal,
                "{}{}{}{}{}",
                decset!(MouseTracking),
                decset!(ButtonEventMouse),
                decset!(AnyEventMouse),
                decset!(RXVTMouse),
                decset!(SGRMouse),
            )?;
        }
        Ok(())
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        if self.config.enable_mouse_capture {
            write!(
                self.terminal,
                "{}{}{}{}{}",
                decreset!(MouseTracking),
                decreset!(ButtonEventMouse),
                decreset!(AnyEventMouse),
                decreset!(RXVTMouse),
                decreset!(SGRMouse),
            )?;
        }
        Ok(())
    }

    fn reset_background_color(&mut self) -> io::Result<()> {
        write!(
            self.terminal,
            "{}",
            match self.original_background_color {
                Some(color) => Osc::ChangeDynamicColors(
                    osc::DynamicColorNumber::TextBackgroundColor,
                    vec![color.into()]
                ),
                None => Osc::ResetDynamicColor(osc::DynamicColorNumber::TextBackgroundColor),
            }
        )
    }

    fn enable_extensions(&mut self) -> io::Result<()> {
        const KEYBOARD_FLAGS: csi::KittyKeyboardFlags =
            csi::KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES
                .union(csi::KittyKeyboardFlags::REPORT_ALTERNATE_KEYS);

        match self.capabilities.kitty_keyboard {
            KittyKeyboardSupport::None | KittyKeyboardSupport::Partial => (),
            KittyKeyboardSupport::Full => {
                write!(
                    self.terminal,
                    "{}",
                    Csi::Keyboard(csi::Keyboard::PushFlags(KEYBOARD_FLAGS))
                )?;
            }
            KittyKeyboardSupport::Some => {
                write!(
                    self.terminal,
                    "{}{}",
                    // Enable the flags we need.
                    Csi::Keyboard(csi::Keyboard::PushFlags(KEYBOARD_FLAGS)),
                    // Then request the current flags. We need to check if the terminal enabled
                    // all of the flags we require.
                    Csi::Keyboard(csi::Keyboard::QueryFlags),
                )?;
                self.terminal.flush()?;

                let event = self.terminal.read(|event| {
                    matches!(
                        event,
                        Event::Csi(Csi::Keyboard(csi::Keyboard::ReportFlags(_)))
                    )
                })?;
                let Event::Csi(Csi::Keyboard(csi::Keyboard::ReportFlags(flags))) = event else {
                    unreachable!();
                };
                if flags != KEYBOARD_FLAGS {
                    log::info!("Turning off enhanced keyboard support because the terminal enabled different flags. Requested {KEYBOARD_FLAGS:?} but got {flags:?}");
                    write!(
                        self.terminal,
                        "{}",
                        Csi::Keyboard(csi::Keyboard::PopFlags(1))
                    )?;
                    self.terminal.flush()?;
                    self.capabilities.kitty_keyboard = KittyKeyboardSupport::Partial;
                } else {
                    log::debug!(
                        "The terminal fully supports the requested keyboard enhancement flags"
                    );
                    self.capabilities.kitty_keyboard = KittyKeyboardSupport::Full;
                }
            }
        }

        if self.capabilities.theme_mode.is_some() {
            // Enable mode 2031 theme mode notifications:
            write!(self.terminal, "{}", decset!(Theme))?;
        }

        Ok(())
    }

    fn disable_extensions(&mut self) -> io::Result<()> {
        if self.capabilities.kitty_keyboard == KittyKeyboardSupport::Full {
            write!(
                self.terminal,
                "{}",
                Csi::Keyboard(csi::Keyboard::PopFlags(1))
            )?;
        }

        if self.capabilities.theme_mode.is_some() {
            // Mode 2031 theme notifications.
            write!(self.terminal, "{}", decreset!(Theme))?;
        }

        Ok(())
    }

    // See <https://gist.github.com/christianparpart/d8a62cc1ab659194337d73e399004036>.
    // Synchronized output sequences tell the terminal when we are "starting to render" and
    // stopping, enabling to make better choices about when it draws a frame. This avoids all
    // kinds of ugly visual artifacts like tearing and flashing (i.e. the background color
    // after clearing the terminal).

    fn start_synchronized_render(&mut self) -> io::Result<()> {
        if self.capabilities.synchronized_output && !self.is_synchronized_output_set {
            write!(self.terminal, "{}", decset!(SynchronizedOutput))?;
            self.is_synchronized_output_set = true;
        }
        Ok(())
    }

    fn end_sychronized_render(&mut self) -> io::Result<()> {
        if self.is_synchronized_output_set {
            write!(self.terminal, "{}", decreset!(SynchronizedOutput))?;
            self.is_synchronized_output_set = false;
        }
        Ok(())
    }
}

impl Backend for TerminaBackend {
    fn claim(&mut self) -> io::Result<()> {
        self.terminal.enter_raw_mode()?;

        write!(
            self.terminal,
            "{}{}{}{}",
            // Enter an alternate screen.
            decset!(ClearAndEnableAlternateScreen),
            decset!(BracketedPaste),
            decset!(FocusTracking),
            // Clear the buffer. `ClearAndEnableAlternateScreen` **should** do this but some
            // things like mosh are buggy. See <https://github.com/helix-editor/helix/pull/1944>.
            Csi::Edit(csi::Edit::EraseInDisplay(csi::EraseInDisplay::EraseDisplay)),
        )?;
        if let Some(color) = self.background_color {
            write!(
                self.terminal,
                "{}",
                Osc::ChangeDynamicColors(
                    osc::DynamicColorNumber::TextBackgroundColor,
                    vec![color.into()]
                )
            )?;
        }
        self.enable_mouse_capture()?;
        self.enable_extensions()?;

        Ok(())
    }

    fn reconfigure(&mut self, mut config: Config) -> io::Result<()> {
        std::mem::swap(&mut self.config, &mut config);
        if self.config.enable_mouse_capture != config.enable_mouse_capture {
            if self.config.enable_mouse_capture {
                self.enable_mouse_capture()?;
            } else {
                self.disable_mouse_capture()?;
            }
        }
        self.capabilities.extended_underlines |= self.config.force_enable_extended_underlines;
        if self.config.cursor_graphics != config.cursor_graphics {
            self.draw_cursor_graphics(None)?;
        }
        Ok(())
    }

    fn restore(&mut self) -> io::Result<()> {
        self.draw_cursor_graphics(None)?;
        self.end_sychronized_render()?;
        self.disable_extensions()?;
        self.disable_mouse_capture()?;
        write!(
            self.terminal,
            "{}{}{}{}",
            &self.reset_cursor_command,
            decreset!(BracketedPaste),
            decreset!(FocusTracking),
            decreset!(ClearAndEnableAlternateScreen),
        )?;
        if self.background_color.is_some() {
            self.reset_background_color()?;
        }
        self.terminal.flush()?;
        self.terminal.enter_cooked_mode()?;
        Ok(())
    }

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut underline_color = Color::Reset;
        let mut underline_style = UnderlineStyle::Reset;
        let mut modifier = Modifier::empty();
        let mut last_pos: Option<(u16, u16)> = None;
        for (x, y, cell) in content {
            // Move the cursor if the previous location was not (x - 1, y)
            if !matches!(last_pos, Some(p) if x == p.0 + 1 && y == p.1) {
                write!(
                    self.terminal,
                    "{}",
                    Csi::Cursor(csi::Cursor::Position {
                        col: OneBased::from_zero_based(x),
                        line: OneBased::from_zero_based(y),
                    })
                )?;
            }
            last_pos = Some((x, y));

            let mut attributes = SgrAttributes::default();
            if cell.fg != fg {
                attributes.foreground = Some(cell.fg.into());
                fg = cell.fg;
            }
            if cell.bg != bg {
                attributes.background = Some(cell.bg.into());
                bg = cell.bg;
            }
            if cell.modifier != modifier {
                attributes.modifiers = diff_modifiers(modifier, cell.modifier);
                modifier = cell.modifier;
            }

            // Set underline style and color separately from SgrAttributes. Some terminals seem
            // to not like underline colors and styles being intermixed with other SGRs.
            let mut new_underline_style = cell.underline_style;
            if self.capabilities.extended_underlines {
                if cell.underline_color != underline_color {
                    write!(
                        self.terminal,
                        "{}",
                        Csi::Sgr(csi::Sgr::UnderlineColor(cell.underline_color.into()))
                    )?;
                    underline_color = cell.underline_color;
                }
            } else {
                match new_underline_style {
                    UnderlineStyle::Reset | UnderlineStyle::Line => (),
                    _ => new_underline_style = UnderlineStyle::Line,
                }
            }
            if new_underline_style != underline_style {
                write!(
                    self.terminal,
                    "{}",
                    Csi::Sgr(csi::Sgr::Underline(new_underline_style.into()))
                )?;
                underline_style = new_underline_style;
            }

            // `attributes` will be empty if nothing changed between two cells. Empty
            // `SgrAttributes` behave the same as a `Sgr::Reset` rather than a 'no-op' though so
            // we should avoid writing them if they're empty.
            if !attributes.is_empty() {
                write!(
                    self.terminal,
                    "{}",
                    Csi::Sgr(csi::Sgr::Attributes(attributes))
                )?;
            }

            write!(self.terminal, "{}", &cell.symbol)?;
        }

        write!(self.terminal, "{}", Csi::Sgr(csi::Sgr::Reset))?;

        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        write!(self.terminal, "{}", decreset!(ShowCursor))
    }

    fn show_cursor(&mut self, kind: CursorKind) -> io::Result<()> {
        let style = match kind {
            CursorKind::Block => CursorStyle::SteadyBlock,
            CursorKind::Bar => CursorStyle::SteadyBar,
            CursorKind::Underline => CursorStyle::SteadyUnderline,
            CursorKind::Hidden => unreachable!(),
        };
        write!(
            self.terminal,
            "{}{}",
            decset!(ShowCursor),
            Csi::Cursor(csi::Cursor::CursorStyle(style)),
        )
    }

    fn set_cursor(&mut self, x: u16, y: u16) -> io::Result<()> {
        let col = OneBased::from_zero_based(x);
        let line = OneBased::from_zero_based(y);
        write!(
            self.terminal,
            "{}",
            Csi::Cursor(csi::Cursor::Position { line, col })
        )
    }

    fn clear(&mut self) -> io::Result<()> {
        self.draw_cursor_graphics(None)?;
        write!(
            self.terminal,
            "{}",
            Csi::Edit(csi::Edit::EraseInDisplay(csi::EraseInDisplay::EraseDisplay))
        )
    }

    fn start_sync(&mut self) -> io::Result<()> {
        self.start_synchronized_render()
    }

    fn end_sync(&mut self) -> io::Result<()> {
        self.end_sychronized_render()
    }

    fn size(&self) -> io::Result<Rect> {
        let WindowSize { rows, cols, .. } = self.terminal.get_dimensions()?;
        Ok(Rect::new(0, 0, cols, rows))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.terminal.flush()
    }

    fn supports_true_color(&self) -> bool {
        self.capabilities.true_color
    }

    fn cursor_graphics_cell_size(&self) -> Option<CellSize> {
        self.window_metrics().ok()?.cell_size
    }

    fn window_metrics(&self) -> io::Result<WindowMetrics> {
        let size = self.terminal.get_dimensions()?;
        Ok(WindowMetrics {
            area: Rect::new(0, 0, size.cols, size.rows),
            cell_size: (self.config.cursor_graphics && self.capabilities.cursor_graphics)
                .then(|| cell_size(size))
                .flatten(),
        })
    }

    fn draw_cursor_graphics(&mut self, image: Option<&CursorImage<'_>>) -> io::Result<()> {
        let metrics = if image.is_some() {
            self.window_metrics()?
        } else {
            WindowMetrics {
                area: Rect::default(),
                cell_size: None,
            }
        };
        self.draw_cursor_graphics_with_metrics(image, metrics)
    }

    fn draw_cursor_graphics_with_metrics(
        &mut self,
        image: Option<&CursorImage<'_>>,
        metrics: WindowMetrics,
    ) -> io::Result<()> {
        let Some(image) = image else {
            if self.cursor_image_active.load(Ordering::Relaxed) {
                kitty::delete_image(&mut self.terminal, self.cursor_image_id)?;
                self.cursor_image_active.store(false, Ordering::Relaxed);
            }
            return Ok(());
        };
        let Some(size) = metrics.cell_size else {
            return self.draw_cursor_graphics(None);
        };
        let area = metrics.area;
        if image.position.row >= usize::from(area.height)
            || image.position.col >= usize::from(area.width)
            || image.offset_x >= size.width
            || image.offset_y >= size.height
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cursor image outside terminal",
            ));
        }
        // Mark before writing so a partial transfer is also deleted during cleanup.
        self.cursor_image_active.store(true, Ordering::Relaxed);
        self.cursor_encoder
            .draw_image(&mut self.terminal, self.cursor_image_id, image)
    }

    fn get_theme_mode(&self) -> Option<theme::Mode> {
        self.capabilities.theme_mode
    }

    fn set_background_color(&mut self, color: Option<Color>) -> io::Result<()> {
        if !self.capabilities.dynamic_background_color {
            return Ok(());
        }
        self.background_color = match color {
            Some(Color::Rgb(r, g, b)) => Some(RgbColor::new(r, g, b)),
            _ => None,
        };
        if let Some(color) = self.background_color {
            write!(
                self.terminal,
                "{}",
                Osc::ChangeDynamicColors(
                    osc::DynamicColorNumber::TextBackgroundColor,
                    vec![color.into()]
                )
            )
        } else {
            self.reset_background_color()
        }
    }
}

impl Drop for TerminaBackend {
    fn drop(&mut self) {
        // Avoid resetting the terminal while panicking because we set a panic hook above in
        // `Self::new`.
        if !std::thread::panicking() {
            let _ = self.draw_cursor_graphics(None);
            let _ = self.end_sychronized_render();
            let _ = self.disable_extensions();
            let _ = self.disable_mouse_capture();
            let _ = write!(
                self.terminal,
                "{}{}{}{}",
                &self.reset_cursor_command,
                decreset!(BracketedPaste),
                decreset!(FocusTracking),
                decreset!(ClearAndEnableAlternateScreen),
            );
            if self.background_color.is_some() {
                let _ = self.reset_background_color();
            }
            // NOTE: Drop for Platform terminal resets the mode and flushes the buffer when not
            // panicking.
        }
    }
}

fn diff_modifiers(from: Modifier, to: Modifier) -> SgrModifiers {
    let mut modifiers = SgrModifiers::default();

    let removed = from - to;
    if removed.contains(Modifier::REVERSED) {
        modifiers |= SgrModifiers::NO_REVERSE;
    }
    if removed.contains(Modifier::BOLD) {
        modifiers |= SgrModifiers::INTENSITY_NORMAL;
        if to.contains(Modifier::DIM) {
            modifiers |= SgrModifiers::INTENSITY_DIM
        }
    }
    if removed.contains(Modifier::DIM) {
        modifiers |= SgrModifiers::INTENSITY_NORMAL;
    }
    if removed.contains(Modifier::ITALIC) {
        modifiers |= SgrModifiers::NO_ITALIC;
    }
    if removed.contains(Modifier::CROSSED_OUT) {
        modifiers |= SgrModifiers::NO_STRIKE_THROUGH;
    }
    if removed.contains(Modifier::HIDDEN) {
        modifiers |= SgrModifiers::NO_INVISIBLE;
    }
    if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
        modifiers |= SgrModifiers::BLINK_NONE;
    }

    let added = to - from;
    if added.contains(Modifier::REVERSED) {
        modifiers |= SgrModifiers::REVERSE;
    }
    if added.contains(Modifier::BOLD) {
        modifiers |= SgrModifiers::INTENSITY_BOLD;
    }
    if added.contains(Modifier::DIM) {
        modifiers |= SgrModifiers::INTENSITY_DIM;
    }
    if added.contains(Modifier::ITALIC) {
        modifiers |= SgrModifiers::ITALIC;
    }
    if added.contains(Modifier::CROSSED_OUT) {
        modifiers |= SgrModifiers::STRIKE_THROUGH;
    }
    if added.contains(Modifier::HIDDEN) {
        modifiers |= SgrModifiers::INVISIBLE;
    }
    if added.contains(Modifier::SLOW_BLINK) {
        modifiers |= SgrModifiers::BLINK_SLOW;
    }
    if added.contains(Modifier::RAPID_BLINK) {
        modifiers |= SgrModifiers::BLINK_RAPID;
    }

    modifiers
}

#[cfg(test)]
mod cursor_graphics_tests {
    use super::*;

    #[test]
    fn ghostty_uses_raw_rgba_and_kitty_keeps_zlib() {
        for (term, program) in [
            (Some("xterm-ghostty"), None),
            (Some("xterm-ghostty"), Some("ghostty")),
            (Some("xterm-256color"), Some("ghostty")),
            (None, Some("ghostty")),
        ] {
            assert_eq!(
                cursor_graphics_compression(term, program),
                kitty::Compression::None
            );
        }
        for (term, program) in [
            (Some("xterm-kitty"), None),
            (Some("xterm-kitty"), Some("kitty")),
            (Some("xterm-256color"), Some("kitty")),
        ] {
            assert_eq!(
                cursor_graphics_compression(term, program),
                kitty::Compression::Zlib
            );
        }
    }

    #[test]
    fn advertised_graphics_support_rejects_other_terminals_and_multiplexers() {
        assert!(supports_cursor_graphics(Some("xterm-kitty"), None, false));
        assert!(supports_cursor_graphics(Some("xterm-ghostty"), None, false));
        assert!(supports_cursor_graphics(
            Some("xterm-256color"),
            Some("ghostty"),
            false
        ));
        assert!(!supports_cursor_graphics(
            Some("xterm-256color"),
            None,
            false
        ));
        assert!(!supports_cursor_graphics(
            Some("xterm-kitty"),
            Some("kitty"),
            true
        ));
        assert!(!supports_cursor_graphics(
            Some("xterm-ghostty"),
            Some("ghostty"),
            true
        ));
    }

    #[test]
    fn pixel_metrics_require_nonzero_dimensions_and_follow_live_font_metrics() {
        let size = WindowSize {
            cols: 80,
            rows: 24,
            pixel_width: Some(640),
            pixel_height: Some(384),
        };
        assert_eq!(
            cell_size(size),
            Some(CellSize {
                width: 8,
                height: 16
            })
        );
        assert_eq!(cell_size(WindowSize { cols: 0, ..size }), None);
        assert_eq!(cell_size(WindowSize { rows: 0, ..size }), None);
        let unmeasured = WindowSize {
            pixel_width: Some(0),
            pixel_height: Some(0),
            ..size
        };
        assert_eq!(cell_size(unmeasured), None);
        assert_eq!(
            cell_size(WindowSize {
                pixel_width: Some(800),
                pixel_height: Some(480),
                ..size
            }),
            Some(CellSize {
                width: 10,
                height: 20
            })
        );
    }
}
