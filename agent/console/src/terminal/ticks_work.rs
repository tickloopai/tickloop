//! Runs the work `/ticks` asks for, through the same installs, updates, uninstalls, logins and
//! logouts as `tickloop tick`: each runs as a task, and an OAuth login in two, one that gets the
//! device code the panel shows and one that waits for the user to approve it.

use std::{collections::HashMap, future::Future, path::Path};

use tick_packages::PackageStore;
use tickloop_tick_installer::{
    DeviceLogin, LoggedOut, LoginError, MarketplaceSource, needs_no_login, start_login,
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    console::Console,
    safe_line,
    ticks_panel::{
        Applied, InstalledList, TickPackageRequest, TickPackageResult, TickPackageWork, TicksPanel,
    },
};

/// What a `/ticks` task ends with.
pub(super) enum Done {
    Result(TickPackageResult),
    /// The device authorization of a login, which the panel shows while it waits for approval.
    LoginStarted {
        attempt: u64,
        name: String,
        login: Box<DeviceLogin>,
        cancel: CancellationToken,
    },
}

/// The `/ticks` work under way. Reads of the installed packages are numbered as they start, so a
/// panel keeps the most recent one whatever order they finish in.
#[derive(Default)]
pub(super) struct TickPackageTasks {
    running: JoinSet<(Uuid, Done)>,
    installed_reads: u64,
    /// The login under way in each panel: its attempt and what cancels it.
    logins: HashMap<Uuid, (u64, CancellationToken)>,
}

impl TickPackageTasks {
    pub(super) fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    /// The next finished work and its panel; `None` for work that panicked.
    pub(super) async fn join_next(&mut self) -> Option<(Uuid, Done)> {
        self.running.join_next().await?.ok()
    }

    fn spawn(&mut self, work: impl Future<Output = (Uuid, TickPackageResult)> + Send + 'static) {
        self.running.spawn(async move {
            let (panel, result) = work.await;
            (panel, Done::Result(result))
        });
    }

    /// Forgets the login `attempt` of `panel`, unless a later one has taken its place, and
    /// returns what cancels it.
    fn end_login(&mut self, panel: Uuid, attempt: u64) -> Option<CancellationToken> {
        let (current, _) = self.logins.get(&panel)?;
        if *current != attempt {
            return None;
        }
        self.logins.remove(&panel).map(|(_, cancel)| cancel)
    }

    fn next_installed_read(&mut self) -> u64 {
        self.installed_reads += 1;
        self.installed_reads
    }

    fn read_installed(&mut self, store: PackageStore, panel: Uuid) {
        let read = self.next_installed_read();
        self.spawn(async move {
            let listing = blocking(move || read_installed_now(&store)).await;
            (panel, TickPackageResult::Installed { read, listing })
        });
    }
}

/// Opens `/ticks` and starts reading what it lists.
pub(super) fn open(
    console: &Console,
    root: &Path,
    chat: Option<Uuid>,
    tasks: &mut TickPackageTasks,
) -> Result<(), String> {
    let panel = TicksPanel::new(chat);
    let id = panel.id();
    console.show_ticks_panel(panel)?;
    start(
        root,
        TickPackageWork {
            panel: id,
            request: TickPackageRequest::Load,
        },
        tasks,
    );
    Ok(())
}

pub(super) fn start(root: &Path, work: TickPackageWork, tasks: &mut TickPackageTasks) {
    let store = PackageStore::in_config_root(root);
    let panel = work.panel;
    match work.request {
        TickPackageRequest::Load => {
            tasks.read_installed(store, panel);
            tasks.spawn(async move {
                let marketplace = match MarketplaceSource::configured() {
                    Ok(source) => {
                        source
                            .read(&tickloop_agent_settings::package_http::client)
                            .await
                    }
                    Err(error) => Err(error),
                };
                (panel, TickPackageResult::Marketplace(marketplace))
            });
        }
        TickPackageRequest::Install(package) => {
            tasks.spawn(async move {
                let result = match MarketplaceSource::configured() {
                    Ok(source) => tickloop_tick_installer::install(
                        &tickloop_agent_settings::package_http::client,
                        &store,
                        &source,
                        &package,
                    )
                    .await
                    .map(Box::new),
                    Err(error) => Err(error),
                };
                let name = package.name;
                (panel, TickPackageResult::Installation { name, result })
            });
        }
        TickPackageRequest::Uninstall(name) => {
            tasks.spawn(async move {
                let result = blocking({
                    let name = name.clone();
                    move || tickloop_tick_installer::uninstall(&store, &name)
                })
                .await;
                (panel, TickPackageResult::Uninstallation { name, result })
            });
        }
        TickPackageRequest::LogIn {
            name,
            issuer,
            scopes,
            attempt,
        } => {
            let cancel = CancellationToken::new();
            tasks.logins.insert(panel, (attempt, cancel.clone()));
            tasks.running.spawn(async move {
                let started = async {
                    let credentials = store.credentials(&name).map_err(LoginError::from)?;
                    start_login(
                        &tickloop_agent_settings::package_http::client,
                        &credentials,
                        &issuer,
                        &scopes,
                    )
                    .await
                };
                let started = tokio::select! {
                    () = cancel.cancelled() => Err(LoginError::Cancelled),
                    started = started => started,
                };
                let done = match started {
                    Ok(login) => Done::LoginStarted {
                        attempt,
                        name,
                        login: Box::new(login),
                        cancel,
                    },
                    Err(error) => Done::Result(TickPackageResult::LoginEnded {
                        attempt,
                        name,
                        result: Err(error),
                    }),
                };
                (panel, done)
            });
        }
        TickPackageRequest::CancelLogIn(attempt) => {
            if let Some(cancel) = tasks.end_login(panel, attempt) {
                cancel.cancel();
            }
        }
        TickPackageRequest::SaveKey { name, key } => {
            tasks.spawn(async move {
                let result = blocking({
                    let name = name.clone();
                    move || {
                        store
                            .credentials(&name)
                            .and_then(|credentials| credentials.save_api_key(&key))
                            .map_err(|error| error.to_string())
                    }
                })
                .await;
                (panel, TickPackageResult::KeySaved { name, result })
            });
        }
        TickPackageRequest::LogOut(name) => {
            tasks.spawn(async move {
                let result = log_out(&store, &name).await;
                (panel, TickPackageResult::Logout { name, result })
            });
        }
        TickPackageRequest::RefreshLogin(name) => {
            // Numbered now, for the case where the package went away meanwhile and the list is
            // read instead.
            let read = tasks.next_installed_read();
            tasks.spawn(async move {
                let result = blocking(move || {
                    Ok(match store.get(&name) {
                        Ok(package) => TickPackageResult::Login {
                            state: store.login_state(&package),
                            name,
                        },
                        Err(_) => TickPackageResult::Installed {
                            read,
                            listing: read_installed_now(&store),
                        },
                    })
                })
                .await;
                let result = result.unwrap_or_else(|error| TickPackageResult::Installed {
                    read,
                    listing: Err(error),
                });
                (panel, result)
            });
        }
    }
}

