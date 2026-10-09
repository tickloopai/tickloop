use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickhost_connect::{
    access::TickAccess,
    herdr::{HerdrContext, HerdrPort, Pane},
    host_error::HostError,
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
    replace_instance_after_staging: bool,
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
            "pane.current" => Ok(json!({"pane":state.panes.get("agent").unwrap()})),
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
                state.panes.insert(
                    "staged".into(),
                    pane("staged", "new-tab", workspace, Some("herdr:new")),
                );
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
                } else if state.replace_instance_after_staging {
                    moved
                        .tokens
                        .insert("tickloop_instance".into(), "herdr:replacement".into());
                }
                state.panes.insert(moved.pane_id.clone(), moved.clone());
                if state.change_destination_after_move {
                    state.panes.get_mut("agent").unwrap().terminal_id = "replaced-agent".into();
                }
                Ok(json!({"move_result":{"changed":true,"pane":moved}}))
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

fn pane(id: &str, tab: &str, workspace: &str, instance: Option<&str>) -> Pane {
    Pane {
        pane_id: id.into(),
        terminal_id: format!("terminal-{id}"),
        workspace_id: workspace.into(),
        tab_id: tab.into(),
        cwd: Some("/tmp".into()),
        foreground_cwd: None,
        title: None,
        tokens: instance
            .map(|instance| BTreeMap::from([("tickloop_instance".into(), instance.into())]))
            .unwrap_or_default(),
    }
}

async fn fixture() -> (TickAccess, Arc<FakeHerdr>) {
    let host = Arc::new(FakeHerdr::default());
    host.insert(pane("agent", "agent-tab", "w1", Some("herdr:agent")))
        .await;
    host.insert(pane("tick", "other-tab", "w1", Some("herdr:existing")))
        .await;
    host.insert(pane("anchor", "anchor-tab", "w1", Some("herdr:anchor")))
        .await;
    let access = TickAccess::with_host(
        HerdrContext {
            socket: PathBuf::new(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    );
    (access, host)
}

#[tokio::test]
async fn open_to_the_left_of_the_agent_is_supported() {
    let (access, host) = fixture().await;
    let prepared = access
        .prepare(
            "modify_tick",
            &json!({"action":"open","command":["/bin/sh"],"cwd":"/tmp","placement":{"type":"current_tab","direction":"left"}}),
        )
        .await
        .unwrap();
    let result = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result["status"], "opened");
    assert_eq!(result["tab_id"], "agent-tab");
    assert_eq!(
        host.calls("pane.move").await[0]["destination"]["split"],
        "right"
    );
    assert_eq!(host.calls("pane.swap").await.len(), 1);
    assert_eq!(
        host.calls("pane.swap").await[0],
        json!({"source_pane_id":"agent","target_pane_id":"staged"})
    );
}

#[tokio::test]
async fn all_directions_reposition_an_existing_tick_in_the_same_tab() {
    for direction in ["left", "right", "up", "down"] {
        let (access, host) = fixture().await;
        host.update("tick", |pane| pane.tab_id = "agent-tab".into())
            .await;
        let prepared = access.prepare("modify_tick", &json!({"action":"move","tick_instance_id":"herdr:existing","placement":{"type":"current_tab","direction":direction}})).await.unwrap();
        assert_eq!(prepared.input["resolved_placement"]["direction"], direction);
        let result = access
            .execute(prepared, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result["status"], "moved");
        assert_eq!(result["tick_instance_id"], "herdr:existing");
        assert_eq!(result["pane_id"], "tick");
        assert_eq!(result["tab_id"], "agent-tab");
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
            Some(&"tick".into())
        );
    }
}

#[tokio::test]
async fn open_above_a_named_pane_uses_down_then_swap() {
    let (access, host) = fixture().await;
    let prepared = access.prepare("modify_tick", &json!({"action":"open","command":["/bin/sh"],"cwd":"/tmp","placement":{"type":"pane","pane_id":"anchor","direction":"up"}})).await.unwrap();
    let result = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result["status"], "opened");
    assert_eq!(result["tab_id"], "anchor-tab");
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
async fn failed_or_unchanged_swap_reports_the_created_instance_without_retrying() {
    for unchanged in [false, true] {
        let (access, host) = fixture().await;
        {
            let mut state = host.0.lock().await;
            state.fail_swap = !unchanged;
            state.swap_unchanged = unchanged;
            state.change_pane_id_on_move = true;
        }
        let prepared = access.prepare("modify_tick", &json!({"action":"open","command":["/bin/sh"],"cwd":"/tmp","placement":{"type":"current_tab","direction":"left"}})).await.unwrap();
        let result = access
            .execute(prepared, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result["status"], "created_not_placed");
        assert_eq!(result["pane_id"], "moved-staged");
        assert_eq!(result["error"]["dispatched"], true);
        assert_eq!(host.calls("layout.apply").await.len(), 1);
        assert_eq!(host.calls("pane.move").await.len(), 1);
        assert_eq!(host.calls("pane.swap").await.len(), 1);
    }
}

