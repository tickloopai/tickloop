#![cfg(unix)]
//! Real `tickhost launch` launchers under the Tickhost library: hidden PTYs, the hosted launch contract,
//! one business process shared by protocol calls, its screen and its attached views, and process
//! cleanup.

use std::{path::Path, time::Duration};

use serde_json::{Value, json};
use tickhost::{host, paths::HostPaths};
use tickhost_connect::hosted::{
    ATTACH_INPUT_BYTES, ATTACH_REQUEST_FRAME_BYTES, BUSINESS_FRAME_BYTES, BUSINESS_MAJOR,
    BUSINESS_MINOR, CALL_TIMEOUT, CAPABILITY_DATA, CAPABILITY_LIFECYCLE, CONTROL_TIMEOUT,
    HostConnection, HostOccupancy, ManagementConnection, Role, read_frame, write_frame,
};
use tokio::{
    io::BufReader,
    net::unix::{OwnedReadHalf, OwnedWriteHalf},
};

/// An extension Tick whose screen shows its value, terminal size and the last line typed on its
/// terminal, so protocol calls, attached input and resizes are all visible in one process. Its
/// reads tell its terminal size as well.
const COUNTER: &str = r#"
import json, os, signal, sys, threading, time
LARGE_PAYLOAD = 8 * 1024 * 1024 - 4096
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
        result = {'protocol':3,'revision':'1','kind':'example.counter','title':'Counter','description':'Counter','recover_with':'current','tools':[{'name':'increment','description':'Increment','input_schema':{'type':'object'},'mutates':True},{'name':'current','description':'Read','input_schema':{'type':'object'},'mutates':False},{'name':'flood','description':'Print many lines','input_schema':{'type':'object'},'mutates':False},{'name':'slow','description':'Answer after 16 seconds','input_schema':{'type':'object'},'mutates':False},{'name':'large','description':'Answer with almost 8 MiB','input_schema':{'type':'object'},'mutates':False},{'name':'oversized_update','description':'Send an update beyond the limit','input_schema':{'type':'object'},'mutates':False},{'name':'jam','description':'Fail to run','input_schema':{'type':'object'},'mutates':True}]}
    elif method == 'tick/read':
        size = os.get_terminal_size(sys.stderr.fileno())
        result = {'seq':value,'at':'2026-10-07T00:00:00Z','data':{'value':value,'size':f'{size.columns}x{size.lines}','note':open('note.txt').read(),'sees_launch_token':'TICKLOOP_TICKHOST_LAUNCH' in os.environ,'argument_bytes':sum(len(argument) for argument in sys.argv[1:])}}
    elif method == 'tick/call':
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
            print(json.dumps({'jsonrpc':'2.0','method':'tick/update','params':{'seq':value + 1,'at':'2026-10-07T00:00:01Z','data':{'blob':'x' * (600 * 1024)}}}), flush=True)
        elif request['params']['tool'] == 'large':
            result = {'value':value,'payload':'x' * LARGE_PAYLOAD}
            print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
            continue
        elif request['params']['tool'] in ('jam', 'missing'):
            error = {'code':-32000,'message':'the counter is jammed'} if request['params']['tool'] == 'jam' else {'code':-32602,'message':'unknown Tick Tool'}
            print(json.dumps({'jsonrpc':'2.0','id':request['id'],'error':error}), flush=True)
            continue
        result = {'value':value}
    else:
        raise RuntimeError(method)
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

struct Fixture {
    server: tickhost::in_process::InProcessHost,
    directory: tempfile::TempDir,
    paths: HostPaths,
    key: String,
    daemon_epoch: uuid::Uuid,
    owner: Value,
}

impl Fixture {
    async fn start() -> Self {
        Self::with_idle_timeout(host::DEFAULT_IDLE_TIMEOUT).await
    }

    async fn with_idle_timeout(idle_timeout: Duration) -> Self {
        let directory = crate::runtime_directory("th");
        let paths = HostPaths {
            runtime: directory.path().join("h"),
        };
        let key = host::random_token();
        let server = tickhost::in_process::InProcessHost::start(
            paths.clone(),
            host::HostConfig {
                controller_key: key.clone(),
                idle_timeout,
            },
        )
        .unwrap();
        let fixture = Self {
            directory,
            paths,
            key,
            daemon_epoch: uuid::Uuid::new_v4(),
            owner: json!({"root":"fixture-root","chat":uuid::Uuid::new_v4()}),
            server,
        };
        // The socket file appears when the host binds, before it listens; only an accepted
        // connection shows the host is serving.
        let deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
        while fixture.connect_controller().await.is_err() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the host never accepted a controller"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        fixture
    }

    async fn connect_controller(
        &self,
    ) -> Result<HostConnection, tickhost_connect::host_error::HostError> {
        HostConnection::connect(
            &self.paths.socket(),
            Role::Controller {
                controller_key: self.key.clone(),
                daemon_epoch: self.daemon_epoch,
            },
            &[CAPABILITY_LIFECYCLE],
        )
        .await
    }

    async fn controller(&self) -> HostConnection {
        self.connect_controller().await.unwrap()
    }

    async fn open(
        &self,
        controller: &mut HostConnection,
        command: Vec<String>,
        cwd: &Path,
    ) -> Result<Value, tickhost_connect::host_error::HostError> {
        controller
            .request(
                "open",
                json!({
                    "operation": uuid::Uuid::new_v4(),
                    "owner": self.owner,
                    "owner_revision": 0,
                    "adapter": "protocol",
                    "command": command,
                    "cwd": cwd,
                    "environment": std::env::vars().collect::<Vec<_>>(),
                    "title": null,
                }),
                CONTROL_TIMEOUT + Duration::from_secs(2),
            )
            .await
    }

