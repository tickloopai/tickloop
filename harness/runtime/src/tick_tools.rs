//! The native Tick tools a worker installs, and the checks each call passes before any host is
//! asked. Every Tick runs in the Tickhost as a `hosted:` instance. Artifact and Shell references
//! go to their native tools instead.

use crate::environment::TERMINAL_INPUT_BYTES;
use serde_json::{Value, json};
use tick_protocol::{HOSTED_ACCESS, InstanceRef, SHELL_ACCESS, ShellTaskKind};
use tickloop_ids::{IdContext, ModelRef};

use tickloop_harness_protocol::ToolDefinition;

pub const TICK_DESCRIBE: &str = "TickDescribe";
pub const TICK_READ: &str = "TickRead";
pub const TICK_CALL: &str = "TickCall";
pub const TICK_MANAGE: &str = "TickManage";
const HOSTED_REFERENCE: &str = "hosted:<instance id> from an open";

/// Keys of a Tick tool result that hold what the Tick itself reported; their contents are the
/// Tick's, never rewritten.
const TICK_CONTENT_KEYS: [&str; 5] = ["data", "descriptor", "result", "args", "state"];

pub(crate) fn is_tick_tool(name: &str) -> bool {
    matches!(name, TICK_DESCRIBE | TICK_READ | TICK_CALL | TICK_MANAGE)
}

/// A Tick tool's input with the `tick_instance_id` the model wrote resolved to the stored form.
/// `TickManage` also takes a Shell task by its id alone, as `TaskStop` does.
pub(crate) fn stored_instance_input(ids: &IdContext, name: &str, mut input: Value) -> Value {
    let Some(text) = input["tick_instance_id"].as_str() else {
        return input;
    };
    let instance = match InstanceRef::from_model(ids, text) {
        Ok(instance) => Some(instance),
        Err(_) if name == TICK_MANAGE && !text.contains(':') => ids
            .resolve(&ModelRef::<ShellTaskKind>::from(text))
            .ok()
            .map(InstanceRef::shell),
        Err(_) => None,
    };
    if let Some(instance) = instance {
        input["tick_instance_id"] = instance.as_str().into();
    }
    input
}

/// A Tick tool's result with each Tick Instance it names, outside what the Tick reported, in the
/// Loop's model form.
pub(crate) fn model_tick_result(ids: &IdContext, value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| {
                    let value = match (key.as_str(), value) {
                        (key, value) if TICK_CONTENT_KEYS.contains(&key) => value,
                        ("tick_instance_id" | "replaces" | "instance", Value::String(text)) => {
                            Value::String(
                                text.parse::<InstanceRef>()
                                    .map_or(text, |instance| instance.model_text(ids)),
                            )
                        }
                        (_, value) => model_tick_result(ids, value),
                    };
                    (key, value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| model_tick_result(ids, item))
                .collect(),
        ),
        other => other,
    }
}

/// A Tick tool's plain-text result, such as an error, with each stored `hosted:` or `shell:`
/// identity in it in the Loop's model form.
pub(crate) fn model_tick_text(ids: &IdContext, text: &str) -> String {
    const UUID_LENGTH: usize = 36;
    let mut rewritten = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = [HOSTED_ACCESS, SHELL_ACCESS]
        .iter()
        .filter_map(|access| rest.find(&format!("{access}:")))
        .min()
    {
        let prefix_end = start + rest[start..].find(':').expect("found above") + 1;
        let end = prefix_end + UUID_LENGTH;
        let instance = rest
            .get(start..end)
            .and_then(|candidate| candidate.parse::<InstanceRef>().ok())
            .filter(|instance| instance.hosted_run().is_some() || instance.shell_task().is_some());
        match instance {
            Some(instance) => {
                rewritten.push_str(&rest[..start]);
                rewritten.push_str(&instance.model_text(ids));
                rest = &rest[end..];
            }
            None => {
                rewritten.push_str(&rest[..prefix_end]);
                rest = &rest[prefix_end..];
            }
        }
    }
    rewritten.push_str(rest);
    rewritten
}

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

