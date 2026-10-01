use std::{
    collections::{HashMap, HashSet},
    os::unix::fs::{FileTypeExt, MetadataExt},
    time::{Duration, Instant},
};

use tokio::sync::Mutex;
use uuid::Uuid;

use super::protocol::*;

#[derive(Default)]
pub(crate) struct ViewRegistry {
    entries: Mutex<ViewEntries>,
}
#[derive(Default)]
struct ViewEntries {
    views: HashMap<ViewRef, ViewEntry>,
    closed_sessions: HashSet<Uuid>,
}
struct ViewEntry {
    version: u64,
    execution: Option<ExecutionRef>,
    host: Option<HostLocation>,
    active: bool,
    renewed: Instant,
}

impl ViewRegistry {
    pub async fn update(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        let mut entries = self.entries.lock().await;
        match update {
            DisplayUpdate::Displayed {
                view,
                expected_version,
                execution,
                host,
            } => Self::register(&mut entries, view, expected_version, Some(execution), host),
            DisplayUpdate::Scene {
                view,
                expected_version,
                host,
            } => Self::register(&mut entries, view, expected_version, None, host),
            DisplayUpdate::Renew { view, version } => {
                let current = Self::current(&mut entries, view, version)?;
                current.renewed = Instant::now();
                Ok(DisplayReceipt {
                    view,
                    version,
                    displayed: current.execution.is_some(),
                })
            }
            DisplayUpdate::Left { view, version } => {
                let current = Self::current(&mut entries, view, version)?;
                current.active = false;
                current.execution = None;
                Ok(DisplayReceipt {
                    view,
                    version,
                    displayed: false,
                })
            }
            DisplayUpdate::SessionClosed { session_id } => {
                entries.closed_sessions.insert(session_id);
                entries
                    .views
                    .retain(|view, _| view.session_id != session_id);
                Ok(DisplayReceipt {
                    view: ViewRef {
                        session_id,
                        view_id: Uuid::nil(),
                    },
                    version: 0,
                    displayed: false,
                })
            }
        }
    }

    fn register(
        entries: &mut ViewEntries,
        view: ViewRef,
        expected_version: u64,
        execution: Option<ExecutionRef>,
        host: Option<HostLocation>,
    ) -> Result<DisplayReceipt, AccessError> {
        if entries.closed_sessions.contains(&view.session_id) {
            return Err(AccessError::new(
                AccessErrorCode::AccessDenied,
                "View connection session has closed",
            ));
        }
        let version = entries
            .views
            .get(&view)
            .map_or(0, |current| current.version);
        if version != expected_version {
            return Err(AccessError::new(
                AccessErrorCode::VersionConflict,
                "display changed before registration",
            ));
        }
        let version = version
            .checked_add(1)
            .ok_or_else(|| AccessError::invalid("View version overflow"))?;
        entries.views.insert(
            view,
            ViewEntry {
                version,
                execution,
                host,
                active: true,
                renewed: Instant::now(),
            },
        );
        Ok(DisplayReceipt {
            view,
            version,
            displayed: execution.is_some(),
        })
    }

    fn current(
        entries: &mut ViewEntries,
        view: ViewRef,
        version: u64,
    ) -> Result<&mut ViewEntry, AccessError> {
        entries
            .views
            .get_mut(&view)
            .filter(|entry| {
                entry.active
                    && entry.version == version
                    && entry.renewed.elapsed() < Duration::from_secs(VIEW_LEASE_SECONDS)
            })
            .ok_or_else(|| {
                AccessError::new(
                    AccessErrorCode::VersionConflict,
                    "View lease is no longer current",
                )
            })
    }

    pub async fn validate_source(
        &self,
        source: &InteractionSource,
        execution: Option<ExecutionRef>,
    ) -> Result<(), AccessError> {
        let mut entries = self.entries.lock().await;
        let current = Self::current(&mut entries, source.view, source.display_version)?;
        if current.host != source.host
            || current
                .execution
                .is_some_and(|displayed| Some(displayed) != execution)
        {
            return Err(AccessError::new(
                AccessErrorCode::VersionConflict,
                "input's source View changed before submission",
            ));
        }
        Ok(())
    }

    pub async fn displayed(&self) -> Vec<DisplayedView> {
        self.entries
            .lock()
            .await
            .views
            .iter()
            .filter_map(|(view, current)| {
                if !current.active
                    || current.renewed.elapsed() >= Duration::from_secs(VIEW_LEASE_SECONDS)
                {
                    return None;
                }
                Some(DisplayedView {
                    view: *view,
                    version: current.version,
                    execution: current.execution?,
                    host: current.host.clone(),
                })
            })
            .collect()
    }
}

pub(crate) fn command_origin(command: &AgentCommand) -> Option<&InteractionSource> {
    match command {
        AgentCommand::Create { origin, .. }
        | AgentCommand::Submit { origin, .. }
        | AgentCommand::ContinueLoop { origin, .. } => origin.as_ref(),
        AgentCommand::EnqueueAfterTurn {
            action: AfterTurnAction::Submit { origin, .. },
            ..
        } => origin.as_ref(),
        _ => None,
    }
}

pub(crate) async fn verify_host(host: Option<&HostLocation>) -> Result<(), AccessError> {
    let Some(host) = host else {
        return Ok(());
    };
    if host.refresh().await? != *host {
        return Err(AccessError::new(
            AccessErrorCode::VersionConflict,
            "interaction source Pane moved; register its current display before submission",
        ));
    }
    Ok(())
}

impl HostLocation {
    pub(crate) async fn refresh(&self) -> Result<Self, AccessError> {
        self.verify_endpoint()?;
        let pane_id = self
            .pane
            .as_ref()
            .ok_or_else(|| AccessError::invalid("interaction source has no Pane"))?;
        let terminal_id = self
            .terminal
            .as_ref()
            .ok_or_else(|| AccessError::invalid("interaction source has no terminal"))?;
        let access = tick_connect::access::TickAccess::new(tick_connect::herdr::HerdrContext {
            socket: self.socket.clone(),
            caller_pane: pane_id.clone(),
        });
        let pane = tokio::time::timeout(Duration::from_secs(5), access.caller())
            .await
            .map_err(|_| AccessError::unavailable("interaction source host did not respond"))?
            .map_err(|error| AccessError::new(AccessErrorCode::AccessDenied, error.to_string()))?;
        self.verify_endpoint()?;
        if *terminal_id != pane.terminal_id {
            return Err(AccessError::new(
                AccessErrorCode::VersionConflict,
                "interaction source Pane or terminal was replaced",
            ));
        }
        let mut current = self.clone();
        current.workspace = Some(pane.workspace_id);
        current.tab = Some(pane.tab_id);
        current.pane = Some(pane.pane_id);
        Ok(current)
    }

    fn verify_endpoint(&self) -> Result<(), AccessError> {
        let metadata = std::fs::symlink_metadata(&self.socket).map_err(|error| {
            AccessError::new(
                AccessErrorCode::AccessDenied,
                format!("cannot inspect interaction source endpoint: {error}"),
            )
        })?;
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(AccessError::new(
                AccessErrorCode::AccessDenied,
                "interaction source endpoint must be a socket owned by this user",
            ));
        }
        if self.host_instance != format!("herdr:{}:{}", metadata.dev(), metadata.ino()) {
            return Err(AccessError::new(
                AccessErrorCode::VersionConflict,
                "interaction source host endpoint was replaced",
            ));
        }
        Ok(())
    }
}
