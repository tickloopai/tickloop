//! Local provenance for explicitly connecting an existing artifact to a Loop.
//!
//! The service only reads; this module decides, against the Chat's own saved requests, what a
//! connect or check means before its model-visible result is persisted. The one remote write is
//! the existing hidden associate, dispatched by Runtime with the request saved here first.

use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    ChatSnapshot, EventSourceBinding, FrozenAssociationRequest, LoopId, LoopState, Receipt, RunId,
    SourceConnection, SourcePublication,
};
use crate::host_tools::{
    ConnectAction, ConnectSourceEffect, EventAssociationOutcome, EventAssociationRequest,
    PublishedSourceEffect, RemoteAssociation, RemoteAssociationOperation, connection_result,
};
use tickloop_agent_protocol::ToolResult;

pub(super) fn has_recorded_publication(
    snapshot: &ChatSnapshot,
    effect: &PublishedSourceEffect,
) -> bool {
    snapshot
        .loops
        .iter()
        .flat_map(|goal| &goal.event_sources)
        .any(|source| {
            source.server_id == effect.server_id
                && source.owner_account_id == effect.owner_account_id
                && source.source_key == effect.source_key
                && (source
                    .publication
                    .as_ref()
                    .is_some_and(|publication| publication.operation_id == effect.operation_id)
                    || source
                        .publication_history
                        .iter()
                        .any(|publication| publication.operation_id == effect.operation_id))
        })
}

/// Publication evidence does not assert a current association or reactivate its historical Loop.
pub(super) fn record_connected_publication(
    snapshot: &mut ChatSnapshot,
    effect: &PublishedSourceEffect,
    receipt: &Receipt,
) -> bool {
    let mut has_connection = false;
    for source in snapshot
        .loops
        .iter_mut()
        .flat_map(|goal| &mut goal.event_sources)
    {
        if source.connection.is_some()
            && source.server_id == effect.server_id
            && source.owner_account_id == effect.owner_account_id
            && source.source_key == effect.source_key
        {
            record_publication(source, effect, receipt);
            has_connection = true;
        }
    }
    has_connection
}

pub(super) fn record_publication(
    source: &mut EventSourceBinding,
    effect: &PublishedSourceEffect,
    receipt: &Receipt,
) {
    if let Some(previous) = source.publication.replace(SourcePublication {
        version_id: effect.version_id.clone(),
        operation_id: effect.operation_id.clone(),
        receipt: receipt.clone(),
    }) {
        source.publication_history.push(previous);
    }
}

pub(super) struct ConnectionDecision {
    pub(super) result: ToolResult,
    pub(super) dispatch: Option<EventAssociationRequest>,
    pub(super) previously_unknown: bool,
    /// Set when a check verified the pending connection of the current run itself.
    pub(super) start_watcher: Option<LoopId>,
}

impl ConnectionDecision {
    fn reply(result: ToolResult) -> Self {
        Self {
            result,
            dispatch: None,
            previously_unknown: false,
            start_watcher: None,
        }
    }
}

struct Outcome<'a> {
    effect: &'a ConnectSourceEffect,
}

