use crate::runtime::support::provider_transport;
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_code_settings::local_agent::opened_agent;
use tickloop_harness::test_support::model_http_support;

use tickloop_harness::model::{LocalModelClients, ModelClients, ModelRequest};
use tickloop_harness::protocol::{EventKind, Protocol};
use tickloop_harness::runtime::Agent;
use tickloop_harness::test_support::harness::ChosenModels;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
fn request() -> ModelRequest {
    ModelRequest {
        deferred_tools: Vec::new(),
        history_sources: vec![],
        messages: vec![],
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}
fn decoded(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .chars()
        .filter(|c| {
            !matches!(
                *c,
                '\u{2060}' | '\u{feff}' | '\u{034f}' | '\u{061c}' | '\u{00ad}' | '\u{180e}'
            )
        })
        .collect()
}
#[tokio::test]
async fn confirmation_secret_surfaces() {
    let mut leaked = Vec::new();
    for (label, message, secret, sentinel) in [
        (
            "original-newline",
            "Rejected opaque-\nfixture-credential",
            "opaque-fixture-credential",
            "opaque-fixture-credential",
        ),
        (
            "original-zero-width",
            "Rejected opaque-\u{200b}fixture-credential",
            "opaque-fixture-credential",
            "opaque-fixture-credential",
        ),
        (
            "original-url",
            "https://support.example/opaque%2Dfixture%2Dcredential",
            "opaque-fixture-credential",
            "opaque-fixture-credential",
        ),
        (
            "new-zero-width-key",
            "Rejected opaque-\u{2060}fixture-credential",
            "opaque-fixture-credential",
            "opaque-fixture-credential",
        ),
        (
            "new-zero-width-bearer",
            "Rejected bE\u{2060}aReR pattern-only-token",
            "opaque-fixture-credential",
            "pattern-only-token",
        ),
        (
            "upper-url-path",
            "HTTPS://support.example/opaque%2Dfixture%2Dcredential",
            "opaque-fixture-credential",
            "opaque-fixture-credential",
        ),
        (
            "upper-url-fragment",
            "HTTPS://support.example/help#opaque%2Dfixture%2Dcredential",
            "opaque-fixture-credential",
            "opaque-fixture-credential",
        ),
        (
            "upper-url-userinfo",
            "HTTPS://opaque%2Dfixture%2Dcredential:private-password@support.example/help",
            "opaque-fixture-credential",
            "opaque-fixture-credential",
        ),
        (
            "lower-url-fragment",
            "https://support.example/help#opaque%2Dfixture%2Dcredential",
            "opaque-fixture-credential",
            "opaque-fixture-credential",
        ),
        (
            "lower-url-userinfo",
            "https://opaque%2Dfixture%2Dcredential:private-password@support.example/help",
            "opaque-fixture-credential",
            "opaque-fixture-credential",
        ),
        (
            "encoded-query-key",
            "https://support.example/help?opaque%2Bfixture%2Fcredential=quota",
            "opaque+fixture/credential",
            "opaque+fixture/credential",
        ),
        (
            "spaced-query-key",
            "https://support.example/help?api%20key=pattern-only-token&tab=quota",
            "opaque-fixture-credential",
            "pattern-only-token",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        tickloop_harness::model::credentials::save_api_key(&root, "PE_CONFIRM_FIXTURE_KEY", secret)
            .unwrap();
        let (url, _) = model_http_support::serve_response(
            401,
            serde_json::json!({"error":{"message":message}}).to_string(),
        )
        .await;
        let mut config = tickloop_harness::test_support::config();
        config.protocol = Protocol::ChatCompletions;
        config.base_url = url;
        config.api_key_env = "PE_CONFIRM_FIXTURE_KEY".into();
        config.provider_id = "fixture-api".into();
        let client = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        let display = error.to_string();
        let debug = format!("{error:?}");
        let details = error.provider_failure().unwrap().message.clone();
        let (url, _) = model_http_support::serve_response(
            401,
            serde_json::json!({"error":{"message":message}}).to_string(),
        )
        .await;
        config.base_url = url;
        let client = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap();
        let agent = Agent::open_embedded(
            root.clone(),
            config,
            provider_transport::without_title_requests(client),
        )
        .unwrap();
        let chat = agent.create_chat(root.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "fake-secret probe".into())
            .await
            .unwrap();
        let failed = loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Failed(text) => break text,
                EventKind::ModelRetry { .. } => panic!("auth must not retry"),
                _ => {}
            }
        };
        let snapshot = serde_json::to_string(&agent.snapshot(chat).await.unwrap()).unwrap();
        agent.shutdown().await;
        let transcript = std::fs::read_to_string(tickloop_harness::test_support::transcript_file(
            &root,
            chat.uuid(),
        ))
        .unwrap();
        let record = serde_json::Value::Array(
            transcript
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect(),
        );
        let mut strings = Vec::new();
        fn visit(v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::String(s) => out.push(s.clone()),
                serde_json::Value::Array(a) => {
                    for v in a {
                        visit(v, out)
                    }
                }
                serde_json::Value::Object(o) => {
                    for v in o.values() {
                        visit(v, out)
                    }
                }
                _ => {}
            }
        }
        visit(&record, &mut strings);
        let surfaces = [
            ("Display", display),
            ("Debug", debug),
            ("Details-source", details),
            ("Failed", failed),
            ("Snapshot", snapshot),
            ("Transcript", strings.join("\n")),
        ];
        let exposed = surfaces
            .iter()
            .filter_map(|(name, text)| decoded(text).contains(sentinel).then_some(*name))
            .collect::<Vec<_>>();
        eprintln!("SURFACES {label}: {exposed:?}");
        if !exposed.is_empty() {
            leaked.push(label);
        }
    }
    assert!(leaked.is_empty(), "credential leak cases: {leaked:?}");
}
#[tokio::test]
async fn confirmation_shared_http_incomplete_quota() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let mut failures = Vec::new();
    for mode in ["truncated", "over-bound"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let body = if mode == "truncated" {
            r#"{"error":{"message":"hidden account limit","code":"insufficient_quota"}}"#
                .to_string()
        } else {
            format!(
                r#"{{"error":{{"message":"{}","code":"insufficient_quota"}}}}"#,
                "x".repeat(16400)
            )
        };
        let send = if mode == "truncated" {
            "{".to_string()
        } else {
            body.clone()
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 32768];
            let _ = stream.read(&mut bytes).await;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 429 Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let _ = stream.write_all(send.as_bytes()).await;
        });
        let mut config = tickloop_harness::test_support::config();
        config.base_url = url;
        config.api_key_env = String::new();
        let client = tickloop_harness::model::LocalModelClients::new(None)
            .client(&config)
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        server.await.unwrap();
        eprintln!(
            "SHARED mode={mode}, class={:?}, retryable={}",
            error.provider_failure().unwrap().class,
            error.retry_hint().0
        );
        if error.retry_hint().0 {
            failures.push(mode);
        }
    }
    assert!(
        failures.is_empty(),
        "incomplete/over-bound quota body retries: {failures:?}"
    );
}

