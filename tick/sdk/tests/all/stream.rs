use serde_json::{Value, json};
use tick_protocol::{
    DESCRIBE_METHOD, INVALID_REQUEST, NATIVE_PROTOCOL_VERSION, READ_METHOD, TickDescriptor,
    TickTool, UPDATE_METHOD,
};
use tick_sdk::stream::{Describe, InboundFrame, TickStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const INSTANCE: &str = "hosted:counter-1";

/// A third-party Tick that knows nothing of the financial Ticks.
struct Counter {
    tools: Vec<TickTool>,
}

impl Counter {
    fn with_read_tool() -> Self {
        Self {
            tools: vec![TickTool {
                name: "current".into(),
                description: "Read the value".into(),
                params: json!({"type": "object"}),
                mutates: false,
            }],
        }
    }
}

impl Describe for Counter {
    fn tick_type(&self) -> &str {
        "example.counter"
    }

    fn descriptor(&self) -> Result<TickDescriptor, String> {
        Ok(TickDescriptor {
            protocol: NATIVE_PROTOCOL_VERSION,
            tick_type: self.tick_type().into(),
            title: "Counter".into(),
            description: "Counts".into(),
            recover_with: Some("current".into()),
            tools: self.tools.clone(),
        })
    }
}

/// Feeds `requests` to a stream for `tick`, publishes `published`, and returns every frame the
/// stream wrote back.
async fn serve(tick: Counter, requests: &[Value], published: Option<Value>) -> Vec<Value> {
    let (mut client_requests, tick_requests) = tokio::io::duplex(8 * 1024);
    let (tick_responses, mut client_responses) = tokio::io::duplex(64 * 1024);
    let mut stream = TickStream::duplex(
        tick,
        "example",
        INSTANCE.to_owned(),
        tick_requests,
        tick_responses,
    );
    if let Some(data) = published {
        stream.publish(data).unwrap();
    }
    for request in requests {
        let mut line = serde_json::to_vec(request).unwrap();
        line.push(b'\n');
        client_requests.write_all(&line).await.unwrap();
    }
    client_requests.shutdown().await.unwrap();
    let mut inbound = stream.take_requests();
    while let Some(frame) = inbound.next().await {
        assert!(matches!(&frame, InboundFrame::Line(_)));
        assert!(stream.dispatch(frame).is_none(), "no tool call was sent");
    }
    stream.close().await;
    let mut bytes = Vec::new();
    client_responses.read_to_end(&mut bytes).await.unwrap();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn any_tick_answers_describe_and_read_with_what_it_declares_and_publishes() {
    let frames = serve(
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
    assert_eq!(update["params"]["tick"], "example.counter");
    assert_eq!(update["params"]["source"], "example");
    assert_eq!(update["params"]["tick_instance_id"], INSTANCE);
    let answer = |id: u64| frames.iter().find(|frame| frame["id"] == id).unwrap();
    assert_eq!(answer(1)["result"]["tick_type"], "example.counter");
    assert_eq!(answer(1)["result"]["recover_with"], "current");
    assert_eq!(answer(2)["result"]["data"], json!({"value": 3}));
}

#[tokio::test]
async fn a_descriptor_that_breaks_the_standard_is_refused_instead_of_answered() {
    let undeclared_recovery = Counter { tools: Vec::new() };
    let frames = serve(
        undeclared_recovery,
        &[json!({"jsonrpc": "2.0", "id": 1, "method": DESCRIBE_METHOD})],
        None,
    )
    .await;
    assert_eq!(frames[0]["error"]["code"], INVALID_REQUEST);
}
