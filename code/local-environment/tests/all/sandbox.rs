#![cfg(target_os = "macos")]

use std::sync::Arc;
use tickloop_code_local_environment::{
    environment::LocalEnvironment,
    hosted::shells::{OwnTickhost, ShellGrants},
};
use tickloop_code_settings::UserSettings;
use tickloop_harness::{
    protocol::{ChatId, EventKind, ExecutionMode, ToolResult},
    runtime::{Agent, HarnessOptions, HarnessSeams, environment::*},
    test_support::{ScriptedModel, call, config, harness::ChosenModels, next_event, text},
};
use tokio_util::sync::CancellationToken;

#[path = "sandbox/defaults.rs"]
mod defaults;
#[path = "sandbox/network.rs"]
mod network;
#[path = "sandbox/review_fixes.rs"]
mod review_fixes;

#[tokio::test]
async fn sandbox_blocks_outside_writes_and_reports_folder_isolation() {
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let root = &fixture.root;
    let folder = fixture.workspace.folder.as_ref().unwrap();
    let environment = &fixture.environment;
    let authority = ExecutionAuthority::new(tickloop_ids::generate().into());
    let port = environment.bind(
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: None,
            additional_directories: Vec::new(),
        },
        authority.clone(),
    );
    assert!(port.is_err());
    let chat = tickloop_ids::generate().into();
    let authority = ExecutionAuthority::new(chat);
    let port = environment
        .bind(
            ExecutionScope {
                chat,
                folder: Some(folder.display().to_string()),
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::SpawnShell(ShellSpec {
            folder: Some(folder.display().to_string()),
            dangerously_disable_sandbox: false,
            additional_directories: Vec::new(),
            family_temp: None,
            operation: tickloop_ids::generate(),
            shell: "/bin/sh".into(),
            command: "printf allowed > inside; printf denied > ../outside".into(),
            cwd: folder.display().to_string(),
            output: root.path().join("output").display().to_string(),
            description: None,
        }))
        .await
        .unwrap();
    assert_eq!(
        prepared.isolation(),
        ExecutionIsolation {
            writes: WriteIsolation::FolderAndDirectories,
            network: NetworkIsolation::Disconnected
        }
    );
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(result) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("not dispatched")
    };
    let instance = result.shell.unwrap().instance;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let ExecutionInspection::Shell(status) = port
                .inspect(ExecutionQuery::ShellStatus(instance.clone()))
                .await
                .unwrap()
            else {
                panic!("no status")
            };
            if status.state != tickloop_harness::protocol::CommandState::Running {
                assert_eq!(
                    status.state,
                    tickloop_harness::protocol::CommandState::Failed
                );
                assert_ne!(status.exit_code, Some(0));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let ExecutionInspection::Output(output) = port
        .inspect(ExecutionQuery::ShellOutput {
            instance,
            offset: 0,
            limit: 4096,
        })
        .await
        .unwrap()
    else {
        panic!("no output")
    };
    assert!(String::from_utf8_lossy(&output.data).contains("被沙箱拦截"));
    assert_eq!(
        std::fs::read_to_string(folder.join("inside")).unwrap(),
        "allowed"
    );
    assert!(!root.path().join("outside").exists());
}

struct SandboxFixture {
    root: tempfile::TempDir,
    settings: UserSettings,
    environment: LocalEnvironment,
    workspace: tickloop_harness::tools::Workspace,
    tools: tickloop_harness::tools::Tools,
}

impl SandboxFixture {
    fn new(sandbox: serde_json::Value) -> Self {
        use tickloop_harness::{
            base::private_fs::PrivateDirectory,
            tools::{ChatDirectory, Tools, Workspace},
        };
        let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let root = tempfile::tempdir_in(scratch.canonicalize().unwrap()).unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let folder = root.path().join("folder");
        let config = root.path().join("config");
        std::fs::create_dir(&folder).unwrap();
        std::fs::create_dir(&config).unwrap();
        std::fs::write(
            config.join("settings.json"),
            serde_json::to_vec(&serde_json::json!({"sandbox":sandbox})).unwrap(),
        )
        .unwrap();
        let settings = UserSettings::in_root(&config);
        let environment = LocalEnvironment::with_shells(Arc::new(OwnTickhost::new()))
            .with_sandbox(settings.clone());
        let family = PrivateDirectory::trusted(root.path().join("family"));
        let tools = Tools::for_chat(
            ChatDirectory {
                chat: family.clone(),
                task_output: family.join("tasks"),
                family_temp: family,
            },
            environment
                .shell_tasks()
                .unwrap()
                .for_chat(tickloop_ids::generate()),
        );
        Self {
            root,
            settings,
            environment,
            workspace: Workspace {
                folder: Some(folder),
                additional_directories: vec![],
            },
            tools,
        }
    }

    async fn agent(&self, input: serde_json::Value, mode: ExecutionMode) -> (Agent, ChatId) {
        let model = Arc::new(ScriptedModel::new(vec![
            call("tested", "Bash", input),
            text("done"),
        ]));
        self.agent_with_model(model, config(), mode).await
    }