#[tokio::test]
async fn confirmation_shared_http_runtime_blind_retry() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0; 32768];
        let _ = stream.read(&mut buffer).await;
        let declared = r#"{"error":{"message":"account limit","code":"insufficient_quota"}}"#.len();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 429 Test\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n{{"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(stream);
        if let Ok(Ok((mut retry, _))) =
            tokio::time::timeout(std::time::Duration::from_secs(10), listener.accept()).await
        {
            let _ = retry.read(&mut buffer).await;
            let body = r#"{"error":{"code":"insufficient_quota","message":"quota terminal"}}"#;
            retry.write_all(format!("HTTP/1.1 429 Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            true
        } else {
            false
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = tickloop_harness::test_support::config();
    config.protocol = Protocol::ChatCompletions;
    config.base_url = url;
    config.provider_id = "fixture-api".into();
    let client = LocalModelClients::new(None).client(&config).unwrap();
    let agent = Agent::open_embedded(
        directory.path().into(),
        config,
        provider_transport::without_title_requests(client),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "quota retry probe".into())
        .await
        .unwrap();
    let mut retried = false;
    loop {
        match tickloop_harness::test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::ModelRetry { .. } => {
                retried = true;
                eprintln!("RUNTIME ModelRetry observed");
            }
            EventKind::Failed(text) => {
                eprintln!("RUNTIME terminal={text}");
                break;
            }
            _ => {}
        }
    }
    agent.shutdown().await;
    let resent = server.await.unwrap();
    eprintln!("RUNTIME retry_event={retried}, second_request={resent}");
    assert!(
        !retried && !resent,
        "incomplete quota rejection must not be blindly retried"
    );
}

#[tokio::test]
async fn confirmation_bounded_html_credential_prefix() {
    for prefix_length in [16360, 16365, 16370, 16375, 16380, 16384] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let secret = "opaque-fixture-credential";
        tickloop_harness::model::credentials::save_api_key(&root, "PE_CONFIRM_FIXTURE_KEY", secret)
            .unwrap();
        let body = format!(
            "<html><!--{}-->Rejected {secret}</html>",
            "x".repeat(prefix_length - 22)
        );
        let (url, _) = model_http_support::serve_response(401, body.clone()).await;
        let mut config = tickloop_harness::test_support::config();
        config.protocol = Protocol::ChatCompletions;
        config.base_url = url;
        config.api_key_env = "PE_CONFIRM_FIXTURE_KEY".into();
        config.provider_id = "fixture-api".into();
        let client = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        let (url, _) = model_http_support::serve_response(401, body).await;
        config.base_url = url;
        let client = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap();
        let agent = Agent::open_embedded(
            root.clone(),
            config,
            provider_transport::without_title_requests(client),
        )
        .unwrap();
        let chat = agent.create_chat(root.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "bound fake-key probe".into())
            .await
            .unwrap();
        let failed = loop {
            if let EventKind::Failed(text) = tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                break text;
            }
        };
        agent.shutdown().await;
        let transcript = std::fs::read_to_string(tickloop_harness::test_support::transcript_file(
            &root,
            chat.uuid(),
        ))
        .unwrap();
        let surfaces = [
            ("Display", error.to_string()),
            ("Debug", format!("{error:?}")),
            (
                "Details-source",
                error.provider_failure().unwrap().message.clone(),
            ),
            ("Failed", failed),
            ("Transcript", transcript),
        ];
        let exposed = surfaces
            .iter()
            .filter_map(|(name, text)| text.contains("opaque-fixture-credentia").then_some(*name))
            .collect::<Vec<_>>();
        eprintln!(
            "BOUND_HTTP surfaces={exposed:?}, message={}",
            error.provider_failure().unwrap().message
        );
        assert!(
            exposed.is_empty(),
            "body bound must not leak a nearly complete credential"
        );
    }
}