impl Outcome<'_> {
    fn result(&self, status: &str, fields: Value, is_error: bool, summary: &str) -> ToolResult {
        connection_result(
            &self.effect.result_identity,
            status,
            fields,
            is_error,
            summary,
        )
    }

    fn prepared(&self) -> ToolResult {
        self.result(
            "prepared",
            json!({}),
            false,
            "Connection prepared; the association is not confirmed yet. Call check once with the same artifact and operation ID. Do not repeat connect.",
        )
    }

    fn failed(&self, code: &str) -> ToolResult {
        self.result(
            "failed",
            json!({"error": {"code": code}}),
            true,
            &format!(
                "The connection was definitely rejected ({code}); nothing new was associated."
            ),
        )
    }

    fn unknown(&self, reason: &str) -> ToolResult {
        self.result(
            "unknown",
            json!({"reason": reason}),
            true,
            &format!(
                "The association outcome cannot be confirmed: {reason}. The exact request is kept; do not retry automatically. Only the owner can decide on an exact retry with the same operation ID."
            ),
        )
    }

    fn not_dispatched(&self, reason: &str, fields: Value) -> ToolResult {
        self.result(
            "not_dispatched",
            fields,
            true,
            &format!("This attempt dispatched no association: {reason}. Any earlier unresolved request remains unresolved."),
        )
    }

    fn not_current(&self) -> ToolResult {
        self.result(
            "not_current",
            json!({"resume_required": true}),
            false,
            "This association request is historical or was replaced; it is not current and will not start listening.",
        )
    }

    fn associated_current(
        &self,
        binding: &EventSourceBinding,
        loop_id: LoopId,
        loop_state: &LoopState,
    ) -> ToolResult {
        self.result(
            "associated_current",
            json!({"resume_required": binding.resume_required}),
            false,
            &Self::association_summary(
                "The association is current for this Chat's Loop",
                binding,
                loop_id,
                loop_state,
            ),
        )
    }

    fn already_associated(
        &self,
        binding: &EventSourceBinding,
        loop_id: LoopId,
        loop_state: &LoopState,
    ) -> ToolResult {
        self.result(
            "already_associated",
            json!({
                "resume_required": binding.resume_required,
                "existing_association_operation_id": binding.association_operation_id,
            }),
            false,
            &Self::association_summary(
                "This Chat already holds the current association; no new association receipt was created and nothing moved to another Loop",
                binding,
                loop_id,
                loop_state,
            ),
        )
    }

    fn association_summary(
        status: &str,
        binding: &EventSourceBinding,
        _loop_id: LoopId,
        loop_state: &LoopState,
    ) -> String {
        if matches!(loop_state, LoopState::Stopped | LoopState::Expired) {
            format!(
                "{status}. Listening is inactive: the original Loop has ended and cannot be resumed. Checking an association does not restart it."
            )
        } else if binding.resume_required {
            format!(
                "{status}. Listening is not active; the owner can select the artifact in Monitor and press f to resume feedback. Association does not mean the Agent is online."
            )
        } else {
            format!("{status}. Association does not mean the Agent is online.")
        }
    }
}

fn is_same_account(binding: &EventSourceBinding, effect: &ConnectSourceEffect) -> bool {
    binding.server_id == effect.server_id && binding.owner_account_id == effect.owner_account_id
}

/// Finds a source of any Loop in this Chat; the predicate also receives the source's Loop ID.
fn locate(
    snapshot: &ChatSnapshot,
    matches: impl Fn(LoopId, &EventSourceBinding) -> bool,
) -> Option<(usize, usize)> {
    snapshot
        .loops
        .iter()
        .enumerate()
        .find_map(|(loop_index, loop_binding)| {
            loop_binding
                .event_sources
                .iter()
                .position(|binding| matches(loop_binding.id, binding))
                .map(|source_index| (loop_index, source_index))
        })
}

/// The remote current tuple is exactly this binding of this Chat's Loop at `epoch`.
fn is_current(
    binding: &EventSourceBinding,
    chat_id: Uuid,
    loop_id: LoopId,
    remote: Option<&RemoteAssociation>,
    epoch: u64,
) -> bool {
    remote.is_some_and(|remote| {
        remote.association_epoch == epoch
            && remote.binding_id == binding.binding_id.to_string()
            && remote.client_instance_id == binding.client_instance_id.to_string()
            && remote.chat_id == chat_id.to_string()
            && remote.loop_id == loop_id.to_string()
    })
}

fn dispatch_request(request: &FrozenAssociationRequest) -> EventAssociationRequest {
    EventAssociationRequest {
        server_id: request.server_id.clone(),
        owner_account_id: request.owner_account_id.clone(),
        source_key: request.source_key.clone(),
        binding_id: request.binding_id,
        client_instance_id: request.client_instance_id,
        chat_id: request.chat_id,
        loop_id: request.loop_id,
        operation_id: request.operation_id.clone(),
        expected_epoch: request.expected_epoch,
        credential_generation: request.credential_generation,
        directory_fingerprint: request.directory_fingerprint,
        bridge_fingerprint: request.bridge_fingerprint,
    }
}

