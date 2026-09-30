use crate::{
    backend::{Backend, CellSize, CursorImage},
    buffer::{Buffer, Cell},
    terminal::Config,
};
use helix_core::unicode::width::UnicodeWidthStr;
use helix_core::Position;
use helix_view::graphics::{CursorKind, Rect};
use std::{fmt::Write, io};

/// A backend used for the integration tests.
#[derive(Debug)]
pub struct TestBackend {
    width: u16,
    buffer: Buffer,
    height: u16,
    cursor: bool,
    pos: (u16, u16),
    cursor_graphics_cell_size: Option<CellSize>,
    cursor_image: Option<RecordedCursorImage>,
    graphics_frame_count: usize,
    graphics_delete_count: usize,
    synchronized: bool,
    graphics_frames_synchronized: bool,
    draw_calls: usize,
}

/// The last pixel cursor placement, kept separately from the text-cell buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCursorImage {
    pub position: Position,
    pub offset_x: u16,
    pub offset_y: u16,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Returns a string representation of the given buffer for debugging purpose.
fn buffer_view(buffer: &Buffer) -> String {
    let mut view = String::with_capacity(buffer.content.len() + buffer.area.height as usize * 3);
    for cells in buffer.content.chunks(buffer.area.width as usize) {
        let mut overwritten = vec![];
        let mut skip: usize = 0;
        view.push('"');
        for (x, c) in cells.iter().enumerate() {
            if skip == 0 {
                view.push_str(&c.symbol);
            } else {
                overwritten.push((x, &c.symbol))
            }
            skip = std::cmp::max(skip, c.symbol.width()).saturating_sub(1);
        }
        view.push('"');
        if !overwritten.is_empty() {
            write!(
                &mut view,
                " Hidden by multi-width symbols: {:?}",
                overwritten
            )
            .unwrap();
        }
        view.push('\n');
    }
    view
}

impl TestBackend {
    pub fn new(width: u16, height: u16) -> TestBackend {
        TestBackend {
            width,
            height,
            buffer: Buffer::empty(Rect::new(0, 0, width, height)),
            cursor: false,
            pos: (0, 0),
            cursor_graphics_cell_size: None,
            cursor_image: None,
            graphics_frame_count: 0,
            graphics_delete_count: 0,
            synchronized: false,
            graphics_frames_synchronized: true,
            draw_calls: 0,
        }
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn set_cursor_graphics_cell_size(&mut self, size: Option<CellSize>) {
        self.cursor_graphics_cell_size = size.filter(|size| size.width != 0 && size.height != 0);
        if self.cursor_graphics_cell_size.is_none() {
            let _ = self.draw_cursor_graphics(None);
        }
    }

    pub fn cursor_image(&self) -> Option<&RecordedCursorImage> {
        self.cursor_image.as_ref()
    }

    pub fn graphics_frame_count(&self) -> usize {
        self.graphics_frame_count
    }
    pub fn graphics_delete_count(&self) -> usize {
        self.graphics_delete_count
    }
    pub fn graphics_frames_synchronized(&self) -> bool {
        self.graphics_frames_synchronized
    }
    pub fn cursor_visible(&self) -> bool {
        self.cursor
    }
    pub fn cursor_position(&self) -> (u16, u16) {
        self.pos
    }
    pub fn draw_calls(&self) -> usize {
        self.draw_calls
    }
    pub fn synchronized_output_active(&self) -> bool {
        self.synchronized
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.buffer.resize(Rect::new(0, 0, width, height));
        self.width = width;
        self.height = height;
    }

    pub fn assert_buffer(&self, expected: &Buffer) {
        assert_eq!(expected.area, self.buffer.area);
        let diff = expected.diff(&self.buffer);
        if diff.is_empty() {
            return;
        }

        let mut debug_info = String::from("Buffers are not equal");
        debug_info.push('\n');
        debug_info.push_str("Expected:");
        debug_info.push('\n');
        let expected_view = buffer_view(expected);
        debug_info.push_str(&expected_view);
        debug_info.push('\n');
        debug_info.push_str("Got:");
        debug_info.push('\n');
        let view = buffer_view(&self.buffer);
        debug_info.push_str(&view);
        debug_info.push('\n');

        debug_info.push_str("Diff:");
        debug_info.push('\n');
        let nice_diff = diff
            .iter()
            .enumerate()
            .map(|(i, (x, y, cell))| {
                let expected_cell = expected.get(*x, *y);
                format!(
                    "{}: at ({}, {}) expected {:?} got {:?}",
                    i, x, y, expected_cell, cell
                )
            })
            .collect::<Vec<String>>()
            .join("\n");
        debug_info.push_str(&nice_diff);
        panic!("{}", debug_info);
    }
}

impl Backend for TestBackend {
    fn claim(&mut self) -> Result<(), io::Error> {
        Ok(())
    }

    fn reconfigure(&mut self, _config: Config) -> Result<(), io::Error> {
        if !_config.cursor_graphics {
            self.draw_cursor_graphics(None)?;
        }
        Ok(())
    }

    fn restore(&mut self) -> Result<(), io::Error> {
        self.draw_cursor_graphics(None)?;
        self.synchronized = false;
        Ok(())
    }

    fn draw<'a, I>(&mut self, content: I) -> Result<(), io::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.draw_calls += 1;
        for (x, y, c) in content {
            self.buffer[(x, y)] = c.clone();
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), io::Error> {
        self.cursor = false;
        Ok(())
    }

    fn show_cursor(&mut self, _kind: CursorKind) -> Result<(), io::Error> {
        self.cursor = true;
        Ok(())
    }

    fn set_cursor(&mut self, x: u16, y: u16) -> Result<(), io::Error> {
        self.pos = (x, y);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), io::Error> {
        self.draw_cursor_graphics(None)?;
        self.buffer.reset();
        Ok(())
    }

    fn start_sync(&mut self) -> Result<(), io::Error> {
        self.synchronized = true;
        Ok(())
    }

    fn end_sync(&mut self) -> Result<(), io::Error> {
        self.synchronized = false;
        Ok(())
    }

    fn size(&self) -> Result<Rect, io::Error> {
        Ok(Rect::new(0, 0, self.width, self.height))
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }

    fn supports_true_color(&self) -> bool {
        false
    }

    fn cursor_graphics_cell_size(&self) -> Option<CellSize> {
        self.cursor_graphics_cell_size
    }

    fn draw_cursor_graphics(&mut self, image: Option<&CursorImage<'_>>) -> io::Result<()> {
        if let Some(image) = image.filter(|_| self.cursor_graphics_cell_size.is_some()) {
            image.validate()?;
            self.cursor_image = Some(RecordedCursorImage {
                position: image.position,
                offset_x: image.offset_x,
                offset_y: image.offset_y,
                width: image.width,
                height: image.height,
                rgba: image.rgba.to_vec(),
            });
            self.graphics_frame_count += 1;
            self.graphics_frames_synchronized &= self.synchronized;
        } else if self.cursor_image.take().is_some() {
            self.graphics_delete_count += 1;
        }
        Ok(())
    }

    fn get_theme_mode(&self) -> Option<helix_view::theme::Mode> {
        None
    }

    fn set_background_color(&mut self, _color: Option<helix_view::theme::Color>) -> io::Result<()> {
        Ok(())
    }
}
