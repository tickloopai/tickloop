use super::*;
use crate::terminal::monitor::{MonitorEntry, MonitorTarget, TickGroup};
use crate::terminal::monitor_cards::{
    CardDemand, CardFrame, CardOrigin, CardState, FOCUSED_CARD_HINT,
};

fn shell_entry(title: &str) -> MonitorEntry {
    let instance = tick_protocol::InstanceRef::shell(Uuid::new_v4());
    MonitorEntry {
        instance: instance.clone(),
        group: TickGroup::Shells,
        glyph: Glyph::ShellTick,
        title: title.into(),
        kind: "Shell".into(),
        status: String::new(),
        notes: Vec::new(),
        subscription: None,
        owner: crate::monitor_rows::TickOwner::Unknown,
        tone: crate::monitor_rows::StatusTone::Live,
        updated_at_ms: None,
        update: None,
        ended: None,
        resources: None,
        actions: vec![crate::monitor_rows::MonitorAction {
            key: crate::monitor_rows::MONITOR_ENTER_KEY,
            label: "open",
            intent: crate::monitor_rows::TickIntent::OpenView(instance),
        }],
    }
}

/// A View whose focused Monitor lists two Shells with live screens, the first selected.
fn cards_fixture() -> State {
    let mut state = State::new(true, 140, 40);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state.monitor.replace(
        TickGroup::Shells,
        vec![shell_entry("first shell"), shell_entry("second shell")],
    );
    state.draw_frame(&mut Vec::new()).unwrap();
    state.monitor.focus(false, None);
    state.draw_frame(&mut Vec::new()).unwrap();
    state
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn selected(state: &State) -> tick_protocol::InstanceRef {
    state.monitor.selected().unwrap().instance.clone()
}

fn attaches(event: Option<InputEvent>, chat: Uuid, instance: &tick_protocol::InstanceRef) {
    match event {
        Some(InputEvent::TickAction {
            chat: acted,
            action,
        }) => {
            assert_eq!(acted, chat);
            assert_eq!(action.instance, instance.to_string());
            assert_eq!(action.key, crate::monitor_rows::MONITOR_ENTER_KEY);
        }
        other => panic!("expected the card's attach, got {other:?}"),
    }
}

fn screen(state: &State) -> String {
    state.rendered_screen.join("\n")
}

/// The moment every [`settle_signals`] of a test happens at, so the selection's preview never
/// falls due there; the tests of the preview move their own clock on.
static SETTLED_AT: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

/// Settles what cards settle between keys, as the idle input loop does.
fn settle_signals(state: &mut State) -> (bool, Vec<InputEvent>) {
    state.settle_card_signals(*SETTLED_AT)
}

#[test]
fn space_shows_and_hides_a_card_and_the_focus_stays_on_the_list() {
    let mut state = cards_fixture();
    let chat = state.chat.unwrap();
    let first = selected(&state);
    let title = state.monitor.selected().unwrap().title.clone();
    assert!(state.monitor_list_hint().contains("space show"));
    attaches(state.key(key(KeyCode::Char(' '))), chat, &first);
    assert!(state.cards.is_open(&first));
    assert!(!state.card_focused() && state.monitor.focused);
    state.draw_frame(&mut Vec::new()).unwrap();
    let shown = screen(&state);
    assert!(
        shown.contains(&format!("$ {title}")),
        "the row keeps its own glyph: {shown}"
    );
    assert!(!shown.contains('▾') && !shown.contains('▸'), "{shown}");
    assert!(shown.contains("── opening ──"), "{shown}");
    assert!(state.monitor_list_hint().contains("space hide"));

    state.key(key(KeyCode::Char('j')));
    let second = selected(&state);
    let second_title = state.monitor.selected().unwrap().title.clone();
    assert_ne!(second, first);
    attaches(state.key(key(KeyCode::Char(' '))), chat, &second);
    state.draw_frame(&mut Vec::new()).unwrap();
    assert_eq!(state.cards.cards().len(), 2, "several cards stay open");

    assert!(state.key(key(KeyCode::Char(' '))).is_none());
    assert!(!state.cards.is_open(&second) && state.cards.is_open(&first));
    state.draw_frame(&mut Vec::new()).unwrap();
    let shown = screen(&state);
    assert!(shown.contains(&format!("$ {second_title}")), "{shown}");
    assert_eq!(
        shown.matches("── opening ──").count(),
        1,
        "only the first card shows: {shown}"
    );
}

#[test]
fn enter_shows_and_focuses_a_card_and_a_lone_escape_leaves_a_read_only_one_open() {
    let mut state = cards_fixture();
    let chat = state.chat.unwrap();
    let first = selected(&state);
    attaches(state.key(key(KeyCode::Enter)), chat, &first);
    assert!(state.card_focused());
    state.draw_frame(&mut Vec::new()).unwrap();
    let shown = screen(&state);
    assert!(shown.contains("input"), "{shown}");
    assert!(
        shown.contains("━━"),
        "the focused card's rule is heavy: {shown}"
    );
    assert_eq!(
        state.monitor_list_hint(),
        "esc back to list",
        "a Shell's card is read-only, so only leaving is hinted"
    );

    let start = Instant::now();
    state.card_input(b"\x1b", start, &mut Vec::new());
    assert!(state.card_focused(), "an Esc waits for what follows");
    assert!(state.expire_card_keys(start + Duration::from_millis(60)));
    assert!(!state.card_focused(), "a lone Esc leaves a read-only card");
    assert!(state.cards.is_open(&first), "the card stays shown");
    assert_eq!(selected(&state), first, "the selection stays on the Tick");
    assert!(state.monitor.focused);

    assert!(
        state.key(key(KeyCode::Enter)).is_none(),
        "entering an open card attaches nothing new"
    );
    assert!(state.card_focused());
    state.card_key(key(KeyCode::Esc));
    assert!(
        !state.card_focused(),
        "an Esc decoded before the raw input took over leaves too"
    );
}

#[test]
fn keys_and_clicks_on_a_focused_card_never_reach_the_console() {
    let mut state = cards_fixture();
    let first = selected(&state);
    state.key(key(KeyCode::Enter));
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(
        state
            .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .is_none()
    );
    assert!(state.card_focused(), "Ctrl+C belongs to the Tick");
    assert!(state.key(key(KeyCode::Char('x'))).is_none());
    assert!(state.monitor_confirmation.is_none(), "x is the Tick's key");
    assert!(state.editor.draft().is_empty());

    let rule = state
        .monitor_row_targets
        .iter()
        .find(|target| {
            matches!(&target.target, MonitorTarget::Card { instance, row: Some(0) } if instance == &first)
        })
        .map(|target| (target.row, target.columns.start + 1))
        .unwrap();
    state.card_input(
        format!("\x1b[<0;{};{}M", rule.1 + 3, rule.0 + 1).as_bytes(),
        Instant::now(),
        &mut Vec::new(),
    );
    assert!(
        state.card_focused(),
        "a click inside the card keeps the focus"
    );
    state.card_input(b"\x1b[<0;2;2M", Instant::now(), &mut Vec::new());
    assert!(!state.card_focused(), "a click outside leaves it");
}

#[test]
fn focus_reports_read_by_a_focused_card_reach_the_console() {
    let mut state = cards_fixture();
    state.key(key(KeyCode::Enter));
    state.file_index_focused = true;
    state.card_input(b"\x1b[O", Instant::now(), &mut Vec::new());
    assert!(
        !state.file_index_focused,
        "the Console lost the terminal's focus"
    );
    state.card_input(b"\x1b[I", Instant::now(), &mut Vec::new());
    assert!(state.file_index_focused && state.card_focused());
}

#[test]
fn a_row_click_toggles_its_card_on_release_a_card_click_enters_and_a_drag_does_neither() {
    let mut state = cards_fixture();
    let chat = state.chat.unwrap();
    let first = selected(&state);
    let row = state
        .monitor_row_targets
        .iter()
        .find(|target| target.target == MonitorTarget::Tick(first.clone()))
        .map(|target| (target.row, target.columns.start + 2))
        .unwrap();
    let mouse = |state: &mut State, kind, (row, column): (usize, usize)| {
        state.pointer(
            MouseEvent {
                kind,
                row: row as u16,
                column: column as u16,
                modifiers: KeyModifiers::NONE,
            },
            &mut Vec::new(),
        )
    };
    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), row);
    assert!(state.pointer_input.is_none(), "nothing acts on the press");
    mouse(
        &mut state,
        MouseEventKind::Drag(MouseButton::Left),
        (row.0 + 1, row.1),
    );
    mouse(
        &mut state,
        MouseEventKind::Up(MouseButton::Left),
        (row.0 + 1, row.1),
    );
    assert!(state.pointer_input.is_none() && !state.cards.is_open(&first));

    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), row);
    mouse(&mut state, MouseEventKind::Up(MouseButton::Left), row);
    attaches(state.pointer_input.take(), chat, &first);
    assert!(state.cards.is_open(&first) && !state.card_focused());
    state.draw_frame(&mut Vec::new()).unwrap();

    let card = state
        .monitor_row_targets
        .iter()
        .find(|target| matches!(&target.target, MonitorTarget::Card { row: Some(1), .. }))
        .map(|target| (target.row, target.columns.start + 2))
        .unwrap();
    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), card);
    mouse(&mut state, MouseEventKind::Up(MouseButton::Left), card);
    assert!(
        state.pointer_input.take().is_none(),
        "the card is attaching already"
    );
    assert_eq!(
        state.cards.focused(),
        Some(&first),
        "a click in the card enters it"
    );

    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), row);
    mouse(&mut state, MouseEventKind::Up(MouseButton::Left), row);
    assert!(
        state.cards.is_open(&first),
        "the focused card reads clicks itself"
    );
}

