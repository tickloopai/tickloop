use super::*;

async fn serve_incompatible_daemon(
    endpoint: std::path::PathBuf,
    acknowledge: bool,
) -> (
    tokio::task::JoinHandle<()>,
    Arc<std::sync::atomic::AtomicUsize>,
    uuid::Uuid,
) {
    use std::os::unix::fs::PermissionsExt;
    use tickloop_agent_client::daemon_management::{read, write};
    let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
    std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
    let epoch = uuid::Uuid::new_v4();
    let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recorded = stops.clone();
    let task = tokio::spawn(async move {
        let (mut business, _) = listener.accept().await.unwrap();
        let hello: Value = read(&mut business).await.unwrap().unwrap();
        assert_eq!(
            hello["Hello"]["major"],
            tickloop_agent_protocol::PROTOCOL_MAJOR
        );
        write(&mut business, &json!({"Hello":{"major":tickloop_agent_protocol::PROTOCOL_MAJOR-1,"minor":0,"daemon_epoch":epoch,"session_id":uuid::Uuid::new_v4(),"capabilities":[]}})).await.unwrap();
        assert!(read::<_, Value>(&mut business).await.unwrap().is_none());
        let (mut management, _) = listener.accept().await.unwrap();
        assert_eq!(
            read::<_, Value>(&mut management).await.unwrap().unwrap(),
            json!({"Hello":{"major":1,"minor":0}})
        );
        write(
            &mut management,
            &json!({"Hello":{"major":1,"minor":0,"daemon_epoch":epoch}}),
        )
        .await
        .unwrap();
        let Some(request) = read::<_, Value>(&mut management).await.unwrap() else {
            return;
        };
        assert_eq!(request["ShutdownDaemon"]["daemon_epoch"], json!(epoch));
        recorded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if acknowledge {
            write(
                &mut management,
                &json!({"DaemonStopped":{"request_id":request["ShutdownDaemon"]["request_id"]}}),
            )
            .await
            .unwrap();
        }
        std::fs::remove_file(endpoint).unwrap();
    });
    (task, stops, epoch)
}

