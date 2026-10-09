use tick_market::config::{Overrides, Settings};
use tick_provider::preferences::{Language, Region};
use tickloop_ui::settings::{AmbiguousWidth, ColorMode, IconTheme};

#[test]
fn empty_configuration_yields_documented_defaults() {
    let settings = Settings::parse("").unwrap();
    assert_eq!(settings.color_mode, ColorMode::RedUp);
    assert_eq!(settings.icon_theme, IconTheme::Unicode);
    assert_eq!(settings.language, Language::ZhCn);
    assert_eq!(settings.ambiguous_width, AmbiguousWidth::Narrow);
    assert_eq!(settings.region, None);
}

#[test]
fn every_field_can_be_set_from_the_file() {
    let settings = Settings::parse(
        "color_mode = 'green-up'\nicon_theme = 'nerd'\nlanguage = 'en'\nambiguous_width = 'wide'\nregion = 'cn'\n",
    )
    .unwrap();
    assert_eq!(settings.color_mode, ColorMode::GreenUp);
    assert_eq!(settings.icon_theme, IconTheme::Nerd);
    assert_eq!(settings.language, Language::En);
    assert_eq!(settings.ambiguous_width, AmbiguousWidth::Wide);
    assert_eq!(settings.region, Some(Region::Cn));
}

#[test]
fn unknown_values_and_keys_are_rejected_with_the_offending_text() {
    for (text, needle) in [
        ("color_mode = 'purple'", "purple"),
        ("icon_theme = 'emoji'", "emoji"),
        ("language = 'fr'", "fr"),
        ("ambiguous_width = 'medium'", "medium"),
        ("region = 'mars'", "mars"),
        ("colour_mode = 'red-up'", "colour_mode"),
    ] {
        let error = Settings::parse(text).expect_err(text);
        assert!(error.contains(needle), "{text}: {error}");
    }
}

#[test]
fn command_line_overrides_win_over_the_file_field_by_field() {
    let file = Settings::parse("color_mode = 'green-up'\nlanguage = 'en'\n").unwrap();
    let overrides = Overrides {
        color_mode: Some(ColorMode::RedUp),
        icon_theme: Some(IconTheme::Nerd),
        ..Overrides::default()
    };
    let settings = file.with_overrides(overrides);
    assert_eq!(settings.color_mode, ColorMode::RedUp);
    assert_eq!(settings.icon_theme, IconTheme::Nerd);
    assert_eq!(settings.language, Language::En);
    assert_eq!(settings.ambiguous_width, AmbiguousWidth::Narrow);
}
