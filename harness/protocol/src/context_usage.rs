use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ChatSnapshot, Content, ImageSource, Message, ModelConfig, ModelRequestKind, RequestOutcome,
    RequestUsageLedger, Role, ToolDefinition, ToolResultBlock, ToolResultContent, Usage,
};

pub const ESTIMATED_CONTEXT_BYTES_PER_TOKEN: usize = 2;
const EMPTY_CONTEXT_BYTES: usize = 4;
const IMAGE_PIXELS_PER_TOKEN: usize = 750;
/// The role and delimiter tokens a chat format spends on each message besides its content.
const MESSAGE_FRAMING_TOKENS: usize = 3;

/// Counts the tokens content takes in a model's context. A host installs one per process with
/// [`set_context_token_counter`]; until it does, current context is estimated from serialized
/// bytes.
pub trait ContextTokenCounter: Send + Sync {
    /// The tokenizer's name, as a view credits the estimate.
    fn name(&self) -> &'static str;

    fn text_tokens(&self, text: &str) -> usize;

    fn message_tokens(&self, message: &Message) -> usize {
        message_content_tokens(self, message)
    }

    fn definition_tokens(&self, definition: &ToolDefinition) -> usize {
        self.text_tokens(&definition.name)
            + self.text_tokens(&definition.description)
            + self.text_tokens(&definition.input_schema.to_string())
    }
}

static CONTEXT_TOKEN_COUNTER: OnceLock<&'static dyn ContextTokenCounter> = OnceLock::new();

/// Installs the process's context token counter, like `log::set_logger`: the first installation
/// wins and reports `true`.
pub fn set_context_token_counter(counter: &'static dyn ContextTokenCounter) -> bool {
    CONTEXT_TOKEN_COUNTER.set(counter).is_ok()
}

pub fn context_token_counter() -> Option<&'static dyn ContextTokenCounter> {
    CONTEXT_TOKEN_COUNTER.get().copied()
}

/// The tokens of a message's content as `counter` counts text: tool inputs and protocol records
/// as their JSON, images by their pixels.
pub fn message_content_tokens<C: ContextTokenCounter + ?Sized>(
    counter: &C,
    message: &Message,
) -> usize {
    let content = message
        .content
        .iter()
        .map(|block| match block {
            Content::Text { text } => counter.text_tokens(text),
            Content::ToolUse { name, input, .. } => {
                counter.text_tokens(name) + counter.text_tokens(&input.to_string())
            }
            Content::ToolResult { content, .. } => match content {
                ToolResultContent::Text(text) => counter.text_tokens(text),
                ToolResultContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|block| match block {
                        ToolResultBlock::Text { text } => counter.text_tokens(text),
                        ToolResultBlock::Image { width, height, .. } => {
                            estimated_image_tokens(*width, *height)
                        }
                    })
                    .sum(),
            },
            Content::Thinking { thinking, .. } => counter.text_tokens(thinking),
            Content::ProtocolData { value, .. } => counter.text_tokens(&value.to_string()),
            Content::Image { width, height, .. } => estimated_image_tokens(*width, *height),
        })
        .sum::<usize>();
    content + MESSAGE_FRAMING_TOKENS
}

pub fn estimated_image_tokens(width: u32, height: u32) -> usize {
    (width as usize).saturating_mul(height as usize) / IMAGE_PIXELS_PER_TOKEN
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
/// Request-prefix measurements let status reads count newly appended content without rescanning it.
pub struct ContextMeasurement {
    active_start: usize,
    message_end: usize,
    message_anchor: Option<Uuid>,
    compaction_index: Option<usize>,
    overhead_bytes: usize,
    request_bytes: usize,
    #[serde(default)]
    pub(crate) streamed_output_bytes: usize,
    /// Present when the process counted the request with its context token counter; older
    /// measurements hold bytes alone and keep being read as bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens: Option<MeasuredTokens>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct MeasuredTokens {
    request: usize,
    overhead: usize,
    #[serde(default)]
    streamed_output: usize,
}

/// What current context is measured in: serialized bytes for measurements recorded without a
/// token counter, tokens otherwise.
#[derive(Clone, Copy)]
enum ContextUnit<'a> {
    Bytes,
    Tokens(&'a dyn ContextTokenCounter),
}

impl ContextUnit<'_> {
    fn messages(self, messages: &[Message]) -> usize {
        match self {
            Self::Bytes => serialized_context_bytes(messages, &[]),
            Self::Tokens(counter) => messages
                .iter()
                .map(|message| counter.message_tokens(message))
                .sum(),
        }
    }

    /// What an empty message list measures, which appended content does not add.
    fn empty(self) -> usize {
        match self {
            Self::Bytes => EMPTY_CONTEXT_BYTES,
            Self::Tokens(_) => 0,
        }
    }

    fn uncalibrated_tokens(self, amount: usize) -> usize {
        match self {
            Self::Bytes => amount.div_ceil(ESTIMATED_CONTEXT_BYTES_PER_TOKEN),
            Self::Tokens(_) => amount,
        }
    }
}

