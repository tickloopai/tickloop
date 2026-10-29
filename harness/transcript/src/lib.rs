//! The Transcript record format: an append-only sequence of Lines per Chat, following Claude
//! Code's session format, and the projections derived from them: the display projection a user
//! interface shows, and the Claude Code projection hook scripts read.
//!
//! User, assistant and system entries form a `uuid`/`parentUuid` chain. Metadata stays outside
//! the chain, with the latest entry winning for each kind. Where Lines are kept and how a Chat is
//! assembled from them belong to the runtime; this crate only defines what a Line is.

pub mod claude_code;
pub mod display;
mod entry;
pub mod input_ids;

pub use entry::*;
