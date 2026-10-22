//! Optional, best-effort lifecycle reports for the interactive View's Herdr Pane.
//! No subprocesses, socket I/O or tasks are created outside a complete Herdr environment.

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::watch,
    task::JoinHandle,
};

const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(1200);
const RESPONSE_LIMIT: u64 = 16 * 1024;
static LAST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    Idle,
    Working,
    Blocked,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

struct Context {
    socket: PathBuf,
    pane: String,
}

impl Context {
    fn from_values(interactive: bool, get: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        if !interactive || get("HERDR_ENV")?.to_str()? != "1" {
            return None;
        }
        let nonempty = |name| get(name).filter(|value| !value.is_empty());
        let socket = nonempty("HERDR_SOCKET_PATH")?;
        let pane = nonempty("HERDR_PANE_ID")?.into_string().ok()?;
        // The injected binary path is an additional environment marker, not a command to run.
        nonempty("HERDR_BIN_PATH")?;
        Some(Self {
            socket: socket.into(),
            pane,
        })
    }
}

pub(super) struct Reporter {
    sender: Option<watch::Sender<State>>,
    worker: Option<JoinHandle<()>>,
}

impl Reporter {
    /// Starts with Idle. Missing environment or runtime means reporting is disabled.
    pub(super) fn from_environment(interactive: bool) -> Option<Self> {
        // PTY fixtures rerun this test build as Views that inherit the developer's real Herdr
        // pane, and their reports overwrite that pane's agent. Reporter tests use `start`.
        if cfg!(test) {
            return None;
        }
        let context = Context::from_values(interactive, |name| std::env::var_os(name))?;
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        Some(Self::start(context, &runtime))
    }

    fn start(context: Context, runtime: &tokio::runtime::Handle) -> Self {
        let (sender, receiver) = watch::channel(State::Idle);
        Self {
            sender: Some(sender),
            worker: Some(runtime.spawn(run(context, receiver))),
        }
    }

    /// Latest-state only: never waits for Herdr, and identical states do not trigger I/O.
    pub(super) fn update(&self, state: State) {
        if let Some(sender) = &self.sender {
            sender.send_if_modified(|current| {
                if *current == state {
                    false
                } else {
                    *current = state;
                    true
                }
            });
        }
    }

    /// Await before the runtime exits to give the bounded release request time to finish.
    /// Drop also closes the channel, but cannot keep a departing runtime alive.
    pub(super) async fn shutdown(&mut self) {
        self.sender.take();
        if let Some(mut worker) = self.worker.take()
            && tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut worker)
                .await
                .is_err()
        {
            worker.abort();
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        // Closing the channel wakes the worker even when no state changed.
        self.sender.take();
    }
}

fn next_sequence() -> u64 {
    // Epoch nanoseconds survive ordinary reexec; a process-wide floor also orders same-tick
    // updates and local wall-clock regressions. Herdr's schema accepts a full uint64.
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    LAST_SEQUENCE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
            Some(epoch.max(last.saturating_add(1)))
        })
        .map(|last| epoch.max(last.saturating_add(1)))
        .unwrap()
}

async fn run(context: Context, mut receiver: watch::Receiver<State>) {
    let mut failures = 0u32;
    loop {
        if receiver.has_changed().is_err() {
            break;
        }
        let state = *receiver.borrow_and_update();
        let succeeded = request(&context, Some(state)).await;
        if receiver.has_changed().is_err() {
            break;
        }
        if receiver.has_changed().unwrap_or(false) {
            failures = 0;
            continue;
        }
        if succeeded {
            failures = 0;
        } else {
            failures += 1;
        }
        // At most three attempts per state. A changed state interrupts backoff and gets its
        // own attempts; exhausted failures wait quietly rather than polling an absent host.
        if !succeeded && failures < 3 {
            tokio::select! {
                changed = receiver.changed() => {
                    if changed.is_err() { break; }
                    failures = 0;
                }
                _ = tokio::time::sleep(Duration::from_millis(100 * u64::from(failures))) => {}
            }
        } else {
            if receiver.changed().await.is_err() {
                break;
            }
            failures = 0;
        }
    }
    // Never retry release: shutdown has a fixed upper bound and failures are invisible.
    let _ = request(&context, None).await;
}

