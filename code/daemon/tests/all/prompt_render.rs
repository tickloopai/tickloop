//! Renders the first model requests of a new Chat for a few host setups and checks that the
//! prompt cache prefix, the system part and the listed tools, stays byte for byte the same from
//! one Run to the next and names none of the state that changes during a Chat. With
//! `PROMPT_RENDER_DIR` set it also writes one JSON file per setup, so the always-on model text
//! can be compared across versions. It sets process environment variables, so it runs in a
//! process of its own, as cargo-nextest runs every test:
//!
//! PROMPT_RENDER_DIR=<dir> cargo nextest run -p tickloop-code-daemon prompt_render

use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration};
use tick_packages::MANIFEST_FILE_NAME;
use tickloop_code_access::{EnvironmentValue, HostWorkContext, LaunchSpec};
use tickloop_code_local_environment::hosted::worker_control::WorkerControlGrant;
use tickloop_harness::{
    model::ModelRequest,
    protocol::{ChatId, Content, EventKind},
    runtime::Agent,
    test_support::{
        ScriptedModel, artifact_service::ArtifactService, harness::ChosenModels,
        mcp_service::McpService, text,
    },
};

const MULTIPLEXER_VARIABLES: [&str; 3] = ["HERDR_ENV", "TMUX", "ZELLIJ"];

#[derive(Clone, Copy, PartialEq)]
enum Multiplexer {
    Standalone,
    HerdrWithViews,
    HerdrWithoutViews,
    Tmux,
    Zellij,
}

struct Scenario {
    name: &'static str,
    multiplexer: Multiplexer,
    signed_in: bool,
    web_search: bool,
    mcp_servers: bool,
}

const SCENARIOS: [Scenario; 7] = [
    Scenario {
        name: "a-standalone",
        multiplexer: Multiplexer::Standalone,
        signed_in: true,
        web_search: true,
        mcp_servers: true,
    },
    Scenario {
        name: "b-herdr",
        multiplexer: Multiplexer::HerdrWithViews,
        signed_in: true,
        web_search: true,
        mcp_servers: true,
    },
    Scenario {
        name: "c-tmux",
        multiplexer: Multiplexer::Tmux,
        signed_in: true,
        web_search: true,
        mcp_servers: true,
    },
    Scenario {
        name: "d-signed-out-no-search",
        multiplexer: Multiplexer::Standalone,
        signed_in: false,
        web_search: false,
        mcp_servers: true,
    },
    Scenario {
        name: "e-no-mcp",
        multiplexer: Multiplexer::Standalone,
        signed_in: true,
        web_search: true,
        mcp_servers: false,
    },
    Scenario {
        name: "f-herdr-unavailable",
        multiplexer: Multiplexer::HerdrWithoutViews,
        signed_in: true,
        web_search: true,
        mcp_servers: true,
    },
    Scenario {
        name: "g-zellij",
        multiplexer: Multiplexer::Zellij,
        signed_in: true,
        web_search: true,
        mcp_servers: true,
    },
];

const LONGBRIDGE_INSTRUCTIONS: &str = "Longbridge MCP — market analysis and portfolio insight for US, HK, A-share, and SG markets. Provides live quotes, charts, order-book depth, fundamentals, valuations, analyst research, news, filings, screeners, watchlists, and read-only account, portfolio, and order history. Does not place, modify, or cancel orders.\n\nAll tools accept optional `_jq`, a filter expression using jq CLI syntax applied to the response; one output is returned as-is, several as a JSON array. Response shapes differ per tool and most tool descriptions say what they return. Omit it for the full JSON response.";

fn longbridge_tools() -> serde_json::Value {
    let tools = [
        ("quote", "Real-time quotes for one or more securities"),
        ("candlesticks", "Candlestick (K-line) data for a security"),
        ("depth", "Order-book depth for a security"),
        ("news", "Latest news for a security"),
        ("financial_report", "Financial statements for a company"),
        ("valuation", "Valuation metrics for a security"),
        ("watchlist", "The user's watchlist groups and securities"),
        ("stock_positions", "The account's stock positions"),
    ];
    json!(
        tools
            .iter()
            .map(|(name, description)| json!({
                "name": name,
                "description": description,
                "inputSchema": {"type":"object","properties":{"symbol":{"type":"string","description":"Security symbol such as 700.HK"},"_jq":{"type":"string"}}}
            }))
            .collect::<Vec<_>>()
    )
}

