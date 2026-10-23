use super::*;
use tickloop_agent_protocol::{EventSourceBinding, ShellTask, ShellTaskState, TickLinkState};

/// Subscribes the Chat to an extension or terminal Tick from its Monitor row.
pub(crate) const SUBSCRIBE_KEY: char = 's';
/// The row action ID dispatched by Ctrl+X.
pub(crate) const REMOVE_KEY: char = 'x';

/// Ticks in the dedicated Tickhost: those this Chat opened, which belong to it, and those only
/// its subscriptions show.
pub struct HostedRows;
impl MonitorRows for HostedRows {
    fn access(&self) -> &'static str {
        tick_protocol::HOSTED_ACCESS
    }
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        subscriptions(context, self.access())
            .map(|row| {
                let is_opened_here = context.chat.as_ref().is_some_and(|chat| {
                    chat.monitor
                        .members
                        .iter()
                        .any(|member| member.instance == row.instance)
                });
                let run = if row.link == TickLinkState::InstanceGone {
                    HostedRun::Ended
                } else if row.program_exit.is_some() {
                    HostedRun::Exited
                } else {
                    HostedRun::Running
                };
                let mut descriptor = super::subscription_descriptor(row);
                if context.chat.as_ref().is_some_and(|chat| {
                    chat.monitor
                        .members
                        .iter()
                        .any(|member| member.instance == row.instance && member.is_terminal)
                }) {
                    descriptor.glyph = TickGlyph::Terminal;
                    descriptor.kind = "Terminal".into();
                }
                TickDescriptor {
                    instance: row.instance.clone(),
                    group: TickGroup::Ticks,
                    glyph: descriptor.glyph,
                    title: row
                        .title
                        .clone()
                        .unwrap_or_else(|| row.instance.to_string()),
                    kind: descriptor.kind,
                    status: row
                        .program_exit
                        .filter(|_| run == HostedRun::Exited)
                        .map(terminal_exit_status)
                        .unwrap_or_else(|| link_status(row.link).into()),
                    notes: Vec::new(),
                    subscription: None,
                    owner: if is_opened_here {
                        TickOwner::ThisChat
                    } else {
                        TickOwner::Unknown
                    },
                    tone: if run == HostedRun::Exited {
                        StatusTone::Neutral
                    } else {
                        link_tone(row.link)
                    },
                    updated_at_ms: row.updated_at_ms,
                    update: subscription_update(row),
                    running: run == HostedRun::Running && row.link == TickLinkState::Live,
                    resources: None,
                    actions: hosted_actions(&row.instance, is_opened_here, run),
                }
            })
            .collect()
    }
    /// A hosted member reports its host's latest listing and process sample.
    fn member(&self, member: &MonitorMember, context: &MonitorContext) -> TickDescriptor {
        let mut row = super::member_descriptor(member);
        if context.terminal_members.contains(&member.instance) {
            row.glyph = TickGlyph::Terminal;
            row.kind = "Terminal".into();
        }
        row.resources = context.member_resources.get(&member.instance).cloned();
        add_probed_update(&mut row, context);
        let run = match context.member_states.get(&member.instance) {
            Some(MemberState::Live) => {
                row.running = true;
                row.status = link_status(TickLinkState::Live).into();
                row.tone = link_tone(TickLinkState::Live);
                HostedRun::Running
            }
            Some(MemberState::Exited(status)) => {
                row.status = status.clone();
                row.tone = StatusTone::Neutral;
                HostedRun::Exited
            }
            Some(MemberState::Lost) => {
                row.status = "lost".into();
                row.tone = StatusTone::Gone;
                HostedRun::Ended
            }
            Some(MemberState::Gone) => {
                row.status = link_status(TickLinkState::InstanceGone).into();
                row.tone = link_tone(TickLinkState::InstanceGone);
                HostedRun::Ended
            }
            Some(MemberState::Unavailable) => {
                row.status = "unavailable".into();
                row.tone = StatusTone::Degraded;
                HostedRun::Running
            }
            // Until the first probe answers, the Tick this Chat opened is taken to run.
            None => HostedRun::Running,
        };
        row.actions = hosted_actions(&member.instance, true, run);
        row
    }
}

/// How far a hosted Tick's process has come, as its row's actions see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostedRun {
    Running,
    /// A terminal Tick whose program ended; its last screen stays readable.
    Exited,
    /// Gone or lost with its host: nothing runs any more.
    Ended,
}

