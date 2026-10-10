use super::*;

const PROMPT: &str = "Change the protocol";
const FIRST_QUESTION: &str = "Which change scope should I take?";
const FOLLOW_UP: &str = "No changes, write the current protocol as an HTML page";
const REPLY: &str = "PROTOCOL-HTML-REPLY";
const DECLINED: &str = "User declined to answer questions";

fn four_questions() -> Value {
    let questions = (1..=4)
        .map(|index| {
            json!({
                "question": if index == 1 {
                    FIRST_QUESTION.to_owned()
                } else {
                    format!("Question {index}?")
                },
                "header": format!("Step {index}"),
                "options": [
                    {"label": "Yes", "description": "Go ahead"},
                    {"label": "No", "description": "Hold off"},
                ],
            })
        })
        .collect::<Vec<_>>();
    json!({ "questions": questions })
}

/// The reported session: four questions, Esc on their dialog, then an ordinary message. Esc
/// declines once, with no typed-answer mode, and the message reaches the model as a new turn.
async fn esc_declines_and_the_next_message_is_a_new_turn(fixture: &PrivateFixture) {
    let (url, mut requests) = model_http_support::serve_responses(vec![
        (
            200,
            chat_reply(Some(("AskUserQuestion", "clarify", four_questions())), ""),
        ),
        (200, chat_reply(None, REPLY)),
    ])
    .await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start(fixture);
    view.wait_for("View ready", new_input_is_visible).await;
    view.send(format!("{PROMPT}\r").as_bytes());
    tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .expect("the provider receives the prompt")
        .unwrap();
    let dialog = view
        .wait_for("the question dialog", |frame| {
            frame.lines().any(|line| line.trim() == FIRST_QUESTION) && frame.contains("esc cancel")
        })
        .await;
    assert!(!dialog.contains("answer later"), "{dialog}");

    view.send(b"\x1b");
    let declined = view
        .wait_for("the declined question", |frame| {
            conversation_body(frame).contains(DECLINED) && new_input_is_visible(frame)
        })
        .await;
    eprintln!("[question-decline-frame]\n{declined}\n[/question-decline-frame]");
    let body = conversation_body(&declined);
    assert_eq!(body.matches(DECLINED).count(), 1, "{body}");
    assert!(
        body.lines()
            .any(|line| line.trim_end() == format!("⏺ {DECLINED}")),
        "the decline is one line of its own: {body}"
    );
    for absent in [
        "AskUserQuestion",
        "question was cancelled",
        "Failed",
        "What should the Agent do instead?",
        "Your input is needed",
        "Enter your answer below",
        "still waiting",
        "1/4",
    ] {
        assert!(!body.contains(absent), "{absent}: {body}");
    }

    view.send(format!("{FOLLOW_UP}\r").as_bytes());
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .expect("the follow-up reaches the model as an ordinary message")
        .unwrap();
    let messages = request["messages"].to_string();
    for expected in [
        FOLLOW_UP,
        "The user doesn't want to proceed with this tool use.",
        "[Request interrupted by user for tool use]",
    ] {
        assert!(messages.contains(expected), "{expected}: {messages}");
    }
    let replied = view
        .wait_for("the Agent replies", |frame| frame.contains(REPLY))
        .await;
    let body = conversation_body(&replied);
    assert!(
        body_shows_once_in_order(&replied, &[PROMPT, DECLINED, FOLLOW_UP, REPLY]),
        "{body}"
    );
    assert!(!body.contains("Answer recorded"), "{body}");
    view.close().await;
}

#[tokio::test]
async fn actual_cli_esc_declines_a_question_and_the_next_message_starts_a_new_turn() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(esc_declines_and_the_next_message_is_a_new_turn(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
