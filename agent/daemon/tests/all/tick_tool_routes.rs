use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickhost_connect::{
    access::TickAccess,
    herdr::{HerdrContext, HerdrPort},
    host_error::HostError,
};
use tickloop_agent_protocol::{HostEndpointIdentity, HostToolAuthority};
use tickloop_agent_runtime::{
    host_tools::HostToolProvider,
    hosted::{
        tools::HostedTickTools,
        worker_control::{DaemonControl, WorkerControlGrant},
    },
    native_host::NativeHostToolProvider,
    tick_tool_router::TickToolRouter,
};
use tokio_util::sync::CancellationToken;

/// A Herdr that counts requests and refuses them all: routing must never need it.
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

struct Backends {
    herdr: Arc<CountingHerdr>,
    herdr_authority: HostToolAuthority,
    host_authority: HostToolAuthority,
    native: Arc<NativeHostToolProvider>,
    hosted: Arc<HostedTickTools>,
}

/// A trusted Herdr adapter and a Tickhost behind a daemon nobody listens on, so a call that
/// reached either backend would fail visibly.
fn backends() -> Backends {
    let herdr = Arc::new(CountingHerdr::default());
    let scope = uuid::Uuid::new_v4();
    let native = Arc::new(NativeHostToolProvider::with_trusted_adapter_identity(
        Arc::new(TickAccess::with_host(
            HerdrContext::without_view("/private/tmp/unused-herdr.sock".into()),
            herdr.clone(),
        )),
        scope,
    ));
    let grant = WorkerControlGrant {
        endpoint: "/private/tmp/unused-daemon.sock".into(),
        daemon_epoch: uuid::Uuid::new_v4(),
        binding: uuid::Uuid::new_v4(),
        capability: "0".repeat(64),
        host_endpoint: "/private/tmp/unused-host.sock".into(),
        intent_renewal: std::time::Duration::from_secs(60),
    };
    Backends {
        herdr,
        herdr_authority: HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::TrustedAdapter { scope },
        },
        host_authority: HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::Tickhost {
                endpoint: grant.host_endpoint.clone(),
                uid: unsafe { libc::geteuid() },
            },
        },
        native,
        hosted: Arc::new(HostedTickTools::new(DaemonControl::new(grant), Vec::new())),
    }
}

#[tokio::test]
async fn each_tick_tool_runs_under_the_authority_of_the_backend_its_target_names() {
    let backends = backends();
    let router = TickToolRouter::new(Some(backends.native.clone()), Some(backends.hosted.clone()));
    let herdr = "herdr:terminal:target:1:1";
    let hosted = InstanceRef::hosted(uuid::Uuid::new_v4()).to_string();
    let herdr_authority = Ok(backends.herdr_authority.clone());
    let host_authority = Ok(backends.host_authority.clone());
    let herdr_only = Err("Herdr");
    let host_only = Err("hosted");
    for (name, input, expected) in [
        ("ReadWorkspace", json!({}), &herdr_authority),
        (
            "ReadTick",
            json!({"tick_instance_id":herdr}),
            &herdr_authority,
        ),
        (
            "ReadTick",
            json!({"tick_instance_id":hosted}),
            &host_authority,
        ),
        (
            "CallTickTool",
            json!({"tick_instance_id":hosted,"tool":"increment","args":{}}),
            &host_authority,
        ),
        (
            "ModifyTick",
            json!({"action":"open","command":["tick"]}),
            &host_authority,
        ),
        (
            "ModifyTick",
            json!({"action":"open","command":["tick"],"placement":{"type":"hidden"}}),
            &host_authority,
        ),
        (
            "ModifyTick",
            json!({"action":"open","command":["tick"],"placement":{"type":"new_tab"}}),
            &herdr_authority,
        ),
        (
            "ModifyTick",
            json!({"action":"open","command":["tick"],"placement":{"type":"current_tab"}}),
            &herdr_authority,
        ),
        (
            "ModifyTick",
            json!({"action":"open","command":["tick"],"placement":{"type":"pane","pane_id":"p"}}),
            &herdr_authority,
        ),
        (
            "ModifyTick",
            json!({"action":"move","tick_instance_id":herdr}),
            &herdr_authority,
        ),
        (
            "ModifyTick",
            json!({"action":"move","tick_instance_id":hosted}),
            &herdr_only,
        ),
        (
            "ModifyTick",
            json!({"action":"send_input","tick_instance_id":hosted,"text":"q"}),
            &herdr_only,
        ),
        (
            "ModifyTick",
            json!({"action":"close","tick_instance_id":hosted}),
            &host_authority,
        ),
        (
            "ModifyTick",
            json!({"action":"close","tick_instance_id":herdr}),
            &herdr_authority,
        ),
        (
            "ModifyTick",
            json!({"action":"reopen","tick_instance_id":hosted}),
            &host_authority,
        ),
        (
            "ModifyTick",
            json!({"action":"reopen","tick_instance_id":herdr}),
            &host_only,
        ),
    ] {
        let routed = router.call_authority(name, &input);
        match expected {
            Ok(authority) => assert_eq!(routed, Ok(Some(authority.clone())), "{name} {input}"),
            Err(reason) => {
                let error = routed.unwrap_err();
                assert!(error.contains(reason), "{name} {input}: {error}");
            }
        }
    }
    let both = BTreeSet::from([
        backends.herdr_authority.clone(),
        backends.host_authority.clone(),
    ]);
    for name in ["ReadTick", "CallTickTool", "ModifyTick"] {
        assert_eq!(router.tool_authorities(name), Ok(both.clone()), "{name}");
    }
    assert_eq!(
        router.tool_authorities("ReadWorkspace"),
        Ok(BTreeSet::from([backends.herdr_authority.clone()]))
    );

    let herdr_router = TickToolRouter::new(Some(backends.native.clone()), None);
    let hidden = herdr_router
        .call_authority("ModifyTick", &json!({"action":"open","command":["tick"]}))
        .unwrap_err();
    assert!(hidden.contains("Tickhost"), "{hidden}");
    assert_eq!(
        herdr_router.tool_authorities("ReadTick"),
        Ok(BTreeSet::from([backends.herdr_authority.clone()]))
    );
    let hosted_router = TickToolRouter::new(None, Some(backends.hosted.clone()));
    let visible = hosted_router
        .call_authority(
            "ModifyTick",
            &json!({"action":"open","command":["tick"],"placement":{"type":"new_tab"}}),
        )
        .unwrap_err();
    assert!(visible.contains("Herdr"), "{visible}");
    assert_eq!(
        hosted_router.tool_authorities("ModifyTick"),
        Ok(BTreeSet::from([backends.host_authority.clone()]))
    );
    assert_eq!(
        backends.herdr.requests.load(Ordering::SeqCst),
        0,
        "routing never asks Herdr"
    );
}

