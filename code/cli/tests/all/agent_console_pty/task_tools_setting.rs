use super::*;

const TASK_TOOLS: [&str; 4] = ["TaskCreate", "TaskGet", "TaskList", "TaskUpdate"];
const TASK_TOOLS_MENU_INDEX: usize = 14;
const ENABLED_PROMPT: &str = "Create a task and delegate its review.";
const DISABLED_PROMPT: &str = "Delegate again with task tools disabled.";
const REENABLED_PROMPT: &str = "Continue with task tools enabled.";

fn assert_task_tools(request: &Value, enabled: bool) {
    let names = request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<Vec<_>>();
    for name in TASK_TOOLS {
        assert_eq!(names.contains(&name), enabled, "{name}: {names:?}");
    }
    assert!(!names.contains(&"TodoWrite"));
}

/// Checks every request the model received so far, root and member alike, and takes at least
/// the root's final one.
fn assert_every_pending_request(
    requests: &mut tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
    enabled: bool,
) {
    let mut checked = 0;
    while let Ok((_, request)) = requests.try_recv() {
        assert_task_tools(&request, enabled);
        checked += 1;
    }
    assert!(checked > 0, "the work reached the model");
}

async fn toggle_task_tools(pty: &mut PtyView, enabled: bool) {
    pty.send(b"/settings\r");
    pty.wait_for("Settings opens", |frame| {
        frame.contains("Settings") && frame.contains("enter change")
    })
    .await;
    pty.send(&[b'j'; TASK_TOOLS_MENU_INDEX]);
    let before = if enabled { "Off" } else { "On" };
    pty.wait_for("Task tools setting is selected", |frame| {
        frame.lines().any(|line| {
            (line.contains(SELECTED_MARK) || line.trim_start().starts_with('❯'))
                && line.contains(&format!("Task tools: {before}"))
        })
    })
    .await;
    pty.send(b"\r");
    let after = if enabled { "On" } else { "Off" };
    pty.wait_for("Task tools setting changes immediately", |frame| {
        frame.lines().any(|line| {
            (line.contains(SELECTED_MARK) || line.trim_start().starts_with('❯'))
                && line.contains(&format!("Task tools: {after}"))
        })
    })
    .await;
    pty.send(b"\x1b");
    pty.wait_for_editor("Settings returns to the editor", Some(""))
        .await;
}

fn subagent_reply(name: &str) -> String {
    chat_reply(
        Some((
            "Subagent",
            name,
            json!({"name":name,"description":name,"task":"Report your result"}),
        )),
        "",
    )
}

