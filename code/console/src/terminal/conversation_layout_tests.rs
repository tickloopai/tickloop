//! The conversation laid out as Claude Code lays it out: one blank row above every block, each
//! result under its own call, reads and searches collapsed, and the same rows whether a View
//! follows a run's events or reopens its saved history.
use super::*;
use agent_messaging::AgentColors;
use async_trait::async_trait;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tickloop_code_access::ExecutionRef;
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    Content, Decision, Event, LocalMessage, ModelIdentity, ModelProvider, PermissionRule,
    PermissionScope, ProposedToolCall, Protocol, QuestionAnswer, ToolResult, Usage,
};

use tickloop_harness::tools::ASK_USER_QUESTION_TOOL_NAME;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MARKDOWN_REPLY: &str =
    "I checked the build setup.\n\n- First item\n- Second item\n\n```sh\ncargo build\n```";

/// One scripted reply: thinking streamed before the content, which streams its text.
struct Reply {
    thinking: Option<&'static str>,
    content: Vec<Content>,
}

impl Reply {
    fn of(content: Vec<Vec<Content>>) -> Self {
        Self {
            thinking: None,
            content: content.concat(),
        }
    }
}

/// Answers each request with the next scripted reply, streaming it as a provider does.
struct StreamedScript {
    replies: Mutex<VecDeque<Reply>>,
}

#[async_trait]
impl ModelClient for StreamedScript {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_harness::model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::InvalidResponse("script exhausted".into()))?;
        let mut content = Vec::new();
        if let Some(thinking) = reply.thinking {
            events.send(ModelDelta::Thinking(thinking.into())).unwrap();
            content.push(Content::Thinking {
                thinking: thinking.into(),
                signature: None,
            });
        }
        for block in &reply.content {
            if let Content::Text { text } = block {
                events.send(ModelDelta::Text(text.clone())).unwrap();
            }
        }
        content.extend(reply.content);
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(20),
                ..Usage::default()
            },
            stop_reason: "end_turn".into(),
        })
    }
}

fn rule(decision: Decision, tool: &str) -> PermissionRule {
    PermissionRule {
        id: Uuid::new_v4(),
        decision,
        tool: tool.into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    }
}