#[test]
fn a_dialog_over_the_input_takes_the_focus_from_a_card() {
    let mut state = cards_fixture();
    state.key(key(KeyCode::Enter));
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(state.card_focused());
    state.help_open = true;
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(!state.card_focused(), "keys go to what covers Monitor");
}

/// The focused card's row is in the accent colour on the selection background and ends with
/// `input`, its rule heavy; an unfocused card keeps a quiet row and a light rule. The Tick's
/// screen keeps its own colours.
#[test]
fn a_focused_card_and_an_unfocused_card_render_their_rows_rules_and_screens() {
    use crate::terminal::style::Role;
    let mut monitor = crate::terminal::monitor::Monitor::default();
    let entry = shell_entry("tail log");
    let instance = entry.instance.clone();
    monitor.replace(TickGroup::Shells, vec![entry]);
    monitor.focus(false, None);
    let mut tick = vt100::Parser::new(6, 30, 0);
    tick.process(b"\x1b[32mok\x1b[0m line");
    let frame = |focused: bool| {
        let cards = [CardFrame {
            instance: &instance,
            demand: CardDemand {
                min_rows: 6,
                max_rows: None,
                priority: 1,
            },
            focused,
            screen: Some(tick.screen()),
            note: None,
        }];
        monitor.frame_with_cards(30, 14, DisplayProfile::default(), 0, &cards)
    };
    let line_of = |frame: &crate::terminal::monitor::MonitorFrame, text: &str| {
        frame
            .lines
            .iter()
            .find(|line| line.text().contains(text))
            .cloned()
            .unwrap_or_else(|| panic!("no line with {text:?}"))
    };

    let quiet = frame(false);
    let row = line_of(&quiet, "tail log");
    assert!(row.text().starts_with("$ tail log"), "{:?}", row.text());
    assert!(!row.text().contains("input"));
    assert!(row.spans.iter().all(|span| span.role != Some(Role::Accent)));
    let rule = line_of(&quiet, "───");
    assert_eq!(rule.text(), "─".repeat(30));
    let screen = line_of(&quiet, "ok line");
    assert_eq!(screen.text(), format!("ok line{}", " ".repeat(23)));
    assert_eq!(
        screen.spans[0].literal.unwrap().foreground,
        Some(crate::terminal::style::Colour::Indexed(2))
    );
    assert_eq!(quiet.card_sizes.len(), 1);
    assert_eq!(quiet.card_sizes[0].1.columns, 30);

    let focused = frame(true);
    let row = line_of(&focused, "tail log");
    assert!(row.text().trim_end().ends_with("input"), "{:?}", row.text());
    assert_eq!(row.fill, Some(Role::Selection));
    assert!(row.preserve_selection_colours);
    assert!(
        row.spans
            .iter()
            .filter(|span| !span.text.trim().is_empty())
            .all(|span| span.role == Some(Role::Accent))
    );
    assert_eq!(line_of(&focused, "━").text(), "━".repeat(30));
}

#[test]
fn a_focused_hosted_card_hints_sending_esc() {
    let mut state = cards_fixture();
    let mut hosted = shell_entry("hosted tick");
    let instance = tick_protocol::InstanceRef::hosted(Uuid::new_v4());
    hosted.instance = instance.clone();
    hosted.group = TickGroup::Ticks;
    hosted.actions[0].intent = crate::monitor_rows::TickIntent::OpenView(instance.clone());
    state.monitor.replace(TickGroup::Ticks, vec![hosted]);
    state.monitor.select(&instance);
    state.key(key(KeyCode::Enter));
    assert!(state.card_focused());
    assert_eq!(state.monitor_list_hint(), FOCUSED_CARD_HINT);
}

#[test]
fn a_card_stays_open_while_its_row_is_on_a_page_the_monitor_has_not_loaded() {
    let mut state = State::new(true, 140, 40);
    let chat = Uuid::new_v4();
    state.switch_view_chat(Some(chat));
    let mut status = tickloop_harness::protocol::ChatStatus::of(
        &serde_json::from_value(serde_json::json!({
            "id": chat, "folder":"/fixture", "messages":[],
            "chat_loop":{"id":Uuid::nil(),"state":"Idle"}
        }))
        .unwrap(),
    );
    for index in 0..270 {
        status
            .monitor
            .members
            .push(tickloop_harness::protocol::MonitorMember {
                instance: format!("calendar:{index:03}").parse().unwrap(),
                title: Some(format!("Tick {index:03}")),
                is_terminal: false,
                min_rows: None,
                max_rows: None,
            });
    }
    state.monitor.set_chat_status(Arc::new(status));
    let last: tick_protocol::InstanceRef = "calendar:269".parse().unwrap();
    assert!(
        !state
            .monitor
            .entries()
            .iter()
            .any(|entry| entry.instance == last),
        "the last row's page is not loaded yet"
    );
    state.cards.open(chat, last.clone());
    state.card_sources();
    assert!(state.cards.is_open(&last));
}

#[test]
fn a_frame_that_covers_the_monitor_gives_the_terminal_its_own_keys_back() {
    let mut state = cards_fixture();
    state.key(key(KeyCode::Enter));
    let mut output = Vec::new();
    state.draw_frame(&mut output).unwrap();
    assert!(
        String::from_utf8_lossy(&output).contains("\x1b[>0u"),
        "a focused card turns on legacy keys"
    );
    cover_monitor(&mut state);
    let mut output = Vec::new();
    state.draw_frame(&mut output).unwrap();
    assert!(!state.card_focused());
    assert!(
        String::from_utf8_lossy(&output).contains("\x1b[<u"),
        "the card's keys are popped although the frame shows no Monitor"
    );
}

/// Shows a screen over the whole terminal, which returns from the frame before Monitor.
fn cover_monitor(state: &mut State) {
    state.diff_screen = Some(DiffScreen {
        chat_id: state.chat.unwrap(),
        catalog: ChangeCatalog {
            sources: vec![crate::terminal::diff_catalog::ChangeSource {
                id: ChangeSourceId::Current,
                state: ChangeSourceState::Clean,
                user_prompt: None,
                files: Vec::new(),
            }],
        },
        dialog: DiffDialog::new(),
    });
}

#[test]
fn only_a_card_on_screen_has_its_frames_redraw_the_console() {
    let mut state = cards_fixture();
    state.key(key(KeyCode::Char(' ')));
    assert!(!state.card_in_view(), "not drawn yet");
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(state.card_in_view());
    cover_monitor(&mut state);
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(!state.card_in_view(), "the card is open but not on screen");
}

