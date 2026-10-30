//! Tickloop Code AgentAccess commands, observations and connection framing.

mod access;
pub mod daemon_management;
mod frame;
mod limits;
mod upgrade;

pub use access::{
    AccessCapability, AccessError, AccessErrorCode, AfterTurnAction, AgentAccess, AgentCommand,
    AgentDetailQuery, AgentFilter, AgentSummary, CLIENT_HANDOFF_VERSION, ChunkedResponseKind,
    ClientHandoffHello, ClientMessage, CommandEnvelope, CommandReceipt, CommandResult,
    ConfigurationChange, Cursor, DEFAULT_PAGE_LIMIT, DisplayReceipt, DisplayUpdate, DisplayedView,
    EndReason, EndedAgent, EndedOutcome, EnvironmentValue, ExecutionAvailability, ExecutionRef,
    HistoryAction, HostLocation, HostWorkContext, InteractionSource, LaunchSpec, LiveChatSnapshot,
    LiveChatStatus, LoopAction, LoopRef, MAX_PAGE_LIMIT, Observation, ObservationChange,
    ObservationFailure, ObservationRequest, ObservationUpdate, ObservationValue, PROTOCOL_MAJOR,
    PROTOCOL_MINOR, Page, PendingRequestKind, PersistenceState, ProtocolLimits,
    QueuedCommandFailure, RECEIPT_CAPACITY, RECEIPT_RETENTION_SECONDS, REPLAY_CAPACITY,
    ReceiptState, RecordedMessage, RecordedTranscript, RequestRef, ResetReason, ServerHandoffHello,
    ServerHello, ServerMessage, SessionResume, TaskAction, TaskOrigin, TaskRef, TranscriptRef,
    UserInput, VIEW_LEASE_SECONDS, ViewRef, bounded_update, build_identity, set_build_identity,
};
pub use frame::{check_frame_length, decode_frame, encode_frame};
pub use limits::*;
pub use upgrade::{
    RUNTIME_HANDOFF_VERSION, RuntimeBinaryInfo, RuntimeVersion, WorkerRuntimeVersion,
};

#[cfg(feature = "test-support")]
pub mod test_support;

pub use access::{
    ExecutionProfile, MANAGED_LEASE_SECONDS, ManagedConfiguration, ManagedConfigurationChange,
    ManagedHistoryPage, ManagedModelInfo, ManagedSessionInfo, ManagedSessionPage, ManagedSkillInfo,
    ManagedToken,
};
