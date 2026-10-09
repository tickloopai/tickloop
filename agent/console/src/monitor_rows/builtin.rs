use super::*;
use tickloop_agent_protocol::{
    LoopState, ObservationKind, ShellTask, ShellTaskState, TickLinkState,
};

/// How long a finished background Shell keeps its row once the Chat handled its end.
pub const FINISHED_SHELL_DISPLAY_MS: u64 = 30_000;

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
                    notes: Vec::new(),
                    owner: TickOwner::Herdr,
                    tone: link_tone(row.link),
                    updated_at_ms: row.updated_at_ms,
                    update: subscription_update(row),
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
        add_probed_update(&mut row, context);
        match context.member_states.get(&member.instance) {
            Some(MemberState::Live { protocol }) => {
                row.running = true;
                row.status = link_status(TickLinkState::Live).into();
                row.tone = link_tone(TickLinkState::Live);
                if !protocol {
                    row.kind = "Terminal".into();
                    row.glyph = TickGlyph::Terminal;
                }
            }
            Some(MemberState::Gone | MemberState::Lost) => {
                row.status = link_status(TickLinkState::InstanceGone).into();
                row.tone = link_tone(TickLinkState::InstanceGone);
            }
            Some(MemberState::IdentityUncertain) => {
                row.status = link_status(TickLinkState::IdentityUncertain).into();
                row.tone = link_tone(TickLinkState::IdentityUncertain);
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
                notes: Vec::new(),
                owner: TickOwner::ThisChat,
                tone: link_tone(row.link),
                updated_at_ms: row.updated_at_ms,
                update: subscription_update(row),
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
        add_probed_update(&mut row, context);
        row.actions.push(MonitorAction {
            key: HOSTED_CLOSE_KEY,
            label: "close",
            intent: TickIntent::CloseHosted(member.instance.clone()),
        });
        match context.member_states.get(&member.instance) {
            Some(MemberState::Live { .. }) => {
                row.running = true;
                row.status = link_status(TickLinkState::Live).into();
                row.tone = link_tone(TickLinkState::Live);
            }
            Some(MemberState::Lost) => {
                row.status = "lost".into();
                row.tone = StatusTone::Gone;
                row.actions.push(MonitorAction {
                    key: HOSTED_REOPEN_KEY,
                    label: "reopen as new",
                    intent: TickIntent::ReopenHosted(member.instance.clone()),
                });
            }
            Some(MemberState::Gone) => {
                row.status = link_status(TickLinkState::InstanceGone).into();
                row.tone = link_tone(TickLinkState::InstanceGone);
            }
            Some(MemberState::IdentityUncertain) => {
                row.status = link_status(TickLinkState::IdentityUncertain).into();
                row.tone = link_tone(TickLinkState::IdentityUncertain);
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
                    notes: Vec::new(),
                    owner: ARTIFACT_OWNER,
                    tone: link_tone(row.link),
                    updated_at_ms: row.updated_at_ms,
                    update: row.updated_at_ms.map(RowUpdate::new),
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
                let unavailable = metadata.is_some_and(Option::is_none);
                let refresh_failed = context.metadata_failures.contains_key(&id);
                let feedback = if binding.is_some_and(|binding| !binding.resume_required) {
                    Some("feedback here")
                } else if remote_link.is_some_and(|link| {
                    binding.is_none_or(|binding| binding.binding_id != link.binding_id)
                }) {
                    Some("feedback elsewhere")
                } else if binding.is_some() {
                    Some("feedback paused")
                } else {
                    None
                };
                let status = if unavailable {
                    "unavailable"
                } else if refresh_failed {
                    "refresh failed"
                } else {
                    feedback.unwrap_or_default()
                };
                let metadata = metadata.and_then(Option::as_ref);
                let subscribed = rows.get(&instance);
                let title = subscriptions(context, self.tick_type())
                    .filter(|row| row.instance == instance && row.title.is_some())
                    .max_by_key(|row| row.updated_at_ms)
                    .and_then(|row| row.title.clone())
                    .or_else(|| metadata.map(|metadata| metadata.title.clone()))
                    .unwrap_or_else(|| tick.title.clone());
                let edited_at_ms = metadata
                    .and_then(|metadata| metadata.human_updated_at.as_deref())
                    .and_then(tickloop_agent_base::timestamp::parse_timestamp_ms)
                    .map(|value| value.max(0) as u64)
                    .into_iter()
                    .chain(subscribed.and_then(|row| row.updated_at_ms))
                    .max();
                let requested_at_ms = binding.and_then(|binding| binding.last_request_at_ms);
                let updated_at_ms = edited_at_ms.max(requested_at_ms);
                let update = updated_at_ms.map(RowUpdate::new);
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
                        // Where feedback goes is not a state the glyph's colour tells.
                        notes: feedback.map(str::to_owned).into_iter().collect(),
                        owner: ARTIFACT_OWNER,
                        tone: if unavailable {
                            StatusTone::Gone
                        } else if refresh_failed {
                            StatusTone::Degraded
                        } else {
                            StatusTone::Neutral
                        },
                        updated_at_ms,
                        update,
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
                        && (!task.state.is_terminal()
                            || finished_shell_until(task, handled_at(context, task))
                                .is_none_or(|until| until > context.now_ms)
                            || context.retained.as_ref() == Some(&InstanceRef::shell(task.id))
                            || subscriptions(context, "shell")
                                .any(|row| row.instance == InstanceRef::shell(task.id)))
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
                    title: shell_title(task),
                    kind: "Shell".into(),
                    status: if task.state.is_terminal() {
                        task.status_label()
                    } else {
                        String::new()
                    },
                    notes: shell_notes(task),
                    owner: TickOwner::ThisChat,
                    tone: shell_tone(task, context),
                    updated_at_ms: task.updated_at_ms,
                    update: ended_at(task).map(RowUpdate::new),
                    running: !task.state.is_terminal(),
                    resources: None,
                }
            })
            .collect()
    }
}
/// Until when a finished Shell keeps its row: [`FINISHED_SHELL_DISPLAY_MS`] after the Chat
/// handled its end. `None` while it runs or while its end is unread, so it stays.
pub fn finished_shell_until(task: &ShellTask, handled_at: Option<u64>) -> Option<u64> {
    let handled = handled_at?;
    (task.state.is_terminal() && ended_at(task).is_none_or(|ended| handled >= ended))
        .then(|| handled.saturating_add(FINISHED_SHELL_DISPLAY_MS))
}

