use std::{
    collections::BTreeSet,
    os::unix::fs::MetadataExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickhost_connect::{
    herdr::{HerdrContext, HerdrPort},
    host_error::HostError,
    layout::HerdrLayout,
};
use tickloop_agent_protocol::{HostEndpointIdentity, HostToolAuthority};
use tickloop_agent_runtime::{
    host_tools::HostToolProvider,
    hosted::{
        tools::HostedTickTools,
        views::HerdrViews,
        worker_control::{DaemonControl, WorkerControlGrant},
    },
};
use tokio_util::sync::CancellationToken;

/// A Herdr that counts requests and refuses them all: choosing an authority must never need it.
#[derive(Default)]
struct CountingHerdr {
    requests: AtomicUsize,
}

#[async_trait]
impl HerdrPort for CountingHerdr {
    async fn request(
        &self,
        method: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, HostError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Err(HostError::before("UnknownMethod", method))
    }
}

struct Tools {
    _directory: tempfile::TempDir,
    _listener: std::os::unix::net::UnixListener,
    herdr: Arc<CountingHerdr>,
    herdr_authority: HostToolAuthority,
    host_authority: HostToolAuthority,
    /// The Tick tools of a worker in a Herdr View.
    in_herdr: HostedTickTools,
    /// The Tick tools of a worker outside Herdr.
    hosted: HostedTickTools,
}

fn hosted_tools(directory: &Path) -> HostedTickTools {
    HostedTickTools::new(
        DaemonControl::new(WorkerControlGrant {
            endpoint: directory.join("unused-daemon.sock"),
            daemon_epoch: uuid::Uuid::new_v4(),
            binding: uuid::Uuid::new_v4(),
            capability: "0".repeat(64),
            host_endpoint: directory.join("unused-host.sock"),
        }),
        Vec::new(),
    )
}

/// Tick tools in and outside Herdr behind a daemon nobody listens on, so a call that reached
/// the daemon or Herdr would fail visibly.
fn tools() -> Tools {
    let directory = tempfile::Builder::new()
        .prefix("ttr")
        .tempdir_in(tickloop_agent_test_support::SHORT_TEMP_ROOT)
        .unwrap();
    let socket = directory.path().join("herdr.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let metadata = std::fs::metadata(&socket).unwrap();
    let herdr = Arc::new(CountingHerdr::default());
    let in_herdr = hosted_tools(directory.path()).with_herdr_views(HerdrViews::new(
        HerdrLayout::with_host(HerdrContext::without_view(socket.clone()), herdr.clone()),
        Path::new("/private/tmp/unused/tickloop"),
    ));
    Tools {
        herdr_authority: HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::HerdrSocket {
                path: socket.canonicalize().unwrap(),
                owner: metadata.uid(),
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        },
        host_authority: HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::Tickhost {
                endpoint: directory.path().join("unused-host.sock"),
                uid: unsafe { libc::geteuid() },
            },
        },
        hosted: hosted_tools(directory.path()),
        in_herdr,
        herdr,
        _listener: listener,
        _directory: directory,
    }
}

#[tokio::test]
async fn ticks_run_under_the_tickhost_and_their_herdr_panes_under_herdr() {
    let tools = tools();
    let hosted = InstanceRef::hosted(uuid::Uuid::new_v4()).to_string();
    let herdr_authority = Ok(tools.herdr_authority.clone());
    let host_authority = Ok(tools.host_authority.clone());
    for (name, input, expected) in [
        ("ReadWorkspace", json!({}), &herdr_authority),
        (
            "TickRead",
            json!({"tick_instance_id":hosted}),
            &host_authority,
        ),
        (
            "TickCall",
            json!({"tick_instance_id":hosted,"tool":"increment","args":{}}),
            &host_authority,
        ),
        (
            "TickManage",
            json!({"action":"open","command":["tick"]}),
            &host_authority,
        ),
        (
            "TickManage",
            json!({"action":"open","command":["tick"],"placement":{"type":"hidden"}}),
            &host_authority,
        ),
        (
            "TickManage",
            json!({"action":"open","command":["tick"],"placement":{"type":"new_tab"}}),
            &herdr_authority,
        ),
        (
            "TickManage",
            json!({"action":"open","command":["tick"],"placement":{"type":"current_tab"}}),
            &herdr_authority,
        ),
        (
            "TickManage",
            json!({"action":"open","command":["python3","counter.py"],"protocol":true,"placement":{"type":"pane","pane_id":"p"}}),
            &herdr_authority,
        ),
        (
            "TickManage",
            json!({"action":"move","tick_instance_id":hosted}),
            &herdr_authority,
        ),
        (
            "TickManage",
            json!({"action":"send_input","tick_instance_id":hosted,"text":"q"}),
            &host_authority,
        ),
        (
            "TickManage",
            json!({"action":"close","tick_instance_id":hosted}),
            &host_authority,
        ),
        (
            "TickManage",
            json!({"action":"reopen","tick_instance_id":hosted}),
            &host_authority,
        ),
    ] {
        assert_eq!(
            tools.in_herdr.call_authority(name, &input),
            expected.clone().map(Some),
            "{name} {input}"
        );
    }
    for name in ["TickRead", "TickCall"] {
        assert_eq!(
            tools.in_herdr.tool_authorities(name),
            Ok(BTreeSet::from([tools.host_authority.clone()])),
            "{name}"
        );
    }
    assert_eq!(
        tools.in_herdr.tool_authorities("TickManage"),
        Ok(BTreeSet::from([
            tools.herdr_authority.clone(),
            tools.host_authority.clone()
        ]))
    );
    assert_eq!(
        tools.in_herdr.tool_authorities("ReadWorkspace"),
        Ok(BTreeSet::from([tools.herdr_authority.clone()]))
    );

    for input in [
        json!({"action":"open","command":["tick"],"placement":{"type":"new_tab"}}),
        json!({"action":"move","tick_instance_id":hosted}),
    ] {
        let refused = tools
            .hosted
            .call_authority("TickManage", &input)
            .unwrap_err();
        assert!(refused.contains("Herdr"), "{input}: {refused}");
    }
    assert!(
        tools
            .hosted
            .call_authority("ReadWorkspace", &json!({}))
            .unwrap_err()
            .contains("Herdr")
    );
    assert_eq!(
        tools.hosted.tool_authorities("TickManage"),
        Ok(BTreeSet::from([tools.host_authority.clone()]))
    );
    assert_eq!(
        tools.herdr.requests.load(Ordering::SeqCst),
        0,
        "choosing an authority never asks Herdr"
    );
}

#[tokio::test]
async fn artifact_shell_herdr_and_untyped_targets_never_reach_a_host() {
    let tools = tools();
    let chat = uuid::Uuid::new_v4();
    for tools in [&tools.in_herdr, &tools.hosted] {
        for (target, native_tool) in [
            ("artifact:doc", "ArtifactRead"),
            ("artifact:https://docs.example/doc", "ArtifactRead"),
            ("shell:task", "TaskStop"),
        ] {
            for (name, input) in [
                ("TickRead", json!({"tick_instance_id":target})),
                (
                    "TickCall",
                    json!({"tick_instance_id":target,"tool":"read","args":{}}),
                ),
                (
                    "TickManage",
                    json!({"action":"close","tick_instance_id":target}),
                ),
            ] {
                for error in [
                    tools.call_authority(name, &input).unwrap_err(),
                    tools
                        .prepare_call(chat, name, &input, CancellationToken::new())
                        .await
                        .unwrap_err(),
                ] {
                    assert!(error.contains("NativeToolRequired"), "{name}: {error}");
                    assert!(error.contains(native_tool), "{name}: {error}");
                }
            }
        }
        for target in ["1234", "herdr:terminal:x:1:1"] {
            let error = tools
                .prepare_call(
                    chat,
                    "TickRead",
                    &json!({"tick_instance_id":target}),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(error.contains("InvalidTickTarget"), "{error}");
            assert!(error.contains("hosted:"), "{error}");
        }
    }
    assert_eq!(tools.herdr.requests.load(Ordering::SeqCst), 0);
}

fn tool<'a>(definitions: &'a [tickloop_agent_protocol::ToolDefinition], name: &str) -> &'a Value {
    &definitions
        .iter()
        .find(|definition| definition.name == name)
        .unwrap_or_else(|| panic!("{name} is not defined"))
        .input_schema
}

