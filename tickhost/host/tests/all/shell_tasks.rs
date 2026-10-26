//! Shell tasks under the Tickhost library: one-time spawn grants the controller asks for, pipe
//! mode in the passed directory and output file, stopping, the lifetimes that end a task with its
//! spawner, its daemon or the host, and the read-only views of a task.

use std::{
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};
use tick_protocol::InstanceRef;
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
    self, CAPABILITY_DATA, CAPABILITY_LIFECYCLE, CAPABILITY_SHELL, CONTROL_TIMEOUT, HostConnection,
    ManagementConnection, Role,
};

struct Fixture {
    server: tickhost::in_process::InProcessHost,
    directory: tempfile::TempDir,
    paths: HostPaths,
    key: String,
    daemon_epoch: uuid::Uuid,
    /// The connection the daemon holds for as long as it runs; Shell tasks stop without one.
    daemon: Option<HostConnection>,
    /// Stops the host as `tickhost stop` does, once sent or dropped.
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Fixture {
    async fn start() -> Self {
        Self::start_with_idle_timeout(host::DEFAULT_IDLE_TIMEOUT).await
    }

    async fn start_with_idle_timeout(idle_timeout: Duration) -> Self {
        let directory = crate::runtime_directory("ths");
        let paths = HostPaths {
            runtime: directory.path().join("h"),
        };
        let key = host::random_token();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tickhost::in_process::InProcessHost::start_until(
            paths.clone(),
            host::HostConfig {
                controller_key: key.clone(),
                idle_timeout,
            },
            async move {
                let _ = stopped.await;
            },
        )
        .unwrap();
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

/// A spawn grant no spawner claimed yet is a Shell about to run: what stopping the host ends
/// counts it until a spawner claims it, so a daemon of another build never takes the host for
/// empty in between.
#[tokio::test(flavor = "multi_thread")]
async fn a_spawn_grant_not_yet_claimed_counts_as_starting_until_its_spawner_connects() {
    let fixture = Fixture::start().await;
    let pending = || async {
        ManagementConnection::connect(&fixture.paths.socket(), &fixture.key)
            .await
            .unwrap()
            .hello()
            .host
            .clone()
            .unwrap()
            .occupancy
            .pending
    };
    assert_eq!(pending().await, Some(0));
    let mut controller = fixture.controller().await;
    let workdir = fixture.workdir("work");
    let grant = allow(&mut controller, &fixture.binding("sleep 30", &workdir)).await;
    assert_eq!(pending().await, Some(1));

    let _spawner = HostConnection::connect(
        &fixture.paths.socket(),
        Role::Spawner {
            grant: grant.grant,
            token: grant.token,
        },
        &[CAPABILITY_SHELL],
    )
    .await
    .unwrap();
    let occupancy = ManagementConnection::connect(&fixture.paths.socket(), &fixture.key)
        .await
        .unwrap()
        .hello()
        .host
        .clone()
        .unwrap()
        .occupancy;
    assert_eq!(occupancy.pending, Some(0));
    assert!(
        occupancy.connections >= 2,
        "the spawner's connection counts instead: {occupancy:?}"
    );
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
async fn planned_shell_handoff_preserves_process_and_fences_previous_owner() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("handoff");
    let binding = fixture.binding("while :; do sleep 1; done", &workdir);
    let mut original = start(&fixture, &binding, &[]).await;
    let pid = original.submitted.spawned.pid;
    let handoff = shell::ShellHandoff {
        endpoint: fixture.paths.socket(),
        instance: original.submitted.spawned.instance.clone(),
        owner_token: original.submitted.spawned.owner_token.clone(),
        handoff: uuid::Uuid::new_v4(),
    };
    let mut successor = shell::adopt(&handoff).await.unwrap();
    successor.stops.commit_handoff().await.unwrap();
    assert_eq!(successor.spawned.pid, pid);
    assert_eq!(successor.spawned.instance, handoff.instance);
    assert_ne!(successor.spawned.owner_token, handoff.owner_token);
    original.submitted.stops.stop("stale owner").await.unwrap();
    drop(original);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(group_exists(pid));
    let mut stolen = handoff.clone();
    stolen.handoff = uuid::Uuid::new_v4();
    assert!(shell::adopt(&stolen).await.is_err());
    drop(successor);
    group_ends(pid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_successor_hello_without_adoption_leaves_original_owner_in_control() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("prepare");
    let binding = fixture.binding("while :; do sleep 1; done", &workdir);
    let original = start(&fixture, &binding, &[]).await;
    let pid = original.submitted.spawned.pid;
    let prepared = HostConnection::connect(
        &fixture.paths.socket(),
        Role::ShellSuccessor {
            instance: original.submitted.spawned.instance.clone(),
            owner_token: original.submitted.spawned.owner_token.clone(),
            handoff: uuid::Uuid::new_v4(),
        },
        &[hosted::CAPABILITY_SHELL_HANDOFF],
    )
    .await
    .unwrap();
    drop(prepared);
    assert!(group_exists(pid));
    drop(original);
    group_ends(pid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_unconfirmed_handoff_can_recover_with_its_original_operation() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("retry-handoff");
    let binding = fixture.binding("while :; do sleep 1; done", &workdir);
    let original = start(&fixture, &binding, &[]).await;
    let pid = original.submitted.spawned.pid;
    let handoff = shell::ShellHandoff {
        endpoint: fixture.paths.socket(),
        instance: original.submitted.spawned.instance.clone(),
        owner_token: original.submitted.spawned.owner_token.clone(),
        handoff: uuid::Uuid::new_v4(),
    };
    let interrupted = shell::adopt(&handoff).await.unwrap();
    drop(interrupted);
    drop(original);
    let mut recovered = shell::adopt(&handoff).await.unwrap();
    assert_eq!(recovered.spawned.pid, pid);
    recovered.stops.commit_handoff().await.unwrap();
    recovered.stops.stop("test complete").await.unwrap();
    assert_eq!(
        recovered.exits.exited().await.unwrap().reason.as_deref(),
        Some("test complete")
    );
    drop(recovered);
    group_ends(pid).await;
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
    const IDLE_TIMEOUT: Duration = Duration::from_secs(1);
    let fixture = Fixture::start_with_idle_timeout(IDLE_TIMEOUT).await;
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
    let left = tokio::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(5), fixture.server)
        .await
        .expect("the empty host exits")
        .unwrap()
        .unwrap();
    assert!(
        left.elapsed() >= IDLE_TIMEOUT - Duration::from_millis(100),
        "the ended task stayed viewable for the idle timeout"
    );
}

const VIEW_ROWS: u16 = 16;
const VIEW_COLUMNS: u16 = 60;

/// A view of a Shell task, attached through a grant its daemon asked for, drawing its frames.
struct ShellView {
    events: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    requests: tokio::net::unix::OwnedWriteHalf,
    screen: vt100::Parser,
}

impl ShellView {
    async fn grant(fixture: &Fixture, binding: &SpawnBinding, instance: &InstanceRef) -> Value {
        fixture
            .controller()
            .await
            .request(
                "allow_shell_view",
                json!({"identity":binding.identity,"instance":instance}),
                CONTROL_TIMEOUT,
            )
            .await
            .unwrap()
    }

    async fn open(fixture: &Fixture, binding: &SpawnBinding, instance: &InstanceRef) -> Self {
        let grant = Self::grant(fixture, binding, instance).await;
        let mut connection = HostConnection::connect(
            &fixture.paths.socket(),
            Role::Viewer {
                instance: instance.clone(),
                view: serde_json::from_value(grant["view"].clone()).unwrap(),
            },
            &[CAPABILITY_DATA],
        )
        .await
        .unwrap();
        assert_eq!(grant["epoch"], json!(connection.epoch()));
        connection
            .request(
                "attach",
                json!({"size":{"rows":VIEW_ROWS,"columns":VIEW_COLUMNS}}),
                CONTROL_TIMEOUT,
            )
            .await
            .unwrap();
        let (events, requests) = connection.into_parts();
        Self {
            events,
            requests,
            screen: vt100::Parser::new(VIEW_ROWS, VIEW_COLUMNS, 0),
        }
    }

    async fn next_event(&mut self) -> Option<Value> {
        hosted::read_frame(&mut self.events, hosted::BUSINESS_FRAME_BYTES)
            .await
            .unwrap()
    }

    async fn type_text(&mut self, text: &str) {
        hosted::write_frame(
            &mut self.requests,
            &json!({"method":"input","params":{"data":text.as_bytes()}}),
            hosted::ATTACH_REQUEST_FRAME_BYTES,
        )
        .await
        .unwrap();
    }

    /// Draws frames until the screen shows every one of `texts`, and returns it.
    async fn shows(&mut self, texts: &[&str]) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let contents = self.screen.screen().contents();
            if texts.iter().all(|text| contents.contains(text)) {
                return contents;
            }
            let event = tokio::time::timeout_at(deadline, self.next_event())
                .await
                .unwrap_or_else(|_| panic!("{texts:?} never showed:\n{contents}"))
                .expect("the view stays attached");
            assert_eq!(event["event"], "frame", "{event}");
            self.screen
                .process(event["data"].as_str().unwrap().as_bytes());
        }
    }
}

/// A Shell's view shows its description, full command, directory and status over its live
/// output; it ignores what is typed into it, shows how the task ended, and stays after the
/// task's spawner left, when a new view can still be granted while readers are refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_shell_view_shows_the_task_over_its_live_output_and_then_how_it_ended() {
    let fixture = Fixture::start().await;
    let workdir = fixture.workdir("view");
    let release = workdir.join("release");
    let binding = fixture.binding(
        &format!(
            "printf 'out%s\\n' one; while [ ! -e {} ]; do sleep 0.02; done; printf 'out%s\\n' two; exit $((2 + 2))",
            release.display()
        ),
        &workdir,
    );
    let mut started = start(&fixture, &binding, &[]).await;
    let instance = started.submitted.spawned.instance.clone();
    let mut view = ShellView::open(&fixture, &binding, &instance).await;
    let shown = view
        .shows(&["A task", "$ printf", "running", "outone"])
        .await;
    assert!(
        shown.contains(&format!("in {}", workdir.display())),
        "{shown}"
    );
    view.type_text("typed\r").await;
    std::fs::write(&release, "").unwrap();
    let shown = view.shows(&["outtwo", "exit 4"]).await;
    assert!(!shown.contains("running"), "{shown}");
    assert!(!shown.contains("typed"), "{shown}");
    assert_eq!(exited(&mut started).await.code, Some(4));
    let mut reader = reader(&fixture, &started).await;
    drop(started);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), reader.next_event())
            .await
            .unwrap()
            .unwrap(),
        None,
        "readers end once the spawner left"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(500), view.next_event())
            .await
            .is_err(),
        "the view stays on the ended task"
    );
    let mut later = ShellView::open(&fixture, &binding, &instance).await;
    later.shows(&["outone", "outtwo", "exit 4"]).await;
}