#[tokio::test]
async fn all_64_confirmation_probe_variants_are_safe_on_all_surfaces() {
    let secret = "opaque-fixture-credential";
    let mut cases = Vec::new();
    for ch in [
        '\n', '\u{200b}', '\u{200c}', '\u{200d}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}',
        '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        '\u{2060}', '\u{feff}', '\u{034f}', '\u{061c}', '\u{00ad}', '\u{180e}',
    ] {
        cases.push((
            format!("known-split-U+{:04X}", ch as u32),
            format!("Rejected opaque-{ch}fixture-credential"),
            secret.to_owned(),
        ));
        cases.push((
            format!("bearer-split-U+{:04X}", ch as u32),
            format!("Rejected bE{ch}aReR synthetic-private-value"),
            secret.to_owned(),
        ));
    }
    for scheme in ["https", "HTTPS", "HtTpS"] {
        for tail in [
            "/opaque%2Dfixture%2Dcredential",
            "/help#opaque%2Dfixture%2Dcredential",
            "/help?q=opaque%2Dfixture%2Dcredential",
        ] {
            cases.push((
                format!("url-{scheme}-{tail}"),
                format!("See {scheme}://support.example{tail}"),
                secret.to_owned(),
            ));
        }
        cases.push((
            format!("userinfo-{scheme}"),
            format!(
                "See {scheme}://opaque%2Dfixture%2Dcredential:private-password@support.example/help"
            ),
            secret.to_owned(),
        ));
    }
    cases.push((
        "sensitive-spaced-query-key".into(),
        "https://support.example/help?api%20key=synthetic-private-value&tab=quota".into(),
        secret.to_owned(),
    ));
    cases.push((
        "encoded-query-key".into(),
        "https://support.example/help?opaque%2Bfixture%2Fcredential=quota".into(),
        "opaque+fixture/credential".into(),
    ));
    for n in [980, 1000, 1020, 1023, 1024, 16000, 16384, 20000] {
        cases.push((format!("long-{n}"),format!("{} opaque-\u{200b}fixture-credential https://support.example/opaque%2Dfixture%2Dcredential", "x".repeat(n)),secret.to_owned()));
    }

    assert_eq!(cases.len(), 64);
    let mut failures = Vec::new();
    for (label, message, key) in cases {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        tickloop_harness::model::credentials::save_api_key(&root, "PE_CONFIRM_FIXTURE_KEY", &key)
            .unwrap();
        let (url, _) = model_http_support::serve_response(
            429,
            serde_json::json!({"error":{"message":message,"code":"insufficient_quota"}})
                .to_string(),
        )
        .await;
        let mut config = tickloop_harness::test_support::config();
        config.protocol = Protocol::ChatCompletions;
        config.base_url = url;
        config.api_key_env = "PE_CONFIRM_FIXTURE_KEY".into();
        let client = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap();
        let agent = Agent::open_embedded(
            root.clone(),
            config,
            provider_transport::without_title_requests(client),
        )
        .unwrap();
        let chat = agent.create_chat(root.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "64-case security fixture".into())
            .await
            .unwrap();
        let failed = loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Failed(text) => break text,
                EventKind::ModelRetry { .. } => {
                    agent.interrupt(chat).await.unwrap();
                    break "unexpected retry".into();
                }
                _ => {}
            }
        };
        let snapshot = serde_json::to_value(agent.snapshot(chat).await.unwrap()).unwrap();
        agent.shutdown().await;
        let transcript = std::fs::read_to_string(tickloop_harness::test_support::transcript_file(
            &root,
            chat.uuid(),
        ))
        .unwrap();
        let surface = decoded(&format!("{failed}\n{snapshot}\n{transcript}"));
        if surface.contains(&key)
            || surface.contains("synthetic-private-value")
            || surface.contains("private-password")
            || failed == "unexpected retry"
        {
            failures.push(label);
        }
    }
    assert!(failures.is_empty(), "64-case failures: {failures:?}");
}

