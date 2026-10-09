#![cfg(unix)]

mod model_http_support;
mod support;

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{OpenOptionsExt, PermissionsExt, symlink},
            net::UnixStream as BlockingUnixStream,
        },
    },
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};

use serde_json::{Value, json};
use tickloop_agent::{
    agent_access::*,
    model::{Content, Protocol},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::Command,
};
use uuid::Uuid;

const CLI_TIMEOUT: Duration = Duration::from_secs(25);
const FIXTURE_TEXT: &str = "Keep this saved daemon-stop fixture history.";
/// The frozen management major on daemon.sock; it has no aliases.
const MANAGEMENT_MAJOR: u16 = 1;

struct DaemonFixture {
    directory: tempfile::TempDir,
    runtime: PathBuf,
    config: PathBuf,
    daemon_epoch: Option<Uuid>,
    launch_in_progress: bool,
}

impl DaemonFixture {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("tl-stop-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = root.join("runtime");
        let config = root.join("config");
        for path in [&runtime, &config] {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            directory,
            runtime,
            config,
            daemon_epoch: None,
            launch_in_progress: false,
        }
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = support::fixture_cli::tokio_command();
        command
            .args(arguments)
            .current_dir(self.directory.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.directory.path())
            .env("NO_COLOR", "1")
            .env("TICKLOOP_CONFIG_DIR", &self.config)
            .env("TICKLOOP_DAEMON_RUNTIME_DIR", &self.runtime)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    async fn run(&self, arguments: &[&str]) -> Output {
        tokio::time::timeout(CLI_TIMEOUT, self.command(arguments).output())
            .await
            .expect("daemon CLI exceeded its bounded shutdown window")
            .unwrap()
    }

    fn listener(&self) -> UnixListener {
        let socket = self.runtime.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener
    }

    fn lock(&self) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.runtime.join("daemon.lock"))
            .unwrap()
    }
}

impl Drop for DaemonFixture {
    fn drop(&mut self) {
        let Some(epoch) = self.daemon_epoch else {
            if self.launch_in_progress {
                self.directory.disable_cleanup(true);
                eprintln!(
                    "Retained owned daemon fixture after an unconfirmed launch: {}",
                    self.runtime.display()
                );
            }
            return;
        };
        let socket = self.runtime.join("daemon.sock");
        let Ok(mut stream) = BlockingUnixStream::connect(&socket) else {
            self.directory.disable_cleanup(true);
            eprintln!(
                "Retained unreachable owned daemon fixture: {}",
                self.runtime.display()
            );
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let cleanup = (|| -> std::io::Result<bool> {
            write_blocking(
                &mut stream,
                &json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0}}),
            )?;
            let hello = read_blocking(&mut stream)?;
            if hello["Hello"]["daemon_epoch"] != json!(epoch) {
                return Ok(false);
            }
            let request_id = Uuid::new_v4();
            write_blocking(
                &mut stream,
                &json!({"ShutdownDaemon":{"request_id":request_id,"daemon_epoch":epoch}}),
            )?;
            let response = read_blocking(&mut stream)?;
            Ok(response["DaemonStopped"]["request_id"] == json!(request_id))
        })();
        drop(stream);
        let confirmed = matches!(cleanup, Ok(true));
        if confirmed {
            let deadline = std::time::Instant::now() + Duration::from_secs(7);
            while socket.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        if !confirmed || socket.exists() {
            self.directory.disable_cleanup(true);
            eprintln!(
                "Retained owned daemon fixture after incomplete cleanup: {}",
                self.runtime.display()
            );
        }
    }
}

fn write_blocking(stream: &mut BlockingUnixStream, value: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)
}

fn read_blocking(stream: &mut BlockingUnixStream) -> std::io::Result<Value> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn write_frame(stream: &mut UnixStream, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    stream.write_u32(bytes.len() as u32).await.unwrap();
    stream.write_all(&bytes).await.unwrap();
    stream.flush().await.unwrap();
}

async fn read_frame(stream: &mut UnixStream) -> Option<Value> {
    let length = match stream.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return None,
        Err(error) => panic!("{error}"),
    };
    assert!(length <= tickloop_agent::protocol::AccessLimits::DEFAULT.frame_bytes());
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await.unwrap();
    Some(serde_json::from_slice(&bytes).unwrap())
}