async fn request(context: &Context, state: Option<State>) -> bool {
    let work = async {
        let seq = next_sequence();
        let id = seq.to_string();
        let mut params = json!({
            "pane_id": context.pane, "source": "tickloop", "agent": "tickloop", "seq": seq,
        });
        if let Some(state) = state {
            params["state"] = json!(state.as_str());
        }
        let method = if state.is_some() {
            "pane.report_agent"
        } else {
            "pane.release_agent"
        };
        let mut bytes = serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))?;
        bytes.push(b'\n');
        let mut socket = UnixStream::connect(&context.socket).await?;
        socket.write_all(&bytes).await?;
        let mut response = String::new();
        BufReader::new(socket.take(RESPONSE_LIMIT + 1))
            .read_line(&mut response)
            .await?;
        if response.len() as u64 > RESPONSE_LIMIT || !response.ends_with('\n') {
            return Ok(false);
        }
        let response: Value = serde_json::from_str(&response)?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
            response["id"] == id
                && response.get("result").is_some()
                && response.get("error").is_none(),
        )
    };
    matches!(
        tokio::time::timeout(REQUEST_TIMEOUT, work).await,
        Ok(Ok(true))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    fn context(socket: PathBuf) -> Context {
        Context {
            socket,
            pane: "test-pane".into(),
        }
    }

    async fn accept(listener: &UnixListener) -> Value {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        let mut line = String::new();
        socket.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        let response = json!({"id": request["id"], "result": {}}).to_string() + "\n";
        socket
            .get_mut()
            .write_all(response.as_bytes())
            .await
            .unwrap();
        request
    }

    #[test]
    fn environment_is_strict_and_inert_outside_herdr() {
        assert!(Context::from_values(false, |_| panic!("must not read environment")).is_none());
        let values = [
            ("HERDR_ENV", "1"),
            ("HERDR_SOCKET_PATH", "/tmp/test.sock"),
            ("HERDR_PANE_ID", "pane"),
            ("HERDR_BIN_PATH", "/bin/herdr"),
        ];
        let get = |name: &str| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        };
        assert!(Context::from_values(true, get).is_some());
        for missing in values.iter().map(|(key, _)| *key) {
            assert!(
                Context::from_values(true, |name| if name == missing { None } else { get(name) })
                    .is_none()
            );
            assert!(
                Context::from_values(true, |name| if name == missing {
                    Some(OsString::new())
                } else {
                    get(name)
                })
                .is_none()
            );
        }
        assert!(
            Context::from_values(true, |name| if name == "HERDR_ENV" {
                Some("0".into())
            } else {
                get(name)
            })
            .is_none()
        );
    }

    #[test]
    fn sequences_are_epoch_based_and_strictly_increasing() {
        let first = next_sequence();
        assert!(first > 1_000_000_000_000_000_000);
        assert!(next_sequence() > first);
    }

    #[tokio::test]
    async fn reports_latest_state_and_releases_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut reporter = Reporter::start(context(path), &tokio::runtime::Handle::current());
        reporter.update(State::Working);
        reporter.update(State::Idle);
        reporter.update(State::Blocked);
        let first = accept(&listener).await;
        assert_eq!(first["method"], "pane.report_agent");
        assert_eq!(first["params"]["state"], "blocked");
        assert_eq!(first["params"]["source"], "tickloop");
        assert_eq!(first["params"]["agent"], "tickloop");
        assert_eq!(first["params"]["pane_id"], "test-pane");
        reporter.update(State::Working);
        let second = accept(&listener).await;
        assert_eq!(second["params"]["state"], "working");
        let server = tokio::spawn(async move { accept(&listener).await });
        reporter.shutdown().await;
        let release = server.await.unwrap();
        assert_eq!(release["method"], "pane.release_agent");
        assert!(release["params"].get("state").is_none());
        assert!(second["params"]["seq"].as_u64() > first["params"]["seq"].as_u64());
        assert!(release["params"]["seq"].as_u64() > second["params"]["seq"].as_u64());
    }

    #[tokio::test]
    async fn drop_wakes_worker_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let reporter = Reporter::start(context(path), &tokio::runtime::Handle::current());
        assert_eq!(accept(&listener).await["params"]["state"], "idle");
        drop(reporter);
        let release = tokio::time::timeout(SHUTDOWN_TIMEOUT, accept(&listener))
            .await
            .unwrap();
        assert_eq!(release["method"], "pane.release_agent");
    }

    #[tokio::test]
    async fn missing_socket_does_not_block_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let mut reporter = Reporter::start(
            context(dir.path().join("missing")),
            &tokio::runtime::Handle::current(),
        );
        reporter.update(State::Working);
        tokio::time::timeout(SHUTDOWN_TIMEOUT, reporter.shutdown())
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn errors_retry_three_times_then_wait_for_a_new_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut reporter = Reporter::start(context(path), &tokio::runtime::Handle::current());
        for _ in 0..3 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            let report: Value = serde_json::from_str(&line).unwrap();
            let error =
                json!({"id": report["id"], "error": {"code": "test", "message": "unavailable"}})
                    .to_string()
                    + "\n";
            socket.get_mut().write_all(error.as_bytes()).await.unwrap();
        }
        // Error responses must not count as success, but retries must eventually stop.
        assert!(
            tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .is_err()
        );
        reporter.update(State::Idle);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .is_err()
        );
        reporter.update(State::Blocked);
        assert_eq!(accept(&listener).await["params"]["state"], "blocked");
        let server = tokio::spawn(async move { accept(&listener).await });
        reporter.shutdown().await;
        assert_eq!(server.await.unwrap()["method"], "pane.release_agent");
    }

    #[tokio::test(start_paused = true)]
    async fn unresponsive_socket_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let request = tokio::spawn(async move { request(&context(path), Some(State::Idle)).await });
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::advance(REQUEST_TIMEOUT).await;
        assert!(!request.await.unwrap());
    }
}
