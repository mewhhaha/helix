//! Provides interface for controlling the terminal

use helix_core::Position;
use std::io;

use crate::{buffer::Cell, terminal::Config};

use helix_view::{
    graphics::{CursorKind, Rect},
    theme::Color,
};

#[cfg(all(feature = "termina", not(windows)))]
mod termina;
#[cfg(all(feature = "termina", not(windows)))]
pub use self::termina::TerminaBackend;

#[cfg(all(feature = "termina", windows))]
mod crossterm;
#[cfg(all(feature = "termina", windows))]
pub use self::crossterm::CrosstermBackend;

mod test;
pub use self::test::{RecordedCursorImage, TestBackend};

#[cfg(all(feature = "termina", not(windows)))]
mod kitty;

/// The measured size of one terminal cell in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellSize {
    pub width: u16,
    pub height: u16,
}

/// A tightly packed, straight-alpha RGBA image anchored to a terminal cell.
#[derive(Debug, Clone, Copy)]
pub struct CursorImage<'a> {
    pub position: Position,
    pub offset_x: u16,
    pub offset_y: u16,
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
}

impl CursorImage<'_> {
    pub(crate) fn validate(&self) -> io::Result<()> {
        let len = usize::try_from(self.width)
            .ok()
            .and_then(|width| width.checked_mul(usize::try_from(self.height).ok()?))
            .and_then(|pixels| pixels.checked_mul(4));
        if self.width == 0
            || self.height == 0
            || len != Some(self.rgba.len())
            || self.rgba.len() > 16 * 1024 * 1024
            || self.position.row.checked_add(1).is_none()
            || self.position.col.checked_add(1).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cursor RGBA image",
            ));
        }
        Ok(())
    }
}

/// Representation of a terminal backend.
pub trait Backend {
    /// Claims the terminal for TUI use.
    fn claim(&mut self) -> Result<(), io::Error>;
    /// Update terminal configuration.
    fn reconfigure(&mut self, config: Config) -> Result<(), io::Error>;
    /// Restores the terminal to a normal state, undoes `claim`
    fn restore(&mut self) -> Result<(), io::Error>;
    /// Draws styled text to the terminal
    fn draw<'a, I>(&mut self, content: I) -> Result<(), io::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>;
    /// Hides the cursor
    fn hide_cursor(&mut self) -> Result<(), io::Error>;
    /// Sets the cursor to the given shape
    fn show_cursor(&mut self, kind: CursorKind) -> Result<(), io::Error>;
    /// Sets the cursor to the given position
    fn set_cursor(&mut self, x: u16, y: u16) -> Result<(), io::Error>;
    /// Clears the terminal
    fn clear(&mut self) -> Result<(), io::Error>;
    /// Begins a synchronized-output frame (if the terminal supports it), so the
    /// draw and cursor updates between `start_sync` and `end_sync` present as one
    /// frame instead of flickering.
    fn start_sync(&mut self) -> Result<(), io::Error>;
    /// Ends the synchronized-output frame opened by `start_sync`.
    fn end_sync(&mut self) -> Result<(), io::Error>;
    /// Gets the size of the terminal in cells
    fn size(&self) -> Result<Rect, io::Error>;
    /// Flushes the terminal buffer
    fn flush(&mut self) -> Result<(), io::Error>;
    fn supports_true_color(&self) -> bool;
    fn get_theme_mode(&self) -> Option<helix_view::theme::Mode>;
    fn set_background_color(&mut self, color: Option<Color>) -> io::Result<()>;

    /// Returns measured cell dimensions when cursor graphics are enabled and supported.
    /// Backends without graphics support never emit image commands.
    fn cursor_graphics_cell_size(&self) -> Option<CellSize> {
        None
    }

    /// Replaces the backend's cursor image, or deletes only that image for `None`.
    /// Implementations preserve the terminal cursor position and text cells.
    fn draw_cursor_graphics(&mut self, _image: Option<&CursorImage<'_>>) -> io::Result<()> {
        Ok(())
    }
}
