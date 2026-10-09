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
const FIXTURE_TEXT: &str = "Keep this saved service-stop fixture history.";

struct ServiceFixture {
    directory: tempfile::TempDir,
    runtime: PathBuf,
    config: PathBuf,
    owner: Option<(u16, Uuid)>,
    launch_in_progress: bool,
}

impl ServiceFixture {
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
            owner: None,
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
            .env("TICKLOOP_AGENT_RUNTIME_DIR", &self.runtime)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    async fn run(&self, arguments: &[&str]) -> Output {
        tokio::time::timeout(CLI_TIMEOUT, self.command(arguments).output())
            .await
            .expect("service CLI exceeded its bounded shutdown window")
            .unwrap()
    }

    fn listener(&self) -> UnixListener {
        let socket = self.runtime.join("owner.sock");
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
            .open(self.runtime.join("owner.lock"))
            .unwrap()
    }
}

impl Drop for ServiceFixture {
    fn drop(&mut self) {
        let Some((major, epoch)) = self.owner else {
            if self.launch_in_progress {
                self.directory.disable_cleanup(true);
                eprintln!(
                    "Retained owned service fixture after an unconfirmed launch: {}",
                    self.runtime.display()
                );
            }
            return;
        };
        let socket = self.runtime.join("owner.sock");
        let Ok(mut stream) = BlockingUnixStream::connect(&socket) else {
            self.directory.disable_cleanup(true);
            eprintln!(
                "Retained unreachable owned service fixture: {}",
                self.runtime.display()
            );
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let cleanup = (|| -> std::io::Result<bool> {
            write_blocking(&mut stream, &json!({"Hello":{"major":major,"minor":0}}))?;
            let hello = read_blocking(&mut stream)?;
            if hello["Hello"]["owner_epoch"] != json!(epoch) {
                return Ok(false);
            }
            let request_id = Uuid::new_v4();
            write_blocking(
                &mut stream,
                &json!({"ShutdownOwner":{"request_id":request_id,"owner_epoch":epoch}}),
            )?;
            let response = read_blocking(&mut stream)?;
            Ok(response["OwnerStopped"]["request_id"] == json!(request_id))
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
                "Retained owned service fixture after incomplete cleanup: {}",
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

async fn legacy_connect(socket: &Path) -> (UnixStream, Value) {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    write_frame(&mut stream, &json!({"Hello":{"major":1,"minor":0}})).await;
    let response = read_frame(&mut stream).await.unwrap();
    assert_eq!(response["Hello"]["major"], 1);
    (stream, response["Hello"].clone())
}

async fn legacy_request(stream: &mut UnixStream, kind: &str, mut body: Value) -> Value {
    let request_id = Uuid::new_v4();
    body["request_id"] = json!(request_id);
    write_frame(stream, &json!({kind:body})).await;
    let response = read_frame(stream).await.unwrap();
    assert_eq!(
        response[kind_response(kind)]["request_id"],
        json!(request_id)
    );
    response[kind_response(kind)].clone()
}

fn kind_response(kind: &str) -> &str {
    match kind {
        "Execute" => "Receipt",
        "Observe" => "Opened",
        _ => panic!("fixture request kind {kind}"),
    }
}

fn assert_failed(output: &Output) -> String {
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Agent service stopped."));
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
        "the stop operation must not reconnect or start another owner"
    );
}

#[tokio::test]
async fn absent_stale_or_unready_service_is_never_started_or_removed() {
    let fixture = ServiceFixture::new();
    std::fs::remove_dir(&fixture.runtime).unwrap();
    for _ in 0..2 {
        let output = fixture.run(&["service", "stop"]).await;
        assert!(output.status.success());
        assert_eq!(output.stdout, b"Agent service is not running.\n");
        assert!(output.stderr.is_empty());
        assert!(!fixture.runtime.exists());
    }
    std::fs::create_dir(&fixture.runtime).unwrap();
    std::fs::set_permissions(&fixture.runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = fixture.listener();
    drop(listener);
    let socket = fixture.runtime.join("owner.sock");
    let output = fixture.run(&["service", "stop"]).await;
    assert!(output.status.success());
    assert!(
        socket.exists(),
        "stale sockets belong to startup arbitration"
    );
    assert!(!fixture.runtime.join("owner.lock").exists());
    std::fs::remove_file(&socket).unwrap();
    let lock = fixture.lock();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let output = fixture.run(&["service", "stop"]).await;
    assert!(assert_failed(&output).contains("starting or stopping"));
    assert!(!socket.exists());
    assert!(fixture.runtime.join("owner.lock").exists());
}

#[tokio::test]
async fn service_help_and_invalid_arguments_do_not_connect_or_touch_runtime() {
    let fixture = ServiceFixture::new();
    let listener = fixture.listener();
    for arguments in [
        vec!["service", "--help"],
        vec!["service", "stop", "-h"],
        vec!["service", "stop", "--help"],
        vec!["service"],
        vec!["service", "start"],
        vec!["service", "stop", "--force"],
        vec!["service", "stop", "extra"],
        vec!["service", "stop", "--help", "--unknown"],
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
        assert!(text.contains("Usage: tickloop service stop"));
        assert!(!fixture.runtime.join("owner.lock").exists());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    assert!(fixture.runtime.join("owner.sock").exists());
}

#[tokio::test]
async fn unsafe_runtime_socket_and_lock_are_rejected_before_any_handshake() {
    let fixture = ServiceFixture::new();
    let listener = fixture.listener();
    for (path, unsafe_mode, safe_mode) in [
        (&fixture.runtime, 0o755, 0o700),
        (&fixture.runtime.join("owner.sock"), 0o666, 0o600),
    ] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(unsafe_mode)).unwrap();
        let output = fixture.run(&["service", "stop"]).await;
        assert_failed(&output);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(safe_mode)).unwrap();
    }
    let lock = fixture.lock();
    std::fs::set_permissions(
        fixture.runtime.join("owner.lock"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_failed(&fixture.run(&["service", "stop"]).await);
    drop(lock);
    std::fs::remove_file(fixture.runtime.join("owner.lock")).unwrap();
    let target = fixture.directory.path().join("unrelated");
    std::fs::write(&target, b"unrelated private file").unwrap();
    symlink(&target, fixture.runtime.join("owner.lock")).unwrap();
    assert_failed(&fixture.run(&["service", "stop"]).await);
    assert_eq!(std::fs::read(&target).unwrap(), b"unrelated private file");
    std::fs::remove_file(fixture.runtime.join("owner.lock")).unwrap();
    std::fs::remove_file(fixture.runtime.join("owner.sock")).unwrap();
    symlink(&target, fixture.runtime.join("owner.sock")).unwrap();
    assert_failed(&fixture.run(&["service", "stop"]).await);
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
        json!({"Hello":{"major":3,"minor":0,"owner_epoch":Uuid::new_v4()}}),
        json!({"Hello":{"major":1,"minor":0}}),
        json!({"Hello":{"major":1,"minor":1,"owner_epoch":Uuid::new_v4()}}),
        json!({"OwnerStopped":{"request_id":Uuid::new_v4()}}),
    ] {
        let fixture = ServiceFixture::new();
        let listener = fixture.listener();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(read_frame(&mut stream).await.unwrap()["Hello"]["major"], 1);
            write_frame(&mut stream, &hello).await;
            assert_disconnected(&mut stream, &listener).await;
        });
        assert_failed(&fixture.run(&["service", "stop"]).await);
        server.await.unwrap();
        assert!(fixture.runtime.join("owner.sock").exists());
        assert!(!fixture.runtime.join("owner.lock").exists());
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
        let fixture = ServiceFixture::new();
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
                &json!({"Hello":{"major":1,"minor":0,"owner_epoch":epoch}}),
            )
            .await;
            let shutdown = read_frame(&mut stream).await.unwrap();
            assert_eq!(shutdown["ShutdownOwner"]["owner_epoch"], json!(epoch));
            assert_eq!(shutdown.as_object().unwrap().len(), 1);
            let request_id = shutdown["ShutdownOwner"]["request_id"].clone();
            match response_kind {
                "wrong-id" => write_frame(&mut stream, &json!({"OwnerStopped":{"request_id":Uuid::new_v4()}})).await,
                "wrong-epoch" => write_frame(&mut stream, &json!({"Error":{"request_id":request_id,"error":{"code":"EpochChanged","message":"fixture owner epoch changed"}}})).await,
                "wrong-response" => write_frame(&mut stream, &json!({"Receipt":{"request_id":request_id}})).await,
                "disconnect" => { drop(stream); assert!(tokio::time::timeout(Duration::from_millis(150), listener.accept()).await.is_err()); return; }
                "timeout" => tokio::time::sleep(Duration::from_millis(5250)).await,
                _ => unreachable!(),
            }
            assert_disconnected(&mut stream, &listener).await;
        });
        let message = assert_failed(&fixture.run(&["service", "stop"]).await);
        if response_kind != "wrong-epoch" {
            assert!(
                message.contains("not confirmed"),
                "{response_kind}: {message}"
            );
            assert!(message.contains("no stop request was resent"));
        } else {
            assert!(message.contains("fixture owner epoch changed"));
        }
        server.await.unwrap();
        assert!(fixture.runtime.join("owner.sock").exists());
        assert!(!fixture.runtime.join("owner.lock").exists());
    }
}