#[tokio::test]
async fn artifact_shell_and_untyped_targets_never_reach_a_backend() {
    let backends = backends();
    let both = TickToolRouter::new(Some(backends.native.clone()), Some(backends.hosted.clone()));
    let hosted_only = TickToolRouter::new(None, Some(backends.hosted.clone()));
    let herdr_only = TickToolRouter::new(Some(backends.native.clone()), None);
    let chat = uuid::Uuid::new_v4();
    for router in [&both, &hosted_only, &herdr_only] {
        for (target, native_tool) in [
            ("artifact:doc", "mcp__artifacts__artifact_read"),
            ("shell:task", "TaskStop"),
        ] {
            for (name, input) in [
                ("ReadTick", json!({"tick_instance_id":target})),
                (
                    "CallTickTool",
                    json!({"tick_instance_id":target,"tool":"read","args":{}}),
                ),
                (
                    "ModifyTick",
                    json!({"action":"close","tick_instance_id":target}),
                ),
            ] {
                for error in [
                    router.call_authority(name, &input).unwrap_err(),
                    router
                        .prepare_call(chat, name, &input, CancellationToken::new())
                        .await
                        .unwrap_err(),
                ] {
                    assert!(error.contains("NativeToolRequired"), "{name}: {error}");
                    assert!(error.contains(native_tool), "{name}: {error}");
                }
            }
        }
    }
    for (router, supported, unsupported) in [
        (&both, vec!["herdr:", "hosted:"], vec![]),
        (&hosted_only, vec!["hosted:"], vec!["herdr:"]),
        (&herdr_only, vec!["herdr:"], vec!["hosted:"]),
    ] {
        let error = router
            .prepare_call(
                chat,
                "ReadTick",
                &json!({"tick_instance_id":"1234"}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.contains("InvalidTickTarget"), "{error}");
        for reference in supported {
            assert!(error.contains(reference), "{error}");
        }
        for reference in unsupported {
            assert!(!error.contains(reference), "{error}");
        }
    }
    let error = hosted_only
        .prepare_call(
            chat,
            "ReadTick",
            &json!({"tick_instance_id":"herdr:terminal:x:1:1"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.contains("Herdr"), "{error}");
    assert_eq!(backends.herdr.requests.load(Ordering::SeqCst), 0);
}

fn tool<'a>(definitions: &'a [tickloop_agent_protocol::ToolDefinition], name: &str) -> &'a Value {
    &definitions
        .iter()
        .find(|definition| definition.name == name)
        .unwrap_or_else(|| panic!("{name} is not defined"))
        .input_schema
}

fn variant<'a>(schema: &'a Value, action: &str) -> Option<&'a Value> {
    schema["oneOf"].as_array().unwrap().iter().find(|variant| {
        variant["properties"]["action"]["const"] == action
            || variant["properties"]["action"]["enum"]
                .as_array()
                .is_some_and(|actions| actions.contains(&json!(action)))
    })
}

