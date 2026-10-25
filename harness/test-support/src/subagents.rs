//! Controlled Subagent completion and queue observations shared by runtime and Console tests.

use async_trait::async_trait;
use serde_json::json;
use tickloop_harness_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness_protocol::{Content, EventKind};
use tickloop_harness_runtime::Agent;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::routed::{RoutedModel, has_tool_result, state};

pub struct SubagentTimingModel {
    pub routed: RoutedModel,
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
    pub entered_count: std::sync::atomic::AtomicUsize,
}

impl SubagentTimingModel {
    pub fn new(background: bool) -> Self {
        Self::with_boundary(background, None)
    }

    pub fn with_boundary(background: bool, boundary: Option<String>) -> Self {
        Self {
            routed: RoutedModel::new(move |request| {
                if !state(request)["delegation"].is_null() {
                    crate::text("CHILD-FINAL-REPORT")
                } else if !has_tool_result(request, "launch") {
                    crate::call(
                        "launch",
                        "Subagent",
                        json!({"task":"Inspect bounded evidence","name":"audit","run_in_background":background}),
                    )
                } else if let Some(command) = boundary
                    .as_ref()
                    .filter(|_| !has_tool_result(request, "busy-boundary"))
                {
                    crate::call(
                        "busy-boundary",
                        "Bash",
                        json!({"command":command,"description":"Hold the controlled parent tool boundary"}),
                    )
                } else {
                    crate::text("Parent finished its turn")
                }
            }),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            entered_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ModelClient for SubagentTimingModel {
    fn identity(&self) -> tickloop_harness_protocol::ModelIdentity {
        self.routed.identity()
    }
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(crate::title_output(request))
    }
    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let is_child = !state(&request)["delegation"].is_null();
        if is_child {
            self.entered_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.entered.notify_one();
            tokio::select! {
                () = self.release.notified() => {},
                () = cancel.cancelled() => return Err(ModelError::Cancelled),
            }
        }
        let mut output = self.routed.complete(request, events, cancel).await?;
        if is_child {
            output.usage = tickloop_harness_protocol::Usage {
                input_tokens: Some(11),
                output_tokens: Some(7),
                cache_read_input_tokens: Some(3),
                cache_write_input_tokens: None,
            };
        }
        Ok(output)
    }
}

pub async fn completed(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_harness_protocol::Event>,
    receipt: &tickloop_harness_protocol::Receipt,
) {
    loop {
        let event = crate::next_event(events).await;
        if event.run_id == receipt.run_id {
            match event.kind {
                EventKind::Completed { .. } => return,
                EventKind::Failed(error) => panic!("execution failed: {error}"),
                _ => {}
            }
        }
    }
}

pub async fn wait_for_snapshot(
    core: &Agent,
    chat: Uuid,
    awaited: &str,
    ready: impl Fn(&tickloop_harness_protocol::ChatSnapshot) -> bool,
) -> tickloop_harness_protocol::ChatSnapshot {
    let mut latest = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = core.snapshot(tickloop_ids::Id::from(chat)).await.unwrap();
            if ready(&snapshot) {
                break snapshot;
            }
            latest = format!(
                "completions {:?}; loop {:?}; latest events {:?}",
                snapshot
                    .family_completions
                    .iter()
                    .map(|completion| &completion.notification)
                    .collect::<Vec<_>>(),
                snapshot.chat_loop,
                snapshot.events.iter().rev().take(6).collect::<Vec<_>>()
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {awaited}: {latest}"))
}

pub fn task_notifications(
    snapshot: &tickloop_harness_protocol::ChatSnapshot,
) -> Vec<(usize, &str)> {
    snapshot
        .messages
        .iter()
        .enumerate()
        .flat_map(|(index, message)| {
            message
                .content
                .iter()
                .filter_map(move |content| match content {
                    Content::Text { text } if text.starts_with("<task-notification>") => {
                        Some((index, text.as_str()))
                    }
                    _ => None,
                })
        })
        .collect()
}
