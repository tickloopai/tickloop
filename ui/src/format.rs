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

/// One key hint of a status line, with the shorter form a narrow Panel falls back to.
#[derive(Clone, Copy)]
pub struct Hint<'a> {
    pub full: &'a str,
    pub short: &'a str,
}

impl<'a> Hint<'a> {
    pub const fn new(full: &'a str, short: &'a str) -> Self {
        Self { full, short }
    }

    /// A hint with no shorter form: it is shown whole or not at all.
    pub const fn whole(text: &'a str) -> Self {
        Self::new(text, text)
    }
}

/// Joins the hints into at most `columns`: hints shorten from the last one back, then drop
/// from the end, so a hint is never cut mid-word. The help overlay keeps every key.
pub fn fit_hints(
    hints: &[Hint<'_>],
    separator: &str,
    columns: usize,
    ambiguous: AmbiguousWidth,
) -> String {
    let mut forms: Vec<&str> = hints.iter().map(|hint| hint.full).collect();
    let fits = |forms: &[&str]| width(&forms.join(separator), ambiguous) <= columns;
    for index in (0..hints.len()).rev() {
        if fits(&forms) {
            break;
        }
        forms[index] = hints[index].short;
    }
    while !fits(&forms) {
        forms.pop();
    }
    forms.join(separator)
}

/// Packs fields into as few lines of `columns` as the order allows, `gap` columns apart, and
/// returns the fields of each line; a field wider than a whole line gets a line of its own.
pub fn pack_fields<S: AsRef<str>>(
    fields: &[S],
    gap: usize,
    columns: usize,
    ambiguous: AmbiguousWidth,
) -> Vec<std::ops::Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, field) in fields.iter().enumerate() {
        let field_width = width(field.as_ref(), ambiguous);
        if index > start && used + gap + field_width > columns {
            lines.push(start..index);
            start = index;
            used = field_width;
        } else {
            used += if index > start { gap } else { 0 } + field_width;
        }
    }
    if start < fields.len() {
        lines.push(start..fields.len());
    }
    lines
}
