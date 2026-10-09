use std::{collections::BTreeMap, sync::Arc};

use serde_json::{Value, json};
use tickloop_agent_model::{
    ModelError,
    credentials::save_api_key,
    judgment::{Evaluation, EvaluationRequest, JevClient, JevConfig, JudgmentClient, Question},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

fn request() -> EvaluationRequest {
    EvaluationRequest {
        state: json!({"ticket": "Please help"}),
        questions: BTreeMap::from([
            (
                "route".into(),
                Question::Choice {
                    instructions: json!("Which team?"),
                    criteria: BTreeMap::from([
                        ("help".into(), Value::Null),
                        ("sales".into(), Value::Null),
                    ]),
                },
            ),
            (
                "urgency".into(),
                Question::Score {
                    instructions: json!("How urgent?"),
                    criteria: vec![json!("Routine"), json!("Urgent")],
                },
            ),
            (
                "question".into(),
                Question::Noul {
                    instructions: json!("Is help requested?"),
                    criteria: None,
                },
            ),
        ]),
    }
}

fn response() -> Value {
    json!({"model": "jev-1.13.0", "answers": {
        "route": {"type":"choice", "choice":"help", "confidence":0.9, "probabilities":{"help":0.95,"sales":0.05}},
        "urgency": {"type":"score", "score":0.2,"confidence":0.6,"probabilities":{"0":0.8,"1":0.2},"legend":{"0":"Routine","1":"Urgent"}},
        "question": {"type":"noul", "noul":0.99}
    }, "usage":{"input_tokens":100,"output_tokens":20}})
}

async fn server(
    replies: Vec<(u16, String)>,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let capture = requests.clone();
    let task = tokio::spawn(async move {
        for (status, body) in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..offset]);
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= offset + 4 + length {
                        break;
                    }
                }
            }
            capture.lock().await.push(String::from_utf8(bytes).unwrap());
            stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    (url, requests, task)
}

#[tokio::test]
async fn sends_typed_questions_and_reads_rotated_private_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("state");
    save_api_key(&root, "TYPESAFE_API_KEY", "first-test-key").unwrap();
    let (endpoint, requests, task) = server(vec![
        (200, response().to_string()),
        (200, response().to_string()),
    ])
    .await;
    let client = JevClient::new(
        JevConfig {
            endpoint,
            ..Default::default()
        },
        Some(root.clone()),
    )
    .unwrap();
    for key in ["first-test-key", "rotated-test-key"] {
        save_api_key(&root, "TYPESAFE_API_KEY", key).unwrap();
        let result = client
            .evaluate(&request(), CancellationToken::new())
            .await
            .unwrap();
        result.validate(&request()).unwrap();
        assert_eq!(result.usage.input_tokens, Some(100));
        assert_eq!(result.model, "jev-1.13.0");
    }
    task.await.unwrap();
    let requests = requests.lock().await;
    for (raw, key) in requests.iter().zip(["first-test-key", "rotated-test-key"]) {
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(headers.contains(&format!("Bearer {key}")));
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["state"], request().state);
        assert_eq!(body["questions"]["route"]["type"], "choice");
        assert!(body.get("messages").is_none());
        assert!(!body.to_string().contains(key));
    }
}

#[tokio::test]
async fn errors_do_not_echo_provider_body_or_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("state");
    save_api_key(&root, "TYPESAFE_API_KEY", "test-secret").unwrap();
    let (endpoint, _, task) = server(vec![(
        401,
        "test-secret private provider diagnostic".into(),
    )])
    .await;
    let client = JevClient::new(
        JevConfig {
            endpoint,
            ..Default::default()
        },
        Some(root.clone()),
    )
    .unwrap();
    let error = client
        .evaluate(&request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error.error, ModelError::Authentication));
    assert!(!format!("{error:?}").contains("test-secret"));
    task.await.unwrap();
}

#[tokio::test]
async fn cancelled_requests_make_no_connection() {
    let client = JevClient::new(
        JevConfig {
            endpoint: "http://127.0.0.1:1/v1/systemone".into(),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        client.evaluate(&request(), cancel).await,
        Err(tickloop_agent_model::judgment::JudgmentError {
            error: ModelError::Cancelled,
            ..
        })
    ));
}

#[test]
fn accepts_only_normalized_or_feasibly_rounded_choice_probabilities() {
    let mut question = request();
    let Question::Choice { criteria, .. } = question.questions.get_mut("route").unwrap() else {
        panic!()
    };
    criteria.insert("review".into(), Value::Null);
    criteria.insert("other".into(), Value::Null);
    for (probabilities, accepted) in [
        (
            json!({"help":0.93,"sales":0.04,"review":0.01,"other":0.01}),
            true,
        ),
        (
            json!({"help":0.93,"sales":0.04,"review":0.02,"other":0.02}),
            true,
        ),
        (
            json!({"help":0.93,"sales":0.01,"review":0.01,"other":0.01}),
            false,
        ),
        (
            json!({"help":0.93,"sales":0.05,"review":0.05,"other":0.05}),
            false,
        ),
        (
            json!({"help":0.931,"sales":0.04,"review":0.01,"other":0.01}),
            false,
        ),
        (
            json!({"help":0.0,"sales":0.0,"review":0.0,"other":0.0}),
            false,
        ),
    ] {
        let mut value = response();
        value["answers"]["route"]["probabilities"] = probabilities.clone();
        let evaluation: Evaluation = serde_json::from_value(value).unwrap();
        assert_eq!(
            evaluation.validate(&question).is_ok(),
            accepted,
            "{probabilities}"
        );
        assert_eq!(
            serde_json::to_value(&evaluation).unwrap()["answers"]["route"]["probabilities"],
            probabilities
        );
    }
}

