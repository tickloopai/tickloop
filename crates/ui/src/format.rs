//! Adapted width and number formatting; see THIRD_PARTY_NOTICES.md.
use crate::settings::AmbiguousWidth;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy)]
pub enum NumberLanguage {
    Chinese,
    English,
}

pub fn width(text: &str, ambiguous: AmbiguousWidth) -> usize {
    match ambiguous {
        AmbiguousWidth::Narrow => text.width(),
        AmbiguousWidth::Wide => text.width_cjk(),
    }
}

pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .collect()
}

pub fn truncate(text: &str, columns: usize, ambiguous: AmbiguousWidth) -> String {
    let text = clean(text);
    if width(&text, ambiguous) <= columns {
        return text;
    }
    let marker = "…";
    let marker_width = width(marker, ambiguous);
    if columns < marker_width {
        return " ".repeat(columns);
    }
    let budget = columns - marker_width;
    let mut result = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let next = width(grapheme, ambiguous);
        if used + next > budget {
            break;
        }
        result.push_str(grapheme);
        used += next;
    }
    result.push_str(marker);
    result
}

pub fn pad(text: &str, columns: usize, ambiguous: AmbiguousWidth, right: bool) -> String {
    let text = truncate(text, columns, ambiguous);
    let spaces = " ".repeat(columns.saturating_sub(width(&text, ambiguous)));
    if right {
        format!("{spaces}{text}")
    } else {
        format!("{text}{spaces}")
    }
}

pub fn abbreviate(number: f64, language: NumberLanguage) -> String {
    const CHINESE: &[(f64, &str)] = &[(100_000_000.0, "亿"), (10_000.0, "万")];
    const ENGLISH: &[(f64, &str)] = &[(1_000_000_000.0, "B"), (1_000_000.0, "M"), (1_000.0, "K")];
    if !number.is_finite() {
        return "--".into();
    }
    let units = match language {
        NumberLanguage::Chinese => CHINESE,
        NumberLanguage::English => ENGLISH,
    };
    for &(scale, unit) in units {
        if number.abs() >= scale {
            return format!("{:.2}{unit}", number / scale);
        }
    }
    if number.fract() == 0.0 {
        format!("{number:.0}")
    } else {
        format!("{number:.2}")
    }
}

pub fn write_text(
    buffer: &mut ratatui::buffer::Buffer,
    x: u16,
    y: u16,
    text: &str,
    columns: usize,
    ambiguous: AmbiguousWidth,
    style: ratatui::style::Style,
) {
    use ratatui::buffer::CellDiffOption;
    let area = buffer.area;
    if x < area.x || y < area.y || y >= area.bottom() || x >= area.right() {
        return;
    }
    let end = usize::from(area.right()).min(usize::from(x) + columns);
    let mut column = usize::from(x);
    for grapheme in text.graphemes(true) {
        let size = width(grapheme, ambiguous);
        if size == 0 {
            continue;
        }
        if column + size > end {
            break;
        }
        buffer[(column as u16, y)]
            .set_diff_option(CellDiffOption::None)
            .set_symbol(grapheme)
            .set_style(style);
        for reserved in 1..size {
            let cell = &mut buffer[((column + reserved) as u16, y)];
            let inherited_style = cell.style();
            cell.reset();
            cell.set_style(inherited_style.patch(style));
            cell.set_diff_option(CellDiffOption::Skip);
        }
        column += size;
    }
}