#[tokio::test]
async fn changed_anchor_after_move_blocks_swap_and_reports_partial_placement() {
    let (access, host) = fixture().await;
    host.0.lock().await.change_destination_after_move = true;
    let prepared = access.prepare("modify_tick", &json!({"action":"move","tick_instance_id":"herdr:existing","placement":{"type":"current_tab","direction":"up"}})).await.unwrap();
    let error = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "TargetChanged");
    assert!(error.dispatched);
    assert!(host.calls("pane.swap").await.is_empty());
    assert_eq!(host.0.lock().await.panes["tick"].tab_id, "agent-tab");
}

#[tokio::test]
async fn failure_after_staging_a_same_tab_tick_preserves_its_instance() {
    let (access, host) = fixture().await;
    host.update("tick", |pane| pane.tab_id = "agent-tab".into())
        .await;
    host.0.lock().await.fail_move_number = Some(2);
    let prepared = access.prepare("modify_tick", &json!({"action":"move","tick_instance_id":"herdr:existing","placement":{"type":"current_tab","direction":"left"}})).await.unwrap();
    let error = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.dispatched);
    let pane = host.0.lock().await.panes["tick"].clone();
    assert_eq!(pane.tab_id, "temporary-tab");
    assert_eq!(pane.tokens["tickloop_instance"], "herdr:existing");
    assert_eq!(host.calls("pane.move").await.len(), 2);
    assert!(host.calls("pane.swap").await.is_empty());
}

#[tokio::test]
async fn unchanged_host_move_is_not_reported_as_success() {
    let (access, host) = fixture().await;
    host.0.lock().await.move_unchanged = true;
    let prepared = access
        .prepare(
            "modify_tick",
            &json!({"action":"move","tick_instance_id":"herdr:existing"}),
        )
        .await
        .unwrap();
    let error = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "PlacementNotApplied");
    assert!(host.calls("pane.swap").await.is_empty());
}

#[tokio::test]
async fn replacement_after_staging_is_not_moved_into_the_destination() {
    let (access, host) = fixture().await;
    host.update("tick", |pane| pane.tab_id = "agent-tab".into())
        .await;
    host.0.lock().await.replace_instance_after_staging = true;
    let prepared = access
        .prepare(
            "modify_tick",
            &json!({"action":"move","tick_instance_id":"herdr:existing"}),
        )
        .await
        .unwrap();
    let error = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "TargetGone");
    assert!(error.dispatched);
    assert_eq!(host.calls("pane.move").await.len(), 1);
    assert!(host.calls("pane.swap").await.is_empty());
}

#[tokio::test]
async fn changed_adjacency_is_not_reported_as_a_successful_placement() {
    for direction in ["left", "right", "up", "down"] {
        let (access, host) = fixture().await;
        host.0.lock().await.lose_adjacency_after_move = true;
        let prepared = access.prepare("modify_tick", &json!({"action":"move","tick_instance_id":"herdr:existing","placement":{"type":"current_tab","direction":direction}})).await.unwrap();
        let error = access
            .execute(prepared, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, "TargetChanged");
        assert!(error.dispatched);
        assert!(host.calls("pane.swap").await.is_empty());
    }
}

#[tokio::test]
async fn move_to_current_tab_keeps_instance_and_uses_frozen_caller_pane() {
    let (access, host) = fixture().await;
    let prepared = access
        .prepare(
            "modify_tick",
            &json!({"action":"move","tick_instance_id":"herdr:existing"}),
        )
        .await
        .unwrap();
    assert!(prepared.summary.contains("agent-tab"));
    assert_eq!(
        prepared.input["resolved_placement"],
        json!({"type":"pane","pane_id":"agent","terminal_id":"terminal-agent","tab_id":"agent-tab","workspace_id":"w1","direction":"right"})
    );
    let result = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result["status"], "moved");
    assert_eq!(result["tick_instance_id"], "herdr:existing");
    assert_eq!(result["tab_id"], "agent-tab");
    let calls = host.calls("pane.move").await;
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0],
        json!({"pane_id":"tick","focus":false,"destination":{"type":"tab","tab_id":"agent-tab","target_pane_id":"agent","split":"right"}})
    );
}

#[tokio::test]
async fn move_tracks_herdr_pane_id_change_without_changing_tick_identity() {
    let (access, host) = fixture().await;
    host.0.lock().await.change_pane_id_on_move = true;
    let prepared = access
        .prepare(
            "modify_tick",
            &json!({"action":"move","tick_instance_id":"herdr:existing"}),
        )
        .await
        .unwrap();
    let result = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result["tick_instance_id"], "herdr:existing");
    assert_eq!(result["pane_id"], "moved-tick");
    assert_eq!(result["tab_id"], "agent-tab");
}

