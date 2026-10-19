use super::*;
use crate::terminal::scrollback::ScrollCheckpoint;
use serde::{Deserialize, Serialize};

const PAUSE_TIMEOUT: Duration = Duration::from_millis(250);
const PAUSE_POLL: Duration = Duration::from_millis(5);

#[derive(Serialize, Deserialize)]
pub(in crate::terminal) struct ConsoleUpgrade {
    current_chat: Option<Uuid>,
    current: ChatInputSnapshot,
    cached: Vec<(Option<Uuid>, ChatInputSnapshot)>,
    scroll: Vec<(Option<Uuid>, ScrollCheckpoint)>,
    overview_input: ChatInputSnapshot,
    overview_source_input: Option<ChatInputSnapshot>,
    overview_source_scroll: Option<ScrollCheckpoint>,
    overview: OverviewCheckpoint,
    retry_of: Option<Uuid>,
    submitted_prompt: Option<SubmittedPrompt>,
    monitor_width: Option<usize>,
    conversation_focused: bool,
    renderer: Renderer,
    theme: ThemeName,
    prefers_reduced_motion: bool,
}

#[derive(Serialize, Deserialize)]
struct ChatInputSnapshot {
    editor: Editor,
    paste_blocks: PasteBlocks,
    restored_contexts: Vec<InputContext>,
    input_stash: Option<InputStash>,
    shell_input: ShellInputState,
    mentions: Vec<AcceptedMention>,
    family_mentions: Vec<super::super::file_mentions::AcceptedFamilyMention>,
    owner: Uuid,
}

impl ChatInputSnapshot {
    fn from_input(input: &ChatInput) -> Self {
        Self {
            editor: input.editor.clone(),
            paste_blocks: input.paste_blocks.clone(),
            restored_contexts: input.restored_contexts.clone(),
            input_stash: input.input_stash.clone(),
            shell_input: input.shell_input.clone(),
            mentions: input.file_mentions.accepted_mentions().cloned().collect(),
            family_mentions: input.file_mentions.accepted_family_mentions().to_vec(),
            owner: input.owner,
        }
    }