async fn management_connect(socket: &Path) -> UnixStream {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    write_frame(
        &mut stream,
        &json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0}}),
    )
    .await;
    let response = read_frame(&mut stream).await.unwrap();
    assert_eq!(response["Hello"]["major"], MANAGEMENT_MAJOR);
    stream
}

fn assert_failed(output: &Output) -> String {
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Agent daemon stopped."));
    message
}

async fn assert_disconnected(stream: &mut UnixStream, listener: &UnixListener) {
    assert!(
        tokio::time::timeout(Duration::from_secs(2), read_frame(stream))
            .await
            .unwrap()
            .is_none(),
        "no business request or repeated shutdown may follow"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "the stop operation must not reconnect or start another daemon"
    );
}

#[tokio::test]
async fn absent_stale_or_unready_daemon_is_never_started_or_removed() {
    let fixture = DaemonFixture::new();
    std::fs::remove_dir(&fixture.runtime).unwrap();
    for _ in 0..2 {
        let output = fixture.run(&["daemon", "stop"]).await;
        assert!(output.status.success());
        assert_eq!(output.stdout, b"Agent daemon is not running.\n");
        assert!(output.stderr.is_empty());
        assert!(!fixture.runtime.exists());
    }
    std::fs::create_dir(&fixture.runtime).unwrap();
    std::fs::set_permissions(&fixture.runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = fixture.listener();
    drop(listener);
    let socket = fixture.runtime.join("daemon.sock");
    let output = fixture.run(&["daemon", "stop"]).await;
    assert!(output.status.success());
    assert!(
        socket.exists(),
        "stale sockets belong to startup arbitration"
    );
    assert!(!fixture.runtime.join("daemon.lock").exists());
    std::fs::remove_file(&socket).unwrap();
    let lock = fixture.lock();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let output = fixture.run(&["daemon", "stop"]).await;
    assert!(assert_failed(&output).contains("starting or stopping"));
    assert!(!socket.exists());
    assert!(fixture.runtime.join("daemon.lock").exists());
}

#[tokio::test]
async fn daemon_help_and_invalid_arguments_do_not_connect_or_touch_runtime() {
    let fixture = DaemonFixture::new();
    let listener = fixture.listener();
    for arguments in [
        vec!["daemon", "--help"],
        vec!["daemon", "stop", "-h"],
        vec!["daemon", "stop", "--help"],
        vec!["daemon"],
        vec!["daemon", "start"],
        vec!["daemon", "stop", "--force"],
        vec!["daemon", "stop", "extra"],
        vec!["daemon", "stop", "--help", "--unknown"],
    ] {
        let output = fixture.run(&arguments).await;
        let help = arguments.contains(&"--help") || arguments.contains(&"-h");
        assert_eq!(
            output.status.success(),
            help && !arguments.contains(&"--unknown")
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(text.contains("Usage: tickloop daemon stop"));
        if help && output.status.success() {
            for promise in [
                "all of its workers, which ends every running Agent",
                "Saved history is kept.",
                "Ticks in the Tickhost keep running.",
                "The next tickloop launch starts this build.",
            ] {
                assert!(text.contains(promise), "{promise}: {text}");
            }
            assert!(!text.contains("daemon run"), "{text}");
        }
        assert!(!fixture.runtime.join("daemon.lock").exists());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    assert!(fixture.runtime.join("daemon.sock").exists());
}

#[tokio::test]
async fn unsafe_runtime_socket_and_lock_are_rejected_before_any_handshake() {
    let fixture = DaemonFixture::new();
    let listener = fixture.listener();
    for (path, unsafe_mode, safe_mode) in [
        (&fixture.runtime, 0o755, 0o700),
        (&fixture.runtime.join("daemon.sock"), 0o666, 0o600),
    ] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(unsafe_mode)).unwrap();
        let output = fixture.run(&["daemon", "stop"]).await;
        assert_failed(&output);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(safe_mode)).unwrap();
    }
    let lock = fixture.lock();
    std::fs::set_permissions(
        fixture.runtime.join("daemon.lock"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_failed(&fixture.run(&["daemon", "stop"]).await);
    drop(lock);
    std::fs::remove_file(fixture.runtime.join("daemon.lock")).unwrap();
    let target = fixture.directory.path().join("unrelated");
    std::fs::write(&target, b"unrelated private file").unwrap();
    symlink(&target, fixture.runtime.join("daemon.lock")).unwrap();
    assert_failed(&fixture.run(&["daemon", "stop"]).await);
    assert_eq!(std::fs::read(&target).unwrap(), b"unrelated private file");
    std::fs::remove_file(fixture.runtime.join("daemon.lock")).unwrap();
    std::fs::remove_file(fixture.runtime.join("daemon.sock")).unwrap();
    symlink(&target, fixture.runtime.join("daemon.sock")).unwrap();
    assert_failed(&fixture.run(&["daemon", "stop"]).await);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"unrelated private file");
}

#[tokio::test]
async fn malformed_and_unknown_hello_never_receive_shutdown() {
    for hello in [
        json!({"Hello":{"major":3,"minor":0,"daemon_epoch":Uuid::new_v4()}}),
        json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0}}),
        json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":1,"daemon_epoch":Uuid::new_v4()}}),
        json!({"DaemonStopped":{"request_id":Uuid::new_v4()}}),
    ] {
        let fixture = DaemonFixture::new();
        let listener = fixture.listener();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(
                read_frame(&mut stream).await.unwrap()["Hello"]["major"],
                MANAGEMENT_MAJOR
            );
            write_frame(&mut stream, &hello).await;
            assert_disconnected(&mut stream, &listener).await;
        });
        assert_failed(&fixture.run(&["daemon", "stop"]).await);
        server.await.unwrap();
        assert!(fixture.runtime.join("daemon.sock").exists());
        assert!(!fixture.runtime.join("daemon.lock").exists());
    }
}

