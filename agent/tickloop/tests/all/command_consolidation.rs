use crate::support;

use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn retired_commands_do_not_execute_or_become_prompts() {
    let directory = tempfile::tempdir().unwrap();
    let retired = [
        "/new",
        "/theme",
        "/logout",
        "/triggers",
        "/schedule 60 check",
        "/checkpoint",
        "/undo",
        "/effort high",
        "/config",
        "/jev",
        "/commands",
        "/bashes",
        "/subs",
        "/quit",
        "/loops",
        "/loops new",
        "/events check",
        "/trigger feed {}",
        "/clear-trigger",
    ];
    let runtime_directory = support::cli_runtime::adopt(&directory.path().join("state"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .args(["--working-directory-is-trusted", "--no-session-persistence"])
        .current_dir(directory.path())
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n/help advanced\n/status\n/exit\n", retired.join("\n")).as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    assert_eq!(
        stdout.matches("Unknown command; use /help.").count(),
        retired.len(),
        "{stdout}"
    );
    assert!(stdout.contains("/rule allow|ask|deny"));
    assert!(stdout.contains("/interrupt"));
    // No retired command created an Agent or queued input: the New Agent screen still has none.
    assert!(stdout.contains("No Chat is selected."), "{stdout}");
    assert!(!stdout.contains("Chat: "), "{stdout}");
    assert!(!stdout.contains("Accepted:"));
    assert!(!stdout.contains("Codex login removed"));
    use tickloop_agent_client::LocalAgentClient;

    use tickloop_agent_access::{AgentAccess as _, ObservationRequest, ObservationValue, Page};

    let catalog = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let client = LocalAgentClient::connect(runtime_directory.join("daemon.sock"))
                .await
                .unwrap();
            client
                .observe(ObservationRequest::Overview {
                    filter: Default::default(),
                    page: Page {
                        offset: 0,
                        limit: 50,
                    },
                    after: None,
                })
                .await
                .unwrap()
                .initial
        });
    let ObservationValue::Overview(agents) = catalog else {
        panic!("expected the daemon catalog");
    };
    assert!(agents.is_empty(), "{agents:?}");
}
