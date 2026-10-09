#![cfg(unix)]
//! Real `tick` launchers under the Tick host library: hidden PTYs, the hosted launch contract,
//! one business process shared by protocol calls, its screen and an attached view, and process
//! cleanup.

use std::{path::Path, time::Duration};

use serde_json::{Value, json};
use tick_connect::hosted::{
    ATTACH_INPUT_BYTES, ATTACH_REQUEST_FRAME_BYTES, BUSINESS_FRAME_BYTES, CALL_TIMEOUT,
    CAPABILITY_DATA, CAPABILITY_LIFECYCLE, CONTROL_TIMEOUT, HostConnection, Role, read_frame,
    write_frame,
};
use tick_host::{host, paths::HostPaths};
use tokio::{
    io::BufReader,
    net::unix::{OwnedReadHalf, OwnedWriteHalf},
};

/// A protocol Tick whose screen shows its value, terminal size and the last line typed on its
/// terminal, so protocol calls, attached input and resizes are all visible in one process.
const COUNTER: &str = r#"
import json, os, signal, sys, threading, time
LARGE_PAYLOAD = 8 * 1024 * 1024 - 4096
instance = os.environ['TICKLOOP_INSTANCE_ID']
value = 0
typed = ''
def draw(*_):
    size = os.get_terminal_size(sys.stderr.fileno())
    print(f'\r\033[2KCounter {value} in {os.path.basename(os.getcwd())} {size.columns}x{size.lines} typed {typed}', file=sys.stderr, flush=True)
def keys():
    global typed
    for line in open('/dev/tty'):
        typed = line.strip()
        draw()
signal.signal(signal.SIGWINCH, draw)
threading.Thread(target=keys, daemon=True).start()
draw()
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':2,'tick_type':'example.counter','title':'Counter','description':'Counter','recover_with':'current','tools':[{'name':'increment','description':'Increment','params':{'type':'object'},'mutates':True},{'name':'current','description':'Read','params':{'type':'object'},'mutates':False},{'name':'flood','description':'Print many lines','params':{'type':'object'},'mutates':False},{'name':'slow','description':'Answer after 16 seconds','params':{'type':'object'},'mutates':False},{'name':'large','description':'Answer with almost 8 MiB','params':{'type':'object'},'mutates':False},{'name':'oversized_update','description':'Send an update beyond the limit','params':{'type':'object'},'mutates':False}]}
    elif method == 'tick/read':
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':value,'observed_at':'2026-10-07T00:00:00Z','data':{'value':value,'note':open('note.txt').read(),'sees_launch_token':'TICKLOOP_TICK_HOST_LAUNCH' in os.environ}}
    elif method == 'tool/call':
        if request['params']['tool'] == 'increment':
            value += 1
            draw()
        elif request['params']['tool'] == 'flood':
            for row in range(3000):
                print(f'flood {row:04d} ' + 'x' * 100, file=sys.stderr)
            draw()
        elif request['params']['tool'] == 'slow':
            time.sleep(16)
        elif request['params']['tool'] == 'oversized_update':
            print(json.dumps({'jsonrpc':'2.0','method':'tick/update','params':{'version':3,'tick_instance_id':instance,'source':'example.counter','sequence':value + 1,'observed_at':'2026-10-07T00:00:01Z','data':{'blob':'x' * (600 * 1024)}}}), flush=True)
        elif request['params']['tool'] == 'large':
            result = {'value':value,'payload':'x' * LARGE_PAYLOAD}
            print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
            continue
        result = {'value':value}
    else:
        raise RuntimeError(method)
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