impl ContextMeasurement {
    pub fn capture(snapshot: &ChatSnapshot, request_bytes: usize, overhead_bytes: usize) -> Self {
        Self {
            active_start: snapshot.active_start(),
            message_end: snapshot.messages.len(),
            message_anchor: snapshot
                .messages
                .len()
                .checked_sub(1)
                .and_then(|index| snapshot.message_ids.get(&index).copied()),
            compaction_index: snapshot.message_compactions.keys().next_back().copied(),
            overhead_bytes,
            request_bytes,
            streamed_output_bytes: 0,
            tokens: None,
        }
    }

    /// Adds the request's token counts: all of it, and the system prompt and tool definitions.
    pub fn with_tokens(mut self, request_tokens: usize, overhead_tokens: usize) -> Self {
        self.tokens = Some(MeasuredTokens {
            request: request_tokens,
            overhead: overhead_tokens,
            streamed_output: 0,
        });
        self
    }

    pub(crate) fn add_streamed_output_tokens(&mut self, tokens: usize) {
        if let Some(measured) = &mut self.tokens {
            measured.streamed_output = measured.streamed_output.saturating_add(tokens);
        }
    }

    fn request(&self, unit: ContextUnit<'_>) -> Option<usize> {
        match unit {
            ContextUnit::Bytes => Some(self.request_bytes),
            ContextUnit::Tokens(_) => self.tokens.as_ref().map(|tokens| tokens.request),
        }
    }

    fn overhead(&self, unit: ContextUnit<'_>) -> usize {
        match unit {
            ContextUnit::Bytes => self.overhead_bytes,
            ContextUnit::Tokens(_) => self.tokens.as_ref().map_or(0, |tokens| tokens.overhead),
        }
    }

    fn streamed_output_tokens(&self, unit: ContextUnit<'_>) -> usize {
        match unit {
            ContextUnit::Bytes => self
                .streamed_output_bytes
                .div_ceil(ESTIMATED_CONTEXT_BYTES_PER_TOKEN),
            ContextUnit::Tokens(_) => self
                .tokens
                .as_ref()
                .map_or(0, |tokens| tokens.streamed_output),
        }
    }

    fn current_context(
        &self,
        snapshot: &ChatSnapshot,
        completed_usage: Option<&Usage>,
        unit: ContextUnit<'_>,
    ) -> Option<(usize, u64)> {
        let anchor = self
            .message_end
            .checked_sub(1)
            .and_then(|index| snapshot.message_ids.get(&index).copied());
        if self.active_start == snapshot.active_start()
            && self.compaction_index == snapshot.message_compactions.keys().next_back().copied()
            && self.message_end <= snapshot.messages.len()
            && self.message_anchor == anchor
        {
            let completed_output = completed_usage
                .filter(|usage| {
                    snapshot
                        .messages
                        .get(self.message_end)
                        .is_some_and(|message| message.role == Role::Assistant)
                        && snapshot.message_usage.get(&self.message_end) == Some(*usage)
                })
                .and_then(|usage| usage.output_tokens);
            let tail_start = self.message_end + usize::from(completed_output.is_some());
            let added = unit
                .messages(&snapshot.messages[tail_start..])
                .saturating_sub(unit.empty());
            return Some((
                self.request(unit)?.saturating_add(added),
                completed_output.unwrap_or(0),
            ));
        }
        Some((
            active_history(snapshot, unit)?.saturating_add(self.overhead(unit)),
            0,
        ))
    }
}

fn active_history(snapshot: &ChatSnapshot, unit: ContextUnit<'_>) -> Option<usize> {
    let messages = crate::projection::projected_messages(snapshot, &Default::default()).ok()?;
    Some(unit.messages(&messages))
}

/// The tokens the Chat's active context holds for `model`: the last conversation request's
/// measurement calibrated by the input the service reported for it, plus what was appended
/// since and the output still streaming. Without a request of `model`, the active history alone.
/// Counted with the process's context token counter when one is installed.
pub fn current_context_tokens(
    snapshot: &ChatSnapshot,
    ledger: &RequestUsageLedger,
    model: &ModelConfig,
) -> Option<u64> {
    current_context_tokens_counted(snapshot, ledger, model, context_token_counter())
}

