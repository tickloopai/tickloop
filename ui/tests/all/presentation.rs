use tickloop_ui::{
    format::{NumberLanguage, abbreviate, pad, truncate, width},
    settings::AmbiguousWidth,
};

#[test]
fn financial_columns_align_cjk_and_abbreviate_in_the_selected_language() {
    assert_eq!(pad("腾讯", 7, AmbiguousWidth::Narrow, false), "腾讯   ");
    assert_eq!(truncate("腾讯控股", 5, AmbiguousWidth::Narrow), "腾讯…");
    assert_eq!(truncate("腾讯控股", 5, AmbiguousWidth::Wide), "腾…");
    assert_eq!(width("·", AmbiguousWidth::Wide), 2);
    assert_eq!(abbreviate(123_450_000.0, NumberLanguage::Chinese), "1.23亿");
    assert_eq!(
        abbreviate(123_450_000.0, NumberLanguage::English),
        "123.45M"
    );
    assert_eq!(abbreviate(-15_000.0, NumberLanguage::Chinese), "-1.50万");
    assert_eq!(abbreviate(0.0, NumberLanguage::English), "0");
}

#[test]
fn price_direction_respects_both_color_modes() {
    use tickloop_ui::{
        palette::{DOWN_GREEN, NEUTRAL, UP_RED, price_color},
        settings::ColorMode,
    };
    assert_eq!(price_color(1.0, ColorMode::RedUp), UP_RED);
    assert_eq!(price_color(-1.0, ColorMode::RedUp), DOWN_GREEN);
    assert_eq!(price_color(1.0, ColorMode::GreenUp), DOWN_GREEN);
    assert_eq!(price_color(-1.0, ColorMode::GreenUp), UP_RED);
    assert_eq!(price_color(0.0, ColorMode::RedUp), NEUTRAL);
}

#[test]
fn wide_ambiguous_glyphs_reserve_their_second_buffer_cell() {
    use ratatui::{buffer::Buffer, layout::Rect, style::Style};
    let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
    tickloop_ui::format::write_text(
        &mut buffer,
        0,
        0,
        "·a",
        8,
        AmbiguousWidth::Wide,
        Style::default(),
    );
    assert_eq!(buffer[(0, 0)].symbol(), "·");
    assert_eq!(
        buffer[(1, 0)].diff_option,
        ratatui::buffer::CellDiffOption::Skip
    );
    assert_eq!(buffer[(2, 0)].symbol(), "a");
}

#[test]
fn the_tick_ring_lights_its_eight_dots_one_at_a_time_and_its_progress_keeps_them_lit() {
    use tickloop_ui::icons::{TICK_PROGRESS_FRAMES, TICK_SPINNER_FRAMES};
    let dots = |frame: &str| {
        let character = frame.chars().next().unwrap() as u32;
        assert!(
            (0x2800..=0x28ff).contains(&character),
            "{frame} is a braille cell"
        );
        character - 0x2800
    };
    let mut lit = 0;
    for (spinner, progress) in TICK_SPINNER_FRAMES.iter().zip(TICK_PROGRESS_FRAMES) {
        assert_eq!(dots(spinner).count_ones(), 1, "{spinner} lights one dot");
        assert_eq!(lit & dots(spinner), 0, "{spinner} lights a new dot");
        lit |= dots(spinner);
        assert_eq!(dots(progress), lit, "{progress} keeps every dot lit so far");
    }
    assert_eq!(lit, 0xff, "the ring covers all eight dots");
}

#[test]
fn status_icons_are_single_column_unicode_symbols() {
    use tickloop_ui::icons::Icon;
    assert_eq!(Icon::Connection.glyph(), "●");
    assert_eq!(Icon::MarketOpen.glyph(), "▶");
    assert_eq!(Icon::TickUpdate.glyph(), "⠁");
    for icon in [
        Icon::Connection,
        Icon::TickUpdate,
        Icon::MarketPre,
        Icon::MarketOpen,
        Icon::MarketClosed,
        Icon::Paper,
        Icon::Rise,
        Icon::Fall,
        Icon::Position,
        Icon::Kline,
    ] {
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(icon.glyph()),
            1,
            "{icon:?}"
        );
    }
}

#[test]
fn overwriting_a_reserved_wide_character_cell_makes_the_replacement_visible() {
    use ratatui::{
        buffer::{Buffer, CellDiffOption},
        layout::Rect,
        style::{Color, Style},
    };
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
    buffer.set_style(buffer.area, Style::new().bg(Color::Indexed(99)));
    tickloop_ui::format::write_text(
        &mut buffer,
        0,
        0,
        "┃",
        4,
        AmbiguousWidth::Wide,
        Style::new(),
    );
    assert_eq!(buffer[(1, 0)].diff_option, CellDiffOption::Skip);
    tickloop_ui::format::write_text(
        &mut buffer,
        1,
        0,
        "x",
        1,
        AmbiguousWidth::Wide,
        Style::new(),
    );
    assert_ne!(buffer[(1, 0)].diff_option, CellDiffOption::Skip);
    assert_eq!(buffer[(1, 0)].bg, Color::Indexed(99));
}
