use super::*;
use tickloop_agent_protocol::{LoopState, ObservationKind, ShellTaskState, TickLinkState};

pub struct HerdrRows;
impl MonitorRows for HerdrRows {
    fn tick_type(&self) -> &'static str {
        tick_protocol::HERDR_INSTANCE_TYPE
    }
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        subscriptions(context, self.tick_type())
            .map(|row| {
                let terminal = matches!(row.kind, Some(ObservationKind::Terminal { .. }));
                TickDescriptor {
                    instance: row.instance.clone(),
                    group: TickGroup::Ticks,
                    glyph: if terminal {
                        TickGlyph::Terminal
                    } else {
                        TickGlyph::Protocol
                    },
                    title: row
                        .title
                        .clone()
                        .unwrap_or_else(|| row.instance.to_string()),
                    kind: if terminal { "Terminal" } else { "Tick" }.into(),
                    status: link_status(row.link).into(),
                    owner: TickOwner::Herdr,
                    updated_at_ms: row.last_delivery_ms,
                    running: row.link == TickLinkState::Live,
                    resources: None,
                    actions: subscription_actions(row),
                }
            })
            .collect()
    }
    /// A Herdr member reports the latest probe of its Pane; a program the user started in Herdr
    /// stays Herdr's even when added here.
    fn member(&self, member: &MonitorMember, context: &MonitorContext) -> TickDescriptor {
        let mut row = super::member_descriptor(member);
        if member.source == MonitorMemberSource::Added {
            row.owner = TickOwner::Herdr;
        }
        match context.member_states.get(&member.instance) {
            Some(MemberState::Live { protocol }) => {
                row.running = true;
                row.status = link_status(TickLinkState::Live).into();
                if !protocol {
                    row.kind = "Terminal".into();
                    row.glyph = TickGlyph::Terminal;
                }
            }
            Some(MemberState::Gone | MemberState::Lost) => {
                row.status = link_status(TickLinkState::InstanceGone).into()
            }
            Some(MemberState::IdentityUncertain) => {
                row.status = link_status(TickLinkState::IdentityUncertain).into()
            }
            None => {}
        }
        row
    }
}

/// Shows a hosted Tick's screen as a text snapshot detail.
pub(crate) const HOSTED_SNAPSHOT_KEY: char = 's';
/// Ends a hosted Tick from its Monitor row after confirmation; the row stays until removed.
pub(crate) const HOSTED_CLOSE_KEY: char = 'c';
/// Starts a new instance in place of a hosted Tick lost with its host, after confirmation.
pub(crate) const HOSTED_REOPEN_KEY: char = 'n';

/// Ticks this Chat opened in the dedicated Tickhost. They belong to the Chat that opened them.
pub struct HostedRows;
impl MonitorRows for HostedRows {
    fn tick_type(&self) -> &'static str {
        tick_protocol::HOSTED_INSTANCE_TYPE
    }
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        subscriptions(context, self.tick_type())
            .map(|row| TickDescriptor {
                instance: row.instance.clone(),
                group: TickGroup::Ticks,
                glyph: TickGlyph::Protocol,
                title: row
                    .title
                    .clone()
                    .unwrap_or_else(|| row.instance.to_string()),
                kind: "Tick".into(),
                status: link_status(row.link).into(),
                owner: TickOwner::ThisChat,
                updated_at_ms: row.last_delivery_ms,
                running: row.link == TickLinkState::Live,
                resources: None,
                actions: subscription_actions(row),
            })
            .collect()
    }
    /// A hosted member reports its host's latest listing and process sample, and can be closed;
    /// one lost with its host can be reopened as a new instance.
    fn member(&self, member: &MonitorMember, context: &MonitorContext) -> TickDescriptor {
        let mut row = super::member_descriptor(member);
        row.actions.retain(|action| action.key != MONITOR_ENTER_KEY);
        row.actions.splice(
            0..0,
            [
                MonitorAction {
                    key: MONITOR_ENTER_KEY,
                    label: "open",
                    intent: TickIntent::OpenHosted(member.instance.clone()),
                },
                MonitorAction {
                    key: HOSTED_SNAPSHOT_KEY,
                    label: "snapshot",
                    intent: TickIntent::ShowHostedSnapshot(member.instance.clone()),
                },
            ],
        );
        row.resources = context.member_resources.get(&member.instance).cloned();
        row.actions.push(MonitorAction {
            key: HOSTED_CLOSE_KEY,
            label: "close",
            intent: TickIntent::CloseHosted(member.instance.clone()),
        });
        match context.member_states.get(&member.instance) {
            Some(MemberState::Live { .. }) => {
                row.running = true;
                row.status = link_status(TickLinkState::Live).into();
            }
            Some(MemberState::Lost) => {
                row.status = "lost".into();
                row.actions.push(MonitorAction {
                    key: HOSTED_REOPEN_KEY,
                    label: "reopen as new",
                    intent: TickIntent::ReopenHosted(member.instance.clone()),
                });
            }
            Some(MemberState::Gone) => row.status = link_status(TickLinkState::InstanceGone).into(),
            Some(MemberState::IdentityUncertain) => {
                row.status = link_status(TickLinkState::IdentityUncertain).into()
            }
            None => {}
        }
        row
    }
}

