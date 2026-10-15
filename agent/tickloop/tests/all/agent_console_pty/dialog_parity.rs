use super::*;

const FIRST: &str = "Which report should I shorten?";
const SECOND: &str = "Which section goes first?";
const THIRD: &str = "Which scope should I take?";
const ANSWERED: &str = "⏺ User answered the Agent's questions:";
const DECLINED: &str = "⏺ User declined to answer questions";
const EXIT_HINT: &str = "Press ctrl+c again to exit";

fn questions(texts: &[&str]) -> Value {
    let questions = texts
        .iter()
        .enumerate()
        .map(|(index, text)| {
            json!({
                "question": text,
                "header": format!("Step {}", index + 1),
                "options": [
                    {"label": "Yes", "description": "Go ahead"},
                    {"label": "No", "description": "Hold off"},
                ],
            })
        })
        .collect::<Vec<_>>();
    json!({ "questions": questions })
}

fn shows_question(frame: &str, question: &str) -> bool {
    frame.lines().any(|line| line.trim() == question) && frame.contains("esc cancel")
}

/// The conversation rows from the answers through the decline.
fn question_rows(text: &str) -> Vec<String> {
    let lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| *line == ANSWERED)
        .unwrap_or_else(|| panic!("no answers: {text}"));
    let end = lines
        .iter()
        .position(|line| *line == DECLINED)
        .unwrap_or_else(|| panic!("no decline: {text}"));
    lines[start..=end]
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

/// Two questions answered, then a third declined with Ctrl+C: as Claude Code draws
/// AskUserQuestion, no call row, the answers under one heading and the decline as one line; the
/// live conversation, its Ctrl+O transcript and another View show the same rows.
async fn answered_and_declined_questions(fixture: &PrivateFixture) {
    let (url, mut requests) = model_http_support::serve_responses(vec![
        (
            200,
            chat_reply(
                Some(("AskUserQuestion", "report", questions(&[FIRST, SECOND]))),
                "",
            ),
        ),
        (
            200,
            chat_reply(Some(("AskUserQuestion", "scope", questions(&[THIRD]))), ""),
        ),
    ])
    .await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start(fixture);
    view.wait_for("View ready", new_input_is_visible).await;
    view.resize(120, 40).await;
    view.send(b"Shorten the report\r");
    tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .expect("the provider receives the prompt")
        .unwrap();
    view.wait_for("the first question", |frame| shows_question(frame, FIRST))
        .await;
    view.send(b"\r");
    view.wait_for("the second question", |frame| shows_question(frame, SECOND))
        .await;
    view.send(b"\r");
    view.wait_for("the review", |frame| frame.contains("Review your answers"))
        .await;
    view.send(b"\r");
    tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .expect("the answers reach the provider")
        .unwrap();
    view.wait_for("the third question", |frame| shows_question(frame, THIRD))
        .await;
    view.send(b"\x03");
    let live = view
        .wait_for("Ctrl+C declines the question and ends the run", |frame| {
            conversation_body(frame).contains(DECLINED)
                && !frame.contains("• Working")
                && new_input_is_visible(frame)
        })
        .await;
    eprintln!("[question-parity-live]\n{live}\n[/question-parity-live]");
    let body = conversation_body(&live);
    let rows = question_rows(&body);
    assert_eq!(
        rows,
        [
            ANSWERED.to_owned(),
            format!("  ⎿  · {FIRST} → Yes"),
            format!("     · {SECOND} → Yes"),
            String::new(),
            DECLINED.to_owned(),
        ],
        "{live}"
    );
    for absent in ["AskUserQuestion", "Interrupted ·", "Failed", EXIT_HINT] {
        assert!(!live.contains(absent), "{absent}: {live}");
    }

    view.send(b"\x0f");
    let transcript = view
        .wait_for("the detailed transcript", |frame| {
            frame.contains("Showing detailed transcript") && frame.contains(DECLINED)
        })
        .await;
    assert_eq!(question_rows(&transcript), rows, "{transcript}");
    assert!(!transcript.contains("AskUserQuestion"), "{transcript}");
    view.send(b"\x0f");
    view.wait_for("the conversation again", |frame| {
        !frame.contains("Showing detailed transcript") && new_input_is_visible(frame)
    })
    .await;

    let mut attached = PtyView::start(fixture);
    attached
        .wait_for("second View ready", new_input_is_visible)
        .await;
    attached.resize(120, 40).await;
    attached.send(b"\x1b[D");
    attached
        .wait_for("Agents lists the conversation", |frame| {
            frame.contains("Agents · local") && frame.contains("Shorten the report")
        })
        .await;
    select_overview_row(&mut attached, "Shorten the report").await;
    attached.send(b"\r");
    let reopened = attached
        .wait_for("the other View shows the questions", |frame| {
            !frame.contains("Agents · local") && conversation_body(frame).contains(DECLINED)
        })
        .await;
    assert_eq!(
        question_rows(&conversation_body(&reopened)),
        rows,
        "{reopened}"
    );
    attached.close().await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_questions_show_only_their_answers_or_decline_in_every_view() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(answered_and_declined_questions(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Ctrl+C in a permission dialog rejects the call as Esc does, as Claude Code's
/// `PermissionRequest`; outside the dialog it still asks for a second press to exit.
async fn control_c_rejects_a_permission(fixture: &PrivateFixture) {
    const CALL: &str = "PERMISSION-CTRL-C-PROBE";
    let marker = fixture.path("permission-marker");
    let (url, mut requests) = model_http_support::serve_responses(vec![(
        200,
        chat_reply(
            Some((
                "Bash",
                "probe",
                json!({"command": format!("printf rejected > {}", marker.display()), "description": CALL}),
            )),
            "",
        ),
    )])
    .await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start_with_flags(fixture, &["--mode", "ask"]);
    view.wait_for("View ready", new_input_is_visible).await;
    view.send(b"Run the probe\r");
    tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .expect("the provider receives the prompt")
        .unwrap();
    view.wait_for("the permission dialog", |frame| {
        frame.contains("Permission required · Bash") && frame.contains(CALL)
    })
    .await;
    view.send(b"\x03");
    let rejected = view
        .wait_for("Ctrl+C rejects the call", |frame| {
            !frame.contains("Permission required")
                && new_input_is_visible(frame)
                && frame.contains("Interrupted · What should the Agent do instead?")
        })
        .await;
    eprintln!("[permission-ctrl-c]\n{rejected}\n[/permission-ctrl-c]");
    let body = conversation_body(&rejected);
    assert_eq!(
        body.matches("Interrupted · What should the Agent do instead?")
            .count(),
        1,
        "{rejected}"
    );
    assert!(!rejected.contains(EXIT_HINT), "{rejected}");
    assert!(!marker.exists());

    view.send(b"\x03");
    view.wait_for(
        "Ctrl+C outside the dialog asks again before exiting",
        |frame| frame.contains(EXIT_HINT),
    )
    .await;
    assert!(view.child.try_wait().unwrap().is_none());
    view.send(b"x");
    view.wait_for("another key cancels the exit", |frame| {
        !frame.contains(EXIT_HINT)
    })
    .await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_ctrl_c_rejects_a_permission_and_keeps_its_exit_meaning_outside() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(control_c_rejects_a_permission(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
