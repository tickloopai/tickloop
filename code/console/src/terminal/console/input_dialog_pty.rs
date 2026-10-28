use super::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Read;

use tickloop_harness::test_support::screen;

pub(super) const FIXTURE_DIRECTORY: &str = "TICKLOOP_INPUT_DIALOG_FIXTURE_DIR";
pub(super) const TIMEOUT: Duration = Duration::from_secs(8);
const REQUEST: Uuid = Uuid::from_u128(7101);
#[test]
#[ignore]
fn input_dialog_pty_fixture() {
    let directory = PathBuf::from(std::env::var(FIXTURE_DIRECTORY).unwrap());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let (console, mut input) = Console::start(true).unwrap();
        console.select_chat(Some(Uuid::from_u128(7100))).unwrap();
        console.set_prompt_enabled(true).unwrap();
        console.state.lock().unwrap().insert("Unsent fixture draft");
        console.record_input("Pinned fixture prompt").unwrap();
        console.write("Visible reply before dialogs\n").unwrap();
        console.set_activity(Some(&[("Working fixture".into(), Some(Role::Accent))])).unwrap();
        let picker = ConversationPicker::new(
            super::super::conversation_picker::ConversationPickerPurpose::Resume,
            (0..20).map(|index| super::super::conversation_picker::ConversationEntry::new(&format!("Chat {index}"), &[], &[])).collect(),
        );
        let user_menu = picker.id();
        console.show_conversation_picker(picker).unwrap();
        std::fs::write(directory.join("phase"), "picker").unwrap();
        let mut requested = false;
        let mut chosen = false;
        let mut poll = tokio::time::interval(Duration::from_millis(20));
        loop {
            tokio::select! {
                event = input.recv() => match event {
                    Some(InputEvent::Choice { menu_id, index, .. }) if menu_id == REQUEST => {
                        assert_eq!(index, 1);
                        console.finish_menu_submission(REQUEST).unwrap();
                        console.clear_menu(REQUEST).unwrap();
                        while console.state.lock().unwrap().menu_submission_is_pending() { tokio::time::sleep(Duration::from_millis(10)).await; }
                        std::fs::write(directory.join("phase"), "resumed").unwrap();
                    }
                    Some(InputEvent::Choice { menu_id, index, .. }) if menu_id == user_menu => {
                        assert_eq!(index, 11);
                        console.finish_menu_submission(user_menu).unwrap();
                        console.clear_menu(user_menu).unwrap();
                        assert_eq!(console.state.lock().unwrap().editor.draft(), "Unsent fixture draft");
                        std::fs::write(directory.join("phase"), "chosen").unwrap();
                        chosen = true;
                    }
                    Some(InputEvent::Eof | InputEvent::Error(_)) | None => panic!("fixture input ended early"),
                    _ => {}
                },
                _ = poll.tick() => {
                    let trigger = std::fs::read_to_string(directory.join("trigger")).unwrap_or_default();
                    if trigger == "request" && !requested {
                        console.set_request_layout(Some(REQUEST)).unwrap();
                        console.show_focused_menu(Menu { id: REQUEST, title: "Permission fixture".into(), context: vec!["Isolated Console request".into()], markdown: None, options: vec!["Allow fixture".into(), "Deny fixture".into()], selected: 0, confirmation: true }).unwrap();
                        std::fs::write(directory.join("phase"), "request").unwrap();
                        requested = true;
                    } else if trigger == "finish" && chosen { break; }
                }
            }
        }
    });
}

