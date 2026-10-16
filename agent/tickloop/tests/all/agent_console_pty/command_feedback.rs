use super::*;

const MODEL_FEEDBACK: &str = "Model selection: Applied · DeepSeek-V4.1-Flash · Thinking auto";
const FIRST_PROMPT: &str = "Keep the prior model selection visible";
const FIRST_REPLY: &str = "First message completed";

enum LocalCommand {
    Model,
    Provider,
    Rule,
}

impl LocalCommand {
    fn input(&self) -> &str {
        match self {
            Self::Model => "/model",
            Self::Provider => "/provider",
            Self::Rule => "/rule allow ReadFile session",
        }
    }

    fn result_prefix(&self) -> &str {
        match self {
            Self::Model | Self::Provider => MODEL_FEEDBACK,
            Self::Rule => "Rule added:",
        }
    }

    async fn apply(&self, view: &mut PtyView) -> String {
        view.send(format!("{}\r", self.input()).as_bytes());
        let model_title = match self {
            Self::Model => Some("Choose model · Signed-in Providers"),
            Self::Provider => {
                view.wait_for("real Provider picker opens", |frame| {
                    frame.contains("Choose Provider · All") && frame.contains("API key saved")
                })
                .await;
                view.send(b"\r");
                Some("Choose model · DeepSeek")
            }
            Self::Rule => None,
        };
        if let Some(title) = model_title {
            view.wait_for("real model picker opens", |frame| {
                frame.contains(title) && frame.contains("DeepSeek-V4.1-Flash")
            })
            .await;
            view.send(b"\r");
            // Applying a choice closes the picker and leaves the feedback in the conversation.
        }
        view.wait_for_editor("local command restores the composer", Some(""))
            .await;
        view.wait_for("local command produces its feedback", |frame| {
            body_shows_once_in_order(frame, &[self.input(), self.result_prefix()])
        })
        .await
    }
}

async fn start_feedback_view(fixture: &PrivateFixture, base_url: &str) -> PtyView {
    fixture.set_chat_model(base_url);
    tickloop_agent_model::credentials::save_api_key(
        &fixture.path("config").canonicalize().unwrap(),
        "DEEPSEEK_API_KEY",
        "fixture-deepseek-key",
    )
    .unwrap();
    let mut view = PtyView::start(fixture);
    view.wait_for("new model feedback View ready", new_input_is_visible)
        .await;
    view
}

