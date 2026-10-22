//! ACP configuration is a projection of the managed execution's authoritative configuration.
//! No settings or permission state are written directly by this adapter.

use std::collections::HashSet;

use agent_client_protocol::Error;
use serde_json::{Value, json};
use tickloop_agent_client::managed::ManagedSession;
use tickloop_agent_protocol::{
    ExecutionMode, ManagedConfiguration, ManagedConfigurationChange, UserInput,
};

use super::{access_error, invalid};

pub(super) async fn response(managed: &ManagedSession) -> Result<Value, Error> {
    project(&managed.configuration().await.map_err(access_error)?)
}

/// Returns the complete updated `configOptions`, not just the option changed.
pub(super) async fn configure(
    managed: &ManagedSession,
    config_id: &str,
    value: Value,
) -> Result<Value, Error> {
    let current = managed.configuration().await.map_err(access_error)?;
    let change = change(&current, config_id, &value)?;
    let updated = managed.configure(change).await.map_err(access_error)?;
    let projected = project(&updated)?;
    Ok(json!({"configOptions": projected["configOptions"]}))
}

/// Returns the same full projection as `response`, for the caller to notify both mode/config
/// changes. The ACP `session/set_mode` acknowledgement itself should be an empty object.
pub(super) async fn set_mode(managed: &ManagedSession, mode_id: &str) -> Result<Value, Error> {
    let current = managed.configuration().await.map_err(access_error)?;
    let change = change(&current, "mode", &Value::String(mode_id.to_owned()))?;
    project(&managed.configure(change).await.map_err(access_error)?)
}

fn mode_id(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Auto => "auto",
        ExecutionMode::AcceptEdits => "ask",
        ExecutionMode::FullAccess => "full-access",
        ExecutionMode::Plan => "plan",
    }
}

fn supported_modes(config: &ManagedConfiguration) -> Vec<ExecutionMode> {
    let mut modes = Vec::new();
    for mode in &config.modes {
        // Managed ACP does not support the Plan approval workflow.
        if matches!(mode, ExecutionMode::Auto | ExecutionMode::AcceptEdits) && !modes.contains(mode)
        {
            modes.push(*mode);
        }
    }
    modes
}

fn project(config: &ManagedConfiguration) -> Result<Value, Error> {
    let modes = supported_modes(config);
    if !modes.contains(&config.mode) {
        return Err(Error::new(
            -32603,
            "managed configuration has an unsupported current mode",
        ));
    }
    let mut seen = HashSet::new();
    let models: Vec<Value> = config
        .models
        .iter()
        .filter(|model| !model.id.is_empty() && seen.insert(model.id.as_str()))
        .map(|model| json!({"value": model.id, "name": model.name}))
        .collect();
    if !models
        .iter()
        .any(|model| model["value"].as_str() == Some(config.model_id.as_str()))
    {
        return Err(Error::new(
            -32603,
            "managed configuration does not offer its current model",
        ));
    }
    let options: Vec<Value> = modes
        .iter()
        .map(|mode| json!({"value": mode_id(*mode), "name": mode.label()}))
        .collect();
    Ok(json!({
        "configOptions": [
            {"id": "model", "name": "Model", "category": "model", "type": "select",
             "currentValue": config.model_id, "options": models},
            {"id": "mode", "name": "Execution Mode", "category": "mode", "type": "select",
             "currentValue": mode_id(config.mode), "options": options}
        ],
        "modes": {
            "currentModeId": mode_id(config.mode),
            "availableModes": modes.iter().map(|mode| json!({
                "id": mode_id(*mode), "name": mode.label()
            })).collect::<Vec<_>>()
        }
    }))
}

