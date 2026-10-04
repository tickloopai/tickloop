use super::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Read;

mod screen {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/screen.rs"
    ));
}

const DIRECTORY: &str = "TICKLOOP_DIALOG_HEIGHT_FIXTURE_DIR";
const TIMEOUT: Duration = Duration::from_secs(15);
const FIRST: Uuid = Uuid::from_u128(7300);
const SECOND: Uuid = Uuid::from_u128(7301);
const USER_MENU: Uuid = Uuid::from_u128(7302);
const REQUEST: Uuid = Uuid::from_u128(7303);
const LOGIN: Uuid = Uuid::from_u128(7304);

fn choices(count: usize) -> Menu {
    Menu {
        id: USER_MENU,
        title: "Height picker".into(),
        context: vec!["Changes apply immediately".into()],
        options: (0..count).map(|index| format!("Choice {index}")).collect(),
        selected: 0,
        confirmation: false,
    }
}

#[test]
#[ignore = "owned PTY producer for content-driven dialog geometry"]
fn dialog_height_pty_fixture() {
    let directory = PathBuf::from(std::env::var_os(DIRECTORY).unwrap());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let (console, mut input) = Console::start(true).unwrap();
        console.select_chat(Some(FIRST)).unwrap();
        console.set_prompt_enabled(true).unwrap();
        {
            let mut state = console.state.lock().unwrap();
            state.tick_list.replace(TickGroup::Artifacts, input_dialog_tests::fixture_ticks());
            state.record_input("Pinned height prompt");
            for index in 0..40 { state.scrollback.append(&format!("History row {index}\n")); }
        }
        console.write("Ready for height fixture\n").unwrap();
        std::fs::write(directory.join("ready"), "ready").unwrap();
        let mut poll = tokio::time::interval(Duration::from_millis(20));
        let mut requested = false;
        let mut switched = false;
        let mut completed = false;
        let mut restored = false;
        let mut grown = false;
        loop {
            tokio::select! {
                _ = poll.tick() => {
                    if directory.join("grow").exists() && !grown {
                        console.show_presented_picker_menu(choices(80), PickerKind::Actions, PickerPresentation { hint: "Enter · Esc back".into(), ..Default::default() }).unwrap();
                        grown = true;
                    }
                    if directory.join("request").exists() && !requested {
                        console.set_request_layout(Some(REQUEST)).unwrap();
                        let mut menu = choices(4);
                        menu.id = REQUEST; menu.title = "Question height".into();
                        console.show_question_menu(menu, QuestionMenu { entry_option: None, entry_text: String::new(), multi_select: false, navigable: false }).unwrap();
                        requested = true;
                    }
                    if directory.join("switch-b").exists() && !switched {
                        console.select_chat(Some(SECOND)).unwrap();
                        console.state.lock().unwrap().draw(&mut io::stdout().lock()).unwrap(); switched = true;
                    }
                    if directory.join("complete-login").exists() && !completed {
                        console.complete_login_menu(LOGIN, "Login saved", "Fixture source account").unwrap(); completed = true;
                    }
                    if directory.join("switch-a").exists() && !restored {
                        console.select_chat(Some(FIRST)).unwrap();
                        console.state.lock().unwrap().draw(&mut io::stdout().lock()).unwrap(); restored = true;
                    }
                }
                event = input.recv() => match event {
                    Some(InputEvent::Line(text) | InputEvent::Prompt { text, .. }) => match text.as_str() {
                        "/short" | "/long" => console.show_presented_picker_menu(choices(if text == "/short" { 2 } else { 80 }), PickerKind::Actions, PickerPresentation { hint: "Enter · Esc back".into(), ..Default::default() }).unwrap(),
                        "/login" => console.show_login_menu(Menu { id: LOGIN, title: "Browser height".into(), context: vec!["https://fixture.test/authorize".into(), "Complete authorization".into()], options: vec!["Submit callback".into(), "Cancel login".into()], selected: 0, confirmation: false }, "https://fixture.test/authorize", true).unwrap(),
                        "/exit" => break,
                        _ => panic!("Unexpected fixture input"),
                    },
                    Some(InputEvent::Cancel { menu_id }) => { console.finish_menu_submission(menu_id).unwrap(); console.clear_menu(menu_id).unwrap(); },
                    Some(InputEvent::Choice { menu_id, index, .. }) => {
                        std::fs::write(directory.join("choice"), format!("{menu_id}:{index}")).unwrap();
                        console.continue_menu_submission(menu_id).unwrap();
                    }
                    Some(InputEvent::Eof) | None => break,
                    Some(InputEvent::Error(error)) => panic!("{error}"),
                    _ => {},
                }
            }
        }
    });
}

struct Fixture {
    directory: tempfile::TempDir,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    rows: u16,
}