async fn assert_safe_provider_surfaces(cases: Vec<(String, String, String)>) {
    let mut leaked = Vec::new();
    for (label, message, secret) in cases {
        let sentinel = secret.as_str();
        let secret = secret.as_str();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        tickloop_harness::model::credentials::save_api_key(&root, "PE_CONFIRM_FIXTURE_KEY", secret)
            .unwrap();
        let (url, _) = model_http_support::serve_response(
            401,
            serde_json::json!({"error":{"message":message}}).to_string(),
        )
        .await;
        let mut config = tickloop_harness::test_support::config();
        config.protocol = if secret.is_ascii() {
            Protocol::ChatCompletions
        } else {
            Protocol::Messages
        };
        config.base_url = url;
        config.api_key_env = "PE_CONFIRM_FIXTURE_KEY".into();
        config.provider_id = "fixture-api".into();
        let client = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        let display = error.to_string();
        let debug = format!("{error:?}");
        let details = error.provider_failure().unwrap().message.clone();
        let (url, _) = model_http_support::serve_response(
            401,
            serde_json::json!({"error":{"message":message}}).to_string(),
        )
        .await;
        config.base_url = url;
        let client = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap();
        let agent = Agent::open_embedded(
            root.clone(),
            config,
            provider_transport::without_title_requests(client),
        )
        .unwrap();
        let chat = agent.create_chat(root.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "fake-secret probe".into())
            .await
            .unwrap();
        let failed = loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Failed(text) => break text,
                EventKind::ModelRetry { .. } => panic!("auth must not retry"),
                _ => {}
            }
        };
        let snapshot = serde_json::to_string(&agent.snapshot(chat).await.unwrap()).unwrap();
        agent.shutdown().await;
        let transcript = std::fs::read_to_string(tickloop_harness::test_support::transcript_file(
            &root,
            chat.uuid(),
        ))
        .unwrap();
        let record = serde_json::Value::Array(
            transcript
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect(),
        );
        let mut strings = Vec::new();
        fn visit(v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::String(s) => out.push(s.clone()),
                serde_json::Value::Array(a) => {
                    for v in a {
                        visit(v, out)
                    }
                }
                serde_json::Value::Object(o) => {
                    for v in o.values() {
                        visit(v, out)
                    }
                }
                _ => {}
            }
        }
        visit(&record, &mut strings);
        let surfaces = [
            ("Display", display),
            ("Debug", debug),
            ("Details-source", details),
            ("Failed", failed),
            ("Snapshot", snapshot),
            ("Transcript", strings.join("\n")),
        ];
        let key_skeleton = security_skeleton(sentinel);
        let characters = key_skeleton.chars().collect::<Vec<_>>();
        let fragments = characters
            .windows(8)
            .map(|fragment| fragment.iter().collect::<String>())
            .collect::<Vec<_>>();
        let exposed = surfaces
            .iter()
            .filter_map(|(name, text)| {
                let skeleton = security_skeleton(text);
                (skeleton.contains("syntheticprivatevalue")
                    || fragments.iter().any(|fragment| skeleton.contains(fragment)))
                .then_some(*name)
            })
            .collect::<Vec<_>>();
        eprintln!("SURFACES {label}: {exposed:?}");
        if !exposed.is_empty() {
            leaked.push(label);
        }
    }
    assert!(leaked.is_empty(), "credential leak cases: {leaked:?}");
}

