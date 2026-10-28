use std::{fs, path::Path, sync::Arc, time::Duration};
use tickloop_code_settings::local_agent::LocalAgent;

use tickloop_harness::model::ModelRequest;
use tickloop_harness::protocol::{ChatId, Content, EventKind, Role};
use tickloop_harness::runtime::Agent;
use tickloop_harness::test_support::{ScriptedModel, config, text};

const PREAMBLE: &str = "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# claudeMd\nCodebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.\n\n";
const DATE: &str = "# currentDate\nToday's date is 2026-10-27.";
const RELEVANCE_NOTE: &str = "\n\n      Tickloop attached this context automatically; it isn't part of the user's message. It describes the user's own account and workspace, so they don't need it reported back.\n</system-reminder>\n";

/// Late on the 26th in UTC is already the 27th in Shanghai, the user's zone.
fn set_clock(agent: &Agent) {
    use chrono::TimeZone;
    agent.set_scheduled_now_for_tests(
        chrono::Utc
            .with_ymd_and_hms(2026, 10, 26, 23, 30, 0)
            .unwrap(),
    );
    agent.set_time_zone("Asia/Shanghai").unwrap();
}

async fn complete_turn(agent: &Agent, chat: ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::Completed { .. } => return,
                EventKind::Failed(error) => panic!("turn failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("turn timed out");
}

fn text_of(request: &ModelRequest, index: usize) -> (Role, String) {
    let message = &request.messages[index];
    let text = message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    (message.role, text)
}

/// The first request that carries `prompt`: the one its own turn sent.
fn request_for(model: &ScriptedModel, prompt: &str) -> ModelRequest {
    model
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| {
            request.messages.iter().any(|message| {
                message.role == Role::User
                    && message
                        .content
                        .iter()
                        .any(|content| matches!(content, Content::Text { text } if text == prompt))
            })
        })
        .cloned()
        .expect("no request for the prompt")
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[tokio::test]
async fn instruction_files_lead_each_request_and_reload_after_compaction() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let project = directory.path().canonicalize().unwrap().join("project");
    write(&root.join("AGENTS.md"), "Global rule.");
    write(&project.join("AGENTS.md"), "Project rule.");
    write(&project.join("CLAUDE.md"), "Not loaded beside AGENTS.md.");
    write(&project.join("CLAUDE.local.md"), "Private rule.");
    let model = Arc::new(ScriptedModel::new(vec![
        text("first answer"),
        text("second answer"),
        text("compacted summary"),
        text("third answer"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    set_clock(&agent);
    let chat = agent.create_chat(project.clone()).await.unwrap();
    let root = root.canonicalize().unwrap();

    complete_turn(&agent, chat, "first prompt").await;
    let first = request_for(&model, "first prompt");
    assert_eq!(first.messages[0].role, Role::System);
    assert_eq!(
        text_of(&first, 1),
        (
            Role::User,
            format!(
                "{PREAMBLE}Contents of {} (user's private global instructions for all projects):\n\nGlobal rule.\n\nContents of {} (project instructions, checked into the codebase):\n\nProject rule.\n\nContents of {} (user's private project instructions, not checked in):\n\nPrivate rule.\n{DATE}{RELEVANCE_NOTE}",
                root.join("AGENTS.md").display(),
                project.join("AGENTS.md").display(),
                project.join("CLAUDE.local.md").display(),
            )
        )
    );
    assert_eq!(text_of(&first, 2), (Role::User, "first prompt".into()));

    write(&project.join("AGENTS.md"), "Changed project rule.");
    complete_turn(&agent, chat, "second prompt").await;
    let second = request_for(&model, "second prompt");
    assert_eq!(text_of(&second, 1), text_of(&first, 1));

    agent.compact(chat).await.unwrap();
    complete_turn(&agent, chat, "third prompt").await;
    let (_, reloaded) = text_of(&request_for(&model, "third prompt"), 1);
    assert!(reloaded.contains("\n\nChanged project rule.\n\n"));
    assert!(!reloaded.contains("Not loaded"));
}

#[tokio::test]
async fn a_chat_without_instruction_files_hears_only_the_date() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("answer")]));
    let agent =
        Agent::open_embedded(directory.path().join("root"), config(), model.clone()).unwrap();
    set_clock(&agent);
    let chat = agent.create_chat(project).await.unwrap();

    complete_turn(&agent, chat, "only prompt").await;
    let request = request_for(&model, "only prompt");
    assert_eq!(
        text_of(&request, 1),
        (
            Role::User,
            format!(
                "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n{DATE}{RELEVANCE_NOTE}"
            )
        )
    );
    assert_eq!(text_of(&request, 2), (Role::User, "only prompt".into()));
}

