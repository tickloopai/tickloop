#![cfg(target_os = "macos")]

use std::sync::Arc;
use tickloop_code_local_environment::{environment::LocalEnvironment, hosted::shells::OwnTickhost};
use tickloop_code_settings::UserSettings;
use tickloop_harness::runtime::environment::*;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn sandbox_blocks_outside_writes_and_reports_folder_isolation() {
    let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let root = tempfile::tempdir_in(scratch.canonicalize().unwrap()).unwrap();
    let folder = root.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    let config = root.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(
        config.join("settings.json"),
        r#"{"sandbox":{"enabled":true,"failIfUnavailable":true}}"#,
    )
    .unwrap();
    let environment = LocalEnvironment::with_shells(Arc::new(OwnTickhost::new()))
        .with_sandbox(UserSettings::in_root(&config));
    let authority = ExecutionAuthority::new(tickloop_ids::generate().into());
    let port = environment.bind(
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: None,
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
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::SpawnShell(ShellSpec {
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
    };
    let spec = ShellSpec {
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
