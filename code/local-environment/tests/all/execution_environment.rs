use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tickloop_code_local_environment::environment::LocalEnvironment;
use tickloop_code_local_environment::hosted::{
    shells::OwnTickhost,
    tools::HostedTickTools,
    worker_control::{DaemonControl, WorkerControlGrant},
};
use tickloop_harness::runtime::environment::{
    ExecutionAuthority, ExecutionEnvironment, ExecutionIntent, ExecutionOutcome, ExecutionQuery,
    ExecutionScope, FolderEnvironment, OpenSpec,
};
use tokio_util::sync::CancellationToken;

struct PortOnlyEnvironment(
    tickloop_harness::test_support::execution_environment::MemoryEnvironment,
);

#[tokio::test]
async fn a_prepared_open_refuses_an_in_place_program_change_with_preserved_mtime() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("program");
    std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let modified = std::fs::metadata(&program).unwrap().modified().unwrap();
    let environment = LocalEnvironment::with_ticks(Arc::new(HostedTickTools::new(
        DaemonControl::new(WorkerControlGrant {
            endpoint: directory.path().join("absent.sock"),
            host_endpoint: directory.path().join("host.sock"),
            daemon_epoch: tickloop_ids::generate(),
            binding: tickloop_ids::generate(),
            capability: "test-only".into(),
        }),
        Vec::new(),
    )));
    tickloop_harness::test_support::execution_contracts::program_change_contract(
        &environment,
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: None,
            additional_directories: Vec::new(),
        },
        OpenSpec {
            command: vec![program.to_string_lossy().into_owned()],
            cwd: Some(directory.path().to_string_lossy().into_owned()),
            protocol: false,
        },
        || {
            std::fs::write(&program, "#!/bin/sh\nexit 1\n").unwrap();
            std::fs::File::open(&program)
                .unwrap()
                .set_modified(modified)
                .unwrap();
        },
    )
    .await;
}

