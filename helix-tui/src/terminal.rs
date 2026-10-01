//! Terminal interface provided through the [Terminal] type.
//! Frontend for [Backend]

use crate::{
    backend::{Backend, CursorImage, WindowMetrics},
    buffer::Buffer,
};
use helix_view::editor::{Config as EditorConfig, KittyKeyboardProtocolConfig};
use helix_view::graphics::{CursorKind, Rect};
use std::io;

#[derive(Debug, Clone, PartialEq)]
/// UNSTABLE
enum ResizeBehavior {
    Fixed,
    Auto,
}

#[derive(Debug, Clone, PartialEq)]
/// UNSTABLE
pub struct Viewport {
    area: Rect,
    resize_behavior: ResizeBehavior,
}

/// Terminal configuration
#[derive(Debug)]
pub struct Config {
    pub enable_mouse_capture: bool,
    pub force_enable_extended_underlines: bool,
    pub kitty_keyboard_protocol: KittyKeyboardProtocolConfig,
    pub cursor_graphics: bool,
}

impl From<&EditorConfig> for Config {
    fn from(config: &EditorConfig) -> Self {
        Self {
            enable_mouse_capture: config.mouse,
            force_enable_extended_underlines: config.undercurl,
            kitty_keyboard_protocol: config.kitty_keyboard_protocol,
            cursor_graphics: config.cursor_smear.enabled,
        }
    }
}

