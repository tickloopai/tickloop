use super::*;
use tickloop_agent_protocol::CommandState;

const HINT: &str = "(ctrl+b to run in background)";
const BACKGROUNDED_REPLY: &str = "BASH-BACKGROUNDED-REPLY";
const NOTIFIED_REPLY: &str = "BASH-NOTIFIED-REPLY";

async fn ctrl_b_backgrounds_the_models_running_bash(fixture: &PrivateFixture) {
    let (url, mut requests) = model_http_support::serve_responses(vec![
        (
            200,
            chat_reply(
                Some((
                    "Bash",
                    "foreground-gate",
                    json!({
                        "command": "printf 'gate began\\n'; while [ ! -f bash-release ]; do sleep 0.02; done; printf 'released\\n'",
                        "description": "Wait for the release file",
                    }),
                )),
                "",
            ),
        ),
        (200, chat_reply(None, BACKGROUNDED_REPLY)),
        (200, chat_reply(None, NOTIFIED_REPLY)),
    ])
    .await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start_with_flags(fixture, &["--dangerously-skip-permissions"]);
    view.wait_for("Bash background View ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    view.send(b"Wait for the release file.\r");
    let offered = view
        .wait_for("the running Bash offers ctrl+b", |frame| {
            frame.contains(HINT)
        })
        .await;
    let lines = offered.lines().collect::<Vec<_>>();
    let call = lines
        .iter()
        .position(|line| line.contains("Bash · Wait for the release file"))
        .unwrap_or_else(|| panic!("{offered}"));
    assert!(lines[call + 1].contains(HINT), "{offered}");
    let execution = catalog(&client).await[0].execution;
    let running = snapshot(&client, execution).await.commands[0].clone();
    assert_eq!(running.state, CommandState::Running);

    view.send(b"\x02");
    view.wait_for(
        "the Bash call returns to the model while its command keeps running",
        |frame| frame.contains(BACKGROUNDED_REPLY) && !frame.contains(HINT),
    )
    .await;
    let moved = snapshot(&client, execution).await;
    let command = &moved.commands[0];
    assert_eq!(command.state, CommandState::Background);
    assert_eq!(command.pid, running.pid);
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        request["messages"]
            .to_string()
            .contains("Wait for the release file"),
        "{request}"
    );
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        request["messages"].to_string().contains(&format!(
            "Command was manually backgrounded by user with ID: {}",
            running.operation_id
        )),
        "{request}"
    );

    std::fs::write(fixture.path("work/bash-release"), "release").unwrap();
    view.wait_for("the completion notification continues the goal", |frame| {
        frame.contains(NOTIFIED_REPLY)
    })
    .await;
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        request["messages"]
            .to_string()
            .contains("<status>completed</status>"),
        "{request}"
    );
    assert_eq!(
        std::fs::read_to_string(&running.output_ref).unwrap(),
        "gate began\nreleased\n"
    );
    view.close().await;
}

#[tokio::test]
async fn actual_cli_ctrl_b_backgrounds_the_models_running_bash() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(ctrl_b_backgrounds_the_models_running_bash(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