#[test]
#[ignore]
fn input_dialog_tick_paste_pty_fixture() {
    let directory = PathBuf::from(std::env::var(FIXTURE_DIRECTORY).unwrap());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let (console, mut input) = Console::start(true).unwrap();
        console.select_chat(Some(Uuid::from_u128(7100))).unwrap();
        console.state.lock().unwrap().monitor.replace(TickGroup::Artifacts, super::input_dialog_tests::fixture_ticks());
        console.set_prompt_enabled(true).unwrap();
        console.record_input("Pinned tick fixture prompt").unwrap();
        std::fs::write(directory.join("phase"), "ticks").unwrap();
        let mut requested = false;
        let mut answered = false;
        let mut poll = tokio::time::interval(Duration::from_millis(20));
        loop {
            tokio::select! {
                event = input.recv() => match event {
                    Some(InputEvent::TickAction { action, .. }) if action.key == 'r' => std::fs::write(directory.join("phase"), "focused").unwrap(),
                    Some(InputEvent::Choice { menu_id, index, comment }) if menu_id == REQUEST => {
                        assert_eq!(index, 0);
                        assert_eq!(comment.as_deref(), Some("j pasted answer 中文"));
                        console.finish_menu_submission(REQUEST).unwrap();
                        console.clear_menu(REQUEST).unwrap();
                        while console.state.lock().unwrap().menu_submission_is_pending() { tokio::time::sleep(Duration::from_millis(10)).await; }
                        assert!(!console.state.lock().unwrap().monitor.focused);
                        answered = true;
                        std::fs::write(directory.join("phase"), "answered").unwrap();
                    }
                    Some(InputEvent::TickAction { action, .. }) => panic!("Monitor took a key after the request: {}", action.key),
                    Some(InputEvent::Eof | InputEvent::Error(_)) | None => panic!("fixture input ended early"),
                    _ => {}
                },
                _ = poll.tick() => {
                    let trigger = std::fs::read_to_string(directory.join("trigger")).unwrap_or_default();
                    if trigger == "request" && !requested {
                        console.set_request_layout(Some(REQUEST)).unwrap();
                        console.show_question_menu(Menu { id: REQUEST, title: "Question fixture".into(), context: vec!["Isolated Tick focus".into()], markdown: None, options: vec!["Other".into()], selected: 0, confirmation: false }, QuestionMenu { entry_option: Some(0), entry_text: String::new(), multi_select: false, navigable: false }).unwrap();
                        requested = true;
                    } else if trigger == "finish" && answered { break; }
                }
            }
        }
    });
}

pub(super) struct Fixture {
    _runtime_directories: crate::terminal::test_runtime::RuntimeDirectories,
    pub(super) child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    width: usize,
    height: usize,
}

impl Fixture {
    pub(super) fn start(
        directory: &std::path::Path,
        width: u16,
        height: u16,
        fixture_name: &str,
    ) -> Self {
        let runtime_directories =
            crate::terminal::test_runtime::RuntimeDirectories::of_config_root(directory);
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: height,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args(["--ignored", "--exact", fixture_name, "--nocapture"]);
        command.env(FIXTURE_DIRECTORY, directory);
        command.env("TICKLOOP_AGENT_TUI", "fullscreen");
        command.env("TERM", "xterm-256color");
        command.env("TICKLOOP_CONFIG_DIR", directory);
        command.env("TICKLOOP_TMPDIR", directory);
        command.cwd(directory);
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = output.clone();
        thread::spawn(move || {
            let mut bytes = [0; 8192];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                captured.lock().unwrap().extend_from_slice(&bytes[..count]);
            }
        });
        Self {
            _runtime_directories: runtime_directories,
            child,
            writer,
            output,
            _master: pair.master,
            width: width as usize,
            height: height as usize,
        }
    }

    pub(super) fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    /// Everything the child wrote, terminal mode switches included.
    pub(super) fn raw_output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Every frame the child completed after the first `offset` bytes of its output.
    pub(super) fn frames_after(&self, offset: usize) -> Vec<(usize, String)> {
        screen::frames_after(&self.raw_output(), offset, self.width, self.height)
    }

    pub(super) fn frame(&self) -> String {
        // Copies the output first: the reader keeps draining the PTY while the screen is replayed,
        // so a long replay cannot block the child's writes.
        let output = String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned();
        screen::visible_screen(&output, self.width, self.height)
    }

    pub(super) fn wait(&self, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let frame = self.frame();
            if predicate(&frame) {
                return frame;
            }
            assert!(
                Instant::now() < deadline,
                "{}×{}: {frame}",
                self.width,
                self.height
            );
            thread::sleep(Duration::from_millis(15));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn input_dialog_pty_pastes_into_the_request_and_returns_to_the_input() {
    for (width, height) in [(80, 24), (120, 40), (32, 10)] {
        let directory = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::start(
            directory.path(),
            width,
            height,
            "terminal::console::input_dialog_pty::input_dialog_tick_paste_pty_fixture",
        );
        fixture.wait(|_| {
            std::fs::read_to_string(directory.path().join("phase"))
                .is_ok_and(|phase| phase == "ticks")
        });
        fixture.send(b"\x1b[C");
        fixture.wait(|frame| {
            frame.contains("Fixture Tick 0")
                && std::fs::read_to_string(directory.path().join("phase"))
                    .is_ok_and(|phase| phase == "focused")
        });
        std::fs::write(directory.path().join("trigger"), "request").unwrap();
        fixture.wait(|frame| frame.contains("Question fixture"));
        fixture.send(b"\rj");
        fixture.send("\x1b[200~ pasted answer 中文\x1b[201~".as_bytes());
        fixture.wait(|frame| frame.contains("pasted answer"));
        fixture.send(b"\r");
        fixture.wait(|frame| {
            !frame.contains("Question fixture")
                && frame
                    .lines()
                    .last()
                    .is_some_and(|hint| hint.starts_with("← Agents · → Monitor"))
                && std::fs::read_to_string(directory.path().join("phase"))
                    .is_ok_and(|phase| phase == "answered")
        });
        fixture.send(b"j");
        fixture.wait(|frame| {
            frame
                .lines()
                .any(|line| line.replace('\u{a0}', " ").trim_end() == "❯ j")
        });
        std::fs::write(directory.path().join("trigger"), "finish").unwrap();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = fixture.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fixture did not exit: {}",
                fixture.frame()
            );
            thread::sleep(Duration::from_millis(15));
        }
    }
}

