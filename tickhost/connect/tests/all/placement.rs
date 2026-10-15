//! Placing, moving and orienting view Panes through a controllable Herdr.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickhost_connect::{
    herdr::{HerdrContext, HerdrPort, Pane},
    host_error::HostError,
    layout::{CreatedView, HerdrLayout, LayoutContext, Placement},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct State {
    panes: BTreeMap<String, Pane>,
    calls: Vec<(String, Value)>,
    fail_move: bool,
    move_destination_after_launch: bool,
    change_pane_id_on_move: bool,
    neighbors: BTreeMap<(String, String), String>,
    fail_swap: bool,
    swap_unchanged: bool,
    fail_move_number: Option<usize>,
    change_destination_after_move: bool,
    move_unchanged: bool,
    replace_terminal_after_staging: bool,
    lose_adjacency_after_move: bool,
}

#[derive(Default)]
struct FakeHerdr(Mutex<State>);

impl FakeHerdr {
    async fn insert(&self, pane: Pane) {
        self.0.lock().await.panes.insert(pane.pane_id.clone(), pane);
    }

    async fn update(&self, pane_id: &str, update: impl FnOnce(&mut Pane)) {
        update(self.0.lock().await.panes.get_mut(pane_id).unwrap());
    }

    async fn calls(&self, method: &str) -> Vec<Value> {
        self.0
            .lock()
            .await
            .calls
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, params)| params.clone())
            .collect()
    }
}

