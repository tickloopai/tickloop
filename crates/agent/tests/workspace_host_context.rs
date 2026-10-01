use std::{process::Command, sync::Arc};

use async_trait::async_trait;
use serde_json::Value;
use tick_connect::{
    access::TickAccess,
    herdr::{HerdrContext, HerdrPort, HostError},
};
use tickloop_agent::{
    model::{Content, Role},
    native_host::NativeHostToolProvider,
    runtime::{Agent, EventKind},
    skills::SkillSource,
};
use tokio_util::sync::CancellationToken;

mod support;
use support::{ScriptedModel, config, text};

struct UnusedHerdrPort;

#[async_trait]
impl HerdrPort for UnusedHerdrPort {
    async fn request(&self, _: &str, _: Value, _: CancellationToken) -> Result<Value, HostError> {
        panic!("host context must be available before any workspace tool call")
    }
}

#[test]
fn first_model_request_identifies_host_without_advertising_missing_integrations() {
    let binary = std::env::current_exe().unwrap();
    for (case, host, integration) in [
        ("herdr", "herdr", "available"),
        ("herdr_without_provider", "herdr", "unavailable"),
        ("tmux", "tmux", "unavailable"),
        ("zellij", "zellij", "unavailable"),
        ("standalone", "standalone", "unavailable"),
    ] {
        let mut child = Command::new(&binary);
        child
            .arg("--exact")
            .arg("workspace_host_request_probe")
            .arg("--nocapture")
            .env("TICKLOOP_WORKSPACE_HOST_PROBE", case)
            .env_remove("HERDR_ENV")
            .env_remove("TMUX")
            .env_remove("ZELLIJ");
        match case {
            "herdr" | "herdr_without_provider" => {
                child.env("HERDR_ENV", "1");
                child.env("TMUX", "/private/tmux.sock");
                child.env("ZELLIJ", "0");
            }
            "tmux" => {
                child.env("TMUX", "/private/tmux.sock");
                child.env("ZELLIJ", "0");
            }
            "zellij" => {
                child.env("ZELLIJ", "0");
            }
            "standalone" => {}
            _ => unreachable!(),
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let state = stdout
            .lines()
            .find_map(|line| {
                line.split_once("WORKSPACE_HOST_STATE:")
                    .map(|(_, state)| state)
            })
            .unwrap_or_else(|| panic!("{case}: missing model request state in {stdout}"));
        let state: Value = serde_json::from_str(state).unwrap();
        assert_eq!(state["workspace_host"]["name"], host, "{case}");
        assert_eq!(
            state["workspace_host"]["integration"], integration,
            "{case}"
        );
        assert!(state["workspace_host"]["tick_executable"].is_null() || case == "herdr");
        assert!(!state.to_string().contains("/private/tmux.sock"));
    }
}

#[tokio::test]
async fn workspace_host_request_probe() {
    let Ok(case) = std::env::var("TICKLOOP_WORKSPACE_HOST_PROBE") else {
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let state_root = directory.path().join("state");
    let model = Arc::new(ScriptedModel::new(vec![
        text("Ready."),
        text("Still ready."),
    ]));
    let agent = Agent::open(state_root.clone(), config(), model.clone()).unwrap();
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    if case == "herdr" {
        let access = Arc::new(TickAccess::with_host(
            HerdrContext {
                socket: "/private/unused-herdr.sock".into(),
                caller_pane: "agent".into(),
            },
            Arc::new(UnusedHerdrPort),
        ));
        agent
            .install_host_tools(Arc::new(NativeHostToolProvider::new(access)))
            .unwrap();
    }
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    for input in ["Check host", "Check host again"] {
        let receipt = agent.submit(chat, None, input.into()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.run_id == receipt.run_id
                    && matches!(event.kind, EventKind::Completed { .. })
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    if case == "herdr" {
        assert_eq!(snapshot.invoked_skills.len(), 1);
        assert_eq!(snapshot.invoked_skills[0].name, "tickloop");
        assert_eq!(snapshot.invoked_skills[0].source, SkillSource::BuiltIn);
    } else {
        assert!(snapshot.invoked_skills.is_empty());
    }
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            let active_skill = request
                .messages
                .iter()
                .filter_map(|message| match message.content.as_slice() {
                    [Content::Text { text }]
                        if text.starts_with("The `tickloop` Skill is active for this Chat:") =>
                    {
                        Some(text)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if case == "herdr" {
                assert_eq!(active_skill.len(), 1);
                assert!(active_skill[0].contains(&snapshot.invoked_skills[0].content));
            } else {
                assert!(active_skill.is_empty());
            }
        }
    }
    agent.shutdown().await;
    drop(agent);
    let restored = Agent::open(
        state_root,
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let persisted = restored.resume_chat(chat).await.unwrap();
    assert_eq!(persisted.invoked_skills, snapshot.invoked_skills);

    let requests = model.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(
        request
            .tools
            .iter()
            .any(|tool| tool.name == "ReadWorkspace"),
        case == "herdr"
    );
    let first = &request.messages[0];
    assert_eq!(first.role, Role::System);
    let Content::Text { text } = &first.content[0] else {
        panic!("first model message must be system text");
    };
    let state = text.rsplit_once("Current host state: ").unwrap().1;
    let state: Value = serde_json::from_str(state).unwrap();
    println!("WORKSPACE_HOST_STATE:{state}");
}