#[test]
fn keys_decoded_before_the_raw_input_reach_the_tick_as_its_terminal_sends_them() {
    let with = |code, modifiers| KeyEvent::new(code, modifiers);
    let cases: [(KeyEvent, bool, &[u8]); 20] = [
        (key(KeyCode::Char('é')), false, "é".as_bytes()),
        (
            with(KeyCode::Char('c'), KeyModifiers::CONTROL),
            false,
            b"\x03",
        ),
        (with(KeyCode::Char('x'), KeyModifiers::ALT), false, b"\x1bx"),
        (
            with(
                KeyCode::Char('x'),
                KeyModifiers::ALT | KeyModifiers::CONTROL,
            ),
            false,
            b"\x1b\x18",
        ),
        (
            with(KeyCode::Backspace, KeyModifiers::ALT),
            false,
            b"\x1b\x7f",
        ),
        (key(KeyCode::Up), false, b"\x1b[A"),
        (key(KeyCode::Up), true, b"\x1bOA"),
        (key(KeyCode::Home), false, b"\x1b[H"),
        (key(KeyCode::Home), true, b"\x1bOH"),
        (key(KeyCode::End), false, b"\x1b[F"),
        (key(KeyCode::Insert), false, b"\x1b[2~"),
        (key(KeyCode::Delete), false, b"\x1b[3~"),
        (key(KeyCode::PageUp), false, b"\x1b[5~"),
        (key(KeyCode::PageDown), false, b"\x1b[6~"),
        (key(KeyCode::BackTab), false, b"\x1b[Z"),
        (key(KeyCode::F(1)), false, b"\x1bOP"),
        (key(KeyCode::F(4)), false, b"\x1bOS"),
        (key(KeyCode::F(5)), false, b"\x1b[15~"),
        (key(KeyCode::F(11)), false, b"\x1b[23~"),
        (key(KeyCode::F(12)), false, b"\x1b[24~"),
    ];
    for (pressed, application_cursor, expected) in cases {
        assert_eq!(
            super::cards::card_key_bytes(pressed, application_cursor).as_deref(),
            Some(expected),
            "{pressed:?}"
        );
    }
    assert_eq!(
        super::cards::card_key_bytes(key(KeyCode::CapsLock), false),
        None
    );
}

/// A hosted Tick's row with a live screen, its card open on a view whose requests the test reads.
fn attached_hosted_card(
    state: &mut State,
) -> (
    tick_protocol::InstanceRef,
    tokio::sync::mpsc::Receiver<crate::terminal::monitor_cards::CardRequest>,
) {
    let chat = state.chat.unwrap();
    let mut hosted = shell_entry("hosted tick");
    let instance = tick_protocol::InstanceRef::hosted(Uuid::new_v4());
    hosted.instance = instance.clone();
    hosted.group = TickGroup::Ticks;
    hosted.actions[0].intent = crate::monitor_rows::TickIntent::OpenView(instance.clone());
    state.monitor.replace(TickGroup::Ticks, vec![hosted]);
    state.monitor.select(&instance);
    attaches(state.key(key(KeyCode::Char(' '))), chat, &instance);
    let (view, requests) =
        crate::terminal::monitor_cards::CardView::detached(state.card_size(&instance));
    assert!(state.cards.attached(chat, &instance, view));
    state.draw_frame(&mut Vec::new()).unwrap();
    (instance, requests)
}

fn queued_requests(
    requests: &mut tokio::sync::mpsc::Receiver<crate::terminal::monitor_cards::CardRequest>,
) -> Vec<crate::terminal::monitor_cards::CardRequest> {
    std::iter::from_fn(|| requests.try_recv().ok()).collect()
}

#[test]
fn releasing_the_monitor_separator_sends_the_cards_final_size_at_once() {
    use crate::terminal::monitor_cards::CardRequest;
    let mut state = cards_fixture();
    let (_instance, mut requests) = attached_hosted_card(&mut state);
    queued_requests(&mut requests);
    let separator = state.monitor_separator.as_ref().unwrap().0;
    let mouse = |kind, column: usize| MouseEvent {
        kind,
        row: 2,
        column: column as u16,
        modifiers: KeyModifiers::NONE,
    };
    state.pointer(
        mouse(MouseEventKind::Down(MouseButton::Left), separator),
        &mut Vec::new(),
    );
    for column in (separator - 6..separator).rev() {
        state.pointer(
            mouse(MouseEventKind::Drag(MouseButton::Left), column),
            &mut Vec::new(),
        );
        state.draw_frame(&mut Vec::new()).unwrap();
    }
    let dragged = queued_requests(&mut requests);
    assert!(
        dragged
            .iter()
            .all(|request| matches!(request, CardRequest::Resize(_))),
        "the drag paces its resizes in the view: {dragged:?}"
    );
    assert!(
        state.pointer(
            mouse(MouseEventKind::Up(MouseButton::Left), separator - 6),
            &mut Vec::new()
        ),
        "the release draws the final frame"
    );
    state.draw_frame(&mut Vec::new()).unwrap();
    assert_eq!(queued_requests(&mut requests), [CardRequest::FlushResize]);
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(queued_requests(&mut requests).is_empty(), "flushed once");
}

fn card_screen(
    state: &State,
    instance: &tick_protocol::InstanceRef,
) -> Arc<Mutex<crate::terminal::monitor_cards::CardScreen>> {
    match &state.cards.card(instance).unwrap().state {
        crate::terminal::monitor_cards::CardState::Attached(view) => view.screen.clone(),
        _ => panic!("the card's view is attached"),
    }
}

/// The row of `instance` as the Monitor's next listing reports it, changed by `change`.
fn change_row(
    state: &mut State,
    instance: &tick_protocol::InstanceRef,
    change: impl FnOnce(&mut MonitorEntry),
) {
    let mut entry = state
        .monitor
        .entries()
        .iter()
        .find(|entry| &entry.instance == instance)
        .unwrap()
        .clone();
    change(&mut entry);
    state.monitor.replace(TickGroup::Ticks, vec![entry]);
}

#[test]
fn a_card_whose_tick_exited_keeps_its_last_screen_says_how_on_its_rule_and_loses_the_focus() {
    let mut state = cards_fixture();
    let (instance, _requests) = attached_hosted_card(&mut state);
    card_screen(&state, &instance)
        .lock()
        .unwrap()
        .parser
        .process(b"thread 'main' panicked");
    state.key(key(KeyCode::Enter));
    assert!(state.card_focused());
    change_row(&mut state, &instance, |entry| {
        entry.status = "exited 101".into();
        entry.tone = crate::monitor_rows::StatusTone::Neutral;
        entry.ended = Some("exited 101".into());
    });
    state.draw_frame(&mut Vec::new()).unwrap();
    let shown = screen(&state);
    assert!(shown.contains("── exited 101 ──"), "{shown}");
    assert!(
        shown.contains("thread 'main' panicked"),
        "the last screen stays: {shown}"
    );
    assert!(!state.card_focused(), "an ended Tick takes no keys");
    assert!(state.cards.is_open(&instance));
    assert!(state.monitor_list_hint().contains("space hide"));
}

#[test]
fn a_view_that_saw_its_tick_end_says_so_before_the_row_does() {
    let mut state = cards_fixture();
    let (instance, _requests) = attached_hosted_card(&mut state);
    state.key(key(KeyCode::Enter));
    card_screen(&state, &instance).lock().unwrap().ended =
        Some(crate::terminal::monitor_cards::ViewEnd::TickEnded);
    state.draw_frame(&mut Vec::new()).unwrap();
    let shown = screen(&state);
    assert!(shown.contains("── ended ──"), "{shown}");
    assert!(!state.card_focused());

    change_row(&mut state, &instance, |entry| {
        entry.status = "gone".into();
        entry.tone = crate::monitor_rows::StatusTone::Gone;
        entry.ended = Some("ended".into());
        entry.actions = vec![crate::monitor_rows::MonitorAction {
            key: crate::monitor_rows::MONITOR_ENTER_KEY,
            label: "reopen",
            intent: crate::monitor_rows::TickIntent::ReopenHosted(instance.clone()),
        }];
    });
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(screen(&state).contains("── ended ──"), "the row agrees");
    assert!(
        state.monitor_list_hint().contains("space hide"),
        "{}",
        state.monitor_list_hint()
    );
    assert!(state.key(key(KeyCode::Char(' '))).is_none());
    assert!(
        !state.cards.is_open(&instance),
        "Space still hides the card of a Tick that no longer opens a view"
    );
    assert!(
        state.key(key(KeyCode::Char(' '))).is_none() && !state.cards.is_open(&instance),
        "and never shows it again"
    );
}