/// Only the Worker and Chat that submitted a task are granted views of it, and a released task
/// nobody views is forgotten after the idle timeout.
#[tokio::test(flavor = "multi_thread")]
async fn a_shell_view_is_granted_only_to_its_own_chat_until_the_task_is_forgotten() {
    const IDLE_TIMEOUT: Duration = Duration::from_millis(800);
    let fixture = Fixture::start_with_idle_timeout(IDLE_TIMEOUT).await;
    let workdir = fixture.workdir("grants");
    let binding = fixture.binding("printf done", &workdir);
    let mut started = start(&fixture, &binding, &[]).await;
    let instance = started.submitted.spawned.instance.clone();
    let mut controller = fixture.controller().await;
    for identity in [
        ShellIdentity {
            chat: uuid::Uuid::new_v4(),
            ..binding.identity.clone()
        },
        ShellIdentity {
            worker: uuid::Uuid::new_v4(),
            ..binding.identity.clone()
        },
    ] {
        assert_eq!(
            controller
                .request(
                    "allow_shell_view",
                    json!({"identity":identity,"instance":instance}),
                    CONTROL_TIMEOUT,
                )
                .await
                .unwrap_err()
                .code,
            "TargetGone"
        );
    }
    exited(&mut started).await;
    drop(started);
    let view = ShellView::open(&fixture, &binding, &instance).await;
    tokio::time::sleep(IDLE_TIMEOUT * 2).await;
    drop(view);
    // Viewed until now, so its idle time starts over.
    ShellView::grant(&fixture, &binding, &instance).await;
    // A pending spawn grant keeps the host while the task is forgotten.
    allow(&mut controller, &fixture.binding("true", &workdir)).await;
    tokio::time::sleep(IDLE_TIMEOUT * 2).await;
    assert_eq!(
        controller
            .request(
                "allow_shell_view",
                json!({"identity":binding.identity,"instance":instance}),
                CONTROL_TIMEOUT,
            )
            .await
            .unwrap_err()
            .code,
        "TargetGone"
    );
}

#[test]
fn fixture_drop_stops_shells_even_after_panic_and_runtime_shutdown() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut pid = 0;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            let fixture = Fixture::start().await;
            let work = fixture.workdir("work");
            let started = start(
                &fixture,
                &fixture.binding("echo $$; exec sleep 30", &work),
                &[],
            )
            .await;
            for _ in 0..100 {
                if let Ok(contents) = std::fs::read_to_string(&started.output)
                    && let Ok(process) = contents.trim().parse::<i32>()
                {
                    pid = process;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(pid > 0, "shell reported its PID");
            panic!("fixture unwind probe");
        });
    }));
    drop(runtime);
    assert!(pid > 0, "shell did not start");
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    if alive {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    assert!(result.is_err());
    assert!(!alive, "fixture left Shell PID {pid} alive");
}
