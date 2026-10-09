//! An in-memory backend a Tick draws into when its Panel is smaller than its minimum layout;
//! the scroll viewport then copies the visible slice of the buffer onto the Panel.
use std::convert::Infallible;

use ratatui::{
    backend::{Backend, ClearType, WindowSize},
    buffer::{Buffer, Cell},
    layout::{Position, Rect, Size},
};

pub struct CanvasBackend {
    buffer: Buffer,
    cursor: Position,
    is_cursor_visible: bool,
}

impl CanvasBackend {
    pub fn new(size: Size) -> Self {
        Self {
            buffer: Buffer::empty(Rect::new(0, 0, size.width, size.height)),
            cursor: Position::ORIGIN,
            is_cursor_visible: false,
        }
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// The canvas cell of the cursor the view placed in its last frame, if it placed one.
    pub fn cursor(&self) -> Option<Position> {
        self.is_cursor_visible.then_some(self.cursor)
    }
}

impl Backend for CanvasBackend {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Infallible>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, cell) in content {
            self.buffer[(x, y)] = cell.clone();
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), Infallible> {
        self.is_cursor_visible = false;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Infallible> {
        self.is_cursor_visible = true;
        Ok(())
    }

    fn get_cursor_position(&mut self) -> Result<Position, Infallible> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Infallible> {
        self.cursor = position.into();
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Infallible> {
        self.buffer.reset();
        Ok(())
    }

    // A canvas lives for one frame and is never cleared in part.
    fn clear_region(&mut self, _: ClearType) -> Result<(), Infallible> {
        self.clear()
    }

    fn size(&self) -> Result<Size, Infallible> {
        Ok(self.buffer.area.as_size())
    }

    fn window_size(&mut self) -> Result<WindowSize, Infallible> {
        Ok(WindowSize {
            columns_rows: self.buffer.area.as_size(),
            pixels: Size::default(),
        })
    }

    fn flush(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
}