/// Refuses a call these tools cannot serve, decided from its input alone: an unknown tool or
/// action, or a target that is not a hosted Tick.
pub fn check_call(name: &str, input: &Value) -> Result<(), String> {
    match name {
        TICK_DESCRIBE | TICK_READ | TICK_CALL => check_target(name, input),
        TICK_MANAGE => match input["action"].as_str() {
            Some("open") => Ok(()),
            Some("send_input" | "close" | "reopen") => check_target(name, input),
            _ => Err("TickManage action must be open, send_input, close or reopen".into()),
        },
        _ => Err(format!("unknown Tick tool: {name}")),
    }
}

fn check_target(name: &str, input: &Value) -> Result<(), String> {
    let operation = name.to_owned();
    let target = input["tick_instance_id"].as_str().unwrap_or_default();
    let error = match target.split_once(':').map(|(kind, _)| kind) {
        Some(HOSTED_ACCESS) => return Ok(()),
        Some("artifact") => TickToolTargetError::Artifact { operation },
        Some("shell") => TickToolTargetError::Shell { operation },
        _ => TickToolTargetError::Invalid { operation },
    };
    Err(error.to_string())
}

/// The model-visible Tick tools.
pub fn definitions() -> Vec<ToolDefinition> {
    let mut definitions = Vec::new();
    let target = json!({"tick_instance_id":{"type":"string","minLength":1}});
    definitions.push(ToolDefinition {
        name: TICK_DESCRIBE.into(),
        description: "Describe a running Tick by its complete hosted: instance identity without reading its business content. Extension Ticks return their title, kind, description, revision and declared tool catalog with parameter schemas. Terminal Ticks return their command and working directory. Use the Tick package's Skill for launch commands and login prerequisites; use TickDescribe for the capabilities of a running instance. This does not subscribe or mark its content read.".into(),
        input_schema: json!({"type":"object","properties":target.clone(),"required":["tick_instance_id"],"additionalProperties":false}),
    });
    definitions.push(ToolDefinition {
        name: TICK_READ.into(),
        description: "Read a Tick this Chat opened using its complete hosted: instance identity. Extension Ticks return their state (seq, at, and data, which is null until the Tick has content: a Tick just opened may still be loading, so read it again shortly) only. Use TickDescribe for capabilities and the tool catalog. Terminal Ticks return their screen as plain text (data.text, at most 200 lines and 64 KiB) and data.exit, null while the program runs and its exit code or signal once it ended. Reading never subscribes. For artifacts use ArtifactRead; for Shell tasks use Read on Bash's output_file.".into(),
        input_schema: json!({"type":"object","properties":target,"required":["tick_instance_id"],"additionalProperties":false}),
    });
    let mut call_properties = target.clone();
    call_properties["tool"] = json!({"type":"string","minLength":1});
    call_properties["args"] = json!({"type":"object"});
    definitions.push(ToolDefinition {
        name: TICK_CALL.into(),
        description: "Call one tool declared by an extension Tick using its complete hosted: instance identity. Use TickDescribe for that Tick's catalog first; query tools read content, while modifying tools require permission. Use ArtifactRead/ArtifactEdit/ArtifactComment for artifacts and Bash/Read/TaskStop for Shell tasks.".into(),
        input_schema: json!({"type":"object","properties":call_properties,"required":["tick_instance_id","tool","args"],"additionalProperties":false}),
    });
    definitions.push(ToolDefinition {
        name: TICK_MANAGE.into(),
        description: tick_manage_description(),
        input_schema: tick_manage_schema(),
    });
    definitions
}

