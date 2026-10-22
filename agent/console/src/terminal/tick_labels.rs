//! Human-readable Tick action labels shared by live output and history.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use serde_json::Value;
use tick_protocol::InstanceRef;
use tickloop_agent_protocol::{
    ChatSnapshot, Content, MonitorMembership, ObservationKind, TickDeliveryLine,
    TickSubscriptionStatus,
};
use uuid::Uuid;

use super::{
    conversation_picker::ellipsize,
    display::{DisplayProfile, Glyph},
    presentation::{OutputDetail, RoleRuns},
    safe_line,
    style::Role,
};

pub(super) const TICK_NAME_COLUMNS: usize = 48;
const SHORT_INSTANCE_CHARACTERS: usize = 8;

#[derive(Default)]
pub(super) struct TickLabels {
    names: HashMap<InstanceRef, (String, Glyph)>,
    subscriptions: HashMap<Uuid, InstanceRef>,
    shell_outputs: HashMap<PathBuf, InstanceRef>,
    artifacts: HashMap<String, InstanceRef>,
    workdir: PathBuf,
}

impl TickLabels {
    pub(super) fn from_membership(membership: &MonitorMembership) -> Self {
        let mut labels = Self::default();
        for member in &membership.members {
            if let Some(title) = &member.title {
                labels.names.insert(
                    member.instance.clone(),
                    (
                        title.clone(),
                        if member.is_terminal {
                            Glyph::TerminalTick
                        } else {
                            Glyph::ProtocolTick
                        },
                    ),
                );
            }
        }
        labels
    }

    pub(super) fn include_native(
        &mut self,
        commands: &[tickloop_agent_protocol::CommandSnapshot],
        shells: &[tickloop_agent_protocol::UserShellRecord],
        artifacts: &[tickloop_agent_protocol::ArtifactTick],
        workdir: &Path,
    ) {
        self.workdir = workdir.to_owned();
        for command in commands {
            let instance = InstanceRef::shell(command.operation_id);
            self.shell_outputs
                .insert(command.output_ref.clone(), instance.clone());
            self.names.insert(
                instance,
                (
                    command
                        .description
                        .as_ref()
                        .unwrap_or(&command.command)
                        .clone(),
                    Glyph::ShellTick,
                ),
            );
        }
        for shell in shells {
            let instance = InstanceRef::shell(shell.id);
            if let Some(path) = &shell.output_ref {
                self.shell_outputs.insert(path.clone(), instance.clone());
            }
            self.names
                .insert(instance, (shell.command.clone(), Glyph::ShellTick));
        }
        for artifact in artifacts {
            if let Ok(instance) = InstanceRef::artifact(&artifact.origin, &artifact.artifact_id) {
                self.artifacts
                    .insert(artifact.artifact_id.clone(), instance.clone());
                self.names
                    .insert(instance, (artifact.title.clone(), Glyph::ArtifactTick));
            }
        }
    }

    pub(super) fn artifact_instance(&self, id: &str) -> Option<InstanceRef> {
        self.artifacts.get(id).cloned()
    }

    pub(super) fn artifact_title(&self, id: &str) -> Option<String> {
        self.artifacts
            .get(id)
            .and_then(|instance| self.names.get(instance))
            .map(|(title, _)| title.clone())
    }

    pub(super) fn native_tool_instance(&self, name: &str, input: &Value) -> Option<InstanceRef> {
        match name {
            "Read" => {
                let path = Path::new(input["file_path"].as_str()?);
                let path = if path.is_absolute() {
                    path.to_owned()
                } else {
                    self.workdir.join(path)
                };
                self.shell_outputs.get(&path).cloned()
            }
            "ArtifactRead" if matches!(input["action"].as_str(), Some("read" | "threads")) => {
                self.artifacts.get(input["artifact_id"].as_str()?).cloned()
            }
            "ArtifactEdit" | "ArtifactComment" => {
                self.artifacts.get(input["artifact_id"].as_str()?).cloned()
            }
            _ => None,
        }
    }

