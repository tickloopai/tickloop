//! The native Tick tools a worker installs, and the checks each call passes before any host is
//! asked. Every Tick runs in the Tickhost as a `hosted:` instance; a worker in a Herdr View can
//! also show its Ticks in Herdr Panes and list a Workspace's Panes. Artifact and Shell references
//! go to their native tools instead.

use serde_json::{Value, json};
use tick_protocol::HOSTED_INSTANCE_TYPE;
use tickhost_connect::{hosted::ATTACH_INPUT_BYTES, page::MAX_PAGE_SIZE};

use tickloop_agent_protocol::ToolDefinition;

pub const READ_WORKSPACE: &str = "ReadWorkspace";
pub const READ_TICK: &str = "ReadTick";
pub const CALL_TICK_TOOL: &str = "CallTickTool";
pub const MODIFY_TICK: &str = "ModifyTick";
/// The open placement that shows a Tick nowhere. An open without a placement means the same.
pub const HIDDEN_PLACEMENT: &str = "hidden";
const PLACEMENT_DIRECTIONS: [&str; 4] = ["left", "right", "up", "down"];
const HOSTED_REFERENCE: &str = "hosted:<instance id> from an open";

#[derive(Debug, thiserror::Error)]
pub enum TickToolTargetError {
    #[error(
        "NativeToolRequired: {operation} cannot operate on an Artifact Tick; use ArtifactRead, ArtifactEdit and ArtifactComment with artifact_id"
    )]
    Artifact { operation: String },
    #[error(
        "NativeToolRequired: {operation} cannot operate on a Shell Tick; use Bash, Read on its output_file, and TaskStop with task_id"
    )]
    Shell { operation: String },
    #[error(
        "InvalidTickTarget: {operation} requires the complete {HOSTED_REFERENCE}; use ArtifactRead/ArtifactEdit/ArtifactComment for artifacts and Bash/Read/TaskStop for shells"
    )]
    Invalid { operation: String },
}

/// Whether the call places, moves or lists Herdr Panes, which only a worker in a Herdr View can
/// do and which stays inside the Workspace a delegated Agent inherited.
pub(crate) fn uses_herdr_layout(name: &str, input: &Value) -> bool {
    match name {
        READ_WORKSPACE => true,
        MODIFY_TICK => match input["action"].as_str() {
            Some("move") => true,
            Some("open") => input["placement"]["type"]
                .as_str()
                .is_some_and(|placement| placement != HIDDEN_PLACEMENT),
            _ => false,
        },
        _ => false,
    }
}

/// Refuses a call these tools cannot serve, decided from its input alone: an unknown tool or
/// action, a target that is not a hosted Tick, or a Herdr Pane without Herdr `views`.
pub(crate) fn check_call(name: &str, input: &Value, views: bool) -> Result<(), String> {
    match name {
        READ_WORKSPACE => {}
        READ_TICK | CALL_TICK_TOOL => check_target(name, input)?,
        MODIFY_TICK => match input["action"].as_str() {
            Some("open") => {}
            Some("move" | "send_input" | "close" | "reopen") => check_target(name, input)?,
            _ => {
                return Err(
                    "ModifyTick action must be open, move, send_input, close or reopen".into(),
                );
            }
        },
        _ => return Err(format!("unknown Tick tool: {name}")),
    }
    if uses_herdr_layout(name, input) && !views {
        return Err(format!(
            "UnsupportedCapability: this Agent does not run in Herdr, so Workspaces and Tick views in Herdr Panes are unavailable; omit placement or pass {{\"type\":\"{HIDDEN_PLACEMENT}\"}} to open the Tick in the Tickhost alone"
        ));
    }
    Ok(())
}

fn check_target(name: &str, input: &Value) -> Result<(), String> {
    let operation = name.to_owned();
    let target = input["tick_instance_id"].as_str().unwrap_or_default();
    let error = match target.split_once(':').map(|(kind, _)| kind) {
        Some(HOSTED_INSTANCE_TYPE) => return Ok(()),
        Some("artifact") => TickToolTargetError::Artifact { operation },
        Some("shell") => TickToolTargetError::Shell { operation },
        _ => TickToolTargetError::Invalid { operation },
    };
    Err(error.to_string())
}