fn tick_manage_description() -> String {
    [
        "open runs the program in the Tickhost: it keeps running without any View, joins this Chat's Monitor, and is not subscribed. Pass protocol: true for a program that speaks the Tick protocol; an installed Tick package's program that declares the protocol always does. Any other program runs as a terminal Tick: TickRead returns its screen text and how it exited, and send_input types into it.".to_owned(),
        format!(
            "send_input types text, then logical keys ({}), into a terminal Tick, at most {TERMINAL_INPUT_BYTES} bytes; an extension Tick is operated with TickCall.",
            crate::terminal_input::KEY_NAMES
        ),
        "close ends the Tick's process.".to_owned(),
        "reopen starts a new instance from the recorded program of one lost with its Tickhost; it needs approval again, gets a new identity, and no subscription follows it.".to_owned(),
        "Omitted cwd uses the Agent file work directory. A bare command[0] is found as the program of an installed Tick package (command[0]='tick-<package>'), then on PATH. Results report only verified facts: the instance. Users need not say Tick: a request to see what an installed plugin shows asks for its Tick. Before opening an installed plugin's program, load the tickloop Skill and that plugin's <package>:usage Skill.".to_owned(),
    ]
    .join(" ")
}

/// One accepted shape of an object, selected by the value of its discriminating field. The
/// model sees a single flat object; these forms drive both the field descriptions it reads and
/// the per-form check its input must pass.
struct Form {
    name: &'static str,
    required: &'static [&'static str],
    optional: &'static [&'static str],
}

impl Form {
    fn choice(&self) -> String {
        match self.required {
            [] => self.name.to_owned(),
            required => format!("{} (requires {})", self.name, required.join(", ")),
        }
    }

    fn fields(&self) -> String {
        match (self.required, self.optional) {
            ([], []) => "no other fields".to_owned(),
            (required, []) => format!("only {}", required.join(", ")),
            ([], optional) => format!("optionally {}", optional.join(", ")),
            (required, optional) => format!(
                "{}, and optionally {}",
                required.join(", "),
                optional.join(", ")
            ),
        }
    }

    fn check(
        &self,
        fields: &serde_json::Map<String, Value>,
        selector: &str,
        label: &str,
    ) -> Result<(), String> {
        if let Some(missing) = self
            .required
            .iter()
            .find(|field| !fields.contains_key(**field))
        {
            return Err(format!("{label} requires {missing}"));
        }
        if let Some(extra) = fields.keys().find(|field| {
            *field != selector
                && !self.required.contains(&field.as_str())
                && !self.optional.contains(&field.as_str())
        }) {
            return Err(format!(
                "{extra} is not allowed for {label}; {label} takes {}",
                self.fields()
            ));
        }
        Ok(())
    }
}

const OPEN: Form = Form {
    name: "open",
    required: &["command"],
    optional: &["cwd", "protocol"],
};
const SEND_INPUT: Form = Form {
    name: "send_input",
    required: &["tick_instance_id"],
    optional: &["text", "keys"],
};
const CLOSE: Form = Form {
    name: "close",
    required: &["tick_instance_id"],
    optional: &[],
};
const REOPEN: Form = Form {
    name: "reopen",
    required: &["tick_instance_id"],
    optional: &[],
};

const TICK_MANAGE_FORMS: [Form; 4] = [OPEN, SEND_INPUT, CLOSE, REOPEN];

/// Which forms require or accept `field`, leading its description so the model knows when to
/// pass it without a schema variant per form.
fn usage(field: &str, forms: &[Form]) -> String {
    let names = |list: fn(&Form) -> &'static [&'static str]| {
        forms
            .iter()
            .filter(|form| list(form).contains(&field))
            .map(|form| form.name)
            .collect::<Vec<_>>()
    };
    [
        ("Required by", names(|form| form.required)),
        ("Optional for", names(|form| form.optional)),
    ]
    .into_iter()
    .filter(|(_, names)| !names.is_empty())
    .map(|(usage, names)| format!("{usage} {}.", names.join(", ")))
    .collect::<Vec<_>>()
    .join(" ")
}