pub(super) fn apply(
    snapshot: &mut ChatSnapshot,
    active_run: Option<RunId>,
    receipt: &Receipt,
    effect: ConnectSourceEffect,
    activation_blocked: bool,
) -> ConnectionDecision {
    match effect.action {
        ConnectAction::Connect { replace_existing } => connect(
            snapshot,
            active_run,
            receipt,
            &effect,
            replace_existing,
            activation_blocked,
        ),
        ConnectAction::Check => check(snapshot, active_run, receipt, &effect, activation_blocked),
    }
}

fn connect(
    snapshot: &mut ChatSnapshot,
    active_run: Option<RunId>,
    receipt: &Receipt,
    effect: &ConnectSourceEffect,
    replace_existing: bool,
    activation_blocked: bool,
) -> ConnectionDecision {
    let outcome = Outcome { effect };
    let chat_id = snapshot.id;
    let has_owner_prompt = snapshot.prompt_execution_modes.keys().any(|index| {
        snapshot.message_runs.get(index) == Some(&receipt.run_id)
            && !snapshot.inherited_prompts.contains(index)
    });
    if let Some((loop_index, source_index)) = locate(snapshot, |_, binding| {
        is_same_account(binding, effect)
            && binding
                .connection
                .as_ref()
                .is_some_and(|connection| connection.operation_id == effect.operation_id)
    }) {
        let loop_state = snapshot.loops[loop_index].state.clone();
        let binding = &mut snapshot.loops[loop_index].event_sources[source_index];
        let request = binding
            .connection
            .as_ref()
            .expect("located connection")
            .request
            .clone();
        if binding.source_key != effect.source_key || request.replace_existing != replace_existing {
            return ConnectionDecision::reply(outcome.failed("operation_conflict"));
        }
        if binding.association_operation_id != effect.operation_id
            || (binding.association_epoch.is_some() && !binding.association_unknown)
        {
            return check(snapshot, active_run, receipt, effect, activation_blocked);
        }
        if let Some(code) = &binding.association_rejected {
            return ConnectionDecision::reply(outcome.failed(code));
        }
        if binding.association_unknown
            && (!has_owner_prompt
                || binding
                    .connection
                    .as_ref()
                    .is_some_and(|connection| connection.receipt.run_id == receipt.run_id))
        {
            return ConnectionDecision::reply(outcome.unknown(
                "the saved request has an unknown outcome; an exact retry requires a new explicit owner request",
            ));
        }
        if matches!(loop_state, LoopState::Stopped | LoopState::Expired) || activation_blocked {
            return ConnectionDecision::reply(outcome.not_dispatched(
                "the original Loop is stopped, expired or this run was cancelled",
                json!({"reason": "lifecycle"}),
            ));
        }
        if request.credential_generation != effect.credential_generation
            || request.directory_fingerprint != effect.directory_fingerprint
            || request.bridge_fingerprint != effect.bridge_fingerprint
        {
            return ConnectionDecision::reply(outcome.not_dispatched(
                "the account credential, tool directory or bridge changed; the frozen request was retained",
                json!({"reason": "frozen_context_changed"}),
            ));
        }
        let previously_unknown = binding.association_unknown;
        binding.association_unknown = true;
        return ConnectionDecision {
            result: outcome.prepared(),
            dispatch: Some(dispatch_request(&request)),
            previously_unknown,
            start_watcher: None,
        };
    }

    let is_same_source = |binding: &EventSourceBinding| {
        is_same_account(binding, effect) && binding.source_key == effect.source_key
    };
    if let Some((loop_index, source_index)) = locate(snapshot, |_, binding| {
        is_same_source(binding) && binding.association_unknown
    }) {
        let pending = &snapshot.loops[loop_index].event_sources[source_index];
        return ConnectionDecision::reply(outcome.not_dispatched(
            "an earlier association request for this artifact is unresolved; check its original operation ID",
            json!({
                "reason": "unresolved_request",
                "existing_association_operation_id": pending.association_operation_id,
            }),
        ));
    }
    if let Some((loop_index, source_index)) = locate(snapshot, |loop_id, binding| {
        is_same_source(binding)
            && binding.association_epoch.is_some_and(|epoch| {
                is_current(
                    binding,
                    chat_id,
                    loop_id,
                    effect.current_association.as_ref(),
                    epoch,
                )
            })
    }) {
        let loop_id = snapshot.loops[loop_index].id;
        let current = &snapshot.loops[loop_index].event_sources[source_index];
        return ConnectionDecision::reply(outcome.already_associated(
            current,
            loop_id,
            &snapshot.loops[loop_index].state,
        ));
    }
    if effect.association_operation.is_some() {
        return ConnectionDecision::reply(outcome.failed("operation_conflict"));
    }
    let expected_epoch = match (&effect.current_association, replace_existing) {
        (Some(_), false) => {
            return ConnectionDecision::reply(outcome.failed("association_conflict"));
        }
        (Some(current), true) => Some(current.association_epoch),
        (None, _) => None,
    };
    let Some(goal) = snapshot
        .loops
        .iter_mut()
        .find(|goal| goal.id == receipt.loop_id)
        .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
    else {
        return ConnectionDecision::reply(outcome.not_dispatched(
            "the current Loop is not active",
            json!({"reason": "lifecycle"}),
        ));
    };
    if activation_blocked || active_run != Some(receipt.run_id) {
        return ConnectionDecision::reply(outcome.not_dispatched(
            "this run was cancelled before association",
            json!({"reason": "cancelled"}),
        ));
    }
    let request = FrozenAssociationRequest {
        server_id: effect.server_id.clone(),
        owner_account_id: effect.owner_account_id.clone(),
        source_key: effect.source_key.clone(),
        operation_id: effect.operation_id.clone(),
        binding_id: Uuid::new_v4(),
        client_instance_id: Uuid::new_v4(),
        chat_id: receipt.chat_id,
        loop_id: receipt.loop_id,
        expected_epoch,
        replace_existing,
        credential_generation: effect.credential_generation,
        directory_fingerprint: effect.directory_fingerprint,
        bridge_fingerprint: effect.bridge_fingerprint,
    };
    let connection = SourceConnection {
        operation_id: effect.operation_id.clone(),
        receipt: receipt.clone(),
        request: request.clone(),
    };
    match goal
        .event_sources
        .iter_mut()
        .find(|binding| is_same_source(binding))
    {
        Some(binding) => {
            binding.connection = Some(connection);
            binding.association_operation_id = request.operation_id.clone();
            binding.binding_id = request.binding_id;
            binding.client_instance_id = request.client_instance_id;
            binding.credential_generation = request.credential_generation;
            binding.directory_fingerprint = request.directory_fingerprint;
            binding.bridge_fingerprint = request.bridge_fingerprint;
            binding.association_epoch = None;
            binding.resume_required = true;
            binding.association_unknown = true;
            binding.association_rejected = None;
            binding.pending_cursor = None;
        }
        None => goal.event_sources.push(EventSourceBinding {
            server_id: request.server_id.clone(),
            owner_account_id: request.owner_account_id.clone(),
            source_key: request.source_key.clone(),
            publication: None,
            publication_history: Vec::new(),
            connection: Some(connection),
            association_operation_id: request.operation_id.clone(),
            binding_id: request.binding_id,
            client_instance_id: request.client_instance_id,
            credential_generation: request.credential_generation,
            directory_fingerprint: request.directory_fingerprint,
            bridge_fingerprint: request.bridge_fingerprint,
            association_epoch: None,
            resume_required: true,
            association_unknown: true,
            association_rejected: None,
            pending_cursor: None,
            claim_operations: Vec::new(),
        }),
    }
    ConnectionDecision {
        result: outcome.prepared(),
        dispatch: Some(dispatch_request(&request)),
        previously_unknown: false,
        start_watcher: None,
    }
}