/// When a finished Shell ended; `None` while it runs.
fn ended_at(task: &ShellTask) -> Option<u64> {
    task.state
        .is_terminal()
        .then(|| task.ended_at_ms.or(task.updated_at_ms))
        .flatten()
}

/// When the Chat last handled the Shell.
fn handled_at(context: &MonitorContext, task: &ShellTask) -> Option<u64> {
    context
        .chat
        .as_ref()
        .and_then(|chat| chat.monitor.handled_at(&InstanceRef::shell(task.id)))
}

/// A subscribed instance's latest Tick Update or observation.
pub(super) fn subscription_update(row: &TickSubscriptionStatus) -> Option<RowUpdate> {
    row.updated_at_ms.map(RowUpdate::new)
}

/// A member's latest Tick Update as its host saw it, when no subscription knows a later one.
fn add_probed_update(row: &mut TickDescriptor, context: &MonitorContext) {
    if let Some(at) = context.member_updates.get(&row.instance).copied() {
        row.updated_at_ms = row.updated_at_ms.max(Some(at));
        row.update = Some(RowUpdate::new(at));
    }
}

/// What a finished Shell's row says beside its glyph's colour: the signal that ended it, by
/// name, or a non-zero exit code. Success needs no words.
fn shell_notes(task: &ShellTask) -> Vec<String> {
    if !task.state.is_terminal() {
        return Vec::new();
    }
    match (task.signal, task.exit_code) {
        (Some(signal), _) => {
            vec![signal_name(signal).map_or_else(|| format!("signal {signal}"), str::to_owned)]
        }
        (None, Some(code)) if code != 0 => vec![code.to_string()],
        _ => Vec::new(),
    }
}

/// The name of a signal that ends a process, such as `SIGTERM`.
pub(crate) fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGSYS => "SIGSYS",
        _ => return None,
    })
}

