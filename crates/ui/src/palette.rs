use crate::settings::ColorMode;
use crate::styles::RendererStyles;
use ratatui::style::{Color, Modifier, Style};

pub const UP_RED: Color = Color::Rgb(240, 106, 116);
pub const DOWN_GREEN: Color = Color::Rgb(88, 194, 157);
pub const NEUTRAL: Color = Color::Rgb(205, 214, 224);
pub const MUTED: Color = Color::Rgb(126, 143, 159);
pub const ACCENT: Color = Color::Rgb(103, 183, 220);

pub fn price_color(change: f64, mode: ColorMode) -> Color {
    if !change.is_finite() || change == 0.0 {
        return NEUTRAL;
    }
    if (change > 0.0) == (mode == ColorMode::RedUp) {
        UP_RED
    } else {
        DOWN_GREEN
    }
}

/// Standalone CLI defaults until the client Theme adapter is integrated.
/// Renderers consume resolved styles, never this palette or a second theme parser.
pub fn standalone_styles(mode: ColorMode) -> RendererStyles {
    RendererStyles {
        canvas: Style::reset(),
        text: Style::new(),
        panel: Style::new(),
        overlay: Style::new(),
        muted_text: Style::new().fg(MUTED),
        title: Style::new().add_modifier(Modifier::BOLD),
        selection: Style::new().add_modifier(Modifier::BOLD),
        border: Style::new().fg(MUTED),
        status_bar: Style::new().fg(MUTED),
        error: Style::new(),
        price_up: Style::new().fg(price_color(1.0, mode)),
        price_down: Style::new().fg(price_color(-1.0, mode)),
        moving_averages: [
            Style::new().fg(Color::Rgb(248, 199, 95)),
            Style::new().fg(Color::Rgb(117, 187, 245)),
            Style::new().fg(Color::Rgb(211, 146, 241)),
        ],
        chart_cursor: Style::new().add_modifier(Modifier::BOLD),
        price_mark: Style::new().fg(ACCENT),
        strong: Style::new().add_modifier(Modifier::BOLD),
        emphasis: Style::new().add_modifier(Modifier::ITALIC),
        link: Style::new().fg(ACCENT).add_modifier(Modifier::UNDERLINED),
        inline_code: Style::new().fg(ACCENT),
    }
}
