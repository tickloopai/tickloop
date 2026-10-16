use std::fmt;

use crate::exit_code;

#[derive(Debug)]
pub struct Error {
    pub code: u8,
    pub message: String,
    /// The source answered and turned the request down, such as an unknown symbol or a missing
    /// permission: asking again unchanged gets the same answer.
    pub rejected: bool,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            rejected: false,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(exit_code::USAGE, message)
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(exit_code::NOT_AUTHORIZED, message)
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::new(exit_code::NETWORK, message)
    }

    /// A request the source turned down; it exits like any other API error.
    pub fn rejected(message: impl Into<String>) -> Self {
        Self {
            rejected: true,
            ..Self::network(message)
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
