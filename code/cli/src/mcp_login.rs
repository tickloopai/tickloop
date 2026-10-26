//! User-initiated remote MCP login. Workers only consume the saved credentials.
use std::{
    ffi::OsString,
    io::{self, BufRead, IsTerminal, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tickloop_code_console::terminal::safe_line;
use tickloop_code_settings::{UserSettings, mcp::PluginServers, mcp_credentials::FileCredentials};
use tickloop_harness::{
    connector::{Connector, Logout, OAuthSettings},
    protocol::mcp::{McpServerConfig, McpTransport},
};
use tokio_util::sync::CancellationToken;

pub(super) async fn run(arguments: &[OsString]) -> Result<(), String> {
    let arguments = arguments
        .iter()
        .map(|arg| arg.to_str().ok_or("MCP arguments must be valid UTF-8"))
        .collect::<Result<Vec<_>, _>>()?;
    let [command @ ("login" | "logout"), name] = arguments.as_slice() else {
        return Err("Usage: tickloop mcp login <name> | logout <name>".into());
    };
    let root = tickloop_code_settings::config_root_directory()
        .ok_or("cannot find the Tickloop config directory")?;
    let server = resolve(&root, name)?;
    let McpTransport::Http { url, headers } = &server.transport else {
        return Err("MCP login requires a configured streamable HTTP server".into());
    };
    if headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
    {
        return Err("This MCP server uses an Authorization header; manage that credential in its configuration instead of OAuth login or logout.".into());
    }
    let credentials = Arc::new(
        FileCredentials::new(&root, &server.source, &server.name)
            .map_err(|error| error.to_string())?,
    );
    let connector = Connector::new(
        url,
        OAuthSettings {
            client_id: server.oauth.client_id,
            callback_port: server.oauth.callback_port,
            authorization_server_metadata_url: server.oauth.authorization_server_metadata_url,
        },
        credentials,
    )
    .map_err(|error| error.to_string())?;
    let cancel = CancellationToken::new();
    let operation = async {
        if *command == "login" {
            login(&connector, &cancel).await?;
            println!(
                "Logged in to MCP {}. Available from the next Agent request.",
                safe_line(name)
            );
        } else {
            let outcome = connector
                .logout(&cancel)
                .await
                .map_err(|error| error.to_string())?;
            println!("Logged out of MCP {}.", safe_line(name));
            if outcome == Logout::RevocationFailed {
                eprintln!("The server could not revoke the token; the local token was deleted.");
            }
        }
        Ok(())
    };
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => result,
        _ = tokio::signal::ctrl_c() => { cancel.cancel(); operation.await }
    }
}

fn resolve(root: &Path, name: &str) -> Result<McpServerConfig, String> {
    let name = if let Some(plugin) = name.strip_prefix("plugin:") {
        let (plugin, server) = plugin
            .split_once(':')
            .ok_or("use plugin:<plugin>:<server>")?;
        format!("plugin_{plugin}_{server}")
    } else {
        name.to_owned()
    };
    let store = tick_plugins::PluginStore::in_config_root(root);
    let plugins = store
        .installed()
        .map_err(|error| error.to_string())?
        .plugins
        .into_iter()
        .filter(|plugin| plugin.enabled)
        .map(|plugin| PluginServers {
            data_directory: store.data_directory(plugin.name()),
            name: plugin.name().to_owned(),
            root: plugin.root,
            entries: plugin.manifest.mcp_servers,
        })
        .collect::<Vec<_>>();
    let settings = UserSettings::in_root(root).load();
    let resolved = settings.mcp_servers_with_plugins(
        &std::env::current_dir().map_err(|error| error.to_string())?,
        &std::env::vars().collect(),
        &plugins,
    );
    resolved
        .servers
        .into_iter()
        .find(|server| server.name == name)
        .ok_or_else(|| {
            format!(
                "MCP server {} is not configured or enabled in this directory",
                safe_line(&name)
            )
        })
}

async fn login(connector: &Connector, cancel: &CancellationToken) -> Result<(), String> {
    let pending = connector
        .start_login(None, cancel)
        .await
        .map_err(|error| error.to_string())?;
    println!("Open this link in your browser to log in:");
    println!("{}", pending.authorization_url());
    println!(
        "Waiting for the browser callback. On another machine, paste the full callback address here. Ctrl+C cancels."
    );
    io::stdout().flush().map_err(|error| error.to_string())?;
    let paste = pending.paste();
    let input_cancel = cancel.child_token();
    let _stop_input = input_cancel.clone().drop_guard();
    let input_finished = input_cancel.clone();
    let terminal = io::stdin().is_terminal();
    let _terminal_mode = if terminal {
        Some(RawInput::start()?)
    } else {
        None
    };
    if terminal {
        print!("Callback input ready (hidden; Ctrl+G editor; Esc cancels).\r\n");
        io::stdout().flush().map_err(|error| error.to_string())?;
    }
    let (sender, mut lines) = tokio::sync::mpsc::unbounded_channel();
    let input_thread = std::thread::spawn(move || {
        if terminal {
            let mut line = String::new();
            while !input_cancel.is_cancelled() {
                use crossterm::event::{self, Event, KeyCode, KeyModifiers};
                if !event::poll(Duration::from_millis(50)).unwrap_or(false) {
                    continue;
                }
                let Ok(Event::Key(key)) = event::read() else {
                    continue;
                };
                if key.kind == crossterm::event::KeyEventKind::Release {
                    continue;
                }
                match key.code {
                    KeyCode::Esc => {
                        input_cancel.cancel();
                        break;
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input_cancel.cancel();
                        break;
                    }
                    KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let edited = crossterm::terminal::disable_raw_mode()
                            .map_err(|error| error.to_string())
                            .and_then(|()| {
                                tickloop_code_console::terminal::edit_external_text(
                                    &line,
                                    Path::new("."),
                                    &input_cancel,
                                )
                            });
                        if crossterm::terminal::enable_raw_mode().is_err() {
                            input_cancel.cancel();
                            break;
                        }
                        match edited {
                            Ok(Some(content)) if content.len() <= MAX_CALLBACK_BYTES => {
                                line = content;
                                print!("Callback updated; press Enter to submit.\r\n");
                            }
                            Ok(Some(_)) => print!("Callback address is too long.\r\n"),
                            Ok(None) => {}
                            Err(error) => print!("{}\r\n", safe_line(&error)),
                        }
                        let _ = io::stdout().flush();
                    }
                    KeyCode::Enter => {
                        if sender.send(std::mem::take(&mut line)).is_err() {
                            break;
                        }
                    }
                    KeyCode::Backspace => {
                        line.pop();
                    }
                    KeyCode::Char(character)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            && line.len() < MAX_CALLBACK_BYTES =>
                    {
                        line.push(character)
                    }
                    _ => {}
                }
            }
        } else {
            let mut input = io::stdin().lock();
            loop {
                let mut line = Vec::new();
                let read = std::io::Read::take(&mut input, MAX_CALLBACK_BYTES as u64 + 1)
                    .read_until(b'\n', &mut line);
                if !matches!(read, Ok(1..))
                    || line.len() > MAX_CALLBACK_BYTES
                    || input_cancel.is_cancelled()
                {
                    break;
                }
                let Ok(line) = String::from_utf8(line) else {
                    continue;
                };
                if sender.send(line).is_err() {
                    break;
                }
            }
        }
    });
    let finish = pending.finish(cancel);
    tokio::pin!(finish);
    let result = loop {
        tokio::select! {
            result = &mut finish => break result.map_err(|error| error.to_string()),
            _ = input_finished.cancelled() => { cancel.cancel(); break finish.await.map_err(|error| error.to_string()); }
            Some(line) = lines.recv() => {
                if let Err(error) = paste.submit(line.trim()) {
                    eprintln!("{error}");
                }
            }
        }
    };
    input_finished.cancel();
    if terminal {
        let _ = input_thread.join();
    }
    // A pipe reader may still wait for its writer; it must not hold up runtime shutdown.
    result
}

const MAX_CALLBACK_BYTES: usize = 16 * 1024;
struct RawInput;
impl RawInput {
    fn start() -> Result<Self, String> {
        crossterm::terminal::enable_raw_mode().map_err(|error| error.to_string())?;
        Ok(Self)
    }
}
impl Drop for RawInput {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
