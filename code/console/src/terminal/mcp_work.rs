//! Runs the work `/mcp` asks for. The Agent's Worker reports its servers and reconnects one;
//! turning a server off or on, and logins, happen here as in `tickloop mcp`, and the Worker takes
//! them up from its next request. A login runs in two tasks: one finds the authorization link the
//! panel shows, and one waits for the browser's callback or the address the user pastes.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
};

use tickloop_code_settings::{
    UserSettings,
    mcp::{check_authorization_link, configured_servers, configured_statuses, login_connector},
};
use tickloop_harness::{base::oauth_callback::CallbackPaste, connector::PendingLogin};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    console::Console,
    execution::ConsoleExecution,
    mcp_panel::{McpListing, McpPanel, McpRequest, McpResult, McpWork},
    safe_line,
    user_panel::UserPanel,
};

const PANEL_COMMAND: &str = "/mcp";

/// What an `/mcp` task ends with.
pub(super) enum Done {
    Result(McpResult),
    /// The authorization link of a login, which the panel shows while it waits for the callback.
    LoginStarted {
        attempt: u64,
        name: String,
        login: Box<PendingLogin>,
        cancel: CancellationToken,
    },
}

/// What a panel works on: its Chat, when the Agent runs, and the directory it works in.
#[derive(Clone)]
struct PanelContext {
    chat: Option<Uuid>,
    directory: PathBuf,
}

struct Login {
    attempt: u64,
    cancel: CancellationToken,
    paste: Option<CallbackPaste>,
}

/// The `/mcp` work under way. Reads are numbered as they start, so a panel keeps the most recent
/// one whatever order they finish in.
#[derive(Default)]
pub(super) struct McpTasks {
    running: JoinSet<(Uuid, Done)>,
    reads: u64,
    contexts: HashMap<Uuid, PanelContext>,
    logins: HashMap<Uuid, Login>,
    /// The panels with a read under way.
    reading: HashSet<Uuid>,
}

impl McpTasks {
    pub(super) fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    /// The next finished work and its panel; `None` for work that panicked.
    pub(super) async fn join_next(&mut self) -> Option<(Uuid, Done)> {
        self.running.join_next().await?.ok()
    }

    fn spawn(&mut self, work: impl Future<Output = (Uuid, McpResult)> + Send + 'static) {
        self.running.spawn(async move {
            let (panel, result) = work.await;
            (panel, Done::Result(result))
        });
    }

    /// Forgets the login `attempt` of `panel`, unless a later one has taken its place, and
    /// returns what cancels it.
    fn end_login(&mut self, panel: Uuid, attempt: u64) -> Option<CancellationToken> {
        if self.logins.get(&panel)?.attempt != attempt {
            return None;
        }
        self.logins.remove(&panel).map(|login| login.cancel)
    }

    fn read(&mut self, root: &Path, agent: &ConsoleExecution, panel: Uuid) {
        let Some(context) = self.contexts.get(&panel).cloned() else {
            return;
        };
        self.reads += 1;
        let read = self.reads;
        self.reading.insert(panel);
        let (root, agent) = (root.to_owned(), agent.clone());
        self.spawn(async move {
            let listing = read_servers(&root, &agent, context).await;
            (panel, McpResult::Servers { read, listing })
        });
    }
}

/// Opens `/mcp` for `chat`, or for an Agent not started yet in `directory`, and reads its servers.
pub(super) fn open(
    console: &Console,
    root: &Path,
    agent: &ConsoleExecution,
    chat: Option<Uuid>,
    directory: PathBuf,
    tasks: &mut McpTasks,
) -> Result<(), String> {
    let panel = McpPanel::new(chat);
    let id = panel.id();
    console.show_user_panel(UserPanel::Mcp(panel))?;
    tasks.contexts.insert(id, PanelContext { chat, directory });
    tasks.read(root, agent, id);
    Ok(())
}

/// Reads the servers of `panel` again while they connect, unless a read is under way.
pub(super) fn refresh(root: &Path, agent: &ConsoleExecution, panel: Uuid, tasks: &mut McpTasks) {
    if !tasks.reading.contains(&panel) {
        tasks.read(root, agent, panel);
    }
}

