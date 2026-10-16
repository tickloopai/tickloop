//! Runs the work `/ticks` asks for, through the same installs, updates, uninstalls and logouts
//! as `tickloop tick`: reading the installed packages and the Tick marketplace, installing,
//! uninstalling and logging out run as tasks.

use std::{future::Future, path::Path};

use tick_packages::PackageStore;
use tickloop_agent_packages::{LoggedOut, MarketplaceSource, needs_no_login};
use tokio::task::JoinSet;
use uuid::Uuid;

use super::{
    console::Console,
    safe_line,
    ticks_panel::{
        InstalledList, TickPackageRequest, TickPackageResult, TickPackageWork, TicksPanel,
    },
};

/// The `/ticks` work under way. Reads of the installed packages are numbered as they start, so a
/// panel keeps the most recent one whatever order they finish in.
#[derive(Default)]
pub(super) struct TickPackageTasks {
    running: JoinSet<(Uuid, TickPackageResult)>,
    installed_reads: u64,
}

impl TickPackageTasks {
    pub(super) fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    /// The next finished work and its panel; `None` for work that panicked.
    pub(super) async fn join_next(&mut self) -> Option<(Uuid, TickPackageResult)> {
        self.running.join_next().await?.ok()
    }

    fn spawn(&mut self, work: impl Future<Output = (Uuid, TickPackageResult)> + Send + 'static) {
        self.running.spawn(work);
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
                    Ok(source) => source.read().await,
                    Err(error) => Err(error),
                };
                (panel, TickPackageResult::Marketplace(marketplace))
            });
        }
        TickPackageRequest::Install(package) => {
            tasks.spawn(async move {
                let result = match MarketplaceSource::configured() {
                    Ok(source) => tickloop_agent_packages::install(&store, &source, &package)
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
                    move || tickloop_agent_packages::uninstall(&store, &name)
                })
                .await;
                (panel, TickPackageResult::Uninstallation { name, result })
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

/// Hands `result` to its panel and reads again what it changed. When the panel is gone, the
/// outcome of an operation still reaches the user in the conversation.
pub(super) fn finish(
    console: &Console,
    root: &Path,
    panel: Uuid,
    result: TickPackageResult,
    tasks: &mut TickPackageTasks,
) -> Result<(), String> {
    let changed_packages = matches!(
        &result,
        TickPackageResult::Installation { result: Ok(_), .. }
            | TickPackageResult::Uninstallation { result: Ok(_), .. }
    );
    let logged_out = match &result {
        TickPackageResult::Logout { name, .. } => Some(name.clone()),
        _ => None,
    };
    let notice = result.notice();
    if !console.apply_tick_package_result(panel, result)? {
        if let Some(notice) = notice {
            console.write_line(&format!("/ticks: {}", safe_line(&notice.text)))?;
        }
        return Ok(());
    }
    if changed_packages {
        tasks.read_installed(PackageStore::in_config_root(root), panel);
    }
    if let Some(name) = logged_out {
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
        auth => tickloop_agent_packages::log_out(&credentials, &auth).await,
    }
}
