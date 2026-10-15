//! Shell tasks under the Tickhost library: one-time spawn grants the controller asks for, pipe
//! mode in the passed directory and output file, stopping, and the lifetimes that end a task with
//! its spawner, its daemon or the host.

use std::{
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};
use tickhost::{
    host,
    paths::HostPaths,
    shell::{
        self, CANCELLED_REASON, MAX_SHELL_OUTPUT_BYTES, OUTPUT_LIMIT_REASON, ShellApproval,
        ShellExit, ShellIdentity, ShellOutcome, SpawnBinding, SpawnGrant, SpawnRequest,
        SubmittedShell, UNRESOLVED_DESCENDANTS_REASON,
    },
};
use tickhost_connect::hosted::{
    CAPABILITY_DATA, CAPABILITY_LIFECYCLE, CAPABILITY_SHELL, CONTROL_TIMEOUT, HostConnection, Role,
};

struct Fixture {
    directory: tempfile::TempDir,
    paths: HostPaths,
    key: String,
    daemon_epoch: uuid::Uuid,
    /// The connection the daemon holds for as long as it runs; Shell tasks stop without one.
    daemon: Option<HostConnection>,
    /// Stops the host as `tickhost stop` does, once sent or dropped.
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Fixture {
    async fn start() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("ths")
            .tempdir_in("/private/tmp")
            .unwrap();
        let paths = HostPaths {
            runtime: directory.path().join("h"),
        };
        let key = host::random_token();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(host::serve_until(
            paths.clone(),
            host::HostConfig {
                controller_key: key.clone(),
                idle_timeout: host::DEFAULT_IDLE_TIMEOUT,
            },
            async move {
                let _ = stopped.await;
            },
        ));
        let mut fixture = Self {
            directory,
            paths,
            key,
            daemon_epoch: uuid::Uuid::new_v4(),
            daemon: None,
            stop: Some(stop),
            server,
        };
        let deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
        loop {
            match fixture.connect_controller(fixture.daemon_epoch).await {
                Ok(daemon) => {
                    fixture.daemon = Some(daemon);
                    return fixture;
                }
                Err(_) => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "the host never accepted a controller"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }

    async fn connect_controller(
        &self,
        daemon_epoch: uuid::Uuid,
    ) -> Result<HostConnection, tickhost_connect::host_error::HostError> {
        HostConnection::connect(
            &self.paths.socket(),
            Role::Controller {
                controller_key: self.key.clone(),
                daemon_epoch,
            },
            &[CAPABILITY_LIFECYCLE],
        )
        .await
    }

    async fn controller(&self) -> HostConnection {
        self.connect_controller(self.daemon_epoch).await.unwrap()
    }

    /// A private working directory under the fixture.
    fn workdir(&self, name: &str) -> PathBuf {
        let workdir = self.directory.path().join(name);
        std::fs::create_dir(&workdir).unwrap();
        workdir.canonicalize().unwrap()
    }

    fn binding(&self, command: &str, cwd: &Path) -> SpawnBinding {
        SpawnBinding {
            operation: uuid::Uuid::new_v4(),
            identity: ShellIdentity {
                daemon_epoch: self.daemon_epoch,
                worker: uuid::Uuid::new_v4(),
                chat: uuid::Uuid::new_v4(),
            },
            approval: ShellApproval {
                shell: "/bin/sh".into(),
                command: command.into(),
                cwd: cwd.to_path_buf(),
                environment: shell::environment_digest(&[]),
            },
        }
    }
}

async fn allow(controller: &mut HostConnection, binding: &SpawnBinding) -> SpawnGrant {
    let grant = controller
        .request("allow_spawn", json!(binding), CONTROL_TIMEOUT)
        .await
        .unwrap();
    serde_json::from_value(grant).unwrap()
}

struct Started {
    submitted: SubmittedShell,
    output: PathBuf,
}

fn environment(variables: &[(&str, &str)]) -> Vec<(Vec<u8>, Vec<u8>)> {
    variables
        .iter()
        .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
        .collect()
}

fn request(binding: &SpawnBinding, variables: &[(&str, &str)]) -> SpawnRequest {
    SpawnRequest {
        binding: binding.clone(),
        description: Some("A task".into()),
        environment: environment(variables),
    }
}

fn output_file(binding: &SpawnBinding) -> (std::fs::File, PathBuf) {
    let path = binding
        .approval
        .cwd
        .join(format!("{}.output", binding.operation));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    (file, path)
}

async fn start(fixture: &Fixture, binding: &SpawnBinding, environment: &[(&str, &str)]) -> Started {
    let mut controller = fixture.controller().await;
    let grant = allow(&mut controller, binding).await;
    let directory = std::fs::File::open(&binding.approval.cwd).unwrap();
    let (output, path) = output_file(binding);
    let submitted = shell::submit(
        &fixture.paths.socket(),
        &grant,
        &request(binding, environment),
        &directory,
        &output,
    )
    .await
    .unwrap();
    Started {
        submitted,
        output: path,
    }
}

async fn exited(started: &mut Started) -> ShellExit {
    tokio::time::timeout(Duration::from_secs(10), started.submitted.exits.exited())
        .await
        .expect("the task did not end")
        .unwrap()
}

fn group_exists(pid: u32) -> bool {
    shell::process_group_exists(pid)
}

async fn group_ends(pid: u32) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while group_exists(pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "process group {pid} still runs"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_granted_task_runs_in_pipe_mode_in_its_passed_directory_and_reports_its_exit() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("pipe");
    let mut binding = fixture.binding(
        r#"printf 'out|'; printf 'err|' >&2; cat; printf '%s|' "$GREETING"; pwd -P; [ -t 0 ] || printf 'no-tty|'; exit 3"#,
        &workdir,
    );
    let variables = [("GREETING", "hello")];
    binding.approval.environment = shell::environment_digest(&environment(&variables));
    let mut started = start(&fixture, &binding, &variables).await;
    assert_eq!(
        started.submitted.spawned.instance.as_str(),
        format!("shell:{}", binding.operation)
    );
    let exit = exited(&mut started).await;
    assert_eq!(
        (exit.code, exit.signal, exit.reason.as_deref(), exit.outcome),
        (Some(3), None, None, ShellOutcome::Failed)
    );
    assert_eq!(
        std::fs::read_to_string(&started.output).unwrap(),
        format!("out|err|hello|{}\nno-tty|", workdir.display())
    );
    assert!(!group_exists(started.submitted.spawned.pid));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_task_runs_where_its_descriptor_points_even_after_the_path_was_replaced() {
    let fixture = Fixture::start().await;
    let approved = fixture.workdir("approved");
    let moved = fixture.directory.path().join("moved");
    let binding = fixture.binding("pwd -P; ls", &approved);
    let mut controller = fixture.controller().await;
    let grant = allow(&mut controller, &binding).await;
    let directory = std::fs::File::open(&approved).unwrap();
    let (output, output_path) = output_file(&binding);
    // After the Worker's last check the approved directory is renamed and another directory takes
    // its path; the task still runs in the directory the descriptor names.
    std::fs::rename(&approved, &moved).unwrap();
    std::fs::create_dir(&approved).unwrap();
    std::fs::write(approved.join("planted"), "").unwrap();
    let mut started = Started {
        submitted: shell::submit(
            &fixture.paths.socket(),
            &grant,
            &request(&binding, &[]),
            &directory,
            &output,
        )
        .await
        .unwrap(),
        output: moved.join(output_path.file_name().unwrap()),
    };
    assert_eq!(exited(&mut started).await.code, Some(0));
    let shown = std::fs::read_to_string(&started.output).unwrap();
    assert!(
        shown.starts_with(&format!("{}\n", moved.canonicalize().unwrap().display())),
        "{shown}"
    );
    assert!(!shown.contains("planted"), "{shown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_terminates_the_group_then_kills_what_ignores_it() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("stop");
    let binding = fixture.binding(
        "trap '' TERM; printf started; while :; do sleep 0.05; done",
        &workdir,
    );
    let mut started = start(&fixture, &binding, &[]).await;
    let pid = started.submitted.spawned.pid;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while std::fs::read_to_string(&started.output).unwrap() != "started" {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stopping = tokio::time::Instant::now();
    started.submitted.stops.stop("timed_out").await.unwrap();
    let exit = exited(&mut started).await;
    let took = stopping.elapsed();
    assert_eq!(exit.reason.as_deref(), Some("timed_out"));
    assert_eq!(exit.signal, Some(libc::SIGKILL));
    assert_eq!(exit.outcome, ShellOutcome::Failed);
    assert!(
        took >= shell::SHELL_TERMINATE_GRACE && took < Duration::from_secs(2),
        "{took:?}"
    );
    assert!(!group_exists(pid));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_ends_with_the_spawner_connection_that_submitted_it() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("leave");
    let binding = fixture.binding("sleep 30", &workdir);
    let started = start(&fixture, &binding, &[]).await;
    let pid = started.submitted.spawned.pid;
    assert!(group_exists(pid));
    drop(started);
    group_ends(pid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_daemon_or_the_last_controller_leaving_stops_every_task() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("fence");
    let mut first = start(&fixture, &fixture.binding("sleep 30", &workdir), &[]).await;
    let mut second = start(&fixture, &fixture.binding("sleep 30", &workdir), &[]).await;
    let _replacement = fixture
        .connect_controller(uuid::Uuid::new_v4())
        .await
        .unwrap();
    for started in [&mut first, &mut second] {
        let exit = exited(started).await;
        assert_eq!(exit.reason.as_deref(), Some(CANCELLED_REASON));
        group_ends(started.submitted.spawned.pid).await;
    }

    // Under its own daemon, a task outlives one controller connection while another remains,
    // and stops once the last one closed.
    let epoch = uuid::Uuid::new_v4();
    let held = fixture.connect_controller(epoch).await.unwrap();
    let mut controller = fixture.connect_controller(epoch).await.unwrap();
    let mut binding = fixture.binding("sleep 30", &workdir);
    binding.identity.daemon_epoch = epoch;
    let grant = allow(&mut controller, &binding).await;
    let directory = std::fs::File::open(&workdir).unwrap();
    let (output, _) = output_file(&binding);
    let mut submitted = shell::submit(
        &fixture.paths.socket(),
        &grant,
        &request(&binding, &[]),
        &directory,
        &output,
    )
    .await
    .unwrap();
    drop(controller);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(group_exists(submitted.spawned.pid));
    drop(held);
    let exit = tokio::time::timeout(Duration::from_secs(5), submitted.exits.exited())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exit.reason.as_deref(), Some(CANCELLED_REASON));
    group_ends(submitted.spawned.pid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_grant_starts_a_task_once_the_daemons_last_connection_closed() {
    let mut fixture = Fixture::start().await;
    let workdir = fixture.workdir("withdrawn");
    let mut running = start(&fixture, &fixture.binding("sleep 30", &workdir), &[]).await;
    let mut controller = fixture.controller().await;
    let touch = |name: &str| format!("touch {}", workdir.join(name).display());
    let unclaimed_binding = fixture.binding(&touch("unclaimed"), &workdir);
    let unclaimed = allow(&mut controller, &unclaimed_binding).await;
    let claimed_binding = fixture.binding(&touch("claimed"), &workdir);
    let claimed_grant = allow(&mut controller, &claimed_binding).await;
    let mut claimed = HostConnection::connect(
        &fixture.paths.socket(),
        Role::Spawner {
            grant: claimed_grant.grant,
            token: claimed_grant.token,
        },
        &[CAPABILITY_SHELL],
    )
    .await
    .unwrap();
    drop(controller);
    drop(fixture.daemon.take());
    // The running task stopping shows the host saw the daemon's last connection close.
    assert_eq!(
        exited(&mut running).await.reason.as_deref(),
        Some(CANCELLED_REASON)
    );

    let directory = std::fs::File::open(&workdir).unwrap();
    let (output, _) = output_file(&unclaimed_binding);
    let refused = shell::submit(
        &fixture.paths.socket(),
        &unclaimed,
        &request(&unclaimed_binding, &[]),
        &directory,
        &output,
    )
    .await
    .err()
    .expect("an unclaimed grant was withdrawn");
    assert_eq!(refused.code, "Forbidden");

    let (output, _) = output_file(&claimed_binding);
    claimed
        .send_descriptors(&[directory.as_raw_fd(), output.as_raw_fd()])
        .await
        .unwrap();
    let refused = claimed
        .request(
            "spawn",
            json!(request(&claimed_binding, &[])),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, "Fenced");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!workdir.join("unclaimed").exists());
    assert!(!workdir.join("claimed").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopping_host_stops_each_task_and_reports_its_exit_before_it_leaves() {
    let mut fixture = Fixture::start().await;
    let workdir = fixture.workdir("shutdown");
    let mut started = start(&fixture, &fixture.binding("sleep 30", &workdir), &[]).await;
    let pid = started.submitted.spawned.pid;
    fixture.stop.take().unwrap().send(()).unwrap();
    let exit = exited(&mut started).await;
    assert_eq!(exit.reason.as_deref(), Some(CANCELLED_REASON));
    assert!(!group_exists(pid));
    tokio::time::timeout(Duration::from_secs(5), fixture.server)
        .await
        .expect("the stopped host exits")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_live_unused_grant_and_the_request_it_binds_start_a_task() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("grants");
    let mut controller = fixture.controller().await;
    let directory = std::fs::File::open(&workdir).unwrap();
    let marker = workdir.join("ran");
    let command = format!("touch {}", marker.display());

    let unknown = SpawnGrant {
        grant: uuid::Uuid::new_v4(),
        token: host::random_token(),
        epoch: uuid::Uuid::new_v4(),
    };
    let binding = fixture.binding(&command, &workdir);
    let (output, _) = output_file(&binding);
    let refused = |result: Result<SubmittedShell, tickhost_connect::host_error::HostError>| {
        result.err().expect("the spawn was refused").code
    };
    let attempt = |grant: SpawnGrant, request: SpawnRequest| {
        let socket = fixture.paths.socket();
        let directory = directory.try_clone().unwrap();
        let output = output.try_clone().unwrap();
        async move { shell::submit(&socket, &grant, &request, &directory, &output).await }
    };
    assert_eq!(
        refused(attempt(unknown, request(&binding, &[])).await),
        "Forbidden"
    );

    let mut forged = allow(&mut controller, &binding).await;
    forged.token = host::random_token();
    assert_eq!(
        refused(attempt(forged.clone(), request(&binding, &[])).await),
        "Forbidden"
    );

    for differing in [
        SpawnBinding {
            approval: ShellApproval {
                command: format!("{command}; rm -rf {}", workdir.display()),
                ..binding.approval.clone()
            },
            ..binding.clone()
        },
        SpawnBinding {
            identity: ShellIdentity {
                chat: uuid::Uuid::new_v4(),
                ..binding.identity.clone()
            },
            ..binding.clone()
        },
        SpawnBinding {
            operation: uuid::Uuid::new_v4(),
            ..binding.clone()
        },
    ] {
        let grant = allow(&mut controller, &binding).await;
        assert_eq!(
            refused(attempt(grant.clone(), request(&differing, &[])).await),
            "Forbidden"
        );
        // A refused request used its grant up.
        assert_eq!(
            refused(attempt(grant, request(&binding, &[])).await),
            "Forbidden"
        );
    }

    // The environment is bound too: a request may not start with variables the grant never saw.
    let grant = allow(&mut controller, &binding).await;
    assert_eq!(
        refused(attempt(grant, request(&binding, &[("PATH", "/tmp/planted")])).await),
        "Forbidden"
    );

    let grant = allow(&mut controller, &binding).await;
    let mut submitted = attempt(grant.clone(), request(&binding, &[]))
        .await
        .unwrap();
    assert_eq!(submitted.exits.exited().await.unwrap().code, Some(0));
    assert_eq!(
        refused(attempt(grant, request(&binding, &[])).await),
        "Forbidden"
    );

    // A grant names its own daemon: another daemon's epoch is never granted, and a grant whose
    // daemon was replaced no longer admits its spawner.
    let mut stranger = binding.clone();
    stranger.identity.daemon_epoch = uuid::Uuid::new_v4();
    let error = controller
        .request("allow_spawn", json!(stranger), CONTROL_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(error.code, "Fenced");
    let stale = allow(&mut controller, &fixture.binding(&command, &workdir)).await;
    let _replacement = fixture
        .connect_controller(uuid::Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        refused(attempt(stale, request(&binding, &[])).await),
        "Forbidden"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn output_beyond_sixty_four_mebibytes_stops_the_task_and_is_cut_at_the_limit() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("flood");
    let binding = fixture.binding("dd if=/dev/zero bs=1048576 count=65 2>/dev/null", &workdir);
    let mut started = start(&fixture, &binding, &[]).await;
    let exit = exited(&mut started).await;
    assert_eq!(exit.reason.as_deref(), Some(OUTPUT_LIMIT_REASON));
    assert_eq!(exit.outcome, ShellOutcome::Failed);
    assert_eq!(
        std::fs::metadata(&started.output).unwrap().len(),
        MAX_SHELL_OUTPUT_BYTES
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn descendants_left_running_are_ended_before_the_exit_is_reported_unknown() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("descendants");
    let binding = fixture.binding(
        "/bin/sh -c 'trap \"\" TERM; sleep 0.8; echo late > late-write' &",
        &workdir,
    );
    let mut started = start(&fixture, &binding, &[]).await;
    let exit = exited(&mut started).await;
    assert_eq!(exit.outcome, ShellOutcome::Unknown);
    assert_eq!(exit.reason.as_deref(), Some(UNRESOLVED_DESCENDANTS_REASON));
    assert!(!group_exists(started.submitted.spawned.pid));
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(!workdir.join("late-write").exists());
}

async fn reader(fixture: &Fixture, started: &Started) -> HostConnection {
    HostConnection::connect(
        &fixture.paths.socket(),
        Role::Instance {
            instance: started.submitted.spawned.instance.clone(),
            data_token: started.submitted.spawned.data_token.clone(),
        },
        &[CAPABILITY_DATA],
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shell_instance_describes_reads_and_updates_only_when_its_task_exits() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("adapter");
    let release = workdir.join("release");
    let binding = fixture.binding(
        &format!(
            "printf running; while [ ! -e {} ]; do sleep 0.02; done; printf ' finished'",
            release.display()
        ),
        &workdir,
    );
    let mut started = start(&fixture, &binding, &[]).await;
    let mut reader = reader(&fixture, &started).await;
    assert_eq!(
        reader
            .request("describe", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap(),
        json!({"description":"A task","command":binding.approval.command,"cwd":workdir})
    );
    let read = reader
        .request("read", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(
        (read["seq"].clone(), read["exit"].clone()),
        (json!(0), Value::Null)
    );
    assert_eq!(
        reader
            .request("call", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap_err()
            .code,
        "UnsupportedCapability"
    );
    let mut watcher = self::reader(&fixture, &started).await;
    let baseline = watcher
        .request("watch", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(baseline["read"]["seq"], 0);
    // Output while it runs is no update.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), watcher.next_event())
            .await
            .is_err()
    );
    std::fs::write(&release, "").unwrap();
    let update = tokio::time::timeout(Duration::from_secs(5), watcher.next_event())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(update["event"], "observation");
    assert_eq!(update["read"]["seq"], 1);
    assert_eq!(update["read"]["output"], "running finished");
    assert_eq!(update["read"]["exit"]["code"], 0);
    assert_eq!(exited(&mut started).await.outcome, ShellOutcome::Completed);
    // The host forgets the task once its spawner leaves.
    drop(started);
    let gone = tokio::time::timeout(Duration::from_secs(5), watcher.next_event())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(gone["event"], "gone");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_host_stays_while_a_grant_or_task_waits_and_exits_once_none_is_left() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("idle");
    let binding = fixture.binding("sleep 0.6", &workdir);
    let mut controller = fixture.controller().await;
    let grant = allow(&mut controller, &binding).await;
    // A sweep finds only the pending grant, and keeps the host.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!fixture.server.is_finished());
    let directory = std::fs::File::open(&workdir).unwrap();
    let (output, _) = output_file(&binding);
    let mut submitted = shell::submit(
        &fixture.paths.socket(),
        &grant,
        &request(&binding, &[]),
        &directory,
        &output,
    )
    .await
    .unwrap();
    assert_eq!(submitted.exits.exited().await.unwrap().code, Some(0));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        !fixture.server.is_finished(),
        "the spawner still holds its connection"
    );
    drop(submitted);
    tokio::time::timeout(Duration::from_secs(5), fixture.server)
        .await
        .expect("the empty host exits")
        .unwrap()
        .unwrap();
}