struct Fixture {
    directory: tempfile::TempDir,
    paths: HostPaths,
    key: String,
    owner_epoch: uuid::Uuid,
    owner: Value,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Fixture {
    async fn start() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("th")
            .tempdir_in("/private/tmp")
            .unwrap();
        let paths = HostPaths {
            runtime: directory.path().join("h"),
        };
        let key = host::random_token();
        let server = tokio::spawn(host::serve(paths.clone(), key.clone()));
        for _ in 0..200 {
            if paths.socket().exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self {
            directory,
            paths,
            key,
            owner_epoch: uuid::Uuid::new_v4(),
            owner: json!({"root":"fixture-root","chat":uuid::Uuid::new_v4()}),
            server,
        }
    }

    async fn controller(&self) -> HostConnection {
        HostConnection::connect(
            &self.paths.socket(),
            Role::Controller {
                controller_key: self.key.clone(),
                owner_epoch: self.owner_epoch,
            },
            &[CAPABILITY_LIFECYCLE],
        )
        .await
        .unwrap()
    }

    async fn open(
        &self,
        controller: &mut HostConnection,
        command: Vec<String>,
        cwd: &Path,
    ) -> Result<Value, tick_connect::host_error::HostError> {
        controller
            .request(
                "open",
                json!({
                    "operation": uuid::Uuid::new_v4(),
                    "owner": self.owner,
                    "command": command,
                    "cwd": cwd,
                    "environment": std::env::vars().collect::<Vec<_>>(),
                    "title": null,
                }),
                CONTROL_TIMEOUT + Duration::from_secs(2),
            )
            .await
    }

    async fn reader(&self, opened: &Value) -> HostConnection {
        HostConnection::connect(
            &self.paths.socket(),
            Role::Instance {
                instance: serde_json::from_value(opened["instance"].clone()).unwrap(),
                data_token: opened["data_token"].as_str().unwrap().into(),
            },
            &[CAPABILITY_DATA],
        )
        .await
        .unwrap()
    }

    fn counter(&self) -> (Vec<String>, std::path::PathBuf) {
        let work = self.directory.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("note.txt"), "relative file").unwrap();
        let script = self.directory.path().join("counter.py");
        std::fs::write(&script, COUNTER).unwrap();
        (
            vec![
                env!("CARGO_BIN_EXE_tick").into(),
                "run".into(),
                "--".into(),
                which("python3"),
                "-u".into(),
                script.to_string_lossy().into_owned(),
            ],
            work,
        )
    }
}

fn which(program: &str) -> String {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join(program))
        .find(|path| path.is_file())
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