async fn exercise_task_tools_setting(fixture: &PrivateFixture) {
    // Members run in the background, so replies follow who asks and what has arrived.
    let (base_url, mut requests) = model_http_support::serve_router(|request| {
        let history = request["messages"].to_string();
        match delegated_by(request).as_deref() {
            Some("enabled-member") => return chat_reply(None, "ENABLED-MEMBER-DONE"),
            Some(_) => return chat_reply(None, "DISABLED-MEMBER-DONE"),
            None => {}
        }
        let phase = [ENABLED_PROMPT, DISABLED_PROMPT, REENABLED_PROMPT]
            .into_iter()
            .max_by_key(|prompt| history.rfind(prompt).map_or(-1, |at| at as i64))
            .unwrap();
        match phase {
            ENABLED_PROMPT if !has_result(request, "create-shared-task") => chat_reply(
                Some((
                    "TaskCreate",
                    "create-shared-task",
                    json!({
                        "mutation_id":uuid::Uuid::new_v4(),
                        "title":"TASK-SAVED-WHEN-HIDDEN",
                        "description":"Check the authorized work"
                    }),
                )),
                "",
            ),
            ENABLED_PROMPT if !has_result(request, "enabled-member") => {
                subagent_reply("enabled-member")
            }
            ENABLED_PROMPT if history.contains("ENABLED-MEMBER-DONE") => {
                chat_reply(None, "ENABLED-ROOT-DONE")
            }
            DISABLED_PROMPT if !has_result(request, "disabled-member") => {
                subagent_reply("disabled-member")
            }
            DISABLED_PROMPT if history.contains("DISABLED-MEMBER-DONE") => {
                chat_reply(None, "DISABLED-ROOT-DONE")
            }
            REENABLED_PROMPT => chat_reply(None, "REENABLED-ROOT-DONE"),
            _ => chat_reply(None, "Review delegated"),
        }
    })
    .await;
    fixture.set_chat_model(&base_url);
    let mut pty = PtyView::launch(fixture, &["--dangerously-skip-permissions"], &[], true);
    pty.wait_for("task setting draft ready", new_input_is_visible)
        .await;
    pty.resize(100, 50).await;
    let client = fixture.client().await;
    assert!(catalog(&client).await.is_empty());
    toggle_task_tools(&mut pty, true).await;
    assert!(
        catalog(&client).await.is_empty(),
        "Settings must not create an Agent"
    );
    assert!(requests.try_recv().is_err());
    let saved: Value =
        serde_json::from_slice(&std::fs::read(fixture.path("config/settings.json")).unwrap())
            .unwrap();
    assert_eq!(saved["taskToolsEnabled"], true);

    pty.send(format!("{ENABLED_PROMPT}\r").as_bytes());
    pty.wait_for("shared task appears in the compact list", |frame| {
        frame.contains("ENABLED-ROOT-DONE")
            && frame
                .split_once("1 tasks (0 done, 1 open)")
                .is_some_and(|(_, tasks)| {
                    tasks.contains("TASK-SAVED-WHEN-HIDDEN") && tasks.contains("ctrl+t expand")
                })
    })
    .await;
    pty.wait_for_editor("enabled work completed", Some(""))
        .await;
    assert_every_pending_request(&mut requests, true);
    let root = catalog(&client)
        .await
        .into_iter()
        .find(|row| row.parent.is_none())
        .unwrap()
        .execution;
    let before = snapshot(&client, root).await;
    let board = before.conversation.task_boards.values().next().unwrap();
    assert_eq!(board.tasks.len(), 1);
    let task_id = board.tasks[0].id;

    let child_id = catalog(&client)
        .await
        .into_iter()
        .find(|row| row.parent.is_some())
        .unwrap()
        .chat_id
        .to_string();
    let mut child_view = PtyView::launch(fixture, &["--resume", &child_id], &[], true);
    child_view.resize(100, 50).await;
    child_view
        .wait_for("child View shares the task panel", |frame| {
            frame.contains("TASK-SAVED-WHEN-HIDDEN") && frame.contains("1 tasks (0 done, 1 open)")
        })
        .await;
    toggle_task_tools(&mut pty, false).await;
    assert!(requests.try_recv().is_err());
    pty.wait_for(
        "disabling tools immediately hides the task panel",
        |frame| !frame.contains("1 tasks (0 done, 1 open)"),
    )
    .await;
    child_view
        .wait_for(
            "idle child View hides tasks after the setting changes",
            |frame| !frame.contains("1 tasks (0 done, 1 open)"),
        )
        .await;
    let saved: Value =
        serde_json::from_slice(&std::fs::read(fixture.path("config/settings.json")).unwrap())
            .unwrap();
    assert_eq!(saved["taskToolsEnabled"], false);
    for member in catalog(&client).await {
        assert!(
            !snapshot(&client, member.execution)
                .await
                .conversation
                .task_tools_enabled
        );
    }
    pty.send(format!("{DISABLED_PROMPT}\r").as_bytes());
    pty.wait_for("disabled work completed", |frame| {
        frame.contains("DISABLED-ROOT-DONE")
    })
    .await;
    pty.wait_for_editor("disabled work leaves editor ready", Some(""))
        .await;
    assert_every_pending_request(&mut requests, false);
    let after = snapshot(&client, root).await;
    assert_eq!(
        after
            .conversation
            .task_boards
            .values()
            .next()
            .unwrap()
            .tasks[0]
            .id,
        task_id
    );
    toggle_task_tools(&mut pty, true).await;
    assert!(requests.try_recv().is_err());
    pty.wait_for("reenabling tools restores the same task panel", |frame| {
        frame
            .split_once("1 tasks (0 done, 1 open)")
            .is_some_and(|(_, tasks)| tasks.contains("TASK-SAVED-WHEN-HIDDEN"))
    })
    .await;
    child_view
        .wait_for("idle child View restores tasks after reenabling", |frame| {
            frame.contains("1 tasks (0 done, 1 open)") && frame.contains("TASK-SAVED-WHEN-HIDDEN")
        })
        .await;
    child_view.close().await;
    pty.send(format!("{REENABLED_PROMPT}\r").as_bytes());
    pty.wait_for("re-enabled work completed", |frame| {
        frame.contains("REENABLED-ROOT-DONE")
    })
    .await;
    let request = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap()
        .1;
    assert_task_tools(&request, true);
    pty.wait_for_editor("re-enabled work leaves editor ready", Some(""))
        .await;
    pty.close().await;
}

#[tokio::test]
async fn actual_cli_settings_toggle_task_tools_for_main_and_subagents_without_model_calls() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(exercise_task_tools_setting(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn exercise_default_task_tools(fixture: &PrivateFixture) {
    let (base_url, mut requests) = model_http_support::serve_responses(vec![
        (200, subagent_reply("default-member")),
        (200, chat_reply(None, "DEFAULT-MEMBER-DONE")),
        (200, chat_reply(None, "DEFAULT-ROOT-DONE")),
    ])
    .await;
    fixture.set_chat_model(&base_url);
    let mut pty = PtyView::launch(fixture, &["--dangerously-skip-permissions"], &[], true);
    pty.wait_for("default task setting draft ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    pty.send(b"Delegate one check using default settings.\r");
    pty.wait_for("default main and child completed", |frame| {
        frame.contains("DEFAULT-ROOT-DONE")
    })
    .await;
    for _ in 0..3 {
        let request = tokio::time::timeout(UI_TIMEOUT, requests.recv())
            .await
            .unwrap()
            .unwrap()
            .1;
        assert_task_tools(&request, false);
    }
    let members = catalog(&client).await;
    assert_eq!(members.len(), 2);
    for member in members {
        let current = snapshot(&client, member.execution).await;
        assert!(!current.conversation.task_tools_enabled);
        assert!(
            current
                .conversation
                .task_boards
                .values()
                .all(|board| board.tasks.is_empty())
        );
    }
    pty.close().await;
}

#[tokio::test]
async fn actual_cli_default_subagent_creation_does_not_enable_task_tools() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(exercise_default_task_tools(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
