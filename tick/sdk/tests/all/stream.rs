use serde_json::{Value, json};
use tick_protocol::{
    CALL_METHOD, DESCRIBE_METHOD, FRAME_TOO_LARGE, INVALID_PARAMS, INVALID_REQUEST,
    MAX_REQUEST_BYTES, MAX_TOOL_RESPONSE_BYTES, NATIVE_PROTOCOL_VERSION, READ_METHOD,
    REVISION_MISMATCH, SeriesEnd, StateDeclaration, StateSeries, TOOL_FAILED, TickDescriptor,
    TickTool, UPDATE_METHOD,
};
use tick_sdk::stream::{Describe, InboundFrame, TickStream, ToolRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A third-party Tick that knows nothing of the financial Ticks.
struct Counter {
    revision: &'static str,
    tools: Vec<TickTool>,
}

impl Counter {
    fn with_read_tool() -> Self {
        Self {
            revision: "1",
            tools: vec![TickTool {
                name: "current".into(),
                description: "Read the value".into(),
                input_schema: json!({"type": "object"}),
                mutates: false,
            }],
        }
    }
}

impl Describe for Counter {
    fn descriptor(&self) -> Result<TickDescriptor, String> {
        Ok(TickDescriptor {
            protocol: NATIVE_PROTOCOL_VERSION,
            kind: "example.counter".into(),
            title: "Counter".into(),
            description: "Counts".into(),
            revision: self.revision.into(),
            recover_with: Some("current".into()),
            tools: self.tools.clone(),
            state: Some(StateDeclaration {
                series: vec![StateSeries {
                    path: "/history".into(),
                    newest: SeriesEnd::Last,
                }],
            }),
        })
    }
}

/// Feeds `requests` to a stream for `tick`, publishes `published`, and returns every frame the
/// stream wrote back with the `tick/call` requests it surfaced to the runtime.
async fn serve(
    tick: Counter,
    requests: &[Value],
    published: Option<Value>,
) -> (Vec<Value>, Vec<ToolRequest>) {
    let lines: Vec<String> = requests.iter().map(Value::to_string).collect();
    serve_lines(tick, &lines, published).await
}

/// [`serve`] for request lines written as they are, valid or not.
async fn serve_lines(
    tick: Counter,
    lines: &[String],
    published: Option<Value>,
) -> (Vec<Value>, Vec<ToolRequest>) {
    let (mut client_requests, tick_requests) = tokio::io::duplex(8 * 1024);
    let (tick_responses, mut client_responses) = tokio::io::duplex(64 * 1024);
    let mut stream = TickStream::duplex(tick, tick_requests, tick_responses);
    if let Some(data) = published {
        stream.publish(data).unwrap();
    }
    for line in lines {
        client_requests
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }
    client_requests.shutdown().await.unwrap();
    let mut inbound = stream.take_requests();
    let mut dispatched = Vec::new();
    while let Some(frame) = inbound.next().await {
        dispatched.extend(stream.dispatch(frame));
    }
    stream.close().await;
    let mut bytes = Vec::new();
    client_responses.read_to_end(&mut bytes).await.unwrap();
    let frames = String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (frames, dispatched)
}

#[tokio::test]
async fn any_tick_answers_describe_and_read_with_what_it_declares_and_publishes() {
    let (frames, _) = serve(
        Counter::with_read_tool(),
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": DESCRIBE_METHOD}),
            json!({"jsonrpc": "2.0", "id": 2, "method": READ_METHOD}),
        ],
        Some(json!({"value": 3})),
    )
    .await;
    let update = frames
        .iter()
        .find(|frame| frame["method"] == UPDATE_METHOD)
        .unwrap();
    let mut fields: Vec<_> = update["params"].as_object().unwrap().keys().collect();
    fields.sort();
    assert_eq!(fields, ["at", "data", "seq"]);
    assert_eq!(update["params"]["seq"], 1);
    let answer = |id: u64| frames.iter().find(|frame| frame["id"] == id).unwrap();
    assert_eq!(answer(1)["result"]["kind"], "example.counter");
    assert_eq!(answer(1)["result"]["revision"], "1");
    assert_eq!(answer(1)["result"]["recover_with"], "current");
    assert_eq!(
        answer(1)["result"]["state"],
        json!({"series":[{"path":"/history","newest":"last"}]})
    );
    assert_eq!(answer(2)["result"], update["params"]);
}

#[tokio::test]
async fn a_tick_without_content_reads_as_null_data() {
    let (frames, _) = serve(
        Counter::with_read_tool(),
        &[json!({"jsonrpc": "2.0", "id": 1, "method": READ_METHOD})],
        None,
    )
    .await;
    assert_eq!(frames[0]["result"], json!({"seq": 0, "data": null}));
}

#[tokio::test]
async fn a_call_runs_only_against_the_current_catalog_revision() {
    let call = |id: u64, params: Value| json!({"jsonrpc": "2.0", "id": id, "method": CALL_METHOD, "params": params});
    let (frames, dispatched) = serve(
        Counter {
            revision: "2",
            ..Counter::with_read_tool()
        },
        &[
            call(1, json!({"tool": "current", "args": {}, "revision": "1"})),
            call(2, json!({"tool": "current", "args": {}})),
            call(
                3,
                json!({"tool": "current", "args": {"n": 1}, "revision": "2"}),
            ),
        ],
        None,
    )
    .await;
    let answer = |id: u64| frames.iter().find(|frame| frame["id"] == id).unwrap();
    assert_eq!(answer(1)["error"]["code"], REVISION_MISMATCH);
    assert_eq!(answer(2)["error"]["code"], INVALID_PARAMS);
    assert_eq!(
        dispatched,
        [ToolRequest {
            id: json!(3),
            tool: "current".into(),
            args: json!({"n": 1}),
        }],
        "only the call naming the current revision reaches the runtime"
    );
}