#[tokio::test]
async fn shell_approval_is_bound_to_the_definition_prepared_before_the_question() {
    use serde_json::json;
    use tickloop_harness::protocol::EventKind;
    use tickloop_harness::runtime::{Agent, HarnessOptions, HarnessSeams};
    use tickloop_harness::test_support::{
        ScriptedModel, call, config, harness::ChosenModels, next_event, text,
    };
    let directory = tempfile::tempdir().unwrap();
    let environment =
        Arc::new(tickloop_harness::test_support::execution_environment::MemoryEnvironment::new());
    environment.define_shell("job", std::time::Duration::ZERO, b"approved".to_vec(), 0);
    let model = Arc::new(ScriptedModel::new(vec![
        call("shell", "Bash", json!({"command":"job"})),
        text("done"),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(environment.clone()),
            models: ChosenModels::new(model),
        },
        HarnessOptions::local(config()),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Run job".into()).await.unwrap();
    let mut asked = false;
    let mut rejected = false;
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                asked = true;
                environment.define_shell(
                    "job",
                    std::time::Duration::ZERO,
                    b"unapproved".to_vec(),
                    0,
                );
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "shell" => {
                assert!(
                    result.is_error && result.content.contains("changed before dispatch"),
                    "{}",
                    result.content
                );
                rejected = true;
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(asked && rejected);
    agent.shutdown().await;
}

#[tokio::test]
async fn native_environment_shell_output_and_state_have_one_owner() {
    use serde_json::json;
    use tickloop_harness::protocol::{EventKind, ExecutionMode};
    use tickloop_harness::runtime::{Agent, HarnessOptions, HarnessSeams};
    use tickloop_harness::test_support::{
        ScriptedModel, call, config, harness::ChosenModels, next_event, text,
    };
    let directory = tempfile::tempdir().unwrap();
    let tools = Arc::new(HostedTickTools::new(
        DaemonControl::new(WorkerControlGrant {
            endpoint: directory.path().join("unused-daemon.sock"),
            host_endpoint: directory.path().join("unused-host.sock"),
            daemon_epoch: tickloop_ids::generate(),
            binding: tickloop_ids::generate(),
            capability: "test-only".into(),
        }),
        Vec::new(),
    ));
    let environment = Arc::new(LocalEnvironment::with(
        Arc::new(OwnTickhost::new()),
        tools.clone(),
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call("first", "Bash", json!({"command":"printf 'native\\n'"})),
        text("first done"),
        call(
            "second",
            "Bash",
            json!({"command":"exec /bin/sleep 300", "run_in_background":true}),
        ),
        text("second done"),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(environment.clone()),
            models: ChosenModels::new(model),
        },
        HarnessOptions::local(config()),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Print native".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "first" => assert_eq!(result.content, "native"),
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    environment.install(&agent);
    agent.install_host_tools(tools).unwrap();
    agent
        .submit(chat, "Run the background command".into())
        .await
        .unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    let running = agent
        .commands(chat)
        .await
        .unwrap()
        .into_iter()
        .find(|command| command.command == "exec /bin/sleep 300")
        .unwrap();
    let port = environment
        .bind(
            ExecutionScope {
                chat,
                folder: Some(directory.path().to_string_lossy().into_owned()),
                additional_directories: Vec::new(),
            },
            ExecutionAuthority::new(chat),
        )
        .unwrap();
    let tickloop_harness::runtime::environment::ExecutionInspection::Shell(status) = port
        .inspect(ExecutionQuery::ShellStatus(
            tick_protocol::InstanceRef::shell(running.operation_id),
        ))
        .await
        .unwrap()
    else {
        panic!("missing Shell status");
    };
    assert_eq!(status.pid, running.pid);
    agent
        .stop_command(chat, running.operation_id)
        .await
        .unwrap();
    let checkpoint = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(checkpoint) = agent.prepare_upgrade().await.unwrap() {
                break checkpoint;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let checkpoint = serde_json::to_value(checkpoint).unwrap();
    let retained = checkpoint["chats"][0]["shells"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["operation"] == json!(running.operation_id))
        .unwrap();
    let can_transfer = retained["handoff"].is_object();
    agent.cancel_upgrade().await;
    agent.shutdown().await;
    assert!(
        can_transfer,
        "late provider installation must keep the ended Shell's transfer authority"
    );
}

#[tokio::test]
async fn preparations_with_one_operation_keep_distinct_facts_on_both_adapters() {
    use tickloop_harness::runtime::environment::ShellSpec;
    use tickloop_harness::test_support::{
        execution_contracts::preparation_identity_contract,
        execution_environment::MemoryEnvironment,
    };
    let memory = MemoryEnvironment::new();
    memory.define_shell("pwd", std::time::Duration::ZERO, b"approved\n".to_vec(), 0);
    memory.define_shell(
        "printf wrong",
        std::time::Duration::ZERO,
        b"wrong".to_vec(),
        0,
    );
    let first = ShellSpec {
        folder: None,
        dangerously_disable_sandbox: false,
        additional_directories: Vec::new(),
        family_temp: None,
        operation: tickloop_ids::generate(),
        shell: "/bin/sh".into(),
        command: "pwd".into(),
        cwd: "/a".into(),
        output: "output".into(),
        description: None,
    };
    let second = ShellSpec {
        command: "printf wrong".into(),
        cwd: "/b".into(),
        ..first.clone()
    };
    preparation_identity_contract(
        &memory,
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: Some("/".into()),
            additional_directories: Vec::new(),
        },
        first.clone(),
        second.clone(),
        b"approved\n",
        b"wrong",
    )
    .await;
    let directory = tempfile::Builder::new()
        .prefix("epi")
        .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
        .unwrap();
    let root = directory.path().canonicalize().unwrap();
    let approved = root.join("a");
    let changed = root.join("b");
    std::fs::create_dir(&approved).unwrap();
    std::fs::create_dir(&changed).unwrap();
    let first = ShellSpec {
        cwd: approved.to_string_lossy().into_owned(),
        output: root.join("output").to_string_lossy().into_owned(),
        ..first
    };
    let second = ShellSpec {
        cwd: changed.to_string_lossy().into_owned(),
        ..second
    };
    let environment = LocalEnvironment::with_shells(Arc::new(OwnTickhost::new()));
    preparation_identity_contract(
        &environment,
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: Some(root.to_string_lossy().into_owned()),
            additional_directories: Vec::new(),
        },
        first,
        second,
        format!("{}\n", approved.display()).as_bytes(),
        b"wrong",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ended_shell_releases_its_output_fd_and_refuses_fifo_replacement_without_waiting() {
    use std::os::{
        fd::FromRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    };
    use tickloop_harness::runtime::environment::{ExecutionInspection, ShellSpec};
    let directory = tempfile::Builder::new()
        .prefix("epf")
        .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
        .unwrap();
    let root = directory.path().canonicalize().unwrap();
    let output = root.join("output");
    let environment = LocalEnvironment::with_shells(Arc::new(OwnTickhost::with_idle_timeout(
        std::time::Duration::ZERO,
    )));
    let chat = tickloop_ids::generate().into();
    let authority = ExecutionAuthority::new(chat);
    let port = environment
        .bind(
            ExecutionScope {
                chat,
                folder: Some(root.to_string_lossy().into_owned()),
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::SpawnShell(ShellSpec {
            folder: None,
            dangerously_disable_sandbox: false,
            additional_directories: Vec::new(),
            family_temp: None,
            operation: tickloop_ids::generate(),
            shell: "/bin/sh".into(),
            command: "printf done".into(),
            cwd: root.to_string_lossy().into_owned(),
            output: output.to_string_lossy().into_owned(),
            description: None,
        }))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(started) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("spawn failed");
    };
    let instance = started.shell.unwrap().instance;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if matches!(port.inspect(ExecutionQuery::ShellStatus(instance.clone())).await.unwrap(), ExecutionInspection::Shell(status) if status.state == tickloop_harness::protocol::CommandState::Completed) { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let identity = std::fs::metadata(&output).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let held = std::fs::read_dir("/dev/fd")
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| {
                    let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
                        return false;
                    };
                    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
                    if copy < 0 {
                        return false;
                    }
                    let copy = unsafe { std::fs::File::from_raw_fd(copy) };
                    copy.metadata().is_ok_and(|metadata| {
                        metadata.dev() == identity.dev() && metadata.ino() == identity.ino()
                    })
                });
            if !held {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the completed Shell retained its output FD");
    std::fs::remove_file(&output).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&output)
            .status()
            .unwrap()
            .success()
    );
    let mut reading = tokio::spawn({
        let port = port.clone();
        async move {
            port.inspect(ExecutionQuery::ShellOutput {
                instance,
                offset: 0,
                limit: 128,
            })
            .await
        }
    });
    let before_writer = tokio::time::timeout(std::time::Duration::from_secs(2), &mut reading).await;
    let refused_without_writer = match before_writer {
        Ok(result) => result.unwrap().is_err(),
        Err(_) => {
            // Release a blocking implementation so a failing test still cleans up its runtime.
            let writer = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&output)
                .unwrap();
            drop(writer);
            let _ = reading.await;
            false
        }
    };
    assert!(
        refused_without_writer,
        "a replacement FIFO must not block output inspection"
    );
}

impl ExecutionEnvironment for PortOnlyEnvironment {
    fn bind(
        &self,
        scope: ExecutionScope,
        authority: ExecutionAuthority,
    ) -> Result<
        Arc<dyn tickloop_harness::runtime::environment::ExecutionPort>,
        tickloop_harness::runtime::environment::ExecutionError,
    > {
        self.0.bind(scope, authority)
    }
}

#[tokio::test]
async fn a_harness_uses_its_injected_environment_for_bash() {
    use serde_json::json;
    use tickloop_harness::protocol::{EventKind, ExecutionMode};
    use tickloop_harness::runtime::{Agent, HarnessOptions, HarnessSeams};
    use tickloop_harness::test_support::{
        ScriptedModel, call, config, harness::ChosenModels, next_event, text,
    };
    let directory = tempfile::tempdir().unwrap();
    let environment = Arc::new(PortOnlyEnvironment(
        tickloop_harness::test_support::execution_environment::MemoryEnvironment::new(),
    ));
    environment.0.define_shell(
        "job",
        std::time::Duration::ZERO,
        b"from-memory\n".to_vec(),
        0,
    );
    let model = Arc::new(ScriptedModel::new(vec![
        call("shell", "Bash", json!({"command":"job"})),
        text("done"),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(environment),
            models: ChosenModels::new(model),
        },
        HarnessOptions::local(config()),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Run job".into()).await.unwrap();
    let mut ran = false;
    loop {
        match next_event(&mut events).await.kind {
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "shell" => {
                assert!(!result.is_error, "{}", result.content);
                assert!(result.content.contains("from-memory"), "{}", result.content);
                ran = true;
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(ran);
    agent.shutdown().await;
}

#[tokio::test]
async fn local_and_memory_shells_share_the_stop_contract() {
    use std::time::Duration;
    use tickloop_harness::runtime::environment::ShellSpec;
    use tickloop_harness::test_support::execution_contracts::shell_stop_contract;
    use tickloop_harness::test_support::execution_environment::MemoryEnvironment;
    let memory = MemoryEnvironment::new();
    memory.define_shell("job", Duration::from_secs(300), Vec::new(), 0);
    shell_stop_contract(
        &memory,
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: Some("/".into()),
            additional_directories: Vec::new(),
        },
        ShellSpec {
            folder: None,
            dangerously_disable_sandbox: false,
            additional_directories: Vec::new(),
            family_temp: None,
            operation: tickloop_ids::generate(),
            shell: "/bin/sh".into(),
            command: "job".into(),
            cwd: "/".into(),
            output: "output".into(),
            description: None,
        },
    )
    .await;
    let directory = tempfile::Builder::new()
        .prefix("ep")
        .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
        .unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let output = cwd.join("output");
    let local = LocalEnvironment::with_shells(Arc::new(OwnTickhost::new()));
    shell_stop_contract(
        &local,
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: Some(cwd.to_string_lossy().into_owned()),
            additional_directories: Vec::new(),
        },
        ShellSpec {
            folder: None,
            dangerously_disable_sandbox: false,
            additional_directories: Vec::new(),
            family_temp: None,
            operation: tickloop_ids::generate(),
            shell: "/bin/sh".into(),
            command: "exec /bin/sleep 300".into(),
            cwd: cwd.to_string_lossy().into_owned(),
            output: output.to_string_lossy().into_owned(),
            description: None,
        },
    )
    .await;
}

#[tokio::test]
async fn local_and_memory_shells_share_the_exit_contract() {
    use std::time::Duration;
    use tickloop_harness::runtime::environment::ShellSpec;
    use tickloop_harness::test_support::execution_contracts::shell_exit_contract;
    use tickloop_harness::test_support::execution_environment::MemoryEnvironment;
    let memory = MemoryEnvironment::new();
    memory.define_shell("job", Duration::from_secs(30), b"done\n".to_vec(), 0);
    shell_exit_contract(
        &memory,
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: Some("/".into()),
            additional_directories: Vec::new(),
        },
        ShellSpec {
            folder: None,
            dangerously_disable_sandbox: false,
            additional_directories: Vec::new(),
            family_temp: None,
            operation: tickloop_ids::generate(),
            shell: "/bin/sh".into(),
            command: "job".into(),
            cwd: "/".into(),
            output: "output".into(),
            description: None,
        },
        || async {
            memory.advance(Duration::from_secs(30));
        },
    )
    .await;

    let directory = tempfile::Builder::new()
        .prefix("ep")
        .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
        .unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let gate = cwd.join("gate");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success()
    );
    let output = cwd.join("output");
    let local = LocalEnvironment::with_shells(Arc::new(OwnTickhost::new()));
    shell_exit_contract(
        &local,
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: Some(cwd.to_string_lossy().into_owned()),
            additional_directories: Vec::new(),
        },
        ShellSpec {
            folder: None,
            dangerously_disable_sandbox: false,
            additional_directories: Vec::new(),
            family_temp: None,
            operation: tickloop_ids::generate(),
            shell: "/bin/sh".into(),
            command: "read ignored < gate; printf 'done\\n'".into(),
            cwd: cwd.to_string_lossy().into_owned(),
            output: output.to_string_lossy().into_owned(),
            description: None,
        },
        || async {
            tokio::fs::write(&gate, b"continue\n").await.unwrap();
        },
    )
    .await;
}

#[tokio::test]
async fn shell_observation_reports_one_exit_and_late_observers_only_get_a_baseline() {
    use std::time::Duration;
    use tickloop_harness::protocol::CommandState;
    use tickloop_harness::runtime::environment::{ExecutionInspection, ObserveRequest, ShellSpec};
    let environment =
        tickloop_harness::test_support::execution_environment::MemoryEnvironment::new();
    environment.define_shell("job", Duration::from_secs(30), b"done\n".to_vec(), 0);
    let chat = tickloop_ids::generate().into();
    let authority = ExecutionAuthority::new(chat);
    let port = environment
        .bind(
            ExecutionScope {
                chat,
                folder: Some("/".into()),
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::SpawnShell(ShellSpec {
            folder: None,
            dangerously_disable_sandbox: false,
            additional_directories: Vec::new(),
            family_temp: None,
            operation: tickloop_ids::generate(),
            shell: "/bin/sh".into(),
            command: "job".into(),
            cwd: "/".into(),
            output: "job-output".into(),
            description: None,
        }))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(started) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("spawn failed");
    };
    let instance = started.shell.unwrap().instance;
    let mut observation = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: None,
        })
        .await
        .unwrap();
    environment.advance(Duration::from_secs(29));
    assert!(!observation.updates.has_changed().unwrap());
    environment.advance(Duration::from_secs(1));
    observation.updates.changed().await.unwrap();
    assert_eq!(
        observation
            .updates
            .borrow_and_update()
            .update
            .as_ref()
            .unwrap()
            .data["state"],
        "Completed"
    );
    environment.advance(Duration::from_secs(30));
    assert!(!observation.updates.has_changed().unwrap());
    let late = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(late.baseline.state.data["state"], "Completed");
    assert!(!late.updates.has_changed().unwrap());
    let ExecutionInspection::Shell(status) = port
        .inspect(ExecutionQuery::ShellStatus(instance.clone()))
        .await
        .unwrap()
    else {
        panic!("missing Shell status");
    };
    assert_eq!(status.state, CommandState::Completed);
    let ExecutionInspection::Output(output) = port
        .inspect(ExecutionQuery::ShellOutput {
            instance,
            offset: 0,
            limit: 1024,
        })
        .await
        .unwrap()
    else {
        panic!("missing Shell output");
    };
    assert_eq!(output.data, b"done\n");
}