impl Fixture {
    fn start(renderer: &str, rows: u16) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols: 140,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--ignored",
            "--exact",
            "terminal::console::dialog_height_pty::dialog_height_pty_fixture",
            "--nocapture",
        ]);
        command.env_clear();
        command.env(DIRECTORY, directory.path());
        command.env("HOME", directory.path());
        command.env("TICKLOOP_CONFIG_DIR", directory.path().join("config"));
        command.env(
            "TICKLOOP_AGENT_RUNTIME_DIR",
            directory.path().join("runtime"),
        );
        command.env("TICKLOOP_TMPDIR", directory.path());
        command.env("TICKLOOP_AGENT_TUI", renderer);
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                captured.lock().unwrap().extend_from_slice(&buffer[..count]);
            }
        });
        let fixture = Self {
            directory,
            child,
            master: pair.master,
            writer,
            output,
            rows,
        };
        fixture.wait(|frame| {
            fixture.directory.path().join("ready").exists()
                && frame.contains("Ready for height fixture")
        });
        fixture
    }
    fn send(&mut self, data: &[u8]) {
        self.writer.write_all(data).unwrap();
        self.writer.flush().unwrap();
    }
    fn signal(&self, name: &str) {
        std::fs::write(self.directory.path().join(name), "ready").unwrap();
    }
    fn resize(&mut self, rows: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols: 140,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        self.rows = rows;
    }
    fn wait(&self, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let frame = screen::visible_screen(
                &String::from_utf8_lossy(&self.output.lock().unwrap()),
                140,
                usize::from(self.rows),
            );
            if predicate(&frame) {
                if let Some(root) = std::env::var_os("TICKLOOP_SETTINGS_UX_EVIDENCE_DIR") {
                    static FRAME: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    let root = PathBuf::from(root);
                    std::fs::create_dir_all(&root).unwrap();
                    let name = thread::current()
                        .name()
                        .unwrap_or("dialog-height")
                        .replace(':', "_");
                    let number = FRAME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    std::fs::write(root.join(format!("{name}-{number:04}.txt")), &frame).unwrap();
                }
                return frame;
            }
            assert!(
                Instant::now() < deadline,
                "Height PTY did not reach the expected frame:\n{frame}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn dialog_rows(frame: &str) -> usize {
    let lines = frame.lines().collect::<Vec<_>>();
    let top = lines
        .iter()
        .rposition(|line| !line.is_empty() && line.chars().all(|character| character == '▔'))
        .unwrap();
    lines.len() - top
}

#[test]
fn dialog_height_pty_grows_shrinks_resizes_and_restores_requests_in_both_renderers() {
    for renderer in ["inline", "fullscreen"] {
        let mut fixture = Fixture::start(renderer, 24);
        fixture.send(b"/short\r");
        let short =
            fixture.wait(|frame| frame.contains("Height picker") && frame.contains("Choice 1"));
        assert_eq!(dialog_rows(&short), 6);
        fixture.send(b"j");
        fixture.wait(|frame| {
            frame
                .lines()
                .any(|line| line.starts_with('❯') && line.contains("Choice 1"))
        });
        fixture.signal("grow");
        let grown =
            fixture.wait(|frame| frame.contains("Height picker") && dialog_rows(frame) == 22);
        assert!(grown.contains("Pinned height prompt"));
        assert!(
            grown.contains("│ Ticks"),
            "Tick List in the upper two rows: {grown}"
        );
        assert!(
            grown
                .lines()
                .any(|line| line.starts_with('❯') && line.contains("Choice 1"))
        );
        fixture.signal("request");
        let request = fixture.wait(|frame| frame.contains("Question height"));
        assert_eq!(dialog_rows(&request), 12);
        fixture.send(b"\x1b");
        let restored = fixture
            .wait(|frame| frame.contains("Height picker") && !frame.contains("Question height"));
        assert_eq!(dialog_rows(&restored), 22);
        assert!(
            restored
                .lines()
                .any(|line| line.starts_with('❯') && line.contains("Choice 1"))
        );
        fixture.resize(40);
        fixture.wait(|frame| frame.contains("Height picker") && dialog_rows(frame) == 38);
        fixture.resize(5);
        fixture.wait(|frame| frame.contains("Resize") && frame.contains("Esc"));
        fixture.send(b"\r");
        std::thread::sleep(Duration::from_millis(100));
        assert!(!fixture.directory.path().join("choice").exists());
        fixture.resize(24);
        fixture.wait(|frame| frame.contains("Height picker") && dialog_rows(frame) == 22);
        fixture.send(b"\x1b");
        fixture.wait(|frame| !frame.contains("Height picker") && frame.contains("Ctrl+G editor"));
        fixture.send(b"/short\r");
        let shrunk =
            fixture.wait(|frame| frame.contains("Height picker") && dialog_rows(frame) == 6);
        assert!(shrunk.contains("Fixture Tick 0"));
    }
}

#[test]
fn dialog_height_pty_background_login_retains_other_agent_draft_and_source_field() {
    let mut fixture = Fixture::start("fullscreen", 24);
    fixture.send(b"/login\r");
    fixture.wait(|frame| frame.contains("Browser height"));
    fixture.send(b"\t");
    fixture.send(b"\x1b[200~fixture callback draft\x1b[201~");
    let edited = fixture.wait(|frame| frame.contains("fixture callback draft"));
    assert!(dialog_rows(&edited) < 12);
    fixture.signal("switch-b");
    fixture.wait(|frame| !frame.contains("Browser height") && frame.contains("Ctrl+G editor"));
    fixture.send(b"Other Agent draft");
    fixture.wait(|frame| frame.contains("Other Agent draft"));
    fixture.signal("complete-login");
    std::thread::sleep(Duration::from_millis(100));
    let untouched = fixture.wait(|frame| frame.contains("Other Agent draft"));
    assert!(!untouched.contains("Fixture source account"));
    fixture.signal("switch-a");
    fixture.wait(|frame| {
        frame.contains("Browser height")
            && frame.contains("fixture callback draft")
            && frame.contains("Enter return")
    });
    fixture.send(b"\x1b");
    fixture.wait(|frame| frame.contains("Browser height") && !frame.contains("Esc close field"));
    fixture.send(b"\x1b");
    fixture.wait(|frame| !frame.contains("Browser height") && frame.contains("Ctrl+G editor"));
}