    async fn agent_with_model(
        &self,
        model: Arc<dyn tickloop_harness::model::ModelClient>,
        model_config: tickloop_harness::protocol::ModelConfig,
        mode: ExecutionMode,
    ) -> (Agent, ChatId) {
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(Arc::new(self.environment.clone())),
                models: ChosenModels::new(model),
            },
            HarnessOptions {
                settings: Some(Arc::new(self.settings.clone())),
                ..HarnessOptions::local(model_config)
            },
        )
        .unwrap();
        let chat = agent
            .create_chat(self.workspace.folder.clone().unwrap())
            .await
            .unwrap();
        agent.set_execution_mode(chat, mode).await.unwrap();
        (agent, chat)
    }

    async fn run_with_human_approval(
        &self,
        agent: &Agent,
        chat: ChatId,
        allow: bool,
    ) -> (bool, ToolResult) {
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(
                chat,
                "Run the requested local command after confirmation.".into(),
            )
            .await
            .unwrap();
        let mut asked = false;
        let mut result = None;
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    assert!(!self.root.path().join("outside").exists());
                    asked = true;
                    agent
                        .answer_permission(chat, request.id, allow)
                        .await
                        .unwrap();
                }
                EventKind::ToolCompleted { result: value, .. } => result = Some(value),
                EventKind::Completed { .. } | EventKind::Interrupted => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        agent.shutdown().await;
        (asked, result.expect("Bash returns a result"))
    }

    async fn prepare(&self, command: &str) -> (tickloop_harness::tools::PreparedTool, uuid::Uuid) {
        let operation = tickloop_ids::generate();
        let prepared = self
            .tools
            .prepare_for_operation(
                &tickloop_ids::IdContext::new(),
                &self.workspace,
                "Bash",
                &serde_json::json!({"command":command}),
                operation,
            )
            .await
            .unwrap();
        (prepared, operation)
    }

    async fn run(
        &self,
        (prepared, operation): (tickloop_harness::tools::PreparedTool, uuid::Uuid),
    ) -> tickloop_harness::protocol::ToolResult {
        self.tools
            .execute_with_context(
                prepared,
                CancellationToken::new(),
                tickloop_harness::tools::ExecutionContext {
                    ids: tickloop_ids::IdContext::new(),
                    operation_id: operation,
                    loop_id: tickloop_ids::generate(),
                    accepts_images: false,
                },
                &NoSecondaryModel,
            )
            .await
            .result
            .unwrap()
    }
}

struct NoSecondaryModel;

#[async_trait::async_trait]
impl tickloop_harness::tools::SecondaryModel for NoSecondaryModel {
    async fn respond(
        &self,
        _: String,
        _: CancellationToken,
    ) -> Result<String, tickloop_harness::tools::ToolError> {
        panic!("no model needed")
    }
}

#[tokio::test]
async fn unsandboxed_bash_writes_outside_only_after_human_approval() {
    for allow in [false, true] {
        let fixture =
            SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
        let (agent, chat) = fixture
            .agent(
                serde_json::json!({
                    "command":"printf approved > ../outside", "dangerouslyDisableSandbox":true
                }),
                ExecutionMode::AcceptEdits,
            )
            .await;
        let (asked, result) = fixture.run_with_human_approval(&agent, chat, allow).await;
        assert!(asked, "{result:?}");
        assert_eq!(fixture.root.path().join("outside").exists(), allow);
        if allow {
            assert!(!result.is_error);
            assert_eq!(
                std::fs::read_to_string(fixture.root.path().join("outside")).unwrap(),
                "approved"
            );
        }
    }
}

#[tokio::test]
async fn unsandboxed_settings_control_explicit_and_excluded_commands() {
    for (settings, requested, outside) in [
        (
            serde_json::json!({"enabled":true,"failIfUnavailable":true,"allowUnsandboxedCommands":false}),
            true,
            false,
        ),
        (
            serde_json::json!({"enabled":true,"failIfUnavailable":true,"excludedCommands":["printf approved > ../outside"]}),
            false,
            true,
        ),
        (
            serde_json::json!({"enabled":true,"failIfUnavailable":true,"excludedCommands":["printf approved > ../outside"],"allowUnsandboxedCommands":false}),
            false,
            true,
        ),
        (
            serde_json::json!({"enabled":true,"failIfUnavailable":true,"excludedCommands":["unrelated:* "]}),
            false,
            false,
        ),
        (
            serde_json::json!({"enabled":false,"allowUnsandboxedCommands":false}),
            true,
            true,
        ),
    ] {
        let fixture = SandboxFixture::new(settings.clone());
        let (agent, chat) = fixture
            .agent(
                serde_json::json!({
                    "command":"printf approved > ../outside", "dangerouslyDisableSandbox":requested
                }),
                ExecutionMode::AcceptEdits,
            )
            .await;
        let (asked, result) = fixture.run_with_human_approval(&agent, chat, true).await;
        // Calls kept in the sandbox run without approval; only unsandboxed ones ask.
        assert_eq!(asked, outside, "settings={settings}, result={result:?}");
        assert_eq!(
            fixture.root.path().join("outside").exists(),
            outside,
            "settings={settings}, result={result:?}"
        );
        if outside {
            assert!(!result.is_error);
            assert_eq!(
                std::fs::read_to_string(fixture.root.path().join("outside")).unwrap(),
                "approved"
            );
        }
    }
}

#[tokio::test]
async fn sandbox_uses_chat_additional_directories_and_family_scratchpad() {
    let mut fixture =
        SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let additional = fixture.root.path().join("additional");
    std::fs::create_dir(&additional).unwrap();
    fixture
        .workspace
        .additional_directories
        .push(additional.clone());
    let scratch = fixture.root.path().join("family/scratchpad/note");
    let prepared = fixture
        .prepare(&format!(
            "printf additional > '{}'; printf scratch > '{}'",
            additional.join("note").display(),
            scratch.display()
        ))
        .await;
    let result = fixture.run(prepared).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(additional.join("note")).unwrap(),
        "additional"
    );
    assert_eq!(std::fs::read_to_string(scratch).unwrap(), "scratch");
}