#[test]
fn input_dialog_pty_preserves_search_and_draft_through_permission_preemption() {
    for (width, height, top) in [(80, 24, 12), (120, 40, 28), (32, 10, 4)] {
        let directory = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::start(
            directory.path(),
            width,
            height,
            "terminal::console::input_dialog_pty::input_dialog_pty_fixture",
        );
        let top = top as usize;
        fixture.wait(|frame| {
            frame
                .lines()
                .nth(2 + input_dialog::INPUT_DIALOG_RULE_ROWS)
                .is_some_and(|line| line.starts_with("Resume Chat"))
        });
        fixture.send(b"/Chat 1\x1b[B\x1b[B\x1b[B");
        fixture.wait(|frame| frame.contains("❯ Chat 11"));
        std::fs::write(directory.path().join("trigger"), "request").unwrap();
        let frame = fixture.wait(|frame| {
            frame
                .lines()
                .nth(top + input_dialog::INPUT_DIALOG_RULE_ROWS)
                .is_some_and(|line| line.starts_with("Permission fixture"))
        });
        assert_eq!(frame.matches("Pinned fixture prompt").count(), 1, "{frame}");
        if top >= 12 {
            assert!(
                !frame
                    .lines()
                    .next()
                    .unwrap()
                    .contains("Pinned fixture prompt"),
                "{frame}"
            );
        }
        // The smallest layout keeps the activity and the blank row above the dialog.
        let last_visible = if top > 4 {
            "Visible reply before dialogs"
        } else {
            "Working fixture"
        };
        assert!(
            frame
                .lines()
                .take(top)
                .any(|line| line.contains(last_visible)),
            "{frame}"
        );
        fixture.send(b"j\r");
        let frame = fixture.wait(|frame| {
            frame.contains("❯ Chat 11")
                && frame.contains("⌕ Chat 1")
                && std::fs::read_to_string(directory.path().join("phase"))
                    .is_ok_and(|phase| phase == "resumed")
        });
        assert!(
            frame
                .lines()
                .nth(if height == 40 { 14 } else { 2 } + input_dialog::INPUT_DIALOG_RULE_ROWS)
                .unwrap()
                .starts_with("Resume Chat")
        );
        fixture.send(b"\r");
        fixture.wait(|frame| frame.contains("Unsent fixture draft"));
        std::fs::write(directory.path().join("trigger"), "finish").unwrap();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = fixture.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fixture did not exit: {}",
                fixture.frame()
            );
            thread::sleep(Duration::from_millis(15));
        }
    }
}