#[tokio::test]
async fn same_tab_open_tracks_herdr_pane_id_change() {
    let (access, host) = fixture().await;
    host.0.lock().await.change_pane_id_on_move = true;
    let prepared = access.prepare("modify_tick", &json!({"action":"open","command":["/bin/sh"],"cwd":"/tmp","placement":{"type":"current_tab"}})).await.unwrap();
    let result = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result["status"], "opened");
    assert_eq!(result["pane_id"], "moved-staged");
    assert_eq!(result["tick_instance_id"], "herdr:new");
}

#[tokio::test]
async fn explicit_pane_direction_is_frozen_and_changed_destination_blocks_dispatch() {
    let (access, host) = fixture().await;
    let input = json!({"action":"move","tick_instance_id":"herdr:existing","placement":{"type":"pane","pane_id":"anchor","direction":"down"}});
    let prepared = access.prepare("modify_tick", &input).await.unwrap();
    host.update("anchor", |pane| pane.tab_id = "changed-tab".into())
        .await;
    let error = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "TargetChanged");
    assert!(host.calls("pane.move").await.is_empty());
}

#[tokio::test]
async fn source_move_after_approval_blocks_dispatch() {
    let (access, host) = fixture().await;
    let prepared = access
        .prepare(
            "modify_tick",
            &json!({"action":"move","tick_instance_id":"herdr:existing"}),
        )
        .await
        .unwrap();
    host.update("tick", |pane| pane.tab_id = "changed-tab".into())
        .await;
    let error = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "TargetChanged");
    assert!(host.calls("pane.move").await.is_empty());
}

#[tokio::test]
async fn same_tab_open_stages_a_new_tab_then_moves_without_focus() {
    let (access, host) = fixture().await;
    let prepared = access.prepare("modify_tick", &json!({"action":"open","command":["/bin/sh"],"cwd":"/tmp","placement":{"type":"current_tab"}})).await.unwrap();
    let result = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result["status"], "opened");
    assert_eq!(result["tick_instance_id"], "herdr:new");
    assert_eq!(result["tab_id"], "agent-tab");
    assert_eq!(host.calls("layout.apply").await[0]["focus"], false);
    assert_eq!(host.calls("pane.move").await[0]["focus"], false);
}

#[tokio::test]
async fn failed_second_stage_reports_created_pane_and_uncertain_move() {
    let (access, host) = fixture().await;
    host.0.lock().await.fail_move = true;
    let prepared = access.prepare("modify_tick", &json!({"action":"open","command":["/bin/sh"],"cwd":"/tmp","placement":{"type":"current_tab"}})).await.unwrap();
    let result = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result["status"], "created_not_placed");
    assert_eq!(result["pane_id"], "staged");
    assert_eq!(result["error"]["code"], "OutcomeUnknown");
    assert_eq!(result["error"]["dispatched"], true);
}

#[tokio::test]
async fn destination_changed_during_staging_leaves_created_pane_visible() {
    let (access, host) = fixture().await;
    host.0.lock().await.move_destination_after_launch = true;
    let prepared = access.prepare("modify_tick", &json!({"action":"open","command":["/bin/sh"],"cwd":"/tmp","placement":{"type":"current_tab"}})).await.unwrap();
    let result = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result["status"], "created_not_placed");
    assert_eq!(result["pane_id"], "staged");
    assert_eq!(result["error"]["code"], "TargetChanged");
    assert!(host.calls("pane.move").await.is_empty());
}

#[tokio::test]
async fn read_workspace_exposes_caller_container_for_placement() {
    let (access, _) = fixture().await;
    let prepared = access
        .prepare("read_workspace", &json!({"workspace_id":"empty-workspace"}))
        .await
        .unwrap();
    let result = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        result["caller"],
        json!({"pane_id":"agent","tab_id":"agent-tab","workspace_id":"w1"})
    );
    assert_eq!(result["ticks"], json!([]));
}

#[tokio::test]
async fn unsafe_destinations_are_rejected_before_a_move() {
    let (access, host) = fixture().await;
    for placement in [
        json!({"type":"pane","pane_id":"tick"}),
        json!({"type":"pane","pane_id":"missing"}),
        json!({"type":"new_tab"}),
        json!({"type":"pane","pane_id":"anchor","direction":"diagonal"}),
    ] {
        assert!(access.prepare("modify_tick", &json!({"action":"move","tick_instance_id":"herdr:existing","placement":placement})).await.is_err());
    }
    host.insert(pane("foreign", "foreign-tab", "w2", Some("herdr:foreign")))
        .await;
    assert!(access.prepare("modify_tick", &json!({"action":"move","tick_instance_id":"herdr:existing","placement":{"type":"pane","pane_id":"foreign"}})).await.is_err());
    assert!(host.calls("pane.move").await.is_empty());
}