fn airtable_tools() -> serde_json::Value {
    let tools = [
        ("list_bases", "List the Airtable bases the user can access"),
        (
            "list_records",
            "List records in a table, optionally filtered by a formula",
        ),
        ("create_record", "Create a record in a table"),
        ("update_record", "Update fields of a record"),
    ];
    json!(
        tools
            .iter()
            .map(|(name, description)| json!({
                "name": name,
                "description": description,
                "inputSchema": {"type":"object","properties":{"base_id":{"type":"string"},"table":{"type":"string"},"fields":{"type":"object"}}}
            }))
            .collect::<Vec<_>>()
    )
}

fn install_plugin(root: &Path, name: &str, description: &str) {
    let source = root.join("sources").join(name);
    fs::create_dir_all(source.join("bin")).unwrap();
    fs::write(
        source.join(MANIFEST_FILE_NAME),
        json!({
            "name": name,
            "version": "1.0.0",
            "description": description,
            "executable": format!("bin/tick-{name}"),
            "protocol": true,
            "auth": {"type": "none"},
        })
        .to_string(),
    )
    .unwrap();
    let program = source.join("bin").join(format!("tick-{name}"));
    fs::write(&program, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let skill = source.join("skills").join("usage");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        format!(
            "---\nname: usage\ndescription: Open tick-{name} and pass its arguments\n---\nRun tick-{name} with the arguments the user asks for.\n"
        ),
    )
    .unwrap();
    tick_plugins::PluginStore::in_config_root(root)
        .install_path(&source)
        .unwrap();
}

fn set_multiplexer(multiplexer: Multiplexer) {
    for variable in MULTIPLEXER_VARIABLES {
        // SAFETY: cargo-nextest runs the render in a process of its own, and no other thread
        // reads the environment while it changes.
        unsafe { std::env::remove_var(variable) };
    }
    let variable = match multiplexer {
        Multiplexer::Standalone => return,
        Multiplexer::HerdrWithViews | Multiplexer::HerdrWithoutViews => ("HERDR_ENV", "1"),
        Multiplexer::Tmux => ("TMUX", "/tmp/tmux-501/default,1,0"),
        Multiplexer::Zellij => ("ZELLIJ", "0"),
    };
    // SAFETY: as above.
    unsafe { std::env::set_var(variable.0, variable.1) };
}