/// [`current_context_tokens`] counted with `counter`, or in serialized bytes without one.
pub fn current_context_tokens_counted(
    snapshot: &ChatSnapshot,
    ledger: &RequestUsageLedger,
    model: &ModelConfig,
    counter: Option<&dyn ContextTokenCounter>,
) -> Option<u64> {
    let identity = model.identity();
    let records =
        || {
            ledger.entries().iter().rev().filter(|entry| {
                entry.kind == ModelRequestKind::Conversation && entry.model == identity
            })
        };
    let latest = records().next();
    let measured = records().find(|entry| entry.context_measurement.is_some());
    // A measurement recorded in bytes keeps the whole estimate in bytes, calibration included.
    let unit = counter
        .filter(|_| {
            measured.is_none_or(|entry| {
                entry
                    .context_measurement
                    .as_ref()
                    .is_some_and(|measurement| measurement.tokens.is_some())
            })
        })
        .map_or(ContextUnit::Bytes, ContextUnit::Tokens);
    let (amount, completed_output) = match measured {
        Some(entry) => entry.context_measurement.as_ref()?.current_context(
            snapshot,
            (entry.outcome == RequestOutcome::Accepted).then_some(&entry.usage),
            unit,
        )?,
        None => (
            active_history(snapshot, unit)?.saturating_sub(unit.empty()),
            0,
        ),
    };
    let calibration = records().find_map(|entry| {
        let measurement = entry.context_measurement.as_ref()?;
        let input = entry.context_input_tokens().filter(|tokens| *tokens > 0)?;
        let request = measurement.request(unit).filter(|amount| *amount > 0)?;
        Some((input, request))
    });
    let tokens = match calibration {
        Some((input, previous)) => (amount as u128)
            .saturating_mul(u128::from(input))
            .div_ceil(previous as u128),
        None => unit.uncalibrated_tokens(amount) as u128,
    };
    let output = latest
        .filter(|entry| entry.outcome == RequestOutcome::InFlight)
        .map(|entry| {
            let streamed = entry
                .context_measurement
                .as_ref()
                .map_or(0, |measurement| measurement.streamed_output_tokens(unit));
            entry
                .usage
                .output_tokens
                .unwrap_or(0)
                .max(u64::try_from(streamed).unwrap_or(u64::MAX))
        })
        .unwrap_or(0);
    Some(
        u64::try_from(tokens)
            .unwrap_or(u64::MAX)
            .saturating_add(completed_output)
            .saturating_add(output),
    )
}

