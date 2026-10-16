//! The shared `~/.tickloop/config.toml` file and its command-line overrides.

use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use serde::Deserialize;
use tick_provider::preferences::Language;
use tickloop_ui::settings::{AmbiguousWidth, ColorMode};

pub const DEFAULT_RELATIVE_PATH: &str = ".tickloop/config.toml";

/// Effective settings after merging the file with command-line overrides.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub renderer_styles: Option<tickloop_ui::styles::RendererStyles>,
    pub color_mode: ColorMode,
    pub language: Language,
    pub ambiguous_width: AmbiguousWidth,
}

/// Values given on the command line; `None` keeps the file or default value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    pub color_mode: Option<ColorMode>,
    pub language: Option<Language>,
    pub ambiguous_width: Option<AmbiguousWidth>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    color_mode: Option<String>,
    /// The icon glyph set earlier versions offered; one set is left, so a file that still names
    /// one loads and the value is not read.
    #[serde(rename = "icon_theme")]
    _icon_theme: Option<serde::de::IgnoredAny>,
    language: Option<String>,
    ambiguous_width: Option<String>,
    /// The Longbridge network earlier versions chose here; a login now chooses it, so a file
    /// that still names one loads and the value is not read.
    #[serde(rename = "region")]
    _region: Option<serde::de::IgnoredAny>,
}

impl Settings {
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: ConfigFile = toml::from_str(text).map_err(|error| error.to_string())?;
        Ok(Self {
            renderer_styles: None,
            color_mode: parse_field("color_mode", file.color_mode)?.unwrap_or_default(),
            language: parse_field("language", file.language)?.unwrap_or_default(),
            ambiguous_width: parse_field("ambiguous_width", file.ambiguous_width)?
                .unwrap_or_default(),
        })
    }

    /// Loads the file at `path`; a missing default-location file yields defaults,
    /// while an explicitly requested file must exist.
    pub fn load(path: &Path, is_explicit: bool) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !is_explicit => {
                Ok(Self::default())
            }
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    pub fn with_overrides(self, overrides: Overrides) -> Self {
        Self {
            renderer_styles: self.renderer_styles,
            color_mode: overrides.color_mode.unwrap_or(self.color_mode),
            language: overrides.language.unwrap_or(self.language),
            ambiguous_width: overrides.ambiguous_width.unwrap_or(self.ambiguous_width),
        }
    }

    pub fn styles(&self) -> tickloop_ui::styles::RendererStyles {
        self.renderer_styles
            .unwrap_or_else(|| tickloop_ui::palette::standalone_styles(self.color_mode))
    }
}

fn parse_field<T: FromStr<Err: std::fmt::Display>>(
    name: &str,
    value: Option<String>,
) -> Result<Option<T>, String> {
    value
        .map(|text| {
            text.parse::<T>()
                .map_err(|error| format!("{name}: {error}"))
        })
        .transpose()
}

pub fn default_path() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(DEFAULT_RELATIVE_PATH))
}
