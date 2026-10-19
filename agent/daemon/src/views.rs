use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tickhost_connect::herdr::{PaneIdentityError, confirm_pane};
use tokio::{sync::Mutex, time::Instant};
use uuid::Uuid;

use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, AfterTurnAction, AgentCommand, DisplayReceipt, DisplayUpdate,
    DisplayedView, ExecutionRef, HostLocation, InteractionSource, VIEW_LEASE_SECONDS, ViewRef,
};

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

#[derive(Serialize, Deserialize)]
pub(crate) struct ViewCheckpoint {
    views: Vec<(ViewRef, RetainedView)>,
    closed_sessions: HashSet<Uuid>,
}

#[derive(Serialize, Deserialize)]
struct RetainedView {
    version: u64,
    execution: Option<ExecutionRef>,
    host: Option<HostLocation>,
    active: bool,
    age: Duration,
}

impl ViewRegistry {
    pub async fn checkpoint(&self) -> ViewCheckpoint {
        let entries = self.entries.lock().await;
        ViewCheckpoint {
            views: entries
                .views
                .iter()
                .map(|(view, entry)| {
                    (
                        *view,
                        RetainedView {
                            version: entry.version,
                            execution: entry.execution,
                            host: entry.host.clone(),
                            active: entry.active,
                            age: entry.renewed.elapsed(),
                        },
                    )
                })
                .collect(),
            closed_sessions: entries.closed_sessions.clone(),
        }
    }

    pub async fn restore(&self, checkpoint: ViewCheckpoint) {
        let now = Instant::now();
        *self.entries.lock().await = ViewEntries {
            views: checkpoint
                .views
                .into_iter()
                .map(|(view, entry)| {
                    (
                        view,
                        ViewEntry {
                            version: entry.version,
                            execution: entry.execution,
                            host: entry.host,
                            active: entry.active,
                            renewed: now.checked_sub(entry.age).unwrap_or(now),
                        },
                    )
                })
                .collect(),
            closed_sessions: checkpoint.closed_sessions,
        };
    }

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

    pub async fn validate_displayed_source(
        &self,
        source: &InteractionSource,
        execution: ExecutionRef,
    ) -> Result<(), AccessError> {
        let mut entries = self.entries.lock().await;
        let current = Self::current(&mut entries, source.view, source.display_version)?;
        if current.host != source.host || current.execution != Some(execution) {
            return Err(AccessError::new(
                AccessErrorCode::VersionConflict,
                "directed input source is no longer the displayed Agent",
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
        AgentCommand::SendFamilyInput { origin, .. } => Some(origin),
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
    if refresh_host_location(host).await? != *host {
        return Err(AccessError::new(
            AccessErrorCode::VersionConflict,
            "interaction source Pane moved; register its current display before submission",
        ));
    }
    Ok(())
}

/// `host` as Herdr reports its Pane now; the endpoint and the terminal must be the ones recorded.
pub(crate) async fn refresh_host_location(
    host: &HostLocation,
) -> Result<HostLocation, AccessError> {
    let pane_id = host
        .pane
        .as_ref()
        .ok_or_else(|| AccessError::invalid("interaction source has no Pane"))?;
    let terminal_id = host
        .terminal
        .as_ref()
        .ok_or_else(|| AccessError::invalid("interaction source has no terminal"))?;
    let pane = confirm_pane(&host.socket, &host.host_instance, pane_id, terminal_id)
        .await
        .map_err(|error| {
            let code = match error {
                PaneIdentityError::Denied(_) => AccessErrorCode::AccessDenied,
                PaneIdentityError::Replaced(_) => AccessErrorCode::VersionConflict,
                PaneIdentityError::Unresponsive => AccessErrorCode::Unavailable,
            };
            AccessError::new(code, format!("interaction source: {error}"))
        })?;
    let mut current = host.clone();
    current.workspace = Some(pane.workspace_id);
    current.tab = Some(pane.tab_id);
    current.pane = Some(pane.pane_id);
    Ok(current)
}