fn change(
    config: &ManagedConfiguration,
    config_id: &str,
    value: &Value,
) -> Result<ManagedConfigurationChange, Error> {
    let value = value
        .as_str()
        .ok_or_else(|| invalid("configuration value must be a string"))?;
    match config_id {
        "model"
            if config
                .models
                .iter()
                .any(|model| model.id == value && !value.is_empty()) =>
        {
            Ok(ManagedConfigurationChange {
                model_id: Some(value.to_owned()),
                mode: None,
            })
        }
        "model" => Err(invalid("model was not offered by this session")),
        "mode" => {
            let mode = supported_modes(config)
                .into_iter()
                .find(|mode| mode_id(*mode) == value)
                .ok_or_else(|| invalid("mode was not offered by this session"))?;
            Ok(ManagedConfigurationChange {
                model_id: None,
                mode: Some(mode),
            })
        }
        _ => Err(invalid("unknown configuration option")),
    }
}

/// Publish only the authoritative registry's user-invocable commands.
pub(super) async fn available_commands(managed: &ManagedSession) -> Result<Vec<Value>, Error> {
    let skills = managed.skills().await.map_err(access_error)?;
    let mut counts = std::collections::HashMap::new();
    for skill in &skills {
        *counts.entry(skill.name.as_str()).or_insert(0usize) += 1;
    }
    Ok(skills
        .iter()
        .filter(|skill| command_name_valid(&skill.name) && counts[skill.name.as_str()] == 1)
        .map(|skill| json!({"name":skill.name,"description":skill.description}))
        .collect())
}

// Console/editor control names are never forwarded to Skills, even if a malformed catalogue
// supplies one. There is deliberately no emulation of Console slash commands in ACP.
fn command_name_valid(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "help",
        "clear",
        "new",
        "exit",
        "quit",
        "cancel",
        "model",
        "mode",
        "plan",
        "provider",
        "login",
        "logout",
        "permissions",
        "settings",
        "resume",
        "compact",
        "status",
        "init",
        "undo",
        "redo",
        "mcp",
        "commands",
        "sessions",
        "load",
    ];
    !name.is_empty()
        && name.len() <= 128
        && !RESERVED
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':'))
        && name.as_bytes()[0].is_ascii_alphanumeric()
}