/// The model-visible Tick tools; Herdr `views` add ReadWorkspace, placements and move.
pub(crate) fn definitions(views: bool) -> Vec<ToolDefinition> {
    let mut definitions = Vec::new();
    if views {
        definitions.push(ToolDefinition {
            name: READ_WORKSPACE.into(),
            description: format!("List the Panes of the current Herdr Workspace, or of an explicitly named Workspace, marking each Pane that shows a view of one of this Chat's Ticks, and this Chat's Ticks as the Tickhost runs them. Uses the Agent's Work Context by default; without one, lists every Workspace. The optional caller describes the View carrying this input, and may be null while the Agent works in the background. Panes come in pages of at most {MAX_PAGE_SIZE} with the complete total; pass next_cursor back as cursor until it is null. CursorExpired means the Workspace changed: list again from the first page. Listing never subscribes."),
            input_schema: json!({"type":"object","properties":{"workspace_id":{"type":"string","minLength":1},"cursor":{"type":"object","properties":{"generation":{"type":"string"},"after":{"type":"string"}},"required":["generation","after"],"additionalProperties":false},"limit":{"type":"integer","minimum":1,"maximum":MAX_PAGE_SIZE}},"additionalProperties":false}),
        });
    }
    let target = json!({"tick_instance_id":{"type":"string","minLength":1}});
    definitions.push(ToolDefinition {
        name: READ_TICK.into(),
        description: "Read a Tick this Chat opened using its complete hosted: instance identity. Protocol Ticks return their state (seq, at, and data, which is null until the Tick has content) and their declared tool catalog. Terminal Ticks return their screen as plain text (data.text, at most 200 lines and 64 KiB) and data.exit, null while the program runs and its exit code or signal once it ended. Reading never subscribes. For artifacts use ArtifactRead; for Shell tasks use Read on Bash's output_file.".into(),
        input_schema: json!({"type":"object","properties":target,"required":["tick_instance_id"],"additionalProperties":false}),
    });
    let mut call_properties = target.clone();
    call_properties["tool"] = json!({"type":"string","minLength":1});
    call_properties["args"] = json!({"type":"object"});
    definitions.push(ToolDefinition {
        name: CALL_TICK_TOOL.into(),
        description: "Call one tool declared by a protocol Tick using its complete hosted: instance identity. Read that Tick's catalog first; query tools read content, while modifying tools require permission. Use ArtifactRead/ArtifactEdit/ArtifactComment for artifacts and Bash/Read/TaskStop for Shell tasks.".into(),
        input_schema: json!({"type":"object","properties":call_properties,"required":["tick_instance_id","tool","args"],"additionalProperties":false}),
    });
    definitions.push(ToolDefinition {
        name: MODIFY_TICK.into(),
        description: modify_description(views),
        input_schema: modify_schema(views),
    });
    definitions
}

fn modify_description(views: bool) -> String {
    let mut parts = vec![
        "open runs the program in the Tickhost: it keeps running without any View or layout, joins this Chat's Monitor, and is not subscribed. Pass protocol: true for a program that speaks the Tick protocol; the bundled tick always does. Any other program runs as a terminal Tick: ReadTick returns its screen text and how it exited, and send_input types into it.".to_owned(),
    ];
    if views {
        parts.push("Omit placement, or pass {\"type\":\"hidden\"}, to show the Tick nowhere. An explicit placement also shows it in a new Herdr Pane as a view of the Tick: current_tab places relative to the View captured for this input, which must still be valid; pane places relative to the named Pane in the selected Workspace; new_tab opens a new Tab. Explicit Pane targets and new_tab work without an Agent View. Direction accepts left, right, up, or down and defaults to right. Placement is frozen before approval and never follows UI focus. move moves the Herdr Pane that shows the Tick, or shows the Tick in a new Pane when none does, and defaults to current_tab; for an explicit relative direction, invoke move even when the view is already in the target Tab. Closing a view Pane closes only that view; moving or closing views never changes the Tick, its identity or its subscriptions.".into());
    }
    parts.push(format!(
        "send_input types text, then logical keys ({}), into a terminal Tick, at most {ATTACH_INPUT_BYTES} bytes; a protocol Tick is operated with CallTickTool.",
        crate::hosted::terminal_input::KEY_NAMES
    ));
    parts.push(
        if views {
            "close ends the Tick's process and closes the Herdr Panes that show it."
        } else {
            "close ends the Tick's process."
        }
        .into(),
    );
    parts.push("reopen starts a new instance from the recorded program of one lost with its Tickhost; it needs approval again, gets a new identity, and no subscription follows it.".into());
    parts.push(format!(
        "Omitted cwd uses the Agent file work directory; for command[0]='tick', prefer the bundled sibling executable, then PATH. Results report only verified facts: the instance{}. Use the tickloop Skill for guidance.",
        if views {
            ", whether a Herdr Pane shows it and where"
        } else {
            ""
        }
    ));
    parts.join(" ")
}

