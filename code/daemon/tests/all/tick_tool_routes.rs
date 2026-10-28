use std::{collections::BTreeSet, path::Path};

use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickloop_code_local_environment::hosted::{
    tools::HostedTickTools,
    worker_control::{DaemonControl, WorkerControlGrant},
};
use tickloop_harness::protocol::{HostEndpointIdentity, HostToolAuthority};
use tickloop_harness::runtime::host_tools::HostToolProvider;
use tokio_util::sync::CancellationToken;

struct Tools {
    _directory: tempfile::TempDir,
    host_authority: HostToolAuthority,
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

/// Tick tools behind a daemon nobody listens on, so a call that reached the daemon would fail
/// visibly.
fn tools() -> Tools {
    let directory = tempfile::Builder::new()
        .prefix("ttr")
        .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
        .unwrap();
    Tools {
        host_authority: HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::Tickhost {
                endpoint: directory.path().join("unused-host.sock"),
                uid: unsafe { libc::geteuid() },
            },
        },
        hosted: hosted_tools(directory.path()),
        _directory: directory,
    }
}

/// Every Tick tool runs under the Tickhost alone; no call names a Herdr endpoint.
#[tokio::test]
async fn every_tick_call_runs_under_the_tickhost() {
    let tools = tools();
    let hosted = InstanceRef::hosted(uuid::Uuid::new_v4()).to_string();
    for (name, input) in [
        ("TickRead", json!({"tick_instance_id":hosted})),
        ("TickDescribe", json!({"tick_instance_id":hosted})),
        (
            "TickCall",
            json!({"tick_instance_id":hosted,"tool":"increment","args":{}}),
        ),
        ("TickManage", json!({"action":"open","command":["tick"]})),
        (
            "TickManage",
            json!({"action":"open","command":["python3","counter.py"],"protocol":true}),
        ),
        (
            "TickManage",
            json!({"action":"send_input","tick_instance_id":hosted,"text":"q"}),
        ),
        (
            "TickManage",
            json!({"action":"close","tick_instance_id":hosted}),
        ),
        (
            "TickManage",
            json!({"action":"reopen","tick_instance_id":hosted}),
        ),
    ] {
        assert_eq!(
            tools.hosted.call_authority(name, &input),
            Ok(Some(tools.host_authority.clone())),
            "{name} {input}"
        );
    }
    for name in ["TickRead", "TickDescribe", "TickCall", "TickManage"] {
        assert_eq!(
            tools.hosted.tool_authorities(name),
            Ok(BTreeSet::from([tools.host_authority.clone()])),
            "{name}"
        );
    }
    // What earlier builds offered for Herdr Panes is no call at all.
    for (name, input) in [
        ("ReadWorkspace", json!({})),
        (
            "TickManage",
            json!({"action":"move","tick_instance_id":hosted}),
        ),
    ] {
        assert!(
            tools.hosted.call_authority(name, &input).is_err(),
            "{name} {input}"
        );
    }
}

#[tokio::test]
async fn artifact_shell_and_untyped_targets_never_reach_a_host() {
    let tools = tools();
    let chat = uuid::Uuid::new_v4();
    let tools = &tools.hosted;
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
                    .prepare_call(chat.into(), name, &input, CancellationToken::new())
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
                chat.into(),
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

fn tool<'a>(
    definitions: &'a [tickloop_harness::protocol::ToolDefinition],
    name: &str,
) -> &'a Value {
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
fn tick_tool_schemas_open_in_the_tickhost_and_place_no_views() {
    let tools = tools();
    let strings = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
    let definitions = tools.hosted.definitions();
    assert_eq!(
        definitions
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<BTreeSet<_>>(),
        strings(&["TickCall", "TickDescribe", "TickManage", "TickRead"])
    );
    let modify = definitions
        .iter()
        .find(|tool| tool.name == "TickManage")
        .unwrap();
    for fact in ["Tickhost", "Monitor"] {
        assert!(
            modify.description.contains(fact),
            "{fact}: {}",
            modify.description
        );
    }
    assert!(
        !modify.description.contains("Herdr"),
        "{}",
        modify.description
    );
    let schema = tool(&definitions, "TickManage");
    assert_eq!(schema["required"], json!(["action"]));
    assert_eq!(
        choices(&schema["properties"]["action"]),
        strings(&["open", "send_input", "close", "reopen"])
    );
    assert_eq!(schema["properties"]["protocol"]["type"], "boolean");
    for retired in ["placement", "workspace_id"] {
        assert!(schema["properties"].get(retired).is_none(), "{retired}");
    }
    let refusal = |input: Value| {
        tools
            .hosted
            .validate_input("TickManage", schema, &input)
            .unwrap_err()
    };
    assert_eq!(
        refusal(json!({"action":"close"})),
        "invalid TickManage input: close requires tick_instance_id"
    );
    assert_eq!(
        refusal(json!({"action":"open","command":["tick"],"placement":{"type":"hidden"}})),
        "invalid TickManage input: placement is not allowed for open; open takes command, and optionally cwd, protocol"
    );
    assert_eq!(
        refusal(
            json!({"action":"move","tick_instance_id":"hosted:1","placement":{"type":"new_tab"}})
        ),
        "invalid TickManage input: action must be one of open, send_input, close, reopen"
    );
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
    for definition in &definitions {
        assert_eq!(
            definition.input_schema["type"], "object",
            "{} has an object root for model APIs",
            definition.name
        );
    }
}

#[tokio::test]
async fn peers_with_the_old_delegation_authority_schema_are_rejected_at_handshake() {
    use tickloop_code_access::AccessErrorCode;
    use tickloop_code_client::LocalAgentClient;
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