#[tokio::test]
async fn sandbox_denies_sensitive_paths_even_inside_explicitly_writable_roots() {
    let fixture = SandboxFixture::new(
        serde_json::json!({"enabled":true,"failIfUnavailable":true,"filesystem":{"allowWrite":["../cache"],"denyWrite":["../cache/blocked"]}}),
    );
    let cache = fixture.root.path().join("cache");
    std::fs::create_dir(&cache).unwrap();
    let allowed = fixture
        .run(fixture.prepare("printf allowed > ../cache/note").await)
        .await;
    assert!(!allowed.is_error, "{}", allowed.content);
    for relative in [
        "../cache/blocked",
        "../cache/BLOCKED",
        ".git/config",
        ".bashrc",
        ".ssh/id_rsa",
        ".env",
        ".ENV.local",
        "api_TOKEN.txt",
        "client.PEM",
        "../config/settings.json",
    ] {
        let path = fixture.workspace.folder.as_ref().unwrap().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let before = std::fs::read(&path).ok();
        let result = fixture
            .run(
                fixture
                    .prepare(&format!("printf forbidden > '{relative}'"))
                    .await,
            )
            .await;
        assert!(result.is_error, "{relative}: {}", result.content);
        assert!(result.content.contains("被沙箱拦截"), "{}", result.content);
        assert_eq!(std::fs::read(&path).ok(), before, "{relative}");
    }
}

#[tokio::test]
async fn sandbox_disconnects_network_and_keeps_the_prepared_policy_after_settings_change() {
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let command = format!(
        "/usr/bin/curl --noproxy '*' --connect-timeout 1 http://127.0.0.1:{}/",
        listener.local_addr().unwrap().port()
    );
    let prepared = fixture.prepare(&command).await;
    assert_eq!(
        prepared.0.approval_facts().isolation.unwrap().network,
        NetworkIsolation::Disconnected
    );
    std::fs::write(
        fixture.settings.root().unwrap().join("settings.json"),
        r#"{"sandbox":{"enabled":false}}"#,
    )
    .unwrap();
    let result = fixture.run(prepared).await;
    assert!(result.is_error, "{}", result.content);
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let prepared = fixture
        .prepare("printf outside > ../outside; printf 'permission denied' >&2; exit 7")
        .await;
    assert_eq!(
        prepared.0.approval_facts().isolation,
        Some(ExecutionIsolation::default())
    );
    let result = fixture.run(prepared).await;
    assert!(result.is_error);
    assert!(result.content.contains("Exit code 7"));
    assert!(!result.content.contains("被沙箱拦截"));
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("outside")).unwrap(),
        "outside"
    );
}

#[tokio::test]
async fn sandbox_unavailable_in_nested_host_warns_or_refuses() {
    let output = tokio::process::Command::new("/usr/bin/sandbox-exec")
        .args(["-p", "(version 1)(allow default)"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sandbox::nested_sandbox_unavailability_contract",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "invoked by sandbox_unavailable_in_nested_host_warns_or_refuses inside an OS sandbox"]
async fn nested_sandbox_unavailability_contract() {
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true}));
    let prepared = fixture.prepare("printf fallback > ../outside").await;
    assert_eq!(
        prepared.0.approval_facts().isolation,
        Some(ExecutionIsolation::default())
    );
    let result = fixture.run(prepared).await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("running without isolation"));
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("outside")).unwrap(),
        "fallback"
    );
    std::fs::write(
        fixture.settings.root().unwrap().join("settings.json"),
        r#"{"sandbox":{"enabled":true,"failIfUnavailable":true}}"#,
    )
    .unwrap();
    let error = fixture
        .tools
        .prepare_for_operation(
            &tickloop_ids::IdContext::new(),
            &fixture.workspace,
            "Bash",
            &serde_json::json!({"command":"printf refused > ../refused"}),
            tickloop_ids::generate(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("SandboxUnavailable"), "{error}");
    assert!(!fixture.root.path().join("refused").exists());
}

#[tokio::test]
async fn sandbox_and_reference_hosts_share_the_frozen_isolation_contract() {
    use tickloop_harness::test_support::{
        execution_contracts::isolation_contract, execution_environment::MemoryEnvironment,
    };
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let scope = ExecutionScope {
        chat: tickloop_ids::generate().into(),
        folder: Some(
            fixture
                .workspace
                .folder
                .as_ref()
                .unwrap()
                .display()
                .to_string(),
        ),
        additional_directories: Vec::new(),
    };
    let spec = ShellSpec {
        folder: scope.folder.clone(),
        dangerously_disable_sandbox: false,
        additional_directories: Vec::new(),
        family_temp: None,
        operation: tickloop_ids::generate(),
        shell: "/bin/sh".into(),
        command: "true".into(),
        cwd: scope.folder.clone().unwrap(),
        output: fixture.root.path().join("output").display().to_string(),
        description: None,
    };
    isolation_contract(
        &fixture.environment,
        scope.clone(),
        spec.clone(),
        ExecutionIsolation {
            writes: WriteIsolation::FolderAndDirectories,
            network: NetworkIsolation::Disconnected,
        },
        || {
            std::fs::write(
                fixture.settings.root().unwrap().join("settings.json"),
                r#"{"sandbox":{"enabled":false}}"#,
            )
            .unwrap();
        },
    )
    .await;
    for writes in [
        WriteIsolation::Unrestricted,
        WriteIsolation::FolderAndDirectories,
        WriteIsolation::Environment,
    ] {
        for network in [
            NetworkIsolation::Unrestricted,
            NetworkIsolation::Allowlist,
            NetworkIsolation::Disconnected,
        ] {
            let memory = MemoryEnvironment::new();
            memory.define_shell("true", std::time::Duration::ZERO, Vec::new(), 0);
            let expected = ExecutionIsolation { writes, network };
            memory.set_isolation(expected);
            isolation_contract(&memory, scope.clone(), spec.clone(), expected, || {
                memory.set_isolation(ExecutionIsolation::default())
            })
            .await;
        }
    }
}