async fn incomplete_glm_server(status: u16, code: &str) -> String {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let body=serde_json::json!({"error":{"code":code,"message":"Provider detail: more than max tokens allowed"}}).to_string();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = vec![0; 32768];
        let _ = stream.read(&mut bytes).await;
        let _ = stream
            .write_all(
                format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len() + 64
                )
                .as_bytes(),
            )
            .await;
    });
    address
}
#[tokio::test]
async fn incomplete_glm_rejections_keep_http_diagnostics_and_do_not_retry() {
    for (status, code) in [400, 429, 500]
        .into_iter()
        .flat_map(|status| ["1261", "1210", "1301"].map(|code| (status, code)))
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut config = tickloop_harness::model::cards::ModelCatalog::defaults()
            .get("zhipu-cn-api", "glm-5.3")
            .unwrap();
        tickloop_harness::model::credentials::save_api_key(
            &root,
            &config.api_key_env,
            "synthetic-glm-fixture-key",
        )
        .unwrap();
        config.base_url = incomplete_glm_server(status, code).await;
        let client = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        let failure = error
            .provider_failure()
            .expect("incomplete GLM errors retain HTTP diagnostics");
        assert_eq!(failure.status, status);
        assert!(failure.message.contains("Provider detail"));
        assert!(failure.hint.contains("Rejection body incomplete"));
        assert!(!error.retry_hint().0);

        config.base_url = incomplete_glm_server(status, code).await;
        let client = LocalModelClients::new(Some(root.to_path_buf()))
            .client(&config)
            .unwrap();
        let agent = Agent::open_embedded(
            root.clone(),
            config,
            provider_transport::without_title_requests(client),
        )
        .unwrap();
        let chat = agent.create_chat(root.clone()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "incomplete GLM fixture".into())
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Failed(message) => {
                    assert!(message.contains(&format!("HTTP {status}")));
                    assert!(message.contains("Rejection body incomplete"));
                    break;
                }
                EventKind::ModelRetry { .. } => panic!("incomplete GLM rejection retried"),
                _ => {}
            }
        }
        assert!(
            !agent
                .snapshot(chat)
                .await
                .unwrap()
                .provider_failures
                .is_empty()
        );
        agent.shutdown().await;
    }
}