async fn complete(agent: &Agent, chat: ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    agent.submit(chat, prompt.into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("model run failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("model run timed out");
}

fn request_json(request: &ModelRequest) -> serde_json::Value {
    let tool = |tool: &tickloop_harness::protocol::ToolDefinition| json!({"name": tool.name, "description": tool.description, "input_schema": tool.input_schema});
    json!({
        "messages": request.messages.iter().map(|message| json!({
            "role": format!("{:?}", message.role),
            "text": message.content.iter().filter_map(|block| match block {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            }).collect::<Vec<_>>().join("\n"),
        })).collect::<Vec<_>>(),
        "tools": request.tools.iter().map(tool).collect::<Vec<_>>(),
        "deferred_tools": request.deferred_tools.iter().map(tool).collect::<Vec<_>>(),
    })
}

/// Host state that changes while a Chat runs, which the system prompt must never name.
const CHANGING_HOST_STATE: [&str; 10] = [
    "service_connections",
    "artifact_ticks",
    "family_members",
    "\"loop\"",
    "\"plan\"",
    "uncertain_operations",
    "execution_mode",
    "web_search",
    "last_request_at_ms",
    "\"login\"",
];

async fn render(scenario: &Scenario, output: Option<&Path>) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let folder = root.join("project");
    fs::create_dir_all(&folder).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&folder)
            .status()
            .unwrap()
            .success()
    );
    tickloop_harness::base::credentials::save_api_key(&root, "ANTHROPIC_API_KEY", "fixture")
        .unwrap();
    let longbridge = McpService::start().await;
    longbridge.set_instructions(LONGBRIDGE_INSTRUCTIONS);
    longbridge.set_tools(longbridge_tools());
    let airtable = McpService::start().await;
    airtable.set_instructions("");
    airtable.set_tools(airtable_tools());
    let mut settings = json!({});
    if scenario.mcp_servers {
        settings["mcpServers"] = json!({
            "longbridge": {"exposure": "search", "url": longbridge.url()},
            "airtable": {"exposure": "direct", "url": airtable.url()},
        });
    }
    if !scenario.web_search {
        settings["webSearch"] = serde_json::Value::Null;
    }
    fs::write(
        root.join("settings.json"),
        serde_json::to_vec_pretty(&settings).unwrap(),
    )
    .unwrap();
    install_plugin(&root, "quotes", "Market quotes as a Tick");
    install_plugin(&root, "notes", "Team notes as a Tick");
    let artifacts = ArtifactService::start().await;
    // SAFETY: as in `set_multiplexer`.
    unsafe { std::env::set_var("TICKLOOP_ARTIFACT_ORIGIN", &artifacts.origin) };
    if scenario.signed_in {
        artifacts.login(&root).await;
    }
    set_multiplexer(scenario.multiplexer);
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    // SAFETY: as in `set_multiplexer`. An empty home keeps the developer's own instruction files
    // out of the rendered request.
    unsafe { std::env::set_var("HOME", &home) };

    let model = tickloop_harness::model::cards::ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-5")
        .unwrap();
    let mut launch = LaunchSpec::embedded(folder.clone());
    launch.config_root = Some(root.clone());
    launch.model = Some(model);
    launch.session_persistence = false;
    launch.environment = std::env::vars()
        .map(|(name, value)| EnvironmentValue {
            name: name.into_bytes(),
            value: value.into_bytes(),
        })
        .collect();
    if scenario.multiplexer == Multiplexer::HerdrWithViews {
        launch.host_work_context = Some(HostWorkContext {
            socket: root.join("herdr.sock"),
            workspace: None,
        });
    }
    let control = WorkerControlGrant {
        endpoint: root.join("unused-daemon.sock"),
        daemon_epoch: uuid::Uuid::new_v4(),
        binding: uuid::Uuid::new_v4(),
        capability: "unused".into(),
        host_endpoint: root.join("unused-host.sock"),
    };
    let scripted = Arc::new(ScriptedModel::new(Vec::new()));
    let (agent, _) = tickloop_code_daemon::build_runtime_with_models(
        &launch,
        ChosenModels::new(scripted.clone()),
        Some(control),
        &std::env::current_exe().unwrap(),
    )
    .await
    .unwrap();

    // A first Chat waits until every configured service is connected, so the rendered Chat
    // starts in the settled state a user normally sees.
    let warmup = agent.create_chat(folder.clone()).await.unwrap();
    let expected_mcp_tool = scenario.mcp_servers.then_some("mcp__airtable__list_bases");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            scripted.enqueue(text("ready"));
            complete(&agent, warmup, "warm up").await;
            let settled = {
                let requests = scripted.requests.lock().unwrap();
                let names = requests
                    .last()
                    .unwrap()
                    .tools
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect::<Vec<_>>();
                let artifacts_ready =
                    names.iter().any(|name| name == "ArtifactRead") == scenario.signed_in;
                let mcp_ready =
                    expected_mcp_tool.is_none_or(|tool| names.iter().any(|name| name == tool));
                artifacts_ready && mcp_ready
            };
            if settled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("services did not settle");

    let chat = agent.create_chat(folder.clone()).await.unwrap();
    let before = scripted.requests.lock().unwrap().len();
    scripted.enqueue(text("Hello!"));
    complete(&agent, chat, "hi").await;
    let mut rendered = {
        let requests = scripted.requests.lock().unwrap();
        let first = requests[before..]
            .iter()
            .find(|request| request.tools.iter().any(|tool| tool.name == "Bash"))
            .expect("the Chat's first conversation request");
        request_json(first)
    };
    // A second turn shows whether the system part, the start of the prompt cache prefix, stays
    // the same across the Chat's Runs.
    let before = scripted.requests.lock().unwrap().len();
    scripted.enqueue(text("You're welcome."));
    complete(&agent, chat, "thanks").await;
    let second = request_json(&scripted.requests.lock().unwrap()[before]);
    rendered["second_run_system"] = second["messages"][0]["text"].clone();
    agent.shutdown().await;
    assert_eq!(
        second["messages"][0], rendered["messages"][0],
        "{}: the system prompt changed between Runs",
        scenario.name
    );
    assert_eq!(
        second["tools"], rendered["tools"],
        "{}: the listed tools changed between Runs",
        scenario.name
    );
    let system = rendered["messages"][0]["text"].as_str().unwrap();
    for changing in CHANGING_HOST_STATE {
        assert!(
            !system.contains(changing),
            "{}: the system prompt names {changing}",
            scenario.name
        );
    }
    let Some(output) = output else {
        return;
    };
    fs::write(
        output.join(format!("{}.json", scenario.name)),
        serde_json::to_vec_pretty(&rendered).unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_render() {
    let output = std::env::var("PROMPT_RENDER_DIR")
        .ok()
        .map(std::path::PathBuf::from);
    if let Some(output) = &output {
        fs::create_dir_all(output).unwrap();
    }
    for scenario in &SCENARIOS {
        render(scenario, output.as_deref()).await;
    }
}