async fn read_servers(
    root: &Path,
    agent: &ConsoleExecution,
    context: PanelContext,
) -> Result<McpListing, String> {
    if let Some(chat) = context.chat {
        let servers = agent
            .mcp_servers(chat)
            .await
            .map_err(|error| error.to_string())?;
        let directory = agent
            .status(chat)
            .await
            .ok()
            .and_then(|status| status.folder)
            .unwrap_or(context.directory);
        return Ok(McpListing {
            servers,
            live: true,
            directory,
        });
    }
    let resolved = configured(root, &context.directory).await?;
    Ok(McpListing {
        servers: configured_statuses(root, &resolved.servers).await,
        live: false,
        directory: context.directory,
    })
}

/// The servers the configuration in `root` gives an Agent working in `directory`, with variables
/// from this View's environment as `tickloop mcp` takes them.
async fn configured(
    root: &Path,
    directory: &Path,
) -> Result<tickloop_code_settings::mcp::ResolvedServers, String> {
    let (root, directory) = (root.to_owned(), directory.to_owned());
    tokio::task::spawn_blocking(move || {
        configured_servers(&root, &directory, &std::env::vars().collect())
    })
    .await
    .map_err(|error| error.to_string())
}

async fn configured_server(
    root: &Path,
    directory: &Path,
    name: &str,
) -> Result<tickloop_harness::protocol::mcp::McpServerConfig, String> {
    configured(root, directory)
        .await?
        .servers
        .into_iter()
        .find(|server| server.name == name)
        .ok_or_else(|| format!("{name} is not configured or enabled in this directory"))
}

