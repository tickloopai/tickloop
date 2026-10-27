//! Runs the work `/mcp` asks for. The Agent's Worker reports its servers and reconnects one;
//! turning a server off or on, changing its exposure, and logins happen here as in
//! `tickloop mcp`, and the Worker takes them up from its next request. A login runs in two tasks:
//! one finds the authorization link the panel shows, and one waits for the browser's callback or
//! the address the user pastes. After a login the server connects at once: through the Agent when
//! it runs, and otherwise through the daemon, which connects it as the Agent would, lists its tools
//! and closes the connection. Before the Agent starts, opening the panel connects its remote
//! servers that way, and `r` or Connect connects local ones too, so that no local program starts
//! unasked.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
};

use tickloop_code_settings::{
    UserSettings,
    mcp::{
        DIRECTORY_SOURCE, GLOBAL_SOURCE, McpLayer, PLUGIN_SOURCE_PREFIX, check_authorization_link,
        configured_servers, configured_statuses, login_connector,
    },
};
use tickloop_harness::{
    base::oauth_callback::CallbackPaste,
    connector::PendingLogin,
    protocol::mcp::{McpServerState, McpServerStatus, ToolExposure},
};
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
    /// The connection numbered `run` made for a panel before the Agent starts has ended.
    Probed {
        name: String,
        run: u64,
        status: Result<McpServerStatus, String>,
    },
}

/// Which servers a listing from the configuration connects for its panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeScope {
    /// The remote servers, when the panel opens: connecting them starts no local program.
    Remote,
    /// Every server that can run, at the user's `r`.
    All,
}

/// A connection made for a panel before the Agent starts, and its status once it ended.
struct Probe {
    run: u64,
    status: Option<Result<McpServerStatus, String>>,
}

/// What a panel works on: its Chat, when the Agent runs, and the directory it works in.
struct PanelContext {
    chat: Option<Uuid>,
    directory: PathBuf,
    /// Before the Agent starts: the servers connected for the panel, by name.
    probes: BTreeMap<String, Probe>,
    /// The servers the next listing connects for the panel.
    pending_probe: Option<ProbeScope>,
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
    probe_runs: u64,
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
        let Some(context) = self.contexts.get(&panel) else {
            return;
        };
        let (chat, directory) = (context.chat, context.directory.clone());
        self.reads += 1;
        let read = self.reads;
        self.reading.insert(panel);
        let (root, agent) = (root.to_owned(), agent.clone());
        self.spawn(async move {
            let listing = read_servers(&root, &agent, chat, directory).await;
            (panel, McpResult::Servers { read, listing })
        });
    }

    /// Connects the servers `names` for `panel` before the Agent starts, unless a connection to
    /// one is under way.
    fn probe(&mut self, agent: &ConsoleExecution, panel: Uuid, names: Vec<String>) {
        let Some(context) = self.contexts.get_mut(&panel) else {
            return;
        };
        for name in names {
            if context
                .probes
                .get(&name)
                .is_some_and(|probe| probe.status.is_none())
            {
                continue;
            }
            self.probe_runs += 1;
            let run = self.probe_runs;
            context
                .probes
                .insert(name.clone(), Probe { run, status: None });
            let (agent, directory) = (agent.clone(), context.directory.clone());
            self.running.spawn(async move {
                let status = agent
                    .draft_mcp_server(directory, &name)
                    .await
                    .map_err(|error| error.to_string());
                (panel, Done::Probed { name, run, status })
            });
        }
    }

    /// Connects the server `name` of `panel` now: through the Agent when it runs, and for the
    /// panel before it starts.
    fn connect(&mut self, agent: &ConsoleExecution, panel: Uuid, name: String) {
        let Some(chat) = self.contexts.get(&panel).map(|context| context.chat) else {
            return;
        };
        match chat {
            Some(chat) => {
                let agent = agent.clone();
                self.spawn(async move {
                    let result = agent
                        .reconnect_mcp_server(chat, &name)
                        .await
                        .map_err(|error| error.to_string());
                    (panel, McpResult::Reconnected { name, result })
                });
            }
            None => {
                self.probe(agent, panel, vec![name.clone()]);
                self.spawn(async move {
                    (
                        panel,
                        McpResult::Reconnected {
                            name,
                            result: Ok(()),
                        },
                    )
                });
            }
        }
    }

    /// Takes the end of the connection `run` to `name` made for `panel`; false when the panel
    /// no longer waits for it.
    fn probed(
        &mut self,
        panel: Uuid,
        name: &str,
        run: u64,
        status: Result<McpServerStatus, String>,
    ) -> bool {
        let Some(probe) = self
            .contexts
            .get_mut(&panel)
            .and_then(|context| context.probes.get_mut(name))
            .filter(|probe| probe.run == run)
        else {
            return false;
        };
        probe.status = Some(status);
        true
    }

    /// Before the Agent starts: connects what the panel asked for and shows the connections made
    /// for it in `listing`, which comes from the configuration.
    fn take_up_probes(&mut self, agent: &ConsoleExecution, panel: Uuid, listing: &mut McpListing) {
        if listing.live {
            return;
        }
        let scope = self
            .contexts
            .get_mut(&panel)
            .and_then(|context| context.pending_probe.take());
        if let Some(scope) = scope {
            self.probe(agent, panel, probe_targets(&listing.servers, scope));
        }
        if let Some(context) = self.contexts.get_mut(&panel) {
            show_probes(listing, &mut context.probes);
        }
    }

    /// Forgets the connection made for `panel` to `name`, whose login or settings change.
    fn forget_probe(&mut self, panel: Uuid, name: &str) {
        if let Some(context) = self.contexts.get_mut(&panel) {
            context.probes.remove(name);
        }
    }
}