#[async_trait]
impl HerdrPort for FakeHerdr {
    async fn request(
        &self,
        method: &str,
        params: Value,
        _: CancellationToken,
    ) -> Result<Value, HostError> {
        let mut state = self.0.lock().await;
        state.calls.push((method.to_owned(), params.clone()));
        match method {
            "pane.get" => state
                .panes
                .get(params["pane_id"].as_str().unwrap())
                .map(|pane| json!({"pane":pane}))
                .ok_or_else(|| HostError::before("TargetGone", "Pane missing")),
            "pane.list" => Ok(
                json!({"panes":state.panes.values().filter(|pane| params["workspace_id"].is_null() || params["workspace_id"] == pane.workspace_id).collect::<Vec<_>>()}),
            ),
            "layout.apply" => {
                let workspace = params["workspace_id"].as_str().unwrap();
                state
                    .panes
                    .insert("staged".into(), pane("staged", "new-tab", workspace));
                if state.move_destination_after_launch {
                    state.panes.get_mut("agent").unwrap().tab_id = "changed-tab".into();
                }
                Ok(json!({"layout":{"root":{"pane_id":"staged"}}}))
            }
            "pane.move" => {
                let move_number = state
                    .calls
                    .iter()
                    .filter(|(name, _)| name == "pane.move")
                    .count();
                if state.fail_move || state.fail_move_number == Some(move_number) {
                    return Err(HostError {
                        code: "OutcomeUnknown".into(),
                        message: "connection lost".into(),
                        dispatched: true,
                    });
                }
                let source = params["pane_id"].as_str().unwrap();
                let is_staging = params["destination"]["type"] == "new_tab";
                let tab = if is_staging {
                    "temporary-tab".to_owned()
                } else {
                    params["destination"]["tab_id"].as_str().unwrap().to_owned()
                };
                if state.panes[source].tab_id == tab || state.move_unchanged {
                    return Ok(
                        json!({"move_result":{"changed":false,"reason":"same_tab","pane":state.panes[source]}}),
                    );
                }
                let mut moved = state.panes.remove(source).unwrap();
                moved.tab_id = tab;
                if state.change_pane_id_on_move {
                    moved.pane_id = format!("moved-{source}");
                }
                if !is_staging {
                    let anchor = params["destination"]["target_pane_id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let direction = params["destination"]["split"].as_str().unwrap().to_owned();
                    state
                        .neighbors
                        .insert((anchor, direction), moved.pane_id.clone());
                    if state.lose_adjacency_after_move {
                        state.neighbors.clear();
                    }
                } else if state.replace_terminal_after_staging {
                    moved.terminal_id = "replacement".into();
                }
                state.panes.insert(moved.pane_id.clone(), moved.clone());
                if state.change_destination_after_move {
                    state.panes.get_mut("agent").unwrap().terminal_id = "replaced-agent".into();
                }
                Ok(json!({"move_result":{"changed":true,"pane":moved}}))
            }
            "pane.close" => {
                let pane_id = params["pane_id"].as_str().unwrap();
                state.panes.retain(|_, pane| pane.pane_id != pane_id);
                Ok(json!({}))
            }
            "pane.neighbor" => {
                let anchor = params["pane_id"].as_str().unwrap();
                let direction = params["direction"].as_str().unwrap();
                Ok(
                    json!({"neighbor":{"pane_id":anchor,"direction":direction,"neighbor_pane_id":state.neighbors.get(&(anchor.into(), direction.into()))}}),
                )
            }
            "pane.swap" => {
                if state.fail_swap {
                    return Err(HostError::before("SwapFailed", "swap rejected"));
                }
                let anchor = params["source_pane_id"].as_str().unwrap();
                let source = params["target_pane_id"].as_str().unwrap();
                if !state.swap_unchanged {
                    let key = state
                        .neighbors
                        .iter()
                        .find(|((pane, _), neighbor)| pane == anchor && *neighbor == source)
                        .map(|(key, _)| key.clone())
                        .unwrap();
                    state.neighbors.remove(&key);
                    let opposite = if key.1 == "right" { "left" } else { "up" };
                    state
                        .neighbors
                        .insert((anchor.into(), opposite.into()), source.into());
                }
                Ok(
                    json!({"swap":{"changed":!state.swap_unchanged,"source_pane_id":anchor,"target_pane_id":source}}),
                )
            }
            _ => panic!("unexpected Herdr method {method}"),
        }
    }
}

fn pane(id: &str, tab: &str, workspace: &str) -> Pane {
    Pane {
        pane_id: id.into(),
        terminal_id: format!("terminal-{id}"),
        workspace_id: workspace.into(),
        tab_id: tab.into(),
        cwd: Some("/tmp".into()),
        foreground_cwd: None,
        tokens: Default::default(),
    }
}

/// A Workspace with the Agent's View, an existing view Pane and another Pane, each in its own
/// Tab.
async fn fixture() -> (HerdrLayout, Arc<FakeHerdr>) {
    let host = Arc::new(FakeHerdr::default());
    host.insert(pane("agent", "agent-tab", "w1")).await;
    host.insert(pane("view", "other-tab", "w1")).await;
    host.insert(pane("anchor", "anchor-tab", "w1")).await;
    let layout = HerdrLayout::with_host(HerdrContext::without_view(PathBuf::new()), host.clone());
    (layout, host)
}

/// The context of an input the Agent's View carried.
async fn from_view(host: &FakeHerdr) -> LayoutContext {
    LayoutContext {
        workspace_id: Some("w1".into()),
        interaction: Some(host.0.lock().await.panes["agent"].clone()),
    }
}

async fn placement(
    layout: &HerdrLayout,
    host: &FakeHerdr,
    is_open: bool,
    placement: Value,
) -> Result<Placement, HostError> {
    let current = host.0.lock().await.panes.get("view").cloned();
    let mut input = json!({});
    if !placement.is_null() {
        input["placement"] = placement;
    }
    layout
        .prepare_placement(
            &input,
            "w1",
            is_open,
            current.as_ref().filter(|_| !is_open),
            &from_view(host).await,
        )
        .await
}

const VIEW_COMMAND: [&str; 3] = ["/bin/tickloop", "monitor", "attach"];

async fn create(layout: &HerdrLayout, placement: &Placement) -> CreatedView {
    let command = VIEW_COMMAND.map(String::from);
    let environment = BTreeMap::from([("TICKLOOP_MONITOR_ATTACH".to_owned(), "{}".to_owned())]);
    layout
        .create_view(
            &command,
            &environment,
            "w1",
            placement,
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

async fn move_view(
    layout: &HerdrLayout,
    host: &FakeHerdr,
    placement: &Placement,
) -> Result<Pane, HostError> {
    let view = host.0.lock().await.panes["view"].clone();
    layout
        .move_view(&view, placement, CancellationToken::new())
        .await
}

fn placed(created: CreatedView) -> Pane {
    match created {
        CreatedView::Placed(pane) => pane,
        CreatedView::NotPlaced { pane_id, error } => panic!("{pane_id} not placed: {error}"),
    }
}

fn not_placed(created: CreatedView) -> (String, HostError) {
    match created {
        CreatedView::NotPlaced { pane_id, error } => (pane_id, error),
        CreatedView::Placed(pane) => panic!("{} was placed", pane.pane_id),
    }
}

#[tokio::test]
async fn a_view_opened_left_of_the_agent_moves_beside_it_then_swaps() {
    let (layout, host) = fixture().await;
    let placement = placement(
        &layout,
        &host,
        true,
        json!({"type":"current_tab","direction":"left"}),
    )
    .await
    .unwrap();
    let view = placed(create(&layout, &placement).await);
    assert_eq!(view.tab_id, "agent-tab");
    let applied = &host.calls("layout.apply").await[0];
    assert_eq!(applied["focus"], false);
    assert_eq!(applied["root"]["command"], json!(VIEW_COMMAND));
    assert_eq!(applied["root"]["env"]["TICKLOOP_MONITOR_ATTACH"], "{}");
    assert_eq!(
        host.calls("pane.move").await[0]["destination"]["split"],
        "right"
    );
    assert_eq!(
        host.calls("pane.swap").await,
        [json!({"source_pane_id":"agent","target_pane_id":"staged"})]
    );
}

#[tokio::test]
async fn a_view_opened_in_a_new_tab_stays_where_herdr_created_it() {
    let (layout, host) = fixture().await;
    let placement = placement(&layout, &host, true, json!({"type":"new_tab"}))
        .await
        .unwrap();
    let view = placed(create(&layout, &placement).await);
    assert_eq!(view.pane_id, "staged");
    assert_eq!(view.tab_id, "new-tab");
    assert!(host.calls("pane.move").await.is_empty());
}

#[tokio::test]
async fn every_direction_repositions_a_view_already_in_the_target_tab() {
    for direction in ["left", "right", "up", "down"] {
        let (layout, host) = fixture().await;
        host.update("view", |pane| pane.tab_id = "agent-tab".into())
            .await;
        let placement = placement(
            &layout,
            &host,
            false,
            json!({"type":"current_tab","direction":direction}),
        )
        .await
        .unwrap();
        assert_eq!(placement.summary()["direction"], direction);
        let moved = move_view(&layout, &host, &placement).await.unwrap();
        assert_eq!(moved.pane_id, "view");
        assert_eq!(moved.tab_id, "agent-tab");
        let moves = host.calls("pane.move").await;
        assert_eq!(moves.len(), 2);
        assert_eq!(moves[0]["destination"]["type"], "new_tab");
        assert_eq!(moves[1]["destination"]["target_pane_id"], "agent");
        assert!(moves.iter().all(|params| params["focus"] == false));
        assert_eq!(
            host.calls("pane.swap").await.len(),
            usize::from(matches!(direction, "left" | "up"))
        );
        let neighbors = &host.0.lock().await.neighbors;
        assert_eq!(
            neighbors.get(&("agent".into(), direction.into())),
            Some(&"view".into())
        );
    }
}

#[tokio::test]
async fn a_view_opened_above_a_named_pane_moves_down_then_swaps() {
    let (layout, host) = fixture().await;
    let placement = placement(
        &layout,
        &host,
        true,
        json!({"type":"pane","pane_id":"anchor","direction":"up"}),
    )
    .await
    .unwrap();
    let view = placed(create(&layout, &placement).await);
    assert_eq!(view.tab_id, "anchor-tab");
    assert_eq!(
        host.calls("pane.move").await[0]["destination"]["split"],
        "down"
    );
    assert_eq!(
        host.calls("pane.swap").await[0],
        json!({"source_pane_id":"anchor","target_pane_id":"staged"})
    );
}

#[tokio::test]
async fn a_failed_or_unchanged_swap_reports_the_created_view_without_retrying() {
    for unchanged in [false, true] {
        let (layout, host) = fixture().await;
        {
            let mut state = host.0.lock().await;
            state.fail_swap = !unchanged;
            state.swap_unchanged = unchanged;
            state.change_pane_id_on_move = true;
        }
        let placement = placement(
            &layout,
            &host,
            true,
            json!({"type":"current_tab","direction":"left"}),
        )
        .await
        .unwrap();
        let (pane_id, error) = not_placed(create(&layout, &placement).await);
        assert_eq!(pane_id, "moved-staged");
        assert!(error.dispatched);
        assert_eq!(host.calls("layout.apply").await.len(), 1);
        assert_eq!(host.calls("pane.move").await.len(), 1);
        assert_eq!(host.calls("pane.swap").await.len(), 1);
    }
}

#[tokio::test]
async fn a_changed_anchor_after_the_move_blocks_the_swap() {
    let (layout, host) = fixture().await;
    host.0.lock().await.change_destination_after_move = true;
    let placement = placement(
        &layout,
        &host,
        false,
        json!({"type":"current_tab","direction":"up"}),
    )
    .await
    .unwrap();
    let error = move_view(&layout, &host, &placement).await.unwrap_err();
    assert_eq!(error.code, "TargetChanged");
    assert!(error.dispatched);
    assert!(host.calls("pane.swap").await.is_empty());
    assert_eq!(host.0.lock().await.panes["view"].tab_id, "agent-tab");
}

#[tokio::test]
async fn a_failure_after_staging_a_same_tab_view_leaves_it_in_the_temporary_tab() {
    let (layout, host) = fixture().await;
    host.update("view", |pane| pane.tab_id = "agent-tab".into())
        .await;
    host.0.lock().await.fail_move_number = Some(2);
    let placement = placement(
        &layout,
        &host,
        false,
        json!({"type":"current_tab","direction":"left"}),
    )
    .await
    .unwrap();
    let error = move_view(&layout, &host, &placement).await.unwrap_err();
    assert!(error.dispatched);
    let pane = host.0.lock().await.panes["view"].clone();
    assert_eq!(pane.tab_id, "temporary-tab");
    assert_eq!(pane.terminal_id, "terminal-view");
    assert_eq!(host.calls("pane.move").await.len(), 2);
    assert!(host.calls("pane.swap").await.is_empty());
}

#[tokio::test]
async fn an_unchanged_host_move_is_not_reported_as_success() {
    let (layout, host) = fixture().await;
    host.0.lock().await.move_unchanged = true;
    let placement = placement(&layout, &host, false, Value::Null).await.unwrap();
    let error = move_view(&layout, &host, &placement).await.unwrap_err();
    assert_eq!(error.code, "PlacementNotApplied");
    assert!(host.calls("pane.swap").await.is_empty());
}

#[tokio::test]
async fn another_terminal_after_staging_is_not_moved_into_the_destination() {
    let (layout, host) = fixture().await;
    host.update("view", |pane| pane.tab_id = "agent-tab".into())
        .await;
    host.0.lock().await.replace_terminal_after_staging = true;
    let placement = placement(&layout, &host, false, Value::Null).await.unwrap();
    let error = move_view(&layout, &host, &placement).await.unwrap_err();
    assert_eq!(error.code, "OutcomeUnknown");
    assert!(error.dispatched);
    assert_eq!(host.calls("pane.move").await.len(), 1);
    assert!(host.calls("pane.swap").await.is_empty());
}

#[tokio::test]
async fn changed_adjacency_is_not_reported_as_a_successful_placement() {
    for direction in ["left", "right", "up", "down"] {
        let (layout, host) = fixture().await;
        host.0.lock().await.lose_adjacency_after_move = true;
        let placement = placement(
            &layout,
            &host,
            false,
            json!({"type":"current_tab","direction":direction}),
        )
        .await
        .unwrap();
        let error = move_view(&layout, &host, &placement).await.unwrap_err();
        assert_eq!(error.code, "TargetChanged");
        assert!(error.dispatched);
        assert!(host.calls("pane.swap").await.is_empty());
    }
}

#[tokio::test]
async fn a_move_defaults_beside_the_frozen_view_pane() {
    let (layout, host) = fixture().await;
    let placement = placement(&layout, &host, false, Value::Null).await.unwrap();
    assert_eq!(
        placement.summary(),
        json!({"type":"pane","pane_id":"agent","terminal_id":"terminal-agent","tab_id":"agent-tab","workspace_id":"w1","direction":"right"})
    );
    let moved = move_view(&layout, &host, &placement).await.unwrap();
    assert_eq!(moved.terminal_id, "terminal-view");
    assert_eq!(moved.tab_id, "agent-tab");
    assert_eq!(
        host.calls("pane.move").await,
        [
            json!({"pane_id":"view","focus":false,"destination":{"type":"tab","tab_id":"agent-tab","target_pane_id":"agent","split":"right"}})
        ]
    );
}

#[tokio::test]
async fn a_move_follows_the_pane_id_herdr_gives_the_view() {
    let (layout, host) = fixture().await;
    host.0.lock().await.change_pane_id_on_move = true;
    let placement = placement(&layout, &host, false, Value::Null).await.unwrap();
    let moved = move_view(&layout, &host, &placement).await.unwrap();
    assert_eq!(moved.pane_id, "moved-view");
    assert_eq!(moved.terminal_id, "terminal-view");
    assert_eq!(moved.tab_id, "agent-tab");
}

#[tokio::test]
async fn a_view_opened_in_the_agents_tab_is_staged_then_moved_without_focus() {
    let (layout, host) = fixture().await;
    host.0.lock().await.change_pane_id_on_move = true;
    let placement = placement(&layout, &host, true, json!({"type":"current_tab"}))
        .await
        .unwrap();
    let view = placed(create(&layout, &placement).await);
    assert_eq!(view.pane_id, "moved-staged");
    assert_eq!(view.tab_id, "agent-tab");
    assert_eq!(host.calls("layout.apply").await[0]["focus"], false);
    assert_eq!(host.calls("pane.move").await[0]["focus"], false);
}

#[tokio::test]
async fn a_frozen_destination_that_changed_blocks_the_move() {
    let (layout, host) = fixture().await;
    let placement = placement(
        &layout,
        &host,
        false,
        json!({"type":"pane","pane_id":"anchor","direction":"down"}),
    )
    .await
    .unwrap();
    host.update("anchor", |pane| pane.tab_id = "changed-tab".into())
        .await;
    let error = move_view(&layout, &host, &placement).await.unwrap_err();
    assert_eq!(error.code, "TargetChanged");
    assert!(host.calls("pane.move").await.is_empty());
}

#[tokio::test]
async fn a_failed_second_stage_reports_the_created_pane_and_an_uncertain_move() {
    let (layout, host) = fixture().await;
    host.0.lock().await.fail_move = true;
    let placement = placement(&layout, &host, true, json!({"type":"current_tab"}))
        .await
        .unwrap();
    let (pane_id, error) = not_placed(create(&layout, &placement).await);
    assert_eq!(pane_id, "staged");
    assert_eq!(error.code, "OutcomeUnknown");
    assert!(error.dispatched);
}

#[tokio::test]
async fn a_destination_changed_during_staging_leaves_the_created_pane_where_it_is() {
    let (layout, host) = fixture().await;
    host.0.lock().await.move_destination_after_launch = true;
    let placement = placement(&layout, &host, true, json!({"type":"current_tab"}))
        .await
        .unwrap();
    let (pane_id, error) = not_placed(create(&layout, &placement).await);
    assert_eq!(pane_id, "staged");
    assert_eq!(error.code, "TargetChanged");
    assert!(host.calls("pane.move").await.is_empty());
}

#[tokio::test]
async fn unsafe_destinations_are_refused_before_anything_moves() {
    let (layout, host) = fixture().await;
    for refused in [
        json!({"type":"pane","pane_id":"view"}),
        json!({"type":"pane","pane_id":"missing"}),
        json!({"type":"new_tab"}),
        json!({"type":"pane","pane_id":"anchor","direction":"diagonal"}),
    ] {
        assert!(
            placement(&layout, &host, false, refused.clone())
                .await
                .is_err(),
            "{refused}"
        );
    }
    host.insert(pane("foreign", "foreign-tab", "w2")).await;
    assert!(
        placement(
            &layout,
            &host,
            false,
            json!({"type":"pane","pane_id":"foreign"})
        )
        .await
        .is_err()
    );
    let without_view = layout
        .prepare_placement(
            &json!({"placement":{"type":"current_tab"}}),
            "w1",
            true,
            None,
            &LayoutContext::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(without_view.code, "MissingAnchor");
    assert!(host.calls("pane.move").await.is_empty());
}

#[tokio::test]
async fn closing_a_view_finds_its_terminal_wherever_it_moved_and_a_closed_one_stays_closed() {
    let (layout, host) = fixture().await;
    host.update("view", |pane| pane.pane_id = "renamed-view".into())
        .await;
    layout.close_view("terminal-view").await.unwrap();
    let closes = host.calls("pane.close").await;
    assert_eq!(closes, [json!({"pane_id":"renamed-view"})]);
    layout.close_view("terminal-gone").await.unwrap();
    assert_eq!(host.calls("pane.close").await.len(), 1);
}

/// A view Pane is the Pane that names the Tick it shows in its metadata, in whatever Workspace
/// it is; a Pane whose report lapsed or names another Tick is not.
#[tokio::test]
async fn the_view_panes_of_a_tick_are_the_panes_naming_it_in_any_workspace() {
    let (layout, host) = fixture().await;
    let shows = |instance: &str| {
        let instance = instance.to_owned();
        move |pane: &mut Pane| {
            pane.tokens
                .insert(tickhost_connect::herdr::VIEW_TOKEN.into(), instance);
        }
    };
    host.update("view", shows("hosted:quote")).await;
    host.insert(pane("elsewhere", "far-tab", "w2")).await;
    host.update("elsewhere", shows("hosted:quote")).await;
    host.update("anchor", shows("hosted:other")).await;
    let found = layout
        .view_panes("hosted:quote")
        .await
        .unwrap()
        .into_iter()
        .map(|pane| pane.pane_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(found, ["elsewhere".to_owned(), "view".to_owned()].into());
    assert!(layout.view_panes("hosted:gone").await.unwrap().is_empty());
}
