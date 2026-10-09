//! Shared domain types and AgentAccess wire definitions that depend on no other Agent crate.

mod artifact;
mod auto_settings;
mod file_change;
mod host;
mod ids;
mod image;
mod limits;
mod model;
mod model_availability;
mod model_card;
mod permissions;
mod provider_connection;
mod provider_failure;
mod skill;
mod tool;
mod tools;

pub use artifact::ArtifactTick;
pub use auto_settings::{
    AutoReviewerPreference, AutoRuleChange, AutoRuleGroupSettings, AutoRuleKind, AutoRuleSettings,
    AutoSettingsRevision, CustomAutoRule,
};
pub use file_change::{
    FileChangeAction, FileChangeArtifactRef, FileChangeReviewSummary, FileIdentity, FileVersion,
    PreparedFileChange,
};
pub use host::{HerdrPane, HostedResources, HostedScreenSnapshot};
pub use ids::ChatId;
pub use image::{Image, ImageMediaType};
pub use limits::*;
pub use model::{
    ApiKeyStatus, Content, ImageSource, InputModality, Message, ModelAuthSource, ModelConfig,
    ModelIdentity, ModelProvider, PrivateHistorySource, Protocol, ReasoningEffort, Role,
    ToolResultBlock, ToolResultContent, Usage,
};
pub use model_availability::{AccountModelDirectory, DirectorySemantics, ModelAvailability};
pub use model_card::{
    Adapter, ModelCard, ModelPatch, ProviderPatch, ThinkingBudgets, ThinkingControl,
};
pub use permissions::{Decision, ExecutionMode, PermissionPolicy, PermissionRule, PermissionScope};
pub use provider_connection::{ConnectionRegion, ModelDiscovery, ProductKind, ProviderConnection};
pub use provider_failure::{ProviderFailureClass, ProviderHttpFailure};
pub use skill::{InvokedSkill, SkillSource};
pub use tool::ToolDefinition;
pub use tools::{CommandSnapshot, CommandState, FileReadRecord, SearchConfig, ToolResult};
