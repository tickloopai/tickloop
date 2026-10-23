//! The host's frozen management channel, business negotiation and role authentication, served
//! by the real host library on a private socket.

use std::{path::PathBuf, time::Duration};

use serde_json::{Value, json};
use tickhost::{host, paths::HostPaths};
use tickhost_connect::hosted::{
    BUSINESS_FRAME_BYTES, CAPABILITY_LIFECYCLE, HostConnection, MANAGEMENT_FRAME_BYTES, Role,
    read_frame, write_frame,
};
use tokio::{io::BufReader, net::UnixStream};

struct Fixture {
    _directory: tempfile::TempDir,
    paths: HostPaths,
    key: String,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

fn private_directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("th")
        .tempdir_in(crate::SHORT_TEMP_ROOT)
        .unwrap()
}

fn host_paths(directory: &tempfile::TempDir) -> HostPaths {
    HostPaths {
        runtime: directory.path().join("h"),
    }
}

async fn start() -> Fixture {
    start_in(private_directory()).await
}

async fn start_in(directory: tempfile::TempDir) -> Fixture {
    let paths = host_paths(&directory);
    let key = host::random_token();
    let server = tokio::spawn(host::serve(
        paths.clone(),
        host::HostConfig {
            controller_key: key.clone(),
            idle_timeout: host::DEFAULT_IDLE_TIMEOUT,
        },
    ));
    wait_until_serving(&paths, &key).await;
    Fixture {
        _directory: directory,
        paths,
        key,
        server,
    }
}

/// The socket file appears at bind, before the host accepts, so the host is ready once it
/// answers a management hello. Management never takes the controller role, so the controllers a
/// test connects still decide which one is fenced.
async fn wait_until_serving(paths: &HostPaths, key: &str) {
    let socket = paths.socket();
    for _ in 0..500 {
        if let Ok(stream) = UnixStream::connect(&socket).await {
            let (_, _, hello) = exchange_hello(stream, management_hello(key)).await;
            assert!(
                hello["result"]["epoch"].is_string(),
                "the host answers management: {hello}"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the host never accepted a connection on {}",
        socket.display()
    );
}

fn management_hello(key: &str) -> Value {
    json!({"channel":"management","major":1,"minor":0,"controller_key":key})
}

async fn raw(
    socket: &PathBuf,
    hello: Value,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
    Value,
) {
    exchange_hello(UnixStream::connect(socket).await.unwrap(), hello).await
}

async fn exchange_hello(
    stream: UnixStream,
    hello: Value,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
    Value,
) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    write_frame(&mut writer, &hello, MANAGEMENT_FRAME_BYTES)
        .await
        .unwrap();
    let reply = read_frame(&mut reader, BUSINESS_FRAME_BYTES)
        .await
        .unwrap()
        .unwrap();
    (reader, writer, reply)
}

fn controller(fixture: &Fixture) -> Role {
    Role::Controller {
        controller_key: fixture.key.clone(),
        daemon_epoch: uuid::Uuid::new_v4(),
    }
}

#[tokio::test]
async fn only_the_daemon_key_controls_and_incompatible_or_mixed_requests_are_refused() {
    let fixture = start().await;
    let socket = fixture.paths.socket();
    let mut daemon =
        HostConnection::connect(&socket, controller(&fixture), &[CAPABILITY_LIFECYCLE])
            .await
            .unwrap();
    let epoch = daemon.epoch();
    daemon
        .request("list", json!({}), Duration::from_secs(5))
        .await
        .unwrap();

    let forged = HostConnection::connect(
        &socket,
        Role::Controller {
            controller_key: "0".repeat(64),
            daemon_epoch: uuid::Uuid::new_v4(),
        },
        &[CAPABILITY_LIFECYCLE],
    )
    .await;
    assert_eq!(forged.err().unwrap().code, "Forbidden");

    let reader = HostConnection::connect(
        &socket,
        Role::Instance {
            instance: "hosted:00000000-0000-4000-8000-000000000000"
                .parse()
                .unwrap(),
            data_token: "x".into(),
        },
        &[],
    )
    .await;
    assert_eq!(
        reader.err().unwrap().code,
        "Forbidden",
        "unknown instances have no reader"
    );

    let (_, _, future_major) = raw(
        &socket,
        json!({"channel":"business","major":tickhost_connect::hosted::BUSINESS_MAJOR + 1,"minor":0,"required":[],"role":{"kind":"controller","controller_key":fixture.key,"daemon_epoch":uuid::Uuid::new_v4()}}),
    )
    .await;
    assert_eq!(future_major["error"]["code"], "IncompatibleHost");

    let missing = HostConnection::connect(&socket, controller(&fixture), &["teleport"]).await;
    assert_eq!(missing.err().unwrap().code, "IncompatibleHost");

    let (mut reader, mut writer, hello) = raw(&socket, management_hello(&fixture.key)).await;
    assert_eq!(hello["result"]["epoch"], json!(epoch));
    write_frame(
        &mut writer,
        &json!({"id":1,"method":"open","params":{}}),
        MANAGEMENT_FRAME_BYTES,
    )
    .await
    .unwrap();
    let mixed = read_frame(&mut reader, MANAGEMENT_FRAME_BYTES)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        mixed["error"]["code"], "MethodNotAllowed",
        "management never carries business requests"
    );
    let business_shutdown = daemon
        .request(
            "shutdown_host",
            json!({"epoch":epoch,"operation":uuid::Uuid::new_v4()}),
            Duration::from_secs(5),
        )
        .await;
    assert_eq!(business_shutdown.err().unwrap().code, "MethodNotAllowed");

    let (_, _, wrong_key) = raw(&socket, management_hello(&"0".repeat(64))).await;
    assert_eq!(wrong_key["error"]["code"], "Forbidden");
    shutdown(fixture).await;
}