async fn run_new_agent_command_feedback(fixture: &PrivateFixture, command: LocalCommand) {
    let (base_url, mut requests, release) =
        model_http_support::serve_responses_with_gated_response(
            vec![(200, chat_reply(None, FIRST_REPLY))],
            0,
        )
        .await;
    let mut view = start_feedback_view(fixture, &base_url).await;
    let client = fixture.client().await;
    assert!(catalog(&client).await.is_empty());

    let before = command.apply(&mut view).await;
    assert!(catalog(&client).await.is_empty());
    eprintln!("[command-feedback before first message]\n{before}\n[/command-feedback]");

    view.send(format!("{FIRST_PROMPT}\r").as_bytes());
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .expect("first message must reach the isolated provider")
        .unwrap();
    assert!(request["messages"].to_string().contains(FIRST_PROMPT));
    assert!(!request["messages"].to_string().contains(command.input()));
    let rows = catalog(&client).await;
    assert_eq!(rows.len(), 1);
    let after = view
        .wait_for("first message is consumed and echoed", |frame| {
            conversation_body(frame).contains(FIRST_PROMPT)
        })
        .await;
    eprintln!("[command-feedback while first message runs]\n{after}\n[/command-feedback]");
    assert!(
        body_shows_once_in_order(
            &after,
            &[command.input(), command.result_prefix(), FIRST_PROMPT]
        ),
        "the real first message must preserve the prior command and its result once in order: {after}"
    );
    release.send(()).unwrap();
    let completed = view
        .wait_for("first reply is displayed", |frame| {
            conversation_body(frame).contains(FIRST_REPLY)
        })
        .await;
    assert!(body_shows_once_in_order(
        &completed,
        &[
            command.input(),
            command.result_prefix(),
            FIRST_PROMPT,
            FIRST_REPLY
        ]
    ));
    assert_eq!(completed.matches(STARTUP_BANNER_TITLE).count(), 1);
    let completed_snapshot = wait_snapshot(
        &client,
        rows[0].execution,
        "first model run completes before navigation",
        |snapshot| {
            snapshot.conversation.loops[0].state == tickloop_agent_protocol::LoopState::Completed
        },
    )
    .await;

    view.send(b"/clear\r");
    let draft = view
        .wait_for("clear returns to a fresh draft", |frame| {
            new_input_is_visible(frame) && !frame.contains(FIRST_REPLY)
        })
        .await;
    assert!(body_shows_once_in_order(
        &draft,
        &[STARTUP_BANNER_TITLE, "deepseek-flash · ChatCompletions"]
    ));
    assert!(!conversation_body(&draft).contains(command.input()));
    assert!(!conversation_body(&draft).contains(command.result_prefix()));
    assert!(!conversation_body(&draft).contains(FIRST_PROMPT));

    const OTHER_OUTPUT: &str = "Other Agent output";
    view.send(format!("!printf '{OTHER_OUTPUT}\\n'\r").as_bytes());
    let other = view
        .wait_for("another Agent displays its own Shell result", |frame| {
            conversation_body(frame)
                .lines()
                .any(|line| screen::response_line(line) == OTHER_OUTPUT)
        })
        .await;
    assert!(!conversation_body(&other).contains(command.input()));
    assert!(!conversation_body(&other).contains(command.result_prefix()));
    assert!(!conversation_body(&other).contains(FIRST_PROMPT));
    assert_eq!(catalog(&client).await.len(), 2);

    view.send(b"\x1b[D");
    view.wait_for("overview contains the first Agent", |frame| {
        frame.contains("Agents · local") && frame.contains(FIRST_PROMPT)
    })
    .await;
    select_overview_row(&mut view, FIRST_PROMPT).await;
    view.send(b"\r");
    let returned = view
        .wait_for(
            "return restores the first Agent's local command feedback",
            |frame| !frame.contains("Agents · local") && frame.contains(FIRST_REPLY),
        )
        .await;
    assert!(body_shows_once_in_order(
        &returned,
        &[
            command.input(),
            command.result_prefix(),
            FIRST_PROMPT,
            FIRST_REPLY
        ]
    ));
    assert_eq!(returned.matches(STARTUP_BANNER_TITLE).count(), 1);
    assert!(!conversation_body(&returned).contains(OTHER_OUTPUT));
    eprintln!("[command-feedback after cached Agent return]\n{returned}\n[/command-feedback]");
    assert!(
        requests.try_recv().is_err(),
        "navigation starts no model work"
    );
    let after_navigation = snapshot(&client, rows[0].execution).await;
    assert_eq!(
        after_navigation.last_model_run,
        completed_snapshot.last_model_run
    );
    assert_eq!(
        after_navigation.last_sequence,
        completed_snapshot.last_sequence
    );
    view.close().await;
}

