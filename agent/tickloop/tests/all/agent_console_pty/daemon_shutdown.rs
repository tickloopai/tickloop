use super::*;

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
