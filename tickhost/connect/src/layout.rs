//! Herdr as the layout host of Tick views: its Workspaces, Tabs and Panes, the place a view Pane
//! is created at or moved to, and the Panes themselves. A view Pane runs an attach client of a
//! Tick that runs in the Tickhost; nothing here tells a Tick by what a Pane runs, and closing a
//! view Pane closes only that view.

use std::{collections::BTreeMap, os::unix::fs::MetadataExt, sync::Arc};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    herdr::{HerdrContext, HerdrPort, Pane, SocketHerdr, current_pane, report_view},
    host_error::HostError,
};

const DEFAULT_DIRECTION: &str = "right";

/// Where an operation relative to the Agent starts: its Work Context's Workspace and the View
/// Pane carrying the input, when either is known.
#[derive(Clone, Debug, Default)]
pub struct LayoutContext {
    pub workspace_id: Option<String>,
    pub interaction: Option<Pane>,
}

/// Where a view Pane goes, frozen before approval: a new Tab of the Workspace, or beside one of
/// its Panes.
#[derive(Clone, Debug)]
pub enum Placement {
    NewTab,
    Beside {
        destination: Box<Pane>,
        direction: &'static str,
    },
}

impl Placement {
    /// The placement as an approval and a result name it.
    pub fn summary(&self) -> Value {
        match self {
            Self::NewTab => json!({"type":"new_tab"}),
            Self::Beside {
                destination,
                direction,
            } => {
                json!({"type":"pane","pane_id":destination.pane_id,"terminal_id":destination.terminal_id,"tab_id":destination.tab_id,"workspace_id":destination.workspace_id,"direction":direction})
            }
        }
    }

    fn holds(&self, pane: &Pane) -> bool {
        match self {
            Self::NewTab => true,
            Self::Beside { destination, .. } => {
                pane.workspace_id == destination.workspace_id && pane.tab_id == destination.tab_id
            }
        }
    }
}

/// A view Pane Herdr created: at the approved place, or not confirmed there for `error`.
#[derive(Debug)]
pub enum CreatedView {
    Placed(Pane),
    NotPlaced { pane_id: String, error: HostError },
}

pub struct HerdrLayout {
    host: Arc<dyn HerdrPort>,
    context: HerdrContext,
}

impl HerdrLayout {
    pub fn new(context: HerdrContext) -> Self {
        Self::with_host(context.clone(), Arc::new(SocketHerdr::new(context)))
    }

    pub fn with_host(context: HerdrContext, host: Arc<dyn HerdrPort>) -> Self {
        Self { host, context }
    }

    pub fn context(&self) -> &HerdrContext {
        &self.context
    }