impl Viewport {
    /// UNSTABLE
    pub fn fixed(area: Rect) -> Viewport {
        Viewport {
            area,
            resize_behavior: ResizeBehavior::Fixed,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
/// Options to pass to [`Terminal::with_options`]
pub struct TerminalOptions {
    /// Viewport used to draw to the terminal
    pub viewport: Viewport,
}

/// Interface to the terminal backed by crossterm
#[derive(Debug)]
pub struct Terminal<B>
where
    B: Backend,
{
    backend: B,
    /// Holds the results of the current and previous draw calls. The two are compared at the end
    /// of each draw pass to output the necessary updates to the terminal
    buffers: [Buffer; 2],
    /// Index of the current buffer in the previous array
    current: usize,
    /// Kind of cursor (hidden or others)
    cursor_kind: CursorKind,
    /// Viewport
    viewport: Viewport,
    /// Set to request a full clear. The erase is deferred to the next `flush` so it is emitted
    /// inside the same synchronized-output frame as the repaint to avoid painting blank frames
    force_clear: bool,
}

/// Default terminal size: 80 columns, 24 lines
pub const DEFAULT_TERMINAL_SIZE: Rect = Rect {
    x: 0,
    y: 0,
    width: 80,
    height: 24,
};

impl<B> Terminal<B>
where
    B: Backend,
{
    /// Wrapper around Terminal initialization. Each buffer is initialized with a blank string and
    /// default colors for the foreground and the background
    pub fn new(backend: B) -> io::Result<Terminal<B>> {
        let size = backend.size().unwrap_or(DEFAULT_TERMINAL_SIZE);
        Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport {
                    area: size,
                    resize_behavior: ResizeBehavior::Auto,
                },
            },
        )
    }

    /// UNSTABLE
    pub fn with_options(backend: B, options: TerminalOptions) -> io::Result<Terminal<B>> {
        Ok(Terminal {
            backend,
            buffers: [
                Buffer::empty(options.viewport.area),
                Buffer::empty(options.viewport.area),
            ],
            current: 0,
            cursor_kind: CursorKind::Block,
            viewport: options.viewport,
            force_clear: false,
        })
    }

    pub fn claim(&mut self) -> io::Result<()> {
        self.backend.claim()
    }

    pub fn reconfigure(&mut self, config: Config) -> io::Result<()> {
        self.backend.reconfigure(config)
    }

    pub fn restore(&mut self) -> io::Result<()> {
        self.backend.restore()
    }

    // /// Get a Frame object which provides a consistent view into the terminal state for rendering.
    // pub fn get_frame(&mut self) -> Frame<B> {
    //     Frame {
    //         terminal: self,
    //         cursor_position: None,
    //     }
    // }

    pub fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Obtains a difference between the previous and the current buffer and passes it to the
    /// current backend for drawing.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.force_clear {
            self.backend.clear()?;
            self.force_clear = false;
        }
        let previous_buffer = &self.buffers[1 - self.current];
        let current_buffer = &self.buffers[self.current];
        self.backend.draw(previous_buffer.diff_iter(current_buffer))
    }

    /// Updates the Terminal so that internal buffers match the requested size. Requested size will
    /// be saved so the size can remain consistent when rendering.
    pub fn resize(&mut self, area: Rect) -> io::Result<()> {
        self.buffers[self.current].resize(area);
        self.buffers[1 - self.current].resize(area);
        self.viewport.area = area;
        self.draw_cursor_graphics(None)?;
        self.clear()
    }

    /// Queries the backend for size and resizes if it doesn't match the previous size.
    pub fn autoresize(&mut self) -> io::Result<Rect> {
        Ok(self.autoresize_with_metrics()?.area)
    }

    pub fn autoresize_with_metrics(&mut self) -> io::Result<WindowMetrics> {
        let metrics = self.backend.window_metrics().unwrap_or(WindowMetrics {
            area: DEFAULT_TERMINAL_SIZE,
            cell_size: None,
        });
        if metrics.area != self.viewport.area {
            self.resize(metrics.area)?;
        }
        Ok(metrics)
    }

    /// Synchronizes terminal size, calls the rendering closure, flushes the current internal state
    /// and prepares for the next draw call.
    pub fn draw(
        &mut self,
        cursor_position: Option<(u16, u16)>,
        cursor_kind: CursorKind,
    ) -> io::Result<()> {
        self.draw_with_cursor_graphics(cursor_position, cursor_kind, None)
    }

    /// Presents text, cursor state, and a pixel cursor image in one synchronized frame.
    pub fn draw_with_cursor_graphics(
        &mut self,
        cursor_position: Option<(u16, u16)>,
        cursor_kind: CursorKind,
        image: Option<&CursorImage<'_>>,
    ) -> io::Result<()> {
        let metrics = if image.is_some() {
            self.backend.window_metrics()?
        } else {
            WindowMetrics {
                area: self.viewport.area,
                cell_size: None,
            }
        };
        self.draw_with_cursor_graphics_and_metrics(cursor_position, cursor_kind, image, metrics)
    }

    pub fn draw_with_cursor_graphics_and_metrics(
        &mut self,
        cursor_position: Option<(u16, u16)>,
        cursor_kind: CursorKind,
        image: Option<&CursorImage<'_>>,
        metrics: WindowMetrics,
    ) -> io::Result<()> {
        // One synchronized frame for the whole draw
        self.synchronized(|terminal| {
            terminal.flush()?;
            terminal
                .backend
                .draw_cursor_graphics_with_metrics(image, metrics)?;
            if let Some((x, y)) = cursor_position {
                terminal.set_cursor(x, y)?;
            }
            match cursor_kind {
                CursorKind::Hidden => terminal.hide_cursor(),
                kind => terminal.show_cursor(kind),
            }
        })?;

        // Swap buffers
        self.buffers[1 - self.current].reset();
        self.current = 1 - self.current;

        Ok(())
    }

    /// Presents an animation frame without diffing or swapping the text buffers.
    pub fn draw_cursor_graphics(&mut self, image: Option<&CursorImage<'_>>) -> io::Result<()> {
        let metrics = if image.is_some() {
            self.backend.window_metrics()?
        } else {
            WindowMetrics {
                area: self.viewport.area,
                cell_size: None,
            }
        };
        self.draw_cursor_graphics_with_metrics(image, metrics)
    }

    pub fn draw_cursor_graphics_with_metrics(
        &mut self,
        image: Option<&CursorImage<'_>>,
        metrics: WindowMetrics,
    ) -> io::Result<()> {
        self.synchronized(|terminal| {
            terminal
                .backend
                .draw_cursor_graphics_with_metrics(image, metrics)
        })
    }

    fn synchronized(&mut self, draw: impl FnOnce(&mut Self) -> io::Result<()>) -> io::Result<()> {
        self.backend.start_sync()?;
        let result = draw(self);
        // Always release synchronized output, including when image/text writes fail.
        let end = self.backend.end_sync();
        let flush = self.backend.flush();
        result.and(end).and(flush)
    }

    #[inline]
    pub fn cursor_kind(&self) -> CursorKind {
        self.cursor_kind
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()?;
        self.cursor_kind = CursorKind::Hidden;
        Ok(())
    }

    pub fn show_cursor(&mut self, kind: CursorKind) -> io::Result<()> {
        self.backend.show_cursor(kind)?;
        self.cursor_kind = kind;
        Ok(())
    }

    pub fn set_cursor(&mut self, x: u16, y: u16) -> io::Result<()> {
        self.backend.set_cursor(x, y)
    }

    /// Clear the terminal and force a full redraw on the next draw call.
    ///
    /// The physical erase is deferred to the next `flush` so it shares a
    /// synchronized frame with the repaint.
    pub fn clear(&mut self) -> io::Result<()> {
        self.force_clear = true;
        // Reset the back buffer to make sure the next update will redraw everything.
        self.buffers[1 - self.current].reset();
        Ok(())
    }

    /// Queries the real size of the backend.
    pub fn size(&self) -> Rect {
        self.backend.size().unwrap_or(DEFAULT_TERMINAL_SIZE)
    }
}

