//! The Herdr Panes that show this worker's hosted Ticks. A view Pane runs this executable's
//! attach client with a one-time view grant; the Tick runs in the Tickhost whatever happens to the
//! Pane, and closing the Pane only disconnects that view. A worker knows the Panes it created by
//! the terminal each one shows, until that Pane closes.

use std::{
    collections::{BTreeMap, HashMap},
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
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use tickloop_agent_protocol::{
    ATTACH_ARGUMENTS, ATTACH_ENVIRONMENT, ChatId, HerdrPane, HostEndpointIdentity, HostedViewGrant,
};

/// How results name Herdr.
const HERDR_HOST: &str = "herdr";

pub struct HerdrViews {
    layout: HerdrLayout,
    attach_command: Vec<String>,
    /// The terminal of the Pane that shows each Tick, per Chat.
    terminals: Mutex<HashMap<(ChatId, InstanceRef), String>>,
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
            terminals: Mutex::new(HashMap::new()),
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

    /// The Pane that shows `chat`'s `instance` now; a Pane that closed is forgotten.
    pub(crate) async fn pane(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<Option<Pane>, HostError> {
        let key = (chat, instance.clone());
        let Some(terminal) = self.terminals.lock().await.get(&key).cloned() else {
            return Ok(None);
        };
        let pane = self.layout.terminal_pane(&terminal).await?;
        if pane.is_none() {
            self.terminals.lock().await.remove(&key);
        }
        Ok(pane)
    }

    /// Shows `chat`'s `instance` in a new Pane at `placement`, attached with `grant`, and says
    /// how far that got. A Pane Herdr created is never created again.
    pub(crate) async fn show(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
        grant: &HostedViewGrant,
        workspace: &str,
        placement: &Placement,
        cancel: CancellationToken,
    ) -> Value {
        let environment = match serde_json::to_string(grant) {
            Ok(grant) => BTreeMap::from([(ATTACH_ENVIRONMENT.to_owned(), grant)]),
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
                self.remember(chat, instance, &pane.terminal_id).await;
                shown(&pane, "shown")
            }
            Ok(CreatedView::NotPlaced { pane_id, error }) => {
                if let Ok(pane) = self.layout.pane(&pane_id).await {
                    self.remember(chat, instance, &pane.terminal_id).await;
                }
                json!({"status":"created_not_placed","host":HERDR_HOST,"pane_id":pane_id,"error":error,"diagnostic":"The view Pane was created, but its placement was not confirmed; inspect it before retrying. Do not open another view."})
            }
            Err(error) => not_created(&error),
        }
    }

    /// Moves the Pane `view`, which shows `chat`'s `instance`, to `placement`.
    pub(crate) async fn move_view(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
        view: &Pane,
        placement: &Placement,
        cancel: CancellationToken,
    ) -> Result<Value, HostError> {
        let moved = self.layout.move_view(view, placement, cancel).await?;
        self.remember(chat, instance, &moved.terminal_id).await;
        Ok(shown(&moved, "moved"))
    }

    /// Closes the Pane that shows `chat`'s `instance`, once the Tick ended; `None` when no Pane
    /// of this worker showed it.
    pub(crate) async fn close(&self, chat: ChatId, instance: &InstanceRef) -> Option<Value> {
        let terminal = self
            .terminals
            .lock()
            .await
            .remove(&(chat, instance.clone()))?;
        Some(match self.layout.close_view(&terminal).await {
            Ok(()) => json!({"status":"closed","host":HERDR_HOST}),
            Err(error) => {
                json!({"status":"not_closed","host":HERDR_HOST,"error":error,"diagnostic":"The Tick has ended; its view Pane shows that it ended until it is closed."})
            }
        })
    }

    /// The Panes of `listing`'s Workspace, marking each that shows one of `chat`'s Ticks, and
    /// the Chat's Ticks as the Tickhost lists them in `rows`, each with the Panes that show it.
    /// The views of Ticks `rows` no longer lists are forgotten.
    pub(crate) async fn read_workspace(
        &self,
        chat: ChatId,
        listing: WorkspaceListing,
        rows: Vec<TickRow>,
    ) -> Result<Value, HostError> {
        let running = rows
            .iter()
            .filter_map(|row| row.instance.clone())
            .collect::<std::collections::HashSet<_>>();
        self.terminals
            .lock()
            .await
            .retain(|(owner, instance), _| *owner != chat || running.contains(instance));
        let panes = self.layout.panes(listing.workspace.as_deref()).await?;
        let shown = self.shown(chat, &panes).await;
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
                    .filter(|pane| shown.get(&pane.terminal_id) == Some(&instance))
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
            .into_iter()
            .map(|pane| {
                json!({
                    "pane_id": pane.pane_id,
                    "tab_id": pane.tab_id,
                    "workspace_id": pane.workspace_id,
                    "cwd": pane.foreground_cwd.or(pane.cwd),
                    "view_of": shown.get(&pane.terminal_id),
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

    /// The Ticks of `chat` that `panes` show, by the terminal of each Pane.
    async fn shown(&self, chat: ChatId, panes: &[Pane]) -> HashMap<String, InstanceRef> {
        let terminals = self.terminals.lock().await;
        terminals
            .iter()
            .filter(|((owner, _), terminal)| {
                *owner == chat && panes.iter().any(|pane| &pane.terminal_id == *terminal)
            })
            .map(|((_, instance), terminal)| (terminal.clone(), instance.clone()))
            .collect()
    }

    async fn remember(&self, chat: ChatId, instance: &InstanceRef, terminal: &str) {
        self.terminals
            .lock()
            .await
            .insert((chat, instance.clone()), terminal.to_owned());
    }
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