/// The servers of a listing from the configuration that `scope` connects: those that can run,
/// and of them only the remote ones when the panel opens.
fn probe_targets(servers: &[McpServerStatus], scope: ProbeScope) -> Vec<String> {
    servers
        .iter()
        .filter(|server| {
            server.state == McpServerState::Connecting
                && (scope == ProbeScope::All || server.transport == "http")
        })
        .map(|server| server.name.clone())
        .collect()
}

/// Shows in `listing`, which comes from the configuration, the state of each server connected
/// for the panel: connecting while that runs, then how it ended. The exposure stays the
/// configuration's, which `e` may have changed since the connection. A server that can no longer
/// run is forgotten.
fn show_probes(listing: &mut McpListing, probes: &mut BTreeMap<String, Probe>) {
    probes.retain(|name, _| {
        listing
            .servers
            .iter()
            .any(|server| server.name == *name && server.state == McpServerState::Connecting)
    });
    for server in &mut listing.servers {
        let Some(probe) = probes.get(&server.name) else {
            continue;
        };
        match &probe.status {
            None => {}
            Some(Ok(status)) => {
                let exposure = std::mem::take(&mut server.exposure);
                *server = McpServerStatus {
                    exposure,
                    ..status.clone()
                };
            }
            Some(Err(error)) => {
                server.state = McpServerState::Failed;
                server.failure = Some(error.clone());
            }
        }
        listing.probed.insert(server.name.clone());
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
    tasks.contexts.insert(
        id,
        PanelContext {
            chat,
            directory,
            probes: BTreeMap::new(),
            pending_probe: chat.is_none().then_some(ProbeScope::Remote),
        },
    );
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
    chat: Option<Uuid>,
    directory: PathBuf,
) -> Result<McpListing, String> {
    if let Some(chat) = chat {
        let servers = agent
            .mcp_servers(chat)
            .await
            .map_err(|error| error.to_string())?;
        let directory = agent
            .status(chat)
            .await
            .ok()
            .and_then(|status| status.folder)
            .unwrap_or(directory);
        return Ok(McpListing {
            servers,
            live: true,
            directory,
            probed: Default::default(),
        });
    }
    let resolved = configured(root, &directory).await?;
    Ok(McpListing {
        servers: configured_statuses(root, &resolved.servers).await,
        live: false,
        directory,
        probed: Default::default(),
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
    let Some(context) = tasks.contexts.get_mut(&panel) else {
        return Ok(());
    };
    let directory = context.directory.clone();
    match work.request {
        McpRequest::Load => {
            if context.chat.is_none() {
                context.pending_probe = Some(ProbeScope::All);
            }
            tasks.read(root, agent, panel);
        }
        McpRequest::Reconnect(name) => tasks.connect(agent, panel, name),
        McpRequest::SetDisabled { name, disabled } => {
            tasks.forget_probe(panel, &name);
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
        McpRequest::SetExposure {
            name,
            source,
            exposure,
        } => {
            let root = root.to_owned();
            tasks.spawn(async move {
                let result = tokio::task::spawn_blocking({
                    let name = name.clone();
                    move || save_exposure(&root, &directory, &source, &name, exposure)
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
                (
                    panel,
                    McpResult::ExposureSet {
                        name,
                        exposure,
                        result,
                    },
                )
            });
        }
        McpRequest::LogIn { name, attempt } => {
            tasks.forget_probe(panel, &name);
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
            tasks.forget_probe(panel, &name);
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

/// Saves the exposure of the server `name` where `source` says it is configured: in that user
/// layer, or, for a plugin server, as the override in the global settings.
fn save_exposure(
    root: &Path,
    directory: &Path,
    source: &str,
    name: &str,
    exposure: ToolExposure,
) -> Result<(), String> {
    let settings = UserSettings::in_root(root);
    let layer = match source {
        GLOBAL_SOURCE => McpLayer::Global,
        DIRECTORY_SOURCE => McpLayer::Directory(directory.to_owned()),
        _ if source.starts_with(PLUGIN_SOURCE_PREFIX) => {
            return settings
                .set_plugin_mcp_server_exposure(name, exposure)
                .map_err(|error| error.to_string());
        }
        _ => return Err(format!("{name} is not configured in the settings")),
    };
    match settings.set_mcp_server_exposure(&layer, name, exposure) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("{name} is no longer configured there")),
        Err(error) => Err(error.to_string()),
    }
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
    let mut result = match done {
        Done::Result(result) => result,
        Done::LoginStarted {
            attempt,
            name,
            login,
            cancel,
        } => return wait_for_callback(console, panel, (attempt, name), *login, cancel, tasks),
        Done::Probed { name, run, status } => {
            if tasks.probed(panel, &name, run, status) {
                tasks.read(root, agent, panel);
            }
            return Ok(());
        }
    };
    match &mut result {
        McpResult::LoginEnded { attempt, .. } => {
            tasks.end_login(panel, *attempt);
        }
        McpResult::Servers { listing, .. } => {
            tasks.reading.remove(&panel);
            if let Ok(listing) = listing {
                // Later work turns servers off where the Agent works, which its status names.
                if let Some(context) = tasks.contexts.get_mut(&panel) {
                    context.directory = listing.directory.clone();
                }
                tasks.take_up_probes(agent, panel, listing);
            }
        }
        _ => {}
    }
    let logged_in_server = match &result {
        McpResult::LoginEnded {
            name,
            result: Ok(()),
            ..
        } => Some(name.clone()),
        _ => None,
    };
    let changed = matches!(
        &result,
        McpResult::Reconnected { result: Ok(()), .. }
            | McpResult::DisabledSet { result: Ok(()), .. }
            | McpResult::ExposureSet { result: Ok(()), .. }
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
    if let Some(name) = logged_in_server {
        tasks.connect(agent, panel, name);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn configured(name: &str, transport: &str, state: McpServerState) -> McpServerStatus {
        McpServerStatus {
            name: name.into(),
            source: "global".into(),
            transport: transport.into(),
            state,
            exposure: "search".into(),
            tools: Vec::new(),
            failure: None,
            oauth: transport == "http",
            logged_in: false,
        }
    }

    fn listing(servers: Vec<McpServerStatus>) -> McpListing {
        McpListing {
            servers,
            live: false,
            directory: "/work".into(),
            probed: Default::default(),
        }
    }

    #[test]
    fn opening_the_panel_connects_only_remote_servers_that_can_run_and_r_connects_local_ones_too() {
        let servers = [
            configured("remote", "http", McpServerState::Connecting),
            configured("local", "stdio", McpServerState::Connecting),
            configured("off", "http", McpServerState::Disabled),
            configured("broken", "", McpServerState::Failed),
        ];
        assert_eq!(probe_targets(&servers, ProbeScope::Remote), ["remote"]);
        assert_eq!(
            probe_targets(&servers, ProbeScope::All),
            ["remote", "local"]
        );
    }

    #[test]
    fn a_listing_from_the_configuration_shows_the_connections_made_for_the_panel() {
        let mut connected = configured("remote", "http", McpServerState::Connected);
        connected
            .tools
            .push(tickloop_harness::protocol::mcp::McpToolSummary {
                name: "mcp__remote__search".into(),
                description: String::new(),
            });
        let mut probes = BTreeMap::from([
            (
                "remote".to_owned(),
                Probe {
                    run: 1,
                    status: Some(Ok(connected.clone())),
                },
            ),
            (
                "local".to_owned(),
                Probe {
                    run: 2,
                    status: None,
                },
            ),
            (
                "other".to_owned(),
                Probe {
                    run: 3,
                    status: Some(Err("MCP connection timed out".into())),
                },
            ),
            (
                "off".to_owned(),
                Probe {
                    run: 4,
                    status: Some(Ok(connected.clone())),
                },
            ),
        ]);
        let mut shown = listing(vec![
            configured("remote", "http", McpServerState::Connecting),
            configured("local", "stdio", McpServerState::Connecting),
            configured("other", "http", McpServerState::Connecting),
            configured("off", "http", McpServerState::Disabled),
            configured("idle", "stdio", McpServerState::Connecting),
        ]);
        show_probes(&mut shown, &mut probes);
        assert_eq!(shown.servers[0], connected);
        assert_eq!(shown.servers[1].state, McpServerState::Connecting);
        assert_eq!(
            (&shown.servers[2].state, shown.servers[2].failure.as_deref()),
            (&McpServerState::Failed, Some("MCP connection timed out"))
        );
        assert_eq!(shown.servers[3].state, McpServerState::Disabled);
        assert_eq!(
            shown.probed.iter().map(String::as_str).collect::<Vec<_>>(),
            ["local", "other", "remote"]
        );
        assert!(
            !probes.contains_key("off"),
            "a server that can no longer run is forgotten"
        );
    }

    #[test]
    fn a_connection_made_for_the_panel_keeps_the_exposure_chosen_since() {
        let connected = configured("remote", "http", McpServerState::Connected);
        let mut probes = BTreeMap::from([(
            "remote".to_owned(),
            Probe {
                run: 1,
                status: Some(Ok(connected)),
            },
        )]);
        let mut changed = configured("remote", "http", McpServerState::Connecting);
        changed.exposure = "direct".into();
        let mut shown = listing(vec![changed]);
        show_probes(&mut shown, &mut probes);
        assert_eq!(
            (&shown.servers[0].state, shown.servers[0].exposure.as_str()),
            (&McpServerState::Connected, "direct")
        );
    }
}