pub fn serialized_context_bytes(messages: &[Message], tools: &[ToolDefinition]) -> usize {
    let (base64_bytes, image_bytes) = messages
        .iter()
        .flat_map(|message| &message.content)
        .flat_map(|content| match content {
            Content::Image {
                source: ImageSource::Base64 { data, .. },
                width,
                height,
            } => vec![(data.len(), *width, *height)],
            Content::ToolResult {
                content: ToolResultContent::Blocks(blocks),
                ..
            } => blocks
                .iter()
                .filter_map(|block| match block {
                    ToolResultBlock::Image {
                        source: ImageSource::Base64 { data, .. },
                        width,
                        height,
                    } => Some((data.len(), *width, *height)),
                    ToolResultBlock::Text { .. } => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .fold(
            (0usize, 0usize),
            |(base64, estimated), (data, width, height)| {
                (
                    base64.saturating_add(data),
                    estimated.saturating_add(
                        estimated_image_tokens(width, height)
                            .saturating_mul(ESTIMATED_CONTEXT_BYTES_PER_TOKEN),
                    ),
                )
            },
        );
    serde_json::to_vec(messages)
        .map_or(usize::MAX, |bytes| bytes.len())
        .saturating_sub(base64_bytes)
        .saturating_add(image_bytes)
        .saturating_add(serde_json::to_vec(tools).map_or(usize::MAX, |bytes| bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RequestOwner;
    use serde_json::json;

    /// Counts whitespace-separated words, so expected totals are easy to read.
    struct WordCounter;

    impl ContextTokenCounter for WordCounter {
        fn name(&self) -> &'static str {
            "words"
        }

        fn text_tokens(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![Content::Text { text: text.into() }],
        }
    }

    fn chat(messages: &[&str]) -> ChatSnapshot {
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::from_u128(0xc1),
            "folder": "/work",
            "messages": [],
            "chat_loop": {"id": Uuid::from_u128(0x10), "state": "Idle", "run_id": null},
        }))
        .unwrap();
        for text in messages {
            push(&mut snapshot, text);
        }
        snapshot
    }

    fn push(snapshot: &mut ChatSnapshot, text: &str) {
        let index = snapshot.messages.len();
        snapshot.messages.push(user(text));
        snapshot
            .message_ids
            .insert(index, Uuid::from_u128(0x900 + index as u128));
    }

    fn start(snapshot: &mut ChatSnapshot) -> crate::RequestSequence {
        snapshot
            .request_usage
            .start(
                RequestOwner {
                    chat_id: snapshot.id,
                    loop_id: Some(tickloop_ids::Id::from(Uuid::from_u128(0x10))),
                    run_id: Some(tickloop_ids::Id::from(Uuid::from_u128(0x20))),
                },
                ModelRequestKind::Conversation,
                snapshot.model.identity(),
            )
            .unwrap()
    }

    fn observe_input(snapshot: &mut ChatSnapshot, sequence: crate::RequestSequence, input: u64) {
        snapshot
            .request_usage
            .observe(
                sequence,
                Usage {
                    input_tokens: Some(input),
                    ..Usage::default()
                },
            )
            .unwrap();
    }

    fn counted(snapshot: &ChatSnapshot, counter: Option<&dyn ContextTokenCounter>) -> u64 {
        current_context_tokens_counted(snapshot, &snapshot.request_usage, &snapshot.model, counter)
            .unwrap()
    }

    #[test]
    fn without_a_request_the_active_history_is_counted_with_the_counter() {
        let snapshot = chat(&["one two three", "four five"]);
        assert_eq!(counted(&snapshot, Some(&WordCounter)), (3 + 3) + (2 + 3));
    }

    #[test]
    fn appended_tokens_are_calibrated_by_the_input_the_service_reported() {
        let mut snapshot = chat(&["one two three"]);
        let sequence = start(&mut snapshot);
        let measurement = ContextMeasurement::capture(&snapshot, 4_000, 3_000).with_tokens(100, 40);
        snapshot
            .request_usage
            .record_context_measurement(sequence, measurement)
            .unwrap();
        observe_input(&mut snapshot, sequence, 150);
        snapshot
            .request_usage
            .finish(sequence, RequestOutcome::Accepted)
            .unwrap();
        assert_eq!(counted(&snapshot, Some(&WordCounter)), 150);
        push(&mut snapshot, "a b c d e f g h i j");
        assert_eq!(
            counted(&snapshot, Some(&WordCounter)),
            (100u64 + 13) * 150 / 100 + 1,
            "the 13 appended tokens are scaled like the request they follow"
        );
    }

    #[test]
    fn streamed_output_counts_in_the_measurements_unit() {
        let mut snapshot = chat(&["question"]);
        let sequence = start(&mut snapshot);
        let mut measurement = ContextMeasurement::capture(&snapshot, 1_000, 500).with_tokens(10, 5);
        measurement.streamed_output_bytes = 8_000;
        measurement.add_streamed_output_tokens(7);
        snapshot
            .request_usage
            .record_context_measurement(sequence, measurement)
            .unwrap();
        assert_eq!(counted(&snapshot, Some(&WordCounter)), 10 + 7);
        assert_eq!(counted(&snapshot, None), 1_000 / 2 + 8_000 / 2);
    }

    #[test]
    fn a_byte_measurement_recorded_before_token_counting_keeps_its_byte_estimate() {
        let mut snapshot = chat(&["one two three"]);
        let sequence = start(&mut snapshot);
        let recorded = json!({
            "active_start": 0,
            "message_end": 1,
            "message_anchor": Uuid::from_u128(0x900),
            "compaction_index": null,
            "overhead_bytes": 2_000,
            "request_bytes": 10_000,
        });
        let measurement: ContextMeasurement = serde_json::from_value(recorded).unwrap();
        snapshot
            .request_usage
            .record_context_measurement(sequence, measurement)
            .unwrap();
        observe_input(&mut snapshot, sequence, 20_000);
        push(&mut snapshot, "appended words here");
        assert_eq!(
            counted(&snapshot, Some(&WordCounter)),
            counted(&snapshot, None),
            "an old measurement is read in bytes whatever counter is installed"
        );
        assert!(counted(&snapshot, None) > 20_000);
    }

    #[test]
    fn token_counts_round_trip_and_stay_absent_from_byte_measurements() {
        let snapshot = chat(&["one"]);
        let bytes = ContextMeasurement::capture(&snapshot, 10, 5);
        assert!(
            serde_json::to_value(&bytes)
                .unwrap()
                .get("tokens")
                .is_none()
        );
        let tokens = bytes.with_tokens(4, 2);
        let decoded: ContextMeasurement =
            serde_json::from_value(serde_json::to_value(&tokens).unwrap()).unwrap();
        assert_eq!(decoded, tokens);
    }
}