fn confirmation2_cases() -> Vec<(String, String, String)> {
    let secret = "opaque-fixture-credential";
    let mut cases: Vec<(String, String, String)> = Vec::new();
    for width in [1, 3, 5, 7] {
        let groups = secret
            .as_bytes()
            .chunks(width)
            .map(|s| std::str::from_utf8(s).unwrap())
            .collect::<Vec<_>>();
        for separator in [" ", " / ", "%20", "&#32;", r"\u0020", "<b></b>"] {
            let split = groups.join(separator);
            for wrapper in [
                format!("Rejected {split}"),
                format!("<html>{split}</html>"),
                format!("Bearer {split}"),
            ] {
                cases.push((
                    format!("split-{width}-{separator}-{wrapper}"),
                    wrapper,
                    secret.into(),
                ));
            }
        }
    }
    for prefix in [
        "Authorization: Bearer ",
        "Authorization: Basic ",
        "authorization = bearer ",
        "api key = Bearer ",
    ] {
        cases.push((
            format!("nested-label-{prefix}"),
            format!("{prefix}synthetic-private-value"),
            secret.into(),
        ));
    }
    for suffix in [r"\u0041", "&#65;", "%41", "%2541", "&#x41;", "Ａ"] {
        cases.push((
            format!("encoded-A-{suffix}"),
            format!("Rejected {suffix}1234567B7654321"),
            "A1234567B7654321".into(),
        ));
    }
    for n in [1, 2, 4, 8, 15, 16, 17, 24] {
        let mut encoded = "%6fpaque-fixture-credential".to_owned();
        for _ in 1..n {
            encoded = encoded.replace('%', "%25");
        }
        cases.push((format!("nested-percent-{n}"), encoded, secret.into()));
    }
    for form in [
        "Café-credential",
        "Cafe\u{0301}-credential",
        "Ｃａｆé-credential",
    ] {
        cases.push((
            format!("canonical-{form}"),
            format!("Rejected {form}"),
            "Café-credential".into(),
        ));
    }

    assert_eq!(cases.len(), 93);
    cases
}

