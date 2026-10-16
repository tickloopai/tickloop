//! Stands in for `dotenv` 0.15, which the Longbridge SDK calls whenever it builds a
//! configuration. The real crate loads a `.env` file from the working directory or any directory
//! above it into the process environment, where the SDK then reads its LONGBRIDGE_* settings,
//! such as where it writes logs that hold whole API responses. A Tick runs in the Agent's working
//! directory, a project's, so none of that may come from there: this crate loads nothing.

use std::{fmt, path::PathBuf};

#[derive(Debug)]
pub struct Error;

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Tick Loop does not load .env files")
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Loads nothing and says so.
pub fn dotenv() -> Result<PathBuf> {
    Err(Error)
}
