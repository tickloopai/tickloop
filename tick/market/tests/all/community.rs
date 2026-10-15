use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use tick_market::{
    args::{CommunityArgs, OnceArgs},
    commands::community,
    config::Settings,
    metadata::TickKind,
};
use tick_protocol::{CALL_METHOD, TOOL_FAILED};
use tick_provider::{
    community::{
        CommunityApi, MemberPost, MemberProfile, PostKind, PostedReply, reply_outcome_unknown,
    },
    error::Result,
};
use tick_sdk::stream::TickStream;
use tokio::io::AsyncWriteExt;

const MEMBER_ID: i64 = 91;
const LISTED_TOPIC: i64 = 6993508780031016960;

/// A member whose feed answers each request with the next scripted list, repeating the last one,
/// and who records every reply attempt instead of posting it.
struct Member {
    feeds: Mutex<VecDeque<Vec<MemberPost>>>,
    replies: Mutex<Vec<(i64, String)>>,
    replies_fail: bool,
}

impl Member {
    fn new(feeds: impl IntoIterator<Item = Vec<MemberPost>>) -> Arc<Self> {
        Arc::new(Self {
            feeds: Mutex::new(feeds.into_iter().collect()),
            replies: Mutex::new(Vec::new()),
            replies_fail: false,
        })
    }

    fn failing_replies(feeds: impl IntoIterator<Item = Vec<MemberPost>>) -> Arc<Self> {
        Arc::new(Self {
            feeds: Mutex::new(feeds.into_iter().collect()),
            replies: Mutex::new(Vec::new()),
            replies_fail: true,
        })
    }
}

#[async_trait]
impl CommunityApi for Member {
    async fn profile(&self, member_id: i64) -> Result<MemberProfile> {
        Ok(MemberProfile {
            member_id,
            nickname: "资深韭菜".into(),
            description: String::new(),
        })
    }

    async fn posts(&self, _: i64) -> Result<Vec<MemberPost>> {
        let mut feeds = self.feeds.lock().unwrap();
        Ok(if feeds.len() > 1 {
            feeds.pop_front().unwrap()
        } else {
            feeds.front().cloned().unwrap()
        })
    }

    async fn reply(&self, topic_id: i64, body: &str) -> Result<PostedReply> {
        self.replies
            .lock()
            .unwrap()
            .push((topic_id, body.to_owned()));
        if self.replies_fail {
            return Err(reply_outcome_unknown("request timeout"));
        }
        Ok(PostedReply {
            reply_id: 48453318,
            topic_id,
            body: body.to_owned(),
            created_at: "2026-10-14T10:33:22Z".into(),
        })
    }
}

fn post(topic_id: i64, text: &str) -> MemberPost {
    MemberPost {
        topic_id,
        kind: PostKind::Post,
        title: String::new(),
        text: text.into(),
        published_at: "2026-10-14T09:00:00Z".into(),
        url: format!("https://longbridge.com/topics/{topic_id}"),
    }
}

fn arguments() -> CommunityArgs {
    CommunityArgs {
        member_id: MEMBER_ID,
        output: OnceArgs::default(),
    }
}

fn call(id: u64, tool: &str, args: Value) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": CALL_METHOD,
        "params": {"tool": tool, "args": args, "revision": "1"},
    })
}

/// Runs the Community Tick over an in-memory duplex transport until the given requests are
/// answered, and returns every frame it wrote back.
async fn serve(api: Arc<Member>, requests: &[Value]) -> Vec<Value> {
    let (mut client_requests, tick_requests) = tokio::io::duplex(8 * 1024);
    let (tick_responses, mut client_responses) = tokio::io::duplex(64 * 1024);
    let mut session = TickStream::duplex(TickKind::Community, tick_requests, tick_responses);
    for request in requests {
        let mut line = serde_json::to_vec(request).unwrap();
        line.push(b'\n');
        client_requests.write_all(&line).await.unwrap();
    }
    client_requests.shutdown().await.unwrap();
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    community::run_live(
        &arguments(),
        api,
        &Settings::default(),
        &mut terminal,
        futures_util::stream::pending(),
        &mut session,
    )
    .await
    .unwrap();
    session.close().await;
    let mut bytes = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut client_responses, &mut bytes)
        .await
        .unwrap();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn answer(frames: &[Value], id: u64) -> &Value {
    frames
        .iter()
        .find(|frame| frame["id"] == id)
        .unwrap_or_else(|| panic!("no answer to {id}: {frames:?}"))
}

