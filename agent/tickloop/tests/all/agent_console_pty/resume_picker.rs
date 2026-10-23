use super::*;

use tickloop_agent_protocol::{ChatId, EventKind};
use tickloop_agent_runtime::Agent;
use unicode_width::UnicodeWidthStr;

const LONG_PROMPT: &str = "请分析腾讯控股最近三个季度的财报，\n比较营收增长与利润率变化，并整理一份包含风险提示、估值区间和后续跟踪指标的长篇研究备忘录";
const DUPLICATE_PROMPT: &str = "Check the jk ledger notes";
const LONG_REPLY: &str = "Quarterly memo answer";
const OLDER_DUPLICATE_REPLY: &str = "Older duplicate answer BIRCH-7";
const NEWER_DUPLICATE_REPLY: &str = "Newer duplicate answer ASPEN-3";
const FOLLOW_UP: &str = "Which answer did you give?";
const FOLLOW_UP_REPLY: &str = "Follow-up reached the selected conversation";

async fn complete_turn(agent: &Agent, chat: ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("fixture turn failed: {error}"),
            _ => {}
        }
    }
}

/// Saves three conversations of the fixture's working directory, newest last in the returned
/// order: a long multiline Chinese prompt, then two conversations with the same prompt.
async fn save_conversations(fixture: &PrivateFixture) -> Vec<ChatId> {
    let root = fixture.path("config");
    let workdir = fixture.path("work").canonicalize().unwrap();
    let agent = Agent::open_embedded(
        root.clone(),
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text(LONG_REPLY),
            tickloop_agent_test_support::text(OLDER_DUPLICATE_REPLY),
            tickloop_agent_test_support::text(NEWER_DUPLICATE_REPLY),
        ])),
    )
    .unwrap();
    let mut chats = Vec::new();
    for prompt in [LONG_PROMPT, DUPLICATE_PROMPT, DUPLICATE_PROMPT] {
        let chat = agent.create_chat(workdir.clone()).await.unwrap();
        complete_turn(&agent, chat, prompt).await;
        chats.push(chat);
    }
    agent.shutdown().await;
    drop(agent);
    let now = std::time::SystemTime::now();
    for (chat, age) in chats.iter().zip([10_800, 7_200, 60]) {
        std::fs::File::options()
            .write(true)
            .open(tickloop_agent_test_support::transcript_file(&root, *chat))
            .unwrap()
            .set_modified(now - Duration::from_secs(age))
            .unwrap();
    }
    chats
}

fn picker_rows(frame: &str) -> Vec<&str> {
    frame.lines().map(str::trim_end).collect()
}

fn assert_compact_picker(frame: &str, width: usize, chats: &[ChatId]) {
    for line in frame.lines() {
        assert!(
            line.trim_end().width() <= width,
            "row exceeds {width} columns: {frame}"
        );
    }
    for chat in chats {
        let id = chat.to_string();
        assert!(
            !frame.contains(&id) && !frame.contains(&id[..8]),
            "the picker must not show full or short IDs: {frame}"
        );
    }
    assert!(!frame.contains("Cancel"), "{frame}");
}