#[tokio::test]
async fn acknowledged_shutdown_preserves_a_replacement_owner_socket() {
    let fixture = ServiceFixture::new();
    let listener = fixture.listener();
    let socket = fixture.runtime.join("owner.sock");
    let old_socket = fixture.runtime.join("old.sock");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_frame(&mut stream).await.unwrap();
        let epoch = Uuid::new_v4();
        write_frame(
            &mut stream,
            &json!({"Hello":{"major":1,"minor":0,"owner_epoch":epoch}}),
        )
        .await;
        let shutdown = read_frame(&mut stream).await.unwrap();
        assert_eq!(shutdown["ShutdownOwner"]["owner_epoch"], json!(epoch));
        std::fs::rename(&socket, old_socket).unwrap();
        let replacement = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_frame(
            &mut stream,
            &json!({"OwnerStopped":{"request_id":shutdown["ShutdownOwner"]["request_id"]}}),
        )
        .await;
        assert_disconnected(&mut stream, &replacement).await;
        assert!(socket.exists());
    });
    let output = fixture.run(&["service", "stop"]).await;
    assert!(assert_failed(&output).contains("service changed while stopping"));
    server.await.unwrap();
    assert!(fixture.runtime.join("owner.sock").exists());
}

#[tokio::test]
async fn shutdown_ack_is_not_success_until_the_original_owner_lock_is_released() {
    let fixture = ServiceFixture::new();
    let listener = fixture.listener();
    let lock = fixture.lock();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let socket = fixture.runtime.join("owner.sock");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_frame(&mut stream).await.unwrap();
        let epoch = Uuid::new_v4();
        write_frame(
            &mut stream,
            &json!({"Hello":{"major":1,"minor":0,"owner_epoch":epoch}}),
        )
        .await;
        let shutdown = read_frame(&mut stream).await.unwrap();
        assert_eq!(shutdown["ShutdownOwner"]["owner_epoch"], json!(epoch));
        write_frame(
            &mut stream,
            &json!({"OwnerStopped":{"request_id":shutdown["ShutdownOwner"]["request_id"]}}),
        )
        .await;
        std::fs::remove_file(socket).unwrap();
        assert!(read_frame(&mut stream).await.is_none());
        drop(listener);
        tokio::time::sleep(Duration::from_millis(10250)).await;
        drop(lock);
    });
    let message = assert_failed(&fixture.run(&["service", "stop"]).await);
    assert!(message.contains("still stopping"));
    assert!(fixture.runtime.join("owner.lock").exists());
    assert!(!fixture.runtime.join("owner.sock").exists());
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
    .expect("an owned owner/worker PID is still running after service stop succeeded");
}