fn choices(property: &Value) -> BTreeSet<String> {
    property["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|choice| choice.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn tick_tool_schemas_open_in_the_tickhost_and_place_views_only_in_herdr() {
    let tools = tools();
    let strings = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();

    let in_herdr = tools.in_herdr.definitions();
    assert_eq!(
        in_herdr
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<BTreeSet<_>>(),
        strings(&[
            "TickCall",
            "TickDescribe",
            "TickManage",
            "TickRead",
            "ReadWorkspace"
        ])
    );
    let modify = in_herdr
        .iter()
        .find(|tool| tool.name == "TickManage")
        .unwrap();
    for fact in ["Tickhost", "Monitor", "view", "closes only that view"] {
        assert!(
            modify.description.contains(fact),
            "{fact}: {}",
            modify.description
        );
    }
    let schema = tool(&in_herdr, "TickManage");
    assert_eq!(schema["required"], json!(["action"]));
    assert_eq!(
        choices(&schema["properties"]["action"]),
        strings(&["open", "move", "send_input", "close", "reopen"])
    );
    assert_eq!(
        choices(&schema["properties"]["placement"]["properties"]["type"]),
        strings(&["hidden", "current_tab", "pane", "new_tab"])
    );
    assert_eq!(schema["properties"]["protocol"]["type"], "boolean");
    let refusal = |input: Value| {
        tools
            .in_herdr
            .validate_input("TickManage", schema, &input)
            .unwrap_err()
    };
    assert!(refusal(json!({})).starts_with("invalid TickManage input: action is required: one of open (requires command), move (requires tick_instance_id)"));
    assert_eq!(
        refusal(
            json!({"action":"move","tick_instance_id":"hosted:1","placement":{"type":"new_tab"}})
        ),
        "invalid TickManage input: placement.type for move must be one of current_tab, pane"
    );

    let hosted = tools.hosted.definitions();
    assert_eq!(
        hosted
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<BTreeSet<_>>(),
        strings(&["TickCall", "TickDescribe", "TickManage", "TickRead"])
    );
    let schema = tool(&hosted, "TickManage");
    assert_eq!(
        choices(&schema["properties"]["action"]),
        strings(&["open", "send_input", "close", "reopen"])
    );
    assert_eq!(
        choices(&schema["properties"]["placement"]["properties"]["type"]),
        strings(&["hidden"])
    );
    assert!(schema["properties"].get("workspace_id").is_none());
    assert_eq!(
        tools
            .hosted
            .validate_input("TickManage", schema, &json!({"action":"close"}))
            .unwrap_err(),
        "invalid TickManage input: close requires tick_instance_id"
    );
    for definitions in [&in_herdr, &hosted] {
        let read = definitions
            .iter()
            .find(|tool| tool.name == "TickRead")
            .unwrap();
        assert!(read.description.contains("hosted:"), "{}", read.description);
        assert!(!read.description.contains("herdr:"), "{}", read.description);
        assert!(
            read.description.contains("Terminal Ticks"),
            "{}",
            read.description
        );
        for definition in definitions.iter() {
            assert_eq!(
                definition.input_schema["type"], "object",
                "{} has an object root for model APIs",
                definition.name
            );
        }
    }
}

#[tokio::test]
async fn peers_with_the_old_delegation_authority_schema_are_rejected_at_handshake() {
    use tickloop_agent_client::LocalAgentClient;
    use tickloop_agent_protocol::AccessErrorCode;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixListener,
    };
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let socket = directory.path().join("old-peer.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let length = stream.read_u32().await.unwrap();
        let mut request = vec![0; length as usize];
        stream.read_exact(&mut request).await.unwrap();
        let response = serde_json::to_vec(&json!({"Hello":{"major":3,"minor":2,"daemon_epoch":uuid::Uuid::new_v4(),"session_id":uuid::Uuid::new_v4()}})).unwrap();
        stream.write_u32(response.len() as u32).await.unwrap();
        stream.write_all(&response).await.unwrap();
    });
    let result = LocalAgentClient::connect(&socket).await;
    server.await.unwrap();
    assert!(
        matches!(result, Err(ref error) if error.code == AccessErrorCode::VersionMismatch),
        "old peer accepted an incompatible delegation authority schema"
    );
}

#[tokio::test]
async fn a_native_tool_redirect_precedes_an_invalid_herdr_endpoint() {
    use tickloop_agent_protocol::EventKind;
    use tickloop_agent_runtime::Agent;
    let directory = tempfile::tempdir().unwrap();
    let endpoint = directory.path().join("not-a-socket");
    std::fs::write(&endpoint, "fixture").unwrap();
    let provider = Arc::new(
        hosted_tools(directory.path()).with_herdr_views(HerdrViews::new(
            HerdrLayout::new(HerdrContext::without_view(endpoint)),
            Path::new("/private/tmp/unused/tickloop"),
        )),
    );
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::call(
            "redirect",
            "TickCall",
            json!({"tick_instance_id":"artifact:doc","tool":"read","args":{}}),
        ),
        tickloop_agent_test_support::text("Done."),
    ]));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model);
    agent.install_host_tools(provider).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Inspect the artifact".into())
        .await
        .unwrap();
    let result = loop {
        if let EventKind::ToolCompleted { result, .. } =
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
        {
            break result;
        }
    };
    agent.shutdown().await;
    assert!(result.is_error);
    assert!(
        result.content.contains("NativeToolRequired"),
        "{}",
        result.content
    );
}