fn security_skeleton(text: &str) -> String {
    let decoded = percent_encoding::percent_decode_str(text).decode_utf8_lossy();
    icu_normalizer::ComposingNormalizer::new_nfkc()
        .normalize(&decoded)
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[tokio::test]
async fn all_93_confirmation2_probe_variants_are_safe_on_all_surfaces() {
    assert_safe_provider_surfaces(confirmation2_cases()).await;
}

#[tokio::test]
async fn escaped_and_unterminated_auth_quotes_are_safe_on_all_surfaces() {
    let cases = [

 ("prior-inline-html",r#"Authorization: Bearer prefix<b>synthetic-private-value</b>"#),
 ("prior-encoded-quote",r#"api-key: "prefix%22synthetic-private-value""#),
 ("percent-lf",r#"Authorization: Bearer prefix%0Asynthetic-private-value"#),
 ("percent-cr",r#"Authorization: Bearer prefix%0Dsynthetic-private-value"#),
 ("entity-lf",r#"api-key: prefix&#10;synthetic-private-value"#),
 ("entity-cr",r#"token: prefix&#13;synthetic-private-value"#),
 ("unicode-lf",r#"Authorization: Bearer prefix\u000Asynthetic-private-value"#),
 ("unicode-linesep",r#"api-key: prefix&#x2028;synthetic-private-value"#),
 ("double-percent-lf",r#"Authorization: Bearer prefix%250Asynthetic-private-value"#),
 ("quoted-percent-lf",r#"Authorization: "Bearer prefix%0Asynthetic-private-value""#),
 ("nfkc-quote-count-collision", "Authorization: \"Bearer prefix＂synthetic-private-value\"\x1b]0;\"\x07"),
 ("nfkc-single-count-collision", "api-key: 'prefix＇synthetic-private-value'\x1b]0;'\x07"),
 ("raw-line-control", "Authorization: Bearer prefix\nHelp https://support.example/quota"),
            ("r1-escaped-quote",r#"api-key: "prefix\"synthetic-private-value""#),
            ("r2-percent-quote",r#"api-key: "prefix%22synthetic-private-value""#),
            ("r2-entity-quote",r#"Authorization: "Bearer prefix&#34;synthetic-private-value""#),
            ("r2-html",r#"Authorization: Bearer prefix<b>synthetic-private-value</b>"#),
            ("plain-auth", "Authorization: Bearer synthetic-private-value"),
            ("percent-LF", "Authorization: Bearer prefix%0Asynthetic-private-value"),
            ("percent-CR", "api-key: prefix%0Dsynthetic-private-value"),
            ("percent-CRLF", "token: prefix%0D%0Asynthetic-private-value"),
            ("entity-LF", "Authorization: Bearer prefix&#10;synthetic-private-value"),
            ("named-entity-LF", "api-key: prefix&NewLine;synthetic-private-value"),
            ("unicode-LF", r"api-key: prefix\u000asynthetic-private-value"),
            ("unicode-LS", r"token: prefix\u2028synthetic-private-value"),
            ("double-percent-LF", "api-key: prefix%250Asynthetic-private-value"),
            ("quoted-percent-LF", "api-key: \"prefix%0Asynthetic-private-value\""),
            ("quoted-entity-LF", "Authorization: \"Bearer prefix&#10;synthetic-private-value\""),
            ("quoted-unicode-LF", r#"token: "prefix\u000asynthetic-private-value""#),
            ("actual-line-boundary", "api-key: public\nHelp: https://support.example/faq"),


 ("prior-escaped-digest",r#"Authorization: "Digest username=\"public-name\", nonce=\"synthetic-private-value\"""#),
 ("prior-escaped-bearer",r#"Authorization: "Bearer prefix\"synthetic-private-value""#),
 ("prior-single-quoted",r#"api-key: 'prefix\'synthetic-private-value'"#),
 ("unknown-html-prefix",r#"Authorization: Bearer prefix<b>synthetic-private-value</b>"#),
 ("unknown-html-middle",r#"Authorization: Bearer synthetic-<b>private</b>-value"#),
 ("unknown-plain-less",r#"Authorization: Bearer prefix<synthetic-private-value"#),
 ("unknown-html-quoted",r#"Authorization: "Bearer prefix<b>synthetic-private-value</b>""#),
 ("unknown-api-html",r#"api-key: prefix<i></i>synthetic-private-value"#),
 ("unknown-token-html",r#"token: prefix<i></i>synthetic-private-value"#),
 ("unknown-wrapped-label",r#"<html>Authorization: Bearer prefix<b>synthetic-private-value</b></html>"#),
 ("unknown-header-nul", "Authorization: Bearer prefix\0synthetic-private-value"),
 ("escaped-quote-control", r#"api-key: "prefix\"synthetic-private-value""#),
 ("percent-quote", r#"api-key: "prefix%22synthetic-private-value""#),
 ("entity-quote", r#"Authorization: "Bearer prefix&#34;synthetic-private-value""#),
 ("unicode-quote", r#"Authorization: "Bearer prefix\u0022synthetic-private-value""#),
 ("nfkc-quote", "api-key: \"prefix＂synthetic-private-value\""),
 ("single-percent-quote", "token: 'prefix%27synthetic-private-value'"),
 ("escaped-entity-quote", r#"api-key: "prefix\&#34;synthetic-private-value""#),
 ("double-encoded-quote", r#"api-key: "prefix%2522synthetic-private-value""#),

        ("html-unquoted-bearer", "Authorization: Bearer prefix<b>synthetic-private-value</b>"),
        ("html-unquoted-api-key", "api-key: prefix<i></i>synthetic-private-value"),
        ("html-unquoted-token", "token: prefix<span>synthetic-private-value</span>"),
        ("html-wrapped-auth", "<html>Authorization: Bearer prefix<b>synthetic-private-value</b></html>"),
        ("html-quoted-control", r#"api-key: "prefix<b>synthetic-private-value</b>""#),
        ("encoded-double-quote", r#"api-key: "prefix%22synthetic-private-value""#),
        ("entity-double-quote", r#"Authorization: "Bearer prefix&#34;synthetic-private-value""#),
        ("unicode-double-quote", r#"api-key: "prefix\u0022synthetic-private-value""#),
        ("nfkc-double-quote", r#"api-key: "prefix＂synthetic-private-value""#),
        ("encoded-single-quote", "api-key: 'prefix%27synthetic-private-value'"),
        ("double-percent-quote", r#"api-key: "prefix%2522synthetic-private-value""#),

        ("quoted-escaped-digest", r#"Authorization: "Digest username=\"public-name\", nonce=\"synthetic-private-value\"""#),
        ("quoted-escaped-bearer", r#"Authorization: "Bearer prefix\"synthetic-private-value""#),
        ("single-quoted-escaped", r"api-key: 'prefix\'synthetic-private-value'"),
        ("unterminated-double-quote", r#"api-key: "synthetic-private-value"#),
        ("unterminated-single-quote", "api-key: 'synthetic-private-value"),
        ("ordinary-nested", "Authorization: Bearer synthetic-private-value"),
        ("plain-digest", r#"Authorization: Digest username="public-name", nonce="synthetic-private-value""#),
    ].map(|(label, message)| (label.into(), message.into(), "opaque-fixture-credential".into())).to_vec();
    assert_safe_provider_surfaces(cases).await;
}

#[tokio::test]
async fn redacted_presentation_ids_do_not_change_connection_retention_or_success_clearing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut config = tickloop_harness::model::cards::ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash")
        .unwrap();
    tickloop_harness::model::credentials::save_api_key(
        &root,
        &config.api_key_env,
        "deepseek-api-synthetic-key",
    )
    .unwrap();
    let (url, _) = model_http_support::serve_response(
        401,
        serde_json::json!({"error":{"message":"Provider refused this request"}}).to_string(),
    )
    .await;
    config.base_url = url;
    let local = LocalModelClients::new(Some(root.to_path_buf()));
    let models = ChosenModels::new(provider_transport::without_title_requests(
        local.client(&config).unwrap(),
    ));
    let agent = opened_agent(root.clone(), config.clone(), models.clone());
    let chat = agent.create_chat(root.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "connection identity fixture".into())
        .await
        .unwrap();
    loop {
        if matches!(
            tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind,
            EventKind::Failed(_)
        ) {
            break;
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let failure = snapshot
        .provider_failures
        .get("deepseek-api")
        .expect("diagnostic uses the stable connection identity");
    assert_ne!(
        failure.provider_id, "deepseek-api",
        "published provider ID is still sanitized"
    );
    let body = "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Recovered\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let (url, _) = model_http_support::serve_response(200, body.into()).await;
    config.base_url = url;
    models.choose(provider_transport::without_title_requests(
        local.client(&config).unwrap(),
    ));
    agent.set_model(chat, config).await.unwrap();
    agent
        .submit(chat, "successful connection fixture".into())
        .await
        .unwrap();
    loop {
        match tickloop_harness::test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::Completed { .. } => break,
            EventKind::Failed(message) => panic!("{message}"),
            _ => {}
        }
    }
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .provider_failures
            .is_empty()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn provider_messages_stay_within_1024_characters_including_truncation() {
    for json_body in [false, true] {
        let message = "a".repeat(1100);
        let body = if json_body {
            serde_json::json!({"error":{"message":message}}).to_string()
        } else {
            message
        };
        let (url, _) = model_http_support::serve_response(400, body).await;
        let mut config = tickloop_harness::test_support::config();
        config.base_url = url;
        let client = LocalModelClients::new(None).client(&config).unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(request(), events, CancellationToken::new())
            .await
            .unwrap_err();
        let message = &error.provider_failure().unwrap().message;
        assert_eq!(message.chars().count(), 1024);
        assert!(message.ends_with("..."));
    }
}

#[tokio::test]
async fn final_provider_rendering_cannot_join_secret_fragments_across_fields() {
    assert_safe_provider_surfaces(
        [
            "highentropy-AbCdEfGC-final-key",
            "highentropy-AbCdEfGC-safetyCr-safetyAu-final-key",
            "highentropy-AbCdEfGC-QzX-safetyCred-QzX-safetyAuth-final-key",
        ]
        .map(|secret| {
            (
                "cross-field-message-hint".into(),
                "AbCdEfG".into(),
                secret.into(),
            )
        })
        .to_vec(),
    )
    .await;
}
