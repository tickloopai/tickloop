//! A Panel smaller than the layout a Tick needs scrolls over a canvas drawn at that layout,
//! with the bottom row showing which part is visible and the keys that move it.
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect, Size},
};

use crate::{
    format::{width, write_text},
    settings::AmbiguousWidth,
    styles::RendererStyles,
};

const SCROLL_LEFT: char = '<';
const SCROLL_RIGHT: char = '>';
const SCROLL_UP: char = '{';
const SCROLL_DOWN: char = '}';
const TRACK: &str = "─";
const MIN_TRACK: u16 = 8;
const THUMB: &str = "━";
const HELP: &str = "< / >  Scroll a narrow Panel sideways · { / }  up / down";

/// How a Panel that cannot hold its Tick's layout splits into the visible slice and the
/// indicator row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewportLayout {
    /// The full layout the Tick draws into.
    pub canvas: Size,
    /// The part of the Panel that shows a slice of the canvas.
    pub visible: Rect,
    /// The bottom row: the horizontal thumb, the vertical position and the scroll keys.
    pub indicator: Rect,
}

impl ViewportLayout {
    /// `None` while the Panel holds the minimum layout, so a large enough Panel never scrolls.
    pub fn of(area: Rect, minimum: Size) -> Option<Self> {
        if area.width >= minimum.width && area.height >= minimum.height || area.height == 0 {
            return None;
        }
        let visible = Rect::new(area.x, area.y, area.width, area.height - 1);
        Some(Self {
            canvas: Size::new(
                visible.width.max(minimum.width),
                visible.height.max(minimum.height),
            ),
            visible,
            indicator: Rect::new(area.x, visible.bottom(), area.width, 1),
        })
    }

    pub fn scrolls_horizontally(&self) -> bool {
        self.canvas.width > self.visible.width
    }

    pub fn scrolls_vertically(&self) -> bool {
        self.canvas.height > self.visible.height
    }

    fn largest_offset(&self) -> Position {
        Position::new(
            self.canvas.width - self.visible.width,
            self.canvas.height - self.visible.height,
        )
    }
}

/// The scroll position of a Panel over its canvas; it lasts across frames and resizes.
#[derive(Debug, Default)]
pub struct ScrollViewport {
    offset: Position,
    layout: Option<ViewportLayout>,
    followed: Option<(Option<u16>, Option<u16>)>,
}

impl ScrollViewport {
    pub fn layout(&self) -> Option<ViewportLayout> {
        self.layout
    }

    /// The canvas cell shown at the top-left of the visible slice.
    pub fn offset(&self) -> Position {
        self.offset
    }

    /// Takes this frame's layout and keeps the offset inside the canvas.
    pub fn set_layout(&mut self, layout: Option<ViewportLayout>) {
        self.layout = layout;
        let Some(layout) = layout else {
            self.offset = Position::ORIGIN;
            self.followed = None;
            return;
        };
        let largest = layout.largest_offset();
        self.offset = Position::new(self.offset.x.min(largest.x), self.offset.y.min(largest.y));
    }

    /// Brings the focused canvas column and row into view when the focus moved since the
    /// last frame, so manual scrolling stays put while the focus does.
    pub fn follow(&mut self, column: Option<u16>, row: Option<u16>) {
        let Some(layout) = self.layout else {
            return;
        };
        if self.followed == Some((column, row)) {
            return;
        }
        self.followed = Some((column, row));
        let reveal = |offset: u16, focus: Option<u16>, extent: u16| match focus {
            Some(focus) if focus < offset => focus,
            Some(focus) if focus >= offset + extent => focus + 1 - extent,
            _ => offset,
        };
        let largest = layout.largest_offset();
        self.offset = Position::new(
            reveal(self.offset.x, column, layout.visible.width).min(largest.x),
            reveal(self.offset.y, row, layout.visible.height).min(largest.y),
        );
    }

    /// Where a canvas cell shows on the Panel, if it is in the visible slice.
    pub fn on_panel(&self, cell: Position) -> Option<Position> {
        let visible = self.layout?.visible;
        let column = cell.x.checked_sub(self.offset.x)?;
        let row = cell.y.checked_sub(self.offset.y)?;
        (column < visible.width && row < visible.height)
            .then(|| Position::new(visible.x + column, visible.y + row))
    }

    /// Scrolls half the visible slice for one of the scroll keys and reports whether the key
    /// was one; a key for an axis that does not scroll stays with the Tick.
    pub fn handle_key(&mut self, key: char) -> bool {
        let Some(layout) = self.layout else {
            return false;
        };
        let largest = layout.largest_offset();
        let columns = (layout.visible.width / 2).max(1);
        let rows = (layout.visible.height / 2).max(1);
        match key {
            SCROLL_LEFT if layout.scrolls_horizontally() => {
                self.offset.x = self.offset.x.saturating_sub(columns);
            }
            SCROLL_RIGHT if layout.scrolls_horizontally() => {
                self.offset.x = (self.offset.x + columns).min(largest.x);
            }
            SCROLL_UP if layout.scrolls_vertically() => {
                self.offset.y = self.offset.y.saturating_sub(rows);
            }
            SCROLL_DOWN if layout.scrolls_vertically() => {
                self.offset.y = (self.offset.y + rows).min(largest.y);
            }
            _ => return false,
        }
        true
    }