#[test]
fn a_card_collapsed_for_lack_of_room_keeps_its_rows_glyph_and_says_nothing() {
    let mut monitor = crate::terminal::monitor::Monitor::default();
    let (first, second) = (shell_entry("first log"), shell_entry("second log"));
    let instances = [first.instance.clone(), second.instance.clone()];
    monitor.replace(TickGroup::Shells, vec![first, second]);
    let cards = instances.each_ref().map(|instance| CardFrame {
        instance,
        demand: CardDemand {
            min_rows: 6,
            max_rows: None,
            priority: 1,
        },
        focused: false,
        screen: None,
        note: None,
    });
    let frame = monitor.frame_with_cards(30, 12, DisplayProfile::default(), 0, &cards);
    let texts = frame
        .lines
        .iter()
        .map(|line| line.text())
        .collect::<Vec<_>>();
    assert_eq!(frame.collapsed_cards.len(), 1, "{texts:?}");
    assert!(
        texts.iter().all(|line| !line.contains("collapsed")),
        "neither its row nor a line under the list tells of it: {texts:?}"
    );
    let rows = texts
        .iter()
        .filter(|line| line.starts_with("$ "))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2, "{texts:?}");
    assert!(
        texts
            .iter()
            .all(|line| !line.contains('▸') && !line.contains('▾'))
    );
}

/// One SGR left press, or release, at the 0-based screen `cell`, as the terminal reports it.
fn sgr_left(pressed: bool, (row, column): (usize, usize)) -> Vec<u8> {
    let end = if pressed { 'M' } else { 'm' };
    format!("\x1b[<0;{};{}{end}", column + 1, row + 1).into_bytes()
}

fn target_cell(state: &State, wanted: &MonitorTarget) -> (usize, usize) {
    state
        .monitor_row_targets
        .iter()
        .find(|target| &target.target == wanted)
        .map(|target| (target.row, target.columns.start + 2))
        .unwrap_or_else(|| panic!("{wanted:?} is not on screen"))
}

#[test]
fn a_click_outside_a_focused_card_leaves_it_and_then_acts_where_it_landed() {
    let mut state = cards_fixture();
    let chat = state.chat.unwrap();
    let first = selected(&state);
    state.key(key(KeyCode::Char('j')));
    let second = selected(&state);
    state.key(key(KeyCode::Char('k')));
    attaches(state.key(key(KeyCode::Enter)), chat, &first);
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(state.card_focused());

    let second_row = target_cell(&state, &MonitorTarget::Tick(second.clone()));
    let click = [sgr_left(true, second_row), sgr_left(false, second_row)].concat();
    state.card_input(&click, Instant::now(), &mut Vec::new());
    assert!(!state.card_focused(), "the click leaves the card");
    attaches(state.pointer_input.take(), chat, &second);
    assert!(
        state.cards.is_open(&first) && state.cards.is_open(&second),
        "a click on another row shows its card too"
    );
    assert_eq!(selected(&state), second);
    state.draw_frame(&mut Vec::new()).unwrap();

    state.enter_card(first.clone());
    state.draw_frame(&mut Vec::new()).unwrap();
    let second_card = target_cell(
        &state,
        &MonitorTarget::Card {
            instance: second.clone(),
            row: Some(1),
        },
    );
    state.card_input(
        &sgr_left(true, second_card),
        Instant::now(),
        &mut Vec::new(),
    );
    assert!(!state.card_focused(), "acted on the release");
    state.pointer(
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            row: second_card.0 as u16,
            column: second_card.1 as u16,
            modifiers: KeyModifiers::NONE,
        },
        &mut Vec::new(),
    );
    assert_eq!(
        state.cards.focused(),
        Some(&second),
        "a click in another open card enters it"
    );
    state.draw_frame(&mut Vec::new()).unwrap();

    let first_row = target_cell(&state, &MonitorTarget::Tick(first.clone()));
    let elsewhere = (first_row.0 + 1, first_row.1);
    let drag = [
        sgr_left(true, first_row),
        format!("\x1b[<32;{};{}M", elsewhere.1 + 1, elsewhere.0 + 1).into_bytes(),
        sgr_left(false, elsewhere),
    ]
    .concat();
    state.card_input(&drag, Instant::now(), &mut Vec::new());
    assert!(!state.card_focused());
    assert!(
        state.pointer_input.is_none() && state.cards.is_open(&first),
        "a drag toggles nothing"
    );
}

fn hosted_entry(title: &str) -> MonitorEntry {
    let mut hosted = shell_entry(title);
    let instance = tick_protocol::InstanceRef::hosted(Uuid::new_v4());
    hosted.instance = instance.clone();
    hosted.group = TickGroup::Ticks;
    hosted.actions[0].intent = crate::monitor_rows::TickIntent::OpenView(instance);
    hosted
}

/// A row that can have no card, as an Artifact's.
fn plain_entry(title: &str) -> MonitorEntry {
    let mut plain = shell_entry(title);
    plain.instance = tick_protocol::InstanceRef::new("calendar", title).unwrap();
    plain.group = TickGroup::Artifacts;
    plain.actions[0].intent = crate::monitor_rows::TickIntent::ShowDetails(plain.instance.clone());
    plain
}

/// A View whose focused Monitor lists a row without a card and three hosted Ticks; returns the
/// rows in the list's order, the first selected.
fn tab_fixture() -> (State, Vec<tick_protocol::InstanceRef>) {
    let mut state = State::new(true, 140, 60);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state
        .monitor
        .replace(TickGroup::Artifacts, vec![plain_entry("notes")]);
    state.monitor.replace(
        TickGroup::Ticks,
        vec![
            hosted_entry("one"),
            hosted_entry("two"),
            hosted_entry("three"),
        ],
    );
    state.draw_frame(&mut Vec::new()).unwrap();
    state.monitor.focus(false, None);
    let rows: Vec<_> = state
        .monitor
        .entries()
        .iter()
        .map(|entry| entry.instance.clone())
        .collect();
    state.monitor.select(&rows[0]);
    state.draw_frame(&mut Vec::new()).unwrap();
    (state, rows)
}

fn origin(state: &State, instance: &tick_protocol::InstanceRef) -> Option<CardOrigin> {
    state.cards.card(instance).map(|card| card.origin)
}

/// Presses `code` with `modifiers` at `at`, then settles the selection's preview then, as the
/// input loop does after each key; returns the attach of a card that showed.
fn press_at(
    state: &mut State,
    code: KeyCode,
    modifiers: KeyModifiers,
    at: Instant,
) -> Option<InputEvent> {
    let event = state.key(KeyEvent::new(code, modifiers));
    let (_, attach) = state.settle_selection_preview(at);
    event.or(attach)
}

/// Settles the selection's preview at `at`, as the idle input loop does; returns the attach of a
/// card that showed.
fn rest_until(state: &mut State, at: Instant) -> Option<InputEvent> {
    state.settle_selection_preview(at).1
}

const RESTED: Duration = super::cards::SELECTION_PREVIEW_DELAY;
const QUICKLY: Duration = Duration::from_millis(149);