pub struct ArtifactRows;
impl MonitorRows for ArtifactRows {
    fn tick_type(&self) -> &'static str {
        "artifact"
    }
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        let mut rows =
            super::merge_descriptors(subscriptions(context, self.tick_type()).map(|row| {
                TickDescriptor {
                    instance: row.instance.clone(),
                    group: TickGroup::Artifacts,
                    glyph: TickGlyph::Artifact,
                    title: row
                        .title
                        .clone()
                        .unwrap_or_else(|| row.instance.to_string()),
                    kind: "Artifact".into(),
                    status: link_status(row.link).into(),
                    owner: ARTIFACT_OWNER,
                    updated_at_ms: row.updated_at_ms,
                    running: false,
                    resources: None,
                    actions: artifact_actions(row.instance.as_str()),
                }
            }));
        if let Some(chat) = &context.chat {
            for tick in &chat.artifact_ticks {
                let id = tick.instance_id();
                let Ok(instance) = InstanceRef::artifact(&tick.origin, &tick.artifact_id) else {
                    continue;
                };
                let metadata = context.metadata.get(&id);
                let binding = chat
                    .loops
                    .iter()
                    .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
                    .flat_map(|goal| &goal.event_sources)
                    .find(|binding| {
                        binding.origin == tick.origin && binding.artifact_id == tick.artifact_id
                    });
                let remote_link = metadata
                    .and_then(Option::as_ref)
                    .and_then(|metadata| metadata.agent_link.as_ref());
                let status = if metadata.is_some_and(Option::is_none) {
                    "unavailable"
                } else if context.metadata_failures.contains_key(&id) {
                    "refresh failed"
                } else if binding.is_some_and(|binding| !binding.resume_required) {
                    "feedback here"
                } else if remote_link.is_some_and(|link| {
                    binding.is_none_or(|binding| binding.binding_id != link.binding_id)
                }) {
                    "feedback elsewhere"
                } else if binding.is_some() {
                    "feedback paused"
                } else {
                    ""
                };
                let metadata = metadata.and_then(Option::as_ref);
                let subscribed = rows.get(&instance);
                let title = subscriptions(context, self.tick_type())
                    .filter(|row| row.instance == instance && row.title.is_some())
                    .max_by_key(|row| row.updated_at_ms)
                    .and_then(|row| row.title.clone())
                    .or_else(|| metadata.map(|metadata| metadata.title.clone()))
                    .unwrap_or_else(|| tick.title.clone());
                let updated_at_ms = metadata
                    .and_then(|metadata| metadata.human_updated_at.as_deref())
                    .and_then(tickloop_agent_base::timestamp::parse_timestamp_ms)
                    .map(|value| value.max(0) as u64)
                    .into_iter()
                    .chain(subscribed.and_then(|row| row.updated_at_ms))
                    .chain(binding.and_then(|binding| binding.last_request_at_ms))
                    .max();
                rows.insert(
                    instance.clone(),
                    TickDescriptor {
                        instance,
                        group: TickGroup::Artifacts,
                        glyph: TickGlyph::Artifact,
                        title,
                        kind: if metadata
                            .map(|metadata| metadata.format.as_str())
                            .unwrap_or(&tick.format)
                            == "html"
                        {
                            "Artifact·HTML"
                        } else {
                            "Artifact·MD"
                        }
                        .into(),
                        status: status.into(),
                        owner: ARTIFACT_OWNER,
                        updated_at_ms,
                        running: false,
                        resources: None,
                        actions: artifact_actions(&id),
                    },
                );
            }
        }
        rows.into_values().collect()
    }
    fn refresh_intent(&self) -> Option<TickIntent> {
        Some(TickIntent::RefreshArtifacts)
    }
}