#[tokio::test]
async fn incompatible_startup_confirmation_opens_the_new_daemon_with_original_options() {
    let fixture = PrivateFixture::new();
    let (old, stops, old_epoch) =
        serve_incompatible_daemon(fixture.path("runtime/daemon.sock"), true).await;
    let mut view = PtyView::start_with_flags(&fixture, &["--mode", "ask"]);
    let result = AssertUnwindSafe(async {
        let frame = view
            .wait_for("daemon replacement confirmation", |frame| {
                frame.contains("Stop the running daemon and continue?")
            })
            .await;
        assert!(!frame.contains("TICKLOOP_DAEMON_RUNTIME_DIR="));
        assert_eq!(stops.load(std::sync::atomic::Ordering::SeqCst), 0);
        view.send(b"k\r");
        view.wait_for(
            "new version opens without another launch",
            new_input_is_visible,
        )
        .await;
        let client = fixture.client().await;
        assert_ne!(client.daemon_epoch(), old_epoch);
        assert_eq!(stops.load(std::sync::atomic::Ordering::SeqCst), 1);
        view.send(b"!printf 'restarted-in-original-directory\\n'\r");
        view.wait_for("new daemon runs the original View", |frame| {
            frame
                .lines()
                .any(|line| line.trim() == "restarted-in-original-directory")
        })
        .await;
        let agents = catalog(&client).await;
        let opened = snapshot(&client, agents[0].execution).await;
        assert_eq!(
            opened.conversation.workdir,
            fixture.path("work").canonicalize().unwrap()
        );
        assert_eq!(
            opened.policy.mode,
            tickloop_agent_protocol::ExecutionMode::AcceptEdits
        );
        view.close().await;
    })
    .catch_unwind()
    .await;
    if view.child.try_wait().unwrap().is_none() {
        view.child.kill().unwrap();
        view.child.wait().unwrap();
    }
    old.abort();
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn incompatible_startup_never_replaces_a_changed_target_or_retries_an_unknown_stop() {
    use std::os::unix::fs::PermissionsExt;
    for replace_target in [true, false] {
        let fixture = PrivateFixture::new();
        let endpoint = fixture.path("runtime/daemon.sock");
        let (old, stops, _) = serve_incompatible_daemon(endpoint.clone(), false).await;
        let mut view = PtyView::start(&fixture);
        let result = AssertUnwindSafe(async {
            view.wait_for("daemon replacement confirmation", |frame| {
                frame.contains("Stop the running daemon and continue?")
            })
            .await;
            let replacement = if replace_target {
                std::fs::rename(&endpoint, fixture.path("runtime/previous.sock")).unwrap();
                let replacement = tokio::net::UnixListener::bind(&endpoint).unwrap();
                std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
                Some(replacement)
            } else {
                None
            };
            view.send(b"k\r");
            tokio::time::timeout(UI_TIMEOUT, async {
                loop {
                    if let Some(status) = view.child.try_wait().unwrap() {
                        assert!(!status.success());
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                stops.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(!replace_target)
            );
            assert!(
                !fixture.path("runtime/daemon.lock").exists(),
                "no replacement daemon was started"
            );
            if let Some(listener) = replacement {
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), listener.accept())
                        .await
                        .is_err(),
                    "the new target must not receive any request"
                );
                drop(listener);
                std::fs::remove_file(&endpoint).unwrap();
            } else {
                assert!(!endpoint.exists());
            }
        })
        .catch_unwind()
        .await;
        if view.child.try_wait().unwrap().is_none() {
            view.child.kill().unwrap();
            view.child.wait().unwrap();
        }
        old.abort();
        fixture.shutdown().await;
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

#[tokio::test]
async fn explicit_daemon_stop_stays_stopped_with_a_live_view_until_a_new_launch() {
    let fixture = PrivateFixture::new();
    let mut view = PtyView::start(&fixture);
    let result = AssertUnwindSafe(async {
        view.wait_for("View ready before daemon stop", new_input_is_visible)
            .await;
        view.send(b"UNSENT-STOP-DRAFT");
        view.wait_for_editor("draft before daemon stop", Some("UNSENT-STOP-DRAFT"))
            .await;
        let client = fixture.client().await;
        let original_epoch = client.daemon_epoch();
        let output = support::fixture_cli::tokio_command()
            .args(["daemon", "stop"])
            .env("TICKLOOP_DAEMON_RUNTIME_DIR", fixture.path("runtime"))
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "stop must finish while its View stays open: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let endpoint = fixture.path("runtime/daemon.sock");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            assert!(
                !endpoint.exists(),
                "the surviving View restarted the explicitly stopped daemon"
            );
            assert!(view.child.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        view.wait_for_editor(
            "disconnected draft survives stop",
            Some("UNSENT-STOP-DRAFT"),
        )
        .await;

        let replacement = tickloop_agent_client::connect_or_start_at(
            fixture.path("runtime"),
            support::fixture_cli::executable(),
        )
        .await
        .unwrap();
        assert_ne!(replacement.daemon_epoch(), original_epoch);
        view.wait_for("View reconnects to an explicitly started daemon", |frame| {
            frame.contains("UNSENT-STOP-DRAFT") && !frame.contains("Disconnected")
        })
        .await;
        view.send(b"\x15!printf 'after-stop-reconnected\\n'\r");
        view.wait_for(
            "the surviving View can use the replacement daemon",
            |frame| {
                frame
                    .lines()
                    .any(|line| line.trim() == "after-stop-reconnected")
            },
        )
        .await;
        let agents = catalog(&replacement).await;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].execution.daemon_epoch, replacement.daemon_epoch());
    })
    .catch_unwind()
    .await;
    if view.child.try_wait().unwrap().is_none() {
        view.child.kill().unwrap();
    }
    view.child.wait().unwrap();
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