fn updates(frames: &[Value]) -> Vec<&Value> {
    frames
        .iter()
        .filter(|frame| frame["method"] == "tick/update")
        .map(|frame| &frame["params"]["data"])
        .collect()
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[tokio::test]
async fn a_reply_goes_once_under_a_listed_post_and_answers_with_the_posted_reply() {
    let api = Member::new([vec![post(LISTED_TOPIC, "Tesla again")]]);
    let frames = serve(
        api.clone(),
        &[
            call(
                1,
                "reply",
                json!({"topic_id": LISTED_TOPIC, "body": "  Nice call  "}),
            ),
            call(2, "reply", json!({"topic_id": 1, "body": "Elsewhere"})),
            call(3, "reply", json!({"topic_id": LISTED_TOPIC, "body": "   "})),
        ],
    )
    .await;

    assert_eq!(
        *api.replies.lock().unwrap(),
        [(LISTED_TOPIC, "Nice call".to_owned())]
    );
    assert_eq!(
        answer(&frames, 1)["result"],
        json!({
            "reply_id": 48453318, "topic_id": LISTED_TOPIC, "body": "Nice call",
            "created_at": "2026-10-14T10:33:22Z",
        })
    );
    let unlisted = answer(&frames, 2)["error"]["message"].as_str().unwrap();
    assert!(
        unlisted.contains("topic 1 is not one of 资深韭菜's newest posts"),
        "{unlisted}"
    );
    assert_eq!(answer(&frames, 3)["error"]["message"], "a reply needs text");
    assert_eq!(
        updates(&frames).len(),
        1,
        "a reply leaves the state unchanged: {frames:?}"
    );
}

#[tokio::test]
async fn a_failed_reply_is_attempted_once_and_says_its_outcome_is_unknown() {
    let api = Member::failing_replies([vec![post(LISTED_TOPIC, "Tesla again")]]);
    let frames = serve(
        api.clone(),
        &[call(
            1,
            "reply",
            json!({"topic_id": LISTED_TOPIC, "body": "Nice call"}),
        )],
    )
    .await;

    assert_eq!(api.replies.lock().unwrap().len(), 1);
    let error = &answer(&frames, 1)["error"];
    assert_eq!(error["code"], TOOL_FAILED);
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("outcome unknown"), "{message}");
    assert_eq!(updates(&frames).len(), 1, "{frames:?}");
}

#[tokio::test]
async fn a_post_newer_than_the_shown_feed_is_found_by_a_fresh_read_before_replying() {
    let api = Member::new([
        vec![post(1, "First")],
        vec![post(2, "Brand new"), post(1, "First")],
    ]);
    let frames = serve(
        api.clone(),
        &[call(1, "reply", json!({"topic_id": 2, "body": "Welcome"}))],
    )
    .await;

    assert_eq!(*api.replies.lock().unwrap(), [(2, "Welcome".to_owned())]);
    assert_eq!(answer(&frames, 1)["result"]["topic_id"], 2);
    let published = updates(&frames);
    assert_eq!(published.len(), 2, "{frames:?}");
    assert_eq!(published[1]["posts"][0]["topic_id"], 2);
}

#[tokio::test]
async fn the_posts_tool_recovers_exactly_the_published_state() {
    let api = Member::new([vec![post(2, "Second"), post(1, "First")]]);
    let frames = serve(api, &[call(1, "posts", json!({}))]).await;

    let published = &frames
        .iter()
        .find(|frame| frame["method"] == "tick/update")
        .unwrap()["params"]["data"];
    assert_eq!(answer(&frames, 1)["result"], *published);
    assert_eq!(published["member"]["member_id"], MEMBER_ID);
    assert_eq!(published["posts"][0]["topic_id"], 2);
}

#[tokio::test(start_paused = true)]
async fn the_thirty_second_refresh_publishes_a_new_post_and_stays_quiet_otherwise() {
    let api = Member::new([
        vec![post(1, "First")],
        vec![post(2, "Brand new"), post(1, "First")],
    ]);
    let input = futures_util::stream::once(async {
        tokio::time::sleep(Duration::from_secs(95)).await;
        Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
    })
    .boxed();
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let mut events = TickStream::in_memory(TickKind::Community);
    community::run_live(
        &arguments(),
        api,
        &Settings::default(),
        &mut terminal,
        input,
        &mut events,
    )
    .await
    .unwrap();

    let records = events.records().all();
    let newest_topics: Vec<_> = records
        .iter()
        .map(|record| record["data"]["posts"][0]["topic_id"].clone())
        .collect();
    assert_eq!(newest_topics, [json!(1), json!(2)], "{records:?}");
    let screen = screen(&terminal);
    assert!(screen.contains("Brand new"), "{screen}");
}

#[tokio::test(start_paused = true)]
async fn the_selected_post_stays_selected_when_a_new_post_arrives_above_it() {
    let api = Member::new([
        vec![post(2, "Second"), post(1, "First")],
        vec![post(3, "Brand new"), post(2, "Second"), post(1, "First")],
    ]);
    // The second post is selected before the refresh at 30 s and opened after it; End and Down
    // arrive together before any frame clamps the scroll.
    let input = futures_util::stream::iter([
        (Duration::from_secs(1), KeyCode::Down),
        (Duration::from_secs(31), KeyCode::Enter),
        (Duration::ZERO, KeyCode::Char('G')),
        (Duration::ZERO, KeyCode::Char('j')),
        (Duration::from_secs(1), KeyCode::Char('q')),
    ])
    .then(|(delay, code)| async move {
        tokio::time::sleep(delay).await;
        Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    })
    .boxed();
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
    let mut events = TickStream::in_memory(TickKind::Community);
    community::run_live(
        &arguments(),
        api,
        &Settings::default(),
        &mut terminal,
        input,
        &mut events,
    )
    .await
    .unwrap();

    let screen = screen(&terminal);
    assert!(screen.contains("· topic 1"), "{screen}");
    assert!(!screen.contains("Brand new"), "{screen}");
}