#[test]
fn every_move_key_previews_the_card_of_the_row_the_selection_rests_on() {
    let (mut state, rows) = tab_fixture();
    let chat = state.chat.unwrap();
    let [notes, one, two, three] = [&rows[0], &rows[1], &rows[2], &rows[3]];
    assert!(state.monitor_list_hint().contains("tab next"));
    let control = KeyModifiers::CONTROL;
    let none = KeyModifiers::NONE;
    let pairs = [
        (
            "j/k",
            (KeyCode::Char('j'), none),
            (KeyCode::Char('k'), none),
        ),
        ("arrows", (KeyCode::Down, none), (KeyCode::Up, none)),
        (
            "Ctrl+N/P",
            (KeyCode::Char('n'), control),
            (KeyCode::Char('p'), control),
        ),
        (
            "Tab",
            (KeyCode::Tab, none),
            (KeyCode::BackTab, KeyModifiers::SHIFT),
        ),
    ];
    let mut clock = Instant::now();
    for (keys, (down, down_modifiers), (up, up_modifiers)) in pairs {
        state.monitor.select(notes);
        assert!(rest_until(&mut state, clock).is_none());
        clock += Duration::from_secs(1);
        assert!(
            rest_until(&mut state, clock).is_none(),
            "{keys}: a row without a card is only selected"
        );
        assert!(state.cards.cards().is_empty(), "{keys}");

        assert!(press_at(&mut state, down, down_modifiers, clock).is_none());
        assert_eq!(selected(&state), *one, "{keys}");
        assert!(rest_until(&mut state, clock + QUICKLY).is_none(), "{keys}");
        assert!(!state.cards.is_open(one), "{keys}: not before it rested");
        assert!(state.monitor_list_hint().contains("space show"), "{keys}");
        attaches(rest_until(&mut state, clock + RESTED), chat, one);
        assert_eq!(origin(&state, one), Some(CardOrigin::Preview), "{keys}");
        assert!(state.monitor_list_hint().contains("space hide"), "{keys}");
        assert!(
            !state.card_focused() && state.monitor.focused,
            "{keys}: the keys stay on the list"
        );

        clock += Duration::from_secs(1);
        assert!(press_at(&mut state, down, down_modifiers, clock).is_none());
        assert!(
            !state.cards.is_open(one),
            "{keys}: the preview closes as the selection moves on"
        );
        assert!(press_at(&mut state, down, down_modifiers, clock + QUICKLY).is_none());
        assert_eq!(selected(&state), *three, "{keys}");
        assert!(
            !state.cards.is_open(two),
            "{keys}: a row passed over attaches no view"
        );
        assert!(rest_until(&mut state, clock + QUICKLY + QUICKLY).is_none());
        attaches(
            rest_until(&mut state, clock + QUICKLY + RESTED),
            chat,
            three,
        );

        clock += Duration::from_secs(1);
        assert!(press_at(&mut state, up, up_modifiers, clock).is_none());
        assert_eq!(selected(&state), *two, "{keys}");
        assert!(!state.cards.is_open(three), "{keys}");
        attaches(rest_until(&mut state, clock + RESTED), chat, two);
        assert_eq!(state.cards.cards().len(), 1, "{keys}");
        clock += Duration::from_secs(1);
    }

    state.monitor.select(three);
    rest_until(&mut state, clock);
    assert!(press_at(&mut state, KeyCode::Tab, none, clock).is_none());
    assert_eq!(
        selected(&state),
        *notes,
        "Tab wraps to a row without a card"
    );
    assert!(rest_until(&mut state, clock + RESTED).is_none());
    assert!(state.cards.cards().is_empty());
    assert!(state.monitor.detail.is_none(), "it only selects that row");
    assert!(press_at(&mut state, KeyCode::BackTab, KeyModifiers::SHIFT, clock).is_none());
    assert_eq!(selected(&state), *three, "Shift+Tab wraps back");
    state.monitor.select(notes);
    rest_until(&mut state, clock);
    assert!(press_at(&mut state, KeyCode::Char('k'), none, clock).is_none());
    assert_eq!(selected(&state), *notes, "k stops at the first row");
}

#[test]
fn the_selection_previews_wherever_it_lands_and_only_while_the_list_has_the_keys() {
    let (mut state, rows) = tab_fixture();
    let chat = state.chat.unwrap();
    let [one, two] = [&rows[1], &rows[2]];
    let start = Instant::now();
    state.monitor.focused = false;
    state.monitor.select(one);
    assert!(rest_until(&mut state, start).is_none());
    assert!(
        rest_until(&mut state, start + RESTED).is_none(),
        "no preview while the input has the keys"
    );
    assert!(press_at(&mut state, KeyCode::Right, KeyModifiers::NONE, start).is_some());
    assert!(state.monitor.focused, "Right gives the list the keys");
    assert!(rest_until(&mut state, start + QUICKLY).is_none());
    attaches(rest_until(&mut state, start + RESTED), chat, one);

    let later = start + Duration::from_secs(1);
    assert!(press_at(&mut state, KeyCode::Esc, KeyModifiers::NONE, later).is_none());
    assert!(!state.monitor.focused);
    assert!(
        !state.cards.is_open(one),
        "the preview closes when the list loses the keys"
    );

    // `n`, an edge line and a bubbled key select a row without a move key.
    state.monitor.focus(false, None);
    state.monitor.select(two);
    assert!(rest_until(&mut state, later).is_none());
    attaches(rest_until(&mut state, later + RESTED), chat, two);
    assert_eq!(origin(&state, two), Some(CardOrigin::Preview));
    assert!(
        rest_until(&mut state, later + RESTED + RESTED).is_none(),
        "it shows once"
    );

    assert!(
        press_at(
            &mut state,
            KeyCode::Enter,
            KeyModifiers::NONE,
            later + RESTED
        )
        .is_none()
    );
    assert!(state.card_focused());
    assert_eq!(
        origin(&state, two),
        Some(CardOrigin::User),
        "entering keeps it"
    );
    assert!(state.cards.previewed().is_none());
    state.leave_card();
    assert!(
        press_at(
            &mut state,
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            later + RESTED
        )
        .is_none()
    );
    assert!(
        state.cards.is_open(two),
        "a kept card stays when the selection moves on"
    );
}

#[test]
fn tab_in_the_input_never_moves_the_monitor() {
    let (mut state, rows) = tab_fixture();
    state.monitor.focused = false;
    state.key(key(KeyCode::Tab));
    state.key(key(KeyCode::BackTab));
    assert_eq!(selected(&state), rows[0]);
    assert!(state.cards.cards().is_empty());
}

/// Opens and attaches the cards of `rows` with the user's Space.
fn attach_cards(state: &mut State, rows: &[tick_protocol::InstanceRef]) {
    let chat = state.chat.unwrap();
    for row in rows {
        state.monitor.select(row);
        attaches(state.key(key(KeyCode::Char(' '))), chat, row);
        let (view, _requests) =
            crate::terminal::monitor_cards::CardView::detached(state.card_size(row));
        view.screen.lock().unwrap().bubbling = true;
        assert!(state.cards.attached(chat, row, view));
    }
    state.draw_frame(&mut Vec::new()).unwrap();
}

fn bubble(
    state: &mut State,
    instance: &tick_protocol::InstanceRef,
    key: tick_protocol::BubbledKey,
) {
    let Some(card) = state.cards.card(instance) else {
        panic!("no card for {instance}");
    };
    let CardState::Attached(view) = &card.state else {
        panic!("{instance} is not attached");
    };
    view.screen.lock().unwrap().bubbled.push(key);
}

#[test]
fn bubbled_keys_leave_the_card_for_the_list_without_entering_another() {
    use tick_protocol::BubbledKey;
    let (mut state, rows) = tab_fixture();
    let chat = state.chat.unwrap();
    let [notes, one, two, three] = [&rows[0], &rows[1], &rows[2], &rows[3]];
    attach_cards(&mut state, &[one.clone(), two.clone()]);
    state.enter_card(one.clone());
    state.draw_frame(&mut Vec::new()).unwrap();
    assert_eq!(
        state.monitor_list_hint(),
        crate::terminal::monitor_cards::FOCUSED_BUBBLING_CARD_HINT
    );

    bubble(&mut state, one, BubbledKey::Down);
    let (changed, inputs) = settle_signals(&mut state);
    assert!(changed && inputs.is_empty());
    assert!(
        !state.card_focused() && state.monitor.focused,
        "down leaves the card for the list"
    );
    assert_eq!(selected(&state), *two, "the next shown card's row");
    assert!(
        state.cards.is_open(one) && state.cards.is_open(two),
        "both cards stay shown"
    );
    assert!(
        rest_until(&mut state, *SETTLED_AT + RESTED).is_none(),
        "an open card attaches nothing more"
    );
    assert_eq!(
        state.cards.previewed(),
        Some(two),
        "the row it landed on previews its card"
    );

    state.enter_card(two.clone());
    state.draw_frame(&mut Vec::new()).unwrap();
    bubble(&mut state, two, BubbledKey::Right);
    bubble(&mut state, two, BubbledKey::Up);
    bubble(&mut state, two, BubbledKey::Down);
    settle_signals(&mut state);
    assert!(!state.card_focused() && state.monitor.focused);
    assert_eq!(
        selected(&state),
        *one,
        "right is ignored, up acts, the rest go"
    );

    state.enter_card(one.clone());
    bubble(&mut state, one, BubbledKey::Up);
    settle_signals(&mut state);
    assert!(
        !state.card_focused() && state.monitor.focused,
        "up past the first: the list"
    );
    assert_eq!(selected(&state), *one);

    state.enter_card(two.clone());
    bubble(&mut state, two, BubbledKey::Down);
    settle_signals(&mut state);
    assert!(
        !state.card_focused() && state.monitor.focused,
        "down past the last shown card: the list"
    );
    assert_eq!(selected(&state), *two);

    state.enter_card(two.clone());
    bubble(&mut state, two, BubbledKey::Esc);
    settle_signals(&mut state);
    assert!(
        !state.card_focused() && state.monitor.focused,
        "esc: the list at once"
    );

    state.enter_card(two.clone());
    bubble(&mut state, two, BubbledKey::Tab);
    let (changed, inputs) = settle_signals(&mut state);
    assert!(changed && inputs.is_empty());
    assert!(
        !state.card_focused() && state.monitor.focused,
        "tab acts as Tab on the list"
    );
    assert_eq!(selected(&state), *three);
    attaches(rest_until(&mut state, *SETTLED_AT + RESTED), chat, three);
    assert_eq!(
        origin(&state, three),
        Some(CardOrigin::Preview),
        "the next row's card is only previewed once the selection rested there"
    );
    assert_eq!(
        origin(&state, two),
        Some(CardOrigin::User),
        "the card left stays"
    );

    state.enter_card(one.clone());
    bubble(&mut state, one, BubbledKey::Backtab);
    let (_, inputs) = settle_signals(&mut state);
    assert!(inputs.is_empty());
    assert!(!state.card_focused() && state.monitor.focused);
    assert_eq!(
        selected(&state),
        *notes,
        "backtab from the first Tick wraps upward"
    );
    assert!(!state.cards.is_open(three), "the preview closes");
    assert!(state.cards.is_open(one));

    state.monitor.overlay = true;
    state.enter_card(two.clone());
    bubble(&mut state, two, BubbledKey::Left);
    settle_signals(&mut state);
    assert!(
        !state.card_focused() && !state.monitor.focused && !state.monitor.overlay,
        "left: the input, closing an overlay Monitor"
    );
    bubble(&mut state, two, BubbledKey::Down);
    let (changed, inputs) = settle_signals(&mut state);
    assert!(
        !changed && inputs.is_empty(),
        "with no card focused nothing bubbles"
    );
}