fn select_form<'a>(
    fields: &serde_json::Map<String, Value>,
    selector: &str,
    label: &str,
    forms: &'a [Form],
) -> Result<&'a Form, String> {
    let Some(selected) = fields.get(selector) else {
        let choices = forms.iter().map(Form::choice).collect::<Vec<_>>();
        return Err(format!(
            "{label} is required: one of {}",
            choices.join(", ")
        ));
    };
    selected
        .as_str()
        .and_then(|name| forms.iter().find(|form| form.name == name))
        .ok_or_else(|| {
            let names = forms.iter().map(|form| form.name).collect::<Vec<_>>();
            format!("{label} must be one of {}", names.join(", "))
        })
}

/// Checks a Tick tool input before any host is asked. TickManage must match the form its
/// action selects, so a refusal names the field to add or drop; the flat model schema then
/// checks every value.
pub fn validate_input(name: &str, schema: &Value, input: &Value) -> Result<(), String> {
    if name == TICK_MANAGE
        && let Some(fields) = input.as_object()
    {
        let action = select_form(fields, "action", "action", &TICK_MANAGE_FORMS)
            .map_err(|reason| format!("invalid {name} input: {reason}"))?;
        action
            .check(fields, "action", action.name)
            .map_err(|reason| format!("invalid {name} input: {reason}"))?;
    }
    crate::validate_host_schema_input(name, schema, input)
}

fn tick_manage_schema() -> Value {
    let forms = &TICK_MANAGE_FORMS;
    let actions = forms.iter().map(|form| form.name).collect::<Vec<_>>();
    let properties = json!({
        "action": {"type":"string","enum":actions,"description":format!("One of {}; each takes the fields whose description names it.", actions.join(", "))},
        "command": {"type":"array","minItems":1,"items":{"type":"string"},"description":format!("{} The program's argv, executable first.", usage("command", forms))},
        "cwd": {"type":"string","minLength":1,"description":format!("{} The program's working directory; defaults to the Agent file work directory.", usage("cwd", forms))},
        "protocol": {"type":"boolean","description":format!("{} true for a program that speaks the Tick protocol; an installed Tick package's program that declares the protocol needs no flag. Any other program runs as a terminal Tick.", usage("protocol", forms))},
        "tick_instance_id": {"type":"string","minLength":1,"description":format!("{} The complete hosted: identity an open returned.", usage("tick_instance_id", forms))},
        "text": {"type":"string","description":format!("{} Text typed first; pass text, keys or both.", usage("text", forms))},
        "keys": {"type":"array","minItems":1,"items":{"type":"string"},"description":format!("{} Logical keys typed after text, such as Enter, Up or Ctrl-C.", usage("keys", forms))},
    });
    json!({"type":"object","properties":properties,"required":["action"],"additionalProperties":false})
}

#[cfg(test)]
mod tests {
    use super::*;
    use tick_protocol::{HostedRunId, ShellTaskId};

    /// A Shell task reaches TickManage as `shell:` and its id in either form, or as the id alone;
    /// a hosted Tick as `hosted:` and its id in either form. Each is stored as a UUID.
    #[test]
    fn tick_tools_store_instances_the_model_names_in_any_accepted_form() {
        let ids = IdContext::new();
        let task = ShellTaskId::new();
        let run = HostedRunId::new();
        let stored_shell = InstanceRef::shell(task).as_str().to_owned();
        let stored_hosted = InstanceRef::hosted(run).as_str().to_owned();
        for written in [
            ids.model_text(task),
            format!("shell:{}", task.uuid()),
            task.public(),
            task.uuid().to_string(),
        ] {
            let input = stored_instance_input(
                &ids,
                TICK_MANAGE,
                json!({"action":"subscribe","tick_instance_id":written}),
            );
            assert_eq!(input["tick_instance_id"], stored_shell, "{written}");
        }
        for written in [ids.model_text(run), format!("hosted:{}", run.uuid())] {
            let input = stored_instance_input(&ids, TICK_READ, json!({"tick_instance_id":written}));
            assert_eq!(input["tick_instance_id"], stored_hosted, "{written}");
        }
        // Outside TickManage a bare id names no access, and a name the host does not know is
        // left for the host to refuse.
        let bare = json!({"tick_instance_id":task.public()});
        assert_eq!(stored_instance_input(&ids, TICK_READ, bare.clone()), bare);
        let unknown = json!({"tick_instance_id":"hosted:nothing"});
        assert_eq!(
            stored_instance_input(&ids, TICK_READ, unknown.clone()),
            unknown
        );
        let shell = stored_instance_input(
            &ids,
            TICK_READ,
            json!({"tick_instance_id":ids.model_text(task)}),
        );
        assert!(
            check_target(TICK_READ, &shell)
                .unwrap_err()
                .contains("Shell Tick")
        );
    }