    pub(super) fn include_subscription(&mut self, row: &TickSubscriptionStatus) {
        self.subscriptions.insert(row.id, row.instance.clone());
        if let Some(title) = &row.title {
            self.names
                .entry(row.instance.clone())
                .or_insert_with(|| (title.clone(), kind_glyph(&row.instance, row.kind.as_ref())));
        }
        if matches!(row.kind, Some(ObservationKind::Terminal { .. }))
            && let Some((_, glyph)) = self.names.get_mut(&row.instance)
        {
            *glyph = Glyph::TerminalTick;
        }
    }

    pub(super) fn from_snapshot(snapshot: &ChatSnapshot) -> Self {
        let mut labels = Self::from_membership(&snapshot.monitor);
        labels.include_native(
            &snapshot.commands,
            &snapshot.user_shells,
            &snapshot.artifact_ticks,
            super::shown_folder(snapshot.folder.as_deref()),
        );
        for subscription in snapshot
            .chat_loop
            .trigger
            .iter()
            .flat_map(|trigger| &trigger.subscriptions)
        {
            labels
                .subscriptions
                .insert(subscription.id, subscription.instance.clone());
        }
        for content in snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
        {
            if let Content::ToolResult { content, .. } = content
                && let Ok(result) = serde_json::from_str::<Value>(&content.text())
                && let (Ok(id), Ok(instance)) = (
                    serde_json::from_value(result["subscription"].clone()),
                    serde_json::from_value(result["tick_instance_id"].clone()),
                )
            {
                labels.subscriptions.insert(id, instance);
            }
        }
        labels
    }

    pub(super) fn merge(&mut self, newer: Self) {
        self.names.extend(newer.names);
        self.subscriptions.extend(newer.subscriptions);
        self.shell_outputs.extend(newer.shell_outputs);
        self.artifacts.extend(newer.artifacts);
        self.workdir = newer.workdir;
    }

    pub(super) fn name_delivery(&self, item: &mut TickDeliveryLine) {
        if let Some((title, glyph)) = self.names.get(&item.instance) {
            item.title = Some(title.clone());
            if *glyph == Glyph::TerminalTick {
                item.kind = Some(ObservationKind::Terminal { truncated: false });
            }
        }
    }

