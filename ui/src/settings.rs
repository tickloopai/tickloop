//! Presentation settings shared by every Tick Command.
//!
//! These enums are parsed from plain strings so that configuration crates can
//! deserialize them without this crate depending on any serialization library.

use std::fmt;

/// Which direction of price movement is painted red.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    /// Rising prices are red and falling prices are green (default).
    #[default]
    RedUp,
    /// Rising prices are green and falling prices are red.
    GreenUp,
}

/// How East Asian Ambiguous width characters are measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AmbiguousWidth {
    #[default]
    Narrow,
    Wide,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownVariant {
    pub value: String,
    pub expected: &'static [&'static str],
}

impl fmt::Display for UnknownVariant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unknown value '{}', expected one of: {}",
            self.value,
            self.expected.join(", ")
        )
    }
}

impl std::error::Error for UnknownVariant {}

#[macro_export]
macro_rules! string_variants {
    ($type:ty { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $type {
            pub const NAMES: &'static [&'static str] = &[$($text),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl ::std::str::FromStr for $type {
            type Err = $crate::settings::UnknownVariant;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                match text {
                    $($text => Ok(Self::$variant),)+
                    other => Err($crate::settings::UnknownVariant { value: other.to_owned(), expected: Self::NAMES }),
                }
            }
        }

        impl ::std::fmt::Display for $type {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

string_variants!(ColorMode { RedUp => "red-up", GreenUp => "green-up" });
string_variants!(AmbiguousWidth { Narrow => "narrow", Wide => "wide" });