fn check(
    snapshot: &mut ChatSnapshot,
    active_run: Option<RunId>,
    receipt: &Receipt,
    effect: &ConnectSourceEffect,
    activation_blocked: bool,
) -> ConnectionDecision {
    let outcome = Outcome { effect };
    let chat_id = snapshot.id;
    let Some((loop_index, source_index)) = locate(snapshot, |_, binding| {
        is_same_account(binding, effect)
            && (binding.association_operation_id == effect.operation_id
                || binding
                    .connection
                    .as_ref()
                    .is_some_and(|connection| connection.operation_id == effect.operation_id))
    }) else {
        return ConnectionDecision::reply(
            outcome.unknown("this Chat has no saved association request with this operation ID"),
        );
    };
    let loop_id = snapshot.loops[loop_index].id;
    let loop_state = snapshot.loops[loop_index].state.clone();
    let binding = &mut snapshot.loops[loop_index].event_sources[source_index];
    if binding.source_key != effect.source_key {
        return ConnectionDecision::reply(outcome.failed("operation_conflict"));
    }
    if binding.connection.is_none() {
        return ConnectionDecision::reply(
            outcome.unknown("this Chat has no saved connection request with this operation ID"),
        );
    }
    if binding.association_operation_id != effect.operation_id {
        return ConnectionDecision::reply(outcome.not_current());
    }
    match &effect.association_operation {
        Some(RemoteAssociationOperation::Committed { association_epoch }) => {
            binding.association_unknown = false;
            binding.association_rejected = None;
            binding.association_epoch = Some(*association_epoch);
            if !is_current(
                binding,
                chat_id,
                loop_id,
                effect.current_association.as_ref(),
                *association_epoch,
            ) {
                binding.resume_required = true;
                return ConnectionDecision::reply(outcome.not_current());
            }
            let is_initiating_run = binding
                .connection
                .as_ref()
                .is_some_and(|connection| connection.receipt.run_id == receipt.run_id)
                && active_run == Some(receipt.run_id)
                && !activation_blocked
                && !matches!(loop_state, LoopState::Stopped | LoopState::Expired);
            let start_watcher = (is_initiating_run && binding.resume_required).then(|| {
                binding.resume_required = false;
                loop_id
            });
            ConnectionDecision {
                result: outcome.associated_current(binding, loop_id, &loop_state),
                dispatch: None,
                previously_unknown: false,
                start_watcher,
            }
        }
        Some(RemoteAssociationOperation::Failed { error_code }) => {
            let code = error_code
                .clone()
                .unwrap_or_else(|| "association_failed".into());
            binding.association_unknown = false;
            binding.association_rejected = Some(code.clone());
            ConnectionDecision::reply(outcome.failed(&code))
        }
        Some(RemoteAssociationOperation::Pending | RemoteAssociationOperation::Unknown) => {
            ConnectionDecision::reply(outcome.unknown("the service has no final outcome yet"))
        }
        None => ConnectionDecision::reply(if let Some(code) = &binding.association_rejected {
            outcome.failed(code)
        } else if binding.association_unknown {
            outcome.unknown("the request may have been dispatched but no receipt was found")
        } else if binding.association_epoch.is_none() {
            outcome.not_dispatched(
                "this request was never dispatched",
                json!({"reason": "not_dispatched"}),
            )
        } else {
            outcome.unknown("the saved association has no service receipt to verify")
        }),
    }
}

/// Records the only associate outcome of a connection on its original Loop. Even a committed
/// receipt does not start listening; a check verifies the current tuple first.
pub(super) fn record_outcome(
    snapshot: &mut ChatSnapshot,
    request: &EventAssociationRequest,
    outcome: EventAssociationOutcome,
    previously_unknown: bool,
) -> bool {
    let Some(binding) = snapshot
        .loops
        .iter_mut()
        .filter(|goal| goal.id == request.loop_id)
        .flat_map(|goal| goal.event_sources.iter_mut())
        .find(|binding| {
            binding.association_operation_id == request.operation_id
                && binding.binding_id == request.binding_id
        })
    else {
        return false;
    };
    match outcome {
        EventAssociationOutcome::Committed(epoch) => {
            binding.association_epoch = Some(epoch);
            binding.association_unknown = false;
            binding.association_rejected = None;
        }
        EventAssociationOutcome::OutcomeUnknown(_) => binding.association_unknown = true,
        EventAssociationOutcome::NotDispatched(_) => {
            binding.association_unknown = previously_unknown
        }
        EventAssociationOutcome::Rejected(code) => {
            binding.association_unknown = false;
            binding.association_rejected = Some(code);
        }
    }
    true
}