/// The Agent daemon lock admits one daemon at a time, so the controller that said hello last is
/// the current daemon.
#[tokio::test]
async fn a_later_daemon_fences_the_earlier_controller() {
    let fixture = start().await;
    let socket = fixture.paths.socket();
    let mut old = HostConnection::connect(&socket, controller(&fixture), &[CAPABILITY_LIFECYCLE])
        .await
        .unwrap();
    let mut new = HostConnection::connect(&socket, controller(&fixture), &[CAPABILITY_LIFECYCLE])
        .await
        .unwrap();
    assert_eq!(
        old.request("list", json!({}), Duration::from_secs(5))
            .await
            .err()
            .unwrap()
            .code,
        "Fenced"
    );
    new.request("list", json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    shutdown(fixture).await;
}

#[tokio::test]
async fn oversized_and_partial_frames_never_stop_the_host() {
    let fixture = start().await;
    let socket = fixture.paths.socket();
    let (reader, mut writer) = UnixStream::connect(&socket).await.unwrap().into_split();
    let mut reader = BufReader::new(reader);
    let huge = vec![b'x'; MANAGEMENT_FRAME_BYTES + 2];
    tokio::io::AsyncWriteExt::write_all(&mut writer, &huge)
        .await
        .unwrap();
    let refused = read_frame(&mut reader, BUSINESS_FRAME_BYTES).await;
    assert!(
        matches!(&refused, Ok(None) | Err(_)),
        "the connection closes: {refused:?}"
    );
    // A partial hello followed by EOF ends only its own connection.
    let mut partial = UnixStream::connect(&socket).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut partial, br#"{"channel":"business","ma"#)
        .await
        .unwrap();
    drop(partial);
    let mut daemon =
        HostConnection::connect(&socket, controller(&fixture), &[CAPABILITY_LIFECYCLE])
            .await
            .unwrap();
    daemon
        .request("list", json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    shutdown(fixture).await;
}

#[tokio::test]
async fn management_names_its_host_epoch_and_answers_after_cleanup() {
    let fixture = start().await;
    let (mut reader, mut writer, hello) =
        raw(&fixture.paths.socket(), management_hello(&fixture.key)).await;
    let epoch = hello["result"]["epoch"].clone();
    let mut ask = async |id: u64, method: &str, params: Value| {
        write_frame(
            &mut writer,
            &json!({"id":id,"method":method,"params":params}),
            MANAGEMENT_FRAME_BYTES,
        )
        .await
        .unwrap();
        read_frame(&mut reader, MANAGEMENT_FRAME_BYTES)
            .await
            .unwrap()
            .unwrap()
    };
    let operation = uuid::Uuid::new_v4();
    let stale = ask(
        1,
        "shutdown_host",
        json!({"epoch":uuid::Uuid::new_v4(),"operation":operation}),
    )
    .await;
    assert_eq!(
        stale["error"]["code"], "EpochChanged",
        "a request for another host process never stops this one"
    );
    let unknown = ask(
        2,
        "shutdown_status",
        json!({"epoch":epoch,"operation":operation}),
    )
    .await;
    assert_eq!(unknown["result"]["state"], "unknown");
    let completed = ask(
        3,
        "shutdown_host",
        json!({"epoch":epoch,"operation":operation}),
    )
    .await;
    assert_eq!(completed["result"]["state"], "completed");
    assert!(
        !fixture.paths.socket().exists(),
        "completion is reported after the endpoint is gone"
    );
    tokio::time::timeout(Duration::from_secs(5), fixture.server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn an_unused_host_exits_after_its_initial_grace() {
    let fixture = start().await;
    let started = std::time::Instant::now();
    tokio::time::timeout(host::INITIAL_GRACE + Duration::from_secs(2), fixture.server)
        .await
        .expect("an unused host exits")
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= host::INITIAL_GRACE - Duration::from_millis(500));
    assert!(
        !fixture.paths.socket().exists(),
        "the host removes its own socket"
    );
}

/// Management answers left unread are given up after the 10-second control bound: the host
/// closes that connection instead of waiting on its reader forever.
#[tokio::test]
async fn a_management_reader_that_stops_reading_loses_its_connection() {
    const REQUESTS: u64 = 4000;
    let fixture = start().await;
    let (mut reader, mut writer, hello) =
        raw(&fixture.paths.socket(), management_hello(&fixture.key)).await;
    let epoch = hello["result"]["epoch"].clone();
    // Far more answers than the socket buffers hold, and none of them read for a while.
    let flooding = tokio::spawn(async move {
        for id in 0..REQUESTS {
            let request = json!({"id":id,"method":"shutdown_status","params":{"epoch":epoch,"operation":uuid::Uuid::new_v4()}});
            if write_frame(&mut writer, &request, MANAGEMENT_FRAME_BYTES)
                .await
                .is_err()
            {
                break;
            }
        }
        writer
    });
    tokio::time::sleep(tickhost_connect::hosted::CONTROL_TIMEOUT + Duration::from_secs(3)).await;
    let mut answered = 0;
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(Some(_)) = read_frame(&mut reader, MANAGEMENT_FRAME_BYTES).await {
            answered += 1;
        }
    })
    .await;
    assert!(closed.is_ok(), "the host closed the stalled connection");
    assert!(
        answered < REQUESTS,
        "every one of {answered} answers waited"
    );
    drop(flooding);
    tokio::time::timeout(Duration::from_secs(10), fixture.server)
        .await
        .expect("the unused host exits")
        .unwrap()
        .unwrap();
}

/// The session directory a host gives its launchers starts empty: what the Ticks of an earlier
/// host left there goes once the new host holds the lock.
#[tokio::test]
async fn a_new_host_clears_what_the_ticks_of_an_earlier_host_left() {
    let directory = private_directory();
    let paths = host_paths(&directory);
    paths.prepare().unwrap();
    let left = paths.sessions().join("7");
    std::fs::create_dir(&left).unwrap();
    for socket in [
        left.join(format!("{}.sock", uuid::Uuid::new_v4())),
        paths
            .sessions()
            .join(format!("{}.sock", uuid::Uuid::new_v4())),
    ] {
        drop(std::os::unix::net::UnixListener::bind(socket).unwrap());
    }
    let fixture = start_in(directory).await;
    assert_eq!(
        std::fs::read_dir(fixture.paths.sessions())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>(),
        Vec::<PathBuf>::new()
    );
    shutdown(fixture).await;
}

async fn shutdown(fixture: Fixture) {
    let (mut reader, mut writer, hello) =
        raw(&fixture.paths.socket(), management_hello(&fixture.key)).await;
    let operation = uuid::Uuid::new_v4();
    write_frame(
        &mut writer,
        &json!({"id":1,"method":"shutdown_host","params":{"epoch":hello["result"]["epoch"],"operation":operation}}),
        MANAGEMENT_FRAME_BYTES,
    )
    .await
    .unwrap();
    let completed = read_frame(&mut reader, MANAGEMENT_FRAME_BYTES)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed["result"]["state"], "completed");
    tokio::time::timeout(Duration::from_secs(10), fixture.server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!fixture.paths.socket().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_closes_connections_waiting_for_a_hello_before_releasing_the_host() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let fixture = start().await;
    let mut unfinished = UnixStream::connect(fixture.paths.socket()).await.unwrap();
    unfinished.write_all(b"{").await.unwrap();
    shutdown(fixture).await;
    let mut byte = [0];
    let read = tokio::time::timeout(Duration::from_millis(100), unfinished.read(&mut byte))
        .await
        .expect("a pending hello must not retain its connection after host shutdown")
        .unwrap();
    assert_eq!(read, 0);
}
