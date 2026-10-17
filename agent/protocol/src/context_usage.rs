use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ChatSnapshot, Content, ImageSource, Message, ModelConfig, ModelRequestKind, RequestOutcome,
    RequestUsageLedger, ToolDefinition, ToolResultBlock, ToolResultContent,
};

pub const ESTIMATED_CONTEXT_BYTES_PER_TOKEN: usize = 2;
const EMPTY_CONTEXT_BYTES: usize = 4;
const IMAGE_PIXELS_PER_TOKEN: usize = 750;

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
        }
    }

    fn current_request_bytes(&self, snapshot: &ChatSnapshot) -> Option<usize> {
        let anchor = self
            .message_end
            .checked_sub(1)
            .and_then(|index| snapshot.message_ids.get(&index).copied());
        if self.active_start == snapshot.active_start()
            && self.compaction_index == snapshot.message_compactions.keys().next_back().copied()
            && self.message_end <= snapshot.messages.len()
            && self.message_anchor == anchor
        {
            let added = serialized_context_bytes(&snapshot.messages[self.message_end..], &[])
                .saturating_sub(EMPTY_CONTEXT_BYTES);
            return Some(self.request_bytes.saturating_add(added));
        }
        Some(active_history_bytes(snapshot)?.saturating_add(self.overhead_bytes))
    }
}

fn active_history_bytes(snapshot: &ChatSnapshot) -> Option<usize> {
    let messages = crate::projection::projected_messages(snapshot, &Default::default()).ok()?;
    Some(serialized_context_bytes(&messages, &[]))
}

pub(crate) fn current_context_tokens(
    snapshot: &ChatSnapshot,
    ledger: &RequestUsageLedger,
    model: &ModelConfig,
) -> Option<u64> {
    let identity = model.identity();
    let records =
        || {
            ledger.entries().iter().rev().filter(|entry| {
                entry.kind == ModelRequestKind::Conversation && entry.model == identity
            })
        };
    let latest = records().next();
    let bytes = match records().find_map(|entry| entry.context_measurement.as_ref()) {
        Some(measurement) => measurement.current_request_bytes(snapshot)?,
        None => active_history_bytes(snapshot)?.saturating_sub(EMPTY_CONTEXT_BYTES),
    };
    let calibration = records().find_map(|entry| {
        let measurement = entry.context_measurement.as_ref()?;
        let input = entry.context_input_tokens().filter(|tokens| *tokens > 0)?;
        (measurement.request_bytes > 0).then_some((input, measurement.request_bytes))
    });
    let tokens = match calibration {
        Some((input, previous_bytes)) => (bytes as u128)
            .saturating_mul(u128::from(input))
            .div_ceil(previous_bytes as u128),
        None => bytes.div_ceil(ESTIMATED_CONTEXT_BYTES_PER_TOKEN) as u128,
    };
    let output = latest
        .filter(|entry| entry.outcome == RequestOutcome::InFlight)
        .map(|entry| {
            let streamed = entry.context_measurement.as_ref().map_or(0, |measurement| {
                measurement
                    .streamed_output_bytes
                    .div_ceil(ESTIMATED_CONTEXT_BYTES_PER_TOKEN)
            });
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
                        image_tokens(width, height)
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

fn image_tokens(width: u32, height: u32) -> usize {
    (width as usize).saturating_mul(height as usize) / IMAGE_PIXELS_PER_TOKEN
}