#[test]
fn esc_esc_on_a_tick_that_bubbles_esc_lands_on_the_list() {
    use tick_protocol::BubbledKey;
    let (mut state, rows) = tab_fixture();
    let one = &rows[1];
    attach_cards(&mut state, std::slice::from_ref(one));
    state.enter_card(one.clone());
    let start = Instant::now();
    state.card_input(b"\x1b", start, &mut Vec::new());
    state.expire_card_keys(start + Duration::from_millis(60));
    assert!(state.card_focused(), "the first Esc went to the Tick");
    bubble(&mut state, one, BubbledKey::Esc);
    settle_signals(&mut state);
    assert!(!state.card_focused() && state.monitor.focused);
    assert!(state.key(key(KeyCode::Esc)).is_none());
    assert!(
        state.monitor.focused,
        "the second Esc of the pair is the card's, not the list's"
    );
    assert!(state.key(key(KeyCode::Esc)).is_none());
    assert!(!state.monitor.focused, "a later Esc leaves the list");
}

#[test]
fn the_agents_use_shows_a_card_without_moving_the_focus_unless_the_user_hid_it() {
    let (mut state, rows) = tab_fixture();
    let chat = state.chat.unwrap();
    let one = &rows[1];
    state.monitor.focused = false;
    state.monitor.flash_use(rows[0].clone(), 1);
    state.monitor.flash_use(one.clone(), 1);
    let (changed, inputs) = settle_signals(&mut state);
    assert!(changed);
    assert_eq!(inputs.len(), 1, "only the row with a card opens one");
    attaches(inputs.into_iter().next(), chat, one);
    assert_eq!(origin(&state, one), Some(CardOrigin::Agent));
    assert!(
        !state.card_focused() && !state.monitor.focused,
        "the focus stays"
    );
    assert_eq!(selected(&state), rows[0], "the selection stays");

    state.monitor.focus(false, None);
    state.monitor.select(one);
    assert!(state.monitor_list_hint().contains("space hide"));
    assert!(state.key(key(KeyCode::Char(' '))).is_none());
    assert!(!state.cards.is_open(one));
    state.monitor.flash_use(one.clone(), 2);
    let (changed, inputs) = settle_signals(&mut state);
    assert!(
        !changed && inputs.is_empty(),
        "a card the user hid stays hidden"
    );
    attaches(state.key(key(KeyCode::Char(' '))), chat, one);
    state.cards.close(one);
    state.monitor.flash_use(one.clone(), 3);
    let (_, inputs) = settle_signals(&mut state);
    attaches(inputs.into_iter().next(), chat, one);
}

/// A View of a Chat whose Monitor lists `count` hosted Ticks it opened, more than one page holds.
fn many_hosted(count: usize) -> (State, Vec<tick_protocol::InstanceRef>) {
    let chat = Uuid::new_v4();
    let mut state = State::new(true, 140, 40);
    state.switch_view_chat(Some(chat));
    state.prompt_enabled = true;
    let mut status = tickloop_harness::protocol::ChatStatus::of(
        &serde_json::from_value(serde_json::json!({
            "id": chat, "folder":"/fixture", "messages":[],
            "chat_loop":{"id":Uuid::nil(),"state":"Idle"}
        }))
        .unwrap(),
    );
    for _ in 0..count {
        status.monitor.record_opened(
            tick_protocol::InstanceRef::hosted(Uuid::new_v4()),
            Some("Tick".into()),
        );
    }
    state.monitor.set_chat_status(Arc::new(status));
    state.draw_frame(&mut Vec::new()).unwrap();
    let listed = state
        .monitor
        .chat_status
        .as_ref()
        .unwrap()
        .monitor
        .members
        .iter()
        .map(|member| member.instance.clone())
        .collect();
    (state, listed)
}

#[test]
fn the_agents_use_shows_a_card_whose_row_is_not_loaded_or_not_listed_yet() {
    let (mut state, members) = many_hosted(400);
    let chat = state.chat.unwrap();
    let unloaded = members
        .iter()
        .find(|instance| {
            !state
                .monitor
                .entries()
                .iter()
                .any(|entry| &entry.instance == *instance)
        })
        .expect("a row on a page not loaded yet")
        .clone();
    state.monitor.flash_use(unloaded.clone(), 1);
    let (_, inputs) = settle_signals(&mut state);
    attaches(inputs.into_iter().next(), chat, &unloaded);
    assert_eq!(origin(&state, &unloaded), Some(CardOrigin::Agent));

    let arriving = tick_protocol::InstanceRef::hosted(Uuid::new_v4());
    state.monitor.flash_use(arriving.clone(), 2);
    let (_, inputs) = settle_signals(&mut state);
    assert!(inputs.is_empty(), "no row yet");
    let mut status = (**state.monitor.chat_status.as_ref().unwrap()).clone();
    status
        .monitor
        .record_opened(arriving.clone(), Some("Late".into()));
    state.monitor.set_chat_status(Arc::new(status));
    let (_, inputs) = settle_signals(&mut state);
    attaches(inputs.into_iter().next(), chat, &arriving);
}

/// The focus requests among what a card's view asked of the Tickhost.
fn focus_requests(
    requests: &mut tokio::sync::mpsc::Receiver<crate::terminal::monitor_cards::CardRequest>,
) -> Vec<bool> {
    queued_requests(requests)
        .into_iter()
        .filter_map(|request| match request {
            crate::terminal::monitor_cards::CardRequest::Focus(focused) => Some(focused),
            _ => None,
        })
        .collect()
}

fn wants_focus_reports(state: &State, instance: &tick_protocol::InstanceRef) {
    let Some(card) = state.cards.card(instance) else {
        panic!("no card for {instance}");
    };
    let CardState::Attached(view) = &card.state else {
        panic!("{instance} is not attached");
    };
    let mut screen = view.screen.lock().unwrap();
    screen.focus_reports = true;
    screen.focus_reports_announced = true;
}

#[test]
fn a_card_tells_its_tick_it_has_the_keyboard_only_when_the_tick_wants_focus_reports() {
    let mut state = cards_fixture();
    let (instance, mut requests) = attached_hosted_card(&mut state);
    state.enter_card(instance.clone());
    state.leave_card();
    assert!(
        focus_requests(&mut requests).is_empty(),
        "a Tick that asked for no focus reports hears nothing"
    );

    state.enter_card(instance.clone());
    wants_focus_reports(&state, &instance);
    settle_signals(&mut state);
    assert_eq!(
        focus_requests(&mut requests),
        [true],
        "asking while its card is focused tells it so"
    );
    let start = Instant::now();
    state.card_input(b"\x1b\x1b", start, &mut Vec::new());
    state.expire_card_keys(start + Duration::from_millis(60));
    assert!(!state.card_focused());
    assert_eq!(focus_requests(&mut requests), [false], "Esc Esc");

    state.enter_card(instance.clone());
    state.card_input(b"\x1b[<0;2;2M", Instant::now(), &mut Vec::new());
    assert_eq!(
        focus_requests(&mut requests),
        [true, false],
        "a click outside"
    );

    state.enter_card(instance.clone());
    state.cards.close(&instance);
    assert_eq!(
        focus_requests(&mut requests),
        [true, false],
        "closing the focused card, as when its row leaves"
    );
}

