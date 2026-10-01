#![allow(dead_code)]

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

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
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = sender.send(read_request(&mut socket).await);
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
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = sender.send(read_request(&mut socket).await);
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
    assert!(gated_response_index < responses.len());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::unbounded_channel();
    let (release, gate) = oneshot::channel();
    tokio::spawn(async move {
        let mut gate = Some(gate);
        for (index, (status, body)) in responses.into_iter().enumerate() {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = sender.send(read_request(&mut socket).await);
            if index == gated_response_index {
                let gate = gate.take().unwrap();
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
    (format!("http://{address}/anthropic"), receiver, release)
}

fn header_lines(headers: &[(&str, &str)]) -> String {
    headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect()
}

async fn read_request(socket: &mut TcpStream) -> (String, Value) {
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