/// A hosted Tick row's Enter and Ctrl+X. Enter opens the view of a Tick that still has a screen,
/// and reopens one this Chat opened that no longer runs. Ctrl+X closes a Tick this Chat opened, so
/// nothing it opened keeps running out of sight and the record of one that ended goes with its
/// row, and otherwise only takes the row out with this Chat's subscriptions, never ending another
/// Chat's Tick.
fn hosted_actions(
    instance: &InstanceRef,
    is_opened_here: bool,
    run: HostedRun,
) -> Vec<MonitorAction> {
    let enter = match (run, is_opened_here) {
        (HostedRun::Running | HostedRun::Exited, _) => open_view(instance),
        (HostedRun::Ended, true) => MonitorAction {
            key: MONITOR_ENTER_KEY,
            label: "reopen",
            intent: TickIntent::ReopenHosted(instance.clone()),
        },
        (HostedRun::Ended, false) => MonitorAction {
            key: MONITOR_ENTER_KEY,
            label: "details",
            intent: TickIntent::ShowDetails(instance.clone()),
        },
    };
    let remove = match (run, is_opened_here) {
        (HostedRun::Running, true) => MonitorAction {
            key: REMOVE_KEY,
            label: "close",
            intent: TickIntent::CloseHosted {
                instance: instance.clone(),
                ends_process: true,
            },
        },
        (HostedRun::Exited | HostedRun::Ended, true) => MonitorAction {
            key: REMOVE_KEY,
            label: "remove",
            intent: TickIntent::CloseHosted {
                instance: instance.clone(),
                ends_process: false,
            },
        },
        (_, false) => remove_from_monitor(instance),
    };
    vec![enter, remove]
}