/// Waits for `chat`'s run to end, declining every permission it asks for.
async fn finish_run(
    agent: &Agent,
    chat: Uuid,
    events: &mut tokio::sync::broadcast::Receiver<tickloop_harness::protocol::Event>,
) {
    loop {
        match tickloop_harness::test_support::next_event(events)
            .await
            .kind
        {
            EventKind::PermissionRequested { request } => agent
                .answer_permission_with_comment(
                    chat,
                    request.id,
                    false,
                    PermissionScope::Once,
                    None,
                )
                .await
                .unwrap(),
            EventKind::Completed { .. } | EventKind::Interrupted => return,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

/// The conversation of `snapshot` as a View following its events draws it.
fn followed(chat: Uuid, workdir: &std::path::Path, snapshot: &ChatSnapshot) -> String {
    followed_view(chat, workdir, snapshot, AgentColors::default()).test_conversation_text()
}

fn followed_view(
    chat: Uuid,
    workdir: &std::path::Path,
    snapshot: &ChatSnapshot,
    colors: AgentColors,
) -> Console {
    let console = Console::test_interactive_for_chat(chat);
    follow_in(&console, chat, workdir, snapshot, colors);
    console
}

fn follow_in(
    console: &Console,
    chat: Uuid,
    workdir: &std::path::Path,
    snapshot: &ChatSnapshot,
    colors: AgentColors,
) {
    console.remember_agent_colors(colors).unwrap();
    console.set_input_context(Some(chat), workdir, &[]).unwrap();
    let mut tools = ToolPresentation::new(false);
    render_restored_observation(
        console,
        &mut tools,
        chat.into(),
        0,
        &mut HashMap::new(),
        &mut MessageGroups::default(),
        ExecutionObservation {
            execution: ExecutionRef {
                daemon_epoch: Uuid::nil(),
                agent_id: chat,
                generation: 1,
            },
            events: snapshot.conversation_events(),
            snapshot: snapshot.clone(),
            reset_required: false,
        },
    )
    .unwrap();
}

/// The conversation of `snapshot` as a View reopening its history draws it.
fn reopened(chat: Uuid, snapshot: &ChatSnapshot) -> String {
    reopened_view(chat, snapshot, AgentColors::default()).test_conversation_text()
}

fn reopened_view(chat: Uuid, snapshot: &ChatSnapshot, colors: AgentColors) -> Console {
    let console = Console::test_interactive_for_chat(chat);
    console.remember_agent_colors(colors).unwrap();
    workflows::write_public_history(&console, snapshot).unwrap();
    console
}

#[tokio::test]
async fn the_cache_summary_ends_the_finished_line_whether_followed_or_reopened() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let model = StreamedScript {
        replies: Mutex::new(
            [Reply::of(vec![tickloop_harness::test_support::text(
                "Checked.",
            )])]
            .into(),
        ),
    };
    let agent = Agent::new_embedded(tickloop_harness::test_support::config(), Arc::new(model));
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Check it".into()).await.unwrap();
    finish_run(&agent, chat, &mut events).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let conversation =
        |finished: &str| ["", "❯ Check it", "", "⏺ Checked.", "", finished, ""].join("\n");

    let followed = Console::test_interactive_for_chat(chat);
    followed.test_show_debug_details();
    follow_in(&followed, chat, &workdir, &snapshot, AgentColors::default());
    let reopened = Console::test_interactive_for_chat(chat);
    reopened.test_show_debug_details();
    workflows::write_public_history(&reopened, &snapshot).unwrap();
    for view in [&followed, &reopened] {
        // The fixture reports no cache counters, and a View without mouse capture has no button.
        assert_eq!(
            view.test_conversation_text(),
            conversation("⏺ Done · cache n/a")
        );
        let finished = view
            .test_conversation_rows()
            .into_iter()
            .find(|row| row.spans.iter().any(|span| span.text.contains("Done")))
            .unwrap();
        assert!(
            finished
                .spans
                .iter()
                .all(|span| span.text.trim().is_empty() || span.role == Some(Role::MutedText)),
            "{:?}",
            finished.spans
        );
        let details = view.test_debug_details().unwrap();
        assert!(
            details.starts_with(&format!(
                "chat: {}\nrun: {}\nturn: 1\nmodel: fixture\n",
                ChatId::from(chat).public(),
                receipt.run_id.public()
            )),
            "{details}"
        );
    }
    assert_eq!(
        followed_view(chat, &workdir, &snapshot, AgentColors::default()).test_conversation_text(),
        conversation("⏺ Done"),
        "without the switch the finished line stays as it was"
    );

    // A Chat whose ledger no longer holds the run, as after the Agent's process restarted.
    let mut forgotten = snapshot.clone();
    forgotten.request_usage = Default::default();
    let restored = Console::test_interactive_for_chat(chat);
    restored.test_show_debug_details();
    workflows::write_public_history(&restored, &forgotten).unwrap();
    assert_eq!(restored.test_conversation_text(), conversation("⏺ Done"));
    assert_eq!(restored.test_debug_details(), None);
}

#[tokio::test]
async fn followed_and_reopened_conversations_share_one_claude_code_layout() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    std::fs::write(
        workdir.join("notes.txt"),
        "Build notes\nTODO: fix cache\nKeep the ui stage fast\n",
    )
    .unwrap();
    let path = |name: &str| workdir.join(name).display().to_string();
    let call = tickloop_harness::test_support::call;
    let text = tickloop_harness::test_support::text;
    let model = StreamedScript {
        replies: Mutex::new(
            [
                Reply {
                    thinking: Some("Look at the build first."),
                    content: [
                        text(MARKDOWN_REPLY),
                        call(
                            "bash",
                            "Bash",
                            serde_json::json!({
                                "command": "printf 'one\\ntwo\\n'",
                                "description": "Print two lines"
                            }),
                        ),
                    ]
                    .concat(),
                },
                Reply::of(vec![
                    call(
                        "read",
                        "Read",
                        serde_json::json!({"file_path": path("notes.txt")}),
                    ),
                    call(
                        "grep",
                        "Grep",
                        serde_json::json!({"pattern": "TODO", "path": path("")}),
                    ),
                ]),
                Reply::of(vec![call(
                    "missing",
                    "Read",
                    serde_json::json!({"file_path": path("missing.txt")}),
                )]),
                Reply::of(vec![
                    text("Cleaning up."),
                    call(
                        "write",
                        "Write",
                        serde_json::json!({"file_path": path("build.log"), "content": "stale\n"}),
                    ),
                ]),
                Reply::of(vec![call(
                    "edit",
                    "Edit",
                    serde_json::json!({
                        "file_path": path("notes.txt"),
                        "old_string": "TODO: fix cache",
                        "new_string": "DONE: fix cache"
                    }),
                )]),
                Reply::of(vec![text("All set.")]),
            ]
            .into(),
        ),
    };
    let agent = Agent::new_embedded(tickloop_harness::test_support::config(), Arc::new(model));
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![rule(Decision::Ask, "Write"), rule(Decision::Allow, "Bash")],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Audit the build".into()).await.unwrap();
    finish_run(&agent, chat, &mut events).await;
    agent
        .submit(chat, "Continue with the edit".into())
        .await
        .unwrap();
    finish_run(&agent, chat, &mut events).await;
    let snapshot = agent.snapshot(chat).await.unwrap();

    let followed = followed(chat, &workdir, &snapshot);
    assert_eq!(
        followed,
        reopened(chat, &snapshot),
        "a followed run and its reopened history draw the same rows"
    );
    assert_eq!(
        followed,
        [
            "",
            "❯ Audit the build",
            "",
            "∴ Thinking (ctrl+o to expand)",
            "",
            "⏺ I checked the build setup.",
            "",
            "  - First item",
            "  - Second item",
            "",
            "  cargo build",
            "",
            "⏺ Bash · Print two lines",
            "  ⎿  one",
            "     two",
            "",
            "  Searched for 1 pattern, read 2 files (ctrl+o to expand)",
            "",
            "⏺ Cleaning up.",
            "",
            "⏺ Write · build.log",
            "  ⎿  Denied by you",
            "     Explicit Ask: an explicit Ask rule requires human approval",
            "",
            "❯ Continue with the edit",
            "",
            "⏺ Edit · notes.txt",
            "  ⎿  Added 1 line, removed 1 line",
            "      1  Build notes",
            "      2 -TODO: fix cache",
            "      2 +DONE: fix cache",
            "      3  Keep the ui stage fast",
            "",
            "⏺ All set.",
            "",
            "⏺ Done",
            "",
        ]
        .join("\n")
    );
    let transcript = presentation::transcript_document(
        &snapshot,
        DisplayProfile::default(),
        &Default::default(),
        &Default::default(),
    )
    .plain_text(120);
    for expanded in [
        "⏺ Read(",
        "⏺ Grep(",
        "Error: file or directory does not exist",
    ] {
        assert!(
            transcript.contains(expanded),
            "Ctrl+O shows each collapsed call: {transcript}"
        );
    }
    agent.shutdown().await;
}

/// AskUserQuestion as Claude Code draws it: no call row, the answers under one heading, and a
/// decline as one line that stands for the run's interruption; the same rows whether a View
/// follows the run, reopens it or shows its Ctrl+O transcript.
#[tokio::test]
async fn questions_show_only_their_answers_or_decline_alike() {
    let directory = tempfile::tempdir().unwrap();
    let call = tickloop_harness::test_support::call;
    let option = |label: &str| serde_json::json!({"label": label, "description": label});
    let model = tickloop_harness::test_support::ScriptedModel::new(vec![
        call(
            "report",
            ASK_USER_QUESTION_TOOL_NAME,
            serde_json::json!({"questions": [
                {"question": "Which report?", "header": "Report",
                 "options": [option("Weekly"), option("Monthly")]},
                {"question": "Which sections?", "header": "Sections", "multiSelect": true,
                 "options": [option("Summary"), option("Risks"), option("Charts")]}
            ]}),
        ),
        call(
            "scope",
            ASK_USER_QUESTION_TOOL_NAME,
            serde_json::json!({"questions": [
                {"question": "Which scope?", "header": "Scope",
                 "options": [option("Narrow"), option("Wide")]}
            ]}),
        ),
    ]);
    let agent = Agent::new_embedded(tickloop_harness::test_support::config(), Arc::new(model));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Shorten the report".into())
        .await
        .unwrap();
    let mut answered = false;
    loop {
        match tickloop_harness::test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::QuestionAsked { request } if !answered => {
                answered = true;
                agent
                    .answer_question(
                        chat,
                        request.id,
                        vec![
                            QuestionAnswer::option("Weekly"),
                            QuestionAnswer {
                                options: vec!["Summary".into(), "Charts".into()],
                                other: None,
                            },
                        ],
                    )
                    .await
                    .unwrap();
            }
            EventKind::QuestionAsked { request } => {
                agent.decline_question(chat, request.id).await.unwrap();
            }
            EventKind::Completed { .. } | EventKind::Interrupted => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();

    let followed = followed(chat, directory.path(), &snapshot);
    assert_eq!(
        followed,
        reopened(chat, &snapshot),
        "a followed run and its reopened history draw the same rows"
    );
    let rows = [
        "⏺ User answered the Agent's questions:",
        "  ⎿  · Which report? → Weekly",
        "     · Which sections? → Summary, Charts",
        "",
        "⏺ User declined to answer questions",
    ]
    .join("\n");
    assert_eq!(
        followed,
        format!("\n❯ Shorten the report\n\n{rows}\n"),
        "no call row and no separate interruption line"
    );
    let transcript = presentation::transcript_document(
        &snapshot,
        DisplayProfile::default(),
        &Default::default(),
        &Default::default(),
    )
    .plain_text(120);
    assert!(transcript.contains(&rows), "{transcript}");
    assert!(
        !transcript.contains(ASK_USER_QUESTION_TOOL_NAME),
        "{transcript}"
    );
    agent.shutdown().await;
}

/// A family run in steps: the child that the call `background_call` delegated waits at each of
/// its turns until the test lets it go on.
struct FamilyScript {
    routed: tickloop_harness::test_support::routed::RoutedModel,
    background_call: &'static str,
    background_turns: tokio::sync::Semaphore,
}

const FOREGROUND_REPORT: &str = "Config is valid.\nTwo keys are unused.\nOne key is deprecated.";
const BACKGROUND_NOTE: &str = "Found 3 errors so far.\nStill scanning the archive.";
const BACKGROUND_REPORT: &str = "Scan complete: 3 errors.";

impl FamilyScript {
    fn new(
        background_call: &'static str,
        respond: impl Fn(&ModelRequest) -> Vec<Content> + Send + Sync + 'static,
    ) -> Self {
        Self {
            routed: tickloop_harness::test_support::routed::RoutedModel::new(respond),
            background_call,
            background_turns: tokio::sync::Semaphore::new(0),
        }
    }

    /// The root delegates a foreground child that reports and a background one that writes to
    /// it before it reports.
    fn reporting() -> Self {
        use tickloop_harness::test_support::routed::{has_tool_result, history_text, state};
        let call = tickloop_harness::test_support::call;
        let text = tickloop_harness::test_support::text;
        Self::new("logs", move |request| {
            let delegation = &state(request)["delegation"];
            let history = history_text(request);
            if delegation["parent_call_id"] == "config" {
                call(
                    "config-report",
                    "SubagentHandback",
                    serde_json::json!({ "message": FOREGROUND_REPORT }),
                )
            } else if !delegation.is_null() && !has_tool_result(request, "logs-note") {
                call(
                    "logs-note",
                    "SendMessage",
                    serde_json::json!({ "to": "parent", "message": BACKGROUND_NOTE }),
                )
            } else if !delegation.is_null() {
                call(
                    "logs-report",
                    "SubagentHandback",
                    serde_json::json!({ "message": BACKGROUND_REPORT }),
                )
            } else if !has_tool_result(request, "config") {
                call(
                    "config",
                    "Subagent",
                    serde_json::json!({
                        "task": "Check the config files",
                        "name": "config",
                        "description": "Check the config",
                        "run_in_background": false
                    }),
                )
            } else if !has_tool_result(request, "logs") {
                call(
                    "logs",
                    "Subagent",
                    serde_json::json!({
                        "task": "Scan the logs for errors",
                        "name": "logs",
                        "description": "Scan the logs",
                        "run_in_background": true
                    }),
                )
            } else if history.contains(BACKGROUND_REPORT) {
                text("The scan is done.")
            } else if history.contains("Found 3 errors so far.") {
                text("Noted.")
            } else {
                text("Waiting for the logs.")
            }
        })
    }
}

#[async_trait]
impl ModelClient for FamilyScript {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_harness::model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        self.routed.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let delegation = &tickloop_harness::test_support::routed::state(&request)["delegation"];
        if delegation["parent_call_id"] == self.background_call {
            self.background_turns.acquire().await.unwrap().forget();
        }
        self.routed.complete(request, events, cancel).await
    }
}

/// Waits until `chat` has no run left and `ready` holds for its conversation.
async fn settle(agent: &Agent, chat: Uuid, ready: impl Fn(&ChatSnapshot) -> bool) -> ChatSnapshot {
    let latest = Mutex::new(None);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            *latest.lock().unwrap() = Some(format!(
                "{:?}\n{:?}\n{:?}",
                snapshot.chat_loop,
                snapshot.messages,
                snapshot
                    .events
                    .iter()
                    .map(|event| &event.kind)
                    .collect::<Vec<_>>()
            ));
            let idle = snapshot.pending_inputs.is_empty()
                && !matches!(
                    snapshot.chat_loop.state,
                    tickloop_harness::protocol::LoopState::Running
                        | tickloop_harness::protocol::LoopState::Queued
                );
            if idle && ready(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the family settles: {}",
            latest.lock().unwrap().as_ref().unwrap()
        )
    })
}