#[tokio::test]
async fn a_subagent_receives_the_instructions_its_parent_loaded() {
    use tickloop_harness::test_support::{
        call,
        routed::{RoutedModel, has_tool_result, state},
    };
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().canonicalize().unwrap().join("project");
    let rules = project.join("AGENTS.md");
    write(&rules, "Rule when the parent started.");
    let changed_rules = rules.clone();
    let model = Arc::new(RoutedModel::new(
        move |request| match state(request)["delegation"]["task"].as_str() {
            Some("child") => text("child report"),
            _ if !has_tool_result(request, "delegate") => {
                fs::write(&changed_rules, "Rule written after the parent started.").unwrap();
                call(
                    "delegate",
                    "Subagent",
                    serde_json::json!({"task": "child", "name": "child"}),
                )
            }
            _ => text("parent report"),
        },
    ));
    let agent =
        Agent::open_embedded(directory.path().join("root"), config(), model.clone()).unwrap();
    let chat = agent.create_chat(project).await.unwrap();

    complete_turn(&agent, chat, "delegate").await;
    let requests = model.requests.lock().unwrap();
    let parent = requests
        .iter()
        .find(|request| state(request)["delegation"].is_null())
        .unwrap();
    let child = requests
        .iter()
        .find(|request| state(request)["delegation"]["task"] == "child")
        .unwrap();
    assert!(
        text_of(parent, 1)
            .1
            .contains("Rule when the parent started.")
    );
    assert_eq!(text_of(child, 1), text_of(parent, 1));
}

#[tokio::test]
async fn a_new_day_is_announced_once_at_the_end_and_the_user_context_stays() {
    use chrono::TimeZone;
    const DATE_CHANGE: &str = "<system-reminder>\nThe date has changed. Today's date is now 2026-10-28. No need to announce the new date \u{2014} the user's own clock shows it.\n</system-reminder>";
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(ScriptedModel::new(vec![
        text("first answer"),
        text("second answer"),
        text("third answer"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    set_clock(&agent);
    let chat = agent.create_chat(project).await.unwrap();

    complete_turn(&agent, chat, "first prompt").await;
    agent.set_scheduled_now_for_tests(
        chrono::Utc
            .with_ymd_and_hms(2026, 10, 27, 17, 0, 0)
            .unwrap(),
    );
    complete_turn(&agent, chat, "second prompt").await;
    complete_turn(&agent, chat, "third prompt").await;

    let first = request_for(&model, "first prompt");
    let second = request_for(&model, "second prompt");
    let third = request_for(&model, "third prompt");
    assert_eq!(text_of(&second, 1), text_of(&first, 1));
    let last = second.messages.len() - 1;
    assert_eq!(text_of(&second, last), (Role::User, DATE_CHANGE.into()));
    assert_eq!(
        text_of(&second, last - 1),
        (Role::User, "second prompt".into())
    );
    let mentions = |request: &ModelRequest| {
        (0..request.messages.len())
            .filter(|&index| text_of(request, index).1 == DATE_CHANGE)
            .count()
    };
    assert_eq!(mentions(&third), 1);
    assert_eq!(text_of(&third, 1), text_of(&first, 1));
    agent.shutdown().await;

    let resumed_model = Arc::new(ScriptedModel::new(vec![text("fourth answer")]));
    let resumed = Agent::open_embedded(root, config(), resumed_model.clone()).unwrap();
    set_clock(&resumed);
    resumed.set_scheduled_now_for_tests(
        chrono::Utc
            .with_ymd_and_hms(2026, 10, 27, 18, 0, 0)
            .unwrap(),
    );
    resumed.resume_chat(chat).await.unwrap();
    complete_turn(&resumed, chat, "fourth prompt").await;
    let fourth = request_for(&resumed_model, "fourth prompt");
    assert!(
        text_of(&fourth, 1)
            .1
            .contains("Today's date is 2026-10-28.")
    );
    assert_eq!(mentions(&fourth), 1);
}