    /// The device and inode of the Herdr socket, which change when another Herdr serves it.
    pub fn socket_identity(&self) -> Option<(u64, u64)> {
        std::fs::metadata(&self.context.socket)
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino()))
    }

    /// The Pane this process runs in.
    pub async fn caller(&self) -> Result<Pane, HostError> {
        if self.context.caller_pane.is_empty() {
            return Err(HostError::before(
                "MissingAnchor",
                "this Agent has no interaction View",
            ));
        }
        current_pane(self.host.as_ref(), &self.context.caller_pane).await
    }

    /// Every Pane of `workspace`, or of every Workspace.
    pub async fn panes(&self, workspace: Option<&str>) -> Result<Vec<Pane>, HostError> {
        let response = self
            .host
            .request(
                "pane.list",
                json!({"workspace_id":workspace}),
                CancellationToken::new(),
            )
            .await?;
        serde_json::from_value(response["panes"].clone()).map_err(invalid_host_response)
    }

    pub async fn pane(&self, pane_id: &str) -> Result<Pane, HostError> {
        let response = self
            .host
            .request(
                "pane.get",
                json!({"pane_id":pane_id}),
                CancellationToken::new(),
            )
            .await?;
        serde_json::from_value(response["pane"].clone()).map_err(invalid_host_response)
    }

    /// The Panes in any Workspace that report showing a view of `instance`.
    pub async fn view_panes(&self, instance: &str) -> Result<Vec<Pane>, HostError> {
        Ok(self
            .panes(None)
            .await?
            .into_iter()
            .filter(|pane| pane.view_of() == Some(instance))
            .collect())
    }

    /// Reports on Pane `pane_id` that it shows a view of `instance`. A Pane's metadata does not
    /// follow it when a move gives it another id, so a view placed or moved is reported at once
    /// rather than at its own next renewal.
    pub async fn report_view(&self, pane_id: &str, instance: &str) -> Result<(), HostError> {
        report_view(self.host.as_ref(), pane_id, Some(instance)).await
    }

    /// The Pane that shows `terminal_id` now, wherever it was moved; `None` once it closed.
    pub async fn terminal_pane(&self, terminal_id: &str) -> Result<Option<Pane>, HostError> {
        Ok(self
            .panes(None)
            .await?
            .into_iter()
            .find(|pane| pane.terminal_id == terminal_id))
    }

    /// The Workspace an operation with `input` acts in: the named one; for an operation
    /// `relative_to_view`, the View's; else the Work Context's; else the Workspace of the Pane
    /// its placement names. `None` when nothing names one.
    pub async fn workspace(
        &self,
        input: &Value,
        context: &LayoutContext,
        relative_to_view: bool,
    ) -> Result<Option<String>, HostError> {
        match input.get("workspace_id") {
            Some(Value::String(workspace)) if !workspace.is_empty() => {
                return Ok(Some(workspace.clone()));
            }
            Some(_) => {
                return Err(HostError::before(
                    "InvalidArguments",
                    "workspace_id must be a nonempty string",
                ));
            }
            None => {}
        }
        if relative_to_view && let Some(view) = &context.interaction {
            return Ok(Some(view.workspace_id.clone()));
        }
        if let Some(workspace) = &context.workspace_id {
            return Ok(Some(workspace.clone()));
        }
        match input["placement"]["pane_id"].as_str() {
            Some(pane_id) if input["placement"]["type"] == "pane" => {
                Ok(Some(self.pane(pane_id).await?.workspace_id))
            }
            _ => Ok(None),
        }
    }

    /// The placement `input` names in `workspace`, frozen before approval. A move has no new
    /// Tab and defaults to the View's Tab; `current` is the view Pane it would move.
    pub async fn prepare_placement(
        &self,
        input: &Value,
        workspace: &str,
        is_open: bool,
        current: Option<&Pane>,
        context: &LayoutContext,
    ) -> Result<Placement, HostError> {
        let placement = input.get("placement");
        let kind = match placement {
            None => "current_tab",
            Some(Value::Object(object)) => {
                object.get("type").and_then(Value::as_str).ok_or_else(|| {
                    HostError::before("InvalidArguments", "placement.type is required")
                })?
            }
            _ => {
                return Err(HostError::before(
                    "InvalidArguments",
                    "placement must be an object",
                ));
            }
        };
        let object = placement.and_then(Value::as_object);
        if input.get("destination_workspace_id").is_some() {
            return Err(HostError::before(
                "UnsupportedCapability",
                "moving a Tick view across Workspaces is not supported",
            ));
        }
        let direction = match object.and_then(|object| object.get("direction")) {
            None => DEFAULT_DIRECTION,
            Some(Value::String(direction)) => match direction.as_str() {
                "left" => "left",
                "right" => "right",
                "up" => "up",
                "down" => "down",
                _ => return Err(invalid_direction()),
            },
            _ => return Err(invalid_direction()),
        };
        if kind == "new_tab" {
            if !is_open
                || object.is_some_and(|object| {
                    object.get("pane_id").is_some() || object.get("direction").is_some()
                })
            {
                return Err(HostError::before(
                    "InvalidArguments",
                    "new_tab is only valid for open and has no pane_id or direction",
                ));
            }
            return Ok(Placement::NewTab);
        }
        let destination = match kind {
            "current_tab" => {
                if object.is_some_and(|object| object.get("pane_id").is_some()) {
                    return Err(HostError::before(
                        "InvalidArguments",
                        "current_tab cannot specify pane_id",
                    ));
                }
                context.interaction.clone().ok_or_else(|| {
                    HostError::before(
                        "MissingAnchor",
                        "this operation has no interaction View; specify a target Pane or use new_tab",
                    )
                })?
            }
            "pane" => {
                let pane_id = object
                    .and_then(|object| object.get("pane_id"))
                    .and_then(Value::as_str)
                    .filter(|pane_id| !pane_id.is_empty())
                    .ok_or_else(|| {
                        HostError::before("InvalidArguments", "placement.pane_id is required")
                    })?;
                self.pane(pane_id).await?
            }
            _ => {
                return Err(HostError::before(
                    "InvalidArguments",
                    "placement.type must be new_tab, current_tab or pane",
                ));
            }
        };
        if destination.workspace_id != workspace {
            return Err(HostError::before(
                "ScopeRequired",
                "destination Pane must be in the selected Workspace",
            ));
        }
        if current.is_some_and(|current| current.terminal_id == destination.terminal_id) {
            return Err(HostError::before(
                "InvalidArguments",
                "cannot move a Tick view beside itself",
            ));
        }
        Ok(Placement::Beside {
            destination: Box::new(destination),
            direction,
        })
    }

    /// Checks that the Pane a placement is relative to still shows what it showed when the
    /// placement was approved.
    pub async fn revalidate_placement(&self, placement: &Placement) -> Result<(), HostError> {
        if let Placement::Beside { destination, .. } = placement {
            let current = self.pane(&destination.pane_id).await.map_err(|_| {
                HostError::before(
                    "TargetChanged",
                    "destination Pane disappeared after approval",
                )
            })?;
            if current.terminal_id != destination.terminal_id
                || current.tab_id != destination.tab_id
                || current.workspace_id != destination.workspace_id
            {
                return Err(HostError::before(
                    "TargetChanged",
                    "destination Pane moved or changed after approval",
                ));
            }
        }
        Ok(())
    }

    /// Creates a Pane running `command` with `environment` in `workspace` and moves it to
    /// `placement`. Once Herdr created it, every later failure leaves it where it is and says
    /// so; it is never created twice.
    pub async fn create_view(
        &self,
        command: &[String],
        environment: &BTreeMap<String, String>,
        workspace: &str,
        placement: &Placement,
        cancel: CancellationToken,
    ) -> Result<CreatedView, HostError> {
        let response = self.host.request("layout.apply", json!({"workspace_id":workspace,"focus":false,"root":{"type":"pane","command":command,"env":environment}}), cancel.clone()).await?;
        let pane_id = response["layout"]["root"]["pane_id"]
            .as_str()
            .ok_or_else(|| {
                HostError::after(
                    "OutcomeUnknown",
                    "Herdr created a layout but returned no Pane identity; inspect the Workspace before retrying",
                )
            })?
            .to_owned();
        let not_placed = |pane_id: &str, error: HostError| CreatedView::NotPlaced {
            pane_id: pane_id.to_owned(),
            error,
        };
        let staged = match self.pane(&pane_id).await {
            Ok(staged) if staged.workspace_id == workspace => staged,
            Ok(_) => {
                return Ok(not_placed(
                    &pane_id,
                    HostError::before(
                        "TargetChanged",
                        "created Pane was not in the approved Workspace",
                    ),
                ));
            }
            Err(error) => return Ok(not_placed(&pane_id, error)),
        };
        if matches!(placement, Placement::NewTab) {
            return Ok(CreatedView::Placed(staged));
        }
        if let Err(error) = self.revalidate_placement(placement).await {
            return Ok(not_placed(&pane_id, error));
        }
        if cancel.is_cancelled() {
            return Ok(not_placed(
                &pane_id,
                HostError::before("Cancelled", "placement cancelled after Pane creation"),
            ));
        }
        let moved = match self
            .request_move(&pane_id, placement, cancel.clone())
            .await
            .and_then(|moved| moved_pane(&moved))
        {
            Ok(moved) => moved,
            Err(error) => return Ok(not_placed(&pane_id, error)),
        };
        if moved.terminal_id != staged.terminal_id || !placement.holds(&moved) {
            return Ok(not_placed(
                &pane_id,
                HostError::after(
                    "OutcomeUnknown",
                    "Herdr move reply did not identify the created Pane at the approved destination",
                ),
            ));
        }
        if let Err(error) = self.orient(&moved, placement, cancel).await {
            return Ok(not_placed(&moved.pane_id, error));
        }
        Ok(CreatedView::Placed(moved))
    }

    /// Moves the view Pane `view` to `placement` and returns where it is now.
    pub async fn move_view(
        &self,
        view: &Pane,
        placement: &Placement,
        cancel: CancellationToken,
    ) -> Result<Pane, HostError> {
        self.revalidate_placement(placement).await?;
        let mut source = view.clone();
        let staged = placement.holds(view);
        if staged {
            // Herdr treats moves within one Tab as no-ops; keep the Pane in a temporary Tab.
            let response = self.host.request("pane.move", json!({"pane_id":source.pane_id,"focus":false,"destination":{"type":"new_tab","workspace_id":view.workspace_id}}), cancel.clone()).await?;
            let pane = moved_pane(&response)?;
            if pane.terminal_id != view.terminal_id
                || pane.workspace_id != view.workspace_id
                || placement.holds(&pane)
            {
                return Err(HostError::after(
                    "OutcomeUnknown",
                    "Herdr did not confirm the same view Pane in a temporary Tab; inspect before retrying",
                ));
            }
            self.confirm_view(&pane).await?;
            source = pane;
            self.revalidate_placement(placement)
                .await
                .map_err(|mut error| {
                    error.dispatched = true;
                    error.message = format!(
                        "{}; the view is in temporary Pane {}; inspect before retrying",
                        error.message, source.pane_id
                    );
                    error
                })?;
        }
        let moved = self
            .request_move(&source.pane_id, placement, cancel.clone())
            .await
            .map_err(|mut error| {
                error.dispatched |= staged;
                error.message = format!(
                    "{}; the view was last seen in Pane {}; inspect its location before retrying",
                    error.message, source.pane_id
                );
                error
            })?;
        let moved = moved_pane(&moved)?;
        if moved.terminal_id != view.terminal_id || !placement.holds(&moved) {
            return Err(HostError::after(
                "OutcomeUnknown",
                format!(
                    "Herdr accepted the move of Pane {}, but reported another terminal or destination; inspect before retrying",
                    source.pane_id
                ),
            ));
        }
        self.confirm_view(&moved).await?;
        self.orient(&moved, placement, cancel).await?;
        Ok(moved)
    }

    /// Closes the Pane that shows `terminal_id`, wherever it is now; a Pane already gone stays
    /// gone.
    pub async fn close_view(&self, terminal_id: &str) -> Result<(), HostError> {
        let Some(pane) = self.terminal_pane(terminal_id).await? else {
            return Ok(());
        };
        self.host
            .request(
                "pane.close",
                json!({"pane_id":pane.pane_id}),
                CancellationToken::new(),
            )
            .await
            .map(drop)
    }

    /// Fails unless `expected` still names the view's Pane where it was just moved.
    async fn confirm_view(&self, expected: &Pane) -> Result<(), HostError> {
        let confirmed = self.pane(&expected.pane_id).await.and_then(|current| {
            if current.terminal_id == expected.terminal_id
                && current.workspace_id == expected.workspace_id
                && current.tab_id == expected.tab_id
            {
                Ok(())
            } else {
                Err(HostError::before(
                    "TargetChanged",
                    "the view Pane changed after the preceding move",
                ))
            }
        });
        confirmed.map_err(|mut error| {
            error.dispatched = true;
            error.message = format!(
                "{}; the view was last seen in Pane {} of Tab {}; inspect before retrying",
                error.message, expected.pane_id, expected.tab_id
            );
            error
        })
    }

    async fn request_move(
        &self,
        source_pane_id: &str,
        placement: &Placement,
        cancel: CancellationToken,
    ) -> Result<Value, HostError> {
        let Placement::Beside {
            destination,
            direction,
        } = placement
        else {
            return Err(HostError::before(
                "InvalidArguments",
                "a move requires an existing destination Pane",
            ));
        };
        let split = match *direction {
            "left" | "right" => "right",
            _ => "down",
        };
        self.host.request("pane.move", json!({"pane_id":source_pane_id,"focus":false,"destination":{"type":"tab","tab_id":destination.tab_id,"target_pane_id":destination.pane_id,"split":split}}), cancel).await
    }

    /// Puts `source`, already in the destination Tab, on the requested side of its anchor.
    async fn orient(
        &self,
        source: &Pane,
        placement: &Placement,
        cancel: CancellationToken,
    ) -> Result<(), HostError> {
        let Placement::Beside {
            destination,
            direction,
        } = placement
        else {
            return Ok(());
        };
        let initial_direction = match *direction {
            "left" => "right",
            "up" => "down",
            direction => direction,
        };
        let result = async {
            self.revalidate_placement(placement).await?;
            let current = self.pane(&source.pane_id).await?;
            if current.terminal_id != source.terminal_id || !placement.holds(&current) {
                return Err(HostError::before(
                    "TargetChanged",
                    "the view Pane changed before arranging its position",
                ));
            }
            self.verify_neighbor(&destination.pane_id, initial_direction, &source.pane_id)
                .await?;
            if initial_direction == *direction {
                return Ok(());
            }
            if cancel.is_cancelled() {
                return Err(HostError::before(
                    "Cancelled",
                    "positioning cancelled after Pane move",
                ));
            }
            // Herdr swap focuses its source; keep the anchor (the Agent for current_tab) focused.
            let response = self
                .host
                .request(
                    "pane.swap",
                    json!({"source_pane_id":destination.pane_id,"target_pane_id":source.pane_id}),
                    cancel,
                )
                .await?;
            if response["swap"]["changed"] != true
                || response["swap"]["source_pane_id"] != destination.pane_id
                || response["swap"]["target_pane_id"] != source.pane_id
            {
                return Err(HostError::before(
                    "PlacementNotApplied",
                    format!(
                        "Herdr did not confirm the requested position: {}",
                        response["swap"]
                    ),
                ));
            }
            self.revalidate_placement(placement).await?;
            let current = self.pane(&source.pane_id).await?;
            if current.terminal_id != source.terminal_id || !placement.holds(&current) {
                return Err(HostError::before(
                    "TargetChanged",
                    "the view Pane changed while arranging its position",
                ));
            }
            self.verify_neighbor(&destination.pane_id, direction, &source.pane_id)
                .await
        }
        .await;
        result.map_err(|mut error: HostError| {
            error.dispatched = true;
            error.message = format!(
                "{}; Pane {} was already moved into the target Tab, but its requested side was not confirmed; inspect before retrying",
                error.message, source.pane_id
            );
            error
        })
    }

    async fn verify_neighbor(
        &self,
        anchor: &str,
        direction: &str,
        source: &str,
    ) -> Result<(), HostError> {
        let response = self
            .host
            .request(
                "pane.neighbor",
                json!({"pane_id":anchor,"direction":direction}),
                CancellationToken::new(),
            )
            .await?;
        if response["neighbor"]["pane_id"] != anchor
            || response["neighbor"]["direction"] != direction
            || response["neighbor"]["neighbor_pane_id"] != source
        {
            return Err(HostError::before(
                "TargetChanged",
                "Pane adjacency no longer matches the requested placement",
            ));
        }
        Ok(())
    }
}

fn moved_pane(response: &Value) -> Result<Pane, HostError> {
    if response["move_result"]["changed"] == false {
        return Err(HostError::after(
            "PlacementNotApplied",
            format!(
                "Herdr left the Pane unchanged: {}; inspect its location before retrying",
                response["move_result"]["reason"]
            ),
        ));
    }
    serde_json::from_value(response["move_result"]["pane"].clone()).map_err(|error| {
        HostError::after(
            "OutcomeUnknown",
            format!(
                "Herdr accepted the Pane move but returned no valid Pane identity: {error}; inspect before retrying"
            ),
        )
    })
}

fn invalid_direction() -> HostError {
    HostError::before(
        "InvalidArguments",
        "placement.direction must be left, right, up or down",
    )
}

fn invalid_host_response(error: serde_json::Error) -> HostError {
    HostError::before(
        "ProtocolViolation",
        format!("invalid Herdr response: {error}"),
    )
}
