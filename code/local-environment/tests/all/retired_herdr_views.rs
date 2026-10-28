//! Conversations an earlier build recorded while it could show Ticks in Herdr Panes: TickManage
//! calls with `placement` and `workspace_id`, `move`, ReadWorkspace, and results that report the
//! view Pane. They still load, display and continue under the current Tick tools, which take
//! none of it: a repeated placement or move is refused with what to send instead, and
//! ReadWorkspace is no longer offered.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_code_local_environment::hosted::{
    tools::HostedTickTools,
    worker_control::{DaemonControl, WorkerControlGrant},
};
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_harness::protocol::{
    ChatId, Content, EventKind, ExecutionMode, Role, ToolDefinition, ToolResult,
};
use tickloop_harness::runtime::{
    Agent,
    host_tools::{HostToolPermission, HostToolProvider},
};
use tickloop_harness::test_support::{ScriptedModel, call, config, next_event, text};
use tickloop_harness::transcript::{
    Line,
    display::{DisplayBlock, project},
};
use tokio_util::sync::CancellationToken;

const INSTANCE: &str = "hosted:0190d0c4-6b7a-7c3e-9f00-0123456789ab";

/// The calls an earlier build's model made, as that build accepted them.
fn retired_calls() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "open-beside",
            "TickManage",
            json!({"action":"open","command":["tick-counter"],"workspace_id":"w1","placement":{"type":"current_tab","direction":"right"}}),
        ),
        ("list-panes", "ReadWorkspace", json!({"workspace_id":"w1"})),
        (
            "move-view",
            "TickManage",
            json!({"action":"move","tick_instance_id":INSTANCE,"placement":{"type":"pane","pane_id":"p1","direction":"down"}}),
        ),
        (
            "close",
            "TickManage",
            json!({"action":"close","tick_instance_id":INSTANCE}),
        ),
    ]
}

/// What that build answered to each call, view Pane reports included.
fn retired_result(call_id: &str) -> Value {
    match call_id {
        "open-beside" => {
            json!({"status":"opened","host":"tickhost","visibility":"visible","tick_instance_id":INSTANCE,"view":{"status":"shown","host":"herdr","pane_id":"p2","terminal_id":"t2","tab_id":"tab1","workspace_id":"w1"}})
        }
        "list-panes" => {
            json!({"host":"herdr","workspace_id":"w1","panes":[{"pane_id":"p2","tab_id":"tab1","workspace_id":"w1","view_of":INSTANCE}],"total":1,"next_cursor":null,"ticks":[{"tick_instance_id":INSTANCE,"state":"live","views":1,"view_panes":["p2"]}]})
        }
        "move-view" => {
            json!({"status":"completed","host":"tickhost","tick_instance_id":INSTANCE,"view":{"status":"moved","host":"herdr","pane_id":"p3","workspace_id":"w1"}})
        }
        _ => {
            json!({"status":"closed","host":"tickhost","tick_instance_id":INSTANCE,"view":{"status":"closed","host":"herdr"}})
        }
    }
}

/// What a result says of view Panes, apart from the Tick identities the Loop may rewrite.
fn view_report(result: &Value) -> Value {
    json!([
        result["status"],
        result["visibility"],
        result["view"],
        result["panes"][0]["pane_id"],
        result["ticks"][0]["view_panes"],
    ])
}

/// The Tick tools of an earlier build in a Herdr View, which recorded what [`retired_result`]
/// says for each call.
struct RetiredHerdrTools {
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl HostToolProvider for RetiredHerdrTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["TickManage", "ReadWorkspace"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: format!("{name} as an earlier build offered it."),
                input_schema: json!({"type":"object"}),
            })
            .collect()
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }

    async fn execute(
        &self,
        _: ChatId,
        name: &str,
        input: Value,
        _: CancellationToken,
    ) -> ToolResult {
        let call_id = retired_calls()
            .into_iter()
            .find(|(_, called, expected)| *called == name && *expected == input)
            .map(|(call_id, ..)| call_id)
            .unwrap_or_else(|| panic!("unexpected {name} {input}"));
        self.calls.lock().unwrap().push(call_id.into());
        ToolResult {
            content: retired_result(call_id).to_string(),
            is_error: false,
            output_ref: None,
        }
    }
}

/// The current Tick tools behind a daemon nobody listens on: nothing can reach a host.
fn current_tick_tools(directory: &std::path::Path) -> HostedTickTools {
    HostedTickTools::new(
        DaemonControl::new(WorkerControlGrant {
            endpoint: directory.join("unused-daemon.sock"),
            daemon_epoch: uuid::Uuid::new_v4(),
            binding: uuid::Uuid::new_v4(),
            capability: "unused".into(),
            host_endpoint: directory.join("unused-host.sock"),
        }),
        Vec::new(),
    )
}

