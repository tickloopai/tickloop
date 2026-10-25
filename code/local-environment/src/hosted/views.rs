//! The Herdr Panes that show this worker's hosted Ticks. A view Pane runs this executable's
//! attach client with a one-time view grant; the Tick runs in the Tickhost whatever happens to the
//! Pane, and closing the Pane only disconnects that view. While attached, a view Pane names the
//! Tick it shows in its own Herdr metadata, which is how a worker recognises its view Panes, also
//! the ones an earlier worker of the Agent created.

use std::{
    collections::{BTreeMap, HashSet},
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
};

use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickhost::host::{TickAdapter, TickRow, TickState};
use tickhost_connect::{
    herdr::Pane,
    host_error::HostError,
    layout::{CreatedView, HerdrLayout, Placement},
    page::PageCursor,
};
use tokio_util::sync::CancellationToken;

use tickloop_harness::protocol::{
    ATTACH_ARGUMENTS, ATTACH_ENVIRONMENT, HerdrPane, HostEndpointIdentity, HostedViewGrant,
    VIEW_PANE_ENVIRONMENT,
};

/// How results name Herdr.
const HERDR_HOST: &str = "herdr";

pub struct HerdrViews {
    layout: HerdrLayout,
    attach_command: Vec<String>,
}

/// A ReadWorkspace prepared for dispatch: the Workspace it lists, or every one, the View Pane
/// the input came from, and the page asked for.
pub(crate) struct WorkspaceListing {
    pub workspace: Option<String>,
    pub interaction: Option<Pane>,
    pub cursor: Option<PageCursor>,
    pub limit: Option<usize>,
}

impl HerdrViews {
    /// `executable` is this `tickloop`, whose attach client a view Pane runs.
    pub fn new(layout: HerdrLayout, executable: &Path) -> Self {
        Self {
            layout,
            attach_command: std::iter::once(executable.to_string_lossy().into_owned())
                .chain(ATTACH_ARGUMENTS.map(String::from))
                .collect(),
        }
    }

    pub fn layout(&self) -> &HerdrLayout {
        &self.layout
    }