#[tokio::test]
async fn a_host_restart_ends_the_old_observation_until_an_explicit_reopen() {
    use tickloop_harness::runtime::environment::{ExecutionLinkState, ObserveRequest};
    let environment =
        tickloop_harness::test_support::execution_environment::MemoryEnvironment::new();
    environment.install_program("/terminal");
    let chat = tickloop_ids::generate().into();
    let authority = ExecutionAuthority::new(chat);
    let port = environment
        .bind(
            ExecutionScope {
                chat,
                folder: None,
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::Open(OpenSpec {
            command: vec!["/terminal".into()],
            cwd: Some("/".into()),
            protocol: false,
        }))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(opened) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("open failed");
    };
    let old = opened.opened.unwrap().instance;
    let mut observation = port
        .observe(ObserveRequest {
            instance: old.clone(),
            after: None,
        })
        .await
        .unwrap();
    environment.restart();
    observation.updates.changed().await.unwrap();
    assert_eq!(
        observation.updates.borrow_and_update().state,
        ExecutionLinkState::Gone
    );
    assert!(
        port.inspect(ExecutionQuery::Kind(old.clone()))
            .await
            .is_err()
    );
    let prepared = port
        .prepare(ExecutionIntent::Reopen {
            instance: old.clone(),
        })
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(reopened) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("reopen failed");
    };
    assert_ne!(reopened.opened.unwrap().instance, old);
    assert!(
        port.observe(ObserveRequest {
            instance: old,
            after: Some(observation.cursor)
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn generic_tick_tools_send_prepared_input_to_the_same_terminal() {
    use serde_json::json;
    use tickloop_harness::protocol::{HostEndpointIdentity, HostToolAuthority};
    use tickloop_harness::runtime::{environment::ExecutionTools, host_tools::HostToolProvider};
    let environment =
        Arc::new(tickloop_harness::test_support::execution_environment::MemoryEnvironment::new());
    environment.install_program("/terminal");
    let tools = ExecutionTools::new(
        environment,
        HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::TrustedAdapter {
                scope: tickloop_ids::generate(),
            },
        },
    );
    let chat = tickloop_ids::generate().into();
    let prepared = tools
        .prepare_call(
            chat,
            "TickManage",
            &json!({"action":"open", "command":["/terminal"], "cwd":"/"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let opened = tools
        .execute_prepared(chat, prepared, CancellationToken::new())
        .await
        .into_tool_result();
    let instance =
        serde_json::from_str::<serde_json::Value>(&opened.content).unwrap()["tick_instance_id"]
            .clone();
    let prepared = tools
        .prepare_call(
            chat,
            "TickManage",
            &json!({"action":"send_input", "tick_instance_id":instance, "text":"hello"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        !tools
            .execute_prepared(chat, prepared, CancellationToken::new())
            .await
            .into_tool_result()
            .is_error
    );
    let prepared = tools
        .prepare_call(
            chat,
            "TickRead",
            &json!({"tick_instance_id":instance}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let read = tools
        .execute_prepared(chat, prepared, CancellationToken::new())
        .await
        .into_tool_result();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&read.content).unwrap()["data"]["text"],
        "hello"
    );
}

#[tokio::test]
async fn generic_tick_tools_keep_a_reported_tool_failure_as_a_known_result() {
    use serde_json::json;
    use tickloop_harness::protocol::{HostEndpointIdentity, HostToolAuthority};
    use tickloop_harness::runtime::{
        environment::ExecutionTools,
        host_tools::{HostCallResult, HostToolProvider},
    };
    let environment =
        Arc::new(tickloop_harness::test_support::execution_environment::MemoryEnvironment::new());
    environment.install_tick_program("/counter", serde_json::from_value(json!({
        "protocol":3, "kind":"example.counter", "title":"Counter", "description":"Counter", "revision":"1",
        "tools":[{"name":"jam", "description":"A failing tool", "input_schema":{"type":"object"}, "mutates":true}],
    })).unwrap(), json!({"count":0}), |_, _, _| Err("the counter is jammed".into()));
    let tools = ExecutionTools::new(
        environment,
        HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::TrustedAdapter {
                scope: tickloop_ids::generate(),
            },
        },
    );
    let chat = tickloop_ids::generate().into();
    let prepared = tools
        .prepare_call(
            chat,
            "TickManage",
            &json!({"action":"open", "command":["/counter"], "cwd":"/", "protocol":true}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let opened = tools
        .execute_prepared(chat, prepared, CancellationToken::new())
        .await
        .into_tool_result();
    let instance =
        serde_json::from_str::<serde_json::Value>(&opened.content).unwrap()["tick_instance_id"]
            .clone();
    let prepared = tools
        .prepare_call(
            chat,
            "TickCall",
            &json!({"tick_instance_id":instance, "tool":"jam", "args":{}}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let HostCallResult::Completed(failed) = tools
        .execute_prepared(chat, prepared, CancellationToken::new())
        .await
    else {
        panic!("ToolFailed is a completed result");
    };
    assert!(failed.is_error);
    assert!(failed.content.contains("jammed"));
}

#[tokio::test]
async fn terminal_sampling_ignores_a_screen_that_changed_back_to_its_baseline() {
    use std::collections::BTreeMap;
    use tickloop_harness::protocol::{HostEndpointIdentity, HostToolAuthority};
    use tickloop_harness::runtime::tick_stream::{
        HostedTickStream, StreamTasks, TickStreamEvent, TickStreamPort, WatchId,
    };
    let environment =
        Arc::new(tickloop_harness::test_support::execution_environment::MemoryEnvironment::new());
    environment.install_program("/terminal");
    let chat = tickloop_ids::generate();
    let authority = ExecutionAuthority::new(chat.into());
    let port = environment
        .bind(
            ExecutionScope {
                chat: chat.into(),
                folder: None,
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::Open(OpenSpec {
            command: vec!["/terminal".into()],
            cwd: Some("/".into()),
            protocol: false,
        }))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(opened) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("open failed");
    };
    let instance = opened.opened.unwrap().instance;
    let host_authority = HostToolAuthority::Native {
        endpoint: HostEndpointIdentity::TrustedAdapter {
            scope: tickloop_ids::generate(),
        },
    };
    let stream = HostedTickStream::with_environment(environment.clone(), host_authority.clone());
    let watch = WatchId::new();
    stream.set_watch_chats(BTreeMap::from([(watch, chat)]));
    stream.set_watch_authorities(BTreeMap::from([(watch, host_authority)]));
    let mut handle = stream.open(&StreamTasks::default());
    handle
        .desired
        .send_replace(BTreeMap::from([(watch, instance.clone())]));
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event, TickStreamEvent::Ready { .. }) {
            break;
        }
    }
    environment.set_terminal_screen(&instance, "temporary");
    environment.set_terminal_screen(&instance, "");
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            event,
            TickStreamEvent::ScreenChange {
                is_pending: true,
                ..
            }
        ) {
            break;
        }
    }
    assert!(stream.sample(watch, &instance).await.is_none());
    environment.set_terminal_screen(&instance, "new content");
    let sampled = stream.sample(watch, &instance).await.unwrap();
    assert_eq!(sampled.data["text"], "new content");
    assert!(stream.sample(watch, &instance).await.is_none());
}

#[tokio::test]
async fn the_harness_stream_consumes_memory_environment_facts_without_tickhost() {
    use serde_json::json;
    use std::collections::BTreeMap;
    use tickloop_harness::protocol::{HostEndpointIdentity, HostToolAuthority};
    use tickloop_harness::runtime::tick_stream::{
        HostedTickStream, StreamTasks, TickStreamEvent, TickStreamPort, WatchId,
    };
    let environment =
        Arc::new(tickloop_harness::test_support::execution_environment::MemoryEnvironment::new());
    environment.install_tick_program("/counter", serde_json::from_value(json!({
        "protocol":3, "kind":"example.counter", "title":"Counter", "description":"Counter", "revision":"1", "tools":[],
    })).unwrap(), json!({"count":0}), |_, _, _| Ok(json!(null)));
    let chat = tickloop_ids::generate();
    let authority = ExecutionAuthority::new(chat.into());
    let port = environment
        .bind(
            ExecutionScope {
                chat: chat.into(),
                folder: None,
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::Open(OpenSpec {
            command: vec!["/counter".into()],
            cwd: Some("/".into()),
            protocol: true,
        }))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(opened) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("open failed");
    };
    let instance = opened.opened.unwrap().instance;
    let host_authority = HostToolAuthority::Native {
        endpoint: HostEndpointIdentity::TrustedAdapter {
            scope: tickloop_ids::generate(),
        },
    };
    let stream = HostedTickStream::with_environment(environment.clone(), host_authority.clone());
    let watch = WatchId::new();
    stream.set_watch_chats(BTreeMap::from([(watch, chat)]));
    stream.set_watch_authorities(BTreeMap::from([(watch, host_authority)]));
    let mut handle = stream.open(&StreamTasks::default());
    handle
        .desired
        .send_replace(BTreeMap::from([(watch, instance.clone())]));
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event, TickStreamEvent::Ready { watch: ready } if ready == watch) {
            break;
        }
        assert!(
            !matches!(event, TickStreamEvent::Observation { .. }),
            "baseline is not a delivery"
        );
    }
    environment.publish(&instance, json!({"count":1}));
    environment.publish(&instance, json!({"count":2}));
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        if let TickStreamEvent::Observation { value, .. } = event {
            assert_eq!(value.data, json!({"count":2}));
            break;
        }
    }
}

#[tokio::test]
async fn observation_registers_its_own_baseline_and_continues_from_a_cursor() {
    use serde_json::json;
    use tickloop_harness::runtime::environment::ObserveRequest;
    let environment =
        tickloop_harness::test_support::execution_environment::MemoryEnvironment::new();
    environment.install_tick_program("/counter", serde_json::from_value(json!({
        "protocol":3, "kind":"example.counter", "title":"Counter", "description":"Counter", "revision":"1", "tools":[],
    })).unwrap(), json!({"count":0}), |_, _, _| Ok(json!(null)));
    let chat = tickloop_ids::generate().into();
    let authority = ExecutionAuthority::new(chat);
    let port = environment
        .bind(
            ExecutionScope {
                chat,
                folder: None,
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::Open(OpenSpec {
            command: vec!["/counter".into()],
            cwd: Some("/".into()),
            protocol: true,
        }))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(opened) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("open failed");
    };
    let instance = opened.opened.unwrap().instance;
    let mut first = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(first.baseline.state.data, json!({"count":0}));
    environment.publish(&instance, json!({"count":1}));
    let late = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: None,
        })
        .await
        .unwrap();
    assert_eq!(late.baseline.state.data, json!({"count":1}));
    environment.publish(&instance, json!({"count":2}));
    first.updates.changed().await.unwrap();
    let update = first.updates.borrow_and_update().clone();
    assert_eq!(update.update.unwrap().data, json!({"count":2}));
    let resumed = port
        .observe(ObserveRequest {
            instance: instance.clone(),
            after: Some(late.cursor),
        })
        .await
        .unwrap();
    assert!(!resumed.gap);
    assert_eq!(resumed.baseline.state.data, json!({"count":2}));
    environment.expire_observation_history();
    let expired = port
        .observe(ObserveRequest {
            instance,
            after: Some(first.cursor),
        })
        .await
        .unwrap();
    assert!(expired.gap, "an expired cursor must not claim continuity");
}

#[tokio::test]
async fn a_tick_checks_the_prepared_revision_before_running_the_call() {
    use serde_json::json;
    use tickloop_harness::runtime::environment::ExecutionInspection;
    let environment =
        tickloop_harness::test_support::execution_environment::MemoryEnvironment::new();
    environment.install_tick_program("/counter", serde_json::from_value(json!({
        "protocol":3, "kind":"example.counter", "title":"Counter", "description":"Counter", "revision":"1",
        "tools":[{"name":"increment", "description":"Increment", "input_schema":{"type":"object"}, "mutates":true}],
    })).unwrap(), json!({"count":0}), |data, _, _| {
        data["count"] = json!(data["count"].as_u64().unwrap() + 1);
        Ok(data.clone())
    });
    let chat = tickloop_ids::generate().into();
    let authority = ExecutionAuthority::new(chat);
    let port = environment
        .bind(
            ExecutionScope {
                chat,
                folder: None,
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::Open(OpenSpec {
            command: vec!["/counter".into()],
            cwd: Some("/".into()),
            protocol: true,
        }))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(opened) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("open failed");
    };
    let instance = opened.opened.unwrap().instance;
    let prepared = port
        .prepare(ExecutionIntent::Call {
            instance: instance.clone(),
            tool: "increment".into(),
            args: json!({}),
            revision: "1".into(),
        })
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    environment.change_revision(&instance, "2");
    assert!(
        matches!(port.execute(prepared, admission, CancellationToken::new()).await, ExecutionOutcome::NotDispatched(error) if error.code == "RevisionMismatch")
    );
    let ExecutionInspection::Read(read) =
        port.inspect(ExecutionQuery::Read(instance)).await.unwrap()
    else {
        panic!("missing state");
    };
    assert_eq!(read.state.data, json!({"count":0}));
}

#[tokio::test]
async fn a_native_tool_preparation_cannot_be_executed_for_another_chat() {
    use tickloop_harness::runtime::host_tools::{HostCallResult, HostToolProvider};
    let directory = tempfile::tempdir().unwrap();
    let tools = HostedTickTools::new(
        DaemonControl::new(WorkerControlGrant {
            endpoint: directory.path().join("absent-daemon.sock"),
            host_endpoint: directory.path().join("absent-host.sock"),
            daemon_epoch: tickloop_ids::generate(),
            binding: tickloop_ids::generate(),
            capability: "test-only".into(),
        }),
        Vec::new(),
    );
    for input in [
        serde_json::json!({"action":"open", "command":["/bin/sh"], "cwd":directory.path()}),
        serde_json::json!({"action":"close", "tick_instance_id":tick_protocol::InstanceRef::hosted(tickloop_ids::generate())}),
    ] {
        let prepared = tools
            .prepare_call(
                tickloop_ids::generate().into(),
                "TickManage",
                &input,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match tools
            .execute_prepared(
                tickloop_ids::generate().into(),
                prepared,
                CancellationToken::new(),
            )
            .await
        {
            HostCallResult::NotDispatched(reason) => {
                assert!(reason.contains("another Chat"), "{reason}")
            }
            _ => panic!("a foreign Chat must be rejected before dispatch"),
        }
    }
}

#[tokio::test]
async fn revoking_authority_invalidates_even_reads_on_an_existing_port() {
    let chat = tickloop_ids::generate().into();
    let authority = ExecutionAuthority::new(chat);
    let port = FolderEnvironment::bare()
        .bind(
            ExecutionScope {
                chat,
                folder: None,
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    assert!(port.inspect(ExecutionQuery::Capabilities).await.is_ok());
    authority.revoke();
    assert_eq!(
        port.inspect(ExecutionQuery::Capabilities)
            .await
            .unwrap_err()
            .code,
        "AuthorityRevoked"
    );
}

#[tokio::test]
async fn a_lost_open_reply_can_only_be_queried_and_does_not_open_again() {
    use tickloop_harness::runtime::environment::{ExecutionInspection, ExecutionReceipt};
    let environment =
        tickloop_harness::test_support::execution_environment::MemoryEnvironment::new();
    environment.install_program("/program");
    let chat = tickloop_ids::generate().into();
    let authority = ExecutionAuthority::new(chat);
    let port = environment
        .bind(
            ExecutionScope {
                chat,
                folder: None,
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::Open(OpenSpec {
            command: vec!["/program".into()],
            cwd: Some("/".into()),
            protocol: false,
        }))
        .await
        .unwrap();
    let admission = authority.admit(&prepared).unwrap();
    let operation = prepared.operation();
    environment.lose_next_reply();
    assert!(matches!(
        port.execute(prepared, admission, CancellationToken::new())
            .await,
        ExecutionOutcome::OutcomeUnknown { .. }
    ));
    for _ in 0..2 {
        assert!(matches!(
            port.inspect(ExecutionQuery::Receipt(operation))
                .await
                .unwrap(),
            ExecutionInspection::Receipt(ExecutionReceipt::Completed(_))
        ));
    }
    match port.inspect(ExecutionQuery::Instances).await.unwrap() {
        ExecutionInspection::Instances(instances) => assert_eq!(instances.len(), 1),
        other => panic!("unexpected inspection {other:?}"),
    }
    let foreign_chat = tickloop_ids::generate().into();
    let foreign = environment
        .bind(
            ExecutionScope {
                chat: foreign_chat,
                folder: None,
                additional_directories: Vec::new(),
            },
            ExecutionAuthority::new(foreign_chat),
        )
        .unwrap();
    assert!(matches!(
        foreign
            .inspect(ExecutionQuery::Receipt(operation))
            .await
            .unwrap(),
        ExecutionInspection::Receipt(ExecutionReceipt::Unknown)
    ));
}

#[tokio::test]
async fn a_prepared_open_refuses_a_replaced_program_before_contacting_the_host() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("program");
    std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let control = DaemonControl::new(WorkerControlGrant {
        endpoint: directory.path().join("absent-daemon.sock"),
        host_endpoint: directory.path().join("absent-host.sock"),
        daemon_epoch: tickloop_ids::generate(),
        binding: tickloop_ids::generate(),
        capability: "test-only".into(),
    });
    let environment = LocalEnvironment::with(
        Arc::new(OwnTickhost::new()),
        Arc::new(HostedTickTools::new(control, Vec::new())),
    );
    tickloop_harness::test_support::execution_contracts::program_change_contract(
        &environment,
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: None,
            additional_directories: Vec::new(),
        },
        OpenSpec {
            command: vec![program.to_string_lossy().into_owned()],
            cwd: Some(directory.path().to_string_lossy().into_owned()),
            protocol: false,
        },
        || {
            std::fs::rename(&program, directory.path().join("old-program")).unwrap();
            std::fs::write(&program, "#!/bin/sh\nexit 1\n").unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        },
    )
    .await;
}

#[tokio::test]
async fn the_memory_environment_refuses_the_same_program_replacement() {
    let environment =
        tickloop_harness::test_support::execution_environment::MemoryEnvironment::new();
    environment.install_program("/program");
    tickloop_harness::test_support::execution_contracts::program_change_contract(
        &environment,
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: None,
            additional_directories: Vec::new(),
        },
        OpenSpec {
            command: vec!["/program".into()],
            cwd: Some("/".into()),
            protocol: false,
        },
        || environment.install_program("/program"),
    )
    .await;
}

#[tokio::test]
async fn local_and_reference_hosts_report_isolation_at_preparation() {
    use tickloop_harness::runtime::environment::*;
    use tickloop_harness::test_support::execution_contracts::isolation_contract;
    let directory = tempfile::tempdir().unwrap();
    let scope = ExecutionScope {
        chat: tickloop_ids::generate().into(),
        folder: Some(directory.path().display().to_string()),
        additional_directories: Vec::new(),
    };
    let spec = ShellSpec {
        folder: None,
        dangerously_disable_sandbox: false,
        additional_directories: Vec::new(),
        family_temp: None,
        operation: tickloop_ids::generate(),
        shell: "/bin/sh".into(),
        command: "true".into(),
        cwd: directory.path().display().to_string(),
        output: directory.path().join("output").display().to_string(),
        description: None,
    };
    isolation_contract(
        &LocalEnvironment::with_shells(Arc::new(OwnTickhost::new())),
        scope.clone(),
        spec.clone(),
        ExecutionIsolation::default(),
        || {},
    )
    .await;
    let memory = tickloop_harness::test_support::execution_environment::MemoryEnvironment::new();
    memory.define_shell("true", std::time::Duration::ZERO, Vec::new(), 0);
    let isolated = ExecutionIsolation {
        writes: WriteIsolation::Environment,
        network: NetworkIsolation::Allowlist,
    };
    memory.set_isolation(isolated);
    isolation_contract(&memory, scope, spec, isolated, || {
        memory.set_isolation(ExecutionIsolation::default())
    })
    .await;
}

/// Hosted tools whose daemon is never reached: an open refused before dispatch asks nothing.
fn unreached_ticks(directory: &std::path::Path) -> LocalEnvironment {
    LocalEnvironment::with_ticks(Arc::new(HostedTickTools::new(
        DaemonControl::new(WorkerControlGrant {
            endpoint: directory.join("absent.sock"),
            host_endpoint: directory.join("host.sock"),
            daemon_epoch: tickloop_ids::generate(),
            binding: tickloop_ids::generate(),
            capability: "test-only".into(),
        }),
        Vec::new(),
    )))
}

fn write_program(path: &std::path::Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// A program opened through a link runs by the link, so the approval binds the file the link
/// names when approved: a link pointed at another program afterwards runs nothing.
#[tokio::test]
async fn a_prepared_open_refuses_a_link_pointed_at_another_program() {
    let directory = tempfile::tempdir().unwrap();
    let approved = directory.path().join("approved");
    let other = directory.path().join("other");
    write_program(&approved, "#!/bin/sh\nexit 0\n");
    write_program(&other, "#!/bin/sh\nexit 1\n");
    let link = directory.path().join("program");
    std::os::unix::fs::symlink(&approved, &link).unwrap();
    tickloop_harness::test_support::execution_contracts::program_change_contract(
        &unreached_ticks(directory.path()),
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: None,
            additional_directories: Vec::new(),
        },
        OpenSpec {
            command: vec![link.to_string_lossy().into_owned()],
            cwd: Some(directory.path().to_string_lossy().into_owned()),
            protocol: false,
        },
        || {
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&other, &link).unwrap();
        },
    )
    .await;
}

#[tokio::test]
async fn a_prepared_open_through_a_link_refuses_a_change_to_the_program_it_names() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target");
    write_program(&target, "#!/bin/sh\nexit 0\n");
    let link = directory.path().join("program");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    tickloop_harness::test_support::execution_contracts::program_change_contract(
        &unreached_ticks(directory.path()),
        ExecutionScope {
            chat: tickloop_ids::generate().into(),
            folder: None,
            additional_directories: Vec::new(),
        },
        OpenSpec {
            command: vec![link.to_string_lossy().into_owned()],
            cwd: Some(directory.path().to_string_lossy().into_owned()),
            protocol: false,
        },
        || write_program(&target, "#!/bin/sh\nexit 1\n"),
    )
    .await;
}