    /// The help overlay line for the scroll keys while the Panel scrolls.
    pub fn help(&self) -> Option<&'static str> {
        self.layout.map(|_| HELP)
    }

    /// Copies the visible slice of `canvas` into `target` and draws the indicator row. A wide
    /// glyph cut by either edge of the slice shows as a blank cell.
    pub fn render(
        &self,
        canvas: &Buffer,
        target: &mut Buffer,
        styles: RendererStyles,
        ambiguous: AmbiguousWidth,
    ) {
        let Some(layout) = self.layout else {
            return;
        };
        let visible = layout.visible;
        for row in 0..visible.height {
            let canvas_y = self.offset.y + row;
            let mut covered_until = 0;
            for canvas_x in 0..self.offset.x + visible.width {
                let cell = &canvas[(canvas_x, canvas_y)];
                let is_continuation = canvas_x < covered_until;
                let glyph_width = if is_continuation {
                    0
                } else {
                    width(cell.symbol(), ambiguous).max(1) as u16
                };
                if !is_continuation {
                    covered_until = canvas_x + glyph_width;
                }
                if canvas_x < self.offset.x {
                    continue;
                }
                let target_cell =
                    &mut target[(visible.x + canvas_x - self.offset.x, visible.y + row)];
                let is_cut = if is_continuation {
                    canvas_x == self.offset.x
                } else {
                    canvas_x + glyph_width > self.offset.x + visible.width
                };
                if is_cut {
                    target_cell.reset();
                    target_cell.set_symbol(" ").set_style(cell.style());
                } else {
                    *target_cell = cell.clone();
                }
            }
        }
        self.render_indicator(layout, target, styles, ambiguous);
    }

    fn render_indicator(
        &self,
        layout: ViewportLayout,
        target: &mut Buffer,
        styles: RendererStyles,
        ambiguous: AmbiguousWidth,
    ) {
        let row = layout.indicator;
        let base = styles.status_base();
        target.set_style(row, base);
        write_text(
            target,
            row.x,
            row.y,
            &" ".repeat(row.width as usize),
            row.width as usize,
            ambiguous,
            base,
        );
        let position = layout.scrolls_vertically().then(|| {
            format!(
                "↕ {}–{}/{}",
                self.offset.y + 1,
                self.offset.y + layout.visible.height,
                layout.canvas.height
            )
        });
        let mut keys = Vec::new();
        if layout.scrolls_horizontally() {
            keys.push(format!("{SCROLL_LEFT} {SCROLL_RIGHT}"));
        }
        if let Some(position) = &position {
            keys.push(format!("{position} {SCROLL_UP} {SCROLL_DOWN}"));
        }
        let keys = keys.join(" ");
        // The label gives way to the thumb: first the word, then the keys, which the help
        // overlay also lists.
        let room = row.width.saturating_sub(if layout.scrolls_horizontally() {
            MIN_TRACK
        } else {
            0
        });
        let label = [
            format!(" {keys} scroll"),
            format!(" {keys}"),
            position
                .map(|position| format!(" {position}"))
                .unwrap_or_default(),
        ]
        .into_iter()
        .find(|label| width(label, ambiguous) <= usize::from(room))
        .unwrap_or_default();
        let label_width = width(&label, ambiguous) as u16;
        let track_width = if layout.scrolls_horizontally() {
            row.width.saturating_sub(label_width)
        } else {
            0
        };
        if track_width > 0 {
            let canvas_width = u32::from(layout.canvas.width);
            let scale = |columns: u16| {
                (u32::from(columns) * u32::from(track_width)).div_ceil(canvas_width) as u16
            };
            let thumb_width = scale(layout.visible.width).clamp(1, track_width);
            let largest = layout.largest_offset().x;
            let thumb_start = if self.offset.x >= largest {
                track_width - thumb_width
            } else {
                (u32::from(self.offset.x) * u32::from(track_width - thumb_width))
                    .div_ceil(u32::from(largest)) as u16
            };
            for column in 0..track_width {
                let is_thumb = (thumb_start..thumb_start + thumb_width).contains(&column);
                write_text(
                    target,
                    row.x + column,
                    row.y,
                    if is_thumb { THUMB } else { TRACK },
                    1,
                    ambiguous,
                    base.patch(if is_thumb {
                        styles.title
                    } else {
                        styles.border
                    }),
                );
            }
        }
        write_text(
            target,
            row.x + track_width,
            row.y,
            &label,
            row.width.saturating_sub(track_width) as usize,
            ambiguous,
            base.patch(styles.muted_text),
        );
    }
}