/// A Shell is named by its description; without one, by its first command line less a leading
/// `cd <directory> &&`, which would otherwise start every row with the same path.
fn shell_title(task: &ShellTask) -> String {
    if let Some(description) = task
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
    {
        return description.lines().next().unwrap_or_default().to_owned();
    }
    let first_line = task.command.trim_start().lines().next().unwrap_or_default();
    without_leading_cd(first_line).to_owned()
}

fn without_leading_cd(command: &str) -> &str {
    let Some(rest) = command.strip_prefix("cd ").map(str::trim_start) else {
        return command;
    };
    let directory_end = match rest.chars().next() {
        Some(quote @ ('"' | '\'')) => rest[1..].find(quote).map(|end| end + 2),
        _ => Some(rest.find(char::is_whitespace).unwrap_or(rest.len())),
    };
    let Some(after_directory) = directory_end.map(|end| rest[end..].trim_start()) else {
        return command;
    };
    match ["&&", ";"]
        .into_iter()
        .find_map(|separator| after_directory.strip_prefix(separator))
        .map(str::trim_start)
    {
        Some(remainder) if !remainder.is_empty() => remainder,
        _ => command,
    }
}

fn shell_tone(task: &ShellTask, context: &MonitorContext) -> StatusTone {
    if !task.state.is_terminal() {
        StatusTone::Running {
            since_ms: context.shell_starts.get(&task.id).copied(),
        }
    } else if task.signal.is_some() {
        StatusTone::Stopped
    } else if task.exit_code == Some(0) {
        StatusTone::Succeeded
    } else if task.exit_code.is_some() || task.state == ShellTaskState::Failed {
        StatusTone::Failed
    } else if task.reason.is_some() {
        StatusTone::Stopped
    } else {
        StatusTone::Neutral
    }
}