fn says(snapshot: &ChatSnapshot, reply: &str) -> bool {
    snapshot.messages.iter().any(|message| {
        message.role == tickloop_harness::protocol::Role::Assistant
            && message
                .content
                .iter()
                .any(|content| matches!(content, Content::Text { text } if text == reply))
    })
}

/// A root Chat in `mode` whose family `model` answers; every automatic review allows. The host is
/// non-interactive, so a Subagent call may wait in the foreground.
async fn family(
    model: Arc<FamilyScript>,
    mode: ExecutionMode,
    workdir: &std::path::Path,
) -> (tickloop_harness::runtime::Agent, Agent, Uuid) {
    let core = tickloop_harness::runtime::Agent::new_embedded_with_subagent_launch(
        tickloop_harness::test_support::config(),
        model,
        tickloop_harness::runtime::SubagentLaunch::BackgroundByDefault,
    );
    core.set_auto_reviewer(Arc::new(
        tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::text(
                &tickloop_harness::test_support::auto_classification(false).to_string(),
            );
            20
        ]),
    ))
    .await;
    let agent = Agent::from_embedded(core.clone(), tickloop_harness::test_support::config());
    let chat = agent.create_chat(workdir.into()).await.unwrap();
    agent.set_execution_mode(chat, mode).await.unwrap();
    (core, agent, chat)
}