/// The Chat's artifacts. An artifact takes no Tick Subscription: its association is its
/// subscription and the only one its row shows.
pub struct ArtifactRows;
impl MonitorRows for ArtifactRows {
    fn access(&self) -> &'static str {
        tick_protocol::ARTIFACT_ACCESS
    }
    fn takes_tick_subscriptions(&self) -> bool {
        false
    }
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        let mut rows = BTreeMap::new();
        if let Some(chat) = &context.chat {
            for tick in &chat.artifact_ticks {
                let id = tick.instance_id();
                let Ok(instance) = InstanceRef::artifact(&tick.origin, &tick.artifact_id) else {
                    continue;
                };
                let metadata = context.metadata.get(&id);
                let binding = feedback_binding(chat, &tick.origin, &tick.artifact_id);
                let is_linked_here = binding.is_some_and(|binding| !binding.resume_required);
                let remote_link = metadata.and_then(Option::as_ref).and_then(|metadata| {
                    metadata
                        .agent_links
                        .iter()
                        .find(|link| {
                            binding.is_some_and(|binding| binding.binding_id == link.binding_id)
                        })
                        .or_else(|| metadata.agent_links.first())
                });
                let unavailable = metadata.is_some_and(Option::is_none);
                let refresh_failed = context.metadata_failures.contains_key(&id);
                let feedback = if is_linked_here {
                    Some("feedback here")
                } else if remote_link.is_some_and(|link| {
                    binding.is_none_or(|binding| binding.binding_id != link.binding_id)
                }) {
                    Some("feedback elsewhere")
                } else if binding.is_some() {
                    Some("feedback unavailable")
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
                let title = metadata
                    .map(|metadata| metadata.title.clone())
                    .unwrap_or_else(|| tick.title.clone());
                let edited_at_ms = metadata
                    .and_then(|metadata| metadata.human_updated_at.as_deref())
                    .and_then(tickloop_agent_base::timestamp::parse_timestamp_ms)
                    .map(|value| value.max(0) as u64);
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
                        kind: tickloop_agent_artifacts::artifact_type(
                            metadata.map_or(&tick.format, |metadata| &metadata.format),
                        )
                        .map_or_else(
                            || "Artifact".into(),
                            |artifact_type| format!("Artifact·{}", artifact_type.name),
                        ),
                        status: status.into(),
                        // An association here is the row's default subscription and needs no
                        // words or mark; feedback unavailable or going elsewhere is not a state the
                        // glyph's colour tells, so it stays in words.
                        notes: feedback
                            .filter(|_| !is_linked_here)
                            .map(str::to_owned)
                            .into_iter()
                            .collect(),
                        subscription: None,
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
                        actions: artifact_actions(&id, binding.is_some()),
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
    fn access(&self) -> &'static str {
        "shell"
    }
    fn list(&self, context: &MonitorContext) -> Vec<TickDescriptor> {
        context
            .shells
            .iter()
            .flat_map(|snapshot| {
                snapshot.tasks.iter().filter(|task| {
                    task.chat_id == snapshot.chat_id
                        && (task.is_listed_in_monitor(handled_at(context, task), context.now_ms)
                            || (task.went_to_background()
                                && (context.retained.as_ref()
                                    == Some(&InstanceRef::shell(task.id))
                                    || subscriptions(context, "shell")
                                        .any(|row| row.instance == InstanceRef::shell(task.id)))))
                })
            })
            .map(|task| {
                let instance = InstanceRef::shell(task.id);
                TickDescriptor {
                    actions: shell_actions(task, &instance),
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
                    subscription: None,
                    owner: TickOwner::ThisChat,
                    tone: shell_tone(task, context.shell_starts.get(&task.id).copied()),
                    updated_at_ms: task.updated_at_ms,
                    update: task.ended_at().map(RowUpdate::new),
                    running: !task.state.is_terminal(),
                    resources: None,
                }
            })
            .collect()
    }
}
/// A Shell row opens the Shell's view; a running one can be stopped, where its owner allows it,
/// and must be before it is removed.
fn shell_actions(task: &ShellTask, instance: &InstanceRef) -> Vec<MonitorAction> {
    let mut actions = vec![open_view(instance)];
    if task.state.is_terminal() {
        actions.push(remove_from_monitor(instance));
        return actions;
    }
    actions.push(MonitorAction {
        key: REMOVE_KEY,
        label: if task.capabilities.can_stop {
            "stop"
        } else {
            "remove"
        },
        intent: if task.capabilities.can_stop {
            TickIntent::StopShell { task: task.id }
        } else {
            TickIntent::Notify(RUNNING_SHELL_REMOVAL.into())
        },
    });
    actions
}

/// A running Shell that this Chat cannot stop stays visible until it ends.
const RUNNING_SHELL_REMOVAL: &str = "This Shell leaves Monitor once it ends";

/// Enter on a local Tick's row opens its attached view.
fn open_view(instance: &InstanceRef) -> MonitorAction {
    MonitorAction {
        key: MONITOR_ENTER_KEY,
        label: "open",
        intent: TickIntent::OpenView(instance.clone()),
    }
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
fn signal_name(signal: i32) -> Option<&'static str> {
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

/// A Shell row is named by its key command, so the row says what runs; its description, full
/// command and directory head its view.
fn shell_title(task: &ShellTask) -> String {
    key_command(&task.command)
}

/// Commands that prepare for or report on the work rather than do it.
const SETUP_COMMANDS: [&str; 11] = [
    "cd", "export", "set", "source", ".", "echo", "printf", "sleep", "mkdir", "true", ":",
];
/// Commands that run the next word as the program.
const WRAPPER_COMMANDS: [&str; 5] = ["env", "time", "nohup", "timeout", "caffeinate"];
/// The wrapper whose first operand is a duration rather than the program.
const TIMEOUT_COMMAND: &str = "timeout";
/// Redirection operators, longer before their prefixes, after an optional descriptor number.
const REDIRECTION_OPERATORS: [&str; 11] = [
    "&>>", "&>", ">>", ">&", ">|", ">", "<<<", "<<", "<&", "<>", "<",
];

/// The command a person recognises in `command`, such as `make test`: its first line's first
/// command that does the work, without the setup around it, the variables, wrappers and
/// redirections in front of or after it, or the directory of an absolute program path. A line
/// with nothing else falls back to itself less a leading `cd <directory> &&`.
pub(crate) fn key_command(command: &str) -> String {
    let first_line = command.trim_start().lines().next().unwrap_or_default();
    let line = first_line.trim_end();
    let line = line.strip_suffix('\\').unwrap_or(line);
    work_command(without_leading_cd(line))
        .unwrap_or_else(|| without_leading_cd(first_line).to_owned())
}

/// The first top-level command of `command` that does the work, opening groups around it.
fn work_command(command: &str) -> Option<String> {
    let command = command.trim();
    let command = group_body(command).unwrap_or(command);
    top_level_commands(command).into_iter().find_map(|segment| {
        let words = program_words(segment);
        let program = *words.first()?;
        let program = if program.starts_with('/') {
            std::path::Path::new(program)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(program)
        } else {
            program
        };
        if SETUP_COMMANDS.contains(&program) {
            return None;
        }
        let joined = std::iter::once(program)
            .chain(words[1..].iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        match group_body(&joined) {
            Some(body) => work_command(body),
            None => Some(joined),
        }
    })
}

/// A segment's words from its program on: redirections and their targets dropped, then the
/// variable assignments and wrapper commands in front of the program.
fn program_words(segment: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut skips_target = false;
    for word in top_level_words(segment) {
        if std::mem::take(&mut skips_target) {
            continue;
        }
        match redirection_needs_target(word) {
            Some(needs_target) => skips_target = needs_target,
            None => words.push(word),
        }
    }
    let mut start = 0;
    while let Some(word) = words.get(start).copied() {
        if is_assignment(word) {
            start += 1;
        } else if WRAPPER_COMMANDS.contains(&word) {
            start += 1;
            while words.get(start).is_some_and(|word| word.starts_with('-')) {
                start += 1;
            }
            if word == TIMEOUT_COMMAND {
                start += 1;
            }
        } else {
            break;
        }
    }
    words.split_off(start.min(words.len()))
}

/// Whether `word` is a redirection, such as `2>&1`, and whether its target is the next word, as
/// in `> log`.
fn redirection_needs_target(word: &str) -> Option<bool> {
    let operator = word.trim_start_matches(|character: char| character.is_ascii_digit());
    REDIRECTION_OPERATORS
        .iter()
        .find_map(|redirection| operator.strip_prefix(redirection))
        .map(str::is_empty)
}

/// Whether `word` assigns a variable for the command, as `NAME=value` does.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|character: char| character.is_ascii_alphabetic() || character == '_')
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
    })
}

