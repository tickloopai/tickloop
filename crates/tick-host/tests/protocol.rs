//! The host's frozen management channel, business negotiation and role authentication, served
//! by the real host library on a private socket.

use std::{path::PathBuf, time::Duration};

use serde_json::{Value, json};
use tick_connect::hosted::{
    BUSINESS_FRAME_BYTES, CAPABILITY_LIFECYCLE, HostConnection, MANAGEMENT_FRAME_BYTES, Role,
    read_frame, write_frame,
};
use tick_host::{host, paths::HostPaths};
use tokio::{io::BufReader, net::UnixStream};

struct Fixture {
    _directory: tempfile::TempDir,
    paths: HostPaths,
    key: String,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

async fn start() -> Fixture {
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
    Fixture {
        _directory: directory,
        paths,
        key,
        server,
    }
}

async fn raw(
    socket: &PathBuf,
    hello: Value,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
    Value,
) {
    let (reader, mut writer) = UnixStream::connect(socket).await.unwrap().into_split();
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
        owner_epoch: uuid::Uuid::new_v4(),
    }
}

#[tokio::test]
async fn only_the_owner_key_controls_and_incompatible_or_mixed_requests_are_refused() {
    let fixture = start().await;
    let socket = fixture.paths.socket();
    let mut owner = HostConnection::connect(&socket, controller(&fixture), &[CAPABILITY_LIFECYCLE])
        .await
        .unwrap();
    let epoch = owner.epoch();
    owner
        .request("list", json!({}), Duration::from_secs(5))
        .await
        .unwrap();

    let forged = HostConnection::connect(
        &socket,
        Role::Controller {
            controller_key: "0".repeat(64),
            owner_epoch: uuid::Uuid::new_v4(),
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
        json!({"channel":"business","major":2,"minor":0,"required":[],"role":{"kind":"controller","controller_key":fixture.key,"owner_epoch":uuid::Uuid::new_v4()}}),
    )
    .await;
    assert_eq!(future_major["error"]["code"], "IncompatibleHost");

    let missing = HostConnection::connect(&socket, controller(&fixture), &["teleport"]).await;
    assert_eq!(missing.err().unwrap().code, "IncompatibleHost");

    let (mut reader, mut writer, hello) = raw(
        &socket,
        json!({"channel":"management","major":1,"minor":0,"controller_key":fixture.key}),
    )
    .await;
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
    let business_shutdown = owner
        .request(
            "shutdown_host",
            json!({"epoch":epoch,"operation":uuid::Uuid::new_v4()}),
            Duration::from_secs(5),
        )
        .await;
    assert_eq!(business_shutdown.err().unwrap().code, "MethodNotAllowed");

    let (_, _, wrong_key) = raw(
        &socket,
        json!({"channel":"management","major":1,"minor":0,"controller_key":"0".repeat(64)}),
    )
    .await;
    assert_eq!(wrong_key["error"]["code"], "Forbidden");
    shutdown(fixture).await;
}

/// The Agent owner lock admits one owner at a time, so the controller that said hello last is
/// the current owner.
#[tokio::test]
async fn a_later_owner_fences_the_earlier_controller() {
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
    let mut owner = HostConnection::connect(&socket, controller(&fixture), &[CAPABILITY_LIFECYCLE])
        .await
        .unwrap();
    owner
        .request("list", json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    shutdown(fixture).await;
}

#[tokio::test]
async fn management_names_its_host_epoch_and_answers_after_cleanup() {
    let fixture = start().await;
    let (mut reader, mut writer, hello) = raw(
        &fixture.paths.socket(),
        json!({"channel":"management","major":1,"minor":0,"controller_key":fixture.key}),
    )
    .await;
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

async fn shutdown(fixture: Fixture) {
    let (mut reader, mut writer, hello) = raw(
        &fixture.paths.socket(),
        json!({"channel":"management","major":1,"minor":0,"controller_key":fixture.key}),
    )
    .await;
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