async fn screen_contains(reader: &mut HostConnection, needle: &str) -> Value {
    for _ in 0..200 {
        let snapshot = reader
            .request("snapshot", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        if snapshot["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().unwrap().contains(needle))
        {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the hidden screen never showed {needle:?}");
}

async fn send(requests: &mut OwnedWriteHalf, request: Value) {
    write_frame(requests, &request, ATTACH_REQUEST_FRAME_BYTES)
        .await
        .unwrap();
}

/// Paints the attached view's frames into a terminal model until it shows `needle`.
async fn view_shows(events: &mut BufReader<OwnedReadHalf>, view: &mut vt100::Parser, needle: &str) {
    let painted = tokio::time::timeout(Duration::from_secs(10), async {
        while !view.screen().contents().contains(needle) {
            let event = read_frame(events, BUSINESS_FRAME_BYTES)
                .await
                .unwrap()
                .expect("the view stays attached");
            assert_eq!(event["event"], "frame", "{event}");
            view.process(event["data"].as_str().unwrap().as_bytes());
        }
    })
    .await;
    assert!(
        painted.is_ok(),
        "the attached view never showed {needle:?}: {}",
        view.screen().contents()
    );
}

/// The event that ended an attached view, skipping the frames before it.
async fn view_end(events: &mut BufReader<OwnedReadHalf>) -> Option<Value> {
    loop {
        let event = read_frame(events, BUSINESS_FRAME_BYTES).await.unwrap()?;
        if event["event"] != "frame" {
            return Some(event);
        }
    }
}

async fn attach_when_released(fixture: &Fixture, opened: &Value) -> HostConnection {
    for _ in 0..200 {
        let mut view = fixture.reader(opened).await;
        match view.request("attach", json!({}), CONTROL_TIMEOUT).await {
            Ok(_) => return view,
            Err(error) if error.code == "Busy" => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => panic!("attach failed: {error:?}"),
        }
    }
    panic!("the attach lease was never released");
}

/// Whether a process holds a pseudo-terminal master descriptor.
fn holds_a_terminal_master(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .any(|target| target.to_string_lossy().ends_with("ptmx"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let listing = std::process::Command::new("lsof")
            .args(["-a", "-p", &pid.to_string(), "-d", "0-65535", "-F", "n"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&listing.stdout)
            .lines()
            .any(|line| line == "n/dev/ptmx")
    }
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

async fn gone(pid: u32) -> bool {
    for _ in 0..400 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hidden_demo_watchlist_runs_on_a_private_pty_without_herdr() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let opened = fixture
        .open(
            &mut controller,
            vec![
                env!("CARGO_BIN_EXE_tick").into(),
                "demo".into(),
                "watchlist".into(),
            ],
            fixture.directory.path(),
        )
        .await
        .unwrap();
    assert!(opened["instance"].as_str().unwrap().starts_with("hosted:"));
    let mut reader = fixture.reader(&opened).await;
    let descriptor = reader
        .request("describe", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(descriptor["protocol"], 2);
    let read = reader
        .request("read", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(read["tick_instance_id"], opened["instance"]);
    let snapshot = reader
        .request("snapshot", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(
        (snapshot["rows"].as_u64(), snapshot["columns"].as_u64()),
        (Some(30), Some(120))
    );
    let resources = reader
        .request("resources", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(resources["business_pid"], opened["business_pid"]);
    assert!(
        resources["rss_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    let business = opened["business_pid"].as_u64().unwrap() as u32;
    let closed = controller
        .request(
            "close",
            json!({"operation":uuid::Uuid::new_v4(),"owner":fixture.owner,"instance":opened["instance"]}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(closed["state"], "closed");
    assert!(gone(business).await, "close ends the business process");
    tokio::time::timeout(Duration::from_secs(5), fixture.server)
        .await
        .expect("the host exits after its last Tick ends")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn protocol_calls_and_the_screen_belong_to_one_counter_process() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut reader = fixture.reader(&opened).await;
    screen_contains(&mut reader, "Counter 0 in work").await;
    let read = reader
        .request("read", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(
        read["data"]["note"], "relative file",
        "cwd resolves relative files"
    );
    assert_eq!(
        read["data"]["sees_launch_token"], false,
        "the launch token stays with the launcher"
    );
    let called = reader
        .request(
            "call",
            json!({"tool":"increment","args":{}}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(called["value"], 1);
    screen_contains(&mut reader, "Counter 1 in work").await;
    let read = reader
        .request("read", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(
        read["data"]["value"], 1,
        "UI bytes never pollute protocol stdout"
    );
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_the_launcher_ends_its_business_process_and_the_host_reaps_it() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let survivor = fixture
        .open(&mut controller, command.clone(), &work)
        .await
        .unwrap();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let business = opened["business_pid"].as_u64().unwrap() as u32;
    let launcher = tick_host::process::sample(business).unwrap().parent;
    unsafe { libc::kill(launcher as libc::pid_t, libc::SIGKILL) };
    assert!(
        gone(business).await,
        "the business Tick does not outlive its launcher"
    );
    let mut listed = Value::Null;
    for _ in 0..200 {
        listed = controller
            .request("list", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        if listed["total"] == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(listed["items"][0]["instance"], survivor["instance"]);
    let diagnostics = controller
        .request("diagnostics", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(diagnostics["reaped"][0]["instance"], opened["instance"]);
    assert_eq!(diagnostics["reaped"][0]["reason"], "exited");
    let mut reader = fixture.reader(&survivor).await;
    reader
        .request(
            "call",
            json!({"tool":"increment","args":{}}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    let survivor_pid = survivor["business_pid"].as_u64().unwrap() as u32;
    controller
        .request(
            "close",
            json!({"operation":uuid::Uuid::new_v4(),"owner":fixture.owner,"instance":survivor["instance"]}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert!(gone(survivor_pid).await);
    tokio::time::timeout(Duration::from_secs(5), fixture.server)
        .await
        .expect("the host exits once its last Tick ended")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn host_shutdown_ends_every_tick_and_removes_its_endpoint() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let mut businesses = Vec::new();
    for _ in 0..3 {
        let (command, work) = fixture.counter();
        let opened = fixture.open(&mut controller, command, &work).await.unwrap();
        businesses.push(opened["business_pid"].as_u64().unwrap() as u32);
    }
    shutdown(&fixture).await;
    for business in businesses {
        assert!(
            !alive(business) || gone(business).await,
            "shutdown completes after its Ticks ended"
        );
    }
    assert!(!fixture.paths.socket().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_launcher_outside_the_foreground_is_refused_before_its_tick_starts() {
    let fixture = Fixture::start().await;
    let marker = fixture.directory.path().join("started");
    let binding = json!({
        "terminal": {"endpoint": fixture.paths.socket(), "epoch": uuid::Uuid::new_v4(), "terminal": uuid::Uuid::new_v4()},
        "operation": uuid::Uuid::new_v4(),
        "launch_token": "0".repeat(64),
    });
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_tick"))
        .args(["run", "--", "/usr/bin/touch"])
        .arg(&marker)
        .env("TICKLOOP_TICK_HOST_LAUNCH", binding.to_string())
        .env_remove("HERDR_ENV")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("foreground"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists(), "the business program never started");
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_attached_view_controls_the_terminal_until_it_detaches_or_disconnects() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut first = fixture.reader(&opened).await;
    let attached = first
        .request(
            "attach",
            json!({"size":{"rows":20,"columns":80}}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(attached["attached"], true);
    let (mut events, mut requests) = first.into_parts();
    let mut view = vt100::Parser::new(20, 80, 0);
    view_shows(&mut events, &mut view, "Counter 0 in work 80x20").await;
    let listed = controller
        .request("list", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(listed["items"][0]["attached"], true);

    let mut second = fixture.reader(&opened).await;
    let busy = second
        .request("attach", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(busy.code, "Busy", "the first view keeps the terminal");

    send(
        &mut requests,
        json!({"method":"input","params":{"data":b"hello\n".to_vec()}}),
    )
    .await;
    view_shows(&mut events, &mut view, "typed hello").await;
    send(
        &mut requests,
        json!({"method":"resize","params":{"size":{"rows":24,"columns":100}}}),
    )
    .await;
    view.screen_mut().set_size(24, 100);
    view_shows(&mut events, &mut view, "100x24").await;
    let called = second
        .request(
            "call",
            json!({"tool":"increment","args":{}}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(called["value"], 1);
    view_shows(
        &mut events,
        &mut view,
        "Counter 1 in work 100x24 typed hello",
    )
    .await;

    send(&mut requests, json!({"method":"detach"})).await;
    assert_eq!(view_end(&mut events).await, None, "detach ends the view");
    second
        .request("attach", json!({}), CONTROL_TIMEOUT)
        .await
        .expect("detach releases the lease");
    let (mut events, mut requests) = second.into_parts();
    send(
        &mut requests,
        json!({"method":"input","params":{"data":vec![b'x'; ATTACH_INPUT_BYTES + 1]}}),
    )
    .await;
    let refused = view_end(&mut events).await.unwrap();
    assert_eq!(refused["error"]["code"], "InputTooLarge", "{refused}");
    let third = attach_when_released(&fixture, &opened).await;
    drop(third);
    let fourth = attach_when_released(&fixture, &opened).await;
    let listed = controller
        .request("list", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(
        listed["items"][0]["business_pid"], opened["business_pid"],
        "views come and go around one business process"
    );
    drop(fourth);
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_view_loses_its_lease_after_two_seconds_and_the_tick_keeps_running() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut stalled = fixture.reader(&opened).await;
    stalled
        .request("attach", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    let attached_at = std::time::Instant::now();
    let mut reader = fixture.reader(&opened).await;
    let _released = tokio::time::timeout(CONTROL_TIMEOUT + host::STREAM_WRITE_TIMEOUT, async {
        loop {
            reader
                .request("call", json!({"tool":"flood","args":{}}), CALL_TIMEOUT)
                .await
                .unwrap();
            let mut probe = fixture.reader(&opened).await;
            match probe.request("attach", json!({}), CONTROL_TIMEOUT).await {
                Ok(_) => return probe,
                Err(error) if error.code == "Busy" => {}
                Err(error) => panic!("attach failed: {error:?}"),
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("a view that stops reading loses its lease");
    assert!(attached_at.elapsed() >= host::STREAM_WRITE_TIMEOUT);
    let called = reader
        .request(
            "call",
            json!({"tool":"increment","args":{}}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(called["value"], 1, "the Tick keeps running");
    drop(stalled);
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn launch_failures_are_reported_before_dispatch_and_the_host_continues() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let exits = fixture
        .open(
            &mut controller,
            vec!["/usr/bin/false".into()],
            fixture.directory.path(),
        )
        .await
        .unwrap_err();
    assert_eq!(exits.code, "LaunchFailed");
    assert!(!exits.dispatched, "nothing runs after a failed launch");
    let (command, work) = fixture.counter();
    // The Tick Session directory stops being usable: its path now names a file.
    let sessions = fixture.paths.sessions();
    let moved = fixture.directory.path().join("sessions-aside");
    std::fs::rename(&sessions, &moved).unwrap();
    std::fs::write(&sessions, "not a directory").unwrap();
    let unusable = fixture.open(&mut controller, command.clone(), &work).await;
    std::fs::remove_file(&sessions).unwrap();
    std::fs::rename(&moved, &sessions).unwrap();
    let unusable = unusable.unwrap_err();
    assert_eq!(unusable.code, "LaunchFailed", "{unusable:?}");
    let missing = fixture
        .open(
            &mut controller,
            command.clone(),
            &fixture.directory.path().join("missing"),
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code, "InvalidArguments");
    let diagnostics = controller
        .request("diagnostics", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(diagnostics["errors"], 2, "{diagnostics}");
    assert_eq!(diagnostics["ticks"], 0, "{diagnostics}");
    assert!(
        diagnostics["reaped"]
            .as_array()
            .unwrap()
            .iter()
            .all(|reaped| reaped["reason"] == "launch_failed"),
        "{diagnostics}"
    );
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    assert!(alive(opened["business_pid"].as_u64().unwrap() as u32));
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_update_breaks_only_its_own_tick() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let broken = fixture
        .open(&mut controller, command.clone(), &work)
        .await
        .unwrap();
    let healthy = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut reader = fixture.reader(&broken).await;
    let _ = reader
        .request(
            "call",
            json!({"tool":"oversized_update","args":{}}),
            CONTROL_TIMEOUT,
        )
        .await;
    let mut refused = None;
    for _ in 0..200 {
        match fixture
            .reader(&broken)
            .await
            .request("read", json!({}), CONTROL_TIMEOUT)
            .await
        {
            Err(error) => {
                refused = Some(error);
                break;
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
    assert!(
        refused.is_some(),
        "the Tick that broke its protocol is not read"
    );
    let called = fixture
        .reader(&healthy)
        .await
        .request(
            "call",
            json!({"tool":"increment","args":{}}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(called["value"], 1, "the other Tick continues");
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watchers_that_leave_a_quiet_tick_release_their_session_connections() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    // More departed watchers than one Tick Session holds connections.
    for _ in 0..12 {
        let mut watcher = fixture.reader(&opened).await;
        watcher
            .request("watch", json!({}), CONTROL_TIMEOUT)
            .await
            .expect("each new watcher is served");
        drop(watcher);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let called = fixture
        .reader(&opened)
        .await
        .request(
            "call",
            json!({"tool":"increment","args":{}}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(called["value"], 1);
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_legal_eight_mebibyte_tool_answer_passes_through_the_host() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut reader = fixture.reader(&opened).await;
    let answered = reader
        .request("call", json!({"tool":"large","args":{}}), CALL_TIMEOUT)
        .await
        .unwrap();
    let payload = answered["payload"].as_str().unwrap().len();
    assert!(payload > tick_connect::protocol::MAX_TOOL_RESPONSE_BYTES - 8192);
    let current = reader
        .request("call", json!({"tool":"current","args":{}}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(current["value"], 0, "the connection stays usable");
    shutdown(&fixture).await;
}

/// A launcher that speaks the launch contract itself: it tries to register before proving its
/// foreground, proves a wrong terminal, and reuses its launch token.
const PROBE_LAUNCHER: &str = r#"
import json, os, socket, sys
binding = json.loads(os.environ['TICKLOOP_TICK_HOST_LAUNCH'])
def connect():
    channel = socket.socket(socket.AF_UNIX)
    channel.connect(binding['terminal']['endpoint'])
    stream = channel.makefile('rw')
    stream.write(json.dumps({'channel':'business','major':1,'minor':0,'required':['launch'],'role':{'kind':'launcher','operation':binding['operation'],'launch_token':binding['launch_token']}}) + '\n')
    stream.flush()
    return channel, stream, json.loads(stream.readline())
channel, stream, hello = connect()
def ask(method, params):
    stream.write(json.dumps({'id':1,'method':method,'params':params}) + '\n')
    stream.flush()
    return json.loads(stream.readline())
results = {'hello': hello}
results['register_unverified'] = ask('register', {'instance':'hosted:00000000-0000-4000-8000-000000000000','business_pid':os.getpid()})
results['verify_wrong_terminal'] = ask('verify', {'device':{'device':1},'process_group':os.getpgrp()})
results['second_hello'] = connect()[2]
with open(sys.argv[1], 'w') as output:
    json.dump(results, output)
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_launch_token_works_once_and_only_after_a_proven_foreground() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let results = fixture.directory.path().join("probe.json");
    let refused = fixture
        .open(
            &mut controller,
            vec![
                which("python3"),
                "-c".into(),
                PROBE_LAUNCHER.into(),
                results.to_string_lossy().into_owned(),
            ],
            fixture.directory.path(),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, "LaunchFailed", "{refused:?}");
    let results: Value = serde_json::from_slice(&std::fs::read(&results).unwrap()).unwrap();
    assert!(results["hello"]["result"].is_object(), "{results}");
    assert_eq!(
        results["register_unverified"]["error"]["code"], "Forbidden",
        "{results}"
    );
    assert_eq!(
        results["verify_wrong_terminal"]["error"]["code"], "ForegroundMismatch",
        "{results}"
    );
    assert_eq!(
        results["second_hello"]["error"]["code"], "Forbidden",
        "the launch token works once: {results}"
    );
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn conflicting_or_unreadable_launch_contexts_never_start_a_tick() {
    let directory = tempfile::Builder::new()
        .prefix("thctx")
        .tempdir_in("/private/tmp")
        .unwrap();
    let marker = directory.path().join("started");
    let binding = json!({
        "terminal": {"endpoint": directory.path().join("host.sock"), "epoch": uuid::Uuid::new_v4(), "terminal": uuid::Uuid::new_v4()},
        "operation": uuid::Uuid::new_v4(),
        "launch_token": "0".repeat(64),
    })
    .to_string();
    let herdr = [
        ("HERDR_ENV", "1"),
        ("HERDR_SOCKET_PATH", "/nonexistent/herdr.sock"),
        ("HERDR_PANE_ID", "p1"),
    ];
    let cases: [(&[(&str, &str)], &str); 3] = [
        (
            &[
                ("TICKLOOP_TICK_HOST_LAUNCH", &binding),
                herdr[0],
                herdr[1],
                herdr[2],
            ],
            "cannot also carry a Herdr Pane context",
        ),
        (
            &[("TICKLOOP_TICK_HOST_LAUNCH", "{not json")],
            "TICKLOOP_TICK_HOST_LAUNCH is invalid",
        ),
        (&[herdr[0]], "HERDR_SOCKET_PATH is required"),
    ];
    for (environment, expected) in cases {
        // Only the case's own context: never the Herdr session this test may run inside.
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tick"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("HERDR_") {
                command.env_remove(name);
            }
        }
        let output = command
            .args(["run", "--", "/usr/bin/touch"])
            .arg(&marker)
            .env_remove("TICKLOOP_TICK_HOST_LAUNCH")
            .envs(environment.iter().copied())
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{expected}: {stderr}");
        assert!(stderr.contains(expected), "{expected}: {stderr}");
        assert!(!marker.exists(), "the business program never started");
    }
}

/// The prototype cut calls at 10 or 15 seconds; a legal call has the whole 60 second budget.
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_keeps_its_sixty_second_budget() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut reader = fixture.reader(&opened).await;
    let started = std::time::Instant::now();
    let answered = reader
        .request("call", json!({"tool":"slow","args":{}}), CALL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(answered["value"], 0);
    assert!(started.elapsed() >= Duration::from_secs(16));
    assert_eq!(CALL_TIMEOUT, Duration::from_secs(60));
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_requests_never_reach_the_tick_or_change_its_value() {
    use tokio::io::AsyncWriteExt;
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut reader = fixture.reader(&opened).await;
    let padding = "x".repeat(tick_connect::protocol::MAX_REQUEST_BYTES);
    let refused = reader
        .request(
            "call",
            json!({"tool":"increment","args":{"padding":padding}}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap_err();
    assert!(!refused.dispatched, "{refused:?}");
    let (_, mut oversized) = fixture.reader(&opened).await.into_parts();
    let mut frame = vec![b' '; BUSINESS_FRAME_BYTES + 1024];
    frame.push(b'\n');
    let _ = oversized.write_all(&frame).await;
    let current = reader
        .request("call", json!({"tool":"current","args":{}}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(current["value"], 0, "no oversized request reached the Tick");
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_launches_leave_every_terminal_master_only_in_the_host() {
    let fixture = Fixture::start().await;
    let (command, work) = fixture.counter();
    let mut launches = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let mut controller = fixture.controller().await;
        let request = json!({
            "operation": uuid::Uuid::new_v4(),
            "owner": fixture.owner,
            "command": command,
            "cwd": work,
            "environment": std::env::vars().collect::<Vec<_>>(),
            "title": null,
        });
        launches.spawn(async move {
            controller
                .request("open", request, CONTROL_TIMEOUT + Duration::from_secs(2))
                .await
                .unwrap()
        });
    }
    let mut processes = Vec::new();
    while let Some(opened) = launches.join_next().await {
        let business = opened.unwrap()["business_pid"].as_u64().unwrap() as u32;
        processes.push(business);
        processes.push(tick_host::process::sample(business).unwrap().parent);
    }
    assert!(
        holds_a_terminal_master(std::process::id()),
        "the in-process host holds the masters"
    );
    for pid in processes {
        assert!(
            !holds_a_terminal_master(pid),
            "process {pid} inherited a terminal master"
        );
    }
    shutdown(&fixture).await;
}

async fn shutdown(fixture: &Fixture) {
    use tick_connect::hosted::{MANAGEMENT_FRAME_BYTES, read_frame, write_frame};
    let (reader, mut writer) = tokio::net::UnixStream::connect(fixture.paths.socket())
        .await
        .unwrap()
        .into_split();
    let mut reader = tokio::io::BufReader::new(reader);
    write_frame(
        &mut writer,
        &json!({"channel":"management","major":1,"minor":0,"controller_key":fixture.key}),
        MANAGEMENT_FRAME_BYTES,
    )
    .await
    .unwrap();
    let hello = read_frame(&mut reader, MANAGEMENT_FRAME_BYTES)
        .await
        .unwrap()
        .unwrap();
    write_frame(
        &mut writer,
        &json!({"id":1,"method":"shutdown_host","params":{"epoch":hello["result"]["epoch"],"operation":uuid::Uuid::new_v4()}}),
        MANAGEMENT_FRAME_BYTES,
    )
    .await
    .unwrap();
    let completed = read_frame(&mut reader, MANAGEMENT_FRAME_BYTES)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed["result"]["state"], "completed", "{completed}");
    assert!(
        !fixture.paths.socket().exists(),
        "the host answers after removing its endpoint"
    );
}

/// No host is needed: the launcher proves its foreground before it contacts any endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn a_launcher_in_a_background_process_group_of_its_terminal_is_refused() {
    let directory = tempfile::Builder::new()
        .prefix("thbg")
        .tempdir_in("/private/tmp")
        .unwrap();
    let marker = directory.path().join("started");
    let status = directory.path().join("status");
    let binding = json!({
        "terminal": {"endpoint": directory.path().join("host.sock"), "epoch": uuid::Uuid::new_v4(), "terminal": uuid::Uuid::new_v4()},
        "operation": uuid::Uuid::new_v4(),
        "launch_token": "0".repeat(64),
    });
    let (terminal, slave) = tick_host::pty::Pty::open(Default::default()).unwrap();
    // Job control puts the launcher in its own process group, which is not the terminal's
    // foreground group.
    let mut shell = tokio::process::Command::new("/bin/sh");
    shell
        .args([
            "-c",
            r#"set -m; "$TICK" run -- /usr/bin/touch "$MARKER" & wait $!; echo $? > "$STATUS""#,
        ])
        .env("TICK", env!("CARGO_BIN_EXE_tick"))
        .env("MARKER", &marker)
        .env("STATUS", &status)
        .env("TICKLOOP_TICK_HOST_LAUNCH", binding.to_string())
        .env_remove("HERDR_ENV");
    slave.attach(&mut shell).unwrap();
    let mut child = shell.spawn().unwrap();
    drop(shell);
    let waited = tokio::time::timeout(Duration::from_secs(15), async {
        // Drain the terminal so the shell's job notices never block on a full PTY.
        let mut buffer = [0u8; 4096];
        loop {
            tokio::select! {
                status = child.wait() => break status.unwrap(),
                _ = terminal.read(&mut buffer) => {}
            }
        }
    })
    .await
    .expect("the shell finishes");
    assert!(waited.success());
    let launcher_status = std::fs::read_to_string(&status).unwrap();
    assert_ne!(launcher_status.trim(), "0", "the background launcher fails");
    assert!(!marker.exists(), "the business program never started");
}