fn elapsed(snapshot: &ChatSnapshot, handle: &str) -> String {
    let completion = snapshot
        .family_completions
        .iter()
        .find(|completion| completion.handle == handle)
        .unwrap();
    presentation::compact_duration(completion.finished_at - completion.started_at.unwrap())
}

#[tokio::test]
async fn followed_and_reopened_family_conversations_show_agent_communication_alike() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let model = Arc::new(FamilyScript::reporting());
    let (core, agent, chat) = family(model.clone(), ExecutionMode::Auto, &workdir).await;
    agent
        .submit(chat, "Check the config, then scan the logs".into())
        .await
        .unwrap();
    settle(&agent, chat, |snapshot| {
        says(snapshot, "Waiting for the logs.")
    })
    .await;
    model.background_turns.add_permits(1);
    settle(&agent, chat, |snapshot| says(snapshot, "Noted.")).await;
    model.background_turns.add_permits(1);
    let snapshot = settle(&agent, chat, |snapshot| {
        says(snapshot, "The scan is done.") && snapshot.family_completions.len() == 2
    })
    .await;
    let colors = agent_messaging::catalog_colors(&agent.catalog().await.unwrap());

    let followed = followed_view(chat, &workdir, &snapshot, colors.clone());
    let reopened = reopened_view(chat, &snapshot, colors.clone());
    let text = followed.test_conversation_text();
    assert_eq!(
        text,
        reopened.test_conversation_text(),
        "a followed family run and its reopened history draw the same rows"
    );
    assert_eq!(
        text,
        [
            "",
            "❯ Check the config, then scan the logs",
            "",
            "⏺ Subagent(@config · Check the config)",
            "  ⎿  Config is valid.",
            "     Two keys are unused. (ctrl+o to expand)",
            "  ⎿  Allowed by Auto review (LLM)",
            "",
            "⏺ Subagent(@logs · Scan the logs)",
            "  ⎿  Backgrounded agent (ctrl+o to expand)",
            "  ⎿  Allowed by Auto review (LLM)",
            "",
            "⏺ Waiting for the logs.",
            "",
            "› Message from @logs (ctrl+o to expand)",
            "",
            "⏺ Noted.",
            "",
            &format!("⏺ Teammate @logs finished · {}", elapsed(&snapshot, "logs")),
            "  Scan complete: 3 errors.",
            "",
            "⏺ The scan is done.",
            "",
            "⏺ Done",
            "",
        ]
        .join("\n")
    );
    for hidden in [
        "delivered its report",
        "Subagent hand-back",
        "Found 3 errors so far.",
        "Progress",
    ] {
        assert!(!text.contains(hidden), "{hidden}: {text}");
    }
    let logs_color = snapshot
        .family_completions
        .iter()
        .find(|completion| completion.handle == "logs")
        .and_then(|completion| colors[&completion.child_chat_id.uuid()])
        .expect("the runtime gave the child an Agent Color");
    for view in [&followed, &reopened] {
        let rows = view.test_conversation_rows();
        let handles = rows
            .iter()
            .flat_map(|row| &row.spans)
            .filter(|span| span.text == "@logs")
            .collect::<Vec<_>>();
        assert_eq!(handles.len(), 2, "call and completion name @logs");
        assert!(
            handles
                .iter()
                .all(|span| span.role == Some(Role::agent(logs_color))),
            "the handle takes the child's Agent Color"
        );
        let message = rows
            .iter()
            .find(|row| {
                row.spans
                    .iter()
                    .any(|span| span.text.contains("Message from"))
            })
            .unwrap();
        assert!(
            message
                .spans
                .iter()
                .all(|span| span.role == Some(Role::MutedText)),
            "the folded message is one dim line"
        );
        let completion_dot = rows
            .iter()
            .find(|row| row.spans.iter().any(|span| span.text.contains(" finished")))
            .and_then(|row| row.spans.first())
            .unwrap();
        assert_eq!(completion_dot.role, Some(Role::Success));
    }
    let transcript = presentation::transcript_document(
        &snapshot,
        DisplayProfile::default(),
        &Default::default(),
        &colors,
    )
    .plain_text(120);
    for expanded in [
        "⏺ Subagent(@config · Check the config)",
        "One key is deprecated.",
        "⏺ Subagent(@logs · Scan the logs)\n  ⎿  Backgrounded agent\n     Prompt:\n       Scan the logs for errors\n",
        "› Message from @logs\n  Found 3 errors so far.\n  Still scanning the archive.\n",
        &format!(
            "⏺ Teammate @logs finished · {}\n  {BACKGROUND_REPORT}\n",
            elapsed(&snapshot, "logs")
        ),
    ] {
        assert!(
            transcript.contains(expanded),
            "Ctrl+O shows {expanded:?}: {transcript}"
        );
    }
    assert!(!transcript.contains("delivered its report"), "{transcript}");
    core.shutdown().await;
}

