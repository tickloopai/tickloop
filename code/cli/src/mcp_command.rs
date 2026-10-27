//! `tickloop mcp`: the user's own MCP servers in settings.json, and logins to remote servers.
//! Listing reads the configuration only; it never starts or connects to a server.

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use serde_json::{Map, Value, json};
use tickloop_code_console::terminal::safe_line;
use tickloop_code_settings::{
    UserSettings,
    mcp::{
        DIRECTORY_SOURCE, McpLayer, PLUGIN_SERVER_PREFIX, PLUGIN_SOURCE_PREFIX, configured_servers,
        configured_statuses, validate_entry, validate_user_server_name,
    },
};
use tickloop_harness::protocol::mcp::{McpServerState, McpServerStatus, ToolExposure};

const USAGE: &str = "Usage: tickloop mcp list\n       tickloop mcp add <name> <url> [--header 'Name: value']... [--exposure direct|search|hidden] [--dir]\n       tickloop mcp add <name> [--env NAME=value]... [--exposure direct|search|hidden] [--dir] -- <command> [arguments...]\n       tickloop mcp remove <name> [--dir]\n       tickloop mcp login <name> | logout <name>";
const DIRECTORY_FLAG: &str = "--dir";
const EXPOSURE_FLAG: &str = "--exposure";

pub(super) async fn run(arguments: &[OsString]) -> Result<(), String> {
    let words = arguments
        .iter()
        .map(|argument| argument.to_str().ok_or("MCP arguments must be valid UTF-8"))
        .collect::<Result<Vec<_>, _>>()?;
    match words.split_first() {
        Some((&"list", [])) => list().await,
        Some((&"add", rest)) => add(rest),
        Some((&"remove", rest)) => remove(rest),
        Some((&("login" | "logout"), _)) => super::mcp_login::run(arguments).await,
        _ => Err(USAGE.into()),
    }
}

fn config_root() -> Result<PathBuf, String> {
    tickloop_code_settings::config_root_directory()
        .ok_or_else(|| "cannot find the Tickloop config directory".to_owned())
}

/// The directory an Agent started here would work in, as its settings name it.
pub(super) fn working_directory() -> Result<PathBuf, String> {
    std::env::current_dir()
        .and_then(|directory| directory.canonicalize())
        .map_err(|error| format!("cannot read the current directory: {error}"))
}

fn describe(layer: &McpLayer) -> &'static str {
    match layer {
        McpLayer::Global => "the global settings",
        McpLayer::Directory(_) => "this directory's settings",
    }
}

fn layer(directory: bool) -> Result<McpLayer, String> {
    Ok(if directory {
        McpLayer::Directory(working_directory()?)
    } else {
        McpLayer::Global
    })
}

fn refuse_plugin_server(name: &str) -> Result<(), String> {
    if name.starts_with(PLUGIN_SOURCE_PREFIX) || name.starts_with(PLUGIN_SERVER_PREFIX) {
        return Err(format!(
            "MCP server {} comes from its plugin; turn it off in /mcp or disable the plugin with tickloop plugin disable.",
            safe_line(name)
        ));
    }
    Ok(())
}

async fn list() -> Result<(), String> {
    let root = config_root()?;
    let resolved = configured_servers(&root, &working_directory()?, &std::env::vars().collect());
    for warning in &resolved.warnings {
        eprintln!("{}", safe_line(warning));
    }
    let statuses = configured_statuses(&root, &resolved.servers).await;
    if statuses.is_empty() {
        println!("No MCP servers are configured; add one with tickloop mcp add.");
        return Ok(());
    }
    let mut groups = BTreeMap::<(u8, String), Vec<&McpServerStatus>>::new();
    for status in &statuses {
        let key = match status.source.strip_prefix(PLUGIN_SOURCE_PREFIX) {
            Some(plugin) => (2, format!("Plugin {plugin}")),
            None if status.source == DIRECTORY_SOURCE => (1, "This directory".into()),
            None => (0, "Global".into()),
        };
        groups.entry(key).or_default().push(status);
    }
    for ((_, heading), statuses) in groups {
        println!("{}", safe_line(&heading));
        for status in statuses {
            println!("  {}", list_line(status));
        }
    }
    Ok(())
}

fn list_line(status: &McpServerStatus) -> String {
    let mut parts = vec![status.name.clone()];
    if !status.transport.is_empty() {
        parts.push(status.transport.clone());
    }
    parts.push(status.exposure.clone());
    parts.push(match status.state {
        McpServerState::Disabled => "disabled in this directory".into(),
        McpServerState::NeedsConfiguration => "needs configuration".into(),
        McpServerState::Unsupported => "not supported yet".into(),
        McpServerState::Failed => format!(
            "cannot start: {}",
            status.failure.as_deref().unwrap_or("invalid configuration")
        ),
        _ => "enabled".into(),
    });
    // Whether a server without a saved login needs one shows only once it answers.
    if status.logged_in {
        parts.push("logged in".into());
    }
    safe_line(&parts.join(" · "))
}