pub struct ShellRows;
impl MonitorRows for ShellRows {
    fn tick_type(&self) -> &'static str {
        "shell"
    }
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        context
            .shells
            .iter()
            .flat_map(|snapshot| {
                snapshot.tasks.iter().filter(|task| {
                    task.chat_id == snapshot.chat_id
                        && (task.was_background || task.state == ShellTaskState::Background)
                })
            })
            .map(|task| {
                let instance = InstanceRef::shell(task.id);
                TickDescriptor {
                    actions: vec![
                        MonitorAction {
                            key: MONITOR_ENTER_KEY,
                            label: "open",
                            intent: TickIntent::OpenShell { task: task.id },
                        },
                        if task.state.is_terminal() {
                            remove_from_monitor(&instance)
                        } else {
                            MonitorAction {
                                key: 'x',
                                label: "remove",
                                intent: TickIntent::Notify(
                                    "Open this Shell to stop it before removing it".into(),
                                ),
                            }
                        },
                    ],
                    instance,
                    group: TickGroup::Shells,
                    glyph: TickGlyph::Shell,
                    title: task.command.lines().next().unwrap_or_default().into(),
                    kind: "Shell".into(),
                    status: task.status_label(),
                    owner: TickOwner::ThisChat,
                    updated_at_ms: task.updated_at_ms,
                    running: !task.state.is_terminal(),
                    resources: None,
                }
            })
            .collect()
    }
}
fn subscriptions<'a>(
    context: &'a MonitorContext,
    tick_type: &'a str,
) -> impl Iterator<Item = &'a TickSubscriptionStatus> {
    context.subscriptions.iter().filter(move |row| {
        row.instance.tick_type() == tick_type
            && !matches!(row.loop_state, LoopState::Stopped | LoopState::Expired)
    })
}
fn link_status(link: TickLinkState) -> &'static str {
    match link {
        TickLinkState::Initializing => "initializing",
        TickLinkState::Live => "live",
        TickLinkState::Disconnected => "disconnected",
        TickLinkState::InstanceGone => "gone",
        TickLinkState::IdentityUncertain => "uncertain",
    }
}
const ARTIFACT_OWNER: TickOwner = TickOwner::Service("Artifact Service");

pub(crate) fn artifact_actions(id: &str) -> Vec<MonitorAction> {
    [
        (MONITOR_ENTER_KEY, "open"),
        ('x', "remove"),
        ('c', "copy"),
        ('f', "feedback"),
    ]
    .into_iter()
    .map(|(key, label)| MonitorAction {
        key,
        label,
        intent: TickIntent::Artifact { id: id.into(), key },
    })
    .collect()
}

/// Enter opens the subscription; x only removes the row from this Chat's Monitor.
fn subscription_actions(row: &TickSubscriptionStatus) -> Vec<MonitorAction> {
    vec![
        MonitorAction {
            key: MONITOR_ENTER_KEY,
            label: "open",
            intent: TickIntent::OpenSubscription { id: row.id },
        },
        remove_from_monitor(&row.instance),
    ]
}