async fn run_resume_picker(fixture: &PrivateFixture) {
    let chats = save_conversations(fixture).await;
    let (base_url, mut requests) =
        model_http_support::serve_responses(vec![(200, chat_reply(None, FOLLOW_UP_REPLY))]).await;
    fixture.set_chat_model(&base_url);
    let mut view = PtyView::start_saving_chats(fixture);
    view.wait_for("new View ready", new_input_is_visible).await;

    view.send(b"/resume\r");
    let opened = view
        .wait_for("compact Resume picker", |frame| {
            frame.contains("Resume Chat · 1 of 3") && frame.contains("⌕ Search…")
        })
        .await;
    eprintln!("[resume-picker 100x32]\n{opened}\n[/resume-picker]");
    assert_compact_picker(&opened, 100, &chats);
    let rows = picker_rows(&opened);
    let newest = rows
        .iter()
        .position(|row| *row == format!("❯ {DUPLICATE_PROMPT}"))
        .expect("the newest conversation is selected first");
    assert_eq!(rows[newest + 1], "  1m ago");
    assert_eq!(rows[newest + 2], format!("  {DUPLICATE_PROMPT}"));
    assert_eq!(rows[newest + 3], "  2h ago");
    assert!(
        rows[newest + 4].starts_with("  请分析腾讯控股最近三个季度的财报， 比较营收增长")
            && rows[newest + 4].ends_with('…'),
        "a long multiline prompt is one bounded row: {opened}"
    );
    assert_eq!(rows[newest + 5], "  3h ago");

    view.resize(40, 12).await;
    let narrow = view
        .wait_for("picker bounded at 40×12", |frame| {
            frame.contains("1 of 3") && frame.contains(&format!("❯ {DUPLICATE_PROMPT}"))
        })
        .await;
    eprintln!("[resume-picker 40x12]\n{narrow}\n[/resume-picker]");
    assert_compact_picker(&narrow, 40, &chats);
    view.resize(TERMINAL_WIDTH, TERMINAL_HEIGHT).await;

    view.send(b"\x1b");
    let cancelled = view
        .wait_for("Esc closes the picker unchanged", |frame| {
            new_input_is_visible(frame) && !frame.contains("Resume Chat")
        })
        .await;
    assert!(!cancelled.contains(OLDER_DUPLICATE_REPLY), "{cancelled}");
    assert!(!cancelled.contains(NEWER_DUPLICATE_REPLY), "{cancelled}");

    view.send(b"/resume\r");
    view.wait_for("picker reopens", |frame| {
        frame.contains("Resume Chat · 1 of 3")
    })
    .await;
    view.send(b"j");
    view.wait_for("list j selects the older duplicate", |frame| {
        frame.contains("Resume Chat · 2 of 3")
    })
    .await;
    view.send(b"/jk");
    let filtered = view
        .wait_for("search types literal j/k", |frame| {
            frame.contains("⌕ jk") && frame.contains("2 of 2 matching")
        })
        .await;
    eprintln!("[resume-picker filtered]\n{filtered}\n[/resume-picker]");
    assert_compact_picker(&filtered, 100, &chats);
    assert!(!filtered.contains("请分析"), "{filtered}");
    let rows = picker_rows(&filtered);
    let selected = rows
        .iter()
        .position(|row| *row == format!("❯ {DUPLICATE_PROMPT}"))
        .unwrap();
    assert_eq!(rows[selected + 1], "  2h ago", "{filtered}");

    view.send(b"\r");
    let history = view
        .wait_for("Enter opens the selected duplicate's history", |frame| {
            frame.contains(OLDER_DUPLICATE_REPLY)
                && frame.contains("History only; selecting this Chat starts no work.")
        })
        .await;
    eprintln!("[resume-picker opened history]\n{history}\n[/resume-picker]");
    assert!(!history.contains(NEWER_DUPLICATE_REPLY), "{history}");
    assert!(
        history.contains(&format!("Tickloop v{}", env!("CARGO_PKG_VERSION"))),
        "a Chat opened from the picker keeps the startup banner: {history}"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        requests.try_recv().is_err(),
        "opening saved history must not call the model"
    );

    view.keep_history_paused().await;
    assert!(requests.try_recv().is_err(), "Esc must not call the model");
    view.send(format!("{FOLLOW_UP}\r").as_bytes());
    view.wait_for("explicit follow-up answered", |frame| {
        frame.contains(FOLLOW_UP_REPLY) && !frame.contains("Triggers paused")
    })
    .await;
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let sent = request["messages"].to_string();
    assert!(sent.contains(OLDER_DUPLICATE_REPLY), "{sent}");
    assert!(sent.contains(FOLLOW_UP), "{sent}");
    assert!(!sent.contains(NEWER_DUPLICATE_REPLY), "{sent}");
    view.close().await;
}

#[tokio::test]
async fn actual_cli_typing_into_continue_prompt_preserves_the_message_and_resumes_receiving() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let chats = save_conversations(&fixture).await;
        let (base_url, mut requests) =
            model_http_support::serve_responses(vec![(200, chat_reply(None, FOLLOW_UP_REPLY))])
                .await;
        fixture.set_chat_model(&base_url);
        let chat = chats[1].to_string();
        let mut view = PtyView::launch(&fixture, &["--resume", &chat], &[], true);
        view.wait_for("history offers Continue before typing", |frame| {
            frame.contains("Continue Chat?") && frame.contains(OLDER_DUPLICATE_REPLY)
        })
        .await;
        assert!(requests.try_recv().is_err());
        let follow_up = format!("no, {FOLLOW_UP}");
        view.send(follow_up.as_bytes());
        view.wait_for_editor("typing preserves the full draft", Some(&follow_up))
            .await;
        assert!(view.frame().contains("Triggers paused"));
        assert!(
            requests.try_recv().is_err(),
            "a draft must not resume execution"
        );
        view.send(b"\r");
        view.wait_for("typed follow-up answered and receiving resumed", |frame| {
            frame.contains(FOLLOW_UP_REPLY) && !frame.contains("Triggers paused")
        })
        .await;
        let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
            .await
            .unwrap()
            .unwrap();
        let sent = request["messages"].to_string();
        assert!(sent.contains(&follow_up), "{sent}");
        assert!(sent.contains(OLDER_DUPLICATE_REPLY), "{sent}");
        assert!(!sent.contains(NEWER_DUPLICATE_REPLY), "{sent}");
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_cli_resume_picker_is_compact_searchable_and_opens_the_exact_history() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_resume_picker(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
