use super::*;

const REPLY: &str = "REPLY-COMMITTED-WITHOUT-NEWLINE";
const BATCH_PROMPT: &str = "Visibility batch probe";
const QUESTION: &str = "QUEUED-QUESTION-PREVIEW";

fn tool_header_count(frame: &str, name: &str) -> usize {
    conversation_body(frame)
        .lines()
        .filter(|line| {
            line.trim_start().starts_with(['●', '⏺']) && line.contains(&format!(" {name} ·"))
        })
        .count()
}

fn batch_reply() -> String {
    let calls = [
        ("queued-bash", "Bash", json!({"command":"printf admitted > admission-marker; while [ ! -f finish-command ]; do sleep 0.02; done", "description":"pending batch command"})),
        ("queued-question", "AskUserQuestion", json!({"questions":[{"question":QUESTION,"header":"Choose","options":[{"label":"One","description":"First"},{"label":"Two","description":"Second"}]}]})),
    ].into_iter().enumerate().map(|(index,(id,name,input))| json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}})).collect::<Vec<_>>();
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"batch-fixture","choices":[{"index":0,"delta":{"role":"assistant","content":REPLY,"tool_calls":calls},"finish_reason":null}]}),
        json!({"id":"batch-fixture","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})
    )
}

fn review_allow_reply() -> String {
    let rules = ["external_code", "destruction", "persistence", "security", "exfiltration", "credentials", "external_scope", "finance"]
        .into_iter().map(|name| json!({"id": format!("builtin.soft_deny.{name}"), "applicability": "absent", "clearance": "missing"})).collect::<Vec<_>>();
    chat_reply(
        None,
        &json!({
            "task_scope": "within_task",
            "rules": rules,
        })
        .to_string(),
    )
}

async fn reply_before_approval(fixture: &PrivateFixture, text: &str) {
    let (url, mut requests, release) = model_http_support::serve_responses_with_gated_response(
        vec![
            (200, chat_reply(Some(("Bash", "pending-call", json!({"command": "printf admitted > admission-marker", "description": "visibility probe"}))), text)),
            (200, review_allow_reply()),
            (200, chat_reply(None, "PROBE-FINISHED")),
        ], 1,
    ).await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start_with_flags(fixture, &["--mode", "auto"]);
    view.wait_for("fresh reply visibility View", new_input_is_visible)
        .await;
    view.send(b"Show the reply before its command is approved\r");
    let _conversation_request = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let (_, review_request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        review_request.get("response_format").is_some(),
        "{review_request}"
    );
    assert!(!fixture.path("work/admission-marker").exists());
    let before = view
        .wait_for(
            "committed reply visible during gated Auto review",
            |frame| conversation_body(frame).contains(REPLY),
        )
        .await;
    assert_eq!(conversation_body(&before).matches(REPLY).count(), 1);
    assert!(!fixture.path("work/admission-marker").exists());
    eprintln!("[reply-before-admission]\n{before}\n[/reply-before-admission]");
    view.send(b"\x0f");
    view.wait_for("archive has the same committed reply", |frame| {
        frame.contains("Transcript") && frame.contains(REPLY)
    })
    .await;
    view.send(b"\x0f");
    view.wait_for("main reply survives archive round trip", |frame| {
        !frame.contains("Transcript") && conversation_body(frame).matches(REPLY).count() == 1
    })
    .await;
    release.send(()).unwrap();
    view.wait_for("run completes after actual approval", |frame| {
        frame.contains("PROBE-FINISHED")
    })
    .await;
    assert!(fixture.path("work/admission-marker").exists());
    view.close().await;
}