#[tokio::test]
async fn shutdown_requires_correlated_ack_and_is_never_resent() {
    for response_kind in [
        "wrong-id",
        "wrong-epoch",
        "wrong-response",
        "disconnect",
        "timeout",
    ] {
        let fixture = DaemonFixture::new();
        let listener = fixture.listener();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(
                read_frame(&mut stream)
                    .await
                    .unwrap()
                    .get("Hello")
                    .is_some()
            );
            let epoch = Uuid::new_v4();
            write_frame(
                &mut stream,
                &json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0,"daemon_epoch":epoch}}),
            )
            .await;
            let shutdown = read_frame(&mut stream).await.unwrap();
            assert_eq!(shutdown["ShutdownDaemon"]["daemon_epoch"], json!(epoch));
            assert_eq!(shutdown.as_object().unwrap().len(), 1);
            let request_id = shutdown["ShutdownDaemon"]["request_id"].clone();
            match response_kind {
                "wrong-id" => write_frame(&mut stream, &json!({"DaemonStopped":{"request_id":Uuid::new_v4()}})).await,
                "wrong-epoch" => write_frame(&mut stream, &json!({"Error":{"request_id":request_id,"error":{"code":"EpochChanged","message":"fixture daemon epoch changed"}}})).await,
                "wrong-response" => write_frame(&mut stream, &json!({"Receipt":{"request_id":request_id}})).await,
                "disconnect" => { drop(stream); assert!(tokio::time::timeout(Duration::from_millis(150), listener.accept()).await.is_err()); return; }
                "timeout" => tokio::time::sleep(Duration::from_millis(5250)).await,
                _ => unreachable!(),
            }
            assert_disconnected(&mut stream, &listener).await;
        });
        let message = assert_failed(&fixture.run(&["daemon", "stop"]).await);
        if response_kind != "wrong-epoch" {
            assert!(
                message.contains("not confirmed"),
                "{response_kind}: {message}"
            );
            assert!(message.contains("no stop request was resent"));
        } else {
            assert!(message.contains("fixture daemon epoch changed"));
        }
        server.await.unwrap();
        assert!(fixture.runtime.join("daemon.sock").exists());
        assert!(!fixture.runtime.join("daemon.lock").exists());
    }
}

#[tokio::test]
async fn acknowledged_shutdown_preserves_a_replacement_daemon_socket() {
    let fixture = DaemonFixture::new();
    let listener = fixture.listener();
    let socket = fixture.runtime.join("daemon.sock");
    let old_socket = fixture.runtime.join("old.sock");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_frame(&mut stream).await.unwrap();
        let epoch = Uuid::new_v4();
        write_frame(
            &mut stream,
            &json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0,"daemon_epoch":epoch}}),
        )
        .await;
        let shutdown = read_frame(&mut stream).await.unwrap();
        assert_eq!(shutdown["ShutdownDaemon"]["daemon_epoch"], json!(epoch));
        std::fs::rename(&socket, old_socket).unwrap();
        let replacement = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_frame(
            &mut stream,
            &json!({"DaemonStopped":{"request_id":shutdown["ShutdownDaemon"]["request_id"]}}),
        )
        .await;
        assert_disconnected(&mut stream, &replacement).await;
        assert!(socket.exists());
    });
    let output = fixture.run(&["daemon", "stop"]).await;
    assert!(assert_failed(&output).contains("daemon changed while stopping"));
    server.await.unwrap();
    assert!(fixture.runtime.join("daemon.sock").exists());
}

