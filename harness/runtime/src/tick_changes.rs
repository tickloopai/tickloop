//! What happened to the hosted Ticks a Chat opened outside the model's own tool calls: the user
//! reopened or closed one in the Monitor, a program exited, or a host was lost with its Ticks.
//! The model would otherwise keep using an identity that no longer runs. A Loop state note at the
//! end of the next request tells it; nothing wakes the Agent for it, since a Tick Subscription is
//! how a Loop asks to act on a change, and a subscribed Tick's exit already arrives through it.

use std::collections::{BTreeMap, HashMap};

use tick_protocol::InstanceRef;
use tickloop_harness_protocol::tags::single_line;
use tickloop_harness_protocol::{ChatId, Receipt};

use crate::environment::{ExecutionAuthority, ExecutionInspection, ExecutionQuery, InstanceState};
use crate::{Agent, AgentError, Chat};

/// A change the user made to one of the Chat's hosted Ticks, not yet told to the model.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(super) enum UserTickChange {
    Reopened {
        ended: InstanceRef,
        reopened: InstanceRef,
        title: Option<String>,
    },
    Closed {
        instance: InstanceRef,
        title: Option<String>,
    },
}

/// How a hosted Tick stood when the model last knew it, from its own calls or a note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum ToldTickState {
    Running,
    Exited,
    Lost,
    /// Closed or gone; nothing changes it any more.
    Ended,
}

impl ToldTickState {
    fn of(state: Option<&InstanceState>) -> Self {
        match state {
            Some(InstanceState::Running) => Self::Running,
            Some(InstanceState::Exited { .. }) => Self::Exited,
            Some(InstanceState::Lost) => Self::Lost,
            None => Self::Ended,
        }
    }
}

impl Agent {
    /// Records that the user closed `instance` from the Monitor, for the model's next request.
    pub async fn record_user_closed_tick(
        &self,
        chat_id: ChatId,
        instance: &InstanceRef,
    ) -> Result<(), AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        let title = member_title(chat, instance);
        chat.bookkeeping
            .user_tick_changes
            .push(UserTickChange::Closed {
                instance: instance.clone(),
                title,
            });
        chat.bookkeeping
            .told_tick_states
            .insert(instance.clone(), ToldTickState::Ended);
        Ok(())
    }

    /// How each hosted Tick the Chat opened stands now, or `None` when the Chat opened none, has
    /// no host to ask, or the host did not answer; a change is then told on a later request.
    pub(super) async fn hosted_tick_states(
        &self,
        receipt: &Receipt,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Option<HashMap<InstanceRef, InstanceState>>, AgentError> {
        let Some(environment) = &self.inner.environment else {
            return Ok(None);
        };
        let scope = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).ok_or(AgentError::NotFound)?;
            if chat.active_run != Some(receipt.run_id) || hosted_members(chat).next().is_none() {
                return Ok(None);
            }
            chat.snapshot
                .folder
                .is_some()
                .then(|| crate::chat_execution::scope(&chat.snapshot))
        };
        let Some(scope) = scope else {
            return Ok(None);
        };
        let Ok(port) = environment.bind(
            scope,
            ExecutionAuthority::with_cancellation(receipt.chat_id, cancel.clone()),
        ) else {
            return Ok(None);
        };
        let inspection = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(tickloop_harness_model::ModelError::Cancelled.into()),
            inspection = port.inspect(ExecutionQuery::InstanceStates) => inspection,
        };
        Ok(match inspection {
            Ok(ExecutionInspection::InstanceStates(states)) => Some(states.into_iter().collect()),
            _ => None,
        })
    }
}

/// The hosted Ticks this Chat opened.
fn hosted_members(chat: &Chat) -> impl Iterator<Item = &InstanceRef> {
    chat.snapshot
        .monitor
        .members
        .iter()
        .map(|member| &member.instance)
        .filter(|instance| instance.access() == tick_protocol::HOSTED_ACCESS)
}

fn member_title(chat: &Chat, instance: &InstanceRef) -> Option<String> {
    chat.snapshot
        .monitor
        .members
        .iter()
        .find(|member| &member.instance == instance)
        .and_then(|member| member.title.clone())
}

