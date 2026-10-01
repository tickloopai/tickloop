//! Startup check that the terminal locale can display the Unicode glyphs
//! every Tick Command relies on.

const VARIABLES: [&str; 3] = ["LC_ALL", "LC_CTYPE", "LANG"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocaleError {
    NotUtf8 {
        variable: &'static str,
        value: String,
    },
    Unknown,
}

impl std::fmt::Display for LocaleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotUtf8 { variable, value } => write!(
                formatter,
                "terminal locale is not UTF-8: {variable}={value}. Tick Loop draws charts with Unicode glyphs; set LANG to a UTF-8 locale such as en_US.UTF-8 and retry."
            ),
            Self::Unknown => formatter.write_str(
                "terminal locale is unknown: none of LC_ALL, LC_CTYPE or LANG is set. Tick Loop draws charts with Unicode glyphs; set LANG to a UTF-8 locale such as en_US.UTF-8 and retry.",
            ),
        }
    }
}

pub fn check_utf8() -> Result<(), LocaleError> {
    check_utf8_in(|variable| std::env::var(variable).ok())
}

pub fn check_utf8_in(lookup: impl Fn(&str) -> Option<String>) -> Result<(), LocaleError> {
    let effective = VARIABLES.into_iter().find_map(|variable| {
        lookup(variable)
            .filter(|value| !value.is_empty())
            .map(|value| (variable, value))
    });
    match effective {
        Some((_, value)) if is_utf8(&value) => Ok(()),
        Some((variable, value)) => Err(LocaleError::NotUtf8 { variable, value }),
        None => Err(LocaleError::Unknown),
    }
}

fn is_utf8(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase().replace('-', "");
    normalized.contains("utf8")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |variable| {
            pairs
                .iter()
                .find(|(name, _)| *name == variable)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn accepts_common_utf8_spellings() {
        for value in ["en_US.UTF-8", "zh_CN.utf8", "C.UTF-8", "UTF-8"] {
            assert_eq!(check_utf8_in(lookup(&[("LANG", value)])), Ok(()), "{value}");
        }
    }

    #[test]
    fn empty_variables_are_skipped_in_precedence_order() {
        assert_eq!(
            check_utf8_in(lookup(&[
                ("LC_ALL", ""),
                ("LC_CTYPE", "C"),
                ("LANG", "en_US.UTF-8")
            ])),
            Err(LocaleError::NotUtf8 {
                variable: "LC_CTYPE",
                value: "C".into()
            })
        );
    }
}