    fn into_input(self) -> ChatInput {
        let mut file_mentions = FileMentionState::default();
        file_mentions.restore_accepted(self.editor.draft(), self.mentions);
        file_mentions.restore_family(self.family_mentions);
        ChatInput {
            editor: self.editor,
            paste_blocks: self.paste_blocks,
            restored_contexts: self.restored_contexts,
            input_stash: self.input_stash,
            shell_input: self.shell_input,
            completion: Completion::default(),
            file_mentions,
            owner: self.owner,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if !self.editor.valid_upgrade_state()
            || !self
                .shell_input
                .command()
                .is_char_boundary(self.shell_input.cursor())
            || !self
                .paste_blocks
                .projection(self.editor.draft(), self.editor.cursor(), 80)
                .issues
                .is_empty()
        {
            return Err("The saved View has invalid editor state".into());
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct OverviewCheckpoint {
    selected: Option<OverviewTarget>,
    current_draft: bool,
    folded_groups: HashMap<crate::agent_console::AttentionGroup, bool>,
    opened_groups: HashSet<crate::agent_console::AttentionGroup>,
    offset: usize,
    input_focused: bool,
}

impl ConsoleUpgrade {
    pub(in crate::terminal) fn validate(&self) -> Result<(), String> {
        self.current.validate()?;
        self.overview_input.validate()?;
        for input in self
            .cached
            .iter()
            .map(|(_, input)| input)
            .chain(self.overview_source_input.iter())
        {
            input.validate()?;
        }
        Ok(())
    }
}

pub(in crate::terminal) struct UpgradePause {
    console: Console,
    terminal_released: bool,
    holds_frame: bool,
    renderer: Renderer,
}

impl Drop for UpgradePause {
    fn drop(&mut self) {
        if self.terminal_released {
            let _ = terminal::enable_raw_mode();
            let surface = &self.console._lifetime.surface;
            let _ = surface.take(self.renderer, &mut io::stdout());
        }
        if self.holds_frame
            && let Ok(mut state) = self.console.state.lock()
        {
            state.held_frames = state.held_frames.saturating_sub(1);
            if self.terminal_released {
                state.screen_initialized = false;
                state.rendered_region = None;
            }
            let _ = state.draw(&mut io::stdout().lock());
        }
        self.console
            ._lifetime
            .pause_requested
            .store(false, Ordering::Release);
    }
}

impl UpgradePause {
    pub(in crate::terminal) fn snapshot(&self) -> Result<ConsoleUpgrade, String> {
        let state = self
            .console
            .state
            .lock()
            .map_err(|error| error.to_string())?;
        let current = ChatInputSnapshot {
            editor: state.editor.clone(),
            paste_blocks: state.paste_blocks.clone(),
            restored_contexts: state.restored_contexts.clone(),
            input_stash: state.input_stash.clone(),
            shell_input: state.shell_input.clone(),
            mentions: state.file_mentions.accepted_mentions().cloned().collect(),
            family_mentions: state.file_mentions.accepted_family_mentions().to_vec(),
            owner: state.input_owner,
        };
        let mut scroll = state.pending_upgrade_scroll.clone();
        scroll.extend(
            state
                .view_cache
                .iter()
                .map(|(chat, view)| (*chat, view.scrollback.upgrade_checkpoint())),
        );
        scroll.insert(state.chat, state.scrollback.upgrade_checkpoint());
        Ok(ConsoleUpgrade {
            current_chat: state.chat,
            current,
            cached: state
                .editor_cache
                .iter()
                .map(|(chat, input)| (*chat, ChatInputSnapshot::from_input(input)))
                .collect(),
            scroll: scroll.into_iter().collect(),
            overview_input: ChatInputSnapshot::from_input(&state.overview_input),
            overview_source_input: state
                .overview_source_input
                .as_ref()
                .map(ChatInputSnapshot::from_input),
            overview_source_scroll: state
                .overview_source_view
                .as_ref()
                .map(|view| view.scrollback.upgrade_checkpoint()),
            overview: OverviewCheckpoint {
                selected: state.overview.selected,
                current_draft: state.overview.current_draft,
                folded_groups: state.overview.folded_groups.clone(),
                opened_groups: state.overview.opened_groups.clone(),
                offset: state.overview.offset,
                input_focused: state.overview.input_focused,
            },
            retry_of: state.retry_of,
            submitted_prompt: state.submitted_prompt.clone(),
            monitor_width: state.monitor_width,
            conversation_focused: state.conversation_focused,
            renderer: state.renderer,
            theme: state.display.theme().name(),
            prefers_reduced_motion: state.display.preferences().prefers_reduced_motion,
        })
    }

    pub(in crate::terminal) fn release_terminal(&mut self) -> Result<(), String> {
        self.terminal_released = true;
        release_terminal_surface(&mut io::stdout(), &self.console._lifetime.surface)
            .map_err(|error| error.to_string())
    }
}

impl State {
    fn ready_for_upgrade(&self) -> bool {
        self.interactive
            && self.workspace_trust.is_none()
            && self.menu.is_none()
            && self.question_menu.is_none()
            && self.permissions_surface.is_none()
            && self.history_search.is_none()
            && self.suspended_dialog.is_none()
            && self.artifacts_picker.is_none()
            && self.ticks_panel.is_none()
            && self.conversation_picker.is_none()
            && self.picker_search.is_none()
            && self.menu_comment.is_none()
            && self.menu_submission_pending.is_none()
            && self.branch_submission == BranchSubmission::Idle
            && self.full_screen_attach.is_none()
            && !self.terminal_lent
            && self.transcript.is_none()
            && self.diff_screen.is_none()
            && !self.help_open
            && self.submitted_retry_of.is_none()
            && self.selection.is_none()
            && self.exit_confirmation.is_none()
            && self.escape_confirmation.is_none()
            && !self.monitor_resizing
            && self.held_frames == 0
            && !self.completion.is_open()
            && !self.file_mentions.is_open()
            && self
                .dialog_cache
                .values()
                .all(|(dialog, suspended)| !dialog.blocks_upgrade() && suspended.is_none())
            && self
                .view_cache
                .values()
                .all(|view| view.history_search.is_none())
    }
}

impl Console {
    #[cfg(test)]
    pub(in crate::terminal) fn test_upgrade_snapshot(&self) -> ConsoleUpgrade {
        let renderer = self.state.lock().unwrap().renderer;
        UpgradePause {
            console: self.clone(),
            terminal_released: false,
            holds_frame: false,
            renderer,
        }
        .snapshot()
        .unwrap()
    }
    pub(in crate::terminal) fn start_for_upgrade(
        preferences: DisplayPreferences,
    ) -> Result<(Self, mpsc::Receiver<InputEvent>), String> {
        Self::start_with_startup(true, preferences, None, None, true)
    }

    pub(in crate::terminal) fn finish_upgrade_restore(&self) {
        self._lifetime
            .pause_requested
            .store(false, Ordering::Release);
    }

    pub(in crate::terminal) async fn pause_for_upgrade(
        &self,
    ) -> Result<Option<UpgradePause>, String> {
        if !self
            .state
            .try_lock()
            .is_ok_and(|state| state.ready_for_upgrade())
        {
            return Ok(None);
        }
        let lifetime = &self._lifetime;
        if lifetime.paused.load(Ordering::Acquire) {
            return Ok(None);
        }
        lifetime.pause_requested.store(true, Ordering::Release);
        let mut pause = UpgradePause {
            console: self.clone(),
            terminal_released: false,
            holds_frame: false,
            renderer: renderer::STARTUP_RENDERER,
        };
        let deadline = tokio::time::Instant::now() + PAUSE_TIMEOUT;
        while !lifetime.paused.load(Ordering::Acquire) {
            if tokio::time::Instant::now() >= deadline || lifetime.stop.load(Ordering::Acquire) {
                lifetime.pause_requested.store(false, Ordering::Release);
                return Ok(None);
            }
            tokio::time::sleep(PAUSE_POLL).await;
        }
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.ready_for_upgrade() {
            lifetime.pause_requested.store(false, Ordering::Release);
            return Ok(None);
        }
        state.held_frames += 1;
        pause.holds_frame = true;
        pause.renderer = state.renderer;
        if let Some(history) = &mut state.input_history {
            history.flush();
        }
        Ok(Some(pause))
    }

    pub(in crate::terminal) fn restore_upgrade(&self, saved: ConsoleUpgrade) -> Result<(), String> {
        saved.validate()?;
        self.switch_renderer(saved.renderer)?;
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.chat != saved.current_chat {
            return Err("The View handoff belongs to another Chat".into());
        }
        state.replace_input(saved.current.into_input());
        state.editor_cache = saved
            .cached
            .into_iter()
            .map(|(chat, input)| (chat, input.into_input()))
            .collect();
        state.pending_upgrade_scroll = saved.scroll.into_iter().collect();
        let chat = state.chat;
        if let Some(checkpoint) = state.pending_upgrade_scroll.remove(&chat) {
            state.scrollback.restore_upgrade_checkpoint(checkpoint);
        }
        state.overview_input = saved.overview_input.into_input();
        state.overview_source_input = saved
            .overview_source_input
            .map(ChatInputSnapshot::into_input);
        if let Some(checkpoint) = saved.overview_source_scroll
            && let Some(view) = &mut state.overview_source_view
        {
            view.scrollback.restore_upgrade_checkpoint(checkpoint);
        }
        state.overview.selected = saved.overview.selected;
        state.overview.current_draft = saved.overview.current_draft;
        state.overview.folded_groups = saved.overview.folded_groups;
        state.overview.opened_groups = saved.overview.opened_groups;
        state.overview.offset = saved.overview.offset;
        state.overview.input_focused = saved.overview.input_focused;
        state.retry_of = saved.retry_of;
        state.submitted_prompt = saved.submitted_prompt;
        state.monitor_width = saved.monitor_width;
        state.conversation_focused = saved.conversation_focused;
        state.display.set_theme(saved.theme);
        state.display.set_preferences(DisplayPreferences {
            prefers_reduced_motion: saved.prefers_reduced_motion,
        });
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::super::input_dialog_pty::{FIXTURE_DIRECTORY, Fixture, TIMEOUT};
    use super::*;
    use crate::terminal::view_upgrade::{self, ViewSelection};
    use std::os::unix::fs::PermissionsExt;

    fn selection(chat: Option<Uuid>) -> ViewSelection {
        ViewSelection {
            navigation: crate::agent_console::UpgradeNavigation {
                directory: PathBuf::from("/fixture"),
                filter: tickloop_agent_protocol::AgentFilter::default(),
            },
            chat,
            goal: None,
            overview: false,
            selected_loops: HashMap::new(),
            configuration: serde_json::to_value(
                crate::terminal::new_agent_draft::NewAgentDraft::default(),
            )
            .unwrap(),
        }
    }

    #[test]
    fn view_checkpoint_restores_every_chat_draft_paste_image_stash_cursor_and_scroll() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let console = Console::test_for_chat(first);
        {
            let mut state = console.state.lock().unwrap();
            state.editor.replace_draft("保留草稿".into(), 6);
            state.editor.apply(EditorCommand::Stash);
            let text = "first pasted line\nsecond pasted line\nthird pasted line\nlast line";
            state.editor.replace_draft(text.into(), 5);
            state.paste_blocks.register(text, 0..text.len()).unwrap();
            state.scrollback.append("one\ntwo\nthree\nfour\nfive\n");
            state.scrollback.frame(80, 2);
            state.scrollback.scroll(ScrollAction::First);
        }
        console.select_chat(Some(second)).unwrap();
        let image = tickloop_agent_protocol::Image {
            media_type: tickloop_agent_protocol::ImageMediaType::Png,
            data: "fixture".into(),
            width: 1,
            height: 1,
        };
        {
            let mut state = console.state.lock().unwrap();
            let id = state.paste_blocks.next_id();
            let text = PasteBlocks::image_placeholder(id).unwrap();
            state.editor.replace_draft(text.clone(), 3);
            state
                .paste_blocks
                .register_image(&text, 0..text.len(), id, image.clone())
                .unwrap();
            state.retry_of = Some(Uuid::from_u128(7));
            state.display.set_theme(ThemeName::ClaudeCodeLight);
            state.display.set_preferences(DisplayPreferences {
                prefers_reduced_motion: true,
            });
        }
        let before = console.test_upgrade_snapshot();
        let bytes = serde_json::to_vec(&before).unwrap();
        let saved: ConsoleUpgrade = serde_json::from_slice(&bytes).unwrap();
        let restored = Console::test_for_chat(second);
        restored.restore_upgrade(saved).unwrap();
        assert_eq!(restored.test_draft_images()[0].image, image);
        assert_eq!(restored.state.lock().unwrap().editor.cursor(), 3);
        assert_eq!(
            restored.state.lock().unwrap().display.theme().name(),
            ThemeName::ClaudeCodeLight
        );
        assert!(
            restored
                .state
                .lock()
                .unwrap()
                .display
                .preferences()
                .prefers_reduced_motion
        );
        assert_eq!(
            restored.state.lock().unwrap().retry_of,
            Some(Uuid::from_u128(7))
        );
        restored.select_chat(Some(first)).unwrap();
        let mut state = restored.state.lock().unwrap();
        assert_eq!(state.editor.cursor(), 5);
        assert_eq!(state.paste_blocks.ranges().len(), 1);
        state.editor.apply(EditorCommand::Clear);
        state.editor.apply(EditorCommand::Stash);
        assert_eq!(state.editor.draft(), "保留草稿");
        assert_eq!(state.editor.cursor(), 6);
        assert!(state.pending_upgrade_scroll.contains_key(&Some(first)));
    }

    #[test]
    fn upgrade_waits_for_dialogs_attached_views_and_submitted_input() {
        let console = Console::test_interactive_for_chat(Uuid::new_v4());
        let mut state = console.state.lock().unwrap();
        assert!(state.ready_for_upgrade());
        state.terminal_lent = true;
        assert!(!state.ready_for_upgrade());
        state.terminal_lent = false;
        state.menu = Some(Menu {
            id: Uuid::new_v4(),
            title: "Permission".into(),
            context: vec![],
            options: vec![],
            selected: 0,
            confirmation: true,
        });
        assert!(!state.ready_for_upgrade());
        state.menu = None;
        state.submitted_retry_of = Some(Uuid::new_v4());
        assert!(!state.ready_for_upgrade());
        state.interactive = false;
    }

    #[test]
    #[ignore]
    fn view_upgrade_fixture() {
        let directory = PathBuf::from(std::env::var(FIXTURE_DIRECTORY).unwrap());
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let (console, mut input) = Console::start(true).unwrap();
            let chat = Uuid::from_u128(8123);
            console.select_chat(Some(chat)).unwrap();
            console.set_prompt_enabled(true).unwrap();
            console.state.lock().unwrap().insert("kept draft");
            console.write("OLD VIEW READY\n").unwrap();
            std::fs::write(directory.join("old-pid"), std::process::id().to_string()).unwrap();
            let mut tick = tokio::time::interval(Duration::from_millis(10));
            loop {
                tokio::select! {
                    _ = tick.tick() => if directory.join("upgrade").exists() {
                        let result = view_upgrade::try_reexec(&console, &input, &directory.join("successor"), Vec::new(), selection(Some(chat))).await;
                        if result.is_err() {
                            console.write("PREFLIGHT DEFERRED\n").unwrap();
                            std::fs::write(directory.join("rejected"), b"yes").unwrap();
                            std::fs::remove_file(directory.join("upgrade")).unwrap();
                        }
                    },
                    event = input.recv() => if let Some(InputEvent::Prompt {text, ..}) = event {
                        std::fs::write(directory.join("submitted"), text).unwrap();
                        return;
                    }
                }
            }
        });
    }

    #[test]
    #[ignore]
    fn view_upgrade_preflight_fixture() {
        let directory = PathBuf::from(std::env::var(FIXTURE_DIRECTORY).unwrap());
        let path = std::fs::read_to_string(directory.join("preflight-path")).unwrap();
        view_upgrade::check_handoff(Path::new(&path)).unwrap();
    }

    #[test]
    #[ignore]
    fn view_upgrade_successor_fixture() {
        let directory = PathBuf::from(std::env::var(FIXTURE_DIRECTORY).unwrap());
        let path = std::fs::read_to_string(directory.join("handoff-path")).unwrap();
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        view_upgrade::prepare_handoff(&["--view-handoff".into(), path.clone().into()]).unwrap();
        let saved = view_upgrade::read_handoff(Path::new(&path)).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (console, mut input) =
                    Console::start_for_upgrade(DisplayPreferences::default()).unwrap();
                console.select_chat(saved.selection.chat).unwrap();
                console.set_prompt_enabled(true).unwrap();
                console.restore_upgrade(saved.console).unwrap();
                console.write("NEW VIEW RESTORED\n").unwrap();
                view_upgrade::complete_handoff();
                console.finish_upgrade_restore();
                std::fs::remove_file(path).unwrap();
                std::fs::write(directory.join("new-pid"), std::process::id().to_string()).unwrap();
                loop {
                    if let Some(InputEvent::Prompt { text, .. }) = input.recv().await {
                        std::fs::write(directory.join("submitted"), text).unwrap();
                        return;
                    }
                }
            });
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(libc::STDIN_FILENO, attributes.as_mut_ptr()) },
            0
        );
        let attributes = unsafe { attributes.assume_init() };
        assert_eq!(
            serde_json::json!(attributes.c_iflag),
            original["raw_mode"]["input_flags"]
        );
        assert_eq!(
            serde_json::json!(attributes.c_oflag),
            original["raw_mode"]["output_flags"]
        );
        assert_eq!(
            serde_json::json!(attributes.c_cflag),
            original["raw_mode"]["control_flags"]
        );
        assert_eq!(
            serde_json::json!(attributes.c_lflag),
            original["raw_mode"]["local_flags"]
        );
        assert_eq!(
            serde_json::json!(attributes.c_cc[libc::VMIN]),
            original["raw_mode"]["minimum_read"]
        );
        assert_eq!(
            serde_json::json!(attributes.c_cc[libc::VTIME]),
            original["raw_mode"]["read_timeout"]
        );
        std::fs::write(directory.join("terminal-restored"), b"yes").unwrap();
    }

    fn quoted(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    fn run_upgrade_pty(reject: bool, fragmented_input: bool, control_c: bool) {
        let directory = tempfile::tempdir().unwrap();
        let executable = quoted(&std::env::current_exe().unwrap());
        let root = quoted(directory.path());
        let preflight = if reject {
            "exit 2".into()
        } else {
            format!(
                "printf '%s' \"$2\" > {root}/preflight-path\nexec {executable} --ignored --exact terminal::console::upgrade::tests::view_upgrade_preflight_fixture --nocapture"
            )
        };
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = --check-view-handoff ]; then\n{preflight}\nfi\nwhile [ \"$#\" -gt 0 ]; do\nif [ \"$1\" = --view-handoff ]; then\nprintf '%s' \"$2\" > {root}/handoff-path\nbreak\nfi\nshift\ndone\nprintf activated > {root}/activation\nsleep 0.1\nexec {executable} --ignored --exact terminal::console::upgrade::tests::view_upgrade_successor_fixture --nocapture\n"
        );
        let successor = directory.path().join("successor");
        std::fs::write(&successor, script).unwrap();
        std::fs::set_permissions(&successor, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut fixture = Fixture::start(
            directory.path(),
            100,
            30,
            "terminal::console::upgrade::tests::view_upgrade_fixture",
        );
        fixture.wait(|frame| frame.contains("OLD VIEW READY"));
        if fragmented_input {
            fixture.send(&[0xe4]);
        }
        std::fs::write(directory.path().join("upgrade"), b"yes").unwrap();
        if fragmented_input {
            thread::sleep(Duration::from_millis(350));
            assert!(
                !directory.path().join("activation").exists(),
                "a partial character must defer exec"
            );
            fixture.send(&[0xb8, 0xad]);
        }
        let marker = directory
            .path()
            .join(if reject { "rejected" } else { "activation" });
        let deadline = Instant::now() + TIMEOUT;
        while !marker.exists() {
            assert!(Instant::now() < deadline, "{}", fixture.raw_output());
            thread::sleep(Duration::from_millis(5));
        }
        if control_c {
            fixture.send(b"\x03after cancel");
        } else {
            fixture.send(" 后续".as_bytes());
        }
        let expected = if control_c {
            "after cancel"
        } else if fragmented_input {
            "kept draft中 后续"
        } else {
            "kept draft 后续"
        };
        fixture.wait(|frame| {
            frame.contains(if reject {
                "PREFLIGHT DEFERRED"
            } else {
                "NEW VIEW RESTORED"
            }) && frame.contains(expected)
        });
        fixture.send(b"\r");
        while !directory.path().join("submitted").exists() {
            assert!(Instant::now() < deadline, "{}", fixture.raw_output());
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            std::fs::read_to_string(directory.path().join("submitted")).unwrap(),
            expected
        );
        if !reject {
            assert_eq!(
                std::fs::read(directory.path().join("old-pid")).unwrap(),
                std::fs::read(directory.path().join("new-pid")).unwrap()
            );
            while !directory.path().join("terminal-restored").exists() {
                assert!(Instant::now() < deadline, "{}", fixture.raw_output());
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[test]
    fn view_upgrade_reexec_preserves_the_terminal_draft_and_input_typed_during_activation() {
        run_upgrade_pty(false, false, false);
    }

    #[test]
    fn rejected_view_upgrade_resumes_the_old_reader_and_preserves_the_draft() {
        run_upgrade_pty(true, false, false);
    }

    #[test]
    fn view_upgrade_waits_for_a_fragmented_utf8_character_before_exec() {
        run_upgrade_pty(false, true, false);
    }

    #[test]
    fn view_upgrade_keeps_control_c_as_input_and_restores_terminal_modes_on_exit() {
        run_upgrade_pty(false, false, true);
    }
}
