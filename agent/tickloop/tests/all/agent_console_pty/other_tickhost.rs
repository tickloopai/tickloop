use super::*;
use support::hosted::OtherBuildHost;
use tickhost::paths::HostPaths;
use tickhost_connect::hosted::{
    BUSINESS_MAJOR, BUSINESS_MINOR, HostDescription, HostOccupancy, ManagementConnection,
};

const OTHER_BUILD: &str = "0.0.0+an-older-build";
const RESTART_MENU: &str = "Restart the running Tickhost?";
const OTHER_TICKHOST_GUIDANCE: &str = "The running Tickhost is another version; restart tickloop and choose to restart the Tickhost at the prompt, or run `tickhost stop` (its Ticks end)";
/// The part of the guidance a Shell's collapsed result shows.
const SHOWN_GUIDANCE: &str = "The running Tickhost is another version; restart tickloop and choose to restart the Tickhost at the prompt";

/// The fixture's daemon key, and a Tickhost of another build that runs a Tick and a Shell and is
/// starting one more at the daemon's Tickhost endpoint. Its Tick's launcher and a data client
/// are connected, but no view is attached.
fn other_build_running_a_tick(fixture: &PrivateFixture) -> (String, OtherBuildHost) {
    let key = tickhost::host::random_token();
    std::fs::write(fixture.path("runtime/tickhost.key"), &key).unwrap();
    let host = OtherBuildHost::serve(
        &HostPaths {
            runtime: fixture.path("tickhost"),
        },
        &key,
        Some(HostDescription {
            build: OTHER_BUILD.into(),
            executable: None,
            business_major: BUSINESS_MAJOR,
            business_minor: BUSINESS_MINOR,
            occupancy: HostOccupancy {
                ticks: vec!["Quote board".into()],
                shells: 1,
                pending: Some(1),
                connections: 2,
                views: Some(0),
            },
        }),
    );
    (key, host)
}

