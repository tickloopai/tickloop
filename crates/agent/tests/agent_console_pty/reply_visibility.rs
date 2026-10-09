use super::*;

const REPLY: &str = "REPLY-COMMITTED-WITHOUT-NEWLINE";
const BATCH_PROMPT: &str = "Visibility batch probe";
const QUESTION: &str = "QUEUED-QUESTION-PREVIEW";
const PARKED_CALL: &str = "PARKED-CALL-AWAITING-PERMISSION";

async fn short_exchange_has_no_sticky_prompt(fixture: &PrivateFixture, renderer: &str) {
    const PROMPT: &str = "SHORT-EXCHANGE-USER-MESSAGE";
    const REPLY: &str = "SHORT-EXCHANGE-ASSISTANT-REPLY";
    let (url, mut requests) =
        model_http_support::serve_responses(vec![(200, chat_reply(None, REPLY))]).await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start_with_environment(
        fixture,
        &[],
        &[("TICKLOOP_AGENT_TUI", std::path::Path::new(renderer))],
    );
    view.wait_for("short exchange View ready", new_input_is_visible)
        .await;
    view.send(format!("{PROMPT}\r").as_bytes());
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(request["messages"].to_string().contains(PROMPT));
    let frame = view
        .wait_for("short exchange reply", |frame| frame.contains(REPLY))
        .await;
    eprintln!("[short-exchange-frame {renderer}]\n{frame}\n[/short-exchange-frame]");
    assert!(!frame.lines().next().unwrap().contains(PROMPT), "{frame}");
    assert_eq!(frame.matches(PROMPT).count(), 1, "{frame}");
    assert!(
        frame.find(PROMPT).unwrap() < frame.find(REPLY).unwrap(),
        "{frame}"
    );
    view.close().await;
}

#[tokio::test]
async fn actual_cli_short_exchange_keeps_the_user_message_in_the_flow_only() {
    for renderer in ["fullscreen", "inline"] {
        let fixture = PrivateFixture::new();
        let result = AssertUnwindSafe(short_exchange_has_no_sticky_prompt(&fixture, renderer))
            .catch_unwind()
            .await;
        fixture.shutdown().await;
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

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
            daemon_epoch: client.daemon_epoch(),
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

async fn parked_loop_survives_other_completion(fixture: &PrivateFixture) {
    let marker = fixture.path("parked-marker");
    let (url,_)=model_http_support::serve_responses(vec![
        (200,chat_reply(Some(("Bash","parked-call",json!({"command":format!("printf parked > {}",marker.display()),"description":PARKED_CALL}))),"PARKED-REPLY")),
        (200,chat_reply(None,"INDEPENDENT-LOOP-FINISHED")),
        (200,chat_reply(None,"PARKED-LOOP-FINISHED")),
    ]).await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start_with_flags(fixture, &["--mode", "ask"]);
    view.wait_for("parked Loop View ready", new_input_is_visible)
        .await;
    view.resize(120, 50).await;
    view.send(b"Park this Loop for permission\r");
    view.wait_for("first call awaits human approval", |frame| {
        frame.contains(PARKED_CALL) && frame.contains("Queued")
    })
    .await;
    let client = fixture.client().await;
    let execution = catalog(&client).await[0].execution;
    let parked = wait_snapshot(
        &client,
        execution,
        "one actionable permission",
        |snapshot| snapshot.permissions.len() == 1,
    )
    .await;
    let first_run = parked.permissions[0]
        .execution
        .run_id()
        .expect("a model permission has its Run");
    client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution,
                target_loop: None,
                input: UserInput::Text {
                    text: "Complete an independent Loop".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        })
        .await
        .unwrap();
    view.wait_for("independent Loop completes", |frame| {
        frame.contains("INDEPENDENT-LOOP-FINISHED")
    })
    .await;
    let preserved = view.frame();
    let row = conversation_body(&preserved)
        .lines()
        .find(|line| line.contains(PARKED_CALL))
        .unwrap()
        .to_owned();
    assert!(
        row.contains("Queued") && !row.contains("Stopped"),
        "{preserved}"
    );
    let still_parked = snapshot(&client, execution).await;
    assert_eq!(still_parked.permissions.len(), 1);
    assert_eq!(
        still_parked.permissions[0]
            .execution
            .run_id()
            .expect("a model permission has its Run"),
        first_run
    );
    assert!(!marker.exists());
    let request = *still_parked
        .request_refs
        .iter()
        .find(|request| request.kind == RequestKind::Permission)
        .unwrap();
    client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::DecidePermission {
                request,
                allow: true,
                scope: tickloop_agent::permissions::PermissionScope::Once,
                comment: None,
                rule: None,
            },
        })
        .await
        .unwrap();
    view.wait_for("parked Loop resumes after its own approval", |frame| {
        frame.contains("PARKED-LOOP-FINISHED")
    })
    .await;
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "parked");
    view.close().await;
}

