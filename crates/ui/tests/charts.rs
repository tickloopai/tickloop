use tickloop_ui::charts::{candle_glyph, volume_glyph};

#[test]
fn candle_transitions_use_quarter_and_three_quarter_boundaries() {
    assert_eq!(candle_glyph(0.25, 1.0, -1.0, -1.0, 0.0), '│');
    assert_eq!(candle_glyph(0.2501, 1.0, -1.0, -1.0, 0.0), '╽');
    assert_eq!(candle_glyph(0.5, 0.75, -1.0, -1.0, 0.0), '╻');
    assert_eq!(candle_glyph(0.5, 0.7501, -1.0, -1.0, 0.0), '╽');
    assert_eq!(candle_glyph(2.0, 2.0, 0.0, 0.5, 0.0), '╿');
    assert_eq!(candle_glyph(2.0, 2.0, 0.25, 0.5, 0.0), '╹');
    assert_eq!(candle_glyph(2.0, 2.0, 0.0, 0.75, 0.0), '│');
}

#[test]
fn volume_uses_nine_vertical_levels_and_clamps_extremes() {
    assert_eq!(
        (0..=8)
            .map(|level| volume_glyph(level as f64 / 8.0))
            .collect::<String>(),
        " ▁▂▃▄▅▆▇█"
    );
    assert_eq!(volume_glyph(-1.0), ' ');
    assert_eq!(volume_glyph(2.0), '█');
    assert_eq!(volume_glyph(f64::NAN), ' ');
}

#[test]
fn moving_averages_start_only_after_a_complete_window() {
    use tickloop_ui::charts::moving_average;
    assert_eq!(
        moving_average(&[1.0, 2.0, 3.0, 4.0, 5.0, 11.0], 5),
        vec![None, None, None, None, Some(3.0), Some(5.0)]
    );
    assert_eq!(
        moving_average(&[1.0, f64::NAN, 3.0, 5.0], 2),
        vec![None, None, None, Some(4.0)]
    );
    assert_eq!(moving_average(&[1.0], 0), vec![None]);
}

#[test]
fn crossing_braille_lines_use_the_line_with_most_distinct_dots_and_stable_ties() {
    use tickloop_ui::charts::braille_lines;
    let cells = braille_lines(&[vec![(0, 0), (0, 1)], vec![(1, 0), (1, 3)]], 1, 1);
    let cell = cells[0].as_ref().unwrap();
    assert_eq!(cell.glyph, '\u{28bb}');
    assert_eq!(cell.line, 1);
    let tied = braille_lines(&[vec![(0, 0), (0, 1), (0, 0)], vec![(1, 0), (1, 1)]], 1, 1);
    assert_eq!(
        tied[0].as_ref().unwrap().line,
        0,
        "revisiting a dot does not increase a line's contribution"
    );
    assert!(braille_lines(&[], 0, 0).is_empty());
}