/// The words on the screen, whatever lines they wrap over.
fn words(frame: &str) -> String {
    frame.split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn restart_menu(view: &mut PtyView) -> String {
    view.wait_for("the Tickhost restart menu", |frame| {
        frame.contains(RESTART_MENU) && frame.contains("Stop the old Tickhost")
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_restarting_a_tickhost_of_another_build_stops_it_and_runs_shells_in_this_build()
{
    let fixture = PrivateFixture::new();
    let (key, host) = other_build_running_a_tick(&fixture);
    let mut view = PtyView::start(&fixture);

    let menu = words(&restart_menu(&mut view).await);
    assert!(
        menu.contains(&format!("Running Tickhost build {OTHER_BUILD}"))
            && menu.contains("Restarting it ends these Ticks: Quote board.")
            && menu.contains("It also ends 1 running Shell.")
            && menu.contains("It also ends 1 Tick or Shell still starting.")
            && !menu.contains("open view"),
        "the menu names the build and what restarting ends, and no connection as a view: {menu}"
    );
    assert!(!host.was_stopped(), "nothing ends before the user chooses");
    view.send(b"k");
    view.send(b"\r");
    view.wait_for("input ready", new_input_is_visible).await;
    assert!(host.was_stopped());

    view.send(b"!printf 'restarted-%s' tickhost; sleep 30\r");
    view.wait_for("the Shell runs in a Tickhost of this build", |frame| {
        frame.contains("restarted-tickhost")
    })
    .await;
    let hello = ManagementConnection::connect(&fixture.path("tickhost/host.sock"), &key)
        .await
        .unwrap()
        .hello()
        .clone();
    assert_eq!(
        hello.host.unwrap().build,
        tickhost::host::BUILD_IDENTITY,
        "the daemon started a Tickhost of its own build"
    );
    fixture.shutdown().await;
}

/// The tool results the Agent sent back so far, decoded.
fn tool_results(request: &Value) -> Vec<Value> {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "tool")
        .map(|message| {
            message["content"]
                .as_str()
                .and_then(|content| serde_json::from_str(content).ok())
                .unwrap_or_else(|| message["content"].clone())
        })
        .collect()
}

/// After the user restarts a Tickhost of another build, the Agent opens a protocol Tick in the
/// Tickhost of this build and reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_after_restarting_a_tickhost_of_another_build_opens_and_reads_a_protocol_tick() {
    let fixture = PrivateFixture::new();
    let (key, host) = other_build_running_a_tick(&fixture);
    let counter = fixture.path("work/counter.py");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tick/examples/counter.py"),
        &counter,
    )
    .unwrap();
    let open =
        json!({"action":"open","command":["/usr/bin/python3", "-u", counter],"protocol":true});
    let (base_url, mut requests) = model_http_support::serve_router(move |request| {
        let results = tool_results(request);
        match results.as_slice() {
            [] => chat_reply(Some(("TickManage", "open-counter", open.clone())), ""),
            [opened] => chat_reply(
                Some((
                    "TickRead",
                    "read-counter",
                    json!({"tick_instance_id": opened["tick_instance_id"]}),
                )),
                "",
            ),
            _ => chat_reply(None, "The counter reads 0."),
        }
    })
    .await;
    fixture.set_chat_model(&base_url);
    let mut view = PtyView::start_with_flags(&fixture, &["--dangerously-skip-permissions"]);

    restart_menu(&mut view).await;
    view.send(b"k");
    view.send(b"\r");
    view.wait_for("input ready", new_input_is_visible).await;
    assert!(host.was_stopped());

    view.send(b"Open the counter and read it.\r");
    view.wait_for("the Agent's reply", |frame| {
        frame.contains("The counter reads 0.")
    })
    .await;
    let mut last = Value::Null;
    while let Ok((_, request)) = requests.try_recv() {
        last = request;
    }
    let results = tool_results(&last);
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0]["status"], "opened", "{results:?}");
    assert_eq!(results[1]["source"], "protocol", "{results:?}");
    assert_eq!(results[1]["data"]["value"], 0, "{results:?}");
    let hello = ManagementConnection::connect(&fixture.path("tickhost/host.sock"), &key)
        .await
        .unwrap()
        .hello()
        .clone();
    assert_eq!(
        hello.host.unwrap().build,
        tickhost::host::BUILD_IDENTITY,
        "the counter runs in a Tickhost of this build"
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_cli_keeping_a_tickhost_of_another_build_refuses_ticks_and_shells_with_what_to_do() {
    let fixture = PrivateFixture::new();
    let (_key, host) = other_build_running_a_tick(&fixture);
    let (base_url, mut requests) = model_http_support::serve_responses(vec![
        (
            200,
            chat_reply(
                Some((
                    "TickManage",
                    "open-quotes",
                    json!({"action":"open","command":["/bin/sleep","30"]}),
                )),
                "",
            ),
        ),
        (200, chat_reply(None, "The Tick did not open.")),
    ])
    .await;
    fixture.set_chat_model(&base_url);
    let mut view = PtyView::start_with_flags(&fixture, &["--dangerously-skip-permissions"]);

    restart_menu(&mut view).await;
    view.send(b"\r");
    view.wait_for("input ready", new_input_is_visible).await;

    view.send(b"!printf never-runs\r");
    view.wait_for("the Shell refused with what to do", |frame| {
        let shown = words(frame);
        shown.contains(SHOWN_GUIDANCE) && !shown.contains("business protocol")
    })
    .await;

    view.send(b"Open the quotes.\r");
    view.wait_for("the Agent's reply", |frame| {
        frame.contains("The Tick did not open.")
    })
    .await;
    let _opening = requests.recv().await.unwrap();
    let (_, after_open) = requests.recv().await.unwrap();
    assert!(
        after_open.to_string().contains(OTHER_TICKHOST_GUIDANCE),
        "the model learns what to do: {after_open}"
    );
    assert!(!host.was_stopped(), "the old Tickhost keeps running");
    fixture.shutdown().await;
}