pub(super) fn link_tone(link: TickLinkState) -> StatusTone {
    match link {
        TickLinkState::Initializing => StatusTone::Neutral,
        TickLinkState::Live => StatusTone::Live,
        TickLinkState::Disconnected | TickLinkState::IdentityUncertain => StatusTone::Degraded,
        TickLinkState::InstanceGone => StatusTone::Gone,
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
pub(super) fn link_status(link: TickLinkState) -> &'static str {
    match link {
        TickLinkState::Initializing => "initializing",
        TickLinkState::Live => "live",
        TickLinkState::Disconnected => "disconnected",
        TickLinkState::InstanceGone => "gone",
        TickLinkState::IdentityUncertain => "uncertain",
    }
}
const ARTIFACT_OWNER: TickOwner = TickOwner::Service("Artifacts");

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

/// A subscription's own row only removes; the Monitor list adds the subscription keys, and Enter
/// stays with a projection that opens the instance itself.
fn subscription_actions(row: &TickSubscriptionStatus) -> Vec<MonitorAction> {
    vec![remove_from_monitor(&row.instance)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_protocol::{ShellTaskCapabilities, ShellTaskOrigin, ShellTaskOwner};

    fn task(command: &str, description: Option<&str>) -> ShellTask {
        let chat_id = Uuid::from_u128(1);
        ShellTask {
            id: Uuid::from_u128(2),
            chat_id,
            origin: ShellTaskOrigin::User,
            owner: ShellTaskOwner::User { chat_id },
            command: command.into(),
            description: description.map(Into::into),
            cwd: "/fixture".into(),
            state: ShellTaskState::Background,
            was_background: true,
            pid: None,
            exit_code: None,
            signal: None,
            reason: None,
            output_bytes: 0,
            updated_at_ms: Some(1_000),
            ended_at_ms: None,
            capabilities: ShellTaskCapabilities::default(),
        }
    }

    #[test]
    fn shells_are_named_by_description_or_by_command_without_a_leading_cd() {
        for (command, description, expected) in [
            (
                "cargo test",
                Some("Run the console tests"),
                "Run the console tests",
            ),
            ("cargo test", Some("  \n"), "cargo test"),
            (
                "cd /Users/me/worktrees/feature && cargo test -p console",
                None,
                "cargo test -p console",
            ),
            ("cd '/a dir/with space' && make", None, "make"),
            ("cd \"/quoted\"; ls -la", None, "ls -la"),
            ("  cd /x &&   echo a\necho b", None, "echo a"),
            ("cd /tmp", None, "cd /tmp"),
            ("cd /tmp &&", None, "cd /tmp &&"),
            ("cdx /tmp && ls", None, "cdx /tmp && ls"),
        ] {
            assert_eq!(
                shell_title(&task(command, description)),
                expected,
                "{command:?}"
            );
        }
    }

    #[test]
    fn finished_shells_take_the_tone_of_how_they_ended_and_leave_once_their_end_was_handled() {
        let context = MonitorContext::default();
        let mut shell = task("make", None);
        assert_eq!(
            shell_tone(&shell, &context),
            StatusTone::Running { since_ms: None }
        );
        assert_eq!(finished_shell_until(&shell, Some(5_000)), None);
        for (exit_code, signal, reason, state, tone) in [
            (
                Some(0),
                None,
                None,
                ShellTaskState::Completed,
                StatusTone::Succeeded,
            ),
            (
                Some(2),
                None,
                None,
                ShellTaskState::Completed,
                StatusTone::Failed,
            ),
            (
                None,
                Some(15),
                None,
                ShellTaskState::Completed,
                StatusTone::Stopped,
            ),
            (
                None,
                None,
                Some("stopped"),
                ShellTaskState::Completed,
                StatusTone::Stopped,
            ),
            (
                None,
                None,
                Some("spawn"),
                ShellTaskState::Failed,
                StatusTone::Failed,
            ),
        ] {
            shell.exit_code = exit_code;
            shell.signal = signal;
            shell.reason = reason.map(Into::into);
            shell.state = state;
            assert_eq!(shell_tone(&shell, &context), tone);
        }
        assert_eq!(
            finished_shell_until(&shell, None),
            None,
            "an unread end keeps the Shell"
        );
        assert_eq!(
            finished_shell_until(&shell, Some(500)),
            None,
            "handling before the end does not handle the end"
        );
        assert_eq!(
            finished_shell_until(&shell, Some(4_000)),
            Some(4_000 + FINISHED_SHELL_DISPLAY_MS)
        );
        let instance = InstanceRef::shell(shell.id);
        let listed = |now_ms, retained: Option<InstanceRef>, handled_at: Option<u64>| {
            let mut chat: ChatSnapshot = serde_json::from_value(serde_json::json!({
                "id": shell.chat_id,
                "workdir": "/fixture",
                "messages": [],
                "loops": []
            }))
            .unwrap();
            if let Some(at) = handled_at {
                chat.monitor.mark_handled(instance.clone(), at);
            }
            ShellRows
                .list(&MonitorContext {
                    chat: Some(chat),
                    shells: Some(tickloop_agent_protocol::ShellTaskSnapshot {
                        chat_id: shell.chat_id,
                        live_revision: 1,
                        tasks: vec![shell.clone()],
                    }),
                    now_ms,
                    retained,
                    ..Default::default()
                })
                .len()
        };
        assert_eq!(listed(u64::MAX, None, None), 1, "unread stays");
        let handled = Some(4_000);
        assert_eq!(
            listed(4_000 + FINISHED_SHELL_DISPLAY_MS - 1, None, handled),
            1
        );
        assert_eq!(listed(4_000 + FINISHED_SHELL_DISPLAY_MS, None, handled), 0);
        assert_eq!(
            listed(
                4_000 + FINISHED_SHELL_DISPLAY_MS,
                Some(instance.clone()),
                handled
            ),
            1,
            "an open detail keeps its finished Shell listed"
        );
    }

    #[test]
    fn finished_shells_note_only_a_failing_exit_code_or_the_signal_by_name() {
        let mut shell = task("make", None);
        assert!(
            shell_notes(&shell).is_empty(),
            "a running Shell notes nothing"
        );
        shell.state = ShellTaskState::Completed;
        for (exit_code, signal, notes) in [
            (Some(0), None, vec![]),
            (Some(101), None, vec!["101".to_owned()]),
            (None, Some(libc::SIGTERM), vec!["SIGTERM".to_owned()]),
            (Some(143), Some(libc::SIGTERM), vec!["SIGTERM".to_owned()]),
            (None, Some(64), vec!["signal 64".to_owned()]),
            (None, None, vec![]),
        ] {
            shell.exit_code = exit_code;
            shell.signal = signal;
            assert_eq!(shell_notes(&shell), notes, "{exit_code:?} {signal:?}");
        }
        shell.exit_code = Some(0);
        shell.signal = None;
        assert_eq!(
            shell.status_label(),
            "exit 0",
            "the detail still says it in words"
        );
    }

    /// A protocol Tick keeps the name it was opened under, its declared title plus the arguments
    /// that tell instances apart, over the bare title its subscription reports; a program named
    /// by what it runs takes what its host calls it now.
    #[test]
    fn members_keep_their_names_and_programs_take_their_host_names() {
        let quote = InstanceRef::new("hosted", "quote-run").unwrap();
        let terminal = InstanceRef::native("terminal:t1:20:birth").unwrap();
        let mut chat: ChatSnapshot = serde_json::from_value(serde_json::json!({
            "id": Uuid::from_u128(1),
            "workdir": "/fixture",
            "messages": [],
            "loops": []
        }))
        .unwrap();
        chat.monitor
            .record_opened(quote.clone(), Some("Quote · AAPL.US".into()));
        chat.monitor.add(terminal.clone(), Some("make dev".into()));
        let context = MonitorContext {
            chat: Some(chat),
            subscriptions: vec![TickSubscriptionStatus {
                id: Uuid::from_u128(2),
                loop_id: Uuid::from_u128(3),
                loop_state: LoopState::WaitingTrigger,
                instance: quote.clone(),
                every_seconds: 60,
                accumulated: 0,
                last_delivery_ms: None,
                last_delivery: None,
                kind: Some(ObservationKind::Protocol),
                title: Some("Quote".into()),
                updated_at_ms: None,
                link: TickLinkState::Live,
            }],
            member_titles: [(terminal.clone(), "WAF bypass review".to_owned())].into(),
            ..Default::default()
        };
        let titles = MonitorRowsRegistry::builtin()
            .list(&context)
            .into_iter()
            .map(|row| (row.instance, row.title))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(titles[&quote], "Quote · AAPL.US");
        assert_eq!(titles[&terminal], "WAF bypass review");
    }

    fn empty_chat() -> ChatSnapshot {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::from_u128(1),
            "workdir": "/fixture",
            "messages": [],
            "loops": []
        }))
        .unwrap()
    }

    fn subscription(
        instance: &InstanceRef,
        updated_at_ms: Option<u64>,
        accumulated: u64,
    ) -> TickSubscriptionStatus {
        TickSubscriptionStatus {
            id: Uuid::new_v4(),
            loop_id: Uuid::from_u128(3),
            loop_state: LoopState::WaitingTrigger,
            instance: instance.clone(),
            every_seconds: 60,
            accumulated,
            last_delivery_ms: None,
            last_delivery: None,
            kind: Some(ObservationKind::Protocol),
            title: Some("Quote".into()),
            updated_at_ms,
            link: TickLinkState::Live,
        }
    }

    fn unread(context: &MonitorContext) -> std::collections::BTreeMap<InstanceRef, Option<Unread>> {
        MonitorRowsRegistry::builtin()
            .list(context)
            .into_iter()
            .map(|row| (row.instance, row.update.and_then(|update| update.unread)))
            .collect()
    }

    #[test]
    fn rows_are_unread_until_the_chat_handles_their_latest_update() {
        let quote = InstanceRef::new("hosted", "quote-run").unwrap();
        let calendar = InstanceRef::native("calendar-run").unwrap();
        let artifact = InstanceRef::artifact("https://tickloop.ai", "doc-1").unwrap();
        let mut ended = task("make", None);
        ended.state = ShellTaskState::Completed;
        ended.exit_code = Some(0);
        ended.updated_at_ms = Some(8_000);
        let shell = InstanceRef::shell(ended.id);
        let mut chat = empty_chat();
        chat.monitor.record_opened(calendar.clone(), None);
        chat.monitor.mark_handled(calendar.clone(), 6_000);
        let mut context = MonitorContext {
            chat: Some(chat),
            shells: Some(tickloop_agent_protocol::ShellTaskSnapshot {
                chat_id: ended.chat_id,
                live_revision: 1,
                tasks: vec![ended],
            }),
            subscriptions: vec![
                subscription(&quote, Some(5_000), 3),
                subscription(&artifact, Some(4_000), 0),
            ],
            member_updates: [(calendar.clone(), 7_000)].into(),
            now_ms: 9_000,
            ..Default::default()
        };
        let rows = unread(&context);
        assert_eq!(
            rows[&quote],
            Some(Unread { count: Some(3) }),
            "a subscription counts what waits"
        );
        for instance in [&calendar, &shell, &artifact] {
            assert_eq!(rows[instance], Some(Unread { count: None }), "{instance}");
        }

        let chat = context.chat.as_mut().unwrap();
        for (instance, at) in [
            (&quote, 5_000),
            (&calendar, 7_500),
            (&shell, 8_000),
            (&artifact, 4_500),
        ] {
            chat.monitor.mark_handled(instance.clone(), at);
        }
        assert!(
            unread(&context).values().all(Option::is_none),
            "{:?}",
            unread(&context)
        );
        let rows = MonitorRowsRegistry::builtin().list(&context);
        let quote_row = rows.iter().find(|row| row.instance == quote).unwrap();
        assert_eq!(quote_row.status, "live");
        assert_eq!(quote_row.notes, ["⟳ 60s"], "the marker no longer counts");
        assert_eq!(quote_row.update.as_ref().unwrap().at_ms, 5_000);
    }

    #[test]
    fn a_running_shell_and_a_never_updated_tick_have_nothing_to_read() {
        let running = task("make", None);
        let pane = InstanceRef::native("terminal:t1:20:birth").unwrap();
        let mut chat = empty_chat();
        chat.monitor.add(pane.clone(), Some("make dev".into()));
        let context = MonitorContext {
            chat: Some(chat),
            shells: Some(tickloop_agent_protocol::ShellTaskSnapshot {
                chat_id: running.chat_id,
                live_revision: 1,
                tasks: vec![running.clone()],
            }),
            member_states: [(pane.clone(), MemberState::Live { protocol: false })].into(),
            ..Default::default()
        };
        let rows = MonitorRowsRegistry::builtin().list(&context);
        assert!(rows.iter().all(|row| row.update.is_none()), "{rows:?}");
    }

    #[test]
    fn gone_ticks_leave_after_thirty_seconds_unless_subscribed_unread_or_lost() {
        let plain = InstanceRef::native("plain-run").unwrap();
        let watched = InstanceRef::native("watched-run").unwrap();
        let unread_gone = InstanceRef::native("unread-run").unwrap();
        let lost = InstanceRef::new("hosted", "lost-run").unwrap();
        let mut chat = empty_chat();
        for instance in [&plain, &watched, &unread_gone, &lost] {
            chat.monitor.record_opened(instance.clone(), None);
        }
        chat.monitor.mark_handled(unread_gone.clone(), 1_000);
        let mut gone_subscription = subscription(&watched, None, 0);
        gone_subscription.link = TickLinkState::InstanceGone;
        let context = |now_ms| MonitorContext {
            chat: Some(chat.clone()),
            subscriptions: vec![gone_subscription.clone()],
            member_states: [
                (plain.clone(), MemberState::Gone),
                (unread_gone.clone(), MemberState::Gone),
                (lost.clone(), MemberState::Lost),
            ]
            .into(),
            member_updates: [(unread_gone.clone(), 2_000)].into(),
            gone_since: [&plain, &watched, &unread_gone, &lost]
                .into_iter()
                .map(|instance| (instance.clone(), 10_000))
                .collect(),
            now_ms,
            ..Default::default()
        };
        let listed = |now_ms| {
            MonitorRowsRegistry::builtin()
                .list(&context(now_ms))
                .into_iter()
                .map(|row| row.instance)
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(listed(10_000 + GONE_DISPLAY_MS - 1).len(), 4);
        assert_eq!(
            listed(10_000 + GONE_DISPLAY_MS),
            [watched, unread_gone, lost].into(),
            "only the plain gone Tick leaves"
        );
    }
}
