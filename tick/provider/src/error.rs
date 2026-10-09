use std::fmt;

use crate::exit_code;

/// What an authorization failure asks the user to do.
pub const AUTH_HINT: &str = "run `tick longbridge auth`";

#[derive(Debug)]
pub struct Error {
    pub code: u8,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: exit_code::USAGE,
            message: message.into(),
        }
    }

    pub fn auth(message: impl fmt::Display) -> Self {
        Self {
            code: exit_code::NOT_AUTHORIZED,
            message: format!("{message}; {AUTH_HINT}"),
        }
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self {
            code: exit_code::NETWORK,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::usage(error.to_string())
    }
}