#[tokio::test]
async fn background_shell_keeps_its_sandbox_until_exit() {
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let operation = tickloop_ids::generate();
    let mut updates = fixture.tools.subscribe_commands();
    let prepared = fixture.tools.prepare_for_operation(&tickloop_ids::IdContext::new(), &fixture.workspace, "Bash", &serde_json::json!({
        "command":"while [ ! -f release ]; do /bin/sleep 0.01; done; printf denied > ../outside",
        "run_in_background":true
    }), operation).await.unwrap();
    let result = fixture.run((prepared, operation)).await;
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("running in background"));
    std::fs::write(
        fixture.workspace.folder.as_ref().unwrap().join("release"),
        "release",
    )
    .unwrap();
    let ended = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let update = updates.recv().await.unwrap();
            if update.operation_id == operation
                && update.state == tickloop_harness::protocol::CommandState::Failed
            {
                break update;
            }
        }
    })
    .await
    .unwrap();
    assert_ne!(ended.exit_code, Some(0));
    assert!(!fixture.root.path().join("outside").exists());
    let output = fixture
        .tools
        .read_command(operation, 0, 4096)
        .await
        .unwrap();
    assert!(output.content.contains("被沙箱拦截"), "{}", output.content);
}

#[tokio::test]
async fn sandbox_protects_denied_paths_and_user_settings_through_symlinks() {
    let fixture = SandboxFixture::new(
        serde_json::json!({"enabled":true,"failIfUnavailable":true,"filesystem":{"denyWrite":["link/protected"]}}),
    );
    let folder = fixture.workspace.folder.as_ref().unwrap();
    for name in ["original", "replacement"] {
        std::fs::create_dir(folder.join(name)).unwrap();
    }
    std::os::unix::fs::symlink("original", folder.join("link")).unwrap();
    let result = fixture
        .run(
            fixture
                .prepare("rm link; ln -s replacement link; printf bypass > link/protected")
                .await,
        )
        .await;
    assert!(result.is_error, "{}", result.content);
    assert!(!folder.join("replacement/protected").exists());
    let settings = fixture.settings.root().unwrap().join("settings.json");
    let target = folder.join("host-config");
    std::fs::rename(&settings, &target).unwrap();
    std::os::unix::fs::symlink(&target, &settings).unwrap();
    let before = std::fs::read(&target).unwrap();
    let result = fixture
        .run(fixture.prepare("printf bypass > host-config").await)
        .await;
    assert!(result.is_error, "{}", result.content);
    assert_eq!(std::fs::read(&target).unwrap(), before);
}

#[tokio::test]
async fn sandbox_blocks_sensitive_symlink_traversal() {
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let folder = fixture.workspace.folder.as_ref().unwrap();
    std::fs::create_dir(folder.join("ordinary")).unwrap();
    std::os::unix::fs::symlink("ordinary", folder.join(".git")).unwrap();
    let result = fixture
        .run(fixture.prepare("printf bypass > .git/config").await)
        .await;
    assert!(result.is_error, "{}", result.content);
    assert!(!folder.join("ordinary/config").exists());
}

#[tokio::test]
async fn sandbox_does_not_treat_the_folder_name_as_a_sensitive_child_path() {
    let mut fixture =
        SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let folder = fixture.root.path().join("tokenizer");
    std::fs::create_dir(&folder).unwrap();
    fixture.workspace.folder = Some(folder.clone());
    let result = fixture
        .run(fixture.prepare("printf allowed > ordinary.txt").await)
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(folder.join("ordinary.txt")).unwrap(),
        "allowed"
    );
}

#[tokio::test]
async fn sandbox_blocks_sensitive_symlinks_reached_through_an_ordinary_bridge() {
    let fixture = SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let folder = fixture.workspace.folder.as_ref().unwrap();
    let outside = fixture.root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::create_dir(folder.join("ordinary")).unwrap();
    std::os::unix::fs::symlink(&outside, folder.join("bridge")).unwrap();
    std::os::unix::fs::symlink(folder.join("ordinary"), outside.join(".git")).unwrap();
    let result = fixture
        .run(fixture.prepare("printf bypass > bridge/.git/config").await)
        .await;
    assert!(result.is_error, "{}", result.content);
    assert!(!folder.join("ordinary/config").exists());
}

#[tokio::test]
async fn sandbox_ignores_sensitive_ancestors_above_a_folder_inside_the_temp_root() {
    let mut fixture =
        SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    let temporary = tempfile::tempdir().unwrap();
    let folder = temporary.path().join("secret-parent/project");
    std::fs::create_dir_all(folder.join("ordinary")).unwrap();
    std::os::unix::fs::symlink("ordinary", folder.join("link")).unwrap();
    fixture.workspace.folder = Some(folder.clone());
    let result = fixture
        .run(fixture.prepare("printf allowed > link/note").await)
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(folder.join("ordinary/note")).unwrap(),
        "allowed"
    );
}

