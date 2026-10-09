use super::*;

const PROMPT: &str = "echo probe prompt to take back";
const EDIT: &str = " after editing";
const REPLY: &str = "edited prompt answered";
const INTERRUPTED_NOTICE: &str = "What should the Agent do instead?";

fn request_has_message(request: &Value, text: &str) -> bool {
    request["messages"]
        .as_array()
        .is_some_and(|messages| messages.iter().any(|message| message["content"] == text))
}

async fn esc_before_any_reply(fixture: &PrivateFixture) {
    let (base_url, mut requests, _release) =
        model_http_support::serve_responses_with_gated_response(
            vec![
                (200, chat_reply(None, "never delivered")),
                (200, chat_reply(None, REPLY)),
            ],
            0,
        )
        .await;
    fixture.set_chat_model(&base_url);
    let mut view = PtyView::start(fixture);
    view.wait_for("View ready", new_input_is_visible).await;

    view.send(format!("{PROMPT}\r").as_bytes());
    tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .expect("the provider receives the prompt")
        .unwrap();
    view.wait_for("the turn is running", |frame| {
        frame.contains("esc to interrupt") && conversation_body(frame).contains(PROMPT)
    })
    .await;
    view.send(b"\x1b");
    let frame = view
        .wait_for("the prompt returns to the editor", |frame| {
            composer_line(frame).contains(PROMPT) && !conversation_body(frame).contains(PROMPT)
        })
        .await;
    eprintln!("[interrupted-prompt-frame after esc]\n{frame}\n[/interrupted-prompt-frame]");
    assert!(!frame.contains(INTERRUPTED_NOTICE), "{frame}");
    let client = fixture.client().await;
    let rows = catalog(&client).await;
    assert_eq!(rows.len(), 1);
    let stored = snapshot(&client, rows[0].execution).await;
    assert!(
        stored.conversation.messages.is_empty(),
        "the taken-back prompt leaves the Agent's history: {:?}",
        stored.conversation.messages
    );

    view.send(EDIT.as_bytes());
    view.send(b"\r");
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .expect("the provider receives the edited prompt")
        .unwrap();
    assert!(
        request_has_message(&request, &format!("{PROMPT}{EDIT}")),
        "{request}"
    );
    assert!(!request_has_message(&request, PROMPT), "{request}");
    assert!(
        !request["messages"]
            .to_string()
            .contains("Request interrupted"),
        "{request}"
    );
    let frame = view
        .wait_for("the edited prompt is answered", |frame| {
            frame.contains(REPLY)
        })
        .await;
    assert!(
        body_shows_once_in_order(&frame, &[&format!("{PROMPT}{EDIT}"), REPLY]),
        "{frame}"
    );
    assert!(!frame.contains(INTERRUPTED_NOTICE), "{frame}");
    view.close().await;
}

#[tokio::test]
async fn actual_cli_esc_before_any_reply_puts_the_prompt_back_for_editing() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(esc_before_any_reply(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
