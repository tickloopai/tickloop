#![cfg(unix)]
//! Real `tick` launchers under the Tickhost library: hidden PTYs, the hosted launch contract,
//! one business process shared by protocol calls, its screen and an attached view, and process
//! cleanup.

use std::{path::Path, time::Duration};

use serde_json::{Value, json};
use tick_connect::hosted::{
    ATTACH_INPUT_BYTES, ATTACH_REQUEST_FRAME_BYTES, BUSINESS_FRAME_BYTES, BUSINESS_MAJOR,
    CALL_TIMEOUT, CAPABILITY_DATA, CAPABILITY_LIFECYCLE, CONTROL_TIMEOUT, HostConnection, Role,
    read_frame, write_frame,
};
use tickhost::{host, paths::HostPaths};
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
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':value,'observed_at':'2026-10-07T00:00:00Z','data':{'value':value,'note':open('note.txt').read(),'sees_launch_token':'TICKLOOP_TICKHOST_LAUNCH' in os.environ,'argument_bytes':sum(len(argument) for argument in sys.argv[1:])}}
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
    daemon_epoch: uuid::Uuid,
    owner: Value,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Fixture {
    async fn start() -> Self {
        Self::with_idle_timeout(host::DEFAULT_IDLE_TIMEOUT).await
    }

    async fn with_idle_timeout(idle_timeout: Duration) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("th")
            .tempdir_in("/private/tmp")
            .unwrap();
        let paths = HostPaths {
            runtime: directory.path().join("h"),
        };
        let key = host::random_token();
        let server = tokio::spawn(host::serve(
            paths.clone(),
            host::HostConfig {
                controller_key: key.clone(),
                idle_timeout,
            },
        ));
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
            daemon_epoch: uuid::Uuid::new_v4(),
            owner: json!({"root":"fixture-root","chat":uuid::Uuid::new_v4()}),
            server,
        }
    }

    async fn controller(&self) -> HostConnection {
        HostConnection::connect(
            &self.paths.socket(),
            Role::Controller {
                controller_key: self.key.clone(),
                daemon_epoch: self.daemon_epoch,
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
                    "owner_revision": 0,
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
        .request("read", json!({"activity":"explicit"}), CONTROL_TIMEOUT)
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
        .request("read", json!({"activity":"explicit"}), CONTROL_TIMEOUT)
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
        .request("read", json!({"activity":"explicit"}), CONTROL_TIMEOUT)
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
    let launcher = tickhost::process::sample(business).unwrap().parent;
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

/// A listed title stays within its bound however long the command it names, so a page of rows
/// never outgrows its frame, while the Tick itself starts with every argument.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_command_is_listed_under_a_bounded_title_and_launches_whole() {
    const ARGUMENTS: usize = 4;
    const ARGUMENT_BYTES: usize = 64 * 1024;
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (mut command, work) = fixture.counter();
    command.extend((0..ARGUMENTS).map(|_| "x".repeat(ARGUMENT_BYTES)));
    let title = command.join(" ");
    let opened = controller
        .request(
            "open",
            json!({
                "operation": uuid::Uuid::new_v4(),
                "owner": fixture.owner,
                "owner_revision": 0,
                "command": command,
                "cwd": work,
                "environment": std::env::vars().collect::<Vec<_>>(),
                "title": title,
            }),
            CONTROL_TIMEOUT + Duration::from_secs(2),
        )
        .await
        .unwrap();
    let listed = controller
        .request("list", json!({"owner":fixture.owner}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(listed["items"][0]["instance"], opened["instance"]);
    let listed_title = listed["items"][0]["title"].as_str().unwrap();
    assert_eq!(listed_title.chars().count(), host::TITLE_CHARACTERS);
    let kept = listed_title
        .strip_suffix(host::TITLE_ELLIPSIS)
        .expect("a cut title ends with the ellipsis");
    assert!(title.starts_with(kept), "{listed_title}");
    let mut reader = fixture.reader(&opened).await;
    let read = reader
        .request("read", json!({"activity":"explicit"}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(
        read["data"]["argument_bytes"],
        ARGUMENTS * ARGUMENT_BYTES,
        "the launch keeps every argument"
    );
    shutdown(&fixture).await;
}

/// An idle reader connection keeps nothing of its Tick alive: once the Tick is reaped the host
/// closes it, while the host and its other Ticks continue.
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_reader_connection_is_closed_once_its_tick_is_reaped() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let survivor = fixture
        .open(&mut controller, command.clone(), &work)
        .await
        .unwrap();
    let closing = fixture.open(&mut controller, command, &work).await.unwrap();
    let (mut replies, _requests) = fixture.reader(&closing).await.into_parts();
    controller
        .request(
            "close",
            json!({"operation":uuid::Uuid::new_v4(),"owner":fixture.owner,"instance":closing["instance"]}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    let ended = tokio::time::timeout(
        Duration::from_secs(5),
        read_frame(&mut replies, BUSINESS_FRAME_BYTES),
    )
    .await
    .expect("the host closes an idle reader of a reaped Tick");
    assert_eq!(ended.unwrap(), None);
    let mut reader = fixture.reader(&survivor).await;
    reader
        .request("read", json!({"activity":"explicit"}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    shutdown(&fixture).await;
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
        .env("TICKLOOP_TICKHOST_LAUNCH", binding.to_string())
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

/// The owner lets one view attach to its Tick's terminal without handing out the Tick's data
/// token: a grant works once, only to attach, only for the Tick it names, and a newer grant
/// replaces an earlier one.
#[tokio::test(flavor = "multi_thread")]
async fn a_view_grant_attaches_one_view_once_and_allows_nothing_else() {
    async fn grant(controller: &mut HostConnection, owner: &Value, instance: &Value) -> Value {
        controller
            .request(
                "allow_view",
                json!({"owner":owner,"instance":instance}),
                CONTROL_TIMEOUT,
            )
            .await
            .unwrap()["view"]
            .clone()
    }
    async fn viewer(
        fixture: &Fixture,
        instance: &Value,
        view: &Value,
    ) -> Result<HostConnection, tick_connect::host_error::HostError> {
        HostConnection::connect(
            &fixture.paths.socket(),
            Role::Viewer {
                instance: serde_json::from_value(instance.clone()).unwrap(),
                view: serde_json::from_value(view.clone()).unwrap(),
            },
            &[CAPABILITY_DATA],
        )
        .await
    }
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let instance = opened["instance"].clone();
    let foreign = controller
        .request(
            "allow_view",
            json!({"owner":{"root":"fixture-root","chat":uuid::Uuid::new_v4()},"instance":instance}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap_err();
    assert_eq!(
        foreign.code, "TargetGone",
        "only the Tick's owner grants a view"
    );

    let replaced = grant(&mut controller, &fixture.owner, &instance).await;
    let granted = grant(&mut controller, &fixture.owner, &instance).await;
    let refused = viewer(&fixture, &instance, &replaced).await.err().unwrap();
    assert_eq!(
        refused.code, "Forbidden",
        "a newer grant replaces an earlier one"
    );
    let mut view = viewer(&fixture, &instance, &granted).await.unwrap();
    let read = view
        .request("read", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(read.code, "MethodNotAllowed", "a view only attaches");
    let attached = view
        .request(
            "attach",
            json!({"size":{"rows":20,"columns":80}}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(attached["attached"], true);
    let (mut events, mut requests) = view.into_parts();
    let mut screen = vt100::Parser::new(20, 80, 0);
    view_shows(&mut events, &mut screen, "Counter 0 in work 80x20").await;
    send(&mut requests, json!({"method":"detach"})).await;
    assert_eq!(view_end(&mut events).await, None, "detach ends the view");
    let reused = viewer(&fixture, &instance, &granted).await.err().unwrap();
    assert_eq!(reused.code, "Forbidden", "a grant works once");
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
    let released = tokio::time::timeout(CONTROL_TIMEOUT, async {
        loop {
            reader
                .request("call", json!({"tool":"flood","args":{}}), CALL_TIMEOUT)
                .await
                .unwrap();
            let mut probe = fixture.reader(&opened).await;
            match probe.request("attach", json!({}), CONTROL_TIMEOUT).await {
                Ok(_) => break probe,
                Err(error) if error.code == "Busy" => tokio::task::yield_now().await,
                Err(error) => panic!("attach failed: {error:?}"),
            }
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
    drop(released);
    drop(stalled);
    shutdown(&fixture).await;
}

/// A resize outside 2–300 rows or 2–500 columns is refused: an attach asking for one gets an
/// error, an attached view sending one ends with it, and the terminal keeps its size while the
/// Tick keeps running.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_resize_is_refused_and_leaves_the_terminal_and_the_tick_unchanged() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut reader = fixture.reader(&opened).await;
    screen_contains(&mut reader, "120x30").await;
    let terminal_size = |snapshot: &Value| (snapshot["rows"].clone(), snapshot["columns"].clone());
    let invalid = [
        json!({"rows":1,"columns":80}),
        json!({"rows":301,"columns":80}),
        json!({"rows":24,"columns":1}),
        json!({"rows":24,"columns":501}),
    ];
    for size in &invalid {
        let refused = fixture
            .reader(&opened)
            .await
            .request("attach", json!({"size":size}), CONTROL_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(refused.code, "InvalidArguments", "{size}");

        let (mut events, mut requests) = attach_when_released(&fixture, &opened).await.into_parts();
        send(
            &mut requests,
            json!({"method":"resize","params":{"size":size}}),
        )
        .await;
        let ended = view_end(&mut events).await.unwrap();
        assert_eq!(
            ended["error"]["code"], "InvalidArguments",
            "{size}: {ended}"
        );
        let snapshot = reader
            .request("snapshot", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(terminal_size(&snapshot), (json!(30), json!(120)), "{size}");
    }
    let called = reader
        .request(
            "call",
            json!({"tool":"increment","args":{}}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(called["value"], 1, "the Tick keeps running");
    let snapshot = screen_contains(&mut reader, "Counter 1 in work 120x30").await;
    assert_eq!(terminal_size(&snapshot), (json!(30), json!(120)));
    attach_when_released(&fixture, &opened).await;
    shutdown(&fixture).await;
}

/// A reply its reader leaves unread is given up after the 10-second control bound: the host
/// closes that connection instead of holding the reply, and the Tick keeps serving others.
#[tokio::test(flavor = "multi_thread")]
async fn a_reply_left_unread_past_the_control_bound_closes_only_its_connection() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let (mut replies, mut requests) = fixture.reader(&opened).await.into_parts();
    write_frame(
        &mut requests,
        &json!({"id":1,"method":"call","params":{"tool":"large","args":{}}}),
        BUSINESS_FRAME_BYTES,
    )
    .await
    .unwrap();
    tokio::time::sleep(CONTROL_TIMEOUT + Duration::from_secs(5)).await;
    let left = tokio::time::timeout(
        CONTROL_TIMEOUT,
        read_frame(&mut replies, BUSINESS_FRAME_BYTES),
    )
    .await
    .expect("the host closed the stalled connection");
    assert!(
        !matches!(left, Ok(Some(_))),
        "the stalled reply never arrives whole after the bound"
    );
    let current = fixture
        .reader(&opened)
        .await
        .request("call", json!({"tool":"current","args":{}}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(current["value"], 0, "the Tick keeps serving");
    shutdown(&fixture).await;
}

/// Ending an idle Tick whose launcher ignores SIGTERM takes the whole 3-second grace before
/// SIGKILL; the host keeps accepting connections meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn ending_a_stubborn_idle_tick_never_pauses_new_connections() {
    let fixture = Fixture::with_idle_timeout(Duration::from_secs(2)).await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let stubborn = fixture
        .open(&mut controller, command.clone(), &work)
        .await
        .unwrap();
    // An attached view keeps the other Tick, so the host never empties during the test.
    let kept = fixture.open(&mut controller, command, &work).await.unwrap();
    let (mut view_events, view_requests) = attach_when_released(&fixture, &kept).await.into_parts();
    let drain = tokio::spawn(async move {
        while let Ok(Some(_)) = read_frame(&mut view_events, BUSINESS_FRAME_BYTES).await {}
    });
    let business = stubborn["business_pid"].as_u64().unwrap() as u32;
    let launcher = tickhost::process::sample(business).unwrap().parent;
    // A stopped launcher leaves SIGTERM pending, so only SIGKILL after the grace ends it.
    unsafe { libc::kill(launcher as libc::pid_t, libc::SIGSTOP) };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut slowest = Duration::ZERO;
    while alive(launcher) {
        assert!(
            std::time::Instant::now() < deadline,
            "the idle Tick never ended"
        );
        let connecting = std::time::Instant::now();
        drop(fixture.controller().await);
        slowest = slowest.max(connecting.elapsed());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        slowest < Duration::from_secs(1),
        "a new connection waited {slowest:?} while an idle Tick was ending"
    );
    assert!(gone(business).await);
    let mut reaped = Vec::new();
    for _ in 0..200 {
        let diagnostics = controller
            .request("diagnostics", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        reaped = diagnostics["reaped"].as_array().unwrap().clone();
        if !reaped.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(reaped.len(), 1, "{reaped:?}");
    assert_eq!(reaped[0]["reason"], "idle");
    drop(view_requests);
    drain.abort();
    shutdown(&fixture).await;
}

/// A launcher that binds its Tick Session socket in the directory the host gives it, ignores
/// SIGTERM and never registers.
const STUBBORN_LAUNCHER: &str = r#"
import os, signal, socket, time, uuid
signal.signal(signal.SIGTERM, signal.SIG_IGN)
sessions = os.environ['TICKLOOP_TICK_RUNTIME_DIR']
os.makedirs(sessions, exist_ok=True)
listener = socket.socket(socket.AF_UNIX)
listener.bind(os.path.join(sessions, f'{uuid.uuid4()}.sock'))
listener.listen()
time.sleep(60)
"#;

/// Every Tick Session socket under the host's session directory.
fn session_sockets(fixture: &Fixture) -> Vec<std::path::PathBuf> {
    fn walk(directory: &Path, sockets: &mut Vec<std::path::PathBuf>) {
        use std::os::unix::fs::FileTypeExt;
        for entry in std::fs::read_dir(directory).unwrap().flatten() {
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                walk(&entry.path(), sockets);
            } else if kind.is_socket() {
                sockets.push(entry.path());
            }
        }
    }
    let mut sockets = Vec::new();
    walk(&fixture.paths.sessions(), &mut sockets);
    sockets
}

/// The host removes the Tick Session sockets of the Ticks it reaps, including those of a
/// launcher killed before it could remove its own: by kill -9, or by SIGKILL once it ignored
/// SIGTERM for the whole grace while it never finished starting.
#[tokio::test(flavor = "multi_thread")]
async fn the_session_sockets_of_killed_launchers_go_with_their_ticks() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let kept = fixture
        .open(&mut controller, command.clone(), &work)
        .await
        .unwrap();
    let killed = fixture.open(&mut controller, command, &work).await.unwrap();
    let socket_of = |opened: &Value| {
        let run = opened["instance"]
            .as_str()
            .unwrap()
            .strip_prefix("hosted:")
            .unwrap()
            .to_owned();
        format!("{run}.sock")
    };
    let named = |sockets: &[std::path::PathBuf], name: &str| {
        sockets
            .iter()
            .filter(|socket| socket.file_name().unwrap() == name)
            .count()
    };
    let sockets = session_sockets(&fixture);
    assert_eq!(named(&sockets, &socket_of(&killed)), 1, "{sockets:?}");

    let business = killed["business_pid"].as_u64().unwrap() as u32;
    let launcher = tickhost::process::sample(business).unwrap().parent;
    unsafe { libc::kill(launcher as libc::pid_t, libc::SIGKILL) };
    assert!(gone(business).await);
    for _ in 0..200 {
        let listed = controller
            .request("list", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        if listed["total"] == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let stubborn = controller
        .request(
            "open",
            json!({
                "operation": uuid::Uuid::new_v4(),
                "owner": fixture.owner,
                "owner_revision": 0,
                "command": [which("python3"), "-c", STUBBORN_LAUNCHER],
                "cwd": fixture.directory.path(),
                "environment": std::env::vars().collect::<Vec<_>>(),
                "title": null,
            }),
            CONTROL_TIMEOUT + host::TERMINATE_GRACE + Duration::from_secs(2),
        )
        .await
        .unwrap_err();
    assert_eq!(stubborn.code, "LaunchFailed", "{stubborn:?}");

    let sockets = session_sockets(&fixture);
    assert_eq!(sockets.len(), 1, "{sockets:?}");
    assert_eq!(named(&sockets, &socket_of(&kept)), 1, "{sockets:?}");
    shutdown(&fixture).await;
    assert_eq!(session_sockets(&fixture), Vec::<std::path::PathBuf>::new());
}

/// Opens a counter for `chat` of the fixture root at `owner_revision`.
async fn open_for(
    fixture: &Fixture,
    controller: &mut HostConnection,
    chat: uuid::Uuid,
    owner_revision: u64,
) -> Result<Value, tick_connect::host_error::HostError> {
    let (command, work) = fixture.counter();
    controller
        .request(
            "open",
            json!({
                "operation": uuid::Uuid::new_v4(),
                "owner": {"root": "fixture-root", "chat": chat},
                "owner_revision": owner_revision,
                "command": command,
                "cwd": work,
                "environment": std::env::vars().collect::<Vec<_>>(),
                "title": null,
            }),
            CONTROL_TIMEOUT + Duration::from_secs(2),
        )
        .await
}

/// With a 2-second idle timeout, a Tick that is unused, or only read automatically, listed,
/// sampled and snapshotted, is reaped; an attached one, one with any approved Watch and one read
/// explicitly are kept. Removing the last Watch restarts the idle time, a detach does not, and
/// the emptied host exits.
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_tick_is_reaped_unless_attached_watched_or_explicitly_used() {
    let fixture = Fixture::with_idle_timeout(Duration::from_secs(2)).await;
    let mut controller = fixture.controller().await;
    let chat = uuid::Uuid::new_v4();
    let mut opened = Vec::new();
    for _ in 0..5 {
        opened.push(open_for(&fixture, &mut controller, chat, 0).await.unwrap());
    }
    let pid = |tick: &Value| tick["business_pid"].as_u64().unwrap() as u32;
    let [unused, polled, attached, watched, used] = <[Value; 5]>::try_from(opened).unwrap();
    let (mut view_events, view_requests) = {
        let mut view = fixture.reader(&attached).await;
        view.request("attach", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        view.into_parts()
    };
    let drain = tokio::spawn(async move {
        while let Ok(Some(_)) = read_frame(&mut view_events, BUSINESS_FRAME_BYTES).await {}
    });
    let set_intents = |watches: Vec<uuid::Uuid>| json!({"owner":{"root":"fixture-root","chat":chat},"instance":watched["instance"],"watches":watches});
    controller
        .request(
            "set_intents",
            set_intents(vec![uuid::Uuid::new_v4(), uuid::Uuid::new_v4()]),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    let mut polled_reader = fixture.reader(&polled).await;
    let mut used_reader = fixture.reader(&used).await;
    for _ in 0..8 {
        for (method, params) in [
            ("read", json!({"activity":"automatic"})),
            ("snapshot", json!({})),
            ("resources", json!({})),
        ] {
            // Fails with TargetGone once these requests failed to keep the Tick.
            let _ = polled_reader.request(method, params, CONTROL_TIMEOUT).await;
        }
        controller
            .request("list", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        used_reader
            .request("read", json!({"activity":"explicit"}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    assert!(gone(pid(&unused)).await, "an unused Tick idles out");
    assert!(
        gone(pid(&polled)).await,
        "automatic reads, lists, samples and snapshots keep nothing"
    );
    for kept in [&attached, &watched, &used] {
        assert!(alive(pid(kept)), "{kept}");
    }

    controller
        .request(
            "set_intents",
            set_intents(vec![uuid::Uuid::new_v4()]),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(alive(pid(&watched)), "any one Watch keeps the Tick");
    assert!(
        gone(pid(&used)).await,
        "explicit use protects only while it lasts"
    );

    controller
        .request("set_intents", set_intents(Vec::new()), CONTROL_TIMEOUT)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        alive(pid(&watched)),
        "the last unsubscribe restarts the idle time"
    );
    assert!(gone(pid(&watched)).await);

    let diagnostics = tokio::time::timeout(CONTROL_TIMEOUT, async {
        loop {
            let diagnostics = controller
                .request("diagnostics", json!({}), CONTROL_TIMEOUT)
                .await
                .unwrap();
            if diagnostics["reaped"].as_array().unwrap().len() >= 4 {
                break diagnostics;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("every finished idle reap is recorded in diagnostics");
    let reasons = diagnostics["reaped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reaped| reaped["reason"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(reasons, vec!["idle"; 4]);

    let detached = std::time::Instant::now();
    drop(view_requests);
    drain.abort();
    assert!(gone(pid(&attached)).await);
    assert!(
        detached.elapsed() < Duration::from_millis(1500),
        "a detach does not restart the idle time"
    );
    drop(controller);
    drop(polled_reader);
    drop(used_reader);
    let exited = tokio::time::timeout(Duration::from_secs(5), fixture.server)
        .await
        .expect("an emptied host exits")
        .unwrap();
    exited.unwrap();
}

/// A Watch intent is a lease: renewed in time it keeps the Tick, but one nobody renews lapses
/// after the idle timeout, and the Tick then idles out like any other.
#[tokio::test(flavor = "multi_thread")]
async fn an_unrenewed_watch_intent_lapses_and_the_tick_then_idles_out() {
    let fixture = Fixture::with_idle_timeout(Duration::from_secs(2)).await;
    let mut controller = fixture.controller().await;
    let chat = uuid::Uuid::new_v4();
    let renewed = open_for(&fixture, &mut controller, chat, 0).await.unwrap();
    let abandoned = open_for(&fixture, &mut controller, chat, 0).await.unwrap();
    let pid = |tick: &Value| tick["business_pid"].as_u64().unwrap() as u32;
    let intents = |tick: &Value| json!({"owner":{"root":"fixture-root","chat":chat},"instance":tick["instance"],"watches":[uuid::Uuid::new_v4()]});
    controller
        .request("set_intents", intents(&abandoned), CONTROL_TIMEOUT)
        .await
        .unwrap();
    let lapsed_at = std::time::Instant::now() + Duration::from_secs(2);
    for _ in 0..12 {
        controller
            .request("set_intents", intents(&renewed), CONTROL_TIMEOUT)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(alive(pid(&renewed)), "a renewed intent keeps its Tick");
    assert!(gone(pid(&abandoned)).await, "an abandoned intent lapses");
    assert!(
        std::time::Instant::now() >= lapsed_at,
        "it protected the Tick until it lapsed"
    );
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_complete_newer_owner_snapshot_reaps_the_missing_chats_of_its_root() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (kept, missing) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let kept_tick = open_for(&fixture, &mut controller, kept, 0).await.unwrap();
    let missing_tick = open_for(&fixture, &mut controller, missing, 0)
        .await
        .unwrap();
    let snapshot = |root: &str, revision: u64, chunk: u32, chunks: u32, owners: Vec<uuid::Uuid>| json!({"root":root,"revision":revision,"chunk":chunk,"chunks":chunks,"owners":owners});
    let partial = controller
        .request(
            "reconcile_owners",
            snapshot("fixture-root", 1, 0, 2, vec![kept]),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(partial["applied"], false);
    let other_root = controller
        .request(
            "reconcile_owners",
            snapshot("other-root", 1, 0, 1, vec![]),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(
        other_root["reaped"],
        json!([]),
        "another root never reaps these"
    );
    let pid = |opened: &Value| opened["business_pid"].as_u64().unwrap() as u32;
    assert!(
        alive(pid(&missing_tick)),
        "a partial snapshot reaps nothing"
    );
    let applied = controller
        .request(
            "reconcile_owners",
            snapshot("fixture-root", 1, 1, 2, vec![]),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["reaped"], json!([missing_tick["instance"]]));
    assert!(gone(pid(&missing_tick)).await);
    assert!(alive(pid(&kept_tick)), "a listed Chat keeps its Tick");
    let stale = controller
        .request(
            "reconcile_owners",
            snapshot("fixture-root", 1, 0, 1, vec![]),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap_err();
    assert_eq!(
        stale.code, "Fenced",
        "an older or repeated revision never reaps"
    );
    assert!(alive(pid(&kept_tick)));
    let diagnostics = controller
        .request("diagnostics", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert!(
        diagnostics["reaped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reaped| reaped["reason"] == "orphaned"),
        "{diagnostics}"
    );
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ended_owner_cannot_open_again_until_a_later_revision() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let chat = uuid::Uuid::new_v4();
    let other = open_for(&fixture, &mut controller, uuid::Uuid::new_v4(), 0)
        .await
        .unwrap();
    let opened = open_for(&fixture, &mut controller, chat, 2).await.unwrap();
    let owner = json!({"root":"fixture-root","chat":chat});
    let operation = uuid::Uuid::new_v4();
    let end = json!({"owner":owner,"revision":3,"operation":operation});
    let ended = controller
        .request("end_owner", end.clone(), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(ended["state"], "owner_ended");
    assert_eq!(ended["instances"], json!([opened["instance"]]));
    assert!(gone(opened["business_pid"].as_u64().unwrap() as u32).await);
    assert!(
        alive(other["business_pid"].as_u64().unwrap() as u32),
        "other owners keep theirs"
    );
    let repeated = controller
        .request("end_owner", end, CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(repeated, ended, "a repeated end answers from its receipt");
    let late = open_for(&fixture, &mut controller, chat, 3)
        .await
        .unwrap_err();
    assert_eq!(
        late.code, "OwnerEnded",
        "a late open cannot revive the ended owner"
    );
    assert!(!late.dispatched);
    open_for(&fixture, &mut controller, chat, 4)
        .await
        .expect("a later revision of that Chat opens again");
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
            .request("read", json!({"activity":"explicit"}), CONTROL_TIMEOUT)
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
binding = json.loads(os.environ['TICKLOOP_TICKHOST_LAUNCH'])
def connect():
    channel = socket.socket(socket.AF_UNIX)
    channel.connect(binding['terminal']['endpoint'])
    stream = channel.makefile('rw')
    stream.write(json.dumps({'channel':'business','major':int(sys.argv[2]),'minor':0,'required':['launch'],'role':{'kind':'launcher','operation':binding['operation'],'launch_token':binding['launch_token']}}) + '\n')
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
                BUSINESS_MAJOR.to_string(),
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
                ("TICKLOOP_TICKHOST_LAUNCH", &binding),
                herdr[0],
                herdr[1],
                herdr[2],
            ],
            "cannot also carry a Herdr Pane context",
        ),
        (
            &[("TICKLOOP_TICKHOST_LAUNCH", "{not json")],
            "TICKLOOP_TICKHOST_LAUNCH is invalid",
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
            .env_remove("TICKLOOP_TICKHOST_LAUNCH")
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
            "owner_revision": 0,
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
        processes.push(tickhost::process::sample(business).unwrap().parent);
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
    let (terminal, slave) = tickhost::pty::Pty::open(Default::default()).unwrap();
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
        .env("TICKLOOP_TICKHOST_LAUNCH", binding.to_string())
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