#[tokio::test]
async fn sandbox_denied_paths_cannot_gain_a_new_symlink_ancestor_after_preparation() {
    let fixture = SandboxFixture::new(
        serde_json::json!({"enabled":true,"failIfUnavailable":true,"filesystem":{"denyWrite":["missing/protected"]}}),
    );
    let folder = fixture.workspace.folder.as_ref().unwrap();
    std::fs::create_dir(folder.join("ordinary")).unwrap();
    let result = fixture
        .run(
            fixture
                .prepare("ln -s ordinary missing; printf bypass > missing/protected")
                .await,
        )
        .await;
    assert!(result.is_error, "{}", result.content);
    assert!(!folder.join("ordinary/protected").exists());
}

#[tokio::test]
async fn unsandboxed_auto_calls_are_reviewed_even_for_normally_safe_commands() {
    use tickloop_harness::{protocol::ApprovalOutcome, test_support::auto_classification};
    for excluded in [false, true] {
        for command in ["pwd", "mkdir reviewed"] {
            for block in [false, true] {
                let fixture = SandboxFixture::new(serde_json::json!({
                    "enabled":true, "failIfUnavailable":true,
                    "allowUnsandboxedCommands": !excluded,
                    "excludedCommands": if excluded {vec![command]} else {vec![]}
                }));
                let (agent, chat) = fixture
                    .agent(
                        serde_json::json!({
                            "command":command, "dangerouslyDisableSandbox":!excluded
                        }),
                        ExecutionMode::Auto,
                    )
                    .await;
                let reviewer = Arc::new(ScriptedModel::new(vec![
                    text(&auto_classification(block).to_string()),
                    text(&auto_classification(block).to_string()),
                ]));
                agent.set_auto_reviewer(reviewer.clone()).await;
                let mut events = agent.subscribe();
                let receipt = agent
                    .submit(
                        chat,
                        if block {
                            "Do not run this command."
                        } else {
                            "Run this routine local command."
                        }
                        .into(),
                    )
                    .await
                    .unwrap();
                let mut assessment = None;
                let mut result = None;
                loop {
                    let event = next_event(&mut events).await;
                    if event.run_id != receipt.run_id {
                        continue;
                    }
                    match event.kind {
                        EventKind::ApprovalAssessed { assessment: value } => {
                            assessment = Some(value)
                        }
                        EventKind::PermissionRequested { .. } => {
                            panic!("Auto must use its reviewer")
                        }
                        EventKind::ToolCompleted { result: value, .. } => result = Some(value),
                        EventKind::Completed { .. } | EventKind::Interrupted => break,
                        EventKind::Failed(error) => panic!("{error}"),
                        _ => {}
                    }
                }
                agent.shutdown().await;
                let assessment = assessment.unwrap_or_else(|| {
                    panic!(
                        "not reviewed: excluded={excluded}, command={command}, result={result:?}"
                    )
                });
                assert_eq!(
                    assessment.outcome,
                    if block {
                        ApprovalOutcome::Block
                    } else {
                        ApprovalOutcome::Allow
                    }
                );
                assert_eq!(result.unwrap().is_error, block);
                if command == "mkdir reviewed" {
                    assert_eq!(
                        fixture
                            .workspace
                            .folder
                            .as_ref()
                            .unwrap()
                            .join("reviewed")
                            .exists(),
                        !block
                    );
                }
                let requests = reviewer.requests.lock().unwrap();
                let evidence = serde_json::to_string(&requests[0].messages).unwrap();
                assert!(evidence.contains("unrestricted"), "{evidence}");
            }
        }
    }
}