/// A Shell reached through a link runs by the link's path, as its `$0` shows.
#[tokio::test]
async fn a_shell_reached_through_a_link_runs_by_the_links_path() {
    use tickloop_harness::runtime::environment::{ExecutionInspection, ShellSpec};
    let directory = tempfile::Builder::new()
        .prefix("esl")
        .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
        .unwrap();
    let root = directory.path().canonicalize().unwrap();
    let shell = root.join("bin/linked-sh");
    std::fs::create_dir(root.join("bin")).unwrap();
    std::os::unix::fs::symlink("/bin/sh", &shell).unwrap();
    let output = root.join("output");
    let environment = LocalEnvironment::with_shells(Arc::new(OwnTickhost::with_idle_timeout(
        std::time::Duration::ZERO,
    )));
    let chat = tickloop_ids::generate().into();
    let authority = ExecutionAuthority::new(chat);
    let port = environment
        .bind(
            ExecutionScope {
                chat,
                folder: Some(root.to_string_lossy().into_owned()),
                additional_directories: Vec::new(),
            },
            authority.clone(),
        )
        .unwrap();
    let prepared = port
        .prepare(ExecutionIntent::SpawnShell(ShellSpec {
            folder: None,
            dangerously_disable_sandbox: false,
            additional_directories: Vec::new(),
            family_temp: None,
            operation: tickloop_ids::generate(),
            shell: shell.to_string_lossy().into_owned(),
            command: "printf '%s' \"$0\"".into(),
            cwd: root.to_string_lossy().into_owned(),
            output: output.to_string_lossy().into_owned(),
            description: None,
        }))
        .await
        .unwrap();
    let ExecutionIntent::SpawnShell(spec) = &prepared.facts().intent else {
        panic!("a Shell spawn prepares a Shell spawn");
    };
    assert_eq!(spec.shell, shell.to_string_lossy());
    let admission = authority.admit(&prepared).unwrap();
    let ExecutionOutcome::Completed(started) = port
        .execute(prepared, admission, CancellationToken::new())
        .await
    else {
        panic!("spawn failed");
    };
    let instance = started.shell.unwrap().instance;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if matches!(port.inspect(ExecutionQuery::ShellStatus(instance.clone())).await.unwrap(), ExecutionInspection::Shell(status) if status.state == tickloop_harness::protocol::CommandState::Completed) { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        shell.to_string_lossy()
    );
}
