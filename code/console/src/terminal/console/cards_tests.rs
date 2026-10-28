use super::*;
use crate::terminal::monitor::{MonitorEntry, MonitorTarget, TickGroup};
use crate::terminal::monitor_cards::{CardDemand, CardFrame, FOCUSED_CARD_HINT};

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
    assert!(shown.contains(&format!("▾ {title}")), "{shown}");
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
    assert!(!screen(&state).contains(&format!("▾ {second_title}")));
}

#[test]
fn enter_shows_and_focuses_a_card_and_a_lone_escape_leaves_it_open() {
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
        "esc leave",
        "a Shell's card is read-only, so only leaving is hinted"
    );

    let start = Instant::now();
    state.card_input(b"\x1b", start);
    assert!(state.card_focused(), "an Esc waits for what follows");
    assert!(!state.expire_card_keys(start + Duration::from_millis(60)));
    assert!(state.expire_card_keys(start + Duration::from_millis(400)));
    assert!(!state.card_focused());
    assert!(state.cards.is_open(&first), "the card stays shown");
    assert_eq!(selected(&state), first, "the selection stays on the Tick");
    assert!(state.monitor.focused);

    assert!(
        state.key(key(KeyCode::Enter)).is_none(),
        "entering an open card attaches nothing new"
    );
    assert!(state.card_focused());
    state.card_input(b"\x1b\x1b", start);
    state.expire_card_keys(start + Duration::from_millis(60));
    state.expire_card_keys(start + Duration::from_millis(1000));
    assert!(state.card_focused(), "Esc Esc stays in the card");
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
    );
    assert!(
        state.card_focused(),
        "a click inside the card keeps the focus"
    );
    state.card_input(b"\x1b[<0;2;2M", Instant::now());
    assert!(!state.card_focused(), "a click outside leaves it");
}

#[test]
fn focus_reports_read_by_a_focused_card_reach_the_console() {
    let mut state = cards_fixture();
    state.key(key(KeyCode::Enter));
    state.file_index_focused = true;
    state.card_input(b"\x1b[O", Instant::now());
    assert!(
        !state.file_index_focused,
        "the Console lost the terminal's focus"
    );
    state.card_input(b"\x1b[I", Instant::now());
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
    assert!(row.text().starts_with("▾ tail log"), "{:?}", row.text());
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