pub(super) fn start(
    console: &Console,
    root: &Path,
    agent: &ConsoleExecution,
    work: McpWork,
    tasks: &mut McpTasks,
) -> Result<(), String> {
    let panel = work.panel;
    let Some(context) = tasks.contexts.get(&panel).cloned() else {
        return Ok(());
    };
    let directory = context.directory.clone();
    match work.request {
        McpRequest::Load => tasks.read(root, agent, panel),
        McpRequest::Reconnect(name) => {
            let agent = agent.clone();
            tasks.spawn(async move {
                let result = match context.chat {
                    Some(chat) => agent
                        .reconnect_mcp_server(chat, &name)
                        .await
                        .map_err(|error| error.to_string()),
                    None => Err("the Agent has not started".to_owned()),
                };
                (panel, McpResult::Reconnected { name, result })
            });
        }
        McpRequest::SetDisabled { name, disabled } => {
            let root = root.to_owned();
            tasks.spawn(async move {
                let result = tokio::task::spawn_blocking({
                    let name = name.clone();
                    move || {
                        UserSettings::in_root(&root)
                            .set_mcp_server_disabled(&directory, &name, disabled)
                            .map_err(|error| error.to_string())
                    }
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
                (
                    panel,
                    McpResult::DisabledSet {
                        name,
                        disabled,
                        result,
                    },
                )
            });
        }
        McpRequest::LogIn { name, attempt } => {
            let cancel = CancellationToken::new();
            tasks.logins.insert(
                panel,
                Login {
                    attempt,
                    cancel: cancel.clone(),
                    paste: None,
                },
            );
            let root = root.to_owned();
            tasks.running.spawn(async move {
                let started = async {
                    let server = configured_server(&root, &directory, &name).await?;
                    let login = login_connector(&root, &server)?
                        .start_login(None, &cancel)
                        .await
                        .map_err(|error| error.to_string())?;
                    check_authorization_link(login.authorization_url(), &server.sensitive_values)?;
                    Ok::<_, String>(login)
                };
                let started = tokio::select! {
                    () = cancel.cancelled() => Err("the login was cancelled".to_owned()),
                    started = started => started,
                };
                let done = match started {
                    Ok(login) => Done::LoginStarted {
                        attempt,
                        name,
                        login: Box::new(login),
                        cancel,
                    },
                    Err(error) => Done::Result(McpResult::LoginEnded {
                        attempt,
                        name,
                        result: Err(error),
                    }),
                };
                (panel, done)
            });
        }
        McpRequest::SubmitCallback { attempt, address } => {
            let Some(paste) = tasks
                .logins
                .get(&panel)
                .filter(|login| login.attempt == attempt)
                .and_then(|login| login.paste.clone())
            else {
                return Ok(());
            };
            if let Err(error) = paste.submit(&address) {
                console.apply_mcp_result(
                    panel,
                    McpResult::CallbackRefused {
                        attempt,
                        error: error.to_string(),
                    },
                )?;
            }
        }
        McpRequest::CancelLogIn(attempt) => {
            if let Some(cancel) = tasks.end_login(panel, attempt) {
                cancel.cancel();
            }
        }
        McpRequest::LogOut(name) => {
            let root = root.to_owned();
            tasks.spawn(async move {
                let result = async {
                    let server = configured_server(&root, &directory, &name).await?;
                    let outcome = login_connector(&root, &server)?
                        .logout(&CancellationToken::new())
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok(outcome == tickloop_harness::connector::Logout::RevocationFailed)
                }
                .await;
                (panel, McpResult::LoggedOut { name, result })
            });
        }
    }
    Ok(())
}

/// Hands a finished task's result to its panel and reads the servers again after a change. When
/// the panel is gone, the outcome of an operation still reaches the user in the conversation.
pub(super) fn finish(
    console: &Console,
    root: &Path,
    agent: &ConsoleExecution,
    panel: Uuid,
    done: Done,
    tasks: &mut McpTasks,
) -> Result<(), String> {
    let result = match done {
        Done::Result(result) => result,
        Done::LoginStarted {
            attempt,
            name,
            login,
            cancel,
        } => return wait_for_callback(console, panel, (attempt, name), *login, cancel, tasks),
    };
    match &result {
        McpResult::LoginEnded { attempt, .. } => {
            tasks.end_login(panel, *attempt);
        }
        McpResult::Servers { listing, .. } => {
            tasks.reading.remove(&panel);
            // Later work turns servers off where the Agent works, which its status names.
            if let (Ok(listing), Some(context)) = (listing, tasks.contexts.get_mut(&panel)) {
                context.directory = listing.directory.clone();
            }
        }
        _ => {}
    }
    let changed = matches!(
        &result,
        McpResult::Reconnected { result: Ok(()), .. }
            | McpResult::DisabledSet { result: Ok(()), .. }
            | McpResult::LoginEnded { result: Ok(()), .. }
            | McpResult::LoggedOut { result: Ok(_), .. }
    );
    let notice = result.notice();
    match console.apply_mcp_result(panel, result)? {
        Some(true) => {}
        Some(false) => return Ok(()),
        None => {
            tasks.contexts.remove(&panel);
            tasks.logins.remove(&panel);
            if let Some(notice) = notice {
                console.write_line(&format!("{PANEL_COMMAND}: {}", safe_line(&notice.text)))?;
            }
            return Ok(());
        }
    }
    if changed {
        tasks.read(root, agent, panel);
    }
    Ok(())
}

/// Shows the panel the authorization link, opens it in the browser on this machine, and waits for
/// the callback. A login the panel no longer shows ends here.
fn wait_for_callback(
    console: &Console,
    panel: Uuid,
    (attempt, name): (u64, String),
    login: PendingLogin,
    cancel: CancellationToken,
    tasks: &mut McpTasks,
) -> Result<(), String> {
    let link = login.authorization_url().to_owned();
    // Only an https link is opened, and only where the browser runs on this machine.
    let opened = reqwest::Url::parse(&link).is_ok_and(|url| url.scheme() == "https")
        && std::env::var_os("SSH_CONNECTION").is_none();
    let waiting = McpResult::LoginWaiting {
        attempt,
        link: link.clone(),
        opened,
    };
    if cancel.is_cancelled() || console.apply_mcp_result(panel, waiting)? != Some(true) {
        cancel.cancel();
        tasks.end_login(panel, attempt);
        return Ok(());
    }
    if let Some(current) = tasks
        .logins
        .get_mut(&panel)
        .filter(|current| current.attempt == attempt)
    {
        current.paste = Some(login.paste());
    }
    if opened {
        tokio::spawn(async move { super::login::launch_browser(&link).await });
    }
    tasks.spawn(async move {
        let result = login
            .finish(&cancel)
            .await
            .map_err(|error| error.to_string());
        (
            panel,
            McpResult::LoginEnded {
                attempt,
                name,
                result,
            },
        )
    });
    Ok(())
}
