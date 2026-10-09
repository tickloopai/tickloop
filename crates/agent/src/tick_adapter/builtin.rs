use super::*;
use crate::{
    runtime::{Agent, ShellTask, ShellTaskState, TickLinkState},
    tick_stream::{ArtifactTickStream, HostedTickStream, NativeTickStream, ObservationKind},
};
use std::path::PathBuf;

pub struct HerdrAdapter {
    stream: Option<Arc<dyn TickStreamPort>>,
}
impl HerdrAdapter {
    pub fn new(provider: Option<Arc<crate::native_host::NativeHostToolProvider>>) -> Self {
        Self {
            stream: provider.map(|provider| {
                Arc::new(NativeTickStream::new(provider)) as Arc<dyn TickStreamPort>
            }),
        }
    }
}
impl TickAdapter for HerdrAdapter {
    fn tick_type(&self) -> &'static str {
        "herdr"
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
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        self.stream.clone()
    }
}

/// Ends a hosted Tick from its Monitor row after confirmation; the row stays until removed.
pub(crate) const HOSTED_CLOSE_KEY: char = 'c';
/// Starts a new instance in place of a hosted Tick lost with its host, after confirmation.
pub(crate) const HOSTED_REOPEN_KEY: char = 'n';

/// Ticks this Chat opened in the dedicated Tick host. They belong to the Chat that opened them;
/// only a worker with a Tick host grant follows them.
pub struct HostedAdapter {
    stream: Option<Arc<dyn TickStreamPort>>,
}
impl HostedAdapter {
    pub fn new(tools: Option<Arc<crate::hosted::tools::HostedTickTools>>) -> Self {
        Self {
            stream: tools
                .map(|tools| Arc::new(HostedTickStream::new(tools)) as Arc<dyn TickStreamPort>),
        }
    }
}
impl TickAdapter for HostedAdapter {
    fn tick_type(&self) -> &'static str {
        tick_connect::instance::HOSTED_TYPE
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
        if let Some(enter) = row
            .actions
            .iter_mut()
            .find(|action| action.key == MONITOR_ENTER_KEY)
        {
            *enter = MonitorAction {
                key: MONITOR_ENTER_KEY,
                label: "show",
                operation: MonitorOperation::Execute(
                    crate::terminal::tick_actions::builtin::present_hosted(member.instance.clone()),
                ),
            };
        }
        row.resources = context.member_resources.get(&member.instance).cloned();
        row.actions.push(MonitorAction {
            key: HOSTED_CLOSE_KEY,
            label: "close",
            operation: MonitorOperation::Execute(
                crate::terminal::tick_actions::builtin::close_hosted(member.instance.clone()),
            ),
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
                    operation: MonitorOperation::Execute(
                        crate::terminal::tick_actions::builtin::reopen_hosted(
                            member.instance.clone(),
                        ),
                    ),
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
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        self.stream.clone()
    }
}

pub struct ArtifactAdapter {
    stream: Option<Arc<dyn TickStreamPort>>,
}
impl ArtifactAdapter {
    pub fn new(root: Option<PathBuf>) -> Self {
        let stream = root
            .as_ref()
            .map(|root| Arc::new(ArtifactTickStream::new(root.clone())) as Arc<dyn TickStreamPort>);
        Self { stream }
    }
}
impl TickAdapter for ArtifactAdapter {
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
                    .flat_map(|goal| &goal.event_sources)
                    .find(|binding| binding.source_key == tick.artifact_id);
                let status = if metadata.is_some_and(Option::is_none) {
                    "unavailable"
                } else if context.metadata_failures.contains_key(&id) {
                    "refresh failed"
                } else if binding.is_some_and(|binding| binding.resume_required) {
                    "feedback paused"
                } else if binding.is_some() {
                    "feedback here"
                } else if metadata
                    .and_then(Option::as_ref)
                    .is_some_and(|metadata| metadata.association.is_some())
                {
                    "feedback elsewhere"
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
                    .and_then(crate::transcript::parse_timestamp_ms)
                    .map(|value| value.max(0) as u64)
                    .into_iter()
                    .chain(subscribed.and_then(|row| row.updated_at_ms))
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
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        self.stream.clone()
    }
    fn refresh_action(&self) -> Option<Arc<dyn TickActionExecutor>> {
        Some(crate::terminal::tick_actions::builtin::artifact("", 'r'))
    }
}

pub struct ShellAdapter {
    stream: Option<Arc<dyn TickStreamPort>>,
}
impl ShellAdapter {
    pub fn new(agent: Option<Agent>) -> Self {
        let stream = agent
            .as_ref()
            .map(|agent| Arc::new(ShellTickStream::new(agent.clone())) as Arc<dyn TickStreamPort>);
        Self { stream }
    }
}
impl TickAdapter for ShellAdapter {
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
                            operation: MonitorOperation::Execute(
                                crate::terminal::tick_actions::builtin::shell(task.id),
                            ),
                        },
                        if task.state.is_terminal() {
                            remove_from_monitor(&instance)
                        } else {
                            MonitorAction {
                                key: 'x',
                                label: "remove",
                                operation: MonitorOperation::Notify(
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
                    status: shell_status(task),
                    owner: TickOwner::ThisChat,
                    updated_at_ms: task.updated_at_ms,
                    running: !task.state.is_terminal(),
                    resources: None,
                }
            })
            .collect()
    }
    fn stream(&self) -> Option<Arc<dyn TickStreamPort>> {
        self.stream.clone()
    }
}
pub fn shell_status(task: &ShellTask) -> String {
    if task.state == ShellTaskState::Unknown {
        "unknown".into()
    } else if !task.state.is_terminal() {
        "running".into()
    } else if let Some(signal) = task.signal {
        format!("signal {signal}")
    } else if let Some(exit) = task.exit_code {
        format!("exit {exit}")
    } else if task.reason.is_some() {
        "stopped".into()
    } else {
        "unknown".into()
    }
}
fn subscriptions<'a>(
    context: &'a MonitorContext,
    tick_type: &'a str,
) -> impl Iterator<Item = &'a TickSubscriptionStatus> {
    context.subscriptions.iter().filter(move |row| {
        row.instance.tick_type() == tick_type
            && !matches!(
                row.loop_state,
                crate::runtime::LoopState::Stopped | crate::runtime::LoopState::Expired
            )
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
        operation: MonitorOperation::Execute(crate::terminal::tick_actions::builtin::artifact(
            id, key,
        )),
    })
    .collect()
}

/// Enter opens the subscription; x only removes the row from this Chat's Monitor.
fn subscription_actions(row: &TickSubscriptionStatus) -> Vec<MonitorAction> {
    vec![
        MonitorAction {
            key: MONITOR_ENTER_KEY,
            label: "open",
            operation: MonitorOperation::Execute(
                crate::terminal::tick_actions::builtin::subscription(&row.id.to_string()),
            ),
        },
        remove_from_monitor(&row.instance),
    ]
}