/// The commands inside a `( … )` or `{ …; }` group that spans all of `command`.
fn group_body(command: &str) -> Option<&str> {
    let close = match command.chars().next()? {
        '(' => ')',
        '{' => '}',
        _ => return None,
    };
    let spans_command = command.ends_with(close)
        && top_level_characters(command)
            .into_iter()
            .skip(1)
            .all(|(_, _, top_level)| !top_level);
    spans_command.then(|| {
        command[1..command.len() - 1]
            .trim()
            .trim_end_matches(';')
            .trim_end()
    })
}

/// `command` cut at its top-level `;`, `&&`, `||` and `|`.
fn top_level_commands(command: &str) -> Vec<&str> {
    let characters = top_level_characters(command);
    let mut segments = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while let Some(&(offset, character, top_level)) = characters.get(index) {
        let next = characters.get(index + 1).map(|&(_, next, _)| next);
        let separator_length = match (top_level, character, next) {
            (true, ';', _) => 1,
            (true, '&', Some('&')) | (true, '|', Some('|')) => 2,
            (true, '|', _) => 1,
            _ => 0,
        };
        if separator_length == 0 {
            index += 1;
            continue;
        }
        segments.push(command[start..offset].trim());
        start = offset + separator_length;
        index += separator_length;
    }
    segments.push(command[start..].trim());
    segments
}

/// `segment` cut at its top-level whitespace, each word kept as written.
fn top_level_words(segment: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = None;
    for (offset, character, top_level) in top_level_characters(segment) {
        if top_level && character.is_whitespace() {
            if let Some(word_start) = start.take() {
                words.push(&segment[word_start..offset]);
            }
        } else if start.is_none() {
            start = Some(offset);
        }
    }
    if let Some(word_start) = start {
        words.push(&segment[word_start..]);
    }
    words
}