async fn run_background_event_reset(fixture: &PrivateFixture) {
    const THINKING_EVENTS: usize = 1100;
    const SECOND_PROMPT: &str = "Produce enough background Thinking to require history recovery";
    const SECOND_REPLY: &str = "Background thinking completed";
    const OTHER_OUTPUT: &str = "Other reset Agent output";
    let thinking_frame = format!(
        "data: {}\n\n",
        json!({"id":"cli-fixture","choices":[{"index":0,"delta":{"reasoning_content":"thinking "},"finish_reason":null}],"usage":{"prompt_tokens":0,"completion_tokens":0}})
    );
    let background_reply = thinking_frame.repeat(THINKING_EVENTS) + &chat_reply(None, SECOND_REPLY);
    let (base_url, mut requests, release) =
        model_http_support::serve_responses_with_gated_response(
            vec![
                (200, chat_reply(None, FIRST_REPLY)),
                (200, background_reply),
            ],
            1,
        )
        .await;
    let mut view = start_feedback_view(fixture, &base_url).await;
    let client = fixture.client().await;
    LocalCommand::Model.apply(&mut view).await;
    view.send(format!("{FIRST_PROMPT}\r").as_bytes());
    tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    view.wait_for("first reply before background reset", |frame| {
        body_shows_once_in_order(
            frame,
            &["/model", MODEL_FEEDBACK, FIRST_PROMPT, FIRST_REPLY],
        )
    })
    .await;
    let first = catalog(&client).await[0].execution;
    let before = snapshot(&client, first).await;
    view.send(format!("{SECOND_PROMPT}\r").as_bytes());
    tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    view.wait_for("follow-up is echoed before leaving its Agent", |frame| {
        conversation_body(frame).contains(SECOND_PROMPT)
    })
    .await;
    view.send(b"/clear\r");
    view.wait_for("background reset fresh draft", |frame| {
        new_input_is_visible(frame) && !frame.contains(FIRST_REPLY)
    })
    .await;
    view.send(format!("!printf '{OTHER_OUTPUT}\\n'\r").as_bytes());
    view.wait_for(
        "another Agent remains visible while background Thinking runs",
        |frame| {
            conversation_body(frame)
                .lines()
                .any(|line| screen::response_line(line) == OTHER_OUTPUT)
        },
    )
    .await;
    release.send(()).unwrap();
    tokio::time::timeout(UI_TIMEOUT, async {
        loop {
            let observed = client
                .observe(ObservationRequest::Agent {
                    execution: first,
                    detail: AgentDetailQuery::Status,
                    after: None,
                })
                .await
                .unwrap();
            let ObservationValue::Status(status) = observed.initial else {
                panic!("expected background Agent status");
            };
            if status.chat.loops.iter().any(|goal| {
                goal.id == before.conversation.loops[0].id
                    && goal.state == tickloop_agent_protocol::LoopState::Completed
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the background model run completes");
    let completed = snapshot(&client, first).await;
    assert!(completed.conversation.messages.iter().any(|message| {
        message.content.iter().any(|content| {
            matches!(content,
                tickloop_agent_protocol::Content::Text { text } if text == SECOND_REPLY)
        })
    }));
    assert!(
        completed.events[0].sequence > before.last_sequence + 1,
        "the actual worker evicts the earlier event range, requiring history recovery"
    );
    view.send(b"\x1b[D");
    view.wait_for("background reset overview", |frame| {
        frame.contains("Agents · local") && frame.contains(FIRST_PROMPT)
    })
    .await;
    select_overview_row(&mut view, FIRST_PROMPT).await;
    view.send(b"\r");
    let returned = view
        .wait_for(
            "background reply is restored after the event gap",
            |frame| {
                !frame.contains("Agents · local") && conversation_body(frame).contains(SECOND_REPLY)
            },
        )
        .await;
    eprintln!(
        "[command-feedback after real background event reset]\n{returned}\n[/command-feedback]"
    );
    assert_eq!(returned.matches(STARTUP_BANNER_TITLE).count(), 1);
    assert!(
        body_shows_once_in_order(
            &returned,
            &[
                "/model",
                MODEL_FEEDBACK,
                FIRST_PROMPT,
                FIRST_REPLY,
                SECOND_PROMPT,
                SECOND_REPLY
            ]
        ),
        "a real cached Agent return must retain local command feedback after a background event gap: {returned}"
    );
    let after_navigation = snapshot(&client, first).await;
    assert_eq!(after_navigation.last_model_run, completed.last_model_run);
    assert_eq!(after_navigation.last_sequence, completed.last_sequence);
    view.close().await;
}

#[tokio::test]
async fn actual_cli_background_event_reset_keeps_model_command_and_feedback() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_background_event_reset(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_first_message_keeps_model_command_and_feedback() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_new_agent_command_feedback(
        &fixture,
        LocalCommand::Model,
    ))
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_first_message_keeps_provider_command_and_feedback() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_new_agent_command_feedback(
        &fixture,
        LocalCommand::Provider,
    ))
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_first_message_keeps_rule_command_and_feedback() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_new_agent_command_feedback(&fixture, LocalCommand::Rule))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