fn modify_schema(views: bool) -> Value {
    let mut placements = vec![
        json!({"type":"object","properties":{"type":{"const":HIDDEN_PLACEMENT}},"required":["type"],"additionalProperties":false}),
    ];
    if views {
        placements.extend(herdr_placements(true));
    }
    let mut open = json!({"type":"object","properties":{"action":{"const":"open"},"command":{"type":"array","minItems":1,"items":{"type":"string"}},"cwd":{"type":"string","minLength":1},"protocol":{"type":"boolean"},"placement":{"type":"object","oneOf":placements}},"required":["action","command"],"additionalProperties":false});
    if views {
        open["properties"]["workspace_id"] = json!({"type":"string","minLength":1});
    }
    let mut variants = vec![open];
    if views {
        variants.push(json!({"type":"object","properties":{"action":{"const":"move"},"tick_instance_id":{"type":"string","minLength":1},"workspace_id":{"type":"string","minLength":1},"placement":{"type":"object","oneOf":herdr_placements(false)}},"required":["action","tick_instance_id"],"additionalProperties":false}));
    }
    variants.push(json!({"type":"object","properties":{"tick_instance_id":{"type":"string","minLength":1},"action":{"type":"string","enum":["send_input","close"]},"text":{"type":"string"},"keys":{"type":"array","minItems":1,"items":{"type":"string"}}},"required":["tick_instance_id","action"],"additionalProperties":false}));
    variants.push(json!({"type":"object","properties":{"action":{"const":"reopen"},"tick_instance_id":{"type":"string","minLength":1}},"required":["action","tick_instance_id"],"additionalProperties":false}));
    json!({"type":"object","oneOf":variants})
}

fn herdr_placements(allow_new_tab: bool) -> Vec<Value> {
    let mut variants = vec![
        json!({"type":"object","properties":{"type":{"const":"current_tab"},"direction":{"enum":PLACEMENT_DIRECTIONS}},"required":["type"],"additionalProperties":false}),
        json!({"type":"object","properties":{"type":{"const":"pane"},"pane_id":{"type":"string","minLength":1},"direction":{"enum":PLACEMENT_DIRECTIONS}},"required":["type","pane_id"],"additionalProperties":false}),
    ];
    if allow_new_tab {
        variants.push(json!({"type":"object","properties":{"type":{"const":"new_tab"}},"required":["type"],"additionalProperties":false}));
    }
    variants
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_placements_expose_four_directions_for_both_anchor_types() {
        for allow_new_tab in [false, true] {
            let variants = herdr_placements(allow_new_tab);
            for anchor_type in ["current_tab", "pane"] {
                let variant = variants
                    .iter()
                    .find(|variant| variant["properties"]["type"]["const"] == anchor_type)
                    .unwrap();
                assert_eq!(
                    variant["properties"]["direction"]["enum"],
                    json!(["left", "right", "up", "down"])
                );
                assert!(
                    !variant["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("direction"))
                );
            }
            assert_eq!(
                variants
                    .iter()
                    .any(|variant| variant["properties"]["type"]["const"] == "new_tab"),
                allow_new_tab
            );
        }
    }

    #[test]
    fn only_hosted_targets_are_served_and_herdr_panes_need_views() {
        for (name, input) in [
            (READ_TICK, json!({"tick_instance_id":"herdr:w1/p1"})),
            (CALL_TICK_TOOL, json!({"tick_instance_id":"calendar:1"})),
            (
                MODIFY_TICK,
                json!({"action":"close","tick_instance_id":"terminal:1"}),
            ),
        ] {
            let refused = check_call(name, &input, true).unwrap_err();
            assert!(refused.starts_with("InvalidTickTarget"), "{refused}");
        }
        assert!(
            check_call(READ_TICK, &json!({"tick_instance_id":"shell:1"}), true)
                .unwrap_err()
                .starts_with("NativeToolRequired")
        );
        let hidden = json!({"action":"open","command":["tick"],"placement":{"type":"hidden"}});
        let shown = json!({"action":"open","command":["tick"],"placement":{"type":"new_tab"}});
        let moved = json!({"action":"move","tick_instance_id":"hosted:1"});
        for (name, input) in [
            (READ_WORKSPACE, json!({})),
            (MODIFY_TICK, shown.clone()),
            (MODIFY_TICK, moved.clone()),
        ] {
            assert!(uses_herdr_layout(name, &input), "{input}");
            assert!(check_call(name, &input, true).is_ok(), "{input}");
            assert!(
                check_call(name, &input, false)
                    .unwrap_err()
                    .starts_with("UnsupportedCapability"),
                "{input}"
            );
        }
        assert!(!uses_herdr_layout(MODIFY_TICK, &hidden));
        assert!(check_call(MODIFY_TICK, &hidden, false).is_ok());
    }
}