async fn run_turn(agent: &Agent, chat: ChatId, prompt: &str) -> Vec<(String, ToolResult)> {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    let mut results = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolCompleted {
                call_id, result, ..
            } => results.push((call_id, result)),
            EventKind::Completed { .. } => return results,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

fn tool_uses(messages: &[tickloop_harness::protocol::Message]) -> Vec<(String, Value)> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolUse { name, input, .. } => Some((name.clone(), input.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_conversation_with_retired_herdr_view_calls_loads_displays_and_continues() {
    let directory = tempfile::Builder::new()
        .prefix("rhv")
        .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
        .unwrap();
    let folder = directory.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    let root = directory.path().join("state");

    let mut replies = retired_calls()
        .into_iter()
        .map(|(id, name, input)| call(id, name, input))
        .collect::<Vec<_>>();
    replies.push(text("The counter was shown, moved and closed."));
    let earlier = Agent::open(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(replies)),
    )
    .unwrap();
    let retired = Arc::new(RetiredHerdrTools {
        calls: Mutex::new(Vec::new()),
    });
    earlier.install_host_tools(retired.clone()).unwrap();
    earlier
        .trust_working_directory(folder.clone())
        .await
        .unwrap();
    let chat = earlier.create_chat(folder.clone()).await.unwrap();
    earlier
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let recorded = run_turn(&earlier, chat, "Show the counter next to you").await;
    assert_eq!(recorded.len(), 4, "{recorded:?}");
    assert_eq!(retired.calls.lock().unwrap().len(), 4);
    earlier.shutdown().await;
    drop(earlier);

    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "stray-placement",
            "TickManage",
            json!({"action":"open","command":["tick-counter"],"placement":{"type":"current_tab"}}),
        ),
        call(
            "stray-move",
            "TickManage",
            json!({"action":"move","tick_instance_id":INSTANCE}),
        ),
        text("Ticks show in the Monitor now."),
    ]));
    let current = Agent::open(root.clone(), config(), model.clone()).unwrap();
    current
        .install_host_tools(Arc::new(current_tick_tools(directory.path())))
        .unwrap();
    current
        .trust_working_directory(folder.clone())
        .await
        .unwrap();
    let loaded = current.resume_chat(chat).await.unwrap();
    let loaded_calls = tool_uses(&loaded.messages);
    for (_, name, input) in retired_calls() {
        assert!(
            loaded_calls.contains(&(name.to_owned(), input.clone())),
            "{name} {input} survives loading"
        );
    }
    let loaded_results = loaded
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                content, is_error, ..
            } => Some((content.text().into_owned(), *is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    for (call_id, ..) in retired_calls() {
        let reported = view_report(&retired_result(call_id));
        assert!(
            loaded_results.iter().any(|(content, is_error)| !is_error
                && serde_json::from_str::<Value>(content)
                    .is_ok_and(|loaded| view_report(&loaded) == reported)),
            "{call_id}'s result survives loading: {loaded_results:?}"
        );
    }

    let lines = std::fs::read_to_string(tickloop_harness::test_support::transcript_file(
        &root,
        chat.uuid(),
    ))
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str::<Line>(line).unwrap())
    .collect::<Vec<_>>();
    let displayed = project(&lines)
        .into_iter()
        .flat_map(|node| node.blocks)
        .filter_map(|block| match block {
            DisplayBlock::ToolCall {
                call_id,
                name,
                input,
                result,
                ..
            } => Some((call_id, name, input, result)),
            _ => None,
        })
        .collect::<Vec<_>>();
    for (call_id, name, input) in retired_calls() {
        let (_, shown_name, shown_input, result) = displayed
            .iter()
            .find(|(shown, ..)| shown == call_id)
            .unwrap_or_else(|| panic!("{call_id} is displayed: {displayed:?}"));
        assert_eq!(shown_name, name);
        assert_eq!(*shown_input, input);
        let result = result.as_ref().expect("its recorded result is displayed");
        assert!(!result.is_error, "{call_id}: {}", result.text);
        assert!(result.text.contains("\"view\"") || result.text.contains("view_of"));
    }

    let answered = run_turn(&current, chat, "Show it beside you again").await;
    let refusal = |call_id: &str| {
        let (_, result) = answered
            .iter()
            .find(|(answered, _)| answered == call_id)
            .unwrap_or_else(|| panic!("{call_id} was answered: {answered:?}"));
        assert!(result.is_error, "{call_id}: {}", result.content);
        result.content.clone()
    };
    assert!(
        refusal("stray-placement").contains(
            "placement is not allowed for open; open takes command, and optionally cwd, protocol"
        ),
        "{}",
        refusal("stray-placement")
    );
    assert!(
        refusal("stray-move").contains("action must be one of open, send_input, close, reopen"),
        "{}",
        refusal("stray-move")
    );
    current.shutdown().await;

    let requests = model.requests.lock().unwrap();
    let request = &requests[0];
    let sent = tool_uses(&request.messages);
    for (_, name, input) in retired_calls() {
        assert!(
            sent.contains(&(name.to_owned(), input)),
            "the recorded {name} call is sent to the model as it was"
        );
    }
    assert!(
        request
            .messages
            .iter()
            .any(|message| message.role == Role::User),
        "the history continues"
    );
    let offered = request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert!(offered.contains(&"TickManage"), "{offered:?}");
    assert!(!offered.contains(&"ReadWorkspace"), "{offered:?}");
    let tick_manage = request
        .tools
        .iter()
        .find(|tool| tool.name == "TickManage")
        .unwrap();
    assert!(
        tick_manage.input_schema["properties"]
            .get("placement")
            .is_none()
    );
}