#[test]
fn leaving_a_card_for_the_list_and_switching_chats_tell_each_tick_its_focus() {
    use tick_protocol::BubbledKey;
    let (mut state, rows) = tab_fixture();
    let [one, two] = [&rows[1], &rows[2]];
    let chat = state.chat.unwrap();
    let mut queues = Vec::new();
    for row in [one, two] {
        state.monitor.select(row);
        attaches(state.key(key(KeyCode::Char(' '))), chat, row);
        let (view, requests) =
            crate::terminal::monitor_cards::CardView::detached(state.card_size(row));
        view.screen.lock().unwrap().bubbling = true;
        assert!(state.cards.attached(chat, row, view));
        wants_focus_reports(&state, row);
        queues.push(requests);
    }
    settle_signals(&mut state);
    state.draw_frame(&mut Vec::new()).unwrap();
    for key in [BubbledKey::Down, BubbledKey::Tab] {
        state.enter_card(one.clone());
        bubble(&mut state, one, key);
        settle_signals(&mut state);
        assert!(!state.card_focused());
        assert_eq!(selected(&state), *two);
        assert_eq!(focus_requests(&mut queues[0]), [true, false], "{key:?}");
        assert!(
            focus_requests(&mut queues[1]).is_empty(),
            "{key:?} gives no card the keyboard"
        );
    }
    state.enter_card(two.clone());
    assert_eq!(focus_requests(&mut queues[1]), [true]);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.draw_frame(&mut Vec::new()).unwrap();
    assert_eq!(focus_requests(&mut queues[1]), [false], "a Chat switch");
}

/// A View of a Chat whose focused Monitor lists four hosted Ticks; returns them in the list's
/// order, the first selected.
fn four_hosted_fixture() -> (State, Vec<tick_protocol::InstanceRef>) {
    hosted_fixture(60)
}

/// A View `height` rows tall of a Chat whose focused Monitor lists four hosted Ticks; returns
/// them in the list's order, the first selected.
fn hosted_fixture(height: u16) -> (State, Vec<tick_protocol::InstanceRef>) {
    let mut state = State::new(true, 140, height);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state.monitor.replace(
        TickGroup::Ticks,
        ["one", "two", "three", "four"].map(hosted_entry).into(),
    );
    state.draw_frame(&mut Vec::new()).unwrap();
    state.monitor.focus(false, None);
    let rows: Vec<_> = state
        .monitor
        .entries()
        .iter()
        .map(|entry| entry.instance.clone())
        .collect();
    state.monitor.select(&rows[0]);
    state.draw_frame(&mut Vec::new()).unwrap();
    (state, rows)
}

/// The cards whose views the next settle attaches, all for `chat`.
fn queued_attaches(state: &mut State, chat: Uuid) -> Vec<tick_protocol::InstanceRef> {
    let (_, inputs) = settle_signals(state);
    inputs
        .into_iter()
        .map(|input| match input {
            InputEvent::TickAction {
                chat: acted,
                action,
            } => {
                assert_eq!(acted, chat);
                assert_eq!(action.key, crate::monitor_rows::MONITOR_ENTER_KEY);
                action.instance.parse().unwrap()
            }
            other => panic!("expected a card's attach, got {other:?}"),
        })
        .collect()
}

fn show_with_space(state: &mut State, instance: &tick_protocol::InstanceRef) {
    let chat = state.chat.unwrap();
    state.monitor.select(instance);
    attaches(state.key(key(KeyCode::Char(' '))), chat, instance);
}

#[test]
fn switching_chats_away_and_back_restores_the_shown_cards_and_the_hidden_set() {
    let (mut state, rows) = four_hosted_fixture();
    let chat = state.chat.unwrap();
    let [one, two, three, four] = [&rows[0], &rows[1], &rows[2], &rows[3]];
    show_with_space(&mut state, one);
    state.monitor.flash_use(two.clone(), 1);
    assert_eq!(queued_attaches(&mut state, chat), std::slice::from_ref(two));
    show_with_space(&mut state, three);
    assert!(state.key(key(KeyCode::Char(' '))).is_none(), "hides it");
    assert!(press_at(&mut state, KeyCode::Tab, KeyModifiers::NONE, *SETTLED_AT).is_none());
    attaches(rest_until(&mut state, *SETTLED_AT + RESTED), chat, four);
    assert_eq!(origin(&state, four), Some(CardOrigin::Preview));
    for row in [one, two] {
        let (view, _requests) =
            crate::terminal::monitor_cards::CardView::detached(state.card_size(row));
        assert!(state.cards.attached(chat, row, view));
    }
    state.draw_frame(&mut Vec::new()).unwrap();

    let other = Uuid::new_v4();
    state.switch_view_chat(Some(other));
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(state.cards.cards().is_empty(), "another Chat shows none");
    assert!(queued_attaches(&mut state, other).is_empty());

    state.switch_view_chat(Some(chat));
    assert_eq!(
        queued_attaches(&mut state, chat),
        [one.clone(), two.clone()],
        "the views attach again with fresh grants"
    );
    assert_eq!(origin(&state, one), Some(CardOrigin::User));
    assert_eq!(origin(&state, two), Some(CardOrigin::Agent));
    assert!(matches!(
        state.cards.card(one).unwrap().state,
        CardState::Opening
    ));
    assert!(!state.cards.is_open(three));
    assert!(!state.cards.is_open(four), "a preview is not restored");
    state.draw_frame(&mut Vec::new()).unwrap();
    assert_eq!(screen(&state).matches("── opening ──").count(), 2);

    state.monitor.flash_use(three.clone(), 2);
    assert!(
        queued_attaches(&mut state, chat).is_empty(),
        "a card the user hid stays hidden from the Agent's use"
    );
    state.monitor.flash_use(four.clone(), 3);
    assert_eq!(
        queued_attaches(&mut state, chat),
        std::slice::from_ref(four)
    );
}

#[test]
fn a_card_that_had_the_keyboard_comes_back_shown_and_the_input_takes_the_keys() {
    let (mut state, rows) = four_hosted_fixture();
    let chat = state.chat.unwrap();
    let one = &rows[0];
    attaches(state.key(key(KeyCode::Enter)), chat, one);
    assert!(state.card_focused());
    state.draw_frame(&mut Vec::new()).unwrap();

    state.switch_view_chat(Some(Uuid::new_v4()));
    state.draw_frame(&mut Vec::new()).unwrap();
    state.switch_view_chat(Some(chat));
    assert_eq!(queued_attaches(&mut state, chat), std::slice::from_ref(one));
    assert_eq!(origin(&state, one), Some(CardOrigin::User));
    assert!(!state.card_focused(), "no card takes the keyboard back");
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(
        !state.monitor.focused && !state.monitor.card_focused && !state.conversation_focused,
        "the input has the keys"
    );
    assert_eq!(selected(&state), *one, "the selection stays");
}

#[test]
fn cards_whose_ticks_left_the_list_or_ended_while_away_are_not_restored() {
    let (mut state, rows) = four_hosted_fixture();
    let chat = state.chat.unwrap();
    let [one, two, three] = [&rows[0], &rows[1], &rows[2]];
    for row in [one, two, three] {
        show_with_space(&mut state, row);
    }
    let mut entries = state.monitor.entries().to_vec();
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.draw_frame(&mut Vec::new()).unwrap();

    entries.retain(|entry| &entry.instance != two);
    let ended = entries
        .iter_mut()
        .find(|entry| &entry.instance == three)
        .unwrap();
    ended.actions.clear();
    ended.ended = Some("exited 0".into());
    state.update_tick_entries(chat, TickGroup::Ticks, entries);

    state.switch_view_chat(Some(chat));
    assert_eq!(queued_attaches(&mut state, chat), std::slice::from_ref(one));
    assert!(!state.cards.is_open(two) && !state.cards.is_open(three));
}