#[cfg(test)]
mod cursor_graphics_tests {
    use super::*;
    use crate::backend::{CellSize, TestBackend};
    use helix_core::Position;

    fn image() -> CursorImage<'static> {
        CursorImage {
            position: Position::new(1, 2),
            offset_x: 1,
            offset_y: 2,
            width: 2,
            height: 1,
            rgba: &[255, 0, 0, 255, 0, 255, 0, 32],
        }
    }

    #[test]
    fn image_only_frames_preserve_text_and_native_cursor_position() {
        let mut backend = TestBackend::new(10, 5);
        backend.set_cursor_graphics_cell_size(Some(CellSize {
            width: 8,
            height: 16,
        }));
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .current_buffer_mut()
            .get_mut(2, 1)
            .unwrap()
            .set_symbol("界");
        terminal
            .draw_with_cursor_graphics(Some((4, 3)), CursorKind::Hidden, Some(&image()))
            .unwrap();
        let text = terminal.backend().buffer().clone();
        let draws = terminal.backend().draw_calls();
        assert!(!terminal.backend().cursor_visible());
        assert_eq!(terminal.backend().cursor_position(), (4, 3));
        let mut moved = image();
        moved.position.col += 1;
        terminal.draw_cursor_graphics(Some(&moved)).unwrap();
        assert_eq!(terminal.backend().buffer(), &text);
        assert_eq!(terminal.backend().draw_calls(), draws);
        assert_eq!(terminal.backend().cursor_position(), (4, 3));
        assert_eq!(terminal.backend().cursor_image().unwrap().rgba[7], 32);
        assert_eq!(terminal.backend().graphics_frame_count(), 2);
        assert!(terminal.backend().graphics_frames_synchronized());
        assert!(!terminal.backend().synchronized_output_active());
    }

    #[test]
    fn regular_draw_resize_and_restore_remove_owned_cursor_image() {
        let mut backend = TestBackend::new(10, 5);
        backend.set_cursor_graphics_cell_size(Some(CellSize {
            width: 8,
            height: 16,
        }));
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw_cursor_graphics(Some(&image())).unwrap();
        terminal.draw(Some((2, 1)), CursorKind::Block).unwrap();
        assert!(terminal.backend().cursor_image().is_none());
        assert!(terminal.backend().cursor_visible());
        terminal.draw_cursor_graphics(Some(&image())).unwrap();
        terminal.resize(Rect::new(0, 0, 8, 4)).unwrap();
        assert!(terminal.backend().cursor_image().is_none());
        terminal.draw_cursor_graphics(Some(&image())).unwrap();
        terminal.restore().unwrap();
        assert!(terminal.backend().cursor_image().is_none());
        assert_eq!(terminal.backend().graphics_delete_count(), 3);
    }

    #[test]
    fn malformed_image_releases_synchronized_output_and_unsupported_backend_ignores_it() {
        let mut terminal = Terminal::new(TestBackend::new(10, 5)).unwrap();
        terminal.draw(Some((2, 1)), CursorKind::Block).unwrap();
        terminal.draw_cursor_graphics(Some(&image())).unwrap();
        assert_eq!(terminal.backend().graphics_frame_count(), 0);
        assert!(terminal.backend().cursor_visible());
        terminal
            .backend_mut()
            .set_cursor_graphics_cell_size(Some(CellSize {
                width: 8,
                height: 16,
            }));
        let mut malformed = image();
        malformed.width = 4;
        assert_eq!(
            terminal
                .draw_cursor_graphics(Some(&malformed))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!terminal.backend().synchronized_output_active());
        assert!(terminal.backend().cursor_image().is_none());
    }
}
