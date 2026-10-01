use std::time::{Duration, Instant};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::style::{Role, RoleLine};

const SWEEP_DURATION: Duration = Duration::from_secs(2);
const SHADES: [Role; 5] = [
    Role::RunningShade0,
    Role::RunningShade1,
    Role::RunningShade2,
    Role::RunningShade3,
    Role::RunningShade4,
];

pub(super) struct RunningShimmer {
    started_at: Instant,
}

impl Default for RunningShimmer {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }
}

impl RunningShimmer {
    pub(super) fn reset(&mut self) {
        self.started_at = Instant::now();
    }

    pub(super) fn append(&self, line: &mut RoleLine, text: &str, prefers_reduced_motion: bool) {
        self.append_at(
            line,
            text,
            self.started_at.elapsed(),
            prefers_reduced_motion,
        );
    }

    fn append_at(
        &self,
        line: &mut RoleLine,
        text: &str,
        elapsed: Duration,
        prefers_reduced_motion: bool,
    ) {
        let text_width = text.width() as f64;
        let half_width = (text_width * 0.1).max(3.0);
        let phase = elapsed.as_secs_f64() % SWEEP_DURATION.as_secs_f64();
        let position =
            phase / SWEEP_DURATION.as_secs_f64() * (text_width + 2.0 * half_width) - half_width;
        let mut column = 0.0;
        for grapheme in text.graphemes(true) {
            let glyph_width = grapheme.width() as f64;
            let center = column + glyph_width / 2.0;
            column += glyph_width;
            let shade = if prefers_reduced_motion {
                0
            } else {
                let distance = ((center - position).abs() / half_width).min(1.0);
                let intensity = 0.5 * (1.0 + (std::f64::consts::PI * distance).cos());
                (intensity * 4.0).round() as usize
            };
            line.push(grapheme, Some(SHADES[shade]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_moves_left_to_right_and_reduced_motion_is_static() {
        let shimmer = RunningShimmer::default();
        let render = |elapsed, reduced| {
            let mut line = RoleLine::default();
            shimmer.append_at(&mut line, "Working through the data", elapsed, reduced);
            line
        };
        let first = render(Duration::from_millis(300), false);
        let later = render(Duration::from_millis(1000), false);
        let bright_position = |line: &RoleLine| {
            let mut column = 0;
            line.spans.iter().find_map(|span| {
                let start = column;
                column += span.text.width();
                (span.role == Some(Role::RunningShade4)).then_some(start)
            })
        };
        assert!(bright_position(&first) < bright_position(&later));
        assert_eq!(
            render(Duration::from_millis(300), true),
            render(Duration::from_millis(1000), true)
        );
    }
}