#[tokio::test]
async fn shutdown_ack_is_not_success_until_the_original_daemon_lock_is_released() {
    let fixture = DaemonFixture::new();
    let listener = fixture.listener();
    let lock = fixture.lock();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let socket = fixture.runtime.join("daemon.sock");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_frame(&mut stream).await.unwrap();
        let epoch = Uuid::new_v4();
        write_frame(
            &mut stream,
            &json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0,"daemon_epoch":epoch}}),
        )
        .await;
        let shutdown = read_frame(&mut stream).await.unwrap();
        assert_eq!(shutdown["ShutdownDaemon"]["daemon_epoch"], json!(epoch));
        write_frame(
            &mut stream,
            &json!({"DaemonStopped":{"request_id":shutdown["ShutdownDaemon"]["request_id"]}}),
        )
        .await;
        std::fs::remove_file(socket).unwrap();
        assert!(read_frame(&mut stream).await.is_none());
        drop(listener);
        tokio::time::sleep(Duration::from_millis(10250)).await;
        drop(lock);
    });
    let message = assert_failed(&fixture.run(&["daemon", "stop"]).await);
    assert!(message.contains("still stopping"));
    assert!(fixture.runtime.join("daemon.lock").exists());
    assert!(!fixture.runtime.join("daemon.sock").exists());
    server.await.unwrap();
}

async fn assert_processes_exited(process_ids: &[u32]) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut live = false;
            for process_id in process_ids {
                let mut status = 0;
                let reaped = unsafe {
                    libc::waitpid(*process_id as libc::pid_t, &mut status, libc::WNOHANG)
                };
                if reaped == *process_id as libc::pid_t {
                    continue;
                }
                let exists = unsafe { libc::kill(*process_id as libc::pid_t, 0) } == 0;
                if !exists {
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ESRCH),
                        "could not establish liveness of an owned fixture PID"
                    );
                }
                live |= exists;
            }
            if !live {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("an owned daemon/worker PID is still running after daemon stop succeeded");
}

async fn launch_active_worker(
    fixture: &DaemonFixture,
) -> (
    LaunchSpec,
    tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (base_url, requests, release) =
        model_http_support::serve_responses_with_gated_response(vec![(200, String::new())], 0)
            .await;
    let mut model = support::config();
    model.protocol = Protocol::ChatCompletions;
    model.base_url = base_url.trim_end_matches("/anthropic").into();
    model.api_key_env = "TICKLOOP_STOP_FIXTURE_KEY".into();
    let mut launch = LaunchSpec::embedded(fixture.directory.path().canonicalize().unwrap());
    launch.config_root = Some(fixture.config.clone());
    let transport = fixture.directory.path().join("transport.json");
    std::fs::write(&transport, serde_json::to_vec(&model).unwrap()).unwrap();
    launch.model = Some(model);
    launch.environment = [
        ("HOME", fixture.directory.path().to_str().unwrap()),
        ("TICKLOOP_STOP_FIXTURE_KEY", "fixture-only"),
        ("TICKLOOP_TRANSPORT_FIXTURE", transport.to_str().unwrap()),
        ("DEEPSEEK_API_KEY", "fixture-compiled-selection-key"),
    ]
    .into_iter()
    .map(|(name, value)| EnvironmentValue {
        name: name.as_bytes().to_vec(),
        value: value.as_bytes().to_vec(),
    })
    .collect();
    (launch, requests, release)
}

fn create_command(epoch: Uuid, launch: LaunchSpec) -> CommandEnvelope {
    CommandEnvelope {
        daemon_epoch: epoch,
        command_id: Uuid::new_v4(),
        command: AgentCommand::Create {
            launch,
            initial: Some(UserInput::Text {
                text: FIXTURE_TEXT.into(),
                contexts: vec![],
                paths: vec![],
            }),
            origin: None,
        },
    }
}