    pub(super) fn tool_runs(
        &self,
        name: &str,
        input: &Value,
        display: DisplayProfile,
    ) -> Option<RoleRuns> {
        if let Some(instance) = self.native_tool_instance(name, input)
            && let Some((title, glyph)) = self.names.get(&instance)
        {
            return Some(label_runs(
                name,
                title,
                *glyph,
                Role::MutedText,
                String::new(),
                display,
            ));
        }
        let (action, suffix) = match name {
            "TickDescribe" | "TickRead" => (None, String::new()),
            "TickCall" => (None, input["tool"].as_str().unwrap_or_default().to_owned()),
            "TickManage" => {
                let action = input["action"].as_str()?;
                let suffix = if action == "subscribe" {
                    input["frequency"].as_str().unwrap_or("high").to_owned()
                } else {
                    String::new()
                };
                (Some(action), suffix)
            }
            _ => return None,
        };
        let tag = name;
        let instance = serde_json::from_value::<InstanceRef>(input["tick_instance_id"].clone())
            .ok()
            .or_else(|| {
                serde_json::from_value::<Uuid>(input["subscription"].clone())
                    .ok()
                    .and_then(|id| self.subscriptions.get(&id).cloned())
            });
        let known = instance
            .as_ref()
            .and_then(|instance| self.names.get(instance));
        let command = input["command"].as_array().map(|argv| {
            argv.iter()
                .filter_map(Value::as_str)
                .map(|arg| {
                    if Path::new(arg).is_absolute() {
                        Path::new(arg)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or(arg)
                    } else {
                        arg
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        });
        let title = known
            .map(|(title, _)| title.clone())
            .or(command)
            .unwrap_or_else(|| {
                instance
                    .as_ref()
                    .map(short_name)
                    .unwrap_or_else(|| "Tick".into())
            });
        let glyph = if matches!(action, Some("subscribe" | "unsubscribe")) {
            Glyph::Subscription
        } else {
            known.map_or(Glyph::ToolCall, |(_, glyph)| *glyph)
        };
        let suffix = match suffix.as_str() {
            "high" => "1 min",
            "medium" => "10 min",
            "low" => "30 min",
            other => other,
        };
        let mut runs = label_runs(
            tag,
            &title,
            glyph,
            Role::MutedText,
            if suffix.is_empty() {
                String::new()
            } else {
                format!(" · {}", safe_line(suffix))
            },
            display,
        );
        if let Some(action) = action {
            runs.insert(
                3,
                (format!("{} · ", safe_line(action)), Some(Role::MutedStrong)),
            );
        }
        Some(runs)
    }
}

pub(super) fn delivery_runs(
    items: &[TickDeliveryLine],
    detail: OutputDetail,
    display: DisplayProfile,
) -> RoleRuns {
    let mut runs = Vec::new();
    for item in items {
        let title = item
            .title
            .clone()
            .unwrap_or_else(|| short_name(&item.instance));
        let mut suffix = item
            .seq
            .as_ref()
            .map(|seq| format!("  #{}", safe_line(seq)))
            .unwrap_or_default();
        if item.updates > 1 {
            suffix.push_str(&format!(" · {} merged", item.updates));
        }
        if item.is_trimmed {
            suffix.push_str(" · trimmed");
        }
        runs.extend(label_runs(
            "TickUpdate",
            &title,
            kind_glyph(&item.instance, item.kind.as_ref()),
            Role::UpdateMark,
            suffix,
            display,
        ));
        if matches!(detail, OutputDetail::Full) {
            runs.push((
                format!("  Tick Instance: {}\n", item.instance),
                Some(Role::MutedText),
            ));
        }
    }
    runs
}

fn label_runs(
    tag: &str,
    title: &str,
    glyph: Glyph,
    glyph_role: Role,
    suffix: String,
    display: DisplayProfile,
) -> RoleRuns {
    vec![
        ("\n".into(), None),
        (format!("{} ", display.glyph(glyph)), Some(glyph_role)),
        (format!("{tag}: "), Some(Role::ToolName)),
        (
            ellipsize(&safe_line(title), TICK_NAME_COLUMNS),
            Some(Role::MutedText),
        ),
        (format!("{suffix}\n"), Some(Role::MutedText)),
    ]
}

fn short_name(instance: &InstanceRef) -> String {
    let id = instance
        .id()
        .chars()
        .take(SHORT_INSTANCE_CHARACTERS)
        .collect::<String>();
    format!("Tick {id}")
}

fn kind_glyph(instance: &InstanceRef, kind: Option<&ObservationKind>) -> Glyph {
    match instance.access() {
        "shell" => Glyph::ShellTick,
        "artifact" => Glyph::ArtifactTick,
        _ if matches!(kind, Some(ObservationKind::Terminal { .. })) => Glyph::TerminalTick,
        _ => Glyph::ProtocolTick,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(runs: &RoleRuns) -> String {
        runs.iter().map(|(text, _)| text.as_str()).collect()
    }

    #[test]
    fn native_reads_keep_their_tools_and_resolve_the_shell_or_artifact() {
        let task = Uuid::new_v4();
        let command: tickloop_agent_protocol::CommandSnapshot = serde_json::from_value(serde_json::json!({
            "operation_id":task,"loop_id":Uuid::new_v4(),"command":"sleep 30","description":"Wait thirty seconds","cwd":"/work","pid":null,"state":"Completed","exit_code":0,"signal":null,"reason":null,"output_ref":"/work/task.output","output_bytes":4
        })).unwrap();
        let artifact: tickloop_agent_protocol::ArtifactTick = serde_json::from_value(serde_json::json!({
            "artifact_id":"doc","origin":"https://docs.example","title":"State atlas","format":"html","url":"https://docs.example/artifact/doc","access_role":"owner","added_at_ms":1
        })).unwrap();
        let mut labels = TickLabels::default();
        labels.include_native(&[command], &[], &[artifact], Path::new("/work"));
        let read = serde_json::json!({"file_path":"task.output"});
        assert_eq!(
            labels.native_tool_instance("Read", &read),
            Some(InstanceRef::shell(task))
        );
        assert_eq!(
            text(
                &labels
                    .tool_runs("Read", &read, DisplayProfile::default())
                    .unwrap()
            ),
            "\n$ Read: Wait thirty seconds\n"
        );
        assert!(
            labels
                .native_tool_instance("Read", &serde_json::json!({"file_path":"source.rs"}))
                .is_none()
        );
        let read = serde_json::json!({"artifact_id":"doc","action":"read"});
        assert_eq!(
            text(
                &labels
                    .tool_runs("ArtifactRead", &read, DisplayProfile::default())
                    .unwrap()
            ),
            "\n↗ ArtifactRead: State atlas\n"
        );
        assert!(
            labels
                .native_tool_instance(
                    "ArtifactRead",
                    &serde_json::json!({"artifact_id":"doc","action":"list"})
                )
                .is_none()
        );
    }

    #[test]
    fn management_actions_and_delivery_share_the_monitor_name_and_type() {
        let instance = InstanceRef::new("hosted", "watcher-instance").unwrap();
        let mut membership = MonitorMembership::default();
        membership
            .record_opened(instance.clone(), Some("python3 duo_stock.py 60".into()))
            .is_terminal = true;
        let labels = TickLabels::from_membership(&membership);
        let display = DisplayProfile::default();
        let managed = labels.tool_runs("TickManage", &serde_json::json!({"action":"subscribe","tick_instance_id":instance,"frequency":"high"}), display).unwrap();
        assert_eq!(
            text(&managed),
            "\n↻ TickManage: subscribe · python3 duo_stock.py 60 · 1 min\n"
        );
        for name in ["TickRead", "TickDescribe"] {
            let runs = labels
                .tool_runs(
                    name,
                    &serde_json::json!({"tick_instance_id":instance}),
                    display,
                )
                .unwrap();
            assert_eq!(
                text(&runs),
                format!("\n> {name}: python3 duo_stock.py 60\n")
            );
        }
        let mut item = TickDeliveryLine {
            instance: instance.clone(),
            title: None,
            kind: None,
            seq: Some("4".into()),
            updates: 1,
            is_trimmed: false,
        };
        labels.name_delivery(&mut item);
        let preview = text(&delivery_runs(
            &[item.clone()],
            OutputDetail::Preview,
            display,
        ));
        assert_eq!(preview, "\n> TickUpdate: python3 duo_stock.py 60  #4\n");
        assert!(!preview.contains(instance.as_str()));
        let full = text(&delivery_runs(&[item], OutputDetail::Full, display));
        assert!(full.contains(instance.as_str()));
    }

    #[test]
    fn long_tick_names_are_bounded_without_losing_the_sequence_or_action() {
        let item = TickDeliveryLine {
            instance: InstanceRef::new("hosted", "long-name").unwrap(),
            title: Some("股票监控".repeat(40)),
            kind: Some(ObservationKind::Protocol),
            seq: Some("99".into()),
            updates: 3,
            is_trimmed: false,
        };
        let shown = text(&delivery_runs(
            &[item],
            OutputDetail::Preview,
            DisplayProfile::default(),
        ));
        assert!(shown.contains('…'));
        assert!(shown.ends_with("  #99 · 3 merged\n"));
        assert!(unicode_width::UnicodeWidthStr::width(shown.trim()) < 90);
    }
}
