use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ChatId, LoopId, ModelIdentity, Protocol, RunId, Usage};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RequestSequence(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestOwner {
    pub chat_id: ChatId,
    pub loop_id: Option<LoopId>,
    pub run_id: Option<RunId>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRequestKind {
    Conversation,
    Title,
    AutoSetup,
    CompactionAutomatic,
    CompactionManual,
    CompactionDirected,
    PermissionCheck,
    /// `WebFetch` asking the Chat model about a fetched page (Claude Code `web_fetch_apply`).
    WebFetch,
    /// Names a background Shell that has no description of its own.
    ShellDescription,
    /// The completion check of a goal the Agent reported complete.
    GoalCheck,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCounter {
    Input,
    Output,
    CacheRead,
    CacheWrite,
}

impl UsageCounter {
    const ALL: [Self; 4] = [Self::Input, Self::Output, Self::CacheRead, Self::CacheWrite];

    fn value(self, usage: &Usage) -> Option<u64> {
        match self {
            Self::Input => usage.input_tokens,
            Self::Output => usage.output_tokens,
            Self::CacheRead => usage.cache_read_input_tokens,
            Self::CacheWrite => usage.cache_write_input_tokens,
        }
    }

    fn value_mut(self, usage: &mut Usage) -> &mut Option<u64> {
        match self {
            Self::Input => &mut usage.input_tokens,
            Self::Output => &mut usage.output_tokens,
            Self::CacheRead => &mut usage.cache_read_input_tokens,
            Self::CacheWrite => &mut usage.cache_write_input_tokens,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseRejection {
    InvalidToolCall,
    EmptySummary,
    InvalidSummary,
    InvalidVerdict,
    ContextChanged,
    SummaryTooLarge,
    OutputLimit,
    ContextLimit,
    ContentFilter,
    Refusal,
    UnsupportedContinuation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolFailure {
    InvalidResponse,
    UsageRegression(UsageCounter),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestFailure {
    Internal,
    LocalConfiguration,
    Authentication,
    RateLimited,
    Http { status: u16 },
    Connection,
    StreamInterrupted,
    Protocol(ProtocolFailure),
    ProviderFailure,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    InFlight,
    Accepted,
    HostRejected(ResponseRejection),
    Failed(RequestFailure),
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestUsageRecord {
    pub sequence: RequestSequence,
    pub chat_id: ChatId,
    pub loop_id: Option<LoopId>,
    pub run_id: Option<RunId>,
    pub kind: ModelRequestKind,
    pub model: ModelIdentity,
    pub outcome: RequestOutcome,
    pub usage: Usage,
    #[serde(skip)]
    pub serialized_request_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_measurement: Option<crate::ContextMeasurement>,
    /// How this request broke the prompt cache of the Chat's previous request, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_break: Option<CacheBreakNote>,
}

/// What a View shows of a prompt cache break: the part of the request prefix from which the
/// cached prefix no longer matched, why the cache most likely broke, and how long after the
/// Chat's previous request it came.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CacheBreakNote {
    pub first_changed: Option<String>,
    pub likely_cause: String,
    #[serde(default)]
    pub gap_seconds: u64,
}

impl RequestUsageRecord {
    pub fn context_input_tokens(&self) -> Option<u64> {
        let input = self.usage.input_tokens?;
        Some(if self.model.protocol == Protocol::Messages {
            input
                .saturating_add(self.usage.cache_read_input_tokens.unwrap_or(0))
                .saturating_add(self.usage.cache_write_input_tokens.unwrap_or(0))
        } else {
            input
        })
    }

    /// The input the service processed afresh rather than read from its prompt cache. Messages
    /// reports cache reads and cache writes apart from `input_tokens`, and a cache write is fresh
    /// input; the other protocols report the input they read from cache within it.
    pub fn fresh_input_tokens(&self) -> Option<u64> {
        let input = self.usage.input_tokens?;
        Some(if self.model.protocol == Protocol::Messages {
            input.saturating_add(self.usage.cache_write_input_tokens.unwrap_or(0))
        } else {
            input
        })
    }
}

/// The in-process request usage ledger. Following Claude Code, usage is recorded only on assistant
/// messages and saved with the conversation; the ledger is not persisted, and a resumed Chat starts
/// with an empty ledger.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestUsageLedger {
    last_sequence: u64,
    entries: Vec<RequestUsageRecord>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ObservationEffect {
    pub changed: bool,
    pub regression: Option<UsageCounter>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LedgerError {
    #[error("invalid request owner for {0:?}")]
    InvalidOwner(ModelRequestKind),
    #[error(
        "request usage ledger belongs to Chat {}, not {}",
        expected.public(),
        actual.public()
    )]
    ChatMismatch { expected: ChatId, actual: ChatId },
    #[error("request sequence exhausted")]
    SequenceOverflow,
    #[error("unknown request sequence {0:?}")]
    UnknownRequest(RequestSequence),
    #[error("request {0:?} has already finished")]
    RequestAlreadyFinished(RequestSequence),
    #[error("request {0:?} does not match the expected owner, kind, or model")]
    RequestMismatch(RequestSequence),
    #[error("request finish requires a terminal outcome")]
    NonTerminalOutcome,
    #[error("Run conversation {0:?} usage exceeds the supported counter range")]
    RunUsageOverflow(UsageCounter),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CounterTotal {
    #[default]
    NotReported,
    Value(u128),
    Overflow,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UsageCoverage {
    #[default]
    NotReported,
    Partial,
    Complete,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CounterSummary {
    pub total: CounterTotal,
    pub reported_requests: usize,
    pub request_count: usize,
    pub coverage: UsageCoverage,
}

impl CounterSummary {
    fn include(&mut self, value: Option<u64>) {
        self.request_count += 1;
        if let Some(value) = value {
            self.reported_requests += 1;
            self.total = match self.total {
                CounterTotal::NotReported => CounterTotal::Value(u128::from(value)),
                CounterTotal::Value(total) => total
                    .checked_add(u128::from(value))
                    .map_or(CounterTotal::Overflow, CounterTotal::Value),
                CounterTotal::Overflow => CounterTotal::Overflow,
            };
        }
        self.coverage = match self.reported_requests {
            0 => UsageCoverage::NotReported,
            reported if reported == self.request_count => UsageCoverage::Complete,
            _ => UsageCoverage::Partial,
        };
    }

    fn run_total(self, counter: UsageCounter) -> Result<Option<u64>, LedgerError> {
        let value = match self.total {
            CounterTotal::NotReported => return Ok(None),
            CounterTotal::Overflow => return Err(LedgerError::RunUsageOverflow(counter)),
            CounterTotal::Value(value) => {
                u64::try_from(value).map_err(|_| LedgerError::RunUsageOverflow(counter))?
            }
        };
        Ok((self.coverage == UsageCoverage::Complete).then_some(value))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelUsageSummary {
    pub model: ModelIdentity,
    pub request_count: usize,
    pub kind_counts: BTreeMap<ModelRequestKind, usize>,
    pub outcome_counts: BTreeMap<RequestOutcome, usize>,
    pub input_tokens: CounterSummary,
    pub output_tokens: CounterSummary,
    pub cache_read_input_tokens: CounterSummary,
    pub cache_write_input_tokens: CounterSummary,
}

impl ModelUsageSummary {
    pub fn new(model: ModelIdentity) -> Self {
        Self {
            model,
            request_count: 0,
            kind_counts: BTreeMap::new(),
            outcome_counts: BTreeMap::new(),
            input_tokens: CounterSummary::default(),
            output_tokens: CounterSummary::default(),
            cache_read_input_tokens: CounterSummary::default(),
            cache_write_input_tokens: CounterSummary::default(),
        }
    }

    fn include(&mut self, record: &RequestUsageRecord) {
        self.request_count += 1;
        *self.kind_counts.entry(record.kind).or_default() += 1;
        *self.outcome_counts.entry(record.outcome).or_default() += 1;
        self.input_tokens.include(record.usage.input_tokens);
        self.output_tokens.include(record.usage.output_tokens);
        self.cache_read_input_tokens
            .include(record.usage.cache_read_input_tokens);
        self.cache_write_input_tokens
            .include(record.usage.cache_write_input_tokens);
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageSummary {
    pub request_count: usize,
    pub kind_counts: BTreeMap<ModelRequestKind, usize>,
    pub outcome_counts: BTreeMap<RequestOutcome, usize>,
    pub groups: Vec<ModelUsageSummary>,
}

impl RequestUsageLedger {
    /// Private planned-upgrade state, including request sizes omitted from ordinary wire views.
    pub fn upgrade_checkpoint(&self) -> serde_json::Value {
        serde_json::json!({
            "ledger": self,
            "request_bytes": self.entries.iter().map(|entry| (entry.sequence, entry.serialized_request_bytes)).collect::<Vec<_>>(),
        })
    }

    pub fn from_upgrade_checkpoint(value: serde_json::Value) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Checkpoint {
            ledger: RequestUsageLedger,
            request_bytes: Vec<(RequestSequence, Option<usize>)>,
        }
        let mut checkpoint: Checkpoint =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        if checkpoint.request_bytes.len() != checkpoint.ledger.entries.len() {
            return Err("usage checkpoint request counts differ".into());
        }
        for (entry, (sequence, bytes)) in checkpoint
            .ledger
            .entries
            .iter_mut()
            .zip(checkpoint.request_bytes)
        {
            if entry.sequence != sequence {
                return Err("usage checkpoint request identities differ".into());
            }
            entry.serialized_request_bytes = bytes;
        }
        Ok(checkpoint.ledger)
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(
        &mut self,
        owner: RequestOwner,
        kind: ModelRequestKind,
        identity: ModelIdentity,
    ) -> Result<RequestSequence, LedgerError> {
        let valid_owner = match kind {
            ModelRequestKind::CompactionManual
            | ModelRequestKind::CompactionDirected
            | ModelRequestKind::AutoSetup
            | ModelRequestKind::Title
            | ModelRequestKind::ShellDescription => {
                owner.loop_id.is_none() && owner.run_id.is_none()
            }
            _ => owner.loop_id.is_some() && owner.run_id.is_some(),
        };
        if !valid_owner {
            return Err(LedgerError::InvalidOwner(kind));
        }
        if let Some(first) = self.entries.first()
            && first.chat_id != owner.chat_id
        {
            return Err(LedgerError::ChatMismatch {
                expected: first.chat_id,
                actual: owner.chat_id,
            });
        }
        let sequence = self
            .last_sequence
            .checked_add(1)
            .map(RequestSequence)
            .ok_or(LedgerError::SequenceOverflow)?;
        self.entries.push(RequestUsageRecord {
            sequence,
            chat_id: owner.chat_id,
            loop_id: owner.loop_id,
            run_id: owner.run_id,
            kind,
            model: identity,
            outcome: RequestOutcome::InFlight,
            usage: Usage::default(),
            serialized_request_bytes: None,
            context_measurement: None,
            cache_break: None,
        });
        self.last_sequence = sequence.0;
        Ok(sequence)
    }

    pub fn observe(
        &mut self,
        sequence: RequestSequence,
        usage: Usage,
    ) -> Result<ObservationEffect, LedgerError> {
        let record = self.in_flight(sequence)?;
        let mut effect = ObservationEffect::default();
        for counter in UsageCounter::ALL {
            let Some(incoming) = counter.value(&usage) else {
                continue;
            };
            let current = counter.value_mut(&mut record.usage);
            match *current {
                Some(previous) if incoming < previous => {
                    effect.regression.get_or_insert(counter);
                }
                Some(previous) if incoming == previous => {}
                _ => {
                    *current = Some(incoming);
                    effect.changed = true;
                }
            }
        }
        Ok(effect)
    }

    pub fn finish(
        &mut self,
        sequence: RequestSequence,
        outcome: RequestOutcome,
    ) -> Result<(), LedgerError> {
        if outcome == RequestOutcome::InFlight {
            return Err(LedgerError::NonTerminalOutcome);
        }
        self.in_flight(sequence)?.outcome = outcome;
        Ok(())
    }

    pub fn validate_in_flight(
        &self,
        sequence: RequestSequence,
        owner: RequestOwner,
        kind: ModelRequestKind,
        identity: &ModelIdentity,
    ) -> Result<(), LedgerError> {
        let index = self
            .entries
            .binary_search_by_key(&sequence, |record| record.sequence)
            .map_err(|_| LedgerError::UnknownRequest(sequence))?;
        let record = &self.entries[index];
        if record.outcome != RequestOutcome::InFlight {
            return Err(LedgerError::RequestAlreadyFinished(sequence));
        }
        if record.chat_id != owner.chat_id
            || record.loop_id != owner.loop_id
            || record.run_id != owner.run_id
            || record.kind != kind
            || record.model != *identity
        {
            return Err(LedgerError::RequestMismatch(sequence));
        }
        Ok(())
    }

    pub fn entries(&self) -> &[RequestUsageRecord] {
        &self.entries
    }

    pub fn fail_in_flight_requests(&mut self, run_id: RunId, failure: RequestFailure) {
        for request in &mut self.entries {
            if request.run_id == Some(run_id) && request.outcome == RequestOutcome::InFlight {
                request.outcome = RequestOutcome::Failed(failure);
            }
        }
    }

    pub fn record_serialized_request_bytes(
        &mut self,
        sequence: RequestSequence,
        bytes: usize,
    ) -> Result<(), LedgerError> {
        self.in_flight(sequence)?.serialized_request_bytes = Some(bytes);
        Ok(())
    }

    pub fn record_cache_break(
        &mut self,
        sequence: RequestSequence,
        note: CacheBreakNote,
    ) -> Result<(), LedgerError> {
        self.in_flight(sequence)?.cache_break = Some(note);
        Ok(())
    }

    pub fn record_context_measurement(
        &mut self,
        sequence: RequestSequence,
        measurement: crate::ContextMeasurement,
    ) -> Result<(), LedgerError> {
        self.in_flight(sequence)?.context_measurement = Some(measurement);
        Ok(())
    }

    pub fn add_context_output_bytes(
        &mut self,
        sequence: RequestSequence,
        bytes: usize,
    ) -> Result<(), LedgerError> {
        if let Some(measurement) = &mut self.in_flight(sequence)?.context_measurement {
            measurement.streamed_output_bytes =
                measurement.streamed_output_bytes.saturating_add(bytes);
        }
        Ok(())
    }

    /// Adds streamed output text: its bytes, and its tokens when the process counts tokens.
    pub fn add_context_output(
        &mut self,
        sequence: RequestSequence,
        text: &str,
    ) -> Result<(), LedgerError> {
        let tokens = crate::context_token_counter().map(|counter| counter.text_tokens(text));
        if let Some(measurement) = &mut self.in_flight(sequence)?.context_measurement {
            measurement.streamed_output_bytes =
                measurement.streamed_output_bytes.saturating_add(text.len());
            if let Some(tokens) = tokens {
                measurement.add_streamed_output_tokens(tokens);
            }
        }
        Ok(())
    }

    pub fn summary(&self) -> UsageSummary {
        let mut summary = UsageSummary {
            request_count: self.entries.len(),
            ..UsageSummary::default()
        };
        let mut groups = BTreeMap::new();
        for record in &self.entries {
            *summary.kind_counts.entry(record.kind).or_default() += 1;
            *summary.outcome_counts.entry(record.outcome).or_default() += 1;
            groups
                .entry(record.model.clone())
                .or_insert_with(|| ModelUsageSummary::new(record.model.clone()))
                .include(record);
        }
        summary.groups = groups.into_values().collect();
        summary
    }

    pub fn run_conversation_usage(&self, run_id: RunId) -> Result<Usage, LedgerError> {
        let mut counters = [CounterSummary::default(); 4];
        for record in self.entries.iter().filter(|record| {
            record.run_id == Some(run_id)
                && record.kind == ModelRequestKind::Conversation
                && record.outcome == RequestOutcome::Accepted
        }) {
            for (summary, counter) in counters.iter_mut().zip(UsageCounter::ALL) {
                summary.include(counter.value(&record.usage));
            }
        }
        Ok(Usage {
            input_tokens: counters[0].run_total(UsageCounter::Input)?,
            output_tokens: counters[1].run_total(UsageCounter::Output)?,
            cache_read_input_tokens: counters[2].run_total(UsageCounter::CacheRead)?,
            cache_write_input_tokens: counters[3].run_total(UsageCounter::CacheWrite)?,
        })
    }

    fn in_flight(
        &mut self,
        sequence: RequestSequence,
    ) -> Result<&mut RequestUsageRecord, LedgerError> {
        let index = self
            .entries
            .binary_search_by_key(&sequence, |record| record.sequence)
            .map_err(|_| LedgerError::UnknownRequest(sequence))?;
        let record = &mut self.entries[index];
        if record.outcome != RequestOutcome::InFlight {
            return Err(LedgerError::RequestAlreadyFinished(sequence));
        }
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_request_kinds_use_one_current_ledger_wire() {
        let mut ledger = RequestUsageLedger::new();
        ledger
            .start(owner(), ModelRequestKind::Conversation, identity())
            .unwrap();
        ledger
            .start(
                RequestOwner {
                    chat_id: owner().chat_id,
                    loop_id: None,
                    run_id: None,
                },
                ModelRequestKind::Title,
                identity(),
            )
            .unwrap();
        let wire = serde_json::to_value(&ledger).unwrap();
        assert_eq!(wire["entries"].as_array().unwrap().len(), 2);
        assert_eq!(wire["entries"][1]["kind"], "title");
        assert!(wire.get("auxiliary_entries").is_none());
        let restored: RequestUsageLedger = serde_json::from_value(wire).unwrap();
        assert_eq!(restored, ledger);
    }

    #[test]
    fn a_cache_break_note_rides_on_its_in_flight_request_and_is_absent_otherwise() {
        let mut ledger = RequestUsageLedger::new();
        let unbroken = ledger
            .start(owner(), ModelRequestKind::Conversation, identity())
            .unwrap();
        let broken = ledger
            .start(owner(), ModelRequestKind::Conversation, identity())
            .unwrap();
        let note = CacheBreakNote {
            first_changed: Some("system:host_state.mode".into()),
            likely_cause: "prefix_changed".into(),
            gap_seconds: 4,
        };
        ledger.record_cache_break(broken, note.clone()).unwrap();
        ledger.finish(broken, RequestOutcome::Accepted).unwrap();
        assert_eq!(
            ledger.record_cache_break(broken, note.clone()),
            Err(LedgerError::RequestAlreadyFinished(broken))
        );
        let wire = serde_json::to_value(&ledger).unwrap();
        assert!(wire["entries"][0].get("cache_break").is_none());
        assert_eq!(
            wire["entries"][1]["cache_break"],
            serde_json::json!({"first_changed":"system:host_state.mode","likely_cause":"prefix_changed","gap_seconds":4})
        );
        let restored: RequestUsageLedger = serde_json::from_value(wire).unwrap();
        assert_eq!(restored.entries()[0].sequence, unbroken);
        assert_eq!(restored.entries()[1].cache_break, Some(note));
    }
    use crate::Protocol;
    use uuid::Uuid;

    fn owner() -> RequestOwner {
        RequestOwner {
            chat_id: Uuid::from_u128(1).into(),
            loop_id: Some(Uuid::from_u128(2).into()),
            run_id: Some(Uuid::from_u128(3).into()),
        }
    }

    fn identity() -> ModelIdentity {
        ModelIdentity {
            provider: crate::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture-model".into(),
        }
    }

    #[test]
    fn internal_failure_finishes_only_the_failed_runs_pending_requests() {
        let mut ledger = RequestUsageLedger::new();
        let completed = ledger
            .start(owner(), ModelRequestKind::Conversation, identity())
            .unwrap();
        ledger.finish(completed, RequestOutcome::Accepted).unwrap();
        let pending = ledger
            .start(owner(), ModelRequestKind::PermissionCheck, identity())
            .unwrap();
        let usage = Usage {
            input_tokens: Some(17),
            ..Usage::default()
        };
        ledger.observe(pending, usage.clone()).unwrap();
        let other = ledger
            .start(
                RequestOwner {
                    run_id: Some(Uuid::new_v4().into()),
                    ..owner()
                },
                ModelRequestKind::Conversation,
                identity(),
            )
            .unwrap();
        let title = ledger
            .start(
                RequestOwner {
                    run_id: None,
                    loop_id: None,
                    ..owner()
                },
                ModelRequestKind::Title,
                identity(),
            )
            .unwrap();

        ledger.fail_in_flight_requests(owner().run_id.unwrap(), RequestFailure::Internal);
        ledger.fail_in_flight_requests(owner().run_id.unwrap(), RequestFailure::Internal);
        let outcomes = ledger
            .entries()
            .iter()
            .map(|entry| (entry.sequence, entry.outcome))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(outcomes[&completed], RequestOutcome::Accepted);
        assert_eq!(
            outcomes[&pending],
            RequestOutcome::Failed(RequestFailure::Internal)
        );
        assert_eq!(outcomes[&other], RequestOutcome::InFlight);
        assert_eq!(outcomes[&title], RequestOutcome::InFlight);
        assert_eq!(ledger.entries()[1].usage, usage);
        let wire = serde_json::to_value(&ledger).unwrap();
        assert_eq!(
            wire["entries"][1]["outcome"],
            serde_json::json!({"failed":"internal"})
        );
        assert_eq!(
            serde_json::from_value::<RequestUsageLedger>(wire).unwrap(),
            ledger
        );
    }

    fn request(
        ledger: &mut RequestUsageLedger,
        kind: ModelRequestKind,
        model: ModelIdentity,
        usage: Usage,
        outcome: RequestOutcome,
    ) -> RequestSequence {
        let mut request_owner = owner();
        if matches!(
            kind,
            ModelRequestKind::CompactionManual | ModelRequestKind::CompactionDirected
        ) {
            request_owner.loop_id = None;
            request_owner.run_id = None;
        }
        let sequence = ledger.start(request_owner, kind, model).unwrap();
        ledger.observe(sequence, usage).unwrap();
        if outcome != RequestOutcome::InFlight {
            ledger.finish(sequence, outcome).unwrap();
        }
        sequence
    }

    #[test]
    fn observations_are_absolute_independent_and_replayed_without_changes() {
        let mut ledger = RequestUsageLedger::new();
        let sequence = ledger
            .start(owner(), ModelRequestKind::Conversation, identity())
            .unwrap();
        let input = Usage {
            input_tokens: Some(100),
            output_tokens: Some(0),
            cache_read_input_tokens: Some(20),
            cache_write_input_tokens: None,
        };
        assert_eq!(
            ledger.observe(sequence, input.clone()).unwrap(),
            ObservationEffect {
                changed: true,
                regression: None,
            }
        );
        let saved = ledger.clone();
        assert_eq!(
            ledger.observe(sequence, input).unwrap(),
            ObservationEffect::default()
        );
        assert_eq!(ledger, saved);
        ledger
            .observe(
                sequence,
                Usage {
                    output_tokens: Some(30),
                    cache_write_input_tokens: Some(0),
                    ..Usage::default()
                },
            )
            .unwrap();
        assert_eq!(ledger.entries().len(), 1);
        assert_eq!(
            ledger.entries()[0].usage,
            Usage {
                input_tokens: Some(100),
                output_tokens: Some(30),
                cache_read_input_tokens: Some(20),
                cache_write_input_tokens: Some(0),
            }
        );
    }

    #[test]
    fn regression_preserves_previous_fields_and_independently_valid_new_facts() {
        let mut ledger = RequestUsageLedger::new();
        let sequence = request(
            &mut ledger,
            ModelRequestKind::Conversation,
            identity(),
            Usage {
                input_tokens: Some(100),
                cache_read_input_tokens: Some(20),
                ..Usage::default()
            },
            RequestOutcome::InFlight,
        );
        let regressed = Usage {
            input_tokens: Some(90),
            output_tokens: Some(30),
            cache_read_input_tokens: Some(10),
            cache_write_input_tokens: Some(0),
        };
        assert_eq!(
            ledger.observe(sequence, regressed.clone()).unwrap(),
            ObservationEffect {
                changed: true,
                regression: Some(UsageCounter::Input),
            }
        );
        assert_eq!(
            ledger.observe(sequence, regressed).unwrap(),
            ObservationEffect {
                changed: false,
                regression: Some(UsageCounter::Input),
            }
        );
        ledger
            .finish(
                sequence,
                RequestOutcome::Failed(RequestFailure::Protocol(ProtocolFailure::UsageRegression(
                    UsageCounter::Input,
                ))),
            )
            .unwrap();
        assert_eq!(
            ledger.entries()[0].usage,
            Usage {
                input_tokens: Some(100),
                output_tokens: Some(30),
                cache_read_input_tokens: Some(20),
                cache_write_input_tokens: Some(0),
            }
        );
    }

    #[test]
    fn transitions_reject_invalid_ownership_and_second_terminal_updates() {
        let mut ledger = RequestUsageLedger::new();
        for kind in [
            ModelRequestKind::CompactionManual,
            ModelRequestKind::CompactionDirected,
            ModelRequestKind::AutoSetup,
        ] {
            assert!(ledger.start(owner(), kind, identity()).is_err());
        }
        let manual_owner = RequestOwner {
            loop_id: None,
            run_id: None,
            ..owner()
        };
        for kind in [
            ModelRequestKind::Conversation,
            ModelRequestKind::CompactionAutomatic,
            ModelRequestKind::PermissionCheck,
            ModelRequestKind::WebFetch,
        ] {
            assert_eq!(
                ledger.start(manual_owner, kind, identity()),
                Err(LedgerError::InvalidOwner(kind))
            );
        }
        assert!(ledger.entries().is_empty());
        let sequence = ledger
            .start(manual_owner, ModelRequestKind::CompactionManual, identity())
            .unwrap();
        assert_eq!(sequence, RequestSequence(1));
        let directed = ledger
            .start(
                manual_owner,
                ModelRequestKind::CompactionDirected,
                identity(),
            )
            .unwrap();
        assert_eq!(directed, RequestSequence(2));
        ledger.finish(directed, RequestOutcome::Accepted).unwrap();
        assert_eq!(
            ledger.finish(sequence, RequestOutcome::InFlight),
            Err(LedgerError::NonTerminalOutcome)
        );
        assert_eq!(
            ledger.observe(RequestSequence(99), Usage::default()),
            Err(LedgerError::UnknownRequest(RequestSequence(99)))
        );
        ledger.finish(sequence, RequestOutcome::Cancelled).unwrap();
        let saved = ledger.clone();
        assert_eq!(
            ledger.finish(sequence, RequestOutcome::Accepted),
            Err(LedgerError::RequestAlreadyFinished(sequence))
        );
        assert_eq!(
            ledger.observe(sequence, Usage::default()),
            Err(LedgerError::RequestAlreadyFinished(sequence))
        );
        assert!(
            ledger
                .start(
                    RequestOwner {
                        chat_id: Uuid::from_u128(99).into(),
                        ..owner()
                    },
                    ModelRequestKind::Conversation,
                    identity(),
                )
                .is_err()
        );
        assert_eq!(ledger, saved);
    }

    #[test]
    fn prestarted_request_validation_requires_exact_inflight_identity() {
        let mut ledger = RequestUsageLedger::new();
        let request_owner = RequestOwner {
            loop_id: None,
            run_id: None,
            ..owner()
        };
        let sequence = ledger
            .start(
                request_owner,
                ModelRequestKind::CompactionDirected,
                identity(),
            )
            .unwrap();
        assert_eq!(
            ledger.validate_in_flight(
                sequence,
                request_owner,
                ModelRequestKind::CompactionDirected,
                &identity(),
            ),
            Ok(())
        );
        assert_eq!(
            ledger.validate_in_flight(
                RequestSequence(sequence.0 + 1),
                request_owner,
                ModelRequestKind::CompactionDirected,
                &identity(),
            ),
            Err(LedgerError::UnknownRequest(RequestSequence(sequence.0 + 1)))
        );
        assert_eq!(
            ledger.validate_in_flight(
                sequence,
                request_owner,
                ModelRequestKind::CompactionManual,
                &identity(),
            ),
            Err(LedgerError::RequestMismatch(sequence))
        );
        assert_eq!(
            ledger.validate_in_flight(
                sequence,
                RequestOwner {
                    chat_id: Uuid::from_u128(99).into(),
                    ..request_owner
                },
                ModelRequestKind::CompactionDirected,
                &identity(),
            ),
            Err(LedgerError::RequestMismatch(sequence))
        );
        assert_eq!(
            ledger.validate_in_flight(
                sequence,
                request_owner,
                ModelRequestKind::CompactionDirected,
                &ModelIdentity {
                    requested_model: "other".into(),
                    ..identity()
                },
            ),
            Err(LedgerError::RequestMismatch(sequence))
        );
        ledger.finish(sequence, RequestOutcome::Cancelled).unwrap();
        assert_eq!(
            ledger.validate_in_flight(
                sequence,
                request_owner,
                ModelRequestKind::CompactionDirected,
                &identity(),
            ),
            Err(LedgerError::RequestAlreadyFinished(sequence))
        );
    }

    #[test]
    fn summary_keeps_exact_identity_purpose_outcome_and_independent_coverage() {
        let mut ledger = RequestUsageLedger::new();
        request(
            &mut ledger,
            ModelRequestKind::Conversation,
            identity(),
            Usage {
                input_tokens: Some(100),
                output_tokens: Some(10),
                cache_read_input_tokens: Some(90),
                ..Usage::default()
            },
            RequestOutcome::Accepted,
        );
        request(
            &mut ledger,
            ModelRequestKind::PermissionCheck,
            identity(),
            Usage {
                input_tokens: Some(20),
                cache_write_input_tokens: Some(0),
                ..Usage::default()
            },
            RequestOutcome::TimedOut,
        );
        let variants = [
            ModelIdentity {
                provider: crate::ModelProvider::Api,
                profile: "another-profile".into(),
                ..identity()
            },
            ModelIdentity {
                protocol: Protocol::Responses,
                ..identity()
            },
            ModelIdentity {
                requested_model: "another-model".into(),
                ..identity()
            },
        ];
        for (index, model) in variants.into_iter().enumerate() {
            request(
                &mut ledger,
                if index == 0 {
                    ModelRequestKind::CompactionDirected
                } else {
                    ModelRequestKind::CompactionManual
                },
                model,
                Usage::default(),
                RequestOutcome::HostRejected(ResponseRejection::EmptySummary),
            );
        }
        let summary = ledger.summary();
        assert_eq!(summary.request_count, 5);
        assert_eq!(summary.groups.len(), 4);
        assert_eq!(summary.kind_counts[&ModelRequestKind::CompactionManual], 2);
        assert_eq!(
            summary.kind_counts[&ModelRequestKind::CompactionDirected],
            1
        );
        assert_eq!(summary.outcome_counts[&RequestOutcome::TimedOut], 1);
        let group = summary
            .groups
            .iter()
            .find(|group| group.model == identity())
            .unwrap();
        assert_eq!(group.request_count, 2);
        assert_eq!(group.kind_counts[&ModelRequestKind::PermissionCheck], 1);
        assert_eq!(group.input_tokens.total, CounterTotal::Value(120));
        assert_eq!(group.input_tokens.coverage, UsageCoverage::Complete);
        assert_eq!(group.output_tokens.total, CounterTotal::Value(10));
        assert_eq!(group.output_tokens.coverage, UsageCoverage::Partial);
        assert_eq!(group.cache_read_input_tokens.total, CounterTotal::Value(90));
        assert_eq!(
            group.cache_write_input_tokens,
            CounterSummary {
                total: CounterTotal::Value(0),
                reported_requests: 1,
                request_count: 2,
                coverage: UsageCoverage::Partial,
            }
        );
        assert_eq!(
            summary.groups[0].input_tokens,
            CounterSummary {
                total: CounterTotal::NotReported,
                reported_requests: 0,
                request_count: 1,
                coverage: UsageCoverage::NotReported,
            }
        );
    }

    #[test]
    fn run_totals_include_only_accepted_conversation_with_complete_field_coverage() {
        let mut ledger = RequestUsageLedger::new();
        let run_id = owner().run_id.unwrap();
        assert_eq!(
            ledger.run_conversation_usage(run_id).unwrap(),
            Usage::default()
        );
        for input_tokens in [10, 20, 30] {
            request(
                &mut ledger,
                ModelRequestKind::Conversation,
                identity(),
                Usage {
                    input_tokens: Some(input_tokens),
                    output_tokens: Some(5),
                    cache_read_input_tokens: (input_tokens != 20).then_some(3),
                    cache_write_input_tokens: Some(0),
                },
                RequestOutcome::Accepted,
            );
        }
        for (kind, outcome) in [
            (ModelRequestKind::Conversation, RequestOutcome::InFlight),
            (ModelRequestKind::Conversation, RequestOutcome::Cancelled),
            (
                ModelRequestKind::Conversation,
                RequestOutcome::HostRejected(ResponseRejection::InvalidToolCall),
            ),
            (
                ModelRequestKind::CompactionAutomatic,
                RequestOutcome::Accepted,
            ),
            (ModelRequestKind::CompactionManual, RequestOutcome::Accepted),
            (
                ModelRequestKind::CompactionDirected,
                RequestOutcome::Accepted,
            ),
            (ModelRequestKind::PermissionCheck, RequestOutcome::Accepted),
        ] {
            request(
                &mut ledger,
                kind,
                identity(),
                Usage {
                    input_tokens: Some(1_000),
                    ..Usage::default()
                },
                outcome,
            );
        }
        let other = ledger
            .start(
                RequestOwner {
                    run_id: Some(Uuid::from_u128(90).into()),
                    ..owner()
                },
                ModelRequestKind::Conversation,
                identity(),
            )
            .unwrap();
        ledger.finish(other, RequestOutcome::Accepted).unwrap();
        assert_eq!(
            ledger.run_conversation_usage(run_id).unwrap(),
            Usage {
                input_tokens: Some(60),
                output_tokens: Some(15),
                cache_read_input_tokens: None,
                cache_write_input_tokens: Some(0),
            }
        );
    }

    #[test]
    fn wide_totals_remain_exact_and_run_overflow_does_not_damage_entries() {
        let mut ledger = RequestUsageLedger::new();
        for _ in 0..2 {
            request(
                &mut ledger,
                ModelRequestKind::Conversation,
                identity(),
                Usage {
                    input_tokens: Some(u64::MAX),
                    ..Usage::default()
                },
                RequestOutcome::Accepted,
            );
        }
        let saved = ledger.clone();
        assert_eq!(
            ledger.summary().groups[0].input_tokens.total,
            CounterTotal::Value(u128::from(u64::MAX) * 2)
        );
        assert_eq!(
            ledger.run_conversation_usage(owner().run_id.unwrap()),
            Err(LedgerError::RunUsageOverflow(UsageCounter::Input))
        );
        assert_eq!(ledger, saved);
        let mut counter = CounterSummary {
            total: CounterTotal::Value(u128::MAX),
            reported_requests: 1,
            request_count: 1,
            coverage: UsageCoverage::Complete,
        };
        counter.include(Some(1));
        assert_eq!(counter.total, CounterTotal::Overflow);
        counter.include(None);
        counter.include(Some(0));
        assert_eq!(counter.total, CounterTotal::Overflow);
        assert_eq!(counter.coverage, UsageCoverage::Partial);
    }

    #[test]
    fn an_overflowing_reported_lower_bound_is_not_disguised_as_missing_usage() {
        let mut ledger = RequestUsageLedger::new();
        for input_tokens in [Some(u64::MAX), Some(1), None] {
            request(
                &mut ledger,
                ModelRequestKind::Conversation,
                identity(),
                Usage {
                    input_tokens,
                    ..Usage::default()
                },
                RequestOutcome::Accepted,
            );
        }
        let saved = ledger.clone();
        assert_eq!(
            ledger.summary().groups[0].input_tokens,
            CounterSummary {
                total: CounterTotal::Value(u128::from(u64::MAX) + 1),
                reported_requests: 2,
                request_count: 3,
                coverage: UsageCoverage::Partial,
            }
        );
        assert_eq!(
            ledger.run_conversation_usage(owner().run_id.unwrap()),
            Err(LedgerError::RunUsageOverflow(UsageCounter::Input))
        );
        assert_eq!(ledger, saved);
    }

    #[test]
    fn sequence_exhaustion_is_atomic() {
        let mut ledger = RequestUsageLedger {
            last_sequence: u64::MAX,
            ..RequestUsageLedger::new()
        };
        let saved = ledger.clone();
        assert_eq!(
            ledger.start(owner(), ModelRequestKind::Conversation, identity()),
            Err(LedgerError::SequenceOverflow)
        );
        assert_eq!(ledger, saved);
    }
}