async fn launch_active_worker(
    fixture: &ServiceFixture,
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
        owner_epoch: epoch,
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
    fixture: &mut ServiceFixture,
    old_epoch: Uuid,
    record: TranscriptRef,
    worker_socket: PathBuf,
    process_ids: &[u32],
) {
    let output = fixture.run(&["service", "stop"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Agent service stopped."));
    assert!(!fixture.runtime.join("owner.sock").exists());
    assert!(!worker_socket.exists());
    assert_processes_exited(process_ids).await;
    let replacement = tickloop_agent::agent_daemon::connect_or_start_at(
        fixture.runtime.clone(),
        support::fixture_cli::executable(),
    )
    .await
    .unwrap();
    fixture.owner = Some((PROTOCOL_MAJOR, replacement.owner_epoch()));
    assert_ne!(replacement.owner_epoch(), old_epoch);
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
    let output = fixture.run(&["service", "stop"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.owner = None;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_cli_stop_ends_current_owner_and_active_worker_preserving_saved_history() {
    let mut fixture = ServiceFixture::new();
    fixture.launch_in_progress = true;
    let owner = tickloop_agent::agent_daemon::connect_or_start_at(
        fixture.runtime.clone(),
        support::fixture_cli::executable(),
    )
    .await
    .unwrap();
    let epoch = owner.owner_epoch();
    fixture.owner = Some((PROTOCOL_MAJOR, epoch));
    fixture.launch_in_progress = false;
    let (launch, mut requests, _release) = launch_active_worker(&fixture).await;
    let created = owner
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
    assert_eq!(worker.owner_epoch(), epoch);
    let record = TranscriptRef {
        store_root: fixture.config.clone(),
        chat_id: snapshot.conversation.id,
    };
    verify_stopped_and_restart(
        &mut fixture,
        epoch,
        record,
        worker_socket,
        &[owner.protocol().process_id, worker.protocol().process_id],
    )
    .await;
    assert!(
        requests.try_recv().is_err(),
        "stopping and restarting must not resend model work"
    );
    eprintln!(
        "actual current owner + active worker stopped; saved history readable; new owner epoch and zero resumed workers"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated preserved major-1 executable; never uses the default daemon"]
async fn rejected_startup_then_cli_stop_ends_legacy_owner_and_worker_before_current_restart() {
    let mut fixture = ServiceFixture::new();
    let executable = fixture.directory.path().join("legacy-tickloop");
    std::fs::copy(
        std::env::var_os("TICKLOOP_DIAG_OLD_OWNER").expect("preserved major-1 executable"),
        &executable,
    )
    .unwrap();
    fixture.launch_in_progress = true;
    let rejected =
        tickloop_agent::agent_daemon::connect_or_start_at(fixture.runtime.clone(), executable)
            .await;
    assert!(rejected.is_err());
    let (mut owner, hello) = legacy_connect(&fixture.runtime.join("owner.sock")).await;
    let epoch: Uuid = serde_json::from_value(hello["owner_epoch"].clone()).unwrap();
    fixture.owner = Some((1, epoch));
    fixture.launch_in_progress = false;
    let (launch, mut requests, _release) = launch_active_worker(&fixture).await;
    let created = legacy_request(
        &mut owner,
        "Execute",
        json!({"command":create_command(epoch, launch)}),
    )
    .await;
    let receipt: CommandReceipt = serde_json::from_value(created["receipt"].clone()).unwrap();
    let CommandResult::Created { snapshot, .. } = receipt.into_result().unwrap() else {
        panic!("legacy active worker created")
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
    let (_, worker_hello) = legacy_connect(&worker_socket).await;
    assert_eq!(worker_hello["owner_epoch"], hello["owner_epoch"]);
    let output = fixture
        .command(&["--working-directory-is-trusted"])
        .output()
        .await
        .unwrap();
    assert!(assert_failed(&output).contains("service stop"));
    let (_, unchanged) = legacy_connect(&fixture.runtime.join("owner.sock")).await;
    assert_eq!(unchanged["owner_epoch"], hello["owner_epoch"]);
    assert_eq!(unchanged["process_id"], hello["process_id"]);
    let (_, unchanged_worker) = legacy_connect(&worker_socket).await;
    assert_eq!(unchanged_worker["process_id"], worker_hello["process_id"]);
    let record = TranscriptRef {
        store_root: fixture.config.clone(),
        chat_id: snapshot.conversation.id,
    };
    verify_stopped_and_restart(
        &mut fixture,
        epoch,
        record,
        worker_socket,
        &[
            serde_json::from_value(hello["process_id"].clone()).unwrap(),
            serde_json::from_value(worker_hello["process_id"].clone()).unwrap(),
        ],
    )
    .await;
    assert!(
        requests.try_recv().is_err(),
        "the legacy task must never be resent by the new build"
    );
    eprintln!(
        "actual major-1 owner + active worker kept their PID/epoch after startup rejection, then current CLI stopped them; history preserved and current owner starts a new epoch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_stop_wire_can_stop_a_future_business_owner() {
    let mut fixture = ServiceFixture::new();
    fixture.launch_in_progress = true;
    let owner = tickloop_agent::agent_daemon::connect_or_start_at(
        fixture.runtime.clone(),
        env!("CARGO_BIN_EXE_tickloop").into(),
    )
    .await
    .unwrap();
    fixture.owner = Some((PROTOCOL_MAJOR, owner.owner_epoch()));
    fixture.launch_in_progress = false;
    let mut stream = UnixStream::connect(fixture.runtime.join("owner.sock"))
        .await
        .unwrap();
    write_frame(&mut stream, &json!({"Hello":{"major":1,"minor":0}})).await;
    let hello = read_frame(&mut stream).await.unwrap();
    assert_eq!(hello["Hello"]["major"], 1);
    assert_eq!(hello["Hello"]["owner_epoch"], json!(owner.owner_epoch()));
    let request_id = Uuid::new_v4();
    write_frame(
        &mut stream,
        &json!({"ShutdownOwner":{"request_id":request_id,"owner_epoch":owner.owner_epoch()}}),
    )
    .await;
    assert_eq!(
        read_frame(&mut stream).await.unwrap()["OwnerStopped"]["request_id"],
        json!(request_id)
    );
    tokio::time::timeout(Duration::from_secs(7), async {
        while fixture.runtime.join("owner.sock").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_processes_exited(&[owner.protocol().process_id]).await;
    fixture.owner = None;
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
async fn management_aliases_reject_every_business_request_before_dispatch() {
    let fixture = ServiceFixture::new();
    let access = std::sync::Arc::new(UndispatchedAccess(std::sync::atomic::AtomicUsize::new(0)));
    let epoch = Uuid::new_v4();
    let server = tokio::spawn(serve_owner_listener(
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
    for major in [1, 2] {
        for request in &requests {
            let mut stream = UnixStream::connect(fixture.runtime.join("owner.sock"))
                .await
                .unwrap();
            write_frame(&mut stream, &json!({"Hello":{"major":major,"minor":0}})).await;
            let hello = read_frame(&mut stream).await.unwrap();
            assert_eq!(hello["Hello"]["major"], major);
            assert_eq!(hello["Hello"]["minor"], 0);
            write_frame(&mut stream, request).await;
            let response = read_frame(&mut stream).await.unwrap();
            assert_eq!(response["Error"]["error"]["code"], "VersionMismatch");
            assert_eq!(response["Error"]["request_id"], json!(request_id));
            assert!(read_frame(&mut stream).await.is_none());
        }
    }
    assert_eq!(access.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let mut stream = UnixStream::connect(fixture.runtime.join("owner.sock"))
        .await
        .unwrap();
    write_frame(&mut stream, &json!({"Hello":{"major":2,"minor":0}})).await;
    read_frame(&mut stream).await.unwrap();
    write_frame(
        &mut stream,
        &json!({"ShutdownOwner":{"request_id":request_id,"owner_epoch":epoch}}),
    )
    .await;
    assert_eq!(
        read_frame(&mut stream).await.unwrap()["OwnerStopped"]["request_id"],
        json!(request_id)
    );
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn management_rejects_wrong_epoch_malformed_and_oversize_frames_without_dispatch() {
    let fixture = ServiceFixture::new();
    let access = std::sync::Arc::new(UndispatchedAccess(std::sync::atomic::AtomicUsize::new(0)));
    let epoch = Uuid::new_v4();
    let server = tokio::spawn(serve_owner_listener(
        fixture.listener(),
        access.clone(),
        epoch,
        support::TRANSPORT_LIMITS,
    ));
    let request_id = Uuid::new_v4();
    for request in [
        json!({"ShutdownOwner":{"request_id":request_id,"owner_epoch":Uuid::new_v4()}}),
        json!({"ShutdownOwner":{"request_id":"invalid-uuid","owner_epoch":epoch}}),
        json!({"Hello":{"major":1,"minor":0}}),
    ] {
        let (mut stream, _) = legacy_connect(&fixture.runtime.join("owner.sock")).await;
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
        let mut stream = UnixStream::connect(fixture.runtime.join("owner.sock"))
            .await
            .unwrap();
        if after_hello {
            write_frame(&mut stream, &json!({"Hello":{"major":1,"minor":0}})).await;
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
    let mut stream = UnixStream::connect(fixture.runtime.join("owner.sock"))
        .await
        .unwrap();
    stream.write_u32(1).await.unwrap();
    stream.write_all(b"{").await.unwrap();
    assert!(read_frame(&mut stream).await.is_none());
    let mut ordinary = UnixStream::connect(fixture.runtime.join("owner.sock"))
        .await
        .unwrap();
    write_frame(&mut ordinary, &json!({"Hello":{"major":2,"minor":1}})).await;
    assert_eq!(
        read_frame(&mut ordinary).await.unwrap()["Error"]["error"]["code"],
        "VersionMismatch"
    );
    assert!(read_frame(&mut ordinary).await.is_none());
    assert_eq!(access.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn worker_socket_has_no_cross_version_management_facade() {
    let fixture = ServiceFixture::new();
    let access = std::sync::Arc::new(UndispatchedAccess(std::sync::atomic::AtomicUsize::new(0)));
    let server = tokio::spawn(serve_worker_listener(
        fixture.listener(),
        access.clone(),
        Uuid::new_v4(),
        support::TRANSPORT_LIMITS,
    ));
    for major in [1, 2] {
        let mut stream = UnixStream::connect(fixture.runtime.join("owner.sock"))
            .await
            .unwrap();
        write_frame(&mut stream, &json!({"Hello":{"major":major,"minor":0}})).await;
        assert_eq!(
            read_frame(&mut stream).await.unwrap()["Error"]["error"]["code"],
            "VersionMismatch"
        );
        assert!(read_frame(&mut stream).await.is_none());
    }
    assert_eq!(access.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn only_version_mismatch_allows_the_legacy_control_alias_retry() {
    let fixture = ServiceFixture::new();
    let listener = fixture.listener();
    let socket = fixture.runtime.join("owner.sock");
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        assert_eq!(
            read_frame(&mut first).await.unwrap(),
            json!({"Hello":{"major":1,"minor":0}})
        );
        write_frame(&mut first, &json!({"Error":{"request_id":null,"error":{"code":"VersionMismatch","message":"legacy v2"}}})).await;
        assert!(read_frame(&mut first).await.is_none());
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(
            read_frame(&mut stream).await.unwrap(),
            json!({"Hello":{"major":2,"minor":0}})
        );
        let epoch = Uuid::new_v4();
        write_frame(&mut stream, &json!({"Hello":{"major":2,"minor":0,"owner_epoch":epoch,"capabilities":["UnknownFutureBusinessCapability"]}})).await;
        let shutdown = read_frame(&mut stream).await.unwrap();
        assert_eq!(shutdown["ShutdownOwner"]["owner_epoch"], json!(epoch));
        std::fs::remove_file(socket).unwrap();
        write_frame(
            &mut stream,
            &json!({"OwnerStopped":{"request_id":shutdown["ShutdownOwner"]["request_id"]}}),
        )
        .await;
        assert_disconnected(&mut stream, &listener).await;
    });
    assert!(fixture.run(&["service", "stop"]).await.status.success());
    server.await.unwrap();
}

#[tokio::test]
async fn control_oversize_reply_is_rejected_without_shutdown() {
    let fixture = ServiceFixture::new();
    let listener = fixture.listener();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_frame(&mut stream).await.unwrap();
        stream.write_u32(65537).await.unwrap();
        assert_disconnected(&mut stream, &listener).await;
    });
    assert!(assert_failed(&fixture.run(&["service", "stop"]).await).contains("64 KiB"));
    server.await.unwrap();
}

#[tokio::test]
async fn replacement_lock_before_shutdown_or_after_ack_is_preserved() {
    for after_ack in [false, true] {
        let fixture = ServiceFixture::new();
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
                    &json!({"Hello":{"major":1,"minor":0,"owner_epoch":epoch}}),
                )
                .await;
                let shutdown = read_frame(&mut stream).await.unwrap();
                assert_eq!(shutdown["ShutdownOwner"]["owner_epoch"], json!(epoch));
                std::fs::rename(runtime.join("owner.lock"), runtime.join("old.lock")).unwrap();
                let replacement = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(runtime.join("owner.lock"))
                    .unwrap();
                assert_eq!(
                    unsafe { libc::flock(replacement.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
                std::fs::remove_file(runtime.join("owner.sock")).unwrap();
                write_frame(
                    &mut stream,
                    &json!({"OwnerStopped":{"request_id":shutdown["ShutdownOwner"]["request_id"]}}),
                )
                .await;
                assert_disconnected(&mut stream, &listener).await;
                drop(replacement);
            } else {
                std::fs::rename(runtime.join("owner.lock"), runtime.join("old.lock")).unwrap();
                let replacement = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(runtime.join("owner.lock"))
                    .unwrap();
                write_frame(
                    &mut stream,
                    &json!({"Hello":{"major":1,"minor":0,"owner_epoch":epoch}}),
                )
                .await;
                assert_disconnected(&mut stream, &listener).await;
                drop(replacement);
            }
            drop(original);
        });
        let error = assert_failed(&fixture.run(&["service", "stop"]).await);
        assert!(error.contains("owner lock was replaced"), "{error}");
        server.await.unwrap();
        assert!(fixture.runtime.join("owner.lock").is_file());
        assert!(fixture.runtime.join("old.lock").is_file());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_owner_preserves_replacement_endpoint_after_shutdown_dispatch() {
    use std::os::unix::fs::MetadataExt;

    for replace_lock in [false, true] {
        let fixture = ServiceFixture::new();
        let lock = fixture.lock();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let lock_fd = lock.as_raw_fd();
        let mut command = fixture.command(&[
            "--agent-owner",
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
        let endpoint = fixture.runtime.join("owner.sock");
        let owner = tokio::time::timeout(Duration::from_secs(5), async {
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
        owner
            .execute(create_command(owner.owner_epoch(), launch))
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

        let (mut management, _) = legacy_connect(&endpoint).await;
        let request_id = Uuid::new_v4();
        write_frame(
            &mut management,
            &json!({"ShutdownOwner": {
                "request_id":request_id,"owner_epoch":owner.owner_epoch(),
            }}),
        )
        .await;
        assert_eq!(
            read_frame(&mut management).await.unwrap()["OwnerStopped"]["request_id"],
            json!(request_id)
        );
        std::fs::rename(&endpoint, fixture.runtime.join("original.sock")).unwrap();
        let replacement_lock = if replace_lock {
            std::fs::rename(
                fixture.runtime.join("owner.lock"),
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
            .expect("original daemon deleted the replacement owner's endpoint");
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
