#![cfg(target_os = "macos")]
use super::*;

#[tokio::test]
async fn bang_shell_uses_the_daemon_tickhost_authenticated_network_proxy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fixture = PrivateFixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(stream.read_u8().await.unwrap());
        }
        assert!(
            !String::from_utf8_lossy(&header)
                .to_ascii_lowercase()
                .contains("\r\nproxy-authorization:")
        );
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\ndaemon-proxy-ok").await.unwrap();
    });
    let result = AssertUnwindSafe(async {
        std::fs::write(fixture.path("config/settings.json"), serde_json::to_vec(&json!({
            "sandbox":{"enabled":true,"failIfUnavailable":true,"network":{"allowedDomains":["localhost"]}}
        })).unwrap()).unwrap();
        let mut view = PtyView::start(&fixture);
        view.wait_for("new input ready", new_input_is_visible).await;
        view.send(format!("!/usr/bin/curl --silent --show-error --fail --max-time 5 http://localhost:{port}/\r").as_bytes());
        view.wait_for("authenticated proxy response through daemon and worker", |frame| frame.contains("daemon-proxy-ok")).await;
        view.close().await;
    }).catch_unwind().await;
    server.abort();
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn permissions_shows_sandbox_setting_and_bang_shell_obeys_it() {
    for enabled in [true, false] {
        let fixture = PrivateFixture::new();
        let result = AssertUnwindSafe(async {
            std::fs::write(fixture.path("config/settings.json"), serde_json::to_vec(&json!({
                "sandbox": {"enabled": enabled, "failIfUnavailable":true, "filesystem":{"denyWrite":["blocked"]}, "ignoreViolations":{}}
            })).unwrap()).unwrap();
            std::fs::create_dir(fixture.path("work/.tickloop")).unwrap();
            std::fs::write(fixture.path("work/.tickloop/settings.json"), r#"{"sandbox":{"enabled":false}}"#).unwrap();
            let mut view = PtyView::start(&fixture);
            view.wait_for("sandbox configuration warning", |frame| frame.contains("sandbox.ignoreViolations") && frame.contains("暂不支持")).await;
            view.wait_for("new input ready", new_input_is_visible).await;
            view.send(b"!printf allowed > allowed; printf blocked > blocked\r");
            view.wait_for("Shell finished and composer restored", |frame| {
                frame.lines().any(|line| line.trim() == "❯")
                    && if enabled { frame.contains("被沙箱拦截") } else { frame.contains("(No output)") && fixture.path("work/blocked").exists() }
            }).await;
            assert_eq!(std::fs::read_to_string(fixture.path("work/allowed")).unwrap(), "allowed");
            assert_eq!(fixture.path("work/blocked").exists(), !enabled);
            view.wait_for_editor("Shell releases the editor", Some("")).await;
            view.send(b"/permissions\r");
            let label = if enabled { "Sandbox: on" } else { "Sandbox: off" };
            view.wait_for("configured sandbox state in permissions", |frame| frame.contains(label)).await;
            view.send(b"\x1b");
            view.close().await;
        }).catch_unwind().await;
        fixture.shutdown().await;
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

#[tokio::test]
async fn sandbox_mode_changes_apply_to_the_next_shell_without_restarting() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        std::fs::write(
            fixture.path("config/settings.json"),
            json!({
                "sandbox": {"failIfUnavailable":true, "filesystem":{"denyWrite":["blocked"]}}
            })
            .to_string(),
        )
        .unwrap();
        let mut view = PtyView::start(&fixture);
        view.wait_for("new input ready", new_input_is_visible).await;
        view.send(b"/sandbox\r");
        view.wait_for("sandbox Mode", |frame| frame.contains("[Mode]"))
            .await;
        view.send(b"kk\r");
        view.wait_for("saved sandbox mode", |frame| {
            frame.contains("Saved for this Folder")
        })
        .await;
        view.send(b"\x1b");
        view.wait_for_editor("sandbox closes", Some("")).await;
        view.send(b"!printf blocked > blocked\r");
        view.wait_for("next Shell is sandboxed", |frame| {
            frame.contains("被沙箱拦截")
        })
        .await;
        assert!(!fixture.path("work/blocked").exists());
        view.wait_for_editor("Shell finished", Some("")).await;
        view.send(b"/sandbox\r");
        view.wait_for("sandbox Mode again", |frame| frame.contains("[Mode]"))
            .await;
        view.send(b"jj\r");
        view.wait_for("sandbox disabled", |frame| {
            frame.contains("Saved for this Folder")
        })
        .await;
        view.send(b"\x1b");
        view.wait_for_editor("sandbox closes again", Some("")).await;
        view.send(b"!printf allowed > blocked\r");
        view.wait_for("Shell without sandbox", |frame| {
            frame.contains("(No output)") && fixture.path("work/blocked").exists()
        })
        .await;
        assert_eq!(
            std::fs::read_to_string(fixture.path("work/blocked")).unwrap(),
            "allowed"
        );
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn sandbox_mode_changes_apply_to_model_bash_in_the_same_chat() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let (url, _requests) = model_http_support::serve_responses(vec![
            (200, chat_reply(Some(("Bash", "sandboxed-write", json!({"command":"printf first > blocked","description":"Try the protected write"}))), "")),
            (200, chat_reply(None, "FIRST-BASH-FINISHED")),
            (200, chat_reply(Some(("Bash", "ordinary-write", json!({"command":"printf second > blocked","description":"Write after the mode change"}))), "")),
            (200, chat_reply(None, "SECOND-BASH-FINISHED")),
        ]).await;
        fixture.set_chat_model(&url);
        std::fs::write(fixture.path("config/settings.json"), json!({"sandbox":{"failIfUnavailable":true,"filesystem":{"denyWrite":["blocked"]}}}).to_string()).unwrap();
        let mut view = PtyView::start_with_flags(&fixture, &["--dangerously-skip-permissions"]);
        view.wait_for("new input ready", new_input_is_visible).await;
        view.send(b"/sandbox\r");
        view.wait_for("Mode", |frame| frame.contains("[Mode]")).await;
        view.send(b"kk\r");
        view.wait_for("sandbox enabled", |frame| frame.contains("Saved for this Folder")).await;
        view.send(b"\x1b");
        view.wait_for_editor("sandbox closes", Some("")).await;
        view.send(b"Try the protected write.\r");
        view.wait_for("first model Bash finished", |frame| frame.contains("FIRST-BASH-FINISHED")).await;
        assert!(!fixture.path("work/blocked").exists());
        view.wait_for_editor("first turn finished", Some("")).await;
        view.send(b"/sandbox\r");
        view.wait_for("Mode again", |frame| frame.contains("[Mode]")).await;
        view.send(b"jj\r");
        view.wait_for("sandbox disabled", |frame| frame.contains("Saved for this Folder")).await;
        view.send(b"\x1b");
        view.wait_for_editor("sandbox closes again", Some("")).await;
        view.send(b"Write again.\r");
        view.wait_for("second model Bash finished", |frame| frame.contains("SECOND-BASH-FINISHED")).await;
        assert_eq!(std::fs::read_to_string(fixture.path("work/blocked")).unwrap(), "second");
        view.close().await;
    }).catch_unwind().await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn folder_sandbox_unsupported_fields_are_reported_at_startup() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(async {
        let folder = fixture.path("work").canonicalize().unwrap();
        std::fs::write(
            fixture.path("config/settings.json"),
            json!({"projects":{
                folder.to_str().unwrap():{"sandbox":{"filesystem":{"allowRead":["private"]}}}
            }})
            .to_string(),
        )
        .unwrap();
        let mut view = PtyView::start(&fixture);
        view.wait_for("directory sandbox unsupported-field warning", |frame| {
            frame.contains("sandbox.filesystem.allowRead") && frame.contains("暂不支持")
        })
        .await;
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
