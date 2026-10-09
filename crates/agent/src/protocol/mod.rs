//! Shared domain types and AgentAccess wire definitions that depend on nothing else in the crate.

mod ids;
mod limits;
mod tool;

pub use ids::ChatId;
pub use limits::*;
pub use tool::ToolDefinition;
