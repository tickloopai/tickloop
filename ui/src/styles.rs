use ratatui::style::Style;

/// Resolved terminal styles at the renderer boundary, never persisted or sent over IPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RendererStyles {
    pub canvas: Style,
    pub text: Style,
    pub panel: Style,
    pub overlay: Style,
    pub muted_text: Style,
    pub title: Style,
    pub selection: Style,
    pub border: Style,
    pub status_bar: Style,
    pub error: Style,
    pub price_up: Style,
    pub price_down: Style,
    pub moving_averages: [Style; 3],
    pub chart_cursor: Style,
    pub price_mark: Style,
    pub strong: Style,
    pub emphasis: Style,
    pub link: Style,
    pub inline_code: Style,
}

impl RendererStyles {
    pub fn panel_base(self) -> Style {
        self.canvas.patch(self.text).patch(self.panel)
    }

    pub fn overlay_base(self) -> Style {
        self.canvas.patch(self.text).patch(self.overlay)
    }

    pub fn status_base(self) -> Style {
        self.canvas.patch(self.text).patch(self.status_bar)
    }

    pub fn price(self, direction: PriceDirection) -> Style {
        match direction {
            PriceDirection::Up => self.price_up,
            PriceDirection::Down => self.price_down,
            PriceDirection::Flat => Style::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PriceDirection {
    Up,
    Down,
    #[default]
    Flat,
}

impl PriceDirection {
    pub fn from_change(change: f64) -> Self {
        if !change.is_finite() || change == 0.0 {
            Self::Flat
        } else if change > 0.0 {
            Self::Up
        } else {
            Self::Down
        }
    }
}
