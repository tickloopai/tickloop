use std::{collections::BTreeMap, sync::Arc, time::Duration};

use tick_protocol::InstanceRef;
use tickhost::shell::ShellApproval;
use tickhost_connect::host_error::HostError;
use tickloop_code_local_environment::environment::LocalEnvironment;
use tickloop_code_local_environment::hosted::{shells::ShellGrants, worker_control::IssuedSpawn};
use tickloop_harness::protocol::HostedViewGrant;
use tickloop_harness::runtime::environment::*;
use tickloop_harness::test_support::{
    execution_contracts::{define_command_contract, one_shot_command_contract},
    execution_environment::MemoryEnvironment,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Grants that fail the test when asked: one-shot commands never go through Tickhost.
struct NoTickhost;

#[async_trait::async_trait]
impl ShellGrants for NoTickhost {
    async fn allow_spawn(
        &self,
        _: Uuid,
        _: Uuid,
        _: ShellApproval,
    ) -> Result<IssuedSpawn, HostError> {
        panic!("a one-shot command asked Tickhost for a spawn");
    }

    async fn allow_view(&self, _: Uuid, _: &InstanceRef) -> Result<HostedViewGrant, HostError> {
        panic!("a one-shot command asked Tickhost for a view");
    }
}

fn local() -> LocalEnvironment {
    LocalEnvironment::with_shells(Arc::new(NoTickhost))
}

fn scope(directory: &std::path::Path) -> ExecutionScope {
    ExecutionScope {
        chat: tickloop_ids::generate().into(),
        folder: Some(directory.display().to_string()),
        additional_directories: Vec::new(),
    }
}

fn canonical(directory: &tempfile::TempDir) -> String {
    directory
        .path()
        .canonicalize()
        .unwrap()
        .display()
        .to_string()
}

fn spec(command: &str, cwd: &str, timeout: Duration) -> CommandSpec {
    CommandSpec {
        command: command.into(),
        stdin: Vec::new(),
        env: BTreeMap::new(),
        cwd: cwd.into(),
        timeout,
    }
}

async fn run(
    environment: &dyn ExecutionEnvironment,
    spec: CommandSpec,
    cancel: CancellationToken,
) -> ExecutionOutcome {
    let chat = tickloop_ids::generate().into();
    let authority = ExecutionAuthority::new(chat);
    let port = environment
        .bind(
            ExecutionScope {
                chat,
                folder: Some(spec.cwd.clone()),
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::RunCommand(spec))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    port.execute(prepared, admission, cancel).await
}

fn completed(outcome: ExecutionOutcome) -> CommandOutput {
    match outcome {
        ExecutionOutcome::Completed(result) => *result.command.expect("command output"),
        outcome => panic!("the command did not complete: {outcome:?}"),
    }
}

fn process_exists(pid: libc::pid_t) -> bool {
    (unsafe { libc::kill(pid, 0) } == 0)
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

async fn wait_until_gone(pid: libc::pid_t) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while process_exists(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        panic!("process {pid} outlived its command");
    });
}

async fn background_pid(path: &std::path::Path) -> libc::pid_t {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the command did not start its background process")
}

/// Starts a sleeper in the command's process group, records its pid and waits for it.
const GROUP_COMMAND: &str = "sleep 30 & echo $! > \"$PID_FILE\"; wait";

fn group_spec(directory: &tempfile::TempDir, timeout: Duration) -> CommandSpec {
    CommandSpec {
        env: [(
            "PID_FILE".into(),
            directory.path().join("pid").display().to_string(),
        )]
        .into(),
        ..spec(GROUP_COMMAND, &canonical(directory), timeout)
    }
}

#[tokio::test]
async fn local_and_memory_environments_share_the_one_shot_command_contract() {
    let directory = tempfile::tempdir().unwrap();
    one_shot_command_contract(&local(), scope(directory.path()), canonical(&directory)).await;
    let memory = MemoryEnvironment::new();
    define_command_contract(&memory);
    one_shot_command_contract(&memory, scope(directory.path()), canonical(&directory)).await;
}

#[tokio::test]
async fn a_timeout_ends_the_whole_process_group() {
    let directory = tempfile::tempdir().unwrap();
    let pending = tokio::spawn({
        let spec = group_spec(&directory, Duration::from_millis(500));
        async move { run(&local(), spec, CancellationToken::new()).await }
    });
    let pid = background_pid(&directory.path().join("pid")).await;
    let output = completed(pending.await.unwrap());
    assert!(output.timed_out);
    wait_until_gone(pid).await;
}

#[tokio::test]
async fn a_cancellation_ends_the_whole_process_group() {
    let directory = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let pending = tokio::spawn({
        let spec = group_spec(&directory, Duration::from_secs(30));
        let cancel = cancel.clone();
        async move { run(&local(), spec, cancel).await }
    });
    let pid = background_pid(&directory.path().join("pid")).await;
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("a cancelled command did not end")
        .unwrap();
    assert!(matches!(outcome, ExecutionOutcome::OutcomeUnknown { .. }));
    wait_until_gone(pid).await;
}

#[tokio::test]
async fn a_command_inherits_the_worker_environment_under_its_own_variables() {
    let directory = tempfile::tempdir().unwrap();
    let inherited = std::env::var("PATH").unwrap();
    let output = completed(
        run(
            &local(),
            CommandSpec {
                env: [("HOME".into(), "/replaced/home".into())].into(),
                ..spec(
                    "printf '%s\\n%s' \"$PATH\" \"$HOME\"",
                    &canonical(&directory),
                    Duration::from_secs(30),
                )
            },
            CancellationToken::new(),
        )
        .await,
    );
    assert_eq!(output.stdout.text(), format!("{inherited}\n/replaced/home"));
}

#[tokio::test]
async fn input_the_command_never_reads_does_not_hold_it() {
    let directory = tempfile::tempdir().unwrap();
    let output = completed(
        run(
            &local(),
            CommandSpec {
                stdin: vec![b'x'; 4 * 1024 * 1024],
                ..spec("exit 0", &canonical(&directory), Duration::from_secs(30))
            },
            CancellationToken::new(),
        )
        .await,
    );
    assert_eq!(output.exit.code, Some(0));
    assert!(!output.timed_out);
}

#[tokio::test]
async fn preparation_refuses_a_command_it_cannot_run() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = canonical(&directory);
    let prepare = |environment: LocalEnvironment, spec: CommandSpec| async move {
        let scope = scope(std::path::Path::new(&spec.cwd));
        let authority = ExecutionAuthority::new(scope.chat);
        let port = environment.bind(scope, authority).unwrap();
        port.prepare(ExecutionIntent::RunCommand(spec))
            .await
            .err()
            .expect("the preparation was accepted")
    };
    let long = Duration::from_secs(30);
    assert_eq!(
        prepare(LocalEnvironment::bare(), spec("true", &cwd, long))
            .await
            .code,
        "Unsupported"
    );
    assert_eq!(
        prepare(local(), spec("true", &format!("{cwd}/missing"), long))
            .await
            .code,
        "InvalidInput"
    );
    assert_eq!(
        prepare(local(), spec("true", "relative", long)).await.code,
        "InvalidInput"
    );
    assert_eq!(
        prepare(local(), spec("  ", &cwd, long)).await.code,
        "InvalidInput"
    );
    let memory = MemoryEnvironment::new();
    let scope = scope(directory.path());
    let authority = ExecutionAuthority::new(scope.chat);
    let port = memory.bind(scope, authority).unwrap();
    assert_eq!(
        port.prepare(ExecutionIntent::RunCommand(spec("true", &cwd, long)))
            .await
            .err()
            .unwrap()
            .code,
        "ProgramNotFound"
    );
}
