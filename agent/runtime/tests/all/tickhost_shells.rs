//! An Agent's shell commands as its Tickhost runs them: the Tools of one Chat start each command
//! through a one-time spawn grant, follow it live, move it to the background or stop it, and read
//! its output from the file they created.

use std::{path::Path, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tickloop_agent_base::private_fs::PrivateDirectory;
use tickloop_agent_protocol::{CommandState, ToolResult};
use tickloop_agent_runtime::Agent;
use tickloop_agent_tools::{
    ExecutionContext, PreparedTool, SecondaryModel, ToolError, Tools, Workspace,
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
    fn new() -> Self {
        let agent = Agent::new(
            tickloop_agent_test_support::config(),
            Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
        );
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let workdir = root.join("work");
        std::fs::create_dir(&workdir).unwrap();
        let output = PrivateDirectory::trusted(root.join("output"));
        let tools = Tools::for_chat(
            output.clone(),
            output.join("tasks"),
            output,
            agent.shell_tasks().for_chat(Uuid::new_v4()),
        );
        Self {
            _agent: agent,
            directory,
            workspace: Workspace {
                workdir,
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
        self.tools
            .execute_with_context(
                prepared,
                CancellationToken::new(),
                ExecutionContext {
                    operation_id: Uuid::new_v4(),
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
            fixture.workspace.workdir.display()
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
    let approved = fixture.workspace.workdir.join("approved");
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