#[tokio::test]
async fn unsandboxed_exclusions_match_command_patterns_without_granting_approval() {
    for (pattern, command, excluded) in [
        ("printf:*", "printf approved > ../outside", true),
        ("*approved*", "printf approved > ../outside", true),
        ("printf", "printf approved > ../outside", false),
        ("print:*", "printf approved > ../outside", false),
        ("pwd", "pwd && printf approved > ../outside", true),
        ("pwd", "printf '%s' 'pwd' > ../outside", false),
    ] {
        let fixture = SandboxFixture::new(serde_json::json!({
            "enabled":true, "failIfUnavailable":true, "excludedCommands":[pattern]
        }));
        let prepared = fixture.prepare(command).await;
        let facts = prepared.0.approval_facts();
        assert_eq!(
            facts.requires_approval.unwrap_or(false),
            excluded,
            "{pattern}: {command}"
        );
        assert_eq!(
            facts.isolation.unwrap().writes,
            if excluded {
                WriteIsolation::Unrestricted
            } else {
                WriteIsolation::FolderAndDirectories
            }
        );
        let result = fixture.run(prepared).await;
        assert_eq!(
            fixture.root.path().join("outside").exists(),
            excluded,
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn sandbox_denial_suggests_escape_only_when_the_prepared_settings_allow_it() {
    for allowed in [true, false] {
        let fixture = SandboxFixture::new(serde_json::json!({
            "enabled":true, "failIfUnavailable":true, "allowUnsandboxedCommands":allowed
        }));
        let prepared = fixture.prepare("printf denied > ../outside").await;
        std::fs::write(
            fixture.settings.file_path(),
            serde_json::to_vec(&serde_json::json!({
                "sandbox":{"enabled":true,"allowUnsandboxedCommands":!allowed}
            }))
            .unwrap(),
        )
        .unwrap();
        let result = fixture.run(prepared).await;
        assert!(result.is_error, "{result:?}");
        assert!(!fixture.root.path().join("outside").exists());
        assert!(result.content.contains("被沙箱拦截"), "{result:?}");
        assert_eq!(
            result.content.contains("dangerouslyDisableSandbox"),
            allowed,
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn unsandboxed_preparation_keeps_its_route_after_settings_change() {
    for allowed in [true, false] {
        let fixture = SandboxFixture::new(serde_json::json!({
            "enabled":true,"failIfUnavailable":true,"allowUnsandboxedCommands":allowed
        }));
        let operation = tickloop_ids::generate();
        let prepared = fixture.tools.prepare_for_operation(
            &tickloop_ids::IdContext::new(), &fixture.workspace, "Bash",
            &serde_json::json!({"command":"printf frozen > ../outside","dangerouslyDisableSandbox":true}),
            operation,
        ).await.unwrap();
        std::fs::write(
            fixture.settings.file_path(),
            serde_json::to_vec(&serde_json::json!({
                "sandbox":{"enabled":true,"allowUnsandboxedCommands":!allowed}
            }))
            .unwrap(),
        )
        .unwrap();
        let result = fixture.run((prepared, operation)).await;
        assert_eq!(result.is_error, !allowed, "{result:?}");
        assert_eq!(fixture.root.path().join("outside").exists(), allowed);
    }
}

#[tokio::test]
async fn unsandboxed_calls_keep_explicit_ask_and_deny_ahead_of_auto_review() {
    use tickloop_harness::protocol::{Decision, PermissionRule};
    for decision in [Decision::Ask, Decision::Deny] {
        for excluded in [false, true] {
            let command = "printf approved > ../outside";
            let fixture = SandboxFixture::new(serde_json::json!({
                "enabled":true,"failIfUnavailable":true,
                "excludedCommands": if excluded {vec![command]} else {vec![]}
            }));
            let (agent, chat) = fixture
                .agent(
                    serde_json::json!({
                        "command":command,"dangerouslyDisableSandbox":!excluded
                    }),
                    ExecutionMode::Auto,
                )
                .await;
            agent
                .set_permission_rules(
                    chat,
                    vec![PermissionRule {
                        id: tickloop_ids::generate(),
                        decision,
                        tool: "Bash".into(),
                        specifier: None,
                        input: None,
                        loop_id: None,
                        persistent: false,
                    }],
                )
                .await
                .unwrap();
            let reviewer = Arc::new(ScriptedModel::new(vec![]));
            agent.set_auto_reviewer(reviewer.clone()).await;
            let (asked, result) = fixture.run_with_human_approval(&agent, chat, true).await;
            assert_eq!(asked, decision == Decision::Ask);
            assert_eq!(result.is_error, decision == Decision::Deny, "{result:?}");
            assert_eq!(
                fixture.root.path().join("outside").exists(),
                decision == Decision::Ask
            );
            assert!(reviewer.requests.lock().unwrap().is_empty());
        }
    }
}

struct Ticket05Grants(OwnTickhost);

#[async_trait::async_trait]
impl ShellGrants for Ticket05Grants {
    async fn allow_spawn(
        &self,
        chat: uuid::Uuid,
        operation: uuid::Uuid,
        approval: tickhost::shell::ShellApproval,
    ) -> Result<
        tickloop_code_local_environment::hosted::worker_control::IssuedSpawn,
        tickhost_connect::host_error::HostError,
    > {
        // The occupied 10.1 host accepts precisely these SandboxPolicy fields.
        if let Some(policy) = &approval.sandbox {
            let wire = serde_json::to_value(policy).unwrap();
            for field in wire.as_object().unwrap().keys() {
                if ![
                    "writable_roots",
                    "deny_write",
                    "protected_paths",
                    "denied_path_patterns",
                    "network",
                ]
                .contains(&field.as_str())
                {
                    return Err(tickhost_connect::host_error::HostError::before(
                        "InvalidParams",
                        format!("10.1 rejects unknown sandbox field {field}"),
                    ));
                }
            }
        }
        self.0.allow_spawn(chat, operation, approval).await
    }

    async fn allow_view(
        &self,
        chat: uuid::Uuid,
        instance: &tick_protocol::InstanceRef,
    ) -> Result<tickloop_harness::protocol::HostedViewGrant, tickhost_connect::host_error::HostError>
    {
        self.0.allow_view(chat, instance).await
    }
}

#[tokio::test]
async fn sandbox_escape_guidance_preserves_ticket05_host_wire_compatibility() {
    let mut fixture =
        SandboxFixture::new(serde_json::json!({"enabled":true,"failIfUnavailable":true}));
    fixture.environment =
        LocalEnvironment::with_shells(Arc::new(Ticket05Grants(OwnTickhost::new())))
            .with_sandbox(fixture.settings.clone());
    let (agent, chat) = fixture
        .agent(
            serde_json::json!({"command":"printf compatible > inside"}),
            ExecutionMode::AcceptEdits,
        )
        .await;
    let (_, result) = fixture.run_with_human_approval(&agent, chat, true).await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.folder.as_ref().unwrap().join("inside")).unwrap(),
        "compatible"
    );
}

#[tokio::test]
async fn unsandboxed_exclusions_recognize_assignments_and_safe_wrappers() {
    let fixture = SandboxFixture::new(serde_json::json!({
        "enabled":true,"failIfUnavailable":true,"excludedCommands":["pwd:*"]
    }));
    for (command, excluded) in [
        ("FOO=bar pwd", true),
        ("FOO='a b' pwd", true),
        ("FOO=x=y pwd", true),
        ("FOO+=x pwd", true),
        ("time FOO=bar nice -n 0 pwd", true),
        (
            "timeout --signal TERM --kill-after=2s 30s FOO=bar pwd",
            true,
        ),
        ("nohup -- timeout -k2 -s TERM 5 pwd", true),
        ("stdbuf -oL pwd", true),
        ("PATH=/tmp pwd", false),
        ("LD_PRELOAD=x pwd", false),
        ("DYLD_LIBRARY_PATH=x pwd", false),
        ("FOO=$(printf x) pwd", false),
        ("timeout --unknown 5 pwd", false),
        ("time echo pwd", false),
        ("printf 'timeout 30 pwd'", false),
        ("nice -n nope pwd", false),
        ("timeout30 pwd", false),
        ("timeout 30ss pwd", false),
        ("timeout 30 -- pwd", false),
        ("FOO=bar -- pwd", false),
    ] {
        let prepared = fixture.prepare(command).await;
        let facts = prepared.0.approval_facts();
        assert_eq!(
            facts.requires_approval.unwrap_or(false),
            excluded,
            "{command}"
        );
        assert_eq!(
            facts.isolation.unwrap().writes,
            if excluded {
                WriteIsolation::Unrestricted
            } else {
                WriteIsolation::FolderAndDirectories
            },
            "{command}"
        );
    }
}

const BUILD_MAKEFILE: &str = "build/out.txt:\n\tmkdir -p build\n\tprintf built > build/out.txt\n";

fn blocking_reviewer() -> Arc<ScriptedModel> {
    let block = tickloop_harness::test_support::auto_classification(true).to_string();
    Arc::new(ScriptedModel::new(vec![text(&block), text(&block)]))
}

struct AutoApprovalRun {
    reviewed: bool,
    asked: bool,
    result: ToolResult,
    direct_reason: Option<tickloop_harness::protocol::DirectApprovalReason>,
}

impl SandboxFixture {
    async fn run_without_approval_answers(
        &self,
        command: &str,
        mode: ExecutionMode,
    ) -> AutoApprovalRun {
        let (agent, chat) = self
            .agent(serde_json::json!({"command":command}), mode)
            .await;
        let blocking_reviewer = blocking_reviewer();
        agent.set_auto_reviewer(blocking_reviewer.clone()).await;
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(chat, "Build the project.".into())
            .await
            .unwrap();
        let mut asked = false;
        let mut result = None;
        let mut direct_reason = None;
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    asked = true;
                    agent
                        .answer_permission(chat, request.id, false)
                        .await
                        .unwrap();
                }
                EventKind::ApprovalRecorded { record } => direct_reason = record.direct_reason,
                EventKind::ToolCompleted { result: value, .. } => result = Some(value),
                EventKind::Completed { .. } | EventKind::Interrupted => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        agent.shutdown().await;
        let reviewed = !blocking_reviewer.requests.lock().unwrap().is_empty();
        AutoApprovalRun {
            reviewed,
            asked,
            result: result.expect("Bash returns a result"),
            direct_reason,
        }
    }
}

#[tokio::test]
async fn sandboxed_build_runs_without_review_unless_auto_allow_is_turned_off() {
    use tickloop_harness::protocol::DirectApprovalReason;
    for auto_allow in [None, Some(true), Some(false)] {
        for mode in [ExecutionMode::Auto, ExecutionMode::AcceptEdits] {
            let mut sandbox = serde_json::json!({"enabled":true, "failIfUnavailable":true});
            if let Some(auto_allow) = auto_allow {
                sandbox["autoAllowBashIfSandboxed"] = auto_allow.into();
            }
            let fixture = SandboxFixture::new(sandbox);
            let folder = fixture.workspace.folder.clone().unwrap();
            std::fs::write(folder.join("Makefile"), BUILD_MAKEFILE).unwrap();
            let run = fixture.run_without_approval_answers("make", mode).await;
            let case = format!("autoAllowBashIfSandboxed={auto_allow:?} mode={mode:?}");
            let skips_approval = auto_allow != Some(false);
            assert_eq!(
                run.reviewed,
                !skips_approval && mode == ExecutionMode::Auto,
                "{case}"
            );
            assert_eq!(
                run.asked,
                !skips_approval && mode == ExecutionMode::AcceptEdits,
                "{case}"
            );
            assert_eq!(
                run.result.is_error, !skips_approval,
                "{case}: {:?}",
                run.result
            );
            assert_eq!(
                folder.join("build/out.txt").exists(),
                skips_approval,
                "{case}"
            );
            assert_eq!(
                run.direct_reason,
                skips_approval.then_some(DirectApprovalReason::Sandbox),
                "{case}"
            );
        }
    }
}

#[tokio::test]
async fn sandboxed_commands_without_review_still_cannot_write_sensitive_or_outside_paths() {
    let fixture =
        SandboxFixture::new(serde_json::json!({"enabled":true, "failIfUnavailable":true}));
    let folder = fixture.workspace.folder.clone().unwrap();
    std::fs::create_dir_all(folder.join(".git/hooks")).unwrap();
    for (command, target) in [
        (
            "printf hook > .git/hooks/pre-commit",
            folder.join(".git/hooks/pre-commit"),
        ),
        (
            "printf outside > ../outside",
            fixture.root.path().join("outside"),
        ),
    ] {
        let run = fixture
            .run_without_approval_answers(command, ExecutionMode::Auto)
            .await;
        assert!(!run.reviewed && !run.asked, "{command}");
        assert!(run.result.is_error, "{command}: {:?}", run.result);
        assert!(
            run.result.content.contains("被沙箱拦截"),
            "{}",
            run.result.content
        );
        assert!(!target.exists(), "{command}");
    }
}

#[tokio::test]
async fn unsandboxed_routes_keep_ordinary_approval_when_auto_allow_is_on() {
    for (sandbox, input) in [
        (
            serde_json::json!({"enabled":true, "failIfUnavailable":true}),
            serde_json::json!({"command":"make", "dangerouslyDisableSandbox":true}),
        ),
        (
            serde_json::json!({"enabled":true, "failIfUnavailable":true, "excludedCommands":["make"]}),
            serde_json::json!({"command":"make"}),
        ),
        (
            serde_json::json!({"enabled":false}),
            serde_json::json!({"command":"make"}),
        ),
    ] {
        let fixture = SandboxFixture::new(sandbox.clone());
        let folder = fixture.workspace.folder.clone().unwrap();
        std::fs::write(folder.join("Makefile"), BUILD_MAKEFILE).unwrap();
        let (agent, chat) = fixture.agent(input, ExecutionMode::Auto).await;
        let blocking_reviewer = blocking_reviewer();
        agent.set_auto_reviewer(blocking_reviewer.clone()).await;
        let (asked, result) = fixture.run_with_human_approval(&agent, chat, false).await;
        assert!(!asked, "{sandbox}");
        assert!(
            !blocking_reviewer.requests.lock().unwrap().is_empty(),
            "{sandbox}"
        );
        assert!(result.is_error, "{sandbox}: {result:?}");
        assert!(!folder.join("build/out.txt").exists(), "{sandbox}");
    }
}

#[tokio::test]
#[ignore = "requires DeepSeek credentials and runs a real model against the macOS sandbox"]
async fn live_model_runs_an_ordinary_build_in_the_sandbox_without_review() {
    use tickloop_harness::{
        model::{LocalModelClients, ModelClients},
        protocol::{
            DirectApprovalReason, InputModality, ModelAuthSource, ModelConfig, ModelProvider,
            Protocol,
        },
    };
    let model_config = ModelConfig {
        provider: ModelProvider::Api,
        auth_source: ModelAuthSource::ApiKeyEnv,
        name: "DeepSeek sandbox validation".into(),
        protocol: Protocol::Messages,
        base_url: "https://api.deepseek.com/anthropic".into(),
        model: "deepseek-flash".into(),
        api_key_env: "DEEPSEEK_API_KEY".into(),
        max_output_tokens: 4096,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text],
    };
    let model = LocalModelClients::new(None)
        .client(&model_config)
        .expect("valid DeepSeek configuration and credentials");
    let fixture =
        SandboxFixture::new(serde_json::json!({"enabled":true, "failIfUnavailable":true}));
    let folder = fixture.workspace.folder.clone().unwrap();
    std::fs::write(folder.join("Makefile"), BUILD_MAKEFILE).unwrap();
    let (agent, chat) = fixture
        .agent_with_model(model, model_config, ExecutionMode::Auto)
        .await;
    let reviewer = Arc::new(ScriptedModel::new(vec![]));
    agent.set_auto_reviewer(reviewer.clone()).await;
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(
            chat,
            "Use the Bash tool to run exactly `make` in the current working directory to build \
             this project, then reply with one short sentence. Use no other tool or command."
                .into(),
        )
        .await
        .unwrap();
    let observed = tokio::time::timeout(std::time::Duration::from_secs(300), async {
        let mut bash_calls = Vec::new();
        let mut direct_reasons = Vec::new();
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::ToolStarted { name, input, .. } => {
                    eprintln!("tool started: {name} {input}");
                    bash_calls.push((name, input));
                }
                EventKind::ApprovalReviewStarted { .. } | EventKind::ApprovalAssessed { .. } => {
                    panic!("the sandboxed build reached the Auto reviewer")
                }
                EventKind::PermissionRequested { .. } => {
                    panic!("the sandboxed build asked a person")
                }
                EventKind::ApprovalRecorded { record } => {
                    eprintln!(
                        "approval: path={:?} direct_reason={:?} reason={}",
                        record.path, record.direct_reason, record.reason
                    );
                    direct_reasons.push(record.direct_reason);
                }
                EventKind::ToolCompleted { result, .. } => {
                    eprintln!(
                        "tool result: is_error={} content={}",
                        result.is_error, result.content
                    );
                }
                EventKind::Completed { usage } => {
                    eprintln!("completed: usage={usage:?}");
                    break;
                }
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        (bash_calls, direct_reasons)
    })
    .await
    .expect("the live run finished in time");
    agent.shutdown().await;
    let (bash_calls, direct_reasons) = observed;
    assert!(
        bash_calls
            .iter()
            .all(|(name, input)| name == "Bash" && input["command"] == "make"),
        "{bash_calls:?}"
    );
    assert!(!bash_calls.is_empty());
    assert!(
        direct_reasons
            .iter()
            .all(|reason| *reason == Some(DirectApprovalReason::Sandbox))
    );
    assert!(reviewer.requests.lock().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(folder.join("build/out.txt")).unwrap(),
        "built"
    );
}
