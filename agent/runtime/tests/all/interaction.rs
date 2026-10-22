use serde_json::json;
use std::sync::Arc;
use tickloop_agent_model::ModelRequest;
use tickloop_agent_protocol::{
    Content, EventKind, INTERRUPT_MESSAGE_FOR_TOOL_USE, LoopState, QuestionAnswer, Trigger,
    TriggerPromptSource, TriggerSource,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::*;
use tickloop_agent_tools::{ASK_USER_QUESTION_TOOL_NAME, DECLINED_QUESTIONS_RESULT};
use uuid::Uuid;

const UNIQUENESS_ERROR: &str =
    "Question texts must be unique, option labels must be unique within each question";

fn report_questions() -> serde_json::Value {
    json!({"questions":[
        {"question":"Which report should be shortened?","header":"Report","options":[
            {"label":"Weekly","description":"The Monday summary"},
            {"label":"Monthly","description":"The board pack","preview":"Revenue | Costs"}
        ]},
        {"question":"Which sections should stay?","header":"Sections","multiSelect":true,"options":[
            {"label":"Summary","description":"Opening paragraph"},
            {"label":"Risks","description":"Open risks"},
            {"label":"Charts","description":"Every chart"}
        ]}
    ]})
}

fn tool_result(requests: &[ModelRequest], call_id: &str) -> (String, bool) {
    requests
        .iter()
        .flat_map(|request| &request.messages)
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == call_id => Some((content.text().into_owned(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool result for {call_id}"))
}

#[tokio::test]
async fn questions_wait_for_an_answer_to_each_question_and_resume_the_same_goal() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("clarify", ASK_USER_QUESTION_TOOL_NAME, report_questions()),
        text("The monthly report will be shortened."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let other_chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Make that report shorter.".into())
        .await
        .unwrap();
    let question = loop {
        match next_event(&mut events).await.kind {
            EventKind::QuestionAsked { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert_eq!(
        (question.chat_id, question.loop_id, question.run_id),
        (chat, receipt.loop_id, receipt.run_id)
    );
    assert_eq!(question.call_id, "clarify");
    assert_eq!(question.questions.len(), 2);
    assert_eq!(
        question.questions[0].question,
        "Which report should be shortened?"
    );
    assert_eq!(question.questions[0].header, "Report");
    assert!(!question.questions[0].multi_select);
    assert!(question.questions[1].multi_select);
    assert_eq!(question.questions[0].options[0].label, "Weekly");
    assert_eq!(
        question.questions[0].options[0].description,
        "The Monday summary"
    );
    assert_eq!(question.questions[0].options[0].preview, None);
    assert_eq!(
        question.questions[0].options[1].preview.as_deref(),
        Some("Revenue | Costs")
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops[0].state, LoopState::WaitingQuestion);
    assert_eq!(snapshot.questions[0].id, question.id);
    assert_eq!(model.requests.lock().unwrap().len(), 1);

    let valid = || {
        vec![
            QuestionAnswer::option("Monthly"),
            QuestionAnswer::options(["Summary", "Charts"]).with_other("Keep the totals"),
        ]
    };
    assert!(
        agent
            .answer_question(other_chat, question.id, valid())
            .await
            .is_err()
    );
    for invalid in [
        vec![QuestionAnswer::option("Monthly")],
        vec![
            QuestionAnswer::option("Daily"),
            QuestionAnswer::option("Summary"),
        ],
        vec![
            QuestionAnswer::options(["Weekly", "Monthly"]),
            QuestionAnswer::option("Summary"),
        ],
        vec![
            QuestionAnswer::option("Weekly").with_other("Also daily"),
            QuestionAnswer::option("Summary"),
        ],
        vec![
            QuestionAnswer::other(" "),
            QuestionAnswer::option("Summary"),
        ],
        vec![QuestionAnswer::option("Weekly"), QuestionAnswer::default()],
        vec![
            QuestionAnswer::option("Weekly"),
            QuestionAnswer::options(["Risks", "Risks"]),
        ],
    ] {
        assert!(
            agent
                .answer_question(chat, question.id, invalid.clone())
                .await
                .is_err(),
            "{invalid:?}"
        );
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().loops[0].state,
        LoopState::WaitingQuestion
    );
    agent
        .answer_question(chat, question.id, valid())
        .await
        .unwrap();
    assert!(
        agent
            .answer_question(chat, question.id, valid())
            .await
            .is_err()
    );
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.questions.is_empty());
    assert_eq!(snapshot.loops.len(), 1);
    assert_eq!(snapshot.loops[0].id, receipt.loop_id);
    assert_eq!(snapshot.loops[0].goal, "Make that report shorter.");
    assert_eq!(snapshot.loops[0].state, LoopState::Completed);
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(
        tool_result(&requests, "clarify"),
        (
            "User has answered your questions: \"Which report should be shortened?\"=\"Monthly\" selected preview:\nRevenue | Costs, \"Which sections should stay?\"=\"Summary, Charts, Keep the totals\". You can now continue with the user's answers in mind.".into(),
            false
        )
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn question_input_is_validated_like_claude_code() {
    let directory = tempfile::tempdir().unwrap();
    let option = |label: &str| json!({"label":label,"description":"A choice"});
    let question = |text: &str, options: Vec<serde_json::Value>| json!({"question":text,"header":"Choice","options":options});
    let two_options = || vec![option("A"), option("B")];
    let invalid_calls = [
        (
            "duplicate-questions",
            json!({"questions":[question("Same?", two_options()), question("Same?", two_options())]}),
        ),
        (
            "duplicate-labels",
            json!({"questions":[question("Labels?", vec![option("A"), option("A")])]}),
        ),
        ("no-questions", json!({"questions":[]})),
        (
            "five-questions",
            json!({"questions":(1..=5).map(|index| question(&format!("Q{index}?"), two_options())).collect::<Vec<_>>()}),
        ),
        (
            "one-option",
            json!({"questions":[question("One?", vec![option("A")])]}),
        ),
        (
            "five-options",
            json!({"questions":[question("Five?", ["A", "B", "C", "D", "E"].map(option).to_vec())]}),
        ),
        (
            "text-multi-select",
            json!({"questions":[{"question":"Text?","header":"Choice","options":two_options(),"multiSelect":"true"}]}),
        ),
        (
            "unknown-field",
            json!({"questions":[question("Field?", two_options())],"choices":["A"]}),
        ),
        (
            "string-answers",
            json!({"questions":[question("Answers?", two_options())],"answers":{"Answers?":1}}),
        ),
    ];
    let first_reply = invalid_calls
        .iter()
        .flat_map(|(id, input)| call(id, ASK_USER_QUESTION_TOOL_NAME, input.clone()))
        .collect::<Vec<_>>();
    let model = Arc::new(ScriptedModel::new(vec![
        first_reply,
        call(
            "extras",
            ASK_USER_QUESTION_TOOL_NAME,
            json!({
                "questions":[question("Which one?", two_options())],
                "answers":{"Which one?":"A"},
                "annotations":{"Which one?":{"notes":"model supplied"}},
                "metadata":{"source":"remember"}
            }),
        ),
        text("Finished."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Ask me something.".into())
        .await
        .unwrap();
    let question = loop {
        match next_event(&mut events).await.kind {
            EventKind::QuestionAsked { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert_eq!(question.call_id, "extras");
    assert_eq!(question.questions[0].question, "Which one?");
    assert!(!question.questions[0].multi_select);
    agent
        .answer_question(chat, question.id, vec![QuestionAnswer::other("Neither")])
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let requests = model.requests.lock().unwrap().clone();
    let tools = &requests[0].tools;
    assert!(tools.iter().all(|tool| tool.name != "ask_user"));
    let schema = &tools
        .iter()
        .find(|tool| tool.name == ASK_USER_QUESTION_TOOL_NAME)
        .expect("AskUserQuestion is offered to the model")
        .input_schema;
    assert_eq!(schema["required"], json!(["questions"]));
    assert_eq!(schema["properties"]["questions"]["minItems"], 1);
    assert_eq!(schema["properties"]["questions"]["maxItems"], 4);
    let item = &schema["properties"]["questions"]["items"];
    assert_eq!(item["required"], json!(["question", "header", "options"]));
    assert_eq!(item["properties"]["options"]["minItems"], 2);
    assert_eq!(item["properties"]["options"]["maxItems"], 4);
    assert_eq!(item["properties"]["multiSelect"]["default"], false);
    assert_eq!(
        item["properties"]["options"]["items"]["required"],
        json!(["label", "description"])
    );
    assert!(item["properties"]["options"]["items"]["properties"]["preview"].is_object());
    for field in ["answers", "annotations", "metadata"] {
        assert!(schema["properties"][field].is_object(), "{field}");
    }
    for id in ["duplicate-questions", "duplicate-labels"] {
        assert_eq!(
            tool_result(&requests, id),
            (UNIQUENESS_ERROR.into(), true),
            "{id}"
        );
    }
    for (id, reason) in [
        ("no-questions", "input.questions has too few items"),
        ("five-questions", "input.questions has too many items"),
        ("one-option", "input.questions[0].options has too few items"),
        (
            "five-options",
            "input.questions[0].options has too many items",
        ),
        (
            "text-multi-select",
            "input.questions[0].multiSelect has an invalid type",
        ),
        ("unknown-field", "input.choices is not allowed"),
        (
            "string-answers",
            "input.answers.Answers? has an invalid type",
        ),
    ] {
        assert_eq!(
            tool_result(&requests, id),
            (format!("invalid AskUserQuestion input: {reason}"), true),
            "{id}"
        );
    }
    assert_eq!(
        tool_result(&requests, "extras"),
        (
            "User has answered your questions: \"Which one?\"=\"Neither\". You can now continue with the user's answers in mind.".into(),
            false
        )
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn an_independent_goal_is_host_created_and_runs_after_the_current_goal() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "new-goal",
            "StartGoal",
            json!({"goal":"Draft the release announcement."}),
        ),
        text("The weekly summary is complete; the announcement is queued separately."),
        text("Release announcement draft."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let original = agent
        .submit(
            chat,
            None,
            "Summarize the week. Also draft a separate release announcement.".into(),
        )
        .await
        .unwrap();
    let mut completions = Vec::new();
    while completions.len() < 2 {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } => completions.push(event.loop_id),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops.len(), 2);
    assert_eq!(snapshot.loops[0].id, original.loop_id);
    assert_eq!(
        snapshot.loops[0].goal,
        "Summarize the week. Also draft a separate release announcement."
    );
    let new_goal = snapshot
        .loops
        .iter()
        .find(|goal| goal.id != original.loop_id)
        .unwrap();
    assert_eq!(new_goal.goal, "Draft the release announcement.");
    assert_eq!(new_goal.state, LoopState::Completed);
    assert_eq!(completions, [original.loop_id, new_goal.id]);
    assert!(snapshot.pending_inputs.is_empty());
    let requests = model.requests.lock().unwrap().clone();
    assert!(requests[2].messages.iter().flat_map(|message| &message.content).any(|block| matches!(block,Content::Text {text} if text == "The weekly summary is complete; the announcement is queued separately.")));
    agent.shutdown().await;
}

#[tokio::test]
async fn triggers_cannot_bypass_clarification_and_cancelled_answers_cannot_resume_work() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "clarify",
            ASK_USER_QUESTION_TOOL_NAME,
            single_question("Which report?"),
        ),
        call(
            "clarify-again",
            ASK_USER_QUESTION_TOOL_NAME,
            single_question("Which report should I resume?"),
        ),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Shorten the report.".into())
        .await
        .unwrap();
    let question = loop {
        match next_event(&mut events).await.kind {
            EventKind::QuestionAsked { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    agent
        .configure_trigger(
            chat,
            receipt.loop_id,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "Check the report.".into(),
                prompt_source: TriggerPromptSource::Explicit,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    assert!(
        agent
            .trigger(
                chat,
                receipt.loop_id,
                TriggerSource {
                    source: "file_changed".into(),
                    payload: json!({})
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        agent.snapshot(chat).await.unwrap().loops[0].state,
        LoopState::WaitingQuestion
    );
    agent.interrupt(chat).await.unwrap();
    assert!(
        agent
            .answer_question(chat, question.id, vec![QuestionAnswer::option("Weekly")])
            .await
            .is_err()
    );
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Interrupted => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(agent.snapshot(chat).await.unwrap().questions.is_empty());
    let resumed = agent
        .submit(
            chat,
            Some(receipt.loop_id),
            "Continue, but clarify first.".into(),
        )
        .await
        .unwrap();
    let new_question = loop {
        match next_event(&mut events).await.kind {
            EventKind::QuestionAsked { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert_ne!(question.id, new_question.id);
    assert_eq!(new_question.run_id, resumed.run_id);
    assert!(
        agent
            .answer_question(chat, question.id, vec![QuestionAnswer::option("Monthly")])
            .await
            .is_err()
    );
    agent.stop(chat, receipt.loop_id).await.unwrap();
    assert!(
        agent
            .answer_question(
                chat,
                new_question.id,
                vec![QuestionAnswer::option("Weekly")]
            )
            .await
            .is_err()
    );
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Interrupted => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops[0].state, LoopState::Stopped);
    assert!(snapshot.questions.is_empty());
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    agent.shutdown().await;
}

#[tokio::test]
#[ignore = "subprocess fixture invoked by clarification recovery tests"]
async fn clarification_process_fixture() {
    let Some(directory) = std::env::var_os("TICKLOOP_INTERACTION_DIRECTORY") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    let agent = Agent::open_embedded(
        directory.join("archive"),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "clarify",
                ASK_USER_QUESTION_TOOL_NAME,
                single_question("Which report should I shorten?"),
            ),
            call(
                "after-answer",
                ASK_USER_QUESTION_TOOL_NAME,
                single_question("Which report should I open next?"),
            ),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Shorten that report.".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::QuestionAsked { request } => {
                if request.call_id == "clarify" {
                    std::fs::write(
                        directory.join("question.json"),
                        serde_json::to_vec(&request).unwrap(),
                    )
                    .unwrap();
                    if std::env::var("TICKLOOP_INTERACTION_ANSWERED").as_deref() == Ok("yes") {
                        agent
                            .answer_question(
                                chat,
                                request.id,
                                vec![QuestionAnswer::other("Weekly, preserve the totals.")],
                            )
                            .await
                            .unwrap();
                        continue;
                    }
                }
                agent.flush_transcripts().await;
                std::process::exit(0);
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_question_expires_across_processes_and_manual_resume_drops_the_unanswered_call() {
    for answered in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                tickloop_agent_test_support::test_path!(clarification_process_fixture).as_str(),
                "--nocapture",
            ])
            .env("TICKLOOP_INTERACTION_DIRECTORY", directory.path())
            .env(
                "TICKLOOP_INTERACTION_ANSWERED",
                if answered { "yes" } else { "no" },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let question: tickloop_agent_protocol::QuestionRequest =
            serde_json::from_slice(&std::fs::read(directory.path().join("question.json")).unwrap())
                .unwrap();
        let model = Arc::new(ScriptedModel::new(vec![text(
            "Continue from the recorded clarification.",
        )]));
        let agent = Agent::open_embedded(directory.path().join("archive"), config(), model.clone())
            .unwrap();
        let snapshot = agent.resume_chat(question.chat_id).await.unwrap();
        assert!(snapshot.questions.is_empty());
        assert_eq!(snapshot.loops[0].state, LoopState::Interrupted);
        assert!(model.requests.lock().unwrap().is_empty());
        assert!(
            agent
                .answer_question(
                    question.chat_id,
                    question.id,
                    vec![QuestionAnswer::option("Monthly")]
                )
                .await
                .is_err()
        );
        let conversation = snapshot.conversation_messages().unwrap();
        assert_eq!(
            conversation
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| { matches!(block, Content::ToolUse { id, .. } if id == "clarify") }),
            answered
        );
        let recorded_answer = conversation
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if tool_use_id == "clarify" => {
                    assert!(!is_error);
                    Some(content.text())
                }
                _ => None,
            });
        assert_eq!(recorded_answer.as_deref(), answered.then_some("User has answered your questions: \"Which report should I shorten?\"=\"Weekly, preserve the totals.\". You can now continue with the user's answers in mind."));
        assert!(!conversation.iter().flat_map(|message| &message.content).any(|block| {
            matches!(block, Content::ToolUse { id, .. } if id == "after-answer")
                || matches!(block, Content::ToolResult { tool_use_id, .. } if tool_use_id == "after-answer")
        }));
        assert!(snapshot.operations.is_empty());
        let mut events = agent.subscribe();
        agent
            .submit(
                question.chat_id,
                Some(question.loop_id),
                "Continue manually.".into(),
            )
            .await
            .unwrap();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn an_answer_accepted_before_interrupt_remains_in_the_conversation() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![call(
        "clarify",
        ASK_USER_QUESTION_TOOL_NAME,
        single_question("Which report?"),
    )]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Shorten that report.".into())
        .await
        .unwrap();
    let question = loop {
        match next_event(&mut events).await.kind {
            EventKind::QuestionAsked { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    agent
        .answer_question(
            chat,
            question.id,
            vec![QuestionAnswer::other("Weekly, preserve totals.")],
        )
        .await
        .unwrap();
    agent.interrupt(chat).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Interrupted => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.messages.iter().flat_map(|message| &message.content).any(|block| matches!(block,Content::ToolResult {tool_use_id,content,is_error:false} if tool_use_id == "clarify" && content.text() == "User has answered your questions: \"Which report?\"=\"Weekly, preserve totals.\". You can now continue with the user's answers in mind.")));
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn declining_a_question_rejects_the_call_ends_the_run_and_later_input_starts_a_new_run() {
    const FOLLOW_UP: &str = "Write the current protocol as an HTML page instead.";
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("clarify", ASK_USER_QUESTION_TOOL_NAME, report_questions()),
        text("Here is the protocol as an HTML page."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let other_chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let asked = agent
        .submit(chat, None, "Change the protocol.".into())
        .await
        .unwrap();
    let question = loop {
        match next_event(&mut events).await.kind {
            EventKind::QuestionAsked { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert!(
        agent
            .decline_question(other_chat, question.id)
            .await
            .is_err()
    );
    agent.decline_question(chat, question.id).await.unwrap();
    assert!(agent.decline_question(chat, question.id).await.is_err());
    assert!(
        agent
            .answer_question(
                chat,
                question.id,
                vec![
                    QuestionAnswer::option("Weekly"),
                    QuestionAnswer::option("Summary"),
                ],
            )
            .await
            .is_err(),
        "a declined question takes no answer"
    );
    let declined = loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "clarify" => {
                assert_eq!(event.run_id, asked.run_id);
                break result;
            }
            EventKind::Completed { .. } | EventKind::Failed(_) => panic!("{:?}", event.kind),
            _ => {}
        }
    };
    assert_eq!(
        (declined.content.as_str(), declined.is_error),
        (DECLINED_QUESTIONS_RESULT, true)
    );
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Interrupted if event.run_id == asked.run_id => break,
            EventKind::Completed { .. } | EventKind::Failed(_) => panic!("{:?}", event.kind),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.questions.is_empty());
    assert_eq!(snapshot.loops[0].state, LoopState::Interrupted);
    assert_eq!(
        model.requests.lock().unwrap().len(),
        1,
        "the declined run asks the model nothing more"
    );

    let follow_up = agent.submit(chat, None, FOLLOW_UP.into()).await.unwrap();
    assert_ne!(follow_up.run_id, asked.run_id);
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == follow_up.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        tool_result(&requests, "clarify"),
        (DECLINED_QUESTIONS_RESULT.to_owned(), true)
    );
    let texts = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(texts.contains(&INTERRUPT_MESSAGE_FOR_TOOL_USE), "{texts:?}");
    assert!(
        texts.last().is_some_and(|text| text.contains(FOLLOW_UP)),
        "{texts:?}"
    );
    agent.shutdown().await;
}