fn placement_types(open: &Value) -> BTreeSet<String> {
    open["properties"]["placement"]["oneOf"]
        .as_array()
        .map(|variants| {
            variants
                .iter()
                .map(|variant| {
                    variant["properties"]["type"]["const"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn tick_tool_schemas_make_a_hidden_open_the_default() {
    let backends = backends();
    let strings = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();

    let both = TickToolRouter::new(Some(backends.native.clone()), Some(backends.hosted.clone()))
        .definitions();
    let modify = both.iter().find(|tool| tool.name == "ModifyTick").unwrap();
    assert!(
        modify.description.contains("hidden"),
        "{}",
        modify.description
    );
    assert!(
        modify.description.contains("Monitor"),
        "{}",
        modify.description
    );
    assert!(
        !modify.description.contains("creates a new Tab"),
        "{}",
        modify.description
    );
    let open = variant(tool(&both, "ModifyTick"), "open").unwrap();
    assert_eq!(open["required"], json!(["action", "command"]));
    assert_eq!(
        placement_types(open),
        strings(&["hidden", "current_tab", "pane", "new_tab"])
    );
    for action in ["move", "send_input", "close", "reopen"] {
        assert!(
            variant(tool(&both, "ModifyTick"), action).is_some(),
            "{action}"
        );
    }
    let read = both.iter().find(|tool| tool.name == "ReadTick").unwrap();
    assert!(read.description.contains("hosted:"), "{}", read.description);
    assert!(read.description.contains("herdr:"), "{}", read.description);

    let hosted = TickToolRouter::new(None, Some(backends.hosted.clone())).definitions();
    assert_eq!(
        hosted
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<BTreeSet<_>>(),
        strings(&["CallTickTool", "ModifyTick", "ReadTick"])
    );
    let open = variant(tool(&hosted, "ModifyTick"), "open").unwrap();
    assert_eq!(open["required"], json!(["action", "command"]));
    assert_eq!(placement_types(open), strings(&["hidden"]));
    for action in ["move", "send_input"] {
        assert!(
            variant(tool(&hosted, "ModifyTick"), action).is_none(),
            "{action}"
        );
    }
    assert!(variant(tool(&hosted, "ModifyTick"), "reopen").is_some());
    assert_eq!(hosted, backends.hosted.definitions());

    let herdr = TickToolRouter::new(Some(backends.native.clone()), None).definitions();
    assert!(herdr.iter().any(|tool| tool.name == "ReadWorkspace"));
    let open = variant(tool(&herdr, "ModifyTick"), "open").unwrap();
    assert_eq!(open["required"], json!(["action", "command", "placement"]));
    assert_eq!(
        placement_types(open),
        strings(&["current_tab", "pane", "new_tab"])
    );
    assert!(variant(tool(&herdr, "ModifyTick"), "reopen").is_none());
    assert_eq!(herdr, backends.native.definitions());
}

#[tokio::test]
async fn native_tick_tools_redirect_artifact_and_shell_ids_before_identity_or_host_resolution() {
    let directory = tempfile::tempdir().unwrap();
    let provider =
        NativeHostToolProvider::new(Arc::new(tickhost_connect::access::TickAccess::new(
            tickhost_connect::herdr::HerdrContext::without_view(
                directory.path().join("unused.sock"),
            ),
        )));
    for (instance, native_tool) in [
        ("artifact:doc", "mcp__artifacts__artifact_read"),
        (
            "artifact:https://docs.example/doc",
            "mcp__artifacts__artifact_read",
        ),
        ("shell:task", "TaskStop"),
    ] {
        for name in ["ReadTick", "CallTickTool"] {
            let input = if name == "ReadTick" {
                json!({"tick_instance_id":instance})
            } else {
                json!({"tick_instance_id":instance,"tool":"read","args":{}})
            };
            let error = provider
                .prepare_call(uuid::Uuid::new_v4(), name, &input, CancellationToken::new())
                .await
                .unwrap_err();
            assert!(error.contains("NativeToolRequired"), "{error}");
            assert!(error.contains(native_tool), "{error}");
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
async fn root_native_redirect_precedes_invalid_host_authority_resolution() {
    use tickloop_agent_protocol::EventKind;
    use tickloop_agent_runtime::Agent;
    let directory = tempfile::tempdir().unwrap();
    let endpoint = directory.path().join("not-a-socket");
    std::fs::write(&endpoint, "fixture").unwrap();
    let provider = Arc::new(NativeHostToolProvider::new(Arc::new(
        tickhost_connect::access::TickAccess::new(
            tickhost_connect::herdr::HerdrContext::without_view(endpoint),
        ),
    )));
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::call(
            "redirect",
            "CallTickTool",
            json!({"tick_instance_id":"artifact:doc","tool":"read","args":{}}),
        ),
        tickloop_agent_test_support::text("Done."),
    ]));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model);
    agent.install_host_tools(provider).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Inspect the artifact".into())
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