fn add(arguments: &[&str]) -> Result<(), String> {
    let (options, command) = match arguments.iter().position(|word| *word == "--") {
        Some(separator) => (&arguments[..separator], Some(&arguments[separator + 1..])),
        None => (arguments, None),
    };
    let mut positional = Vec::new();
    let mut headers = Map::new();
    let mut environment = Map::new();
    let mut directory = false;
    let mut exposure = None;
    let mut options = options.iter();
    while let Some(option) = options.next() {
        match *option {
            DIRECTORY_FLAG => directory = true,
            "--header" => {
                let header = options.next().ok_or("--header needs 'Name: value'")?;
                let (name, value) = header
                    .split_once(':')
                    .ok_or("--header needs 'Name: value'")?;
                headers.insert(name.trim().to_owned(), Value::from(value.trim()));
            }
            "--env" => {
                let variable = options.next().ok_or("--env needs NAME=value")?;
                let (name, value) = variable.split_once('=').ok_or("--env needs NAME=value")?;
                environment.insert(name.to_owned(), Value::from(value));
            }
            EXPOSURE_FLAG => {
                if exposure.is_some() {
                    return Err(format!("{EXPOSURE_FLAG} is given more than once\n{USAGE}"));
                }
                exposure = Some(parse_exposure(options.next().copied())?);
            }
            word if word.starts_with('-') => {
                return Err(format!("unknown option {}\n{USAGE}", safe_line(word)));
            }
            word => positional.push(word),
        }
    }
    let mut entry = match (positional.as_slice(), command) {
        ([_name, url], None) => {
            if !environment.is_empty() {
                return Err("--env applies to a command; a remote server takes --header".into());
            }
            let mut entry = json!({"type": "http", "url": url});
            if !headers.is_empty() {
                entry["headers"] = Value::Object(headers);
            }
            entry
        }
        ([_name], Some([program, program_arguments @ ..])) => {
            if !headers.is_empty() {
                return Err("--header applies to a remote server; a command takes --env".into());
            }
            let mut entry = json!({"type": "stdio", "command": program});
            if !program_arguments.is_empty() {
                entry["args"] = json!(program_arguments);
            }
            if !environment.is_empty() {
                entry["env"] = Value::Object(environment);
            }
            entry
        }
        ([_name, _url], Some(_)) => {
            return Err("give either a URL or a command after --, not both".into());
        }
        _ => return Err(USAGE.into()),
    };
    if let Some(exposure) = exposure {
        entry["exposure"] = Value::from(exposure.as_str());
    }
    let name = positional[0];
    validate_user_server_name(name)?;
    let layer = layer(directory)?;
    validate_entry(&entry, &working_directory()?)?;
    let settings = UserSettings::in_root(&config_root()?);
    let added = settings
        .add_mcp_server(&layer, name, entry)
        .map_err(|error| error.to_string())?;
    if !added {
        return Err(format!(
            "MCP server {} already exists in {}; remove it first.",
            safe_line(name),
            describe(&layer)
        ));
    }
    println!(
        "Added MCP server {} to {}.",
        safe_line(name),
        match layer {
            McpLayer::Global => "the global settings",
            McpLayer::Directory(_) => "this directory",
        }
    );
    if matches!(layer, McpLayer::Directory(_))
        && settings.load().has_mcp_server(&McpLayer::Global, name)
    {
        println!(
            "Here it takes the place of the global server {}.",
            safe_line(name)
        );
    }
    Ok(())
}

fn parse_exposure(value: Option<&str>) -> Result<ToolExposure, String> {
    value
        .and_then(|value| serde_json::from_value(Value::from(value)).ok())
        .ok_or_else(|| format!("{EXPOSURE_FLAG} needs direct, search or hidden\n{USAGE}"))
}

fn remove(arguments: &[&str]) -> Result<(), String> {
    let (name, directory) = match arguments {
        [name] => (*name, false),
        [name, DIRECTORY_FLAG] | [DIRECTORY_FLAG, name] => (*name, true),
        _ => return Err(USAGE.into()),
    };
    refuse_plugin_server(name)?;
    let layer = layer(directory)?;
    let settings = UserSettings::in_root(&config_root()?);
    if settings
        .remove_mcp_server(&layer, name)
        .map_err(|error| error.to_string())?
    {
        println!(
            "Removed MCP server {} from {}.",
            safe_line(name),
            describe(&layer)
        );
        return Ok(());
    }
    let other = match layer {
        McpLayer::Global => McpLayer::Directory(working_directory()?),
        McpLayer::Directory(_) => McpLayer::Global,
    };
    Err(if settings.load().has_mcp_server(&other, name) {
        format!(
            "MCP server {} is not in {}; it is in {}{}.",
            safe_line(name),
            describe(&layer),
            describe(&other),
            if directory {
                "; remove it without --dir"
            } else {
                "; remove it with --dir"
            }
        )
    } else {
        format!(
            "MCP server {} is not configured in {}.",
            safe_line(name),
            describe(&layer)
        )
    })
}