#[test]
fn rejects_mismatched_missing_and_invalid_answers() {
    let valid: Evaluation = serde_json::from_value(response()).unwrap();
    valid.validate(&request()).unwrap();
    let mut variants = Vec::new();
    let mut value = response();
    value["answers"].as_object_mut().unwrap().remove("question");
    variants.push(value);
    let mut value = response();
    value["answers"]["route"]["choice"] = json!("sales");
    variants.push(value);
    let mut value = response();
    value["answers"]["route"]["probabilities"]["invented"] = json!(0.0);
    variants.push(value);
    let mut value = response();
    value["answers"]["route"]["probabilities"]["help"] = json!(1.1);
    variants.push(value);
    let mut value = response();
    value["answers"]["route"]["confidence"] = json!(-0.1);
    variants.push(value);
    let mut value = response();
    value["answers"]["urgency"]["score"] = json!(1.0);
    variants.push(value);
    let mut value = response();
    value["answers"]["question"]["noul"] = json!(1.5);
    variants.push(value);
    for value in variants {
        assert!(
            serde_json::from_value::<Evaluation>(value)
                .unwrap()
                .validate(&request())
                .is_err()
        );
    }
}

#[test]
fn rejects_unsafe_endpoints_and_invalid_questions() {
    for endpoint in [
        "http://example.com/v1/systemone",
        "https://user:secret@example.com",
        "https://example.com/?token=secret",
        "file:///tmp/data",
    ] {
        assert!(
            JevClient::new(
                JevConfig {
                    endpoint: endpoint.into(),
                    ..Default::default()
                },
                None
            )
            .is_err()
        );
    }
    let mut request = request();
    request.questions.clear();
    assert!(request.validate().is_err());
    assert!(serde_json::from_value::<JevConfig>(json!({"api_key":"never-in-settings"})).is_err());
}

#[tokio::test]
#[ignore = "requires an explicitly supplied Jev test credential directory"]
async fn live_mixed_typed_questions() {
    let root = std::env::var_os("TICKLOOP_JEV_TEST_ROOT").expect("set TICKLOOP_JEV_TEST_ROOT");
    let client = JevClient::new(JevConfig::default(), Some(root.into())).unwrap();
    let result = client
        .evaluate(&request(), CancellationToken::new())
        .await
        .unwrap();
    result.validate(&request()).unwrap();
    assert_eq!(result.answers.len(), 3);
    println!(
        "Mixed Choice/Score/Noul: model={}, usage={:?}",
        result.model, result.usage
    );
}

#[tokio::test]
async fn server_context_limit_preserves_large_request_and_redacts_error_body() {
    for (status, body, context_limit) in [
        (
            400,
            json!({"detail":{"error_type":"max_tokens_exceeded"},"padding":"x".repeat(20 * 1024)}),
            false,
        ),
        (
            413,
            json!({"detail":"capacity-test-secret private diagnostic"}),
            true,
        ),
        (
            400,
            json!({"detail":{"error_type":"max_tokens_exceeded"}, "private":"capacity-test-secret"}),
            true,
        ),
        (
            400,
            json!({"detail":{"error_type":"invalid_question"}, "message":"max_tokens_exceeded capacity-test-secret"}),
            false,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("state");
        save_api_key(&root, "TYPESAFE_API_KEY", "capacity-test-secret").unwrap();
        let (endpoint, captured, task) = server(vec![(status, body.to_string())]).await;
        let client = JevClient::new(
            JevConfig {
                endpoint,
                ..Default::default()
            },
            Some(root),
        )
        .unwrap();
        let mut proposed = request();
        proposed.state = json!({"policy": "Complete policy text. ".repeat(4_000)});
        let error = client
            .evaluate(&proposed, CancellationToken::new())
            .await
            .unwrap_err();
        if context_limit {
            assert!(matches!(
                error.error,
                ModelError::ResponseRejected(
                    tickloop_agent_model::ModelResponseRejection::ContextLimit
                )
            ));
        } else {
            assert!(matches!(
                error.error,
                ModelError::HttpResponse { status: 400, .. }
            ));
        }
        assert!(!format!("{error:?}").contains("capacity-test-secret"));
        task.await.unwrap();
        let captured = captured.lock().await;
        assert_eq!(captured.len(), 1);
        let body: Value =
            serde_json::from_str(captured[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["state"], proposed.state);
    }
}

#[tokio::test]
async fn waiting_for_a_context_error_body_remains_cancellable() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("state");
    save_api_key(&root, "TYPESAFE_API_KEY", "cancel-test-secret").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
    let (started, ready) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = [0; 4096];
        assert!(stream.read(&mut received).await.unwrap() > 0);
        stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 128\r\n\r\n{\"detail\":").await.unwrap();
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let client = JevClient::new(
        JevConfig {
            endpoint,
            ..Default::default()
        },
        Some(root),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let request_cancel = cancel.clone();
    let mut evaluation =
        tokio::spawn(async move { client.evaluate(&request(), request_cancel).await });
    ready.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut evaluation)
            .await
            .is_err()
    );
    cancel.cancel();
    let error = tokio::time::timeout(std::time::Duration::from_secs(2), evaluation)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error.error, ModelError::Cancelled));
    server.abort();
}