async fn verify_stopped_and_restart(
    fixture: &mut DaemonFixture,
    old_epoch: Uuid,
    record: TranscriptRef,
    worker_socket: PathBuf,
    process_ids: &[u32],
) {
    let output = fixture.run(&["daemon", "stop"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Agent daemon stopped."));
    assert!(!fixture.runtime.join("daemon.sock").exists());
    assert!(!worker_socket.exists());
    assert_processes_exited(process_ids).await;
    let replacement = tickloop_agent::agent_daemon::connect_or_start_at(
        fixture.runtime.clone(),
        support::fixture_cli::executable(),
    )
    .await
    .unwrap();
    fixture.daemon_epoch = Some(replacement.daemon_epoch());
    assert_ne!(replacement.daemon_epoch(), old_epoch);
    let history = replacement
        .observe(ObservationRequest::Transcript {
            record,
            page: Page {
                offset: 0,
                limit: 256,
            },
        })
        .await
        .unwrap();
    let ObservationValue::Transcript(history) = history.initial else {
        panic!("saved transcript")
    };
    assert!(history.messages.iter().any(|entry| {
        entry
            .message
            .content
            .iter()
            .any(|block| matches!(block, Content::Text { text } if text == FIXTURE_TEXT))
    }));
    let overview = replacement
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page {
                offset: 0,
                limit: 256,
            },
            after: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(overview.initial, ObservationValue::Overview(rows) if rows.is_empty()),
        "reading history must not resume the stopped worker"
    );
    assert!(std::fs::read_dir(&fixture.runtime).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("w-")
    }));
    let output = fixture.run(&["daemon", "stop"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.daemon_epoch = None;
}

/// The command line `ps` shows for `process_id`.
fn process_arguments(process_id: u32) -> String {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &process_id.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_cli_stop_ends_current_daemon_and_active_worker_preserving_saved_history() {
    let mut fixture = DaemonFixture::new();
    fixture.launch_in_progress = true;
    let daemon = tickloop_agent::agent_daemon::connect_or_start_at(
        fixture.runtime.clone(),
        support::fixture_cli::executable(),
    )
    .await
    .unwrap();
    let epoch = daemon.daemon_epoch();
    fixture.daemon_epoch = Some(epoch);
    fixture.launch_in_progress = false;
    let (launch, mut requests, _release) = launch_active_worker(&fixture).await;
    let created = daemon
        .execute(create_command(epoch, launch))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created { snapshot, .. } = created else {
        panic!("active worker created")
    };
    tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    let worker_socket = std::fs::read_dir(&fixture.runtime)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("w-"))
        .unwrap()
        .path();
    let worker = LocalAgentClient::connect(&worker_socket).await.unwrap();
    assert_eq!(worker.daemon_epoch(), epoch);
    assert!(
        process_arguments(daemon.protocol().process_id).contains(" daemon run "),
        "ps names the daemon by its role"
    );
    assert!(
        process_arguments(worker.protocol().process_id).contains(" worker "),
        "ps names the worker by its role"
    );
    let record = TranscriptRef {
        store_root: fixture.config.clone(),
        chat_id: snapshot.conversation.id,
    };
    verify_stopped_and_restart(
        &mut fixture,
        epoch,
        record,
        worker_socket,
        &[daemon.protocol().process_id, worker.protocol().process_id],
    )
    .await;
    assert!(
        requests.try_recv().is_err(),
        "stopping and restarting must not resend model work"
    );
    eprintln!(
        "actual current daemon + active worker stopped; saved history readable; new daemon epoch and zero resumed workers"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frozen_management_wire_stops_a_daemon_of_any_business_major() {
    let mut fixture = DaemonFixture::new();
    fixture.launch_in_progress = true;
    let daemon = tickloop_agent::agent_daemon::connect_or_start_at(
        fixture.runtime.clone(),
        env!("CARGO_BIN_EXE_tickloop").into(),
    )
    .await
    .unwrap();
    fixture.daemon_epoch = Some(daemon.daemon_epoch());
    fixture.launch_in_progress = false;
    let mut stream = UnixStream::connect(fixture.runtime.join("daemon.sock"))
        .await
        .unwrap();
    write_frame(
        &mut stream,
        &json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0}}),
    )
    .await;
    let hello = read_frame(&mut stream).await.unwrap();
    assert_eq!(hello["Hello"]["major"], MANAGEMENT_MAJOR);
    assert_eq!(hello["Hello"]["daemon_epoch"], json!(daemon.daemon_epoch()));
    let request_id = Uuid::new_v4();
    write_frame(
        &mut stream,
        &json!({"ShutdownDaemon":{"request_id":request_id,"daemon_epoch":daemon.daemon_epoch()}}),
    )
    .await;
    assert_eq!(
        read_frame(&mut stream).await.unwrap()["DaemonStopped"]["request_id"],
        json!(request_id)
    );
    tokio::time::timeout(Duration::from_secs(7), async {
        while fixture.runtime.join("daemon.sock").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_processes_exited(&[daemon.protocol().process_id]).await;
    fixture.daemon_epoch = None;
}

struct UndispatchedAccess(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl AgentAccess for UndispatchedAccess {
    async fn observe(&self, _query: ObservationRequest) -> Result<Observation, AccessError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(AccessError::unavailable("unexpected dispatch"))
    }
    async fn execute(&self, _command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(AccessError::unavailable("unexpected dispatch"))
    }
    async fn show_view(&self, _update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(AccessError::unavailable("unexpected dispatch"))
    }
}

#[tokio::test]
async fn management_session_rejects_every_business_request_before_dispatch() {
    let fixture = DaemonFixture::new();
    let access = std::sync::Arc::new(UndispatchedAccess(std::sync::atomic::AtomicUsize::new(0)));
    let epoch = Uuid::new_v4();
    let server = tokio::spawn(serve_daemon_listener(
        fixture.listener(),
        access.clone(),
        epoch,
        support::TRANSPORT_LIMITS,
    ));
    let request_id = Uuid::new_v4();
    let requests = [
        json!({"Observe":{"request_id":request_id,"query":ObservationRequest::Overview {
            filter: AgentFilter::default(), page: Page { offset:0, limit:10 }, after:None,
        }}}),
        json!({"Execute":{"request_id":request_id,"command":create_command(epoch, LaunchSpec::embedded(fixture.directory.path().into()))}}),
        json!({"ShowView":{"request_id":request_id,"update":DisplayUpdate::SessionClosed {session_id:Uuid::new_v4()}}}),
    ];
    for request in &requests {
        let mut stream = UnixStream::connect(fixture.runtime.join("daemon.sock"))
            .await
            .unwrap();
        write_frame(
            &mut stream,
            &json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0}}),
        )
        .await;
        let hello = read_frame(&mut stream).await.unwrap();
        assert_eq!(hello["Hello"]["major"], MANAGEMENT_MAJOR);
        assert_eq!(hello["Hello"]["minor"], 0);
        write_frame(&mut stream, request).await;
        let response = read_frame(&mut stream).await.unwrap();
        assert_eq!(response["Error"]["error"]["code"], "VersionMismatch");
        assert_eq!(response["Error"]["request_id"], json!(request_id));
        assert!(read_frame(&mut stream).await.is_none());
    }
    assert_eq!(access.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let mut stream = management_connect(&fixture.runtime.join("daemon.sock")).await;
    write_frame(
        &mut stream,
        &json!({"ShutdownDaemon":{"request_id":request_id,"daemon_epoch":epoch}}),
    )
    .await;
    assert_eq!(
        read_frame(&mut stream).await.unwrap()["DaemonStopped"]["request_id"],
        json!(request_id)
    );
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn management_rejects_wrong_epoch_malformed_and_oversize_frames_without_dispatch() {
    let fixture = DaemonFixture::new();
    let access = std::sync::Arc::new(UndispatchedAccess(std::sync::atomic::AtomicUsize::new(0)));
    let epoch = Uuid::new_v4();
    let server = tokio::spawn(serve_daemon_listener(
        fixture.listener(),
        access.clone(),
        epoch,
        support::TRANSPORT_LIMITS,
    ));
    let request_id = Uuid::new_v4();
    for request in [
        json!({"ShutdownDaemon":{"request_id":request_id,"daemon_epoch":Uuid::new_v4()}}),
        json!({"ShutdownDaemon":{"request_id":"invalid-uuid","daemon_epoch":epoch}}),
        json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0}}),
    ] {
        let mut stream = management_connect(&fixture.runtime.join("daemon.sock")).await;
        write_frame(&mut stream, &request).await;
        assert!(
            read_frame(&mut stream)
                .await
                .unwrap()
                .get("Error")
                .is_some()
        );
        assert!(read_frame(&mut stream).await.is_none());
        assert!(!server.is_finished());
    }
    for after_hello in [false, true] {
        let mut stream = UnixStream::connect(fixture.runtime.join("daemon.sock"))
            .await
            .unwrap();
        if after_hello {
            write_frame(
                &mut stream,
                &json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0}}),
            )
            .await;
            read_frame(&mut stream).await.unwrap();
        }
        stream.write_u32(65537).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), read_frame(&mut stream))
                .await
                .unwrap()
                .is_none()
        );
        assert!(!server.is_finished());
    }
    let mut stream = UnixStream::connect(fixture.runtime.join("daemon.sock"))
        .await
        .unwrap();
    stream.write_u32(1).await.unwrap();
    stream.write_all(b"{").await.unwrap();
    assert!(read_frame(&mut stream).await.is_none());
    // The retired 2.0 alias and any other minor are ordinary incompatible peers now.
    for hello in [
        json!({"Hello":{"major":2,"minor":0}}),
        json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":1}}),
    ] {
        let mut ordinary = UnixStream::connect(fixture.runtime.join("daemon.sock"))
            .await
            .unwrap();
        write_frame(&mut ordinary, &hello).await;
        assert_eq!(
            read_frame(&mut ordinary).await.unwrap()["Error"]["error"]["code"],
            "VersionMismatch"
        );
        assert!(read_frame(&mut ordinary).await.is_none());
    }
    assert_eq!(access.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn worker_socket_has_no_cross_version_management_facade() {
    let fixture = DaemonFixture::new();
    let access = std::sync::Arc::new(UndispatchedAccess(std::sync::atomic::AtomicUsize::new(0)));
    let server = tokio::spawn(serve_worker_listener(
        fixture.listener(),
        access.clone(),
        Uuid::new_v4(),
        support::TRANSPORT_LIMITS,
    ));
    let mut stream = UnixStream::connect(fixture.runtime.join("daemon.sock"))
        .await
        .unwrap();
    write_frame(
        &mut stream,
        &json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0}}),
    )
    .await;
    assert_eq!(
        read_frame(&mut stream).await.unwrap()["Error"]["error"]["code"],
        "VersionMismatch"
    );
    assert!(read_frame(&mut stream).await.is_none());
    assert_eq!(access.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn a_version_mismatch_is_reported_without_another_attempt() {
    let fixture = DaemonFixture::new();
    let listener = fixture.listener();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(
            read_frame(&mut stream).await.unwrap(),
            json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0}})
        );
        write_frame(&mut stream, &json!({"Error":{"request_id":null,"error":{"code":"VersionMismatch","message":"another management major"}}})).await;
        assert_disconnected(&mut stream, &listener).await;
    });
    let message = assert_failed(&fixture.run(&["daemon", "stop"]).await);
    assert!(
        message.contains("cannot be stopped by this build"),
        "{message}"
    );
    server.await.unwrap();
    assert!(fixture.runtime.join("daemon.sock").exists());
}