#[tokio::test]
async fn actual_cli_committed_reply_is_visible_before_auto_approval() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(reply_before_approval(&fixture, REPLY))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn queued_batch(fixture: &PrivateFixture) {
    let (url, mut requests, release) = model_http_support::serve_responses_with_gated_response(
        vec![
            (200, batch_reply()),
            (200, review_allow_reply()),
            (200, chat_reply(None, "BATCH-FINISHED")),
        ],
        1,
    )
    .await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start_with_flags(fixture, &["--mode", "auto"]);
    view.wait_for("batch View ready", new_input_is_visible)
        .await;
    view.send(format!("{BATCH_PROMPT}\r").as_bytes());
    let _ = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let (_, review) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(review.get("response_format").is_some());
    let pending = view
        .wait_for("both proposed calls visible before approval", |frame| {
            let body = conversation_body(frame);
            body.contains(REPLY)
                && body.contains("Bash")
                && body.contains(QUESTION)
                && body.matches("Queued").count() == 2
        })
        .await;
    assert!(!pending.contains("Esc answer later"));
    assert!(!fixture.path("work/admission-marker").exists());
    let mut attached = PtyView::start(fixture);
    attached
        .wait_for("second View ready", new_input_is_visible)
        .await;
    attached.send(b"\x1b[D");
    attached
        .wait_for("overview lists pending batch", |frame| {
            frame.contains("Agents · local") && frame.contains(BATCH_PROMPT)
        })
        .await;
    select_overview_row(&mut attached, BATCH_PROMPT).await;
    attached.send(b"\r");
    attached
        .wait_for("attached View projects the same queued calls", |frame| {
            !frame.contains("Agents · local")
                && conversation_body(frame).contains(REPLY)
                && frame.contains(QUESTION)
                && frame.matches("Queued").count() == 2
        })
        .await;
    view.send(b"\x0f");
    view.wait_for("batch Transcript has reply and question", |frame| {
        frame.contains("Transcript") && frame.contains(REPLY) && frame.contains(QUESTION)
    })
    .await;
    view.send(b"\x0f");
    view.wait_for("batch main survives Transcript", |frame| {
        !frame.contains("Transcript") && frame.matches("Queued").count() == 2
    })
    .await;
    release.send(()).unwrap();
    let running = view
        .wait_for("admitted Bash replaces its queued row", |frame| {
            frame.contains("Bash")
                && frame.contains(QUESTION)
                && conversation_body(frame).matches("Queued").count() == 1
        })
        .await;
    assert_eq!(conversation_body(&running).matches("Bash").count(), 1);
    assert!(!running.contains("Esc answer later"));
    attached
        .wait_for("attached queued row also updates once", |frame| {
            conversation_body(frame).matches("Queued").count() == 1
                && conversation_body(frame).matches("Bash").count() == 1
        })
        .await;
    std::fs::write(fixture.path("work/finish-command"), "").unwrap();
    view.wait_for("answer UI activates only after Bash finishes", |frame| {
        frame.contains(QUESTION) && frame.contains("Esc answer later")
    })
    .await;
    view.send(b"\r");
    let (_, followup) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let answers = followup["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool" && message["tool_call_id"] == "queued-question")
        .collect::<Vec<_>>();
    assert_eq!(answers.len(), 1);
    assert!(answers[0]["content"].as_str().unwrap().contains("One"));
    view.resize(120, 60).await;
    let finished = view
        .wait_for("batch completes", |frame| frame.contains("BATCH-FINISHED"))
        .await;
    eprintln!("[batch-finished]\n{finished}\n[/batch-finished]");
    assert_eq!(tool_header_count(&finished, "Bash"), 1, "{finished}");
    assert_eq!(
        tool_header_count(&finished, "AskUserQuestion"),
        1,
        "{finished}"
    );
    assert!(!conversation_body(&finished).contains("Queued"));
    attached.close().await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_proposed_tool_and_question_rows_wait_for_admission() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(queued_batch(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn streamed_reply(deltas: Vec<Value>, reason: &str) -> String {
    let mut reply = String::new();
    for delta in deltas {
        reply.push_str(&format!("data: {}\n\n",json!({"id":"streamed-fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]})));
    }
    reply.push_str(&format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id":"streamed-fixture","choices":[{"index":0,"delta":{},"finish_reason":reason}]})
    ));
    reply
}

async fn multiline_after_interruption(fixture: &PrivateFixture) {
    let prior_calls=[10,60,120].into_iter().enumerate().map(|(index,seconds)|json!({"index":index,"id":if index==0 {"reused-id".into()} else {format!("prior-{index}")},"type":"function","function":{"name":"Bash","arguments":json!({"command":format!("sleep {seconds}; echo \"cmd{index} done at $(date +%T)\""),"description":format!("prior background {index}"),"run_in_background":true}).to_string()}})).collect::<Vec<_>>();
    let first = streamed_reply(
        vec![json!({"role":"assistant","content":"PRIOR-REPLY","tool_calls":prior_calls})],
        "tool_calls",
    );
    let multiline = streamed_reply(
        vec![
            json!({"role":"assistant","reasoning_content":"Consider all lines before tool admission."}),
            json!({"content":"VISIBLE-LINE-1\nVISIBLE-LI"}),
            json!({"content":"NE-2\nVISIBLE-LINE-3\nVISIBLE-LINE-4\nVISIBLE-LINE-5\nVISIBLE-LINE-6\nVISIBLE-TAIL"}),
            json!({"tool_calls":[{"index":0,"id":"reused-id","type":"function","function":{"name":"Bash","arguments":json!({"command":"printf admitted > admission-marker","description":"new run with reused call ID"}).to_string()}}]}),
        ],
        "tool_calls",
    );
    let (url, mut requests, mut releases) =
        model_http_support::serve_responses_with_gated_responses(
            vec![
                (200, first),
                (200, review_allow_reply()),
                (200, multiline),
                (200, review_allow_reply()),
                (200, chat_reply(None, "MULTILINE-FINISHED")),
            ],
            &[1, 3],
        )
        .await;
    let second_release = releases.pop().unwrap();
    let prior_release = releases.pop().unwrap();
    fixture.set_chat_model(&url);
    let mut view = PtyView::start_with_flags(fixture, &["--mode", "auto"]);
    view.wait_for("interrupt fixture ready", new_input_is_visible)
        .await;
    view.resize(120, 70).await;
    view.send(b"Run the prior background batch\r");
    let _ = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let (_, first_review) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        first_review.get("response_format").is_some(),
        "{first_review}"
    );
    view.wait_for("prior tool proposals reach their gated review", |frame| {
        frame.contains("PRIOR-REPLY") && frame.matches("Queued").count() == 3
    })
    .await;
    let client = fixture.client().await;
    let execution = catalog(&client).await[0].execution;
    client
        .execute(CommandEnvelope {
            owner_epoch: client.owner_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::ManageLoop {
                execution,
                action: LoopAction::Interrupt,
            },
        })
        .await
        .unwrap();
    view.wait_for("prior batch is interrupted before admission", |frame| {
        frame.contains("Interrupted")
    })
    .await;
    view.send(b"Check whether the Agent is stuck\r");
    for _ in 0..2 {
        tokio::time::timeout(UI_TIMEOUT, requests.recv())
            .await
            .unwrap()
            .unwrap();
    }
    drop(prior_release);
    let markers = [
        "VISIBLE-LINE-1",
        "VISIBLE-LINE-2",
        "VISIBLE-LINE-3",
        "VISIBLE-LINE-4",
        "VISIBLE-LINE-5",
        "VISIBLE-LINE-6",
        "VISIBLE-TAIL",
    ];
    let before = view
        .wait_for(
            "all split lines including tail drain before approval",
            |frame| body_shows_once_in_order(frame, &markers),
        )
        .await;
    assert!(!fixture.path("work/admission-marker").exists());
    assert_eq!(tool_header_count(&before, "Bash"), 4, "{before}");
    eprintln!("[multiline-before-admission]\n{before}\n[/multiline-before-admission]");
    second_release.send(()).unwrap();
    view.wait_for("new run completes", |frame| {
        frame.contains("MULTILINE-FINISHED")
    })
    .await;
    assert!(fixture.path("work/admission-marker").exists());
    view.close().await;
}

#[tokio::test]
async fn actual_cli_split_multiline_reply_survives_an_interrupted_background_batch() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(multiline_after_interruption(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_newline_reply_remains_visible_before_auto_approval() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(reply_before_approval(&fixture, &format!("{REPLY}\n")))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn text_only_reply(fixture: &PrivateFixture) {
    let (url, _) = model_http_support::serve_responses(vec![(
        200,
        streamed_reply(
            vec![
                json!({"role":"assistant","reasoning_content":"Consider a text-only response."}),
                json!({"content":REPLY}),
            ],
            "stop",
        ),
    )])
    .await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start(fixture);
    view.wait_for("text-only View ready", new_input_is_visible)
        .await;
    view.send(b"Return a text-only response\r");
    let complete = view
        .wait_for("text-only tail appears once at completion", |frame| {
            frame.contains(REPLY) && frame.contains("Done")
        })
        .await;
    assert_eq!(conversation_body(&complete).matches(REPLY).count(), 1);
    view.close().await;
}

#[tokio::test]
async fn actual_cli_text_only_reply_keeps_its_tail_after_thinking() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(text_only_reply(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn independent_question(fixture: &PrivateFixture) {
    let question = json!({"questions":[{"question":QUESTION,"header":"Choose","options":[{"label":"One","description":"First"},{"label":"Two","description":"Second"}]}]});
    let (url, mut requests) = model_http_support::serve_responses(vec![
        (
            200,
            chat_reply(
                Some(("AskUserQuestion", "single-question", question)),
                REPLY,
            ),
        ),
        (200, chat_reply(None, "INDEPENDENT-QUESTION-FINISHED")),
    ])
    .await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start(fixture);
    view.wait_for("independent question View ready", new_input_is_visible)
        .await;
    view.send(b"Ask an independent question\r");
    view.wait_for("independent answer menu and reply are visible", |frame| {
        frame.contains(REPLY) && frame.contains(QUESTION) && frame.contains("Esc answer later")
    })
    .await;
    view.send(b"\r");
    let _ = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let (_, followup) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let answers = followup["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool" && message["tool_call_id"] == "single-question")
        .collect::<Vec<_>>();
    assert_eq!(answers.len(), 1);
    assert!(answers[0]["content"].as_str().unwrap().contains("One"));
    view.wait_for("independent question completes", |frame| {
        frame.contains("INDEPENDENT-QUESTION-FINISHED")
    })
    .await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_independent_question_still_activates_and_accepts_one_answer() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(independent_question(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
