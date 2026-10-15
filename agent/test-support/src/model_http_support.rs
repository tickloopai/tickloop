#![allow(dead_code)]

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

pub fn raw_reasoning_prefix_reply(
    added_prefix: bool,
    completed_text: &str,
    rotating_ids: bool,
) -> String {
    let mut frames = Vec::new();
    let mut sequence = 0;
    let mut event = |kind: &str, mut value: Value| {
        value["type"] = kind.into();
        value["sequence_number"] = sequence.into();
        sequence += 1;
        frames.push(format!("event: {kind}\ndata: {value}\n\n"));
    };
    let id = |kind: &str, phase: &str| {
        if rotating_ids {
            format!("{kind}-{phase}")
        } else {
            kind.to_owned()
        }
    };
    let reasoning = |phase: &str, text: Option<&str>| serde_json::json!({"type":"reasoning","id":id("rs-prefix",phase),"summary":[],"content":text.map(|text|vec![serde_json::json!({"type":"reasoning_text","text":text})]).unwrap_or_default(),"encrypted_content":if rotating_ids {format!("opaque-{phase}")} else {"opaque-fixture".into()}});
    let message = |phase: &str| serde_json::json!({"type":"message","id":id("msg-prefix",phase),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"Answer","annotations":[]}]});
    event(
        "response.created",
        serde_json::json!({"response":{"id":id("response-prefix","created"),"status":"in_progress"}}),
    );
    event(
        "response.output_item.added",
        serde_json::json!({"output_index":0,"item":reasoning("added",added_prefix.then_some("Think"))}),
    );
    if !added_prefix {
        event(
            "response.reasoning_text.delta",
            serde_json::json!({"output_index":0,"item_id":id("rs-prefix","delta"),"content_index":0,"delta":"Think"}),
        );
    }
    event(
        "response.output_item.done",
        serde_json::json!({"output_index":0,"item":reasoning("done",Some(completed_text))}),
    );
    let mut initial_message = message("added");
    initial_message["content"] = serde_json::json!([]);
    event(
        "response.output_item.added",
        serde_json::json!({"output_index":1,"item":initial_message}),
    );
    event(
        "response.output_text.delta",
        serde_json::json!({"output_index":1,"item_id":id("msg-prefix","delta"),"content_index":0,"delta":"Answer"}),
    );
    event(
        "response.output_item.done",
        serde_json::json!({"output_index":1,"item":message("done")}),
    );
    event(
        "response.completed",
        serde_json::json!({"response":{"id":id("response-prefix","completed"),"status":"completed","output":[reasoning("completed",Some(completed_text)),message("completed")],"usage":{"input_tokens":20,"output_tokens":10}}}),
    );
    frames.concat()
}

pub async fn serve_response(
    status: u16,
    body: String,
) -> (String, oneshot::Receiver<(String, Value)>) {
    serve_response_with_headers(status, body, &[]).await
}

pub async fn serve_response_with_headers(
    status: u16,
    body: String,
    response_headers: &[(&str, &str)],
) -> (String, oneshot::Receiver<(String, Value)>) {
    let response_headers = header_lines(response_headers);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, request) = accept_script_request(&listener).await;
        let _ = sender.send(request);
        write_response(&mut socket, status, &response_headers, &body).await;
    });
    (format!("http://{address}/anthropic"), receiver)
}

/// Answers one request per connection with the given responses in order.
pub async fn serve_responses(
    responses: Vec<(u16, String)>,
) -> (String, mpsc::UnboundedReceiver<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for (status, body) in responses {
            let (mut socket, request) = accept_script_request(&listener).await;
            let _ = sender.send(request);
            write_response(&mut socket, status, "", &body).await;
        }
    });
    (format!("http://{address}/anthropic"), receiver)
}

pub async fn serve_responses_with_gated_response(
    responses: Vec<(u16, String)>,
    gated_response_index: usize,
) -> (
    String,
    mpsc::UnboundedReceiver<(String, Value)>,
    oneshot::Sender<()>,
) {
    let (url, requests, mut releases) =
        serve_responses_with_gated_responses(responses, &[gated_response_index]).await;
    (url, requests, releases.remove(0))
}