#[tokio::test]
async fn a_background_child_asked_for_no_report_finishes_as_one_block_alike() {
    use tickloop_harness::test_support::routed::{has_tool_result, history_text, state};
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let model = Arc::new(FamilyScript::new("scan", |request| {
        if !state(request)["delegation"].is_null() {
            tickloop_harness::test_support::text("Background done.\nAll clear.\nNothing else.")
        } else if !has_tool_result(request, "scan") {
            tickloop_harness::test_support::call(
                "scan",
                "Subagent",
                serde_json::json!({
                    "task": "Scan quietly in the background",
                    "name": "scan",
                    "run_in_background": true
                }),
            )
        } else if history_text(request).contains("Background done.") {
            tickloop_harness::test_support::text("Thanks.")
        } else {
            tickloop_harness::test_support::text("Started.")
        }
    }));
    let (core, agent, chat) = family(model.clone(), ExecutionMode::FullAccess, &workdir).await;
    agent
        .submit(chat, "Scan in the background".into())
        .await
        .unwrap();
    settle(&agent, chat, |snapshot| says(snapshot, "Started.")).await;
    model.background_turns.add_permits(1);
    let snapshot = settle(&agent, chat, |snapshot| {
        says(snapshot, "Thanks.") && snapshot.family_completions.len() == 1
    })
    .await;
    let text = followed(chat, &workdir, &snapshot);
    assert_eq!(text, reopened(chat, &snapshot));
    assert_eq!(
        text,
        [
            "",
            "❯ Scan in the background",
            "",
            "⏺ Subagent(@scan · Scan quietly in the background)",
            "  ⎿  Backgrounded agent (ctrl+o to expand)",
            "",
            "⏺ Started.",
            "",
            &format!("⏺ Teammate @scan finished · {}", elapsed(&snapshot, "scan")),
            "  Background done.",
            "  All clear.…",
            "",
            "⏺ Thanks.",
            "",
            "⏺ Done",
            "",
        ]
        .join("\n")
    );
    let transcript = presentation::transcript_document(
        &snapshot,
        DisplayProfile::default(),
        &Default::default(),
        &Default::default(),
    )
    .plain_text(120);
    assert!(
        transcript.contains("  Background done.\n  All clear.\n  Nothing else.\n"),
        "Ctrl+O shows the whole report: {transcript}"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn a_finished_user_shell_shows_the_result_the_agent_records() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        tickloop_harness::test_support::config(),
        Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut shells = agent.subscribe_user_shells();
    for (command, state) in [
        ("printf 'one\\ntwo\\n'", UserShellState::Completed),
        ("printf 'boom\\n'; exit 3", UserShellState::Failed),
    ] {
        let receipt = agent
            .submit_user_shell(chat, command.into(), false)
            .await
            .unwrap();
        let record = loop {
            let record = tokio::time::timeout(Duration::from_secs(10), shells.recv())
                .await
                .unwrap()
                .unwrap();
            if record.id == receipt.command_id && user_shell_is_terminal(&record) {
                break record;
            }
        };
        assert_eq!(record.state, state);
        let output = agent
            .read_user_shell_output(chat, record.id, 0, USER_SHELL_RESULT_BYTES)
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        let snapshot = agent.snapshot(chat).await.unwrap();
        let recorded = snapshot
            .messages
            .iter()
            .rev()
            .find_map(|message| {
                message.content.iter().find_map(|content| match content {
                    Content::Text { text } => match LocalMessage::parse(text) {
                        Some(output @ LocalMessage::BashOutput { .. }) => Some(output),
                        _ => None,
                    },
                    _ => None,
                })
            })
            .unwrap();
        assert_eq!(
            presentation::user_shell_result(&record, output["output"].as_str().unwrap()),
            recorded,
            "{command}"
        );
    }
    agent.shutdown().await;
}

fn notification(status: TaskNotificationStatus, summary: &str) -> TaskNotification {
    TaskNotification {
        chat_id: Uuid::new_v4().into(),
        task_id: Uuid::new_v4().into(),
        tool_use_id: None,
        output_file: "/tmp/task.log".into(),
        status,
        summary: summary.into(),
    }
}

#[test]
fn notices_warnings_and_compaction_are_blocks_of_their_own() {
    let chat = Uuid::new_v4();
    let console = Console::test_interactive_for_chat(chat);
    console.write("Earlier output\n").unwrap();
    write_warning(&console, "Settings could not be read").unwrap();
    write_compaction_notice(&console, chat.into(), "summary").unwrap();
    console
        .write_roles(&task_notification_line(
            &notification(
                TaskNotificationStatus::Failed,
                "Background command \"make\" failed",
            ),
            DisplayProfile::default(),
        ))
        .unwrap();
    assert_eq!(
        console.test_conversation_text(),
        [
            "Earlier output",
            "",
            "! Settings could not be read",
            "",
            "⏺ Conversation compacted (ctrl+o for history)",
            "",
            "⏺ Background command \"make\" failed",
            "",
        ]
        .join("\n")
    );
    let completed = task_notification_line(
        &notification(TaskNotificationStatus::Completed, "done"),
        DisplayProfile::default(),
    );
    assert_eq!(
        completed[1],
        ("⏺".into(), Some(Role::Success)),
        "the task's dot takes its outcome's colour"
    );
}

#[test]
fn a_write_opening_with_a_newline_is_a_block_and_newlines_alone_end_a_line() {
    let console = Console::test_interactive_for_chat(Uuid::new_v4());
    console.write("partial").unwrap();
    console.write("\n").unwrap();
    console.write("next line\n").unwrap();
    console.write("\n\n\nA notice\n").unwrap();
    console
        .write_roles(&[("\n".into(), None), ("Another notice\n".into(), None)])
        .unwrap();
    assert_eq!(
        console.test_conversation_text(),
        "partial\nnext line\n\nA notice\n\nAnother notice\n"
    );
}

/// One assistant message with parallel calls: each row and the collapsed group settle as their
/// own results arrive while later calls still wait, and a call the ended run never started stops.
#[test]
fn parallel_calls_settle_as_their_results_arrive_and_unstarted_ones_stop_with_the_run() {
    let chat = Uuid::new_v4();
    let run = Uuid::new_v4();
    let loop_id = Uuid::new_v4();
    let console = Console::test_interactive_for_chat(chat);
    console
        .set_input_context(Some(chat), std::path::Path::new("/work"), &[])
        .unwrap();
    let mut tools = ToolPresentation::new(false);
    let mut sequence = 0;
    let mut present = |kind: EventKind| {
        sequence += 1;
        let event = Event {
            chat_id: chat.into(),
            loop_id: loop_id.into(),
            run_id: run.into(),
            sequence,
            kind,
        };
        tools
            .present_event(&console, &event, &presentation::CallFacts::default())
            .unwrap();
    };
    let calls = [
        ("todo", "Grep", serde_json::json!({"pattern": "TODO"})),
        ("done", "Grep", serde_json::json!({"pattern": "DONE"})),
        (
            "notes",
            "Read",
            serde_json::json!({"file_path": "/work/notes.txt"}),
        ),
        (
            "print",
            "Bash",
            serde_json::json!({"command": "printf 'one\\n'", "description": "Print one"}),
        ),
        (
            "list",
            "Bash",
            serde_json::json!({"command": "ls", "description": "List files"}),
        ),
    ];
    present(EventKind::AssistantMessageCommitted {
        message_id: Uuid::new_v4(),
        tool_calls: calls
            .iter()
            .map(|(id, name, input)| ProposedToolCall {
                call_id: (*id).into(),
                name: (*name).into(),
                input: input.clone(),
            })
            .collect(),
    });
    let mut run_call = |index: usize, output: &str| {
        let (id, name, input) = &calls[index];
        present(EventKind::ToolStarted {
            call_id: (*id).into(),
            name: (*name).into(),
            input: input.clone(),
        });
        present(EventKind::ToolCompleted {
            sources: Vec::new(),
            call_id: (*id).into(),
            result: ToolResult {
                content: output.into(),
                is_error: false,
                output_ref: None,
            },
        });
    };
    run_call(0, "notes.txt");
    run_call(1, "notes.txt");
    assert_eq!(
        console.test_conversation_text(),
        [
            "",
            "⏺ Searching for 2 patterns, reading 1 file… (ctrl+o to expand)",
            "",
            "⏺ Bash · Print one · Queued",
            "",
            "⏺ Bash · List files · Queued",
            "",
        ]
        .join("\n"),
        "a group whose read still waits keeps working"
    );
    run_call(2, "     1\tTODO: fix cache");
    run_call(3, "one");
    assert_eq!(
        console.test_conversation_text(),
        [
            "",
            "  Searched for 2 patterns, read 1 file (ctrl+o to expand)",
            "",
            "⏺ Bash · Print one",
            "  ⎿  one",
            "",
            "⏺ Bash · List files · Queued",
            "",
        ]
        .join("\n"),
        "finished calls settle while a later one still waits"
    );
    present(EventKind::Interrupted);
    let ended = console.test_conversation_text();
    assert!(ended.contains("⏺ Bash · List files · Stopped"), "{ended}");
}