#[tokio::test]
async fn the_service_command_no_longer_exists() {
    let fixture = DaemonFixture::new();
    let listener = fixture.listener();
    let message = assert_failed(&fixture.run(&["service", "stop"]).await);
    assert!(message.contains("unknown argument service"), "{message}");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    assert!(!fixture.runtime.join("daemon.lock").exists());
}

#[tokio::test]
async fn control_oversize_reply_is_rejected_without_shutdown() {
    let fixture = DaemonFixture::new();
    let listener = fixture.listener();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_frame(&mut stream).await.unwrap();
        stream.write_u32(65537).await.unwrap();
        assert_disconnected(&mut stream, &listener).await;
    });
    assert!(assert_failed(&fixture.run(&["daemon", "stop"]).await).contains("64 KiB"));
    server.await.unwrap();
}

#[tokio::test]
async fn replacement_lock_before_shutdown_or_after_ack_is_preserved() {
    for after_ack in [false, true] {
        let fixture = DaemonFixture::new();
        let listener = fixture.listener();
        let original = fixture.lock();
        assert_eq!(
            unsafe { libc::flock(original.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let runtime = fixture.runtime.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_frame(&mut stream).await.unwrap();
            let epoch = Uuid::new_v4();
            if after_ack {
                write_frame(
                    &mut stream,
                    &json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0,"daemon_epoch":epoch}}),
                )
                .await;
                let shutdown = read_frame(&mut stream).await.unwrap();
                assert_eq!(shutdown["ShutdownDaemon"]["daemon_epoch"], json!(epoch));
                std::fs::rename(runtime.join("daemon.lock"), runtime.join("old.lock")).unwrap();
                let replacement = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(runtime.join("daemon.lock"))
                    .unwrap();
                assert_eq!(
                    unsafe { libc::flock(replacement.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
                std::fs::remove_file(runtime.join("daemon.sock")).unwrap();
                write_frame(
                    &mut stream,
                    &json!({"DaemonStopped":{"request_id":shutdown["ShutdownDaemon"]["request_id"]}}),
                )
                .await;
                assert_disconnected(&mut stream, &listener).await;
                drop(replacement);
            } else {
                std::fs::rename(runtime.join("daemon.lock"), runtime.join("old.lock")).unwrap();
                let replacement = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(runtime.join("daemon.lock"))
                    .unwrap();
                write_frame(
                    &mut stream,
                    &json!({"Hello":{"major":MANAGEMENT_MAJOR,"minor":0,"daemon_epoch":epoch}}),
                )
                .await;
                assert_disconnected(&mut stream, &listener).await;
                drop(replacement);
            }
            drop(original);
        });
        let error = assert_failed(&fixture.run(&["daemon", "stop"]).await);
        assert!(error.contains("daemon lock was replaced"), "{error}");
        server.await.unwrap();
        assert!(fixture.runtime.join("daemon.lock").is_file());
        assert!(fixture.runtime.join("old.lock").is_file());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_daemon_preserves_replacement_endpoint_after_shutdown_dispatch() {
    use std::os::unix::fs::MetadataExt;

    for replace_lock in [false, true] {
        let fixture = DaemonFixture::new();
        let lock = fixture.lock();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let lock_fd = lock.as_raw_fd();
        let mut command = fixture.command(&[
            tickloop_agent::agent_daemon::DAEMON_RUN_ARGUMENTS[0],
            tickloop_agent::agent_daemon::DAEMON_RUN_ARGUMENTS[1],
            fixture.runtime.to_str().unwrap(),
            &lock_fd.to_string(),
        ]);
        command.stderr(Stdio::piped());
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(lock_fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let original = command.spawn().unwrap();
        drop(lock);
        let endpoint = fixture.runtime.join("daemon.sock");
        let daemon = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(client) = LocalAgentClient::connect(&endpoint).await {
                    break client;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        let (launch, mut requests, _release) = launch_active_worker(&fixture).await;
        daemon
            .execute(create_command(daemon.daemon_epoch(), launch))
            .await
            .unwrap()
            .into_result()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        let worker_endpoint = std::fs::read_dir(&fixture.runtime)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with("w-"))
            .unwrap()
            .path();
        let worker = LocalAgentClient::connect(&worker_endpoint).await.unwrap();
        let worker_pid = worker.protocol().process_id;
        assert_eq!(
            unsafe { libc::kill(worker_pid as libc::pid_t, libc::SIGSTOP) },
            0
        );

        let mut management = management_connect(&endpoint).await;
        let request_id = Uuid::new_v4();
        write_frame(
            &mut management,
            &json!({"ShutdownDaemon": {
                "request_id":request_id,"daemon_epoch":daemon.daemon_epoch(),
            }}),
        )
        .await;
        assert_eq!(
            read_frame(&mut management).await.unwrap()["DaemonStopped"]["request_id"],
            json!(request_id)
        );
        std::fs::rename(&endpoint, fixture.runtime.join("original.sock")).unwrap();
        let replacement_lock = if replace_lock {
            std::fs::rename(
                fixture.runtime.join("daemon.lock"),
                fixture.runtime.join("original.lock"),
            )
            .unwrap();
            let replacement_lock = fixture.lock();
            assert_eq!(
                unsafe { libc::flock(replacement_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            Some(replacement_lock)
        } else {
            None
        };
        let replacement = fixture.listener();
        let metadata = std::fs::symlink_metadata(&endpoint).unwrap();
        let replacement_identity = (metadata.dev(), metadata.ino());
        assert_eq!(
            unsafe { libc::kill(worker_pid as libc::pid_t, libc::SIGCONT) },
            0
        );
        let output = tokio::time::timeout(Duration::from_secs(7), original.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert_processes_exited(&[worker_pid]).await;
        let remaining = std::fs::symlink_metadata(&endpoint)
            .expect("original daemon deleted the replacement daemon's endpoint");
        assert_eq!((remaining.dev(), remaining.ino()), replacement_identity);
        assert!(
            !output.status.success(),
            "daemon must report the endpoint/lock change"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("changed"));
        let connection = UnixStream::connect(&endpoint).await.unwrap();
        let accepted = replacement.accept().await.unwrap();
        drop((connection, accepted, replacement, replacement_lock));
    }
}