    #[test]
    fn tick_tool_results_name_instances_in_model_form_but_keep_what_the_tick_reported() {
        let ids = IdContext::new();
        let run = HostedRunId::new();
        let stored = InstanceRef::hosted(run).as_str().to_owned();
        let result = model_tick_result(
            &ids,
            json!({"tick_instance_id":stored,"replaces":stored,"data":{"tick_instance_id":stored},"ticks":[{"tick_instance_id":stored}]}),
        );
        let model = ids.model_text(run);
        assert_eq!(result["tick_instance_id"], model);
        assert_eq!(result["replaces"], model);
        assert_eq!(result["ticks"][0]["tick_instance_id"], model);
        assert_eq!(result["data"]["tick_instance_id"], stored);
        assert_eq!(
            model_tick_text(
                &ids,
                &format!("{stored} is not a running hosted Tick; hosted:x stays")
            ),
            format!("{model} is not a running hosted Tick; hosted:x stays")
        );
    }

    fn validate(input: &Value) -> Result<(), String> {
        validate_input(TICK_MANAGE, &tick_manage_schema(), input)
    }

    fn refusal(input: &Value) -> String {
        validate(input).expect_err(&input.to_string())
    }

    #[test]
    fn the_tick_manage_description_names_where_bare_programs_are_found() {
        let description = tick_manage_description();
        assert!(
            description.contains("command[0]='tick-<package>'"),
            "{description}"
        );
        for text in [description, tick_manage_schema().to_string()] {
            let names_tick = text
                .split(|character: char| {
                    !(character.is_alphanumeric() || character == '-' || character == '_')
                })
                .any(|word| word == "tick");
            assert!(!names_tick, "{text}");
        }
    }

