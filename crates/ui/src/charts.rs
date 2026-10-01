pub fn candle_glyph(open: f64, high: f64, low: f64, close: f64, row: f64) -> char {
    let top = open.max(close);
    let bottom = open.min(close);
    if high.ceil() >= row && row >= top.floor() {
        if top - row > 0.75 {
            '┃'
        } else if top - row > 0.25 {
            if high - row > 0.75 { '╽' } else { '╻' }
        } else if high - row > 0.75 {
            '│'
        } else if high - row > 0.25 {
            '╷'
        } else {
            ' '
        }
    } else if top.floor() >= row && row >= bottom.ceil() {
        '┃'
    } else if bottom.ceil() >= row && row >= low.floor() {
        if bottom - row < 0.25 {
            '┃'
        } else if bottom - row < 0.75 {
            if low - row < 0.25 { '╿' } else { '╹' }
        } else if low - row < 0.25 {
            '│'
        } else if low - row < 0.75 {
            '╵'
        } else {
            ' '
        }
    } else {
        ' '
    }
}

/// The dashed box-drawing line a labelled price mark is drawn with, chosen so
/// that an annotation never reads as a candle body or a wick.
pub const PRICE_MARK_LINE: char = '\u{254c}';

pub fn volume_glyph(fraction: f64) -> char {
    const LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let level = (fraction.clamp(0.0, 1.0) * 8.0).round() as usize;
    LEVELS[level]
}

/// A horizontal magnitude bar for a percentage move, one block per two percent.
pub fn magnitude_bar(percent: f64) -> String {
    const GLYPH: char = '▇';
    const PERCENT_PER_GLYPH: f64 = 2.0;
    const MAXIMUM_GLYPHS: usize = 8;
    let magnitude = percent.abs();
    if !magnitude.is_finite() || magnitude == 0.0 {
        return String::new();
    }
    let glyphs = (magnitude / PERCENT_PER_GLYPH).ceil() as usize;
    GLYPH.to_string().repeat(glyphs.clamp(1, MAXIMUM_GLYPHS))
}

pub fn moving_average(values: &[f64], window: usize) -> Vec<Option<f64>> {
    let mut averages = vec![None; values.len()];
    if window == 0 {
        return averages;
    }
    for (index, values) in values.windows(window).enumerate() {
        if values.iter().all(|value| value.is_finite()) {
            averages[index + window - 1] = Some(values.iter().sum::<f64>() / window as f64);
        }
    }
    averages
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrailleCell {
    pub glyph: char,
    pub line: usize,
}

pub fn braille_lines(
    lines: &[Vec<(u16, u16)>],
    columns: u16,
    rows: u16,
) -> Vec<Option<BrailleCell>> {
    const DOTS: [[u8; 4]; 2] = [[1, 2, 4, 64], [8, 16, 32, 128]];
    let mut contributions = vec![vec![0_u8; lines.len()]; usize::from(columns) * usize::from(rows)];
    for (line, points) in lines.iter().enumerate() {
        for (start, end) in points
            .iter()
            .zip(points.iter().skip(1).chain(points.last()))
        {
            let (mut x, mut y) = (i32::from(start.0), i32::from(start.1));
            let (end_x, end_y) = (i32::from(end.0), i32::from(end.1));
            let delta_x = (end_x - x).abs();
            let delta_y = -(end_y - y).abs();
            let step_x = if x < end_x { 1 } else { -1 };
            let step_y = if y < end_y { 1 } else { -1 };
            let mut error = delta_x + delta_y;
            loop {
                if x >= 0 && y >= 0 && x < i32::from(columns) * 2 && y < i32::from(rows) * 4 {
                    let cell = y as usize / 4 * usize::from(columns) + x as usize / 2;
                    contributions[cell][line] |= DOTS[x as usize % 2][y as usize % 4];
                }
                if x == end_x && y == end_y {
                    break;
                }
                let doubled_error = error * 2;
                if doubled_error >= delta_y {
                    error += delta_y;
                    x += step_x;
                }
                if doubled_error <= delta_x {
                    error += delta_x;
                    y += step_y;
                }
            }
        }
    }
    contributions
        .into_iter()
        .map(|masks| {
            let combined = masks.iter().fold(0_u8, |all, mask| all | mask);
            if combined == 0 {
                return None;
            }
            let line = masks
                .iter()
                .enumerate()
                .max_by_key(|(index, mask)| (mask.count_ones(), std::cmp::Reverse(*index)))
                .map(|(index, _)| index)
                .unwrap_or(0);
            Some(BrailleCell {
                glyph: char::from_u32(0x2800 + u32::from(combined)).unwrap_or(' '),
                line,
            })
        })
        .collect()
}