pub async fn serve_responses_with_gated_responses(
    responses: Vec<(u16, String)>,
    gated_response_indices: &[usize],
) -> (
    String,
    mpsc::UnboundedReceiver<(String, Value)>,
    Vec<oneshot::Sender<()>>,
) {
    let mut gates = std::collections::BTreeMap::new();
    let mut releases = Vec::new();
    for &index in gated_response_indices {
        assert!(index < responses.len());
        let (release, gate) = oneshot::channel();
        assert!(gates.insert(index, gate).is_none());
        releases.push(release);
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for (index, (status, body)) in responses.into_iter().enumerate() {
            let (mut socket, request) = accept_script_request(&listener).await;
            let _ = sender.send(request);
            if let Some(gate) = gates.remove(&index) {
                tokio::spawn(async move {
                    if gate.await.is_ok() {
                        write_response(&mut socket, status, "", &body).await;
                    }
                });
            } else {
                write_response(&mut socket, status, "", &body).await;
            }
        }
    });
    (format!("http://{address}/anthropic"), receiver, releases)
}

pub async fn serve_router(
    mut reply: impl FnMut(&Value) -> String + Send + 'static,
) -> (String, mpsc::UnboundedReceiver<(String, Value)>) {
    serve_router_with_status(move |request| (200, reply(request))).await
}

/// Routes each request to the status and body `reply` chooses for it.
pub async fn serve_router_with_status(
    mut reply: impl FnMut(&Value) -> (u16, String) + Send + 'static,
) -> (String, mpsc::UnboundedReceiver<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (mut socket, request) = accept_script_request(&listener).await;
            let (status, response) = reply(&request.1);
            if sender.send(request).is_err() {
                break;
            }
            write_response(&mut socket, status, "", &response).await;
        }
    });
    (format!("http://{address}/anthropic"), receiver)
}

fn header_lines(headers: &[(&str, &str)]) -> String {
    headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect()
}

pub async fn accept_script_request(listener: &TcpListener) -> (TcpStream, (String, Value)) {
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        if request
            .1
            .get("tools")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
            && request
                .1
                .to_string()
                .contains("Generate a short sentence-case Chat title")
        {
            let title = request
                .1
                .get("messages")
                .or_else(|| request.1.get("input"))
                .and_then(Value::as_array)
                .and_then(|messages| messages.last())
                .and_then(|message| message.get("content"))
                .map(|content| {
                    content.as_str().map(str::to_owned).unwrap_or_else(|| {
                        content
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|block| block.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                })
                .unwrap_or_else(|| "Fixture session".into());
            write_response(&mut socket, 200, "", &title_stream(&request.1, &title)).await;
        } else {
            return (socket, request);
        }
    }
}

fn title_stream(request: &Value, title: &str) -> String {
    let frame = |event: &str, data: Value| format!("event: {event}\ndata: {data}\n\n");
    if request.get("input").is_some() {
        let message = serde_json::json!({"id":"title-message","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":title,"annotations":[]}]});
        frame(
            "response.completed",
            serde_json::json!({"type":"response.completed","response":{"id":"title-response","status":"completed","output":[message],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}),
        )
    } else if request.get("max_tokens").is_some() {
        [frame("message_start", serde_json::json!({"type":"message_start","message":{"id":"title","type":"message","role":"assistant","model":request["model"],"content":[],"usage":{"input_tokens":0,"output_tokens":0}}})),
         frame("content_block_start", serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
         frame("content_block_delta", serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":title}})),
         frame("content_block_stop", serde_json::json!({"type":"content_block_stop","index":0})),
         frame("message_delta", serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":0}})),
         frame("message_stop", serde_json::json!({"type":"message_stop"}))].concat()
    } else {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"id":"title","choices":[{"index":0,"delta":{"role":"assistant","content":title},"finish_reason":"stop"}],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}})
        )
    }
}

pub async fn read_request(socket: &mut TcpStream) -> (String, Value) {
    let mut received = Vec::new();
    let mut buffer = [0; 4096];
    let (headers, start, length) = loop {
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        received.extend_from_slice(&buffer[..count]);
        if let Some(end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = String::from_utf8(received[..end].to_vec()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (headers, end + 4, length);
        }
    };
    while received.len() < start + length {
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        received.extend_from_slice(&buffer[..count]);
    }
    let request = serde_json::from_slice(&received[start..start + length]).unwrap();
    (headers, request)
}

async fn write_response(socket: &mut TcpStream, status: u16, response_headers: &str, body: &str) {
    socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n{response_headers}\r\n",body.len()).as_bytes()).await.unwrap();
    for chunk in body.as_bytes().chunks(7) {
        if socket.write_all(chunk).await.is_err() {
            return;
        }
        tokio::task::yield_now().await;
    }
}
