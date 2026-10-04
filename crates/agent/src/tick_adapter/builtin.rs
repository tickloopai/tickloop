use super::*;
use crate::{
    runtime::{Agent, ShellTask, ShellTaskState, TickLinkState},
    tick_stream::{ArtifactTickStream, NativeTickStream, ObservationKind},
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
    fn list(&self, context: &TickListContext) -> Vec<TickDescriptor> {
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
                    updated_at_ms: row.last_delivery_ms,
                    running: row.link == TickLinkState::Live,
                    actions: remote_actions(&row.id.to_string(), false),
                }
            })
            .collect()
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
    fn list(&self, context: &TickListContext) -> Vec<TickDescriptor> {
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
                    updated_at_ms: row.updated_at_ms,
                    running: false,
                    actions: remote_actions(row.instance.as_str(), true),
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
                        updated_at_ms,
                        running: false,
                        actions: remote_actions(&id, true),
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
    fn list(&self, context: &TickListContext) -> Vec<TickDescriptor> {
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
                        TickListAction {
                            key: 'o',
                            label: "open",
                            operation: TickListOperation::Execute(
                                crate::terminal::tick_actions::builtin::shell(task.id),
                            ),
                        },
                        TickListAction {
                            key: 'x',
                            label: "remove",
                            operation: if task.state.is_terminal() {
                                TickListOperation::RemoveLocal
                            } else {
                                TickListOperation::Notify(
                                    "Open this Shell to stop it before removing it".into(),
                                )
                            },
                        },
                    ],
                    instance,
                    group: TickGroup::Shells,
                    glyph: TickGlyph::Shell,
                    title: task.command.lines().next().unwrap_or_default().into(),
                    kind: "Shell".into(),
                    status: shell_status(task),
                    updated_at_ms: task.updated_at_ms,
                    running: !task.state.is_terminal(),
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
    context: &'a TickListContext,
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
fn remote_actions(id: &str, artifact: bool) -> Vec<TickListAction> {
    let actions = if artifact {
        vec![
            ('o', "open"),
            ('x', "remove"),
            ('c', "copy"),
            ('f', "feedback"),
        ]
    } else {
        vec![('o', "open"), ('x', "remove")]
    };
    actions
        .into_iter()
        .map(|(key, label)| TickListAction {
            key,
            label,
            operation: TickListOperation::Execute(if artifact {
                crate::terminal::tick_actions::builtin::artifact(id, key)
            } else {
                crate::terminal::tick_actions::builtin::subscription(id, key)
            }),
        })
        .collect()
}
