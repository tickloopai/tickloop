use super::*;
use crate::{
    agent_access::*,
    terminal::{Agent, ConfigTarget, Menus, test_support},
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Read;

mod screen {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/screen.rs"
    ));
}
mod dialog {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/input_dialog.rs"
    ));
}

const DIRECTORY: &str = "TICKLOOP_MODEL_DIALOG_FIXTURE_DIR";
const TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST: Uuid = Uuid::from_u128(7201);

struct ChildGuard(Box<dyn portable_pty::Child + Send + Sync>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "owned PTY producer for model dialog chrome and Agent switching"]
fn model_dialog_pty_fixture() {
    std::thread::Builder::new().stack_size(8 * 1024 * 1024).spawn(|| {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let directory = PathBuf::from(std::env::var_os(DIRECTORY).unwrap()).canonicalize().unwrap();
            let root = directory.join("config");
            crate::model::credentials::save_api_key(&root, "DEEPSEEK_API_KEY", "fixture-key").unwrap();
            let model = crate::model::cards::ModelCatalog::defaults().get("deepseek-api", "deepseek-flash").unwrap();
            let core = crate::runtime::Agent::new_embedded(model.clone(), Arc::new(test_support::ScriptedModel::new(Vec::new())));
            let access = Arc::new(EmbeddedAgentAccess::new(core.clone()));
            let mut launch = LaunchSpec::embedded(directory.clone());
            launch.model = Some(model);
            launch.config_root = Some(root.clone());
            launch.session_persistence = false;
            let agent = Agent::from_access_for_test(access.clone(), access.daemon_epoch(), ViewRef { session_id: Uuid::new_v4(), view_id: Uuid::new_v4() }, launch, None);
            let first = agent.create_chat(directory.clone()).await.unwrap();
            let second = agent.create_chat(directory.clone()).await.unwrap();
            let (console, mut input) = Console::start(true).unwrap();
            let theme = if std::env::var("TICKLOOP_MODEL_DIALOG_THEME").as_deref() == Ok("light") { ThemeName::ClaudeCodeLight } else { ThemeName::ClaudeCodeDark };
            console.set_theme(theme).unwrap();
            console.select_chat(Some(first)).unwrap();
            console.set_prompt_enabled(true).unwrap();
            console.record_input("Pinned model dialog fixture").unwrap();
            console.state.lock().unwrap().insert("Unsent fixture draft");
            let mut menus = Menus::new(Some(crate::settings::UserSettings::in_root(&root)), Some(root));
            menus.models(&agent, &console, ConfigTarget::Agent(first)).await.unwrap();
            let model_menu = console.dialog_menu_id().unwrap().unwrap();
            let chrome = |phase: &str| {
                let mut state = console.state.lock().unwrap();
                state.draw(&mut io::stdout().lock()).unwrap();
                let lines = state.styled_frame().0;
                assert_eq!(lines.len(), state.input_dialog_height());
                let rule_role = if phase == "plan" { Role::PlanMode } else { Role::Permission };
                assert_eq!(lines[0].spans[0].role, Some(rule_role));
                assert_eq!(lines[0].resolve(state.display.theme(), state.width).spans[0].style, state.display.theme().style(rule_role));
                assert_eq!(lines[1].spans[0].role, Some(Role::DialogTitle));
                let selected = lines.iter().find(|line| line.fill == Some(Role::Selection)).unwrap();
                assert!(selected.text().starts_with(state.display.glyph(Glyph::Prompt)));
                assert_eq!(selected.spans[0].role, Some(Role::Text));
                assert_eq!(lines.last().unwrap().spans[0].role, Some(Role::MutedText));
                std::fs::write(directory.join("phase"), phase).unwrap();
            };
            chrome("model");
            let mut previous = String::new();
            let mut poll = tokio::time::interval(Duration::from_millis(20));
            loop {
                tokio::select! {
                    event = input.recv() => match event {
                        Some(InputEvent::Choice { menu_id, index, .. }) if menu_id == REQUEST => {
                            assert_eq!(index, 0);
                            console.finish_menu_submission(REQUEST).unwrap();
                            console.clear_menu(REQUEST).unwrap();
                            console.state.lock().unwrap().release_finished_submissions();
                            assert_eq!(console.dialog_menu_id().unwrap(), Some(model_menu));
                            chrome("resumed");
                        }
                        Some(InputEvent::Eof | InputEvent::Error(_)) | None => panic!("fixture input ended early"),
                        _ => {}
                    },
                    _ = poll.tick() => {
                        let trigger = std::fs::read_to_string(directory.join("trigger")).unwrap_or_default();
                        if trigger == previous { continue; }
                        previous = trigger.clone();
                        match trigger.as_str() {
                            "question" | "permission" | "plan" => {
                                console.set_request_layout(Some(REQUEST)).unwrap();
                                let question = trigger == "question";
                                let menu = Menu { id: REQUEST, title: if question { "Question fixture" } else if trigger == "plan" { "Plan fixture" } else { "Permission fixture" }.into(), context: vec!["Controlled request".into()], options: vec!["Allow fixture".into(), "Deny fixture".into()], selected: 0, confirmation: !question };
                                if question {
                                    console.show_question_menu(menu, QuestionMenu { entry_option: None, entry_text: String::new(), multi_select: false, navigable: false }).unwrap();
                                } else if trigger == "plan" { console.show_plan_menu(menu, false).unwrap(); }
                                else { console.show_focused_menu(menu).unwrap(); }
                                chrome(&trigger);
                            }
                            "switch" => {
                                console.select_chat(Some(second)).unwrap();
                                menus.sync_dialog_chat(&console).unwrap();
                                menus.providers(&agent, &console, ConfigTarget::Agent(second)).await.unwrap();
                                chrome("second");
                            }
                            "return" => {
                                console.select_chat(Some(first)).unwrap();
                                menus.sync_dialog_chat(&console).unwrap();
                                assert_eq!(console.dialog_menu_id().unwrap(), Some(model_menu));
                                chrome("restored");
                            }
                            "finish" => break,
                            _ => {}
                        }
                    }
                }
            }
            drop(console);
            core.shutdown().await;
        });
    }).unwrap().join().unwrap();
}