#[tokio::test]
async fn actual_cli_finishing_one_run_preserves_another_loops_actionable_proposal() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(parked_loop_survives_other_completion(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn interrupted_batch_has_the_same_state_on_attach(fixture: &PrivateFixture) {
    let (url, mut requests, _release) = model_http_support::serve_responses_with_gated_response(
        vec![(200, batch_reply()), (200, review_allow_reply())],
        1,
    )
    .await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start(fixture);
    view.wait_for("interrupted attach View ready", new_input_is_visible)
        .await;
    view.resize(120, 50).await;
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
    view.wait_for("both calls await admission", |frame| {
        conversation_body(frame).matches("Queued").count() == 2
    })
    .await;
    let client = fixture.client().await;
    let execution = catalog(&client).await[0].execution;
    client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::ManageLoop {
                execution,
                action: LoopAction::Interrupt,
            },
        })
        .await
        .unwrap();
    let original = view
        .wait_for("original View marks unadmitted calls Stopped", |frame| {
            conversation_body(frame).matches("Stopped").count() == 2
        })
        .await;
    assert!(!fixture.path("work/admission-marker").exists());
    let mut attached = PtyView::start(fixture);
    attached
        .wait_for("new View ready", new_input_is_visible)
        .await;
    attached.resize(120, 50).await;
    attached.send(b"\x1b[D");
    attached
        .wait_for("Agents lists interrupted batch", |frame| {
            frame.contains("Agents · local") && frame.contains(BATCH_PROMPT)
        })
        .await;
    select_overview_row(&mut attached, BATCH_PROMPT).await;
    attached.send(b"\r");
    let restored = attached
        .wait_for("attached interrupted proposals remain Stopped", |frame| {
            !frame.contains("Agents · local")
                && conversation_body(frame).matches("Stopped").count() == 2
        })
        .await;
    assert!(original.contains(REPLY) && restored.contains(REPLY));
    assert_eq!(tool_header_count(&restored, "Bash"), 1);
    assert_eq!(tool_header_count(&restored, "AskUserQuestion"), 1);
    eprintln!("[interrupted live]\n{original}\n[interrupted attached]\n{restored}");
    attached.close().await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_interrupted_unadmitted_calls_keep_their_state_in_an_attached_view() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(interrupted_batch_has_the_same_state_on_attach(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn interrupted_state_survives_event_eviction(fixture: &PrivateFixture, restart_daemon: bool) {
    const CALL: &str = "RETENTION-CALL";
    const QUEUED_QUESTION: &str = "RETENTION-QUEUED-QUESTION";
    const FLOOD_EVENTS: usize = 1100;
    let marker = fixture.path("retention-marker");
    let mut noisy_reply = (0..FLOOD_EVENTS).map(|_| format!("data: {}\n\n", json!({"id":"streamed-fixture","choices":[{"index":0,"delta":{"role":"assistant","content":"."},"finish_reason":null}],"usage":{"prompt_tokens":0,"completion_tokens":0}}))).collect::<String>();
    noisy_reply.push_str(&streamed_reply(
        vec![json!({"content":"RETENTION-FLOOD-DONE"})],
        "stop",
    ));
    let proposal = streamed_reply(
        vec![
            json!({"role":"assistant","content":"RETENTION-REPLY","tool_calls":[
                {"index":0,"id":"retention-call","type":"function","function":{"name":"Bash","arguments":json!({"command":format!("printf forbidden > {}",marker.display()),"description":CALL}).to_string()}},
                {"index":1,"id":"retention-question","type":"function","function":{"name":"AskUserQuestion","arguments":json!({"questions":[{"question":QUEUED_QUESTION,"header":"Choose","options":[{"label":"One","description":"First"},{"label":"Two","description":"Second"}]}]}).to_string()}}
            ]}),
        ],
        "tool_calls",
    );
    let (url, mut requests, _release) = model_http_support::serve_responses_with_gated_response(
        vec![
            (200, proposal),
            (200, noisy_reply),
            (200, chat_reply(None, "UNEXPECTED-RESUME-REQUEST")),
        ],
        2,
    )
    .await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::launch(fixture, &["--mode", "ask"], &[], restart_daemon);
    view.wait_for("retention View ready", new_input_is_visible)
        .await;
    view.resize(120, 60).await;
    view.send(b"Retention probe\r");
    view.wait_for("retention call waits for permission", |frame| {
        frame.contains(CALL) && frame.contains("Queued")
    })
    .await;
    let client = fixture.client().await;
    let execution = catalog(&client).await[0].execution;
    let before = wait_snapshot(
        &client,
        execution,
        "retention permission exists",
        |snapshot| snapshot.permissions.len() == 1,
    )
    .await;
    let interrupted_run = before.permissions[0]
        .execution
        .run_id()
        .expect("a model permission has its Run");
    client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::ManageLoop {
                execution,
                action: LoopAction::Interrupt,
            },
        })
        .await
        .unwrap();
    view.wait_for("live retention header stops", |frame| {
        conversation_body(frame)
            .lines()
            .any(|line| line.contains(CALL) && line.contains("Stopped"))
    })
    .await;
    client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution,
                target_loop: None,
                input: UserInput::Text {
                    text: "Roll event log".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        })
        .await
        .unwrap();
    let evicted = wait_snapshot(
        &client,
        execution,
        "old Run is evicted after the other Loop completes",
        |snapshot| {
            snapshot.conversation.loops.iter().any(|goal| {
                goal.run_id != Some(interrupted_run)
                    && goal.state == tickloop_agent::runtime::LoopState::Completed
            }) && !snapshot
                .events
                .iter()
                .any(|event| event.run_id == interrupted_run)
        },
    )
    .await;
    assert_eq!(evicted.events.len(), 1024);
    assert!(
        evicted
            .tool_call_recoveries
            .iter()
            .any(|fact| fact.run_id == Some(interrupted_run)
                && fact.call_id == "retention-call"
                && fact.was_admitted == Some(false))
    );
    assert!(
        evicted
            .tool_call_recoveries
            .iter()
            .any(|fact| fact.run_id == Some(interrupted_run)
                && fact.call_id == "retention-question"
                && fact.was_admitted.is_none())
    );
    let original = view
        .wait_for("original header remains Stopped after eviction", |frame| {
            frame.contains("RETENTION-FLOOD-DONE")
                && conversation_body(frame)
                    .lines()
                    .any(|line| line.contains(CALL) && line.contains("Stopped"))
        })
        .await;
    assert!(!marker.exists());
    let title = catalog(&client).await[0].title.clone();
    let mut attached = PtyView::start(fixture);
    attached
        .wait_for("retention attached View ready", new_input_is_visible)
        .await;
    attached.resize(120, 60).await;
    attached.send(b"\x1b[D");
    attached
        .wait_for("Agents lists retained execution", |frame| {
            frame.contains("Agents · local") && frame.contains(&title)
        })
        .await;
    select_overview_row(&mut attached, &title).await;
    attached.send(b"\r");
    let restored = attached
        .wait_for(
            "attached retention header remains Stopped without its events",
            |frame| {
                !frame.contains("Agents · local")
                    && conversation_body(frame)
                        .lines()
                        .any(|line| line.contains(CALL) && line.contains("Stopped"))
                    && conversation_body(frame)
                        .lines()
                        .any(|line| line.contains(QUEUED_QUESTION) && line.contains("Stopped"))
            },
        )
        .await;
    assert_eq!(tool_header_count(&restored, "Bash"), 1);
    assert_eq!(tool_header_count(&restored, "AskUserQuestion"), 1);
    eprintln!("[retention live]\n{original}\n[retention attached]\n{restored}");
    attached.close().await;
    view.close().await;
    for _ in 0..2 {
        tokio::time::timeout(UI_TIMEOUT, requests.recv())
            .await
            .unwrap()
            .unwrap();
    }
    if restart_daemon {
        client
            .execute(CommandEnvelope {
                daemon_epoch: client.daemon_epoch(),
                command_id: uuid::Uuid::new_v4(),
                command: AgentCommand::FlushTranscripts,
            })
            .await
            .unwrap();
        fixture.shutdown().await;
        let chat_id = evicted.conversation.id.to_string();
        let mut resumed = PtyView::launch(fixture, &["--resume", &chat_id], &[], true);
        resumed
            .wait_for("resumed history is ready", |frame| {
                frame.contains("History opened.")
            })
            .await;
        resumed.resize(120, 60).await;
        let restarted = resumed
            .wait_for("resumed interrupted batch remains Stopped", |frame| {
                let body = conversation_body(frame);
                body.lines()
                    .any(|line| line.contains(CALL) && line.contains("Stopped"))
                    && body
                        .lines()
                        .any(|line| line.contains(QUEUED_QUESTION) && line.contains("Stopped"))
            })
            .await;
        assert_eq!(tool_header_count(&restarted, "Bash"), 1);
        assert_eq!(tool_header_count(&restarted, "AskUserQuestion"), 1);
        assert!(!marker.exists());
        assert!(
            tokio::time::timeout(Duration::from_millis(250), requests.recv())
                .await
                .is_err(),
            "loading history must not request the model"
        );
        eprintln!("[retention restarted]\n{restarted}");
        resumed.close().await;
    }
}

#[tokio::test]
async fn actual_cli_interrupted_state_survives_bounded_event_log_eviction() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(interrupted_state_survives_event_eviction(&fixture, false))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_interrupted_state_survives_eviction_and_daemon_restart() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(interrupted_state_survives_event_eviction(&fixture, true))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
