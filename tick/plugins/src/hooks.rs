//! Which of a plugin's Claude Code hooks Tickloop runs: `command` hooks of the events it supports,
//! without the `command` fields Claude Code added after them. Other events, types and fields are
//! read only to be named in warnings; the host reads the hooks it runs on its own.

use std::{collections::BTreeSet, fs, path::Path};

use serde_json::Value;

use crate::{manifest::PluginHooks, printable};

/// The Claude Code hook events Tickloop runs.
pub const HOOK_EVENTS: [&str; 10] = [
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "UserPromptSubmit",
    "SessionStart",
    "SessionEnd",
    "Stop",
    "StopFailure",
    "SubagentStop",
    "PreCompact",
];
/// The only hook type Tickloop runs.
pub const COMMAND_HOOK_TYPE: &str = "command";
/// `command` fields newer than Tickloop's support; a hook using one is skipped rather than run
/// without it.
pub const UNSUPPORTED_COMMAND_FIELDS: [&str; 6] =
    ["args", "shell", "async", "asyncRewake", "once", "onFailure"];
const HOOKS_KEY: &str = "hooks";
const TYPE_FIELD: &str = "type";

/// What a plugin's hook declarations use, split into what Tickloop runs and what it does not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookSupport {
    /// How many declared hooks Tickloop can run, each once the user confirms it.
    pub runnable: usize,
    /// The events, types and fields of the hooks Tickloop can run.
    pub events: BTreeSet<String>,
    pub types: BTreeSet<String>,
    pub fields: BTreeSet<String>,
    /// The events, types and fields whose hooks Tickloop skips.
    pub unsupported_events: BTreeSet<String>,
    pub unsupported_types: BTreeSet<String>,
    pub unsupported_fields: BTreeSet<String>,
}

impl HookSupport {
    pub fn has_unsupported(&self) -> bool {
        !(self.unsupported_events.is_empty()
            && self.unsupported_types.is_empty()
            && self.unsupported_fields.is_empty())
    }

    /// Whether the plugin declares hooks yet Tickloop can run none of them.
    pub fn is_unsupported(&self) -> bool {
        self.runnable == 0 && self.has_unsupported()
    }

    fn read_events(&mut self, events: &Value) -> Result<(), ()> {
        let events = events.as_object().ok_or(())?;
        let mut follows_format = true;
        for (event, groups) in events {
            if !HOOK_EVENTS.contains(&event.as_str()) {
                self.unsupported_events.insert(event.clone());
                continue;
            }
            let Some(groups) = groups.as_array() else {
                follows_format = false;
                continue;
            };
            for group in groups {
                follows_format &= self.read_group(event, group).is_ok();
            }
        }
        if follows_format { Ok(()) } else { Err(()) }
    }

    fn read_group(&mut self, event: &str, group: &Value) -> Result<(), ()> {
        if group
            .get("matcher")
            .is_some_and(|matcher| !matcher.is_null() && !matcher.is_string())
        {
            return Err(());
        }
        let handlers = group.get(HOOKS_KEY).and_then(Value::as_array).ok_or(())?;
        let mut follows_format = true;
        for handler in handlers {
            follows_format &= self.read_handler(event, handler).is_ok();
        }
        if follows_format { Ok(()) } else { Err(()) }
    }

    fn read_handler(&mut self, event: &str, handler: &Value) -> Result<(), ()> {
        let handler = handler.as_object().ok_or(())?;
        match handler.get(TYPE_FIELD).and_then(Value::as_str).ok_or(())? {
            COMMAND_HOOK_TYPE => {}
            other => {
                self.unsupported_types.insert(other.to_owned());
                return Ok(());
            }
        }
        let newer_fields = UNSUPPORTED_COMMAND_FIELDS
            .into_iter()
            .filter(|field| handler.contains_key(*field))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !newer_fields.is_empty() {
            self.unsupported_fields.extend(newer_fields);
            return Ok(());
        }
        let has_command = handler
            .get(COMMAND_HOOK_TYPE)
            .and_then(Value::as_str)
            .is_some_and(|command| !command.trim().is_empty());
        let has_valid_timeout = match handler.get("timeout") {
            None | Some(Value::Null) => true,
            Some(timeout) => timeout.as_f64().is_some_and(|seconds| seconds > 0.0),
        };
        if !(has_command && has_valid_timeout) {
            return Err(());
        }
        self.runnable += 1;
        self.events.insert(event.to_owned());
        self.types.insert(COMMAND_HOOK_TYPE.to_owned());
        self.fields
            .extend(handler.keys().filter(|field| *field != TYPE_FIELD).cloned());
        Ok(())
    }

    fn warnings(&self) -> Vec<String> {
        [
            (
                &self.unsupported_events,
                "hook events not supported yet, so their hooks are not used",
            ),
            (
                &self.unsupported_types,
                "hook types not supported yet, so those hooks are not used",
            ),
            (
                &self.unsupported_fields,
                "hook fields not supported yet, so the hooks using them are not used",
            ),
        ]
        .into_iter()
        .filter(|(names, _)| !names.is_empty())
        .map(|(names, lead)| {
            let names = names.iter().map(printable).collect::<Vec<_>>();
            format!("{lead}: {}", names.join(", "))
        })
        .collect()
    }
}

/// What the plugin in `root` declares in `declarations`, with a warning for each declaration that
/// cannot be read and for what Tickloop does not run. `manifest` names where inline hooks come
/// from.
pub(crate) fn hook_support(
    root: &Path,
    declarations: &[PluginHooks],
    manifest: &str,
    warnings: &mut Vec<String>,
) -> HookSupport {
    let mut support = HookSupport::default();
    for declaration in declarations {
        let (events, location) = match declaration {
            PluginHooks::Inline(events) => (Some(events.clone()), manifest.to_owned()),
            PluginHooks::File(relative) => {
                let location = printable(relative);
                match read_hooks_file(&root.join(relative)) {
                    Ok(events) => (events, location),
                    Err(reason) => {
                        warnings.push(format!("hooks file {location} is ignored: {reason}"));
                        continue;
                    }
                }
            }
        };
        if let Some(events) = events
            && support.read_events(&events).is_err()
        {
            warnings.push(format!(
                "hooks in {location} that do not follow the Claude Code hooks format are ignored"
            ));
        }
    }
    warnings.extend(support.warnings());
    support
}

/// The `hooks` object of a hooks file, or `None` when it has none.
fn read_hooks_file(path: &Path) -> Result<Option<Value>, String> {
    let bytes = fs::read(path).map_err(|error| format!("cannot read it: {error}"))?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(mut document)) => Ok(document.remove(HOOKS_KEY)),
        Ok(_) => Err("it must hold a JSON object".to_owned()),
        Err(error) => Err(format!("it is not valid JSON: {error}")),
    }
}
