//! Request preferences a provider honors: the language of names and units, and the data center.

use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    ZhCn,
    En,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Cn,
    Global,
}

impl Language {
    pub const NAMES: &'static [&'static str] = &["zh-CN", "en"];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::En => "en",
        }
    }
}

impl Region {
    pub const NAMES: &'static [&'static str] = &["cn", "global"];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cn => "cn",
            Self::Global => "global",
        }
    }
}

impl FromStr for Language {
    type Err = UnknownPreference;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "zh-CN" => Ok(Self::ZhCn),
            "en" => Ok(Self::En),
            other => Err(UnknownPreference::new(other, Self::NAMES)),
        }
    }
}

impl FromStr for Region {
    type Err = UnknownPreference;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "cn" => Ok(Self::Cn),
            "global" => Ok(Self::Global),
            other => Err(UnknownPreference::new(other, Self::NAMES)),
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Display for Region {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownPreference {
    value: String,
    expected: &'static [&'static str],
}

impl UnknownPreference {
    fn new(value: &str, expected: &'static [&'static str]) -> Self {
        Self {
            value: value.to_owned(),
            expected,
        }
    }
}

impl fmt::Display for UnknownPreference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unknown value '{}', expected one of: {}",
            self.value,
            self.expected.join(", ")
        )
    }
}

impl std::error::Error for UnknownPreference {}