#[tokio::test]
async fn a_descriptor_that_breaks_the_standard_is_refused_instead_of_answered() {
    let undeclared_recovery = Counter {
        revision: "1",
        tools: Vec::new(),
    };
    let (frames, _) = serve(
        undeclared_recovery,
        &[json!({"jsonrpc": "2.0", "id": 1, "method": DESCRIBE_METHOD})],
        None,
    )
    .await;
    assert_eq!(frames[0]["error"]["code"], INVALID_REQUEST);
}

/// A read answered after an update was published reports that update's seq, so writing the
/// update first keeps both in order on the wire.
#[tokio::test]
async fn a_queued_update_is_written_before_the_answers_queued_with_it() {
    let (_requests, tick_requests) = tokio::io::duplex(8 * 1024);
    let (tick_responses, mut client_responses) = tokio::io::duplex(64 * 1024);
    let mut stream = TickStream::duplex(Counter::with_read_tool(), tick_requests, tick_responses);
    stream.publish(json!({"value": 1})).unwrap();
    let read = json!({"jsonrpc": "2.0", "id": 1, "method": READ_METHOD});
    assert!(
        stream
            .dispatch(InboundFrame::Line(read.to_string()))
            .is_none()
    );
    stream.close().await;
    let mut bytes = Vec::new();
    client_responses.read_to_end(&mut bytes).await.unwrap();
    let frames: Vec<Value> = String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["method"], UPDATE_METHOD);
    assert_eq!(frames[1]["id"], 1);
    assert_eq!(frames[1]["result"]["seq"], frames[0]["params"]["seq"]);
}

#[test]
fn a_tick_update_needs_data() {
    let mut stream = TickStream::in_memory(Counter::with_read_tool());
    assert!(stream.publish(Value::Null).is_err());
    stream.publish(json!({"value": 1})).unwrap();
    stream.publish(json!({"value": 1})).unwrap();
    assert_eq!(stream.seq(), 1, "unchanged data publishes nothing");
    assert_eq!(stream.records().len(), 1);
}

/// A request over the limit is refused under the `id` read before the limit, as a host encodes
/// it first; one whose `id` comes only after the limit, or loses digits to it, goes unanswered.
#[tokio::test]
async fn a_request_over_the_limit_is_refused_under_its_own_id() {
    let padding = "x".repeat(MAX_REQUEST_BYTES);
    // Pads a line so that the limit falls right after `before_id`.
    let padded_to_limit = |before_id: &str, after_limit: &str| {
        let before_padding = r#"{"jsonrpc":"2.0","params":{"padding":""#;
        let padding = "x".repeat(MAX_REQUEST_BYTES - before_padding.len() - before_id.len());
        format!("{before_padding}{padding}{before_id}{after_limit}")
    };
    let (frames, _) = serve_lines(
        Counter::with_read_tool(),
        &[
            format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tick/read","params":{{"padding":"{padding}"}}}}"#),
            format!(r#"{{"jsonrpc":"2.0","params":{{"padding":"{padding}"}},"id":2,"method":"tick/read"}}"#),
            padded_to_limit(r#""},"id":12"#, r#"3456,"method":"tick/read"}"#),
            padded_to_limit(r#""},"id":4,"#, r#""method":"tick/read"}"#),
            json!({"jsonrpc": "2.0", "id": 3, "method": READ_METHOD}).to_string(),
        ],
        None,
    )
    .await;
    let answers: Vec<_> = frames
        .iter()
        .map(|frame| (frame["id"].clone(), frame["error"]["code"].clone()))
        .collect();
    assert_eq!(
        answers,
        [
            (json!(1), json!(FRAME_TOO_LARGE)),
            (json!(4), json!(FRAME_TOO_LARGE)),
            (json!(3), Value::Null),
        ]
    );
}

#[tokio::test]
async fn a_line_naming_no_request_id_goes_unanswered() {
    let (frames, _) = serve_lines(
        Counter::with_read_tool(),
        &[
            "not json".into(),
            json!({"jsonrpc": "2.0", "method": READ_METHOD}).to_string(),
            json!({"jsonrpc": "2.0", "id": null, "method": READ_METHOD}).to_string(),
            json!({"jsonrpc": "2.0", "id": 4, "method": READ_METHOD}).to_string(),
        ],
        None,
    )
    .await;
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0]["id"], 4);
}

/// The request has run by the time its answer turns out too large, so it fails rather than
/// being refused.
#[tokio::test]
async fn an_answer_over_the_limit_fails_under_the_request_id() {
    let (_requests, tick_requests) = tokio::io::duplex(8 * 1024);
    let (tick_responses, mut client_responses) = tokio::io::duplex(64 * 1024);
    let mut stream = TickStream::duplex(Counter::with_read_tool(), tick_requests, tick_responses);
    stream.answer(
        json!(7),
        Ok(json!({"text": "x".repeat(MAX_TOOL_RESPONSE_BYTES)})),
    );
    stream.close().await;
    let mut bytes = Vec::new();
    client_responses.read_to_end(&mut bytes).await.unwrap();
    let frame: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(frame["id"], 7);
    assert_eq!(frame["error"]["code"], TOOL_FAILED);
}