#[test]
#[ignore]
fn plan_approval_pty_fixture() {
    use crate::terminal::{execution::ConsoleExecution, menus::Menus};
    use tickloop_harness::protocol::EventKind;
    use tickloop_harness::protocol::ExecutionMode;
    let directory = PathBuf::from(std::env::var(FIXTURE_DIRECTORY).unwrap());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::submit_plan("plan", "# Controlled PTY plan\nCreate executed.txt only after approval."),
            tickloop_harness::test_support::call("execute", "Write", serde_json::json!({"file_path":"executed.txt","content":"approved implementation"})),
            tickloop_harness::test_support::text("Implementation completed."),
        ]));
        let agent = ConsoleExecution::new_embedded(tickloop_harness::test_support::config(), model.clone());
        let chat = agent.create_chat(directory.clone()).await.unwrap();
        for mode in [ExecutionMode::AcceptEdits, ExecutionMode::Plan] {
            agent.set_execution_mode(chat, mode).await.unwrap();
        }
        let (console, mut input) = Console::start(true).unwrap();
        console.select_chat(Some(chat)).unwrap();
        console.set_prompt_enabled(true).unwrap();
        console.state.lock().unwrap().insert("Unsent PTY draft");
        let mut events = agent.subscribe();
        agent.submit(chat, "Plan the controlled file".into()).await.unwrap();
        let mut menus = Menus::default();
        let mut pending = HashMap::new();
        let mut poll = tokio::time::interval(Duration::from_millis(20));
        loop {
            tokio::select! {
                event = events.recv() => match event.unwrap().kind {
                    EventKind::PermissionRequested { request } => { pending.insert(request.id, crate::terminal::PermissionRequestOwner::new(chat.into(), &request)); }
                    EventKind::Failed(error) => panic!("{error}"),
                    EventKind::Completed { .. } => {
                        assert_eq!(model.requests.lock().unwrap().len(), 3);
                        assert_eq!(agent.snapshot(chat).await.unwrap().policy.mode, ExecutionMode::AcceptEdits);
                        assert_eq!(std::fs::read_to_string(directory.join("executed.txt")).unwrap(), "approved implementation");
                        assert_eq!(console.state.lock().unwrap().editor.draft(), "Unsent PTY draft");
                        console.write("Implementation completed after approval\n").unwrap();
                        std::fs::write(directory.join("phase"), "executed").unwrap();
                        break;
                    }
                    _ => {},
                },
                event = input.recv() => match event {
                    Some(InputEvent::Choice { menu_id, index, comment }) => {
                        assert_eq!(index, 0);
                        let outcome = menus.choose(&agent, &console, menu_id, Some(index), comment).await.unwrap();
                        assert!(outcome.receipt.is_none(), "approval must continue the existing run");
                        pending.clear();
                    }
                    Some(InputEvent::Eof | InputEvent::Error(_)) | None => panic!("PTY input ended early"),
                    _ => {},
                },
                _ = poll.tick() => {
                    menus.refresh_family_permissions(&agent, &console, &[chat.into()], &pending, &HashMap::new()).await.unwrap();
                    if !pending.is_empty() && console.dialog_menu_id().unwrap().is_some() {
                        assert!(!directory.join("executed.txt").exists());
                        assert_eq!(model.requests.lock().unwrap().len(), 1);
                        std::fs::write(directory.join("phase"), "approval").unwrap();
                    }
                },
            }
        }
        agent.shutdown().await;
    });
}

#[test]
fn plan_approval_pty_opens_without_a_command_and_starts_execution_after_approval() {
    for (width, height) in [(80, 24), (120, 40)] {
        let directory = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::start(
            directory.path(),
            width,
            height,
            "terminal::console::input_dialog_pty::plan_approval_pty_fixture",
        );
        let frame = fixture
            .wait(|frame| frame.contains("Plan approval") && frame.contains("Yes, approve · Ask"));
        assert!(frame.contains("Controlled PTY plan"), "{frame}");
        assert!(!directory.path().join("executed.txt").exists());
        fixture.send(b"\r");
        fixture.wait(|_| {
            std::fs::read_to_string(directory.path().join("phase"))
                .is_ok_and(|phase| phase == "executed")
        });
        assert_eq!(
            std::fs::read_to_string(directory.path().join("executed.txt")).unwrap(),
            "approved implementation"
        );
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = fixture.child.try_wait().unwrap() {
                assert!(status.success(), "{}", fixture.frame());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fixture did not exit: {}",
                fixture.frame()
            );
            thread::sleep(Duration::from_millis(15));
        }
    }
}