    /// Opens `command` as a terminal Tick in the fixture directory.
    async fn open_terminal(&self, controller: &mut HostConnection, command: &[&str]) -> Value {
        controller
            .request(
                "open",
                json!({
                    "operation": uuid::Uuid::new_v4(),
                    "owner": self.owner,
                    "owner_revision": 0,
                    "adapter": "terminal",
                    "command": command,
                    "cwd": self.directory.path(),
                    "environment": std::env::vars().collect::<Vec<_>>(),
                    "title": command.join(" "),
                }),
                CONTROL_TIMEOUT,
            )
            .await
            .unwrap()
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
                env!("CARGO_BIN_EXE_tickhost").into(),
                tickhost::cli::LAUNCH_COMMAND.into(),
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

/// The terminal size the counter `reader` reads reports, as `<columns>x<rows>`.
async fn terminal_size(reader: &mut HostConnection) -> Value {
    reader
        .request("read", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap()["data"]["size"]
        .clone()
}

async fn send(requests: &mut OwnedWriteHalf, request: Value) {
    write_frame(requests, &request, ATTACH_REQUEST_FRAME_BYTES)
        .await
        .unwrap();
}

/// Paints the attached view's frames into a terminal model, at the screen size each frame names,
/// until it shows `needle`.
async fn view_shows(events: &mut BufReader<OwnedReadHalf>, view: &mut vt100::Parser, needle: &str) {
    let painted = tokio::time::timeout(Duration::from_secs(10), async {
        while !view.screen().contents().contains(needle) {
            let event = read_frame(events, BUSINESS_FRAME_BYTES)
                .await
                .unwrap()
                .expect("the view stays attached");
            assert_eq!(event["event"], "frame", "{event}");
            let size = |dimension: &str| event["size"][dimension].as_u64().unwrap() as u16;
            let (rows, columns) = (size("rows"), size("columns"));
            if view.screen().size() != (rows, columns) {
                view.screen_mut().set_size(rows, columns);
            }
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

/// The terminal size a hosted Tick starts with; a view of this size leaves it unchanged.
fn initial_size() -> Value {
    json!({"rows":30,"columns":120})
}

/// Attaches one more view of `size` to the Tick `opened` names.
async fn attach(fixture: &Fixture, opened: &Value, size: Value) -> HostConnection {
    let mut view = fixture.reader(opened).await;
    let attached = view
        .request("attach", json!({"size":size}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(attached["attached"], true);
    view
}

/// How many views the host lists for its only Tick.
async fn listed_views(controller: &mut HostConnection) -> Value {
    controller
        .request("list", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap()["items"][0]["views"]
        .clone()
}

/// Waits until the host lists `views` views for its only Tick.
async fn views_become(controller: &mut HostConnection, views: usize) {
    for _ in 0..400 {
        if listed_views(controller).await == views {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "the Tick never had {views} views: {}",
        listed_views(controller).await
    );
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
async fn protocol_calls_and_the_screen_belong_to_one_counter_process() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut reader = fixture.reader(&opened).await;
    let refused = reader
        .request("snapshot", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(
        refused.code, "MethodNotAllowed",
        "a hidden screen is seen through a view"
    );
    let (mut events, _requests) = attach(&fixture, &opened, initial_size()).await.into_parts();
    let mut view = vt100::Parser::new(30, 120, 0);
    view_shows(&mut events, &mut view, "Counter 0 in work").await;
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
            json!({"tool":"increment","args":{},"revision":"1"}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(called["value"], 1);
    view_shows(&mut events, &mut view, "Counter 1 in work").await;
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

/// A Tick that refuses a call says it did not run it; one whose tool fails says it ran. Neither
/// leaves the outcome unknown.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_call_was_not_dispatched_and_a_failed_one_is_a_known_failure() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut reader = fixture.reader(&opened).await;
    let call = |tool: &str| json!({"tool":tool,"args":{},"revision":"1"});
    let refused = reader
        .request("call", call("missing"), CONTROL_TIMEOUT)
        .await
        .unwrap_err();
    assert!(!refused.dispatched, "{refused:?}");
    assert!(refused.message.contains("unknown Tick Tool"), "{refused:?}");
    let failed = reader
        .request("call", call("jam"), CONTROL_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(
        failed.code,
        tickhost_connect::host_error::TOOL_FAILED,
        "{failed:?}"
    );
    assert!(failed.message.contains("jammed"), "{failed:?}");
    let called = reader
        .request("call", call("increment"), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(called["value"], 1, "the Tick keeps serving");
    shutdown(&fixture).await;
}

/// Reads a terminal Tick until `ready` accepts what it shows.
async fn terminal_read_until(reader: &mut HostConnection, ready: impl Fn(&Value) -> bool) -> Value {
    let mut read = Value::Null;
    for _ in 0..400 {
        read = reader
            .request("read", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        if ready(&read) {
            return read;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the terminal Tick never showed what was expected: {read}");
}

/// A program that does not speak the Tick protocol runs directly on its terminal and is Live as
/// soon as it starts: it describes what it runs, reads as its screen text, and is written to
/// with input. Its exit is recorded with the screen it left, which stays readable and watched
/// until the Tick is closed.
#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_program_takes_input_shows_it_and_keeps_its_exit_code() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let opened = fixture.open_terminal(&mut controller, &["/bin/cat"]).await;
    assert_eq!(opened["adapter"], "terminal");
    assert!(opened["instance"].as_str().unwrap().starts_with("hosted:"));
    let listed = controller
        .request("list", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(listed["items"][0]["adapter"], "terminal");
    assert_eq!(listed["items"][0]["state"], "running");
    let mut reader = fixture.reader(&opened).await;
    let described = reader
        .request("describe", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(described["command"], json!(["/bin/cat"]));
    assert_eq!(described["cwd"], json!(fixture.directory.path()));
    assert_eq!(described["title"], "/bin/cat");
    let started = terminal_read_until(&mut reader, |_| true).await;
    assert_eq!(started["exit"], Value::Null);
    let mut watcher = fixture.reader(&opened).await;
    let watched = watcher
        .request("watch", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(watched["read"]["exit"], Value::Null, "{watched}");

    let refused = reader
        .request(
            "call",
            json!({"tool":"anything","args":{},"revision":"1"}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, "UnsupportedCapability");
    let written = reader
        .request(
            "input",
            json!({"data": b"hello tickhost\r".to_vec()}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(written["written"], 15);
    // The terminal echoes the line and cat prints it back.
    let echoed = terminal_read_until(&mut reader, |read| {
        read["text"]
            .as_str()
            .unwrap()
            .matches("hello tickhost")
            .count()
            == 2
    })
    .await;
    assert!(echoed["seq"].as_u64() > started["seq"].as_u64(), "{echoed}");
    assert_eq!(echoed["exit"], Value::Null);

    // Ctrl-D at the start of a line ends cat's input, and cat exits.
    reader
        .request("input", json!({"data":[4]}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    let exited = terminal_read_until(&mut reader, |read| read["exit"] != Value::Null).await;
    assert_eq!(exited["exit"], json!({"code":0,"signal":null}));
    assert!(exited["text"].as_str().unwrap().contains("hello tickhost"));
    assert!(exited["seq"].as_u64() > echoed["seq"].as_u64());
    let refused = reader
        .request("input", json!({"data": b"late".to_vec()}), CONTROL_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(refused.code, "ProgramExited");
    // A view still shows the last screen, at its own size, and its keys go nowhere.
    let (mut events, mut requests) = attach(&fixture, &opened, json!({"rows":10,"columns":40}))
        .await
        .into_parts();
    send(
        &mut requests,
        json!({"method":"input","params":{"data":[120]}}),
    )
    .await;
    let mut shown = vt100::Parser::new(10, 40, 0);
    view_shows(&mut events, &mut shown, "hello tickhost").await;
    let listed = controller
        .request("list", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(
        listed["items"][0]["instance"], opened["instance"],
        "an exited terminal Tick stays until it is closed"
    );
    assert_eq!(listed["items"][0]["state"], "exited");
    assert_eq!(listed["items"][0]["exit"], json!({"code":0,"signal":null}));
    let refused = reader
        .request("resources", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(
        refused.code, "ProgramExited",
        "an exited program has no resources to sample"
    );

    let failing = fixture
        .open_terminal(&mut controller, &["/bin/sh", "-c", "echo failing; exit 7"])
        .await;
    let mut failing_reader = fixture.reader(&failing).await;
    let failed = terminal_read_until(&mut failing_reader, |read| read["exit"] != Value::Null).await;
    assert_eq!(failed["exit"], json!({"code":7,"signal":null}));
    assert_eq!(failed["text"], "failing");

    controller
        .request(
            "close",
            json!({"operation":uuid::Uuid::new_v4(),"owner":fixture.owner,"instance":opened["instance"]}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    let mut pushed = Vec::new();
    let ended = loop {
        let event = tokio::time::timeout(
            Duration::from_secs(5),
            watcher.next_event_within(BUSINESS_FRAME_BYTES),
        )
        .await
        .expect("the watch ends with its Tick")
        .unwrap();
        match event {
            Some(event) if event["event"] == "gone" => break event,
            Some(event) => pushed.push(event),
            None => panic!("the watch closed before its Tick's end: {pushed:?}"),
        }
    };
    assert_eq!(ended, json!({"event":"gone"}));
    assert_eq!(
        pushed,
        [
            json!({"event":"changed"}),
            json!({"event":"exited","exit":{"code":0,"signal":null}}),
        ],
        "the watch was told once that the screen changed, then how the program ended"
    );
    shutdown(&fixture).await;
}

/// The next frame on a terminal Tick's watch, within five seconds.
async fn next_watch_frame(events: &mut BufReader<OwnedReadHalf>) -> Value {
    tokio::time::timeout(
        Duration::from_secs(5),
        read_frame(events, BUSINESS_FRAME_BYTES),
    )
    .await
    .expect("the watch pushes or answers in time")
    .unwrap()
    .expect("the watch is still open")
}

/// A terminal Tick's watch says once that the screen text changed after the screen it last read,
/// and nothing more until it reads again through the watch, which arms it against that screen.
/// The program's exit is pushed with how it ended, and no change follows it.
#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_watch_tells_the_first_change_after_each_read_and_the_exit() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let opened = fixture
        .open_terminal(
            &mut controller,
            &[
                "/bin/sh",
                "-c",
                "while read line; do echo \"got $line\"; [ \"$line\" = quit ] && exit 4; done",
            ],
        )
        .await;
    let mut typist = fixture.reader(&opened).await;
    let (mut events, mut requests) = fixture.reader(&opened).await.into_parts();
    send(&mut requests, json!({"id":1,"method":"watch","params":{}})).await;
    let registered = next_watch_frame(&mut events).await;
    assert_eq!(registered["id"], 1, "{registered}");
    assert_eq!(registered["result"]["read"]["exit"], Value::Null);
    let shows =
        |needle: &'static str| move |read: &Value| read["text"].as_str().unwrap().contains(needle);

    typist
        .request("input", json!({"data": b"one\r".to_vec()}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(
        next_watch_frame(&mut events).await,
        json!({"event":"changed"})
    );
    typist
        .request("input", json!({"data": b"two\r".to_vec()}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    terminal_read_until(&mut typist, shows("got two")).await;
    send(&mut requests, json!({"id":2,"method":"read","params":{}})).await;
    let read = next_watch_frame(&mut events).await;
    assert_eq!(
        read["id"], 2,
        "a later change before the watch read again is not pushed: {read}"
    );
    assert!(read["result"]["text"].as_str().unwrap().contains("got two"));
    assert_eq!(read["result"]["exit"], Value::Null);

    let long_line = format!("three-{}\r", "x".repeat(60));
    typist
        .request(
            "input",
            json!({"data": long_line.as_bytes().to_vec()}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(
        next_watch_frame(&mut events).await,
        json!({"event":"changed"}),
        "reading through the watch armed it again"
    );
    typist
        .request(
            "input",
            json!({"data": b"quit\r".to_vec()}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(
        next_watch_frame(&mut events).await,
        json!({"event":"exited","exit":{"code":4,"signal":null}})
    );
    send(&mut requests, json!({"id":3,"method":"read","params":{}})).await;
    let last = next_watch_frame(&mut events).await;
    assert_eq!(last["id"], 3, "{last}");
    assert_eq!(last["result"]["exit"], json!({"code":4,"signal":null}));
    assert!(
        last["result"]["text"]
            .as_str()
            .unwrap()
            .contains("got quit")
    );

    // A narrower view cuts the long line, which changes the screen text after the exit.
    let _view = attach(&fixture, &opened, json!({"rows":8,"columns":30})).await;
    let narrowed = terminal_read_until(&mut typist, |_| true).await;
    assert_ne!(narrowed["text"], last["result"]["text"]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    controller
        .request(
            "close",
            json!({"operation":uuid::Uuid::new_v4(),"owner":fixture.owner,"instance":opened["instance"]}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(
        next_watch_frame(&mut events).await,
        json!({"event":"gone"}),
        "after the exit only the Tick's end is pushed, though a view resized its screen"
    );
    shutdown(&fixture).await;
}

/// A terminal Tick's exit is recorded once its program ended and its terminal closed, so the
/// screen it records is complete; whatever the program left running in its process group ends
/// when the Tick is closed.
#[tokio::test(flavor = "multi_thread")]
async fn closing_an_exited_terminal_tick_ends_what_its_program_left_running() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let pid_file = fixture.directory.path().join("background.pid");
    let script = format!(
        "(trap '' HUP; exec sleep 600) < /dev/null > /dev/null 2>&1 & echo $! > {}; seq 1 50; exit 3",
        pid_file.display()
    );
    let opened = fixture
        .open_terminal(&mut controller, &["/bin/sh", "-c", &script])
        .await;
    let mut reader = fixture.reader(&opened).await;
    let exited = terminal_read_until(&mut reader, |read| read["exit"] != Value::Null).await;
    assert_eq!(exited["exit"], json!({"code":3,"signal":null}));
    assert!(
        exited["text"].as_str().unwrap().ends_with("50"),
        "the exit comes with everything the program wrote: {exited}"
    );
    let background: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        alive(background),
        "the program left a process running that outlives the terminal's hangup"
    );
    controller
        .request(
            "close",
            json!({"operation":uuid::Uuid::new_v4(),"owner":fixture.owner,"instance":opened["instance"]}),
            CONTROL_TIMEOUT + host::TERMINATE_GRACE,
        )
        .await
        .unwrap();
    assert!(
        gone(background).await,
        "closing the Tick ends its program's process group"
    );
    tokio::time::timeout(Duration::from_secs(5), fixture.server)
        .await
        .expect("the host exits once its last Tick ended")
        .unwrap()
        .unwrap();
    assert!(
        !fixture.paths.socket().exists(),
        "the emptied host removes its endpoint"
    );
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
            json!({"tool":"increment","args":{},"revision":"1"}),
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
async fn idle_shutdown_rechecks_work_started_after_the_management_hello() {
    let fixture = Fixture::start().await;
    let management = ManagementConnection::connect(&fixture.paths.socket(), &fixture.key)
        .await
        .unwrap();
    let mut controller = fixture.controller().await;
    let sleep = which("sleep");
    let opened = fixture
        .open_terminal(&mut controller, &[&sleep, "30"])
        .await;
    assert!(
        !management
            .shutdown_idle_host(1, CONTROL_TIMEOUT)
            .await
            .unwrap()
    );
    let listed = controller
        .request("list", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(listed["items"][0]["instance"], opened["instance"]);
    shutdown(&fixture).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_shutdown_stops_an_empty_host_without_ending_work() {
    let fixture = Fixture::start().await;
    let management = ManagementConnection::connect(&fixture.paths.socket(), &fixture.key)
        .await
        .unwrap();
    assert!(
        management
            .shutdown_idle_host(0, CONTROL_TIMEOUT)
            .await
            .unwrap()
    );
    tokio::time::timeout(Duration::from_secs(5), fixture.server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!fixture.paths.socket().exists());
}

/// The frozen management hello tells a daemon which build the host is and what stopping it would
/// end, so a daemon of another build replaces only a host that runs nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_management_hello_names_the_build_and_what_stopping_the_host_ends() {
    let fixture = Fixture::start().await;
    let describe = || async {
        ManagementConnection::connect(&fixture.paths.socket(), &fixture.key)
            .await
            .unwrap()
            .hello()
            .host
            .clone()
            .expect("a host of this build describes itself")
    };
    let empty = describe().await;
    assert_eq!(empty.build, host::BUILD_IDENTITY);
    assert_eq!(
        (empty.business_major, empty.business_minor),
        (BUSINESS_MAJOR, BUSINESS_MINOR)
    );
    assert!(empty.executable.is_some());
    assert_eq!(
        empty.occupancy,
        HostOccupancy {
            pending: Some(0),
            views: Some(0),
            ..HostOccupancy::default()
        }
    );

    let mut controller = fixture.controller().await;
    let sleep = which("sleep");
    let opened = fixture
        .open_terminal(&mut controller, &[&sleep, "30"])
        .await;
    let occupied = describe().await.occupancy;
    assert_eq!(occupied.ticks, [format!("{sleep} 30")], "listed by title");
    assert_eq!(occupied.shells, 0);
    assert_eq!(occupied.pending, Some(0), "the open was answered");
    assert_eq!(occupied.connections, 1, "the controller's connection");
    assert_eq!(occupied.views, Some(0), "a connection is not a view");

    let _view = attach(&fixture, &opened, initial_size()).await;
    let viewed = describe().await.occupancy;
    assert_eq!(viewed.connections, 2);
    assert_eq!(
        viewed.views,
        Some(1),
        "only the attached view counts as one"
    );
    let (counter, work) = fixture.counter();
    fixture.open(&mut controller, counter, &work).await.unwrap();
    let protocol = describe().await.occupancy;
    assert_eq!(
        protocol.connections, 3,
        "the counter's launcher connects too"
    );
    assert_eq!(
        protocol.views,
        Some(1),
        "a protocol Tick's launcher is no view"
    );

    ManagementConnection::connect(&fixture.paths.socket(), &fixture.key)
        .await
        .unwrap()
        .shutdown_host(CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert!(!fixture.paths.socket().exists(), "the host stopped");
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
                "adapter": "protocol",
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
        .request("read", json!({}), CONTROL_TIMEOUT)
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
        .request("read", json!({}), CONTROL_TIMEOUT)
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
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_tickhost"))
        .args([tickhost::cli::LAUNCH_COMMAND, "--", "/usr/bin/touch"])
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

/// Several views attach to one Tick at once and see the same screen, each told its size; keys
/// from any of them reach the one business process. The terminal takes the size of the view that
/// typed last, or of the newest view while none typed, so a resize of another view changes only
/// that view. A view that leaves, even the one the terminal follows, leaves the others and the
/// Tick running.
#[tokio::test(flavor = "multi_thread")]
async fn several_views_share_one_terminal_sized_by_the_view_that_typed_last() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut reader = fixture.reader(&opened).await;
    let (mut first_events, mut first_requests) =
        attach(&fixture, &opened, json!({"rows":20,"columns":80}))
            .await
            .into_parts();
    let mut first = vt100::Parser::new(20, 80, 0);
    view_shows(&mut first_events, &mut first, "Counter 0 in work 80x20").await;
    let (mut second_events, mut second_requests) =
        attach(&fixture, &opened, json!({"rows":24,"columns":100}))
            .await
            .into_parts();
    let mut second = vt100::Parser::new(24, 100, 0);
    view_shows(&mut second_events, &mut second, "Counter 0 in work 100x24").await;
    view_shows(&mut first_events, &mut first, "Counter 0 in work 100x24").await;
    assert_eq!(
        first.screen().size(),
        (24, 100),
        "every view is told the screen size"
    );
    assert_eq!(listed_views(&mut controller).await, 2);

    send(
        &mut first_requests,
        json!({"method":"input","params":{"data":b"hello\n".to_vec()}}),
    )
    .await;
    view_shows(&mut first_events, &mut first, "80x20 typed hello").await;
    view_shows(&mut second_events, &mut second, "80x20 typed hello").await;
    send(
        &mut second_requests,
        json!({"method":"input","params":{"data":b"world\n".to_vec()}}),
    )
    .await;
    view_shows(&mut second_events, &mut second, "100x24 typed world").await;
    view_shows(&mut first_events, &mut first, "100x24 typed world").await;

    send(
        &mut first_requests,
        json!({"method":"resize","params":{"size":{"rows":30,"columns":90}}}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let called = reader
        .request(
            "call",
            json!({"tool":"increment","args":{},"revision":"1"}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(called["value"], 1);
    let unchanged = "Counter 1 in work 100x24 typed world";
    view_shows(&mut second_events, &mut second, unchanged).await;
    view_shows(&mut first_events, &mut first, unchanged).await;
    send(
        &mut second_requests,
        json!({"method":"resize","params":{"size":{"rows":26,"columns":110}}}),
    )
    .await;
    view_shows(&mut second_events, &mut second, "110x26 typed world").await;
    view_shows(&mut first_events, &mut first, "110x26 typed world").await;
    send(
        &mut first_requests,
        json!({"method":"input","params":{"data":b"again\n".to_vec()}}),
    )
    .await;
    view_shows(&mut first_events, &mut first, "90x30 typed again").await;
    view_shows(&mut second_events, &mut second, "90x30 typed again").await;

    send(&mut first_requests, json!({"method":"detach"})).await;
    assert_eq!(
        view_end(&mut first_events).await,
        None,
        "detach ends the view"
    );
    view_shows(&mut second_events, &mut second, "110x26 typed again").await;
    assert_eq!(listed_views(&mut controller).await, 1);
    let called = reader
        .request(
            "call",
            json!({"tool":"increment","args":{},"revision":"1"}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(called["value"], 2);
    view_shows(&mut second_events, &mut second, "Counter 2 in work 110x26").await;

    send(
        &mut second_requests,
        json!({"method":"input","params":{"data":vec![b'x'; ATTACH_INPUT_BYTES + 1]}}),
    )
    .await;
    let refused = view_end(&mut second_events).await.unwrap();
    assert_eq!(refused["error"]["code"], "InputTooLarge", "{refused}");
    views_become(&mut controller, 0).await;
    let listed = controller
        .request("list", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(
        listed["items"][0]["business_pid"], opened["business_pid"],
        "views come and go around one business process"
    );
    shutdown(&fixture).await;
}

/// Paints the attached view's frames until it shows a screen of `rows` rows, the size the host
/// gives every frame after a resize.
async fn view_resized(events: &mut BufReader<OwnedReadHalf>, view: &mut vt100::Parser, rows: u16) {
    let resized = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = read_frame(events, BUSINESS_FRAME_BYTES)
                .await
                .unwrap()
                .expect("the view stays attached");
            assert_eq!(event["event"], "frame", "{event}");
            let size = |dimension: &str| event["size"][dimension].as_u64().unwrap() as u16;
            let (frame_rows, columns) = (size("rows"), size("columns"));
            view.screen_mut().set_size(frame_rows, columns);
            view.process(event["data"].as_str().unwrap().as_bytes());
            if frame_rows == rows {
                break;
            }
        }
    })
    .await;
    assert!(resized.is_ok(), "the view was never resized to {rows} rows");
}

/// A program that neither redraws nor even notices a resize keeps its newest output in view as
/// a view shrinks its terminal: the rows above scroll off the top, as on a real terminal.
#[tokio::test(flavor = "multi_thread")]
async fn shrinking_a_view_keeps_the_newest_output_of_a_scrolling_program_in_view() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let opened = fixture
        .open_terminal(
            &mut controller,
            &[
                "/bin/sh",
                "-c",
                "for line in $(seq 1 40); do printf 'log %02d\\n' \"$line\"; done; exec sleep 60",
            ],
        )
        .await;
    let (mut events, mut requests) = attach(&fixture, &opened, initial_size()).await.into_parts();
    let mut view = vt100::Parser::new(30, 120, 0);
    view_shows(&mut events, &mut view, "log 40").await;
    let newest = |first: usize| {
        (first..=40)
            .map(|line| format!("log {line:02}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    for (rows, first) in [(10, 32), (8, 34)] {
        send(
            &mut requests,
            json!({"method":"resize","params":{"size":{"rows":rows,"columns":120}}}),
        )
        .await;
        view_resized(&mut events, &mut view, rows).await;
        assert_eq!(
            view.screen().contents(),
            newest(first),
            "a {rows}-row view shows the newest lines over the cursor's empty row"
        );
        assert_eq!(view.screen().cursor_position(), (rows - 1, 0));
    }
    shutdown(&fixture).await;
}

/// Paints the attached view's frames until another event comes, and returns that event.
async fn view_signal(events: &mut BufReader<OwnedReadHalf>, view: &mut vt100::Parser) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = read_frame(events, BUSINESS_FRAME_BYTES)
                .await
                .unwrap()
                .expect("the view stays attached");
            if event["event"] != "frame" {
                return event;
            }
            view.process(event["data"].as_str().unwrap().as_bytes());
        }
    })
    .await
    .expect("the view was never told a key-bubbling signal")
}

/// A Tick's key-bubbling sequences never reach the screen. Every view learns whether the Tick
/// bubbles keys, when it attaches and when that changes; each bubbled key goes only to the view
/// that typed last, and unknown names to none.
#[tokio::test(flavor = "multi_thread")]
async fn bubbled_keys_reach_the_view_that_typed_last_and_never_the_screen() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let opened = fixture
        .open_terminal(
            &mut controller,
            &[
                "/bin/sh",
                "-c",
                r#"stty -echo; printf 'ready\033]7777;tickloop;bubble=on\007'
while read line; do
  if [ "$line" = off ]; then printf '\033]7777;tickloop;bubble=off\033\\'
  else printf '\033]7777;tickloop;key=%s\033\\' "$line"; fi
  printf ' got-%s' "$line"
done"#,
            ],
        )
        .await;
    let (mut first_events, mut first_requests) =
        attach(&fixture, &opened, initial_size()).await.into_parts();
    let mut first = vt100::Parser::new(30, 120, 0);
    assert_eq!(
        view_signal(&mut first_events, &mut first).await,
        json!({"event":"bubbling","enabled":true})
    );
    let (mut second_events, mut second_requests) =
        attach(&fixture, &opened, initial_size()).await.into_parts();
    let mut second = vt100::Parser::new(30, 120, 0);
    assert_eq!(
        view_signal(&mut second_events, &mut second).await,
        json!({"event":"bubbling","enabled":true}),
        "a view that attaches later is told at once"
    );

    send(
        &mut first_requests,
        json!({"method":"input","params":{"data":b"down\n".to_vec()}}),
    )
    .await;
    assert_eq!(
        view_signal(&mut first_events, &mut first).await,
        json!({"event":"bubble","key":"down"})
    );
    view_shows(&mut second_events, &mut second, "got-down").await;
    send(
        &mut second_requests,
        json!({"method":"input","params":{"data":b"enter\nesc\n".to_vec()}}),
    )
    .await;
    assert_eq!(
        view_signal(&mut second_events, &mut second).await,
        json!({"event":"bubble","key":"esc"}),
        "an unknown name is not bubbled"
    );
    view_shows(&mut first_events, &mut first, "got-esc").await;
    assert_eq!(
        first.screen().contents(),
        "ready got-down got-enter got-esc",
        "no sequence reaches the screen"
    );

    send(
        &mut first_requests,
        json!({"method":"input","params":{"data":b"off\nup\n".to_vec()}}),
    )
    .await;
    for (events, view) in [
        (&mut first_events, &mut first),
        (&mut second_events, &mut second),
    ] {
        assert_eq!(
            view_signal(events, view).await,
            json!({"event":"bubbling","enabled":false})
        );
        view_shows(events, view, "got-up").await;
    }
    shutdown(&fixture).await;
}

/// A Tick that sets DEC private mode 1004 hears whether any of its views has the keyboard: once
/// when it asks, then at each change of whether any has it, and nothing more once it resets the
/// mode.
#[tokio::test(flavor = "multi_thread")]
async fn a_tick_that_wants_focus_reports_hears_whether_any_view_has_the_keyboard() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let script = fixture.directory.path().join("focus.py");
    std::fs::write(
        &script,
        "import os, tty\ntty.setraw(0)\nos.write(1, b'ready\\x1b[?1004h')\nwhile True:\n    data = os.read(0, 64)\n    if not data:\n        break\n    if data == b'x':\n        os.write(1, b'\\x1b[?2004;1004l')\n    os.write(1, (' ' + data.hex()).encode())\n",
    )
    .unwrap();
    let python = which("python3");
    let opened = fixture
        .open_terminal(
            &mut controller,
            &[&python, "-u", &script.to_string_lossy()],
        )
        .await;
    let (mut first_events, mut first_requests) =
        attach(&fixture, &opened, initial_size()).await.into_parts();
    let mut first = vt100::Parser::new(30, 120, 0);
    let focus = |focused: bool| json!({"method":"focus","params":{"focused":focused}});
    let input = |data: &[u8]| json!({"method":"input","params":{"data":data.to_vec()}});
    assert_eq!(
        view_signal(&mut first_events, &mut first).await,
        json!({"event":"focus_reports","enabled":true})
    );
    view_shows(&mut first_events, &mut first, "ready 1b5b4f").await;
    send(&mut first_requests, focus(true)).await;
    view_shows(&mut first_events, &mut first, "1b5b4f 1b5b49").await;

    let (mut second_events, mut second_requests) =
        attach(&fixture, &opened, initial_size()).await.into_parts();
    let mut second = vt100::Parser::new(30, 120, 0);
    assert_eq!(
        view_signal(&mut second_events, &mut second).await,
        json!({"event":"focus_reports","enabled":true}),
        "a view that attaches later is told at once"
    );
    send(&mut second_requests, focus(true)).await;
    send(&mut first_requests, focus(false)).await;
    send(&mut second_requests, json!({"method":"detach"})).await;
    view_shows(&mut first_events, &mut first, "1b5b49 1b5b4f").await;

    send(&mut first_requests, input(b"x")).await;
    assert_eq!(
        view_signal(&mut first_events, &mut first).await,
        json!({"event":"focus_reports","enabled":false})
    );
    send(&mut first_requests, focus(true)).await;
    send(&mut first_requests, input(b"y")).await;
    view_shows(&mut first_events, &mut first, "1b5b4f 78 79").await;
    assert_eq!(
        first.screen().contents(),
        "ready 1b5b4f 1b5b49 1b5b4f 78 79",
        "no report once the Tick reset the mode, and none of the sequences on its screen"
    );
    shutdown(&fixture).await;
}

/// The owner lets a view attach to its Tick's terminal without handing out the Tick's data
/// token: every grant admits its own view once, only to attach and only for the Tick it names,
/// and a newer grant leaves earlier ones usable, so the views of several grants are attached at
/// once.
#[tokio::test(flavor = "multi_thread")]
async fn every_view_grant_attaches_its_own_view_once_and_allows_nothing_else() {
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
    ) -> Result<HostConnection, tickhost_connect::host_error::HostError> {
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

    let earlier = grant(&mut controller, &fixture.owner, &instance).await;
    let later = grant(&mut controller, &fixture.owner, &instance).await;
    let mut views = Vec::new();
    for granted in [&later, &earlier] {
        let mut view = viewer(&fixture, &instance, granted).await.unwrap();
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
        let (mut events, requests) = view.into_parts();
        let mut screen = vt100::Parser::new(20, 80, 0);
        view_shows(&mut events, &mut screen, "Counter 0 in work 80x20").await;
        views.push((events, requests));
    }
    assert_eq!(listed_views(&mut controller).await, 2);
    for (mut events, mut requests) in views {
        send(&mut requests, json!({"method":"detach"})).await;
        assert_eq!(view_end(&mut events).await, None, "detach ends the view");
    }
    for used in [&earlier, &later] {
        let reused = viewer(&fixture, &instance, used).await.err().unwrap();
        assert_eq!(reused.code, "Forbidden", "a grant works once");
    }
    shutdown(&fixture).await;
}

/// A view that stops reading loses its connection after two seconds; the other views and the Tick
/// keep running.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_view_is_dropped_after_two_seconds_and_the_rest_keep_running() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let stalled = attach(&fixture, &opened, initial_size()).await;
    let attached_at = std::time::Instant::now();
    let (mut events, _requests) = attach(&fixture, &opened, initial_size()).await.into_parts();
    let (shown, mut showing) = tokio::sync::mpsc::unbounded_channel();
    let painting = tokio::spawn(async move {
        let mut screen = vt100::Parser::new(30, 120, 0);
        while let Ok(Some(event)) = read_frame(&mut events, BUSINESS_FRAME_BYTES).await {
            screen.process(event["data"].as_str().unwrap_or_default().as_bytes());
            let _ = shown.send(screen.screen().contents());
        }
    });
    let mut reader = fixture.reader(&opened).await;
    tokio::time::timeout(CONTROL_TIMEOUT, async {
        while listed_views(&mut controller).await != 1 {
            reader
                .request(
                    "call",
                    json!({"tool":"flood","args":{},"revision":"1"}),
                    CALL_TIMEOUT,
                )
                .await
                .unwrap();
        }
    })
    .await
    .expect("a view that stops reading is dropped");
    assert!(attached_at.elapsed() >= host::STREAM_WRITE_TIMEOUT);
    let called = reader
        .request(
            "call",
            json!({"tool":"increment","args":{},"revision":"1"}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(called["value"], 1, "the Tick keeps running");
    tokio::time::timeout(CONTROL_TIMEOUT, async {
        while !showing
            .recv()
            .await
            .expect("the reading view stays attached")
            .contains("Counter 1 in work")
        {}
    })
    .await
    .expect("the reading view keeps showing the Tick");
    painting.abort();
    drop(stalled);
    shutdown(&fixture).await;
}

/// A resize outside 2–300 rows or 2–500 columns is refused: an attach asking for one, or naming
/// no size, gets an error, an attached view sending one ends with it, and the terminal keeps its
/// size while the Tick keeps running.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_resize_is_refused_and_leaves_the_terminal_and_the_tick_unchanged() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let opened = fixture.open(&mut controller, command, &work).await.unwrap();
    let mut reader = fixture.reader(&opened).await;
    assert_eq!(terminal_size(&mut reader).await, "120x30");
    let sizeless = fixture
        .reader(&opened)
        .await
        .request("attach", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(sizeless.code, "InvalidArguments", "a view names its size");
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

        let (mut events, mut requests) =
            attach(&fixture, &opened, initial_size()).await.into_parts();
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
        assert_eq!(terminal_size(&mut reader).await, "120x30", "{size}");
    }
    let called = reader
        .request(
            "call",
            json!({"tool":"increment","args":{},"revision":"1"}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(called["value"], 1, "the Tick keeps running");
    assert_eq!(terminal_size(&mut reader).await, "120x30");
    attach(&fixture, &opened, initial_size()).await;
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
        &json!({"id":1,"method":"call","params":{"tool":"large","args":{},"revision":"1"}}),
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
        .request(
            "call",
            json!({"tool":"current","args":{},"revision":"1"}),
            CONTROL_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(current["value"], 0, "the Tick keeps serving");
    shutdown(&fixture).await;
}

/// Closing a Tick whose launcher ignores SIGTERM takes the whole 3-second grace before SIGKILL;
/// the host keeps accepting connections meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn ending_a_stubborn_tick_never_pauses_new_connections() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let (command, work) = fixture.counter();
    let stubborn = fixture
        .open(&mut controller, command.clone(), &work)
        .await
        .unwrap();
    // A second Tick keeps the host from emptying during the test.
    fixture.open(&mut controller, command, &work).await.unwrap();
    let business = stubborn["business_pid"].as_u64().unwrap() as u32;
    let launcher = tickhost::process::sample(business).unwrap().parent;
    // A stopped launcher leaves SIGTERM pending, so only SIGKILL after the grace ends it.
    unsafe { libc::kill(launcher as libc::pid_t, libc::SIGSTOP) };
    let mut closer = fixture.controller().await;
    let close = json!({"operation":uuid::Uuid::new_v4(),"owner":fixture.owner,"instance":stubborn["instance"]});
    let closing = tokio::spawn(async move {
        closer
            .request("close", close, CONTROL_TIMEOUT + Duration::from_secs(5))
            .await
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut slowest = Duration::ZERO;
    while alive(launcher) {
        assert!(
            std::time::Instant::now() < deadline,
            "the closed Tick never ended"
        );
        let connecting = std::time::Instant::now();
        drop(fixture.controller().await);
        slowest = slowest.max(connecting.elapsed());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        slowest < Duration::from_secs(1),
        "a new connection waited {slowest:?} while a Tick was ending"
    );
    assert!(gone(business).await);
    closing.await.unwrap().unwrap();
    let diagnostics = controller
        .request("diagnostics", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    let reaped = diagnostics["reaped"].as_array().unwrap();
    assert_eq!(reaped.len(), 1, "{reaped:?}");
    assert_eq!(reaped[0]["reason"], "closed");
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
                "adapter": "protocol",
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
) -> Result<Value, tickhost_connect::host_error::HostError> {
    let (command, work) = fixture.counter();
    controller
        .request(
            "open",
            json!({
                "operation": uuid::Uuid::new_v4(),
                "owner": {"root": "fixture-root", "chat": chat},
                "owner_revision": owner_revision,
                "adapter": "protocol",
                "command": command,
                "cwd": work,
                "environment": std::env::vars().collect::<Vec<_>>(),
                "title": null,
            }),
            CONTROL_TIMEOUT + Duration::from_secs(2),
        )
        .await
}

/// With a 2-second idle timeout, no Tick idles out: one nobody uses, one only read, listed and
/// sampled, and one whose view detached all keep running past it. Closing them empties the host,
/// which then exits.
#[tokio::test(flavor = "multi_thread")]
async fn no_tick_idles_out_and_the_host_exits_once_they_are_closed() {
    let fixture = Fixture::with_idle_timeout(Duration::from_secs(2)).await;
    let mut controller = fixture.controller().await;
    let chat = uuid::Uuid::new_v4();
    let mut opened = Vec::new();
    for _ in 0..3 {
        opened.push(open_for(&fixture, &mut controller, chat, 0).await.unwrap());
    }
    let pid = |tick: &Value| tick["business_pid"].as_u64().unwrap() as u32;
    let [unused, polled, viewed] = <[Value; 3]>::try_from(opened).unwrap();
    drop(attach(&fixture, &viewed, initial_size()).await);
    let mut polled_reader = fixture.reader(&polled).await;
    for _ in 0..6 {
        for method in ["read", "resources"] {
            polled_reader
                .request(method, json!({}), CONTROL_TIMEOUT)
                .await
                .unwrap();
        }
        controller
            .request("list", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    for tick in [&unused, &polled, &viewed] {
        assert!(alive(pid(tick)), "past the idle timeout: {tick}");
    }
    let diagnostics = controller
        .request("diagnostics", json!({}), CONTROL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(diagnostics["reaped"], json!([]), "nothing was reaped");

    for tick in [&unused, &polled, &viewed] {
        controller
            .request(
                "close",
                json!({"operation":uuid::Uuid::new_v4(),"owner":{"root":"fixture-root","chat":chat},"instance":tick["instance"]}),
                CONTROL_TIMEOUT,
            )
            .await
            .unwrap();
        assert!(gone(pid(tick)).await, "closing ends it");
    }
    drop(controller);
    drop(polled_reader);
    let exited = tokio::time::timeout(Duration::from_secs(5), fixture.server)
        .await
        .expect("an emptied host exits")
        .unwrap();
    exited.unwrap();
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

/// A program that explains on the terminal why it cannot start keeps its whole explanation in
/// the launch failure. The launcher's own failure, written after it, is checked in
/// `tickhost_connect::launch`.
#[tokio::test(flavor = "multi_thread")]
async fn a_program_that_exits_during_startup_keeps_its_whole_reason_in_the_failure() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let reason = "tick-sample: not authorized; run `tick-sample auth`";
    let failed = fixture
        .open(
            &mut controller,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("printf '%s\\n' '{reason}' >&2; exit 3"),
            ],
            fixture.directory.path(),
        )
        .await
        .unwrap_err();
    assert_eq!(failed.code, "LaunchFailed", "{failed:?}");
    assert!(failed.message.contains(reason), "{failed:?}");
    shutdown(&fixture).await;
}

/// A launch failure says where a silent Tick stood instead of ending on an empty screen: one
/// that exited printing nothing, and one whose launcher ran it but that never answered.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_tick_that_fails_to_start_says_where_it_stood() {
    let fixture = Fixture::start().await;
    let mut controller = fixture.controller().await;
    let exited = fixture
        .open(
            &mut controller,
            vec!["/usr/bin/false".into()],
            fixture.directory.path(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (exited.code.as_str(), exited.message.as_str()),
        (
            "LaunchFailed",
            "the Tick exited during startup; it printed nothing"
        )
    );

    let silent = fixture
        .open(
            &mut controller,
            vec![
                env!("CARGO_BIN_EXE_tickhost").into(),
                tickhost::cli::LAUNCH_COMMAND.into(),
                "--".into(),
                "/bin/sleep".into(),
                "30".into(),
            ],
            fixture.directory.path(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (silent.code.as_str(), silent.message.as_str()),
        (
            "LaunchFailed",
            "the Tick did not become ready within 10s: it did not answer tick/describe and tick/read; it printed nothing"
        )
    );
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
            json!({"tool":"oversized_update","args":{},"revision":"1"}),
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
            json!({"tool":"increment","args":{},"revision":"1"}),
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
            json!({"tool":"increment","args":{},"revision":"1"}),
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
        .request(
            "call",
            json!({"tool":"large","args":{},"revision":"1"}),
            CALL_TIMEOUT,
        )
        .await
        .unwrap();
    let payload = answered["payload"].as_str().unwrap().len();
    assert!(payload > tick_protocol::MAX_TOOL_RESPONSE_BYTES - 8192);
    let current = reader
        .request(
            "call",
            json!({"tool":"current","args":{},"revision":"1"}),
            CONTROL_TIMEOUT,
        )
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

/// Only a launch the Tickhost started runs a program: a person running the launcher, even inside
/// a Herdr Pane, and an unreadable binding are refused before the program starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_launcher_without_a_readable_tickhost_launch_never_starts_a_tick() {
    let directory = crate::runtime_directory("thctx");
    let marker = directory.path().join("started");
    let cases: [(&[(&str, &str)], &str); 2] = [
        (
            &[
                ("HERDR_ENV", "1"),
                ("HERDR_SOCKET_PATH", "/nonexistent/herdr.sock"),
                ("HERDR_PANE_ID", "p1"),
            ],
            "only the Tickhost starts a launcher",
        ),
        (
            &[("TICKLOOP_TICKHOST_LAUNCH", "{not json")],
            "TICKLOOP_TICKHOST_LAUNCH is invalid",
        ),
    ];
    for (environment, expected) in cases {
        // Only the case's own context: never the Herdr session this test may run inside.
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tickhost"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("HERDR_") {
                command.env_remove(name);
            }
        }
        let output = command
            .args([tickhost::cli::LAUNCH_COMMAND, "--", "/usr/bin/touch"])
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
        .request(
            "call",
            json!({"tool":"slow","args":{},"revision":"1"}),
            CALL_TIMEOUT,
        )
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
    let padding = "x".repeat(tick_protocol::MAX_REQUEST_BYTES);
    let refused = reader
        .request(
            "call",
            json!({"tool":"increment","args":{"padding":padding},"revision":"1"}),
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
        .request(
            "call",
            json!({"tool":"current","args":{},"revision":"1"}),
            CONTROL_TIMEOUT,
        )
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
            "adapter": "protocol",
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
    use tickhost_connect::hosted::{MANAGEMENT_FRAME_BYTES, read_frame, write_frame};
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
    let directory = crate::runtime_directory("thbg");
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
            r#"set -m; "$TICKHOST" open -- /usr/bin/touch "$MARKER" & wait $!; echo $? > "$STATUS""#,
        ])
        .env("TICKHOST", env!("CARGO_BIN_EXE_tickhost"))
        .env("MARKER", &marker)
        .env("STATUS", &status)
        .env("TICKLOOP_TICKHOST_LAUNCH", binding.to_string())
        .env_remove("HERDR_ENV");
    slave.attach(&mut shell).unwrap();
    let mut child = shell.kill_on_drop(true).spawn().unwrap();
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
