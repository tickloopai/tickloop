use super::*;

const SCRIPT: &str = "await tools.Write({file_path:'nested-note.txt',content:'NESTED-BODY'});\nconst body = await tools.Read({file_path:'nested-note.txt'});\ntext(body.includes('NESTED-BODY') ? 'SCRIPT-SELECTED' : 'BAD');";
const PROMPT: &str = "Write and read a note with a script.";
const REPLY: &str = "CODEMODE-REPLY";

/// The rows of the Codemode call: its own row, the calls the script made under it, then its
/// result.
fn assert_nested_under_codemode(frame: &str) {
    let lines = frame.lines().collect::<Vec<_>>();
    let position = |label: &str, matches: &dyn Fn(&str) -> bool| {
        lines
            .iter()
            .position(|line| matches(line))
            .unwrap_or_else(|| panic!("{label} missing:\n{frame}"))
    };
    let parent = position("Codemode row", &|line| {
        line.starts_with("⏺ Codemode · 3-line script")
    });
    let write = position("nested Write row", &|line| {
        line.starts_with("     ⏺ Write · nested-note.txt")
    });
    let read = position("nested Read row", &|line| {
        line.starts_with("     ⏺ Read · nested-note.txt")
    });
    let result = position("Codemode result", &|line| {
        line.starts_with("  ⎿  Script completed")
    });
    assert!(parent < write && write < read && read < result, "{frame}");
    assert!(
        lines[write + 1].starts_with("       ⎿  Wrote 1 line"),
        "{frame}"
    );
    assert!(
        !frame.contains("Read 1 file"),
        "a script's read is not folded into a group: {frame}"
    );
}

async fn script_calls_show_under_their_codemode_call(fixture: &PrivateFixture) {
    let (url, _requests) = model_http_support::serve_responses(vec![
        (
            200,
            chat_reply(Some(("Codemode", "script", json!({ "code": SCRIPT }))), ""),
        ),
        (200, chat_reply(None, REPLY)),
    ])
    .await;
    fixture.set_chat_model(&url);
    let mut view = PtyView::start_with_flags(fixture, &["--dangerously-skip-permissions"]);
    view.wait_for("Codemode View ready", new_input_is_visible)
        .await;
    view.send(format!("{PROMPT}\r").as_bytes());
    let live = view
        .wait_for("the script's reply arrives", |frame| frame.contains(REPLY))
        .await;
    eprintln!("[codemode-live]\n{live}\n[/codemode-live]");
    assert_nested_under_codemode(&conversation_body(&live));

    view.send(b"\x0f");
    let transcript = view
        .wait_for("the detailed transcript shows the script", |frame| {
            frame.contains("Showing detailed transcript") && frame.contains(REPLY)
        })
        .await;
    eprintln!("[codemode-transcript]\n{transcript}\n[/codemode-transcript]");
    let lines = transcript.lines().collect::<Vec<_>>();
    let script_line = lines
        .iter()
        .position(|line| {
            line.trim_start() == "const body = await tools.Read({file_path:'nested-note.txt'});"
        })
        .unwrap_or_else(|| panic!("the script is shown line by line: {transcript}"));
    let nested = lines
        .iter()
        .position(|line| line.starts_with("     ⏺ Write(") && line.contains("nested-note.txt"))
        .unwrap_or_else(|| panic!("the script's calls are under it: {transcript}"));
    let result = lines
        .iter()
        .position(|line| line.starts_with("  ⎿  Script completed"))
        .unwrap_or_else(|| panic!("{transcript}"));
    assert!(script_line < nested && nested < result, "{transcript}");
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
            frame.contains("Agents · local") && frame.contains(PROMPT)
        })
        .await;
    select_overview_row(&mut attached, PROMPT).await;
    attached.send(b"\r");
    let reopened = attached
        .wait_for("the other View shows the script", |frame| {
            !frame.contains("Agents · local") && conversation_body(frame).contains(REPLY)
        })
        .await;
    eprintln!("[codemode-reopened]\n{reopened}\n[/codemode-reopened]");
    assert_nested_under_codemode(&conversation_body(&reopened));
    attached.close().await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_nests_script_calls_under_their_codemode_call() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(script_calls_show_under_their_codemode_call(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