/// Hands a finished task's result to its panel and reads again what it changed. When the panel is
/// gone, the outcome of an operation still reaches the user in the conversation.
pub(super) fn finish(
    console: &Console,
    root: &Path,
    panel: Uuid,
    done: Done,
    tasks: &mut TickPackageTasks,
) -> Result<(), String> {
    let result = match done {
        Done::Result(result) => result,
        Done::LoginStarted {
            attempt,
            name,
            login,
            cancel,
        } => return wait_for_approval(console, panel, (attempt, name), *login, cancel, tasks),
    };
    if let TickPackageResult::LoginEnded { attempt, .. } = result {
        tasks.end_login(panel, attempt);
    }
    let changed_packages = matches!(
        &result,
        TickPackageResult::Installation { result: Ok(_), .. }
            | TickPackageResult::Uninstallation { result: Ok(_), .. }
    );
    let changed_login = match &result {
        TickPackageResult::Logout { name, .. }
        | TickPackageResult::LoginEnded { name, .. }
        | TickPackageResult::KeySaved { name, .. } => Some(name.clone()),
        _ => None,
    };
    let notice = result.notice();
    match console.apply_tick_package_result(panel, result)? {
        Applied::Taken => {}
        Applied::Ignored => return Ok(()),
        Applied::NoPanel => {
            if let Some(notice) = notice {
                console.write_line(&format!("/ticks: {}", safe_line(&notice.text)))?;
            }
            return Ok(());
        }
    }
    if changed_packages {
        tasks.read_installed(PackageStore::in_config_root(root), panel);
    }
    if let Some(name) = changed_login {
        start(
            root,
            TickPackageWork {
                panel,
                request: TickPackageRequest::RefreshLogin(name),
            },
            tasks,
        );
    }
    Ok(())
}

/// Shows the panel the link and code of `login`, opens an https link in the browser as the
/// Console's other device logins do, and waits for the user's approval. A login the panel no
/// longer shows, cancelled meanwhile or with its panel gone, ends here: no browser, no polling.
fn wait_for_approval(
    console: &Console,
    panel: Uuid,
    (attempt, name): (u64, String),
    login: DeviceLogin,
    cancel: CancellationToken,
    tasks: &mut TickPackageTasks,
) -> Result<(), String> {
    let link = login.link().to_owned();
    let opened = reqwest::Url::parse(&link).is_ok_and(|url| url.scheme() == "https");
    let waiting = TickPackageResult::LoginWaiting {
        attempt,
        link: link.clone(),
        code: login.user_code().to_owned(),
        opened,
    };
    if cancel.is_cancelled() || console.apply_tick_package_result(panel, waiting)? != Applied::Taken
    {
        cancel.cancel();
        tasks.end_login(panel, attempt);
        return Ok(());
    }
    if opened {
        tokio::spawn(async move { super::login::launch_browser(&link).await });
    }
    tasks.spawn(async move {
        let result = login.finish(&cancel, |_| {}).await;
        (
            panel,
            TickPackageResult::LoginEnded {
                attempt,
                name,
                result,
            },
        )
    });
    Ok(())
}

/// The installed packages with the login states their saved credentials give.
fn read_installed_now(store: &PackageStore) -> Result<InstalledList, String> {
    let packages = store.installed().map_err(|error| error.to_string())?;
    let logins = packages
        .packages
        .iter()
        .map(|package| store.login_state(package))
        .collect();
    Ok(InstalledList { packages, logins })
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| error.to_string())?
}

/// Deletes the package's saved login, revoking an OAuth token where its server allows.
async fn log_out(store: &PackageStore, name: &str) -> Result<LoggedOut, String> {
    let package = store.get(name).map_err(|error| error.to_string())?;
    let credentials = store.credentials(name).map_err(|error| error.to_string())?;
    match package.manifest.auth {
        tick_packages::PackageAuth::None => Err(needs_no_login(name)),
        auth => {
            tickloop_tick_installer::log_out(
                &tickloop_agent_settings::package_http::client,
                &credentials,
                &auth,
            )
            .await
        }
    }
}