/// Each character of `text` with its byte offset and whether it stands outside quotes, escapes
/// and groups, where shell syntax separates commands and words. An opening quote or bracket is
/// itself at the top level; what follows up to its close is not.
fn top_level_characters(text: &str) -> Vec<(usize, char, bool)> {
    let mut quote = None;
    let mut escaped = false;
    let mut depth = 0usize;
    text.char_indices()
        .map(|(offset, character)| {
            let top_level = quote.is_none() && !escaped && depth == 0;
            if escaped {
                escaped = false;
            } else if let Some(open) = quote {
                if character == open {
                    quote = None;
                } else if character == '\\' && open != '\'' {
                    escaped = true;
                }
            } else {
                match character {
                    '\\' => escaped = true,
                    '\'' | '"' | '`' => quote = Some(character),
                    '(' | '{' => depth += 1,
                    ')' | '}' => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
            (offset, character, top_level)
        })
        .collect()
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

/// The tone a Shell's glyph takes: running since `started_at_ms`, or how it ended.
fn shell_tone(task: &ShellTask, started_at_ms: Option<u64>) -> StatusTone {
    if !task.state.is_terminal() {
        StatusTone::Running {
            since_ms: started_at_ms,
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
    access: &'a str,
) -> impl Iterator<Item = &'a TickSubscriptionStatus> {
    context
        .subscriptions
        .iter()
        .filter(move |row| row.instance.access() == access)
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

/// This Chat's association with the artifact: the binding its Loop holds, live or paused.
pub(crate) fn feedback_binding<'a>(
    chat: &'a ChatStatus,
    origin: &str,
    artifact_id: &str,
) -> Option<&'a EventSourceBinding> {
    chat.chat_loop
        .event_sources
        .iter()
        .find(|binding| binding.origin == origin && binding.artifact_id == artifact_id)
}

/// An artifact row's actions. Its Ctrl+X reads `unlink` while this Chat holds the artifact's
/// association, which Ctrl+X ends along with the row; otherwise it only removes the row.
pub(crate) fn artifact_actions(id: &str, holds_association: bool) -> Vec<MonitorAction> {
    [
        (MONITOR_ENTER_KEY, "open"),
        (
            'x',
            if holds_association {
                "unlink"
            } else {
                "remove"
            },
        ),
        ('c', "copy"),
        ('f', "feedback"),
        ('h', "include earlier feedback"),
    ]
    .into_iter()
    .map(|(key, label)| MonitorAction {
        key,
        label,
        intent: TickIntent::Artifact { id: id.into(), key },
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_protocol::{
        FINISHED_SHELL_DISPLAY_MS, ObservationKind, ShellTaskCapabilities, ShellTaskOrigin,
        ShellTaskOwner, TickSubscriptionFrequency,
    };

    fn task(command: &str, description: Option<&str>) -> ShellTask {
        let chat_id = Uuid::from_u128(1);
        ShellTask {
            id: Uuid::from_u128(2),
            chat_id,
            origin: ShellTaskOrigin::User,
            owner: ShellTaskOwner::User { chat_id },
            call_id: None,
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
    fn shells_are_named_by_their_key_command_whatever_their_description() {
        assert_eq!(
            shell_title(&task(
                "cargo test",
                Some("Rerun full test suite in background to verify flakiness")
            )),
            "cargo test"
        );
        for (command, expected) in [
            (
                "cd /a/b && (make test > target/x.log 2>&1; echo \"EXIT $?\" >> target/x.log)",
                "make test",
            ),
            (
                "FOO=1 timeout 60 cargo nextest run -p x 2>&1 | tail -20",
                "cargo nextest run -p x",
            ),
            (
                "cd /Users/me/worktrees/feature && cargo test -p console",
                "cargo test -p console",
            ),
            ("cd '/a dir/with space' && make", "make"),
            ("cd \"/quoted\"; ls -la", "ls -la"),
            ("{ make lint; make test; }", "make lint"),
            (
                "export RUST_LOG=debug; mkdir -p out && cargo run",
                "cargo run",
            ),
            ("source .env && . ./setup.sh && pytest -q", "pytest -q"),
            ("sleep 5; true || : ; ./run.sh", "./run.sh"),
            ("/bin/sleep 1 && /usr/bin/make -j8", "make -j8"),
            ("set -e; printf 'go' | npm test", "npm test"),
            (
                "env -i PATH=/bin nohup time make dev &> dev.log",
                "make dev",
            ),
            (
                "caffeinate -dims cargo build --release < /dev/null",
                "cargo build --release",
            ),
            (
                "timeout --preserve-status 300 make test > out.log",
                "make test",
            ),
            (
                "/usr/local/bin/python3 -m http.server 8000",
                "python3 -m http.server 8000",
            ),
            ("make test 2> errors.log >>all.log", "make test"),
            (
                "grep 'a;b|c&&d' file.txt | wc -l",
                "grep 'a;b|c&&d' file.txt",
            ),
            ("echo \"x && y | z\"; git status", "git status"),
            ("rg \"a|b\" src || echo none", "rg \"a|b\" src"),
            ("cargo test \\\n  -p console", "cargo test"),
            ("  cd /x &&   echo a\necho b", "echo a"),
            ("cd /tmp", "cd /tmp"),
            ("cd /tmp &&", "cd /tmp &&"),
            ("cdx /tmp && ls", "cdx /tmp"),
            (
                "echo \"EXIT $?\" > target/x.log",
                "echo \"EXIT $?\" > target/x.log",
            ),
        ] {
            assert_eq!(key_command(command), expected, "{command:?}");
        }
    }

    #[test]
    fn finished_shells_take_the_tone_of_how_they_ended_and_leave_once_their_end_was_handled() {
        let mut shell = task("make", None);
        assert_eq!(
            shell_tone(&shell, Some(1_000)),
            StatusTone::Running {
                since_ms: Some(1_000)
            }
        );
        assert_eq!(shell.finished_row_until(Some(5_000)), None);
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
            assert_eq!(shell_tone(&shell, None), tone);
        }
        assert_eq!(
            shell.finished_row_until(None),
            None,
            "an unread end keeps the Shell"
        );
        assert_eq!(
            shell.finished_row_until(Some(500)),
            None,
            "handling before the end does not handle the end"
        );
        assert_eq!(
            shell.finished_row_until(Some(4_000)),
            Some(4_000 + FINISHED_SHELL_DISPLAY_MS)
        );
        let instance = InstanceRef::shell(shell.id);
        let listed = |now_ms, retained: Option<InstanceRef>, handled_at: Option<u64>| {
            let mut chat = ChatStatus::of(
                &serde_json::from_value(serde_json::json!({
                    "id": shell.chat_id,
                    "folder": "/fixture",
                    "messages": [],
                    "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}
                }))
                .unwrap(),
            );
            if let Some(at) = handled_at {
                chat.monitor.mark_handled(instance.clone(), at);
            }
            ShellRows
                .list(&MonitorContext {
                    chat: Some(Arc::new(chat)),
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

    /// An extension Tick keeps the name it was opened under, its declared title plus the arguments
    /// that tell instances apart, over the bare title its subscription reports; a terminal Tick
    /// keeps the command it was opened with.
    #[test]
    fn members_keep_the_names_they_were_opened_under() {
        let quote = InstanceRef::new("hosted", "quote-run").unwrap();
        let terminal = InstanceRef::new("hosted", "terminal-run").unwrap();
        let mut chat = empty_chat();
        chat.monitor
            .record_opened(quote.clone(), Some("Quote · AAPL.US".into()));
        chat.monitor
            .record_opened(terminal.clone(), Some("make dev".into()));
        let context = MonitorContext {
            chat: Some(Arc::new(chat)),
            subscriptions: vec![TickSubscriptionStatus {
                paused: false,
                id: Uuid::from_u128(2),
                loop_state: tickloop_agent_protocol::LoopState::Idle,
                instance: quote.clone(),
                frequency: TickSubscriptionFrequency::High,
                accumulated: 0,
                has_pending_change: false,
                pending_since_ms: None,
                program_exit: None,
                last_delivery_ms: None,
                last_delivery: None,
                kind: Some(ObservationKind::Protocol),
                title: Some("Quote".into()),
                updated_at_ms: None,
                link: TickLinkState::Live,
                next_check_at_ms: None,
                period_ms: None,
            }],
            ..Default::default()
        };
        let titles = MonitorRowsRegistry::builtin()
            .list(&context)
            .into_iter()
            .map(|row| (row.instance, row.title))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(titles[&quote], "Quote · AAPL.US");
        assert_eq!(titles[&terminal], "make dev");
    }

    fn empty_chat() -> ChatStatus {
        ChatStatus::of(
            &serde_json::from_value(serde_json::json!({
                "id": Uuid::from_u128(1),
                "folder": "/fixture",
                "messages": [],
                "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}
            }))
            .unwrap(),
        )
    }

    fn subscription(
        instance: &InstanceRef,
        updated_at_ms: Option<u64>,
        accumulated: u64,
    ) -> TickSubscriptionStatus {
        TickSubscriptionStatus {
            paused: false,
            id: Uuid::new_v4(),
            loop_state: tickloop_agent_protocol::LoopState::Idle,
            instance: instance.clone(),
            frequency: TickSubscriptionFrequency::High,
            accumulated,
            has_pending_change: false,
            pending_since_ms: None,
            program_exit: None,
            last_delivery_ms: None,
            last_delivery: None,
            kind: Some(ObservationKind::Protocol),
            title: Some("Quote".into()),
            updated_at_ms,
            link: TickLinkState::Live,
            next_check_at_ms: None,
            period_ms: None,
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
        let calendar = InstanceRef::new("hosted", "calendar-run").unwrap();
        let artifact = InstanceRef::artifact("https://tickloop.ai", "doc-1").unwrap();
        let mut ended = task("make", None);
        ended.state = ShellTaskState::Completed;
        ended.exit_code = Some(0);
        ended.updated_at_ms = Some(8_000);
        let shell = InstanceRef::shell(ended.id);
        let mut chat = empty_chat();
        chat.monitor.record_opened(calendar.clone(), None);
        chat.monitor.mark_taken_in(calendar.clone(), 6_000);
        chat.artifact_ticks = vec![serde_json::from_value(serde_json::json!({"artifact_id":"doc-1","origin":"https://tickloop.ai","title":"Brief","format":"markdown","url":"https://tickloop.ai/a/doc-1","access_role":"owner","added_at_ms":1})).unwrap()];
        let mut context = MonitorContext {
            chat: Some(Arc::new(chat)),
            shells: Some(tickloop_agent_protocol::ShellTaskSnapshot {
                chat_id: ended.chat_id,
                live_revision: 1,
                tasks: vec![ended],
            }),
            subscriptions: vec![subscription(&quote, Some(5_000), 3)],
            // A human edit counts for unread without being a Tick Update.
            metadata: [(
                artifact.to_string(),
                Some(serde_json::from_value(serde_json::json!({"id":"doc-1","title":"Brief","format":"markdown","access_role":"owner","human_updated_at":"1970-01-01T00:00:04.000Z"})).unwrap()),
            )]
            .into(),
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

        let chat = Arc::make_mut(context.chat.as_mut().unwrap());
        chat.monitor.mark_taken_in(quote.clone(), 5_000);
        chat.monitor.mark_handled(calendar.clone(), 7_500);
        chat.monitor.mark_taken_in(shell.clone(), 8_000);
        chat.monitor.mark_handled(artifact.clone(), 4_500);
        assert!(
            unread(&context).values().all(Option::is_none),
            "{:?}",
            unread(&context)
        );
        let rows = MonitorRowsRegistry::builtin().list(&context);
        let taken_in = |instance: &InstanceRef| {
            rows.iter()
                .find(|row| &row.instance == instance)
                .and_then(|row| row.update.as_ref()?.taken_in_at_ms)
        };
        assert_eq!(taken_in(&quote), Some(5_000), "a turn took the delivery in");
        assert_eq!(taken_in(&shell), Some(8_000));
        assert_eq!(taken_in(&artifact), None, "the user opened it");
        assert_eq!(
            taken_in(&calendar),
            None,
            "an earlier take-in does not cover the later update the user opened"
        );
        let quote_row = rows.iter().find(|row| row.instance == quote).unwrap();
        assert_eq!(quote_row.status, "live");
        assert!(quote_row.notes.is_empty(), "{:?}", quote_row.notes);
        assert_eq!(
            quote_row.subscription,
            Some(SubscriptionMark {
                paused: false,
                frequency: TickSubscriptionFrequency::High,
                is_waiting: true,
                next_check: None,
            }),
            "the mark counts nothing"
        );
        assert_eq!(quote_row.update.as_ref().unwrap().at_ms, 5_000);
    }

    #[test]
    fn artifact_rows_name_their_type_and_the_catalog_format_wins_over_the_joined_one() {
        let mut chat = empty_chat();
        chat.artifact_ticks = [("doc", "markdown"), ("page", "html"), ("deck", "slides")]
            .into_iter()
            .map(|(id, format)| {
                serde_json::from_value(serde_json::json!({"artifact_id":id,"origin":"https://tickloop.ai","title":id,"format":format,"url":format!("https://tickloop.ai/artifact/{id}"),"access_role":"owner","added_at_ms":1})).unwrap()
            })
            .collect();
        let mut context = MonitorContext {
            chat: Some(Arc::new(chat)),
            ..Default::default()
        };
        let kinds = |context: &MonitorContext| {
            MonitorRowsRegistry::builtin()
                .list(context)
                .into_iter()
                .map(|row| (row.title, row.kind))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        assert_eq!(
            kinds(&context),
            [
                ("deck".to_owned(), "Artifact·Slides".to_owned()),
                ("doc".to_owned(), "Artifact·Doc".to_owned()),
                ("page".to_owned(), "Artifact·Page".to_owned()),
            ]
            .into()
        );
        context.metadata.insert(
            InstanceRef::artifact("https://tickloop.ai", "doc").unwrap().to_string(),
            Some(serde_json::from_value(serde_json::json!({"id":"doc","title":"doc","format":"slides","access_role":"owner"})).unwrap()),
        );
        assert_eq!(kinds(&context)["doc"], "Artifact·Slides");
    }

    #[test]
    fn a_tick_row_marks_its_most_frequent_subscription_and_the_nearest_check_holding_updates() {
        let quote = InstanceRef::new("hosted", "quote-run").unwrap();
        let checked = |frequency, next_check_at_ms| TickSubscriptionStatus {
            paused: false,
            frequency,
            next_check_at_ms: Some(next_check_at_ms),
            period_ms: frequency
                .period()
                .map(|period: std::time::Duration| period.as_millis() as u64),
            ..subscription(&quote, None, 1)
        };
        let mark = |subscriptions: Vec<TickSubscriptionStatus>| {
            MonitorRowsRegistry::builtin()
                .list(&MonitorContext {
                    subscriptions,
                    ..Default::default()
                })
                .into_iter()
                .find(|row| row.instance == quote)
                .and_then(|row| row.subscription)
        };
        assert_eq!(
            mark(vec![
                checked(TickSubscriptionFrequency::Low, 65_000),
                checked(TickSubscriptionFrequency::High, 70_000),
            ]),
            Some(SubscriptionMark {
                paused: false,
                frequency: TickSubscriptionFrequency::High,
                is_waiting: true,
                next_check: Some(NextCheck {
                    at_ms: 65_000,
                    wait_ms: 1_800_000,
                }),
            }),
            "the most frequent colours the mark and the nearest check hands the updates over"
        );
        assert_eq!(
            mark(vec![
                checked(TickSubscriptionFrequency::Low, 65_000),
                TickSubscriptionStatus {
                    accumulated: 0,
                    ..checked(TickSubscriptionFrequency::High, 60_000)
                },
            ])
            .and_then(|mark| mark.next_check)
            .map(|check| check.at_ms),
            Some(65_000),
            "a sooner check with nothing to hand over is not the one the row waits for"
        );
        assert_eq!(
            mark(vec![TickSubscriptionStatus {
                accumulated: 0,
                ..checked(TickSubscriptionFrequency::High, 60_000)
            }]),
            Some(SubscriptionMark {
                paused: false,
                frequency: TickSubscriptionFrequency::High,
                is_waiting: false,
                next_check: None,
            }),
            "a quiet subscription waits for nothing"
        );
        assert_eq!(
            mark(vec![TickSubscriptionStatus {
                accumulated: 0,
                has_pending_change: true,
                pending_since_ms: None,
                program_exit: None,
                ..checked(TickSubscriptionFrequency::High, 60_000)
            }]),
            Some(SubscriptionMark {
                paused: false,
                frequency: TickSubscriptionFrequency::High,
                is_waiting: true,
                next_check: Some(NextCheck {
                    at_ms: 60_000,
                    wait_ms: 60_000,
                }),
            }),
            "a Terminal Tick's changed screen waits for the check that reads it"
        );
        assert_eq!(
            mark(vec![TickSubscriptionStatus {
                period_ms: None,
                ..checked(TickSubscriptionFrequency::Medium, 65_000)
            }]),
            Some(SubscriptionMark {
                paused: false,
                frequency: TickSubscriptionFrequency::Medium,
                is_waiting: true,
                next_check: None,
            }),
            "a check without its period counts nothing down"
        );

        let shell_task = task("make", None);
        let shell = InstanceRef::shell(shell_task.id);
        let rows = MonitorRowsRegistry::builtin().list(&MonitorContext {
            shells: Some(tickloop_agent_protocol::ShellTaskSnapshot {
                chat_id: shell_task.chat_id,
                live_revision: 1,
                tasks: vec![shell_task],
            }),
            subscriptions: vec![TickSubscriptionStatus {
                paused: false,
                frequency: TickSubscriptionFrequency::Immediate,
                ..subscription(&shell, None, 0)
            }],
            ..Default::default()
        });
        assert_eq!(rows[0].instance, shell);
        assert_eq!(
            rows[0].subscription, None,
            "a Shell's immediate subscription is its default and shows no mark"
        );
    }

    #[test]
    fn a_countdown_fills_columns_as_its_period_passes_and_names_the_next_change() {
        let check = NextCheck {
            at_ms: 160_000,
            wait_ms: 60_000,
        };
        for (now, filled) in [
            (90_000, 0),
            (100_000, 0),
            (101_499, 0),
            (101_500, 1),
            (130_000, 20),
            (160_000, 40),
            (200_000, 40),
        ] {
            assert_eq!(check.elapsed_columns(40, now), filled, "at {now}");
        }
        assert_eq!(check.next_column_at(40, 100_000), Some(101_500));
        assert_eq!(check.next_column_at(40, 101_500), Some(103_000));
        assert_eq!(check.next_column_at(40, 158_499), Some(158_500));
        assert_eq!(check.next_column_at(40, 158_500), Some(160_000));
        assert_eq!(
            check.next_column_at(40, 160_000),
            None,
            "a due check is full"
        );
        assert_eq!(check.next_column_at(0, 100_000), None);
    }

    #[test]
    fn a_running_shell_and_a_never_updated_tick_have_nothing_to_read() {
        let running = task("make", None);
        let pane = InstanceRef::new("hosted", "terminal-run").unwrap();
        let mut chat = empty_chat();
        chat.monitor
            .record_opened(pane.clone(), Some("make dev".into()));
        let context = MonitorContext {
            chat: Some(Arc::new(chat)),
            shells: Some(tickloop_agent_protocol::ShellTaskSnapshot {
                chat_id: running.chat_id,
                live_revision: 1,
                tasks: vec![running.clone()],
            }),
            member_states: [(pane.clone(), MemberState::Live)].into(),
            ..Default::default()
        };
        let rows = MonitorRowsRegistry::builtin().list(&context);
        assert!(rows.iter().all(|row| row.update.is_none()), "{rows:?}");
    }

    #[test]
    fn an_exited_hosted_terminal_tick_shows_how_it_ended_and_stays_listed() {
        let exited = InstanceRef::new("hosted", "exited-run").unwrap();
        let mut chat = empty_chat();
        chat.monitor.record_opened(exited.clone(), None);
        let context = MonitorContext {
            chat: Some(Arc::new(chat)),
            member_states: [(exited.clone(), MemberState::Exited("exited 3".into()))].into(),
            ..Default::default()
        };
        let rows = MonitorRowsRegistry::builtin().list(&context);
        let row = rows.iter().find(|row| row.instance == exited).unwrap();
        assert_eq!(row.status, "exited 3");
        assert!(!row.running);
        assert_eq!(row.tone, StatusTone::Neutral);
    }

    #[test]
    fn ended_ticks_stay_listed_until_the_user_removes_them() {
        let gone = InstanceRef::new("hosted", "gone-run").unwrap();
        let lost = InstanceRef::new("hosted", "lost-run").unwrap();
        let mut chat = empty_chat();
        for instance in [&gone, &lost] {
            chat.monitor.record_opened(instance.clone(), None);
        }
        let context = MonitorContext {
            chat: Some(Arc::new(chat)),
            member_states: [
                (gone.clone(), MemberState::Gone),
                (lost.clone(), MemberState::Lost),
            ]
            .into(),
            now_ms: u64::MAX / 2,
            ..Default::default()
        };
        let rows = MonitorRowsRegistry::builtin().list(&context);
        assert_eq!(rows.len(), 2, "however long ago they ended: {rows:?}");
        for row in rows {
            assert_eq!(row.tone, StatusTone::Gone, "{}", row.instance);
            assert_eq!(
                row.actions
                    .iter()
                    .map(|action| (action.key, action.label))
                    .collect::<Vec<_>>(),
                [('o', "reopen"), ('x', "remove")],
                "{}",
                row.instance
            );
        }
    }

    #[test]
    fn a_tick_of_an_unavailable_host_is_neither_running_nor_lost() {
        let kept = InstanceRef::new("hosted", "kept-run").unwrap();
        let mut chat = empty_chat();
        chat.monitor.record_opened(kept.clone(), None);
        let context = MonitorContext {
            chat: Some(Arc::new(chat)),
            member_states: [(kept.clone(), MemberState::Unavailable)].into(),
            ..Default::default()
        };
        let rows = MonitorRowsRegistry::builtin().list(&context);
        let row = rows.iter().find(|row| row.instance == kept).unwrap();
        assert_eq!(row.status, "unavailable");
        assert_eq!(row.tone, StatusTone::Degraded);
        assert!(!row.running);
        let actions = row
            .actions
            .iter()
            .map(|action| (action.key, action.label))
            .collect::<Vec<_>>();
        assert!(
            actions.starts_with(&[('o', "open"), ('x', "close")]),
            "nothing offers to reopen a Tick that may still run: {actions:?}"
        );
    }
}
