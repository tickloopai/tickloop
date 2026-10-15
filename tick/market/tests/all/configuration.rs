use tick_market::config::{Overrides, Settings};
use tick_provider::preferences::{Language, Region};
use tickloop_ui::settings::{AmbiguousWidth, ColorMode};

#[test]
fn empty_configuration_yields_documented_defaults() {
    let settings = Settings::parse("").unwrap();
    assert_eq!(settings.color_mode, ColorMode::RedUp);
    assert_eq!(settings.language, Language::ZhCn);
    assert_eq!(settings.ambiguous_width, AmbiguousWidth::Narrow);
    assert_eq!(settings.region, None);
}

#[test]
fn every_field_can_be_set_from_the_file() {
    let settings = Settings::parse(
        "color_mode = 'green-up'\nlanguage = 'en'\nambiguous_width = 'wide'\nregion = 'cn'\n",
    )
    .unwrap();
    assert_eq!(settings.color_mode, ColorMode::GreenUp);
    assert_eq!(settings.language, Language::En);
    assert_eq!(settings.ambiguous_width, AmbiguousWidth::Wide);
    assert_eq!(settings.region, Some(Region::Cn));
}

/// Earlier versions offered a second icon glyph set; a file that still names one loads as if
/// it did not.
#[test]
fn an_icon_theme_an_earlier_version_wrote_is_read_past() {
    for value in ["'unicode'", "'private-use'", "3"] {
        let text = format!("icon_theme = {value}\ncolor_mode = 'green-up'\n");
        let settings = Settings::parse(&text).unwrap();
        assert_eq!(settings.color_mode, ColorMode::GreenUp, "{text}");
    }
}

#[test]
fn unknown_values_and_keys_are_rejected_with_the_offending_text() {
    for (text, needle) in [
        ("color_mode = 'purple'", "purple"),
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
        ..Overrides::default()
    };
    let settings = file.with_overrides(overrides);
    assert_eq!(settings.color_mode, ColorMode::RedUp);
    assert_eq!(settings.language, Language::En);
    assert_eq!(settings.ambiguous_width, AmbiguousWidth::Narrow);
}
