//! An Agent's shell commands as its Tickhost runs them: the Tools of one Chat start each command
//! through a one-time spawn grant, follow it live, move it to the background or stop it, and read
//! its output from the file they created; an ended command stays viewable while its Chat's Monitor
//! lists it. Without a daemon or a Tickhost given to it, an Agent runs no command.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickhost::shell::{ShellApproval, ShellIdentity, SpawnBinding, SpawnGrant};
use tickhost_connect::{host_error::HostError, hosted};
use tickloop_agent_base::private_fs::PrivateDirectory;
use tickloop_agent_protocol::{CommandState, HostedViewGrant, ToolResult, UserShellState};
use tickloop_agent_runtime::{
    Agent,
    hosted::{
        shells::{OwnTickhost, ShellGrants},
        worker_control::IssuedSpawn,
    },
};
use tickloop_agent_tools::{
    ChatDirectory, ExecutionContext, PreparedTool, SecondaryModel, ToolError, Tools, Workspace,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct NoSecondaryModel;

#[async_trait::async_trait]
impl SecondaryModel for NoSecondaryModel {
    async fn respond(&self, _: String, _: CancellationToken) -> Result<String, ToolError> {
        Err(ToolError::SecondaryModel("no secondary model".into()))
    }
}

struct Fixture {
    _agent: Agent,
    directory: tempfile::TempDir,
    workspace: Workspace,
    tools: Tools,
}

impl Fixture {
    /// An Agent that runs its commands in a Tickhost of its own.
    fn new() -> Self {
        Self::granting_with(Some(Arc::new(OwnTickhost::new())))
    }

    /// An Agent whose spawns `grants` grant, or an Agent without a daemon.
    fn granting_with(grants: Option<Arc<dyn ShellGrants>>) -> Self {
        let agent = Agent::new(
            tickloop_agent_test_support::config(),
            Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
        );
        if let Some(grants) = grants {
            agent.install_shell_grants(grants);
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let workdir = root.join("work");
        std::fs::create_dir(&workdir).unwrap();
        let output = PrivateDirectory::trusted(root.join("output"));
        let tools = Tools::for_chat(
            ChatDirectory {
                chat: output.clone(),
                task_output: output.join("tasks"),
                family_temp: output,
            },
            agent.shell_tasks().for_chat(Uuid::new_v4()),
        );
        Self {
            _agent: agent,
            directory,
            workspace: Workspace {
                folder: Some(workdir),
                additional_directories: Vec::new(),
            },
            tools,
        }
    }

    async fn prepare(&self, input: Value) -> PreparedTool {
        self.tools
            .prepare(&self.workspace, "Bash", &input)
            .await
            .unwrap()
    }

    async fn run(&self, prepared: PreparedTool) -> Result<ToolResult, ToolError> {
        self.run_as(prepared, Uuid::new_v4()).await
    }

    async fn run_as(
        &self,
        prepared: PreparedTool,
        operation_id: Uuid,
    ) -> Result<ToolResult, ToolError> {
        self.tools
            .execute_with_context(
                prepared,
                CancellationToken::new(),
                ExecutionContext {
                    operation_id,
                    loop_id: Uuid::new_v4(),
                    accepts_images: false,
                },
                &NoSecondaryModel,
            )
            .await
            .result
    }

    async fn command_state(&self, id: Uuid) -> CommandState {
        self.tools
            .commands()
            .await
            .into_iter()
            .find(|command| command.operation_id == id)
            .unwrap()
            .state
    }
}

fn task_id(result: &ToolResult) -> Uuid {
    let id = result
        .content
        .split("with ID: ")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .unwrap_or_else(|| panic!("no task ID in {}", result.content));
    Uuid::parse_str(id).unwrap()
}

#[tokio::test]
async fn a_foreground_command_runs_in_its_directory_and_reports_its_output_and_exit() {
    let fixture = Fixture::new();
    let prepared = fixture
        .prepare(json!({"command":"printf 'out|'; printf 'err|' >&2; pwd; exit 3"}))
        .await;
    let result = fixture.run(prepared).await.unwrap();
    assert!(result.is_error);
    assert_eq!(
        result.content,
        format!(
            "Exit code 3\nout|err|{}",
            fixture.workspace.folder.as_ref().unwrap().display()
        )
    );
}

#[tokio::test]
async fn a_command_that_outlives_its_timeout_moves_to_the_background_until_it_is_stopped() {
    let fixture = Fixture::new();
    let prepared = fixture
        .prepare(json!({"command":"printf started; while :; do sleep 0.05; done","timeout":200}))
        .await;
    let started = fixture.run(prepared).await.unwrap();
    assert!(
        started
            .content
            .starts_with("Command running in background with ID: "),
        "{}",
        started.content
    );
    let id = task_id(&started);
    assert_eq!(fixture.command_state(id).await, CommandState::Background);
    let stopped = fixture.tools.stop_command(id).await.unwrap();
    assert_eq!(stopped.state, CommandState::Failed);
    assert_eq!(
        stopped.reason.as_deref(),
        Some(tickloop_agent_tools::CANCELLED_COMMAND_REASON)
    );
    let group = stopped.pid.unwrap();
    assert!(!tickhost::shell::process_group_exists(group));
    let read = fixture.tools.read_command(id, 0, 1024).await.unwrap();
    let read: Value = serde_json::from_str(&read.content).unwrap();
    assert_eq!(read["output"], "started");
}

#[tokio::test]
async fn a_sleep_that_outlives_its_timeout_is_stopped_instead() {
    let fixture = Fixture::new();
    let prepared = fixture
        .prepare(json!({"command":"sleep 30","timeout":200}))
        .await;
    let result = tokio::time::timeout(Duration::from_secs(10), fixture.run(prepared))
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_error);
    assert!(
        result.content.contains("Command timed out after"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn a_user_command_beyond_sixty_four_mebibytes_is_cut_and_reported() {
    let fixture = Fixture::new();
    let prepared = fixture
        .prepare(json!({"command":"dd if=/dev/zero bs=1048576 count=65 2>/dev/null"}))
        .await;
    let chat = Uuid::new_v4();
    let operation = Uuid::new_v4();
    let mut updates = fixture.tools.subscribe_user_shells();
    fixture
        .tools
        .execute_user_shell(prepared, operation, chat, CancellationToken::new())
        .await
        .unwrap();
    let finished = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = updates.recv().await.unwrap();
            if snapshot.operation_id == operation
                && matches!(
                    snapshot.state,
                    CommandState::Completed | CommandState::Failed | CommandState::Unknown
                )
            {
                break snapshot;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(finished.state, CommandState::Failed);
    assert_eq!(finished.reason.as_deref(), Some("output_limit_exceeded"));
    assert_eq!(finished.output_bytes, 64 * 1024 * 1024);
    assert_eq!(
        std::fs::metadata(&finished.output_ref).unwrap().len(),
        64 * 1024 * 1024
    );
}

#[tokio::test]
async fn a_directory_replaced_after_its_approval_runs_nothing() {
    let fixture = Fixture::new();
    let approved = fixture.workspace.folder.as_ref().unwrap().join("approved");
    std::fs::create_dir(&approved).unwrap();
    let prepared = fixture
        .prepare(json!({"command":"touch probe","cwd":approved}))
        .await;
    let moved = fixture.directory.path().join("moved");
    std::fs::rename(&approved, &moved).unwrap();
    std::fs::create_dir(&approved).unwrap();
    let result = fixture.run(prepared).await;
    assert!(
        matches!(result, Err(ToolError::PathChanged(_))),
        "{result:?}"
    );
    for directory in [approved.as_path(), moved.as_path()] {
        assert!(!Path::new(directory).join("probe").exists());
    }
    assert!(fixture.tools.commands().await.is_empty());
}

#[tokio::test]
async fn an_agent_without_a_daemon_refuses_to_run_a_command() {
    let fixture = Fixture::granting_with(None);
    let prepared = fixture.prepare(json!({"command":"touch probe"})).await;
    let error = fixture.run(prepared).await.unwrap_err();
    assert!(
        matches!(&error, ToolError::Rejected(message) if message.contains("Agent daemon")),
        "{error:?}"
    );
    assert!(
        !fixture
            .workspace
            .folder
            .as_ref()
            .unwrap()
            .join("probe")
            .exists()
    );
    assert!(fixture.tools.commands().await.is_empty());
}

/// Grants spawns from a Tickhost of its own once a test lets each grant through.
struct HeldGrants {
    host: OwnTickhost,
    asked: tokio::sync::Notify,
    released: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl ShellGrants for HeldGrants {
    async fn allow_spawn(
        &self,
        chat: Uuid,
        operation: Uuid,
        approval: ShellApproval,
    ) -> Result<IssuedSpawn, HostError> {
        let issued = self.host.allow_spawn(chat, operation, approval).await;
        self.asked.notify_one();
        self.released.acquire().await.unwrap().forget();
        issued
    }

    async fn allow_view(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
    ) -> Result<HostedViewGrant, HostError> {
        self.host.allow_view(chat, instance).await
    }
}

#[tokio::test]
async fn a_command_stopped_while_its_spawn_is_granted_never_starts() {
    let grants = Arc::new(HeldGrants {
        host: OwnTickhost::new(),
        asked: tokio::sync::Notify::new(),
        released: tokio::sync::Semaphore::new(0),
    });
    let fixture = Arc::new(Fixture::granting_with(Some(grants.clone())));
    let prepared = fixture
        .prepare(json!({"command":"touch probe; sleep 30"}))
        .await;
    let id = Uuid::new_v4();
    let running = tokio::spawn({
        let fixture = fixture.clone();
        async move { fixture.run_as(prepared, id).await }
    });
    grants.asked.notified().await;
    let stopping = tokio::spawn({
        let fixture = fixture.clone();
        async move { fixture.tools.stop_command(id).await }
    });
    while fixture.command_state(id).await != CommandState::Cancelling {
        tokio::task::yield_now().await;
    }
    grants.released.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        result.content.contains("aborted before completion"),
        "{}",
        result.content
    );
    let stopped = stopping.await.unwrap().unwrap();
    assert_eq!(
        (stopped.state, stopped.reason.as_deref(), stopped.pid),
        (
            CommandState::Failed,
            Some(tickloop_agent_tools::CANCELLED_COMMAND_REASON),
            None
        )
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !fixture
            .workspace
            .folder
            .as_ref()
            .unwrap()
            .join("probe")
            .exists()
    );
}

/// Grants every spawn at a Tickhost that takes the spawn request and leaves without answering.
struct AnswerlessHost {
    endpoint: PathBuf,
}

impl AnswerlessHost {
    fn serve(directory: &Path) -> Self {
        let endpoint = directory.join("answerless.sock");
        let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(reader);
                let frames = hosted::BUSINESS_FRAME_BYTES;
                hosted::read_frame(&mut reader, frames).await.unwrap();
                let hello = hosted::HelloReply {
                    epoch: Uuid::new_v4(),
                    pid: std::process::id(),
                    version: hosted::BUILD_VERSION.into(),
                    major: hosted::BUSINESS_MAJOR,
                    minor: hosted::BUSINESS_MINOR,
                    capabilities: hosted::BUSINESS_CAPABILITIES.map(str::to_owned).to_vec(),
                    host: None,
                };
                hosted::write_frame(&mut writer, &hosted::ok(&Value::Null, json!(hello)), frames)
                    .await
                    .unwrap();
                hosted::receive_descriptors(&reader, hosted::SPAWN_DESCRIPTORS)
                    .await
                    .unwrap();
                hosted::read_frame(&mut reader, frames).await.unwrap();
            }
        });
        Self { endpoint }
    }
}

#[async_trait::async_trait]
impl ShellGrants for AnswerlessHost {
    async fn allow_spawn(
        &self,
        chat: Uuid,
        operation: Uuid,
        approval: ShellApproval,
    ) -> Result<IssuedSpawn, HostError> {
        Ok(IssuedSpawn {
            grant: SpawnGrant {
                grant: Uuid::new_v4(),
                token: "token".into(),
                epoch: Uuid::new_v4(),
            },
            binding: SpawnBinding {
                operation,
                identity: ShellIdentity {
                    daemon_epoch: Uuid::new_v4(),
                    worker: Uuid::new_v4(),
                    chat,
                },
                approval,
            },
            endpoint: self.endpoint.clone(),
        })
    }

    async fn allow_view(&self, _: Uuid, _: &InstanceRef) -> Result<HostedViewGrant, HostError> {
        Err(HostError::before("Unsupported", "this host shows nothing"))
    }
}

#[tokio::test]
async fn a_spawn_whose_answer_was_lost_is_listed_with_an_unknown_outcome_not_refused() {
    let sockets = tempfile::Builder::new()
        .prefix("ths")
        .tempdir_in(tickloop_agent_test_support::SHORT_TEMP_ROOT)
        .unwrap();
    let fixture = Fixture::granting_with(Some(Arc::new(AnswerlessHost::serve(sockets.path()))));
    let prepared = fixture.prepare(json!({"command":"touch probe"})).await;
    let id = Uuid::new_v4();
    let result = fixture.run_as(prepared, id).await.unwrap();
    assert!(result.is_error);
    assert!(
        result.content.contains("outcome is unknown")
            && result.content.contains("do not run it again"),
        "{}",
        result.content
    );
    assert_eq!(fixture.command_state(id).await, CommandState::Unknown);
}

/// How long the Tickhost of the view tests lets an ended command nobody holds or views idle.
const VIEW_IDLE_TIMEOUT: Duration = Duration::from_millis(500);

/// Runs `command` as a user Shell of `chat` and waits for it to end.
async fn ended_user_shell(
    agent: &Agent,
    chat: Uuid,
    command: &str,
    background: bool,
) -> InstanceRef {
    let receipt = agent
        .submit_user_shell(chat, command.into(), background)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !agent
        .snapshot(chat)
        .await
        .unwrap()
        .user_shells
        .iter()
        .any(|record| record.id == receipt.command_id && record.state == UserShellState::Completed)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{command} did not end"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    InstanceRef::shell(receipt.command_id)
}

async fn opens(agent: &Agent, chat: Uuid, instance: &InstanceRef) -> bool {
    agent.shell_tasks().allow_view(chat, instance).await.is_ok()
}

#[tokio::test]
async fn an_ended_background_shell_opens_for_as_long_as_monitor_lists_it() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
    );
    agent.install_shell_grants(Arc::new(OwnTickhost::with_idle_timeout(VIEW_IDLE_TIMEOUT)));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let background = ended_user_shell(&agent, chat, "printf background", true).await;
    let foreground = ended_user_shell(&agent, chat, "printf foreground", false).await;
    // Nobody views either, for far longer than the Tickhost keeps an ended command nobody holds.
    tokio::time::sleep(VIEW_IDLE_TIMEOUT * 6).await;
    // Its end is unread, so Monitor still lists it; a foreground command it never lists.
    assert!(opens(&agent, chat, &background).await);
    assert!(!opens(&agent, chat, &foreground).await);

    // Once its end was handled Monitor shows it a while longer, past the worker's next check.
    agent
        .mark_monitor_handled(chat, background.clone())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(opens(&agent, chat, &background).await);

    agent
        .remove_monitor_member(chat, background.clone())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while opens(&agent, chat, &background).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the Tickhost still keeps a Shell Monitor no longer lists"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    agent.shutdown().await;
}