    /// The Herdr this worker places views in, as a host authority: its socket, which must be
    /// this user's; `None` while no Herdr serves it.
    pub(crate) fn endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        let socket = &self.layout.context().socket;
        let metadata = match std::fs::symlink_metadata(socket) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot verify the Herdr endpoint: {error}")),
        };
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err("the Herdr endpoint must be a socket owned by this user".into());
        }
        Ok(Some(HostEndpointIdentity::HerdrSocket {
            path: socket
                .canonicalize()
                .map_err(|error| format!("cannot resolve the Herdr endpoint: {error}"))?,
            owner: metadata.uid(),
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }

    /// A Pane that shows `instance` now, in any Workspace.
    pub(crate) async fn pane(&self, instance: &InstanceRef) -> Result<Option<Pane>, HostError> {
        Ok(self.panes(instance).await?.into_iter().next())
    }

    /// Every Pane that shows `instance` now, in any Workspace.
    pub(crate) async fn panes(&self, instance: &InstanceRef) -> Result<Vec<Pane>, HostError> {
        self.layout.view_panes(instance.as_str()).await
    }

    /// Shows a Tick in a new Pane at `placement`, attached with `grant`, and says how far that
    /// got. A Pane Herdr created is never created again.
    pub(crate) async fn show(
        &self,
        grant: &HostedViewGrant,
        workspace: &str,
        placement: &Placement,
        cancel: CancellationToken,
    ) -> Value {
        let environment = match serde_json::to_string(grant) {
            Ok(grant) => BTreeMap::from([
                (ATTACH_ENVIRONMENT.to_owned(), grant),
                (VIEW_PANE_ENVIRONMENT.to_owned(), "1".to_owned()),
            ]),
            Err(error) => {
                return not_created(&HostError::before("InvalidGrant", error.to_string()));
            }
        };
        let created = self
            .layout
            .create_view(
                &self.attach_command,
                &environment,
                workspace,
                placement,
                cancel,
            )
            .await;
        match created {
            Ok(CreatedView::Placed(pane)) => {
                self.report(&pane.pane_id, grant.instance.as_str()).await;
                shown(&pane, "shown")
            }
            Ok(CreatedView::NotPlaced { pane_id, error }) => {
                self.report(&pane_id, grant.instance.as_str()).await;
                json!({"status":"created_not_placed","host":HERDR_HOST,"pane_id":pane_id,"error":error,"diagnostic":"The view Pane was created, but its placement was not confirmed; inspect it before retrying. Do not open another view."})
            }
            Err(error) => not_created(&error),
        }
    }

    /// Moves the view Pane `view` to `placement`.
    pub(crate) async fn move_view(
        &self,
        view: &Pane,
        placement: &Placement,
        cancel: CancellationToken,
    ) -> Result<Value, HostError> {
        let moved = self.layout.move_view(view, placement, cancel).await?;
        if let Some(instance) = view.view_of() {
            self.report(&moved.pane_id, instance).await;
        }
        Ok(shown(&moved, "moved"))
    }

    /// Names `instance` on a view Pane this worker just placed or moved, where a move may have
    /// dropped what the Pane's attach client reported; that client renews it, so a refused report
    /// only waits for the renewal.
    async fn report(&self, pane_id: &str, instance: &str) {
        let _ = self.layout.report_view(pane_id, instance).await;
    }

    /// Closes `views`, the Panes that showed a Tick before it ended; `None` when there were none.
    pub(crate) async fn close(&self, views: &[Pane]) -> Option<Value> {
        if views.is_empty() {
            return None;
        }
        let mut failures = Vec::new();
        for view in views {
            if let Err(error) = self.layout.close_view(&view.terminal_id).await {
                failures.push(error);
            }
        }
        Some(if failures.is_empty() {
            json!({"status":"closed","host":HERDR_HOST})
        } else {
            json!({"status":"not_closed","host":HERDR_HOST,"error":failures[0],"diagnostic":"The Tick has ended; its view Pane shows that it ended until it is closed."})
        })
    }

    /// The Panes of `listing`'s Workspace, marking each that shows one of the Chat's Ticks, and
    /// the Chat's Ticks as the Tickhost lists them in `rows`, each with the Panes that show it.
    pub(crate) async fn read_workspace(
        &self,
        listing: WorkspaceListing,
        rows: Vec<TickRow>,
    ) -> Result<Value, HostError> {
        let running = rows
            .iter()
            .filter_map(|row| Some(row.instance.as_ref()?.as_str().to_owned()))
            .collect::<HashSet<_>>();
        let panes = self.layout.panes(listing.workspace.as_deref()).await?;
        let workspaces = panes
            .iter()
            .map(|pane| pane.workspace_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let ticks = rows
            .into_iter()
            .filter_map(|row| {
                let instance = row.instance?;
                let view_panes = panes
                    .iter()
                    .filter(|pane| view_of(pane, &running) == Some(instance.as_str()))
                    .map(|pane| pane.pane_id.clone())
                    .collect::<Vec<_>>();
                Some(json!({
                    "tick_instance_id": instance,
                    "title": row.title,
                    "source": match row.adapter {
                        TickAdapter::Protocol => "protocol",
                        TickAdapter::Terminal => "terminal",
                    },
                    "state": match row.state {
                        TickState::Opening => "initializing",
                        TickState::Running => "live",
                        TickState::Exited => "exited",
                        TickState::Closing => "closing",
                    },
                    "views": row.views,
                    "view_panes": view_panes,
                }))
            })
            .collect::<Vec<_>>();
        let caller = match &listing.interaction {
            Some(expected) => self
                .layout
                .pane(&expected.pane_id)
                .await
                .ok()
                .filter(|pane| {
                    pane.terminal_id == expected.terminal_id
                        && pane.tab_id == expected.tab_id
                        && pane.workspace_id == expected.workspace_id
                }),
            None => None,
        };
        let panes = panes
            .iter()
            .map(|pane| {
                json!({
                    "pane_id": pane.pane_id,
                    "tab_id": pane.tab_id,
                    "workspace_id": pane.workspace_id,
                    "cwd": pane.foreground_cwd.as_ref().or(pane.cwd.as_ref()),
                    "view_of": view_of(pane, &running),
                })
            })
            .collect::<Vec<_>>();
        let page = tickhost_connect::page::paginate(
            panes,
            |pane| pane["pane_id"].as_str().unwrap_or_default().to_owned(),
            listing.cursor.as_ref(),
            listing.limit,
        )
        .map_err(|error| HostError::before(error.code(), error.to_string()))?;
        Ok(json!({
            "host": HERDR_HOST,
            "host_instance": self.layout.context().socket,
            "workspace_id": listing.workspace,
            "workspaces": workspaces,
            "caller": caller.map(|caller| json!({"pane_id":caller.pane_id,"tab_id":caller.tab_id,"workspace_id":caller.workspace_id})),
            "panes": page.items,
            "total": page.total,
            "generation": page.generation,
            "next_cursor": page.next,
            "ticks": ticks,
        }))
    }
}

/// The Tick of `running` that `pane` shows a view of. Only those are named; another Chat's view
/// Pane is a plain Pane to this Chat.
fn view_of<'pane>(pane: &'pane Pane, running: &HashSet<String>) -> Option<&'pane str> {
    pane.view_of()
        .filter(|instance| running.contains(*instance))
}

/// The interaction Pane a call came with, as the layout takes it.
pub(crate) fn interaction_pane(pane: HerdrPane) -> Pane {
    let HerdrPane {
        pane_id,
        terminal_id,
        workspace_id,
        tab_id,
        cwd,
        foreground_cwd,
    } = pane;
    Pane {
        pane_id,
        terminal_id,
        workspace_id,
        tab_id,
        cwd,
        foreground_cwd,
        tokens: BTreeMap::new(),
    }
}

fn shown(pane: &Pane, status: &str) -> Value {
    json!({"status":status,"host":HERDR_HOST,"pane_id":pane.pane_id,"terminal_id":pane.terminal_id,"tab_id":pane.tab_id,"workspace_id":pane.workspace_id})
}

/// No view Pane is known to exist: `error` came before Herdr created one, or Herdr may have.
pub(crate) fn not_created(error: &HostError) -> Value {
    if error.dispatched {
        json!({"status":"unknown","host":HERDR_HOST,"error":error,"diagnostic":"Herdr may have created a view Pane; inspect the Workspace before retrying. The Tick runs whether or not a view shows it."})
    } else {
        json!({"status":"failed","host":HERDR_HOST,"error":error,"diagnostic":"No view Pane was created; the Tick runs without one."})
    }
}
