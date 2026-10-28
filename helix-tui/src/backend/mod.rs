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

/// Cell and pixel geometry measured together for one terminal frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowMetrics {
    pub area: Rect,
    pub cell_size: Option<CellSize>,
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

/// Ghostty expands even partial animation frames to full RGBA canvases. Bound
/// the decoded storage of one clip; replacement temporarily needs two clips.
pub const MAX_CURSOR_ANIMATION_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_CURSOR_ANIMATION_FRAMES: usize = 64;

#[derive(Debug, Clone)]
pub struct CursorAnimationFrame {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub gap_ms: u32,
    pub rgba: Vec<u8>,
}

/// Frames contain small rectangles on independent, transparent canvases.
#[derive(Debug, Clone)]
pub struct CursorAnimation {
    pub position: Position,
    pub offset_x: u16,
    pub offset_y: u16,
    pub width: u32,
    pub height: u32,
    pub frames: Vec<CursorAnimationFrame>,
}

impl CursorAnimation {
    pub(crate) fn validate(&self) -> io::Result<()> {
        let bytes = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .and_then(|bytes| bytes.checked_mul(self.frames.len()));
        if self.width == 0
            || self.height == 0
            || self.frames.is_empty()
            || self.frames.len() > MAX_CURSOR_ANIMATION_FRAMES
            || bytes.is_none_or(|bytes| bytes > MAX_CURSOR_ANIMATION_BYTES)
            || self.position.row.checked_add(1).is_none()
            || self.position.col.checked_add(1).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cursor animation canvas",
            ));
        }
        for frame in &self.frames {
            let bytes = (frame.width as usize)
                .checked_mul(frame.height as usize)
                .and_then(|pixels| pixels.checked_mul(4));
            if frame.width == 0
                || frame.height == 0
                || frame.gap_ms == 0
                || bytes != Some(frame.rgba.len())
                || frame
                    .x
                    .checked_add(frame.width)
                    .is_none_or(|x| x > self.width)
                || frame
                    .y
                    .checked_add(frame.height)
                    .is_none_or(|y| y > self.height)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid cursor animation frame",
                ));
            }
        }
        Ok(())
    }
}

pub enum CursorGraphicsUpdate<'a> {
    Image(Option<&'a CursorImage<'a>>),
    Animation(&'a CursorAnimation),
    /// Preserve terminal playback across unrelated text redraws.
    Unchanged,
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

    fn supports_cursor_animation(&self) -> bool {
        false
    }

    /// Upload offscreen, normally before beginning synchronized output. A full
    /// erase reclaims unplaced images, so prepare after the erase on that path.
    fn prepare_cursor_animation(&mut self, _animation: &CursorAnimation) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cursor animations unsupported",
        ))
    }

    /// Swap the prepared clip into view inside synchronized output.
    fn show_cursor_animation(
        &mut self,
        _animation: &CursorAnimation,
        _metrics: WindowMetrics,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cursor animations unsupported",
        ))
    }

    fn window_metrics(&self) -> io::Result<WindowMetrics> {
        Ok(WindowMetrics {
            area: self.size()?,
            cell_size: self.cursor_graphics_cell_size(),
        })
    }

    /// Replaces the backend's cursor image, or deletes only that image for `None`.
    /// Implementations preserve the terminal cursor position and text cells.
    fn draw_cursor_graphics(&mut self, _image: Option<&CursorImage<'_>>) -> io::Result<()> {
        Ok(())
    }

    /// Draw with the geometry already measured for this frame.
    fn draw_cursor_graphics_with_metrics(
        &mut self,
        image: Option<&CursorImage<'_>>,
        _metrics: WindowMetrics,
    ) -> io::Result<()> {
        self.draw_cursor_graphics(image)
    }
}
