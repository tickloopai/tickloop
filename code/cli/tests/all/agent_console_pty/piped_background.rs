//! A piped Console waits for its family's background work as Claude Code's print mode does: it
//! stays open while a Subagent runs or its completion waits, answers each completion in a turn
//! of its own, announces a long wait on stderr, and Ctrl+C leaves at once.

use super::*;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

const PROMPT: &str = "Audit both modules in the background.";
const WAIT_NOTICE: &str = "Waiting for background work to finish (";

/// The piped CLI on the fixture's daemon, with its stdin closed after `input`.
fn piped_cli(fixture: &PrivateFixture) -> tokio::process::Child {
    let mut command = support::fixture_cli::tokio_command();
    command
        .args(["--working-directory-is-trusted", "--transport-fixture"])
        .arg(fixture.path("model.json"))
        .args(["--no-session-persistence", "--dangerously-skip-permissions"])
        .current_dir(fixture.path("work"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", fixture.path("home"))
        .env("TICKLOOP_CONFIG_DIR", fixture.path("config"))
        .env("TICKLOOP_TMPDIR", fixture.path("scratch"))
        .env("TMPDIR", fixture.path("scratch"))
        .env("TICKLOOP_FIXTURE_API_KEY", "fixture-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.spawn().unwrap()
}

/// Sends every line `stream` writes to the returned receiver, and keeps them all.
fn lines_of(
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
) -> (
    mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        let mut kept = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = sender.send(line.clone());
            kept.push(line);
        }
        kept
    });
    (receiver, reader)
}

async fn until_line(lines: &mut mpsc::UnboundedReceiver<String>, wanted: &str) -> String {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let line = lines.recv().await.expect("the stream ended first");
            if line.contains(wanted) {
                return line;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{wanted:?} never appeared"))
}

/// A root that launches the children `children` in turn, then answers each report it takes; a
/// child reports once the test releases it.
async fn family_model(
    children: &[&'static str],
) -> (
    String,
    mpsc::UnboundedReceiver<(String, Value)>,
    Vec<oneshot::Sender<()>>,
) {
    let mut gates = std::collections::HashMap::new();
    let mut releases = Vec::new();
    for child in children {
        let (release, gate) = oneshot::channel();
        gates.insert(*child, gate);
        releases.push(release);
    }
    let children = children.to_vec();
    let (url, requests) = model_http_support::serve_router_with_gates(move |request| {
        let history = request["messages"].to_string();
        if let Some(child) = delegated_by(request) {
            let child = children.iter().find(|name| **name == child).unwrap();
            return (
                chat_reply(None, &format!("{}-REPORT", child.to_uppercase())),
                gates.remove(child),
            );
        }
        if let Some(child) = children.iter().find(|child| !has_result(request, child)) {
            let task = format!("Audit the {child} module");
            return (
                chat_reply(
                    Some((
                        "Subagent",
                        child,
                        json!({"name":child,"description":task,"task":task}),
                    )),
                    "",
                ),
                None,
            );
        }
        let reports = children
            .iter()
            .filter(|child| history.contains(&format!("{}-REPORT", child.to_uppercase())))
            .count();
        let reply = match reports {
            0 => "All audits started".to_owned(),
            count if count == children.len() => "ALL-REPORTS-HANDLED".to_owned(),
            count => format!("{count} report handled"),
        };
        (chat_reply(None, &reply), None)
    })
    .await;
    (url, requests, releases)
}

async fn exercise_waiting_for_both_reports(fixture: &PrivateFixture) {
    let (url, _requests, releases) = family_model(&["first", "second"]).await;
    fixture.set_chat_model(&url);
    let mut cli = piped_cli(fixture);
    let mut input = cli.stdin.take().unwrap();
    input
        .write_all(format!("{PROMPT}\n").as_bytes())
        .await
        .unwrap();
    drop(input);
    let (mut stdout, stdout_kept) = lines_of(cli.stdout.take().unwrap());
    let (mut stderr, stderr_kept) = lines_of(cli.stderr.take().unwrap());

    until_line(&mut stdout, "All audits started").await;
    let notice = until_line(&mut stderr, WAIT_NOTICE).await;
    assert_eq!(
        notice,
        "Waiting for background work to finish (subagent \"Audit the first module\", subagent \"Audit the second module\"). Press Ctrl+C to exit now."
    );
    assert!(
        cli.try_wait().unwrap().is_none(),
        "the View waits for its children"
    );
    let mut releases = releases.into_iter();
    releases.next().unwrap().send(()).unwrap();
    until_line(&mut stdout, "1 report handled").await;
    assert!(
        cli.try_wait().unwrap().is_none(),
        "the View waits for the other child after taking the first report"
    );
    releases.next().unwrap().send(()).unwrap();
    let status = tokio::time::timeout(Duration::from_secs(20), cli.wait())
        .await
        .expect("the View exits once the family is done")
        .unwrap();
    assert!(status.success());
    let stdout = stdout_kept.await.unwrap().join("\n");
    assert!(stdout.contains("ALL-REPORTS-HANDLED"), "{stdout}");
    let stderr = stderr_kept.await.unwrap();
    assert_eq!(
        stderr
            .iter()
            .filter(|line| line.starts_with(WAIT_NOTICE))
            .count(),
        1,
        "each piece of work is announced once: {stderr:?}"
    );
}

#[tokio::test]
async fn a_piped_view_waits_for_both_background_subagents_and_their_reports() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(exercise_waiting_for_both_reports(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn exercise_ctrl_c_while_waiting(fixture: &PrivateFixture) {
    let (url, _requests, _held) = family_model(&["first"]).await;
    fixture.set_chat_model(&url);
    let mut cli = piped_cli(fixture);
    let mut input = cli.stdin.take().unwrap();
    input
        .write_all(format!("{PROMPT}\n").as_bytes())
        .await
        .unwrap();
    drop(input);
    let (_stdout, stdout_kept) = lines_of(cli.stdout.take().unwrap());
    let (mut stderr, _stderr_kept) = lines_of(cli.stderr.take().unwrap());
    until_line(&mut stderr, WAIT_NOTICE).await;
    let pid = cli.id().unwrap() as libc::pid_t;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let status = tokio::time::timeout(Duration::from_secs(5), cli.wait())
        .await
        .expect("Ctrl+C leaves at once")
        .unwrap();
    assert!(status.success());
    let stdout = stdout_kept.await.unwrap().join("\n");
    assert!(
        stdout.contains("Closing this View; Agent work continues."),
        "{stdout}"
    );
    assert!(!stdout.contains("ALL-REPORTS-HANDLED"), "{stdout}");
}

#[tokio::test]
async fn ctrl_c_leaves_a_piped_view_waiting_for_background_work() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(exercise_ctrl_c_while_waiting(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