/// Recognizes only an exact, explicitly advertised leading slash command. Unknown slash text
/// stays ordinary text. Skill inputs cannot carry attachments, so reject rather than drop them.
pub(super) fn route_skill(input: UserInput, available: &[Value]) -> Result<UserInput, Error> {
    let UserInput::Text {
        text,
        contexts,
        paths,
    } = input
    else {
        return Ok(input);
    };
    let Some(command) = text.strip_prefix('/') else {
        return Ok(UserInput::Text {
            text,
            contexts,
            paths,
        });
    };
    let end = command.find(char::is_whitespace).unwrap_or(command.len());
    let name = &command[..end];
    let matches = available
        .iter()
        .filter(|entry| entry["name"].as_str() == Some(name))
        .count();
    if !command_name_valid(name) || matches != 1 {
        return Ok(UserInput::Text {
            text,
            contexts,
            paths,
        });
    }
    if !contexts.is_empty() || !paths.is_empty() {
        return Err(invalid(
            "skill invocation cannot carry context or file attachments",
        ));
    }
    Ok(UserInput::Skill {
        name: name.to_owned(),
        arguments: command[end..].trim_start().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        NewSessionResponse, SessionNotification, SetSessionConfigOptionResponse,
    };
    use tickloop_agent_protocol::ManagedModelInfo;

    fn config() -> ManagedConfiguration {
        ManagedConfiguration {
            model_id: "provider/model".into(),
            mode: ExecutionMode::Auto,
            models: vec![ManagedModelInfo {
                id: "provider/model".into(),
                name: "Model".into(),
            }],
            modes: vec![
                ExecutionMode::Auto,
                ExecutionMode::AcceptEdits,
                ExecutionMode::FullAccess,
                ExecutionMode::Plan,
            ],
        }
    }

    #[test]
    fn config_responses_and_notification_decode_with_official_sdk() {
        let projected = project(&config()).unwrap();
        let mut new = projected.clone();
        new["sessionId"] = json!("session");
        let decoded: NewSessionResponse = serde_json::from_value(new).unwrap();
        let wire = serde_json::to_value(decoded).unwrap();
        assert_eq!(wire["configOptions"][0]["type"], "select");
        assert_eq!(wire["modes"]["availableModes"].as_array().unwrap().len(), 2);
        let _: SetSessionConfigOptionResponse = serde_json::from_value(json!({
            "configOptions": projected["configOptions"]
        }))
        .unwrap();
        let _: SessionNotification = serde_json::from_value(json!({
            "sessionId": "session", "update": {
                "sessionUpdate": "config_option_update", "configOptions": projected["configOptions"]
            }
        }))
        .unwrap();
    }

    #[test]
    fn accepts_only_offered_values_and_never_plan() {
        let config = config();
        assert_eq!(
            change(&config, "mode", &json!("ask")).unwrap().mode,
            Some(ExecutionMode::AcceptEdits)
        );
        assert!(change(&config, "mode", &json!("plan")).is_err());
        assert!(change(&config, "model", &json!("missing")).is_err());
        assert!(change(&config, "unknown", &json!("auto")).is_err());
        assert!(change(&config, "mode", &json!(true)).is_err());
    }

    fn text(value: &str) -> UserInput {
        UserInput::Text {
            text: value.into(),
            contexts: Vec::new(),
            paths: Vec::new(),
        }
    }

    #[test]
    fn routes_exact_advertised_skill_and_preserves_argument_text() {
        let available = vec![json!({"name": "package:skill", "description": "Skill"})];
        let UserInput::Skill { name, arguments } =
            route_skill(text("/package:skill  first\nsecond"), &available).unwrap()
        else {
            panic!("expected Skill input");
        };
        assert_eq!(name, "package:skill");
        assert_eq!(arguments, "first\nsecond");
        for value in [
            " /package:skill",
            "mention /package:skill",
            "/package:skill-extra",
            "/unknown",
        ] {
            assert!(matches!(
                route_skill(text(value), &available).unwrap(),
                UserInput::Text { .. }
            ));
        }
    }

    #[test]
    fn refuses_lossy_attachments_and_ambiguous_or_reserved_commands() {
        let available = vec![json!({"name": "slides"})];
        let mut input = text("/slides");
        if let UserInput::Text { paths, .. } = &mut input {
            paths.push("file.txt".into());
        }
        assert!(route_skill(input, &available).is_err());
        let mut input = text("/slides");
        if let UserInput::Text { contexts, .. } = &mut input {
            contexts.push(tickloop_agent_protocol::InputContext::Resource(
                tickloop_agent_protocol::ResourceContext {
                    uri: "resource://test".into(),
                    mime_type: Some("text/plain".into()),
                    name: None,
                    content: Some("client context".into()),
                },
            ));
        }
        assert!(route_skill(input, &available).is_err());
        for name in ["help", "model", "bad/name", "bad name", "_invalid"] {
            assert!(matches!(
                route_skill(text(&format!("/{name}")), &[json!({"name": name})]).unwrap(),
                UserInput::Text { .. }
            ));
        }
        assert!(matches!(
            route_skill(
                text("/slides"),
                &[json!({"name": "slides"}), json!({"name": "slides"})]
            )
            .unwrap(),
            UserInput::Text { .. }
        ));
    }

    #[test]
    fn unknown_slash_preserves_paths() {
        let mut input = text("/unknown arguments");
        if let UserInput::Text { paths, .. } = &mut input {
            paths.push("file.txt".into());
        }
        let UserInput::Text { paths, .. } = route_skill(input, &[]).unwrap() else {
            panic!("expected Text input")
        };
        assert_eq!(paths, vec![std::path::PathBuf::from("file.txt")]);
    }
}