#[test]
fn cards_shown_in_a_subagents_chat_come_back_when_the_user_returns_to_it() {
    const ROOT_AGENT: Uuid = Uuid::from_u128(100);
    const CHILD_AGENT: Uuid = Uuid::from_u128(101);
    let mut state = State::new(true, 140, 60);
    state.prompt_enabled = true;
    let console = super::tests::console_with_state(state);
    console.activate_agent(ROOT_AGENT).unwrap();
    console.activate_agent(CHILD_AGENT).unwrap();
    let tick = {
        let mut state = console.state.lock().unwrap();
        state
            .monitor
            .replace(TickGroup::Ticks, vec![hosted_entry("child's tick")]);
        state.draw_frame(&mut Vec::new()).unwrap();
        state.monitor.focus(false, None);
        let tick = state.monitor.entries()[0].instance.clone();
        show_with_space(&mut state, &tick);
        state.draw_frame(&mut Vec::new()).unwrap();
        tick
    };

    console.activate_agent(ROOT_AGENT).unwrap();
    {
        let mut state = console.state.lock().unwrap();
        state.draw_frame(&mut Vec::new()).unwrap();
        assert!(state.cards.cards().is_empty(), "the parent shows none");
    }
    assert!(console.activate_agent(CHILD_AGENT).unwrap());
    let mut state = console.state.lock().unwrap();
    assert_eq!(
        queued_attaches(&mut state, CHILD_AGENT),
        std::slice::from_ref(&tick)
    );
    assert_eq!(origin(&state, &tick), Some(CardOrigin::User));
}

fn collapsed(state: &State, instance: &tick_protocol::InstanceRef) -> bool {
    state.cards.is_open(instance) && !state.shown_cards.contains(instance)
}

#[test]
fn space_or_a_click_on_a_collapsed_cards_row_shows_it_in_one_press() {
    let (mut state, rows) = hosted_fixture(26);
    let chat = state.chat.unwrap();
    let [one, two, three] = [&rows[0], &rows[1], &rows[2]];
    for row in [one, two, three] {
        show_with_space(&mut state, row);
    }
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(
        collapsed(&state, one) && !collapsed(&state, two) && !collapsed(&state, three),
        "only the two most recent cards fit: {}",
        screen(&state)
    );

    state.monitor.select(one);
    assert!(state.monitor_list_hint().contains("space show"));
    assert!(state.key(key(KeyCode::Char(' '))).is_none());
    assert!(state.monitor.focused && !state.card_focused());
    assert!(state.monitor_list_hint().contains("space hide"));
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(
        !collapsed(&state, one) && collapsed(&state, two),
        "the card shows as the user's most recent: {}",
        screen(&state)
    );
    assert!(state.key(key(KeyCode::Char(' '))).is_none());
    assert!(!state.cards.is_open(one), "a shown card hides");
    state.draw_frame(&mut Vec::new()).unwrap();

    attaches(state.key(key(KeyCode::Char(' '))), chat, one);
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(collapsed(&state, two));
    let row = target_cell(&state, &MonitorTarget::Tick(two.clone()));
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        state.pointer(
            MouseEvent {
                kind,
                row: row.0 as u16,
                column: row.1 as u16,
                modifiers: KeyModifiers::NONE,
            },
            &mut Vec::new(),
        );
    }
    assert!(state.pointer_input.is_none() && state.cards.is_open(two));
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(
        !collapsed(&state, two) && collapsed(&state, three),
        "a click shows it the same way: {}",
        screen(&state)
    );
}

#[test]
fn the_selected_rows_card_takes_the_room_ahead_of_the_users_and_gives_it_back() {
    let (mut state, rows) = hosted_fixture(26);
    let chat = state.chat.unwrap();
    let [one, two, three, four] = [&rows[0], &rows[1], &rows[2], &rows[3]];
    for row in [one, two, three] {
        show_with_space(&mut state, row);
    }
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(collapsed(&state, one) && !collapsed(&state, two) && !collapsed(&state, three));

    let start = Instant::now();
    rest_until(&mut state, start);
    assert!(press_at(&mut state, KeyCode::Char('j'), KeyModifiers::NONE, start).is_none());
    attaches(rest_until(&mut state, start + RESTED), chat, four);
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(
        !collapsed(&state, four) && !collapsed(&state, three) && collapsed(&state, two),
        "the preview comes before the user's cards: {}",
        screen(&state)
    );
    assert!(!screen(&state).contains("collapsed"), "{}", screen(&state));

    let later = start + Duration::from_secs(1);
    assert!(press_at(&mut state, KeyCode::Esc, KeyModifiers::NONE, later).is_none());
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(!state.cards.is_open(four));
    assert!(
        !collapsed(&state, two) && !collapsed(&state, three) && collapsed(&state, one),
        "the card it took the room from comes back: {}",
        screen(&state)
    );

    state.monitor.focus(false, None);
    state.monitor.select(one);
    rest_until(&mut state, later);
    assert!(
        rest_until(&mut state, later + RESTED).is_none(),
        "an open card only rises"
    );
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(
        !collapsed(&state, one) && !collapsed(&state, three) && collapsed(&state, two),
        "{}",
        screen(&state)
    );
    assert_eq!(origin(&state, one), Some(CardOrigin::User));
    assert!(state.monitor_list_hint().contains("space hide"));
    assert!(press_at(&mut state, KeyCode::Char('j'), KeyModifiers::NONE, later).is_none());
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(
        collapsed(&state, one) && !collapsed(&state, two) && !collapsed(&state, three),
        "it goes back to its own place: {}",
        screen(&state)
    );
    assert_eq!(origin(&state, one), Some(CardOrigin::User));
}

#[test]
fn space_hides_a_preview_until_the_selection_comes_back_and_shows_a_card_before_it() {
    let (mut state, rows) = tab_fixture();
    let chat = state.chat.unwrap();
    let [one, two] = [&rows[1], &rows[2]];
    let start = Instant::now();
    rest_until(&mut state, start);
    assert!(press_at(&mut state, KeyCode::Char('j'), KeyModifiers::NONE, start).is_none());
    assert!(state.monitor_list_hint().contains("space show"));
    attaches(
        press_at(
            &mut state,
            KeyCode::Char(' '),
            KeyModifiers::NONE,
            start + QUICKLY,
        ),
        chat,
        one,
    );
    assert_eq!(
        origin(&state, one),
        Some(CardOrigin::User),
        "Space before the preview shows the card as the user's"
    );
    assert!(rest_until(&mut state, start + RESTED).is_none());
    assert!(state.monitor_list_hint().contains("space hide"));

    let moved = start + Duration::from_secs(1);
    assert!(press_at(&mut state, KeyCode::Char('j'), KeyModifiers::NONE, moved).is_none());
    assert!(state.cards.is_open(one), "the user's card stays");
    attaches(rest_until(&mut state, moved + RESTED), chat, two);
    state.draw_frame(&mut Vec::new()).unwrap();
    assert!(state.monitor_list_hint().contains("space hide"));
    assert!(
        press_at(
            &mut state,
            KeyCode::Char(' '),
            KeyModifiers::NONE,
            moved + RESTED
        )
        .is_none()
    );
    assert!(!state.cards.is_open(two), "Space hides the preview");
    assert!(
        rest_until(&mut state, moved + Duration::from_secs(10)).is_none(),
        "and it stays hidden while the selection stays"
    );
    assert!(!state.cards.is_open(two));
    assert!(state.monitor_list_hint().contains("space show"));

    let back = moved + Duration::from_secs(20);
    assert!(press_at(&mut state, KeyCode::Char('k'), KeyModifiers::NONE, back).is_none());
    assert!(
        press_at(
            &mut state,
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            back + QUICKLY
        )
        .is_none()
    );
    attaches(rest_until(&mut state, back + QUICKLY + RESTED), chat, two);
    assert_eq!(
        origin(&state, two),
        Some(CardOrigin::Preview),
        "coming back previews it again"
    );
    assert!(
        press_at(
            &mut state,
            KeyCode::Char(' '),
            KeyModifiers::NONE,
            back + RESTED
        )
        .is_none()
    );
    attaches(
        press_at(
            &mut state,
            KeyCode::Char(' '),
            KeyModifiers::NONE,
            back + RESTED,
        ),
        chat,
        two,
    );
    assert_eq!(
        origin(&state, two),
        Some(CardOrigin::User),
        "shown as the user's"
    );
}
