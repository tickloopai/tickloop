use super::*;
use tickloop_harness::test_support::mcp_service::McpService;

const WAITING: &str = "Waiting for MCP servers:";
const REPLY: &str = "answered after the grace";

/// The text of the line showing the running activity, if there is one.
fn activity_line(frame: &str) -> Option<String> {
    frame
        .lines()
        .find(|line| line.contains(WAITING) || line.contains("Working"))
        .map(|line| line.trim().to_owned())
}

async fn first_request_shows_the_servers_it_waits_for(fixture: &PrivateFixture) {
    let quick = McpService::start().await;
    quick.set_tools(json!([{"name":"quote","inputSchema":{"type":"object"}}]));
    quick.set_directory_delay(Duration::from_millis(1200));
    let late = McpService::start().await;
    late.set_directory_delay(Duration::from_secs(60));
    std::fs::write(
        fixture.path("config/settings.json"),
        serde_json::to_vec_pretty(&json!({"mcpServers": {
            "quick": {"url": quick.url()},
            "late": {"url": late.url()},
        }}))
        .unwrap(),
    )
    .unwrap();
    let (base_url, mut requests) =
        model_http_support::serve_responses(vec![(200, chat_reply(None, REPLY))]).await;
    fixture.set_chat_model(&base_url);
    let mut view = PtyView::start(fixture);
    view.wait_for("View ready", new_input_is_visible).await;

    view.send(b"hello\r");
    view.wait_for("the request waits for both servers", |frame| {
        frame.contains(&format!("{WAITING} late, quick"))
    })
    .await;
    view.wait_for("the line offers Esc as the turn's interrupt", |frame| {
        activity_line(frame)
            .is_some_and(|line| line.contains(WAITING) && line.contains("esc to interrupt"))
    })
    .await;
    view.wait_for("the list shrinks as a server connects", |frame| {
        activity_line(frame).is_some_and(|line| {
            line.contains(&format!("{WAITING} late")) && !line.contains("quick")
        })
    })
    .await;
    tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .expect("the request goes out once the grace ends")
        .unwrap();
    let frame = view
        .wait_for("the reply arrives", |frame| frame.contains(REPLY))
        .await;
    assert!(!frame.contains(WAITING), "{frame}");
    view.close().await;
}

#[tokio::test]
async fn actual_cli_shows_which_mcp_servers_the_first_request_waits_for() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(first_request_shows_the_servers_it_waits_for(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
