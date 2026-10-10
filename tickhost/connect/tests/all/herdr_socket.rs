use std::path::Path;

use serde_json::{Value, json};
use tickhost_connect::herdr::{HOST_TIMEOUT, HerdrContext, HerdrPort, SocketHerdr};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;

fn adapter(socket: &Path) -> SocketHerdr {
    SocketHerdr::new(HerdrContext {
        socket: socket.to_owned(),
        caller_pane: "test-pane".into(),
    })
}

#[tokio::test]
async fn missing_host_socket_is_a_diagnostic_before_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let error = adapter(&directory.path().join("missing.sock"))
        .request("pane.current", json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "HostUnavailable");
    assert!(!error.dispatched);
}

async fn malformed_reply(reply: impl FnOnce(Value) -> Vec<u8> + Send + 'static) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: Value = serde_json::from_str(&request).unwrap();
        reader.get_mut().write_all(&reply(request)).await.unwrap();
    });
    let error = adapter(&socket)
        .request("pane.current", json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "ProtocolViolation");
    assert!(error.dispatched);
    server.await.unwrap();
}

#[tokio::test]
async fn disconnected_invalid_incomplete_and_wrong_identity_replies_are_protocol_violations() {
    malformed_reply(|_| Vec::new()).await;
    malformed_reply(|_| b"not JSON\n".to_vec()).await;
    malformed_reply(|request| {
        format!("{}\n", json!({"id":request["id"],"wrong":"shape"})).into_bytes()
    })
    .await;
    malformed_reply(|_| b"{\"id\":\"another\",\"result\":{}}\n".to_vec()).await;
    malformed_reply(|request| format!("{}", json!({"id":request["id"],"result":{}})).into_bytes())
        .await;
}

#[tokio::test(start_paused = true)]
async fn response_timeout_after_host_receives_request_has_unknown_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (received, request_received) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        received.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let client = tokio::spawn(async move {
        adapter(&socket)
            .request(
                "pane.close",
                json!({"pane_id":"once"}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err()
    });
    request_received.await.unwrap();
    tokio::time::advance(HOST_TIMEOUT).await;
    let error = client.await.unwrap();
    assert_eq!(error.code, "OutcomeUnknown");
    assert!(error.dispatched);
    server.abort();
}