/// Records what the model's own successful call `name` with `input` did to a hosted Tick: a
/// TickManage `close` or `reopen` ends the instance it names, which its result already told.
pub(super) fn record_own_call(chat: &mut Chat, name: &str, input: &serde_json::Value) {
    if name != crate::tick_tools::TICK_MANAGE
        || !matches!(input["action"].as_str(), Some("close" | "reopen"))
    {
        return;
    }
    if let Some(instance) = input["tick_instance_id"]
        .as_str()
        .and_then(|id| id.parse::<InstanceRef>().ok())
    {
        record_told(chat, instance, ToldTickState::Ended);
    }
}

/// Records that the model knows `instance` as `state` from its own call, such as opening or
/// closing it.
pub(super) fn record_told(chat: &mut Chat, instance: InstanceRef, state: ToldTickState) {
    chat.bookkeeping.told_tick_states.insert(instance, state);
}

/// The note's lines for what changed since the model was told, which it is then told. With
/// `states`, an instance the model never heard of is taken as it is now, as after a load.
pub(super) fn take_change_text(
    chat: &mut Chat,
    states: Option<&HashMap<InstanceRef, InstanceState>>,
) -> Option<String> {
    let ids = chat.ids;
    let name = |chat: &Chat, instance: &InstanceRef| {
        let instance_text = single_line(&instance.model_text(&ids));
        match member_title(chat, instance) {
            Some(title) => format!("{} ({instance_text})", single_line(&title)),
            None => instance_text,
        }
    };
    let mut lines = Vec::new();
    for change in std::mem::take(&mut chat.bookkeeping.user_tick_changes) {
        lines.push(match change {
            UserTickChange::Reopened {
                ended,
                reopened,
                title,
            } => format!(
                "- {}{} was reopened by the user as Tick Instance {}; the earlier instance no longer runs",
                title
                    .as_deref()
                    .map(|title| format!("{} ", single_line(title)))
                    .unwrap_or_default(),
                single_line(&ended.model_text(&ids)),
                single_line(&reopened.model_text(&ids)),
            ),
            UserTickChange::Closed { instance, title } => format!(
                "- {}{} was closed by the user",
                title
                    .as_deref()
                    .map(|title| format!("{} ", single_line(title)))
                    .unwrap_or_default(),
                single_line(&instance.model_text(&ids)),
            ),
        });
    }
    if let Some(states) = states {
        let subscribed = chat
            .snapshot
            .chat_loop
            .subscriptions
            .iter()
            .map(|subscription| subscription.instance.clone())
            .collect::<std::collections::HashSet<_>>();
        let members = hosted_members(chat).cloned().collect::<Vec<_>>();
        let mut told: BTreeMap<InstanceRef, ToldTickState> =
            chat.bookkeeping.told_tick_states.clone();
        for instance in members {
            let state = states.get(&instance);
            let now = ToldTickState::of(state);
            let Some(before) = told.insert(instance.clone(), now) else {
                continue;
            };
            // A subscription delivers its Tick's exit itself.
            if before == now || (now == ToldTickState::Exited && subscribed.contains(&instance)) {
                continue;
            }
            let change = match (now, state) {
                (ToldTickState::Exited, Some(InstanceState::Exited { code, signal })) => {
                    match (code, signal) {
                        (Some(code), _) => format!("its program exited with code {code}"),
                        (_, Some(signal)) => format!("its program was ended by signal {signal}"),
                        _ => "its program exited".into(),
                    }
                }
                (ToldTickState::Lost, _) => {
                    "was lost with its Tickhost; only the user can reopen it".into()
                }
                (ToldTickState::Ended, _) => "ended and no longer runs".into(),
                (ToldTickState::Running, _) => "runs again".into(),
                (ToldTickState::Exited, _) => "its program exited".into(),
            };
            lines.push(format!("- {} {change}", name(chat, &instance)));
        }
        chat.bookkeeping.told_tick_states = told;
    }
    if lines.is_empty() {
        return None;
    }
    lines.insert(
        0,
        "Host notice: these Ticks this Chat opened changed outside your tool calls:".into(),
    );
    Some(lines.join("\n"))
}
