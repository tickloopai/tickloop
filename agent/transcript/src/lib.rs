//! The Transcript record format: an append-only sequence of Lines per Chat, following Claude
//! Code's session format, and the display projection derived from them.
//!
//! User, assistant and system entries form a `uuid`/`parentUuid` chain. Metadata stays outside
//! the chain, with the latest entry winning for each kind. Where Lines are kept and how a Chat is
//! assembled from them belong to the runtime; this crate only defines what a Line is.

pub mod display;
mod entry;

pub use entry::*;
