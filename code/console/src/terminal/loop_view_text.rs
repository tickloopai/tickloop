use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::style::{Role, RoleLine};

pub(super) fn clipped(text: &str, width: usize, role: Role) -> RoleLine {
    RoleLine::role(ellipsize(&super::safe_line(text), width), role)
}

pub(super) fn wrapped(text: &str, width: usize, role: Role) -> Vec<RoleLine> {
    if width == 0 {
        return Vec::new();
    }
    text.split('\n')
        .flat_map(|line| wrap_text(&super::safe_line(line), width))
        .map(|text| RoleLine::role(ellipsize(&text, width), role))
        .collect()
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = grapheme.width();
        if column + grapheme_width > width && column > 0 {
            lines.push(String::new());
            column = 0;
        }
        lines
            .last_mut()
            .expect("there is always one display line")
            .push_str(grapheme);
        column += grapheme_width;
    }
    lines
}

pub(super) fn ellipsize(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let available = width - 1;
    let mut used = 0;
    let mut kept = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        used += grapheme.width();
        if used > width {
            return format!("{}…", text[..kept].trim_end());
        }
        if used <= available {
            kept = offset + grapheme.len();
        }
    }
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_and_clipping_preserve_whole_graphemes() {
        for grapheme in ["👩‍💻", "👍🏽", "🇨🇳"] {
            let input = format!("{grapheme}abc");
            let lines = wrapped(&input, 2, Role::Text);
            assert_eq!(
                lines.iter().map(RoleLine::text).collect::<Vec<_>>(),
                [grapheme, "ab", "c"]
            );
            assert_eq!(
                clipped(&input, 3, Role::Text).text(),
                format!("{grapheme}…")
            );
            assert_eq!(clipped(&input, 2, Role::Text).text(), "…");
            assert_eq!(clipped(grapheme, 2, Role::Text).text(), grapheme);
            assert_eq!(clipped(grapheme, 1, Role::Text).text(), "…");
            assert_eq!(clipped(grapheme, 0, Role::Text).text(), "");
            assert_eq!(wrapped(grapheme, 1, Role::Text)[0].text(), "…");
        }
    }
}