    #[test]
    fn tick_manage_schema_is_one_flat_object_that_describes_each_field() {
        let schema = tick_manage_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["action"]));
        assert_eq!(schema["additionalProperties"], false);
        assert!(!schema.to_string().contains("Of\""), "{schema}");
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(["open", "send_input", "close", "reopen"])
        );
        for (field, property) in schema["properties"].as_object().unwrap() {
            assert!(property["description"].is_string(), "{field}");
        }
        let described = |property: &Value, usage: &str| {
            assert!(
                property["description"].as_str().unwrap().starts_with(usage),
                "{property}"
            );
        };
        described(&schema["properties"]["command"], "Required by open.");
        described(&schema["properties"]["text"], "Optional for send_input.");
        for retired in ["placement", "workspace_id"] {
            assert!(schema["properties"].get(retired).is_none(), "{retired}");
        }
    }

    /// No definition offers a way to show a Tick in a Herdr Pane: Ticks are seen in the Monitor.
    #[test]
    fn no_tick_tool_places_lists_or_moves_panes() {
        let definitions = definitions();
        assert_eq!(
            definitions
                .iter()
                .map(|definition| definition.name.as_str())
                .collect::<Vec<_>>(),
            [TICK_DESCRIBE, TICK_READ, TICK_CALL, TICK_MANAGE]
        );
        for definition in &definitions {
            let text = format!("{} {}", definition.description, definition.input_schema);
            for retired in ["Herdr", "Pane", "placement", "ReadWorkspace", "move"] {
                assert!(!text.contains(retired), "{}: {retired}", definition.name);
            }
        }
    }

    #[test]
    fn tick_manage_input_refusals_name_what_to_change() {
        let empty = refusal(&json!({}));
        assert!(
            empty.starts_with("invalid TickManage input: action is required: one of "),
            "{empty}"
        );
        for requirement in [
            "open (requires command)",
            "send_input (requires tick_instance_id)",
            "close (requires tick_instance_id)",
        ] {
            assert!(empty.contains(requirement), "{empty}");
        }
        for (input, reason) in [
            (json!({"action":"open"}), "open requires command"),
            (json!({"action":"close"}), "close requires tick_instance_id"),
            (
                json!({"action":"open","command":["tick"],"text":"x"}),
                "text is not allowed for open; open takes command, and optionally cwd, protocol",
            ),
            (
                json!({"action":"close","tick_instance_id":"hosted:1","keys":["Enter"]}),
                "keys is not allowed for close; close takes only tick_instance_id",
            ),
            (
                json!({"action":"launch"}),
                "action must be one of open, send_input, close, reopen",
            ),
            (
                json!({"action":"open","command":[]}),
                "input.command has too few items",
            ),
        ] {
            assert_eq!(
                refusal(&input),
                format!("invalid TickManage input: {reason}")
            );
        }
    }

    /// A call an earlier build offered, repeated from an old conversation, is refused like any
    /// other field or action the tool does not take, before any host is asked, and the refusal
    /// says what to send instead. Nothing of it is silently dropped or run.
    #[test]
    fn retired_placement_fields_and_move_are_refused_with_what_to_send_instead() {
        for (input, reason) in [
            (
                json!({"action":"open","command":["tick"],"placement":{"type":"hidden"}}),
                "placement is not allowed for open; open takes command, and optionally cwd, protocol",
            ),
            (
                json!({"action":"open","command":["tick"],"placement":{"type":"current_tab","direction":"left"}}),
                "placement is not allowed for open; open takes command, and optionally cwd, protocol",
            ),
            (
                json!({"action":"open","command":["tick"],"workspace_id":"w1"}),
                "workspace_id is not allowed for open; open takes command, and optionally cwd, protocol",
            ),
            (
                json!({"action":"move","tick_instance_id":"hosted:1","placement":{"type":"new_tab"}}),
                "action must be one of open, send_input, close, reopen",
            ),
        ] {
            assert_eq!(
                refusal(&input),
                format!("invalid TickManage input: {reason}")
            );
        }
        let moved = check_call(
            TICK_MANAGE,
            &json!({"action":"move","tick_instance_id":"hosted:1"}),
        )
        .unwrap_err();
        assert!(
            moved.contains("must be open, send_input, close or reopen"),
            "{moved}"
        );
        assert!(
            check_call("ReadWorkspace", &json!({}))
                .unwrap_err()
                .contains("unknown Tick tool")
        );
    }

    #[test]
    fn every_modify_form_is_accepted() {
        for input in [
            json!({"action":"open","command":["tick"],"cwd":"/tmp","protocol":true}),
            json!({"action":"send_input","tick_instance_id":"hosted:1","text":"q","keys":["Enter"]}),
            json!({"action":"close","tick_instance_id":"hosted:1"}),
            json!({"action":"reopen","tick_instance_id":"hosted:1"}),
        ] {
            assert_eq!(validate(&input), Ok(()), "{input}");
        }
    }

    #[test]
    fn only_hosted_targets_are_served() {
        for (name, input) in [
            (TICK_READ, json!({"tick_instance_id":"herdr:w1/p1"})),
            (TICK_CALL, json!({"tick_instance_id":"calendar:1"})),
            (
                TICK_MANAGE,
                json!({"action":"close","tick_instance_id":"terminal:1"}),
            ),
        ] {
            let refused = check_call(name, &input).unwrap_err();
            assert!(refused.starts_with("InvalidTickTarget"), "{refused}");
        }
        assert!(
            check_call(TICK_READ, &json!({"tick_instance_id":"shell:1"}))
                .unwrap_err()
                .starts_with("NativeToolRequired")
        );
        assert!(check_call(TICK_MANAGE, &json!({"action":"open","command":["tick"]})).is_ok());
    }
}