#[test]
fn model_dialog_pty_matches_question_permission_chrome_and_restores_agent_picker() {
    for (width, height, theme) in [(80, 24), (120, 40), (32, 10)]
        .into_iter()
        .flat_map(|(width, height)| ["dark", "light"].map(|theme| (width, height, theme)))
    {
        let directory = tempfile::tempdir().unwrap();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: height,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--ignored",
            "--exact",
            "terminal::console::model_dialog_pty::model_dialog_pty_fixture",
            "--nocapture",
        ]);
        command.env_clear();
        command.env(DIRECTORY, directory.path());
        command.env("HOME", directory.path());
        command.env("TICKLOOP_CONFIG_DIR", directory.path().join("config"));
        command.env(
            "TICKLOOP_DAEMON_RUNTIME_DIR",
            directory.path().join("runtime"),
        );
        command.env("TICKLOOP_TMPDIR", directory.path());
        command.env("TICKLOOP_AGENT_TUI", "fullscreen");
        command.env("TICKLOOP_MODEL_DIALOG_THEME", theme);
        command.env("TERM", "xterm-256color");
        command.cwd(directory.path());
        let mut child = ChildGuard(pair.slave.spawn_command(command).unwrap());
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
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
        let wait = |phase: &str, title: &str| {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                let frame = screen::visible_screen(
                    &String::from_utf8_lossy(&output.lock().unwrap()),
                    width as usize,
                    height as usize,
                );
                if std::fs::read_to_string(directory.path().join("phase"))
                    .is_ok_and(|current| current == phase)
                    && frame.contains(title)
                {
                    dialog::assert_frame(&frame, height as usize, title);
                    dialog::record_frame(&format!("{theme}-{width}x{height}-{phase}"), &frame);
                    return frame;
                }
                assert!(
                    Instant::now() < deadline,
                    "PTY missing {phase}: {frame}\nProducer: {}\nOutput: {}",
                    std::fs::read_to_string(directory.path().join("phase")).unwrap_or_default(),
                    String::from_utf8_lossy(&output.lock().unwrap())
                );
                thread::sleep(Duration::from_millis(15));
            }
        };
        let original = wait("model", "Choose model");
        let dialog_top = original
            .lines()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|line| !line.is_empty() && line.chars().all(|character| character == '▔'))
            .unwrap();
        let selected_row = |frame: &str| {
            frame
                .lines()
                .skip(dialog_top)
                .find(|line| line.starts_with('❯'))
                .unwrap()
                .to_owned()
        };
        let initial_selection = selected_row(&original);
        for (keys, initial) in [
            (b"j".as_slice(), false),
            (b"\x10".as_slice(), true),
            (b"\x0e".as_slice(), false),
            (b"k".as_slice(), true),
        ] {
            writer.write_all(keys).unwrap();
            writer.flush().unwrap();
            let deadline = Instant::now() + TIMEOUT;
            loop {
                let frame = screen::visible_screen(
                    &String::from_utf8_lossy(&output.lock().unwrap()),
                    width as usize,
                    height as usize,
                );
                if (selected_row(&frame) == initial_selection) == initial {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "Model navigation must change the visible selection: {frame}"
                );
                thread::sleep(Duration::from_millis(15));
            }
        }
        for phase in ["question", "permission", "plan"] {
            std::fs::write(directory.path().join("trigger"), phase).unwrap();
            wait(
                phase,
                if phase == "question" {
                    "Question fixture"
                } else if phase == "plan" {
                    "Plan fixture"
                } else {
                    "Permission fixture"
                },
            );
            writer.write_all(b"j\x10\x0ek\r").unwrap();
            writer.flush().unwrap();
            let restored = wait("resumed", "Choose model");
            assert_eq!(
                original.lines().skip(dialog_top).collect::<Vec<_>>(),
                restored.lines().skip(dialog_top).collect::<Vec<_>>()
            );
        }
        std::fs::write(directory.path().join("trigger"), "switch").unwrap();
        wait("second", "Choose Provider");
        std::fs::write(directory.path().join("trigger"), "return").unwrap();
        let restored = wait("restored", "Choose model");
        assert_eq!(original, restored);
        std::fs::write(directory.path().join("trigger"), "finish").unwrap();
        let deadline = Instant::now() + TIMEOUT;
        while child.0.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                panic!("fixture did not exit");
            }
            thread::sleep(Duration::from_millis(15));
        }
        assert!(child.0.wait().unwrap().success());
    }
}
