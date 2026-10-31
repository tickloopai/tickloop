use super::*;
use crate::monitor_rows::{MONITOR_ENTER_KEY, MonitorAction, TickIntent, TickOwner};
use crate::terminal::monitor::{MonitorEntry, MonitorTarget, TickGroup};

fn entry(
    instance: tick_protocol::InstanceRef,
    group: TickGroup,
    glyph: Glyph,
    intent: TickIntent,
) -> MonitorEntry {
    MonitorEntry {
        instance,
        group,
        glyph,
        title: "watch".into(),
        kind: "Terminal".into(),
        status: String::new(),
        notes: Vec::new(),
        subscription: None,
        owner: TickOwner::ThisChat,
        tone: crate::monitor_rows::StatusTone::Live,
        updated_at_ms: None,
        update: None,
        ended: None,
        resources: None,
        actions: vec![MonitorAction {
            key: MONITOR_ENTER_KEY,
            label: "open",
            intent,
        }],
    }
}

fn terminal_tick() -> MonitorEntry {
    let instance = tick_protocol::InstanceRef::hosted(Uuid::new_v4());
    entry(
        instance.clone(),
        TickGroup::Ticks,
        Glyph::TerminalTick,
        TickIntent::OpenFullScreen(instance),
    )
}

fn ended_terminal_tick() -> MonitorEntry {
    let instance = tick_protocol::InstanceRef::hosted(Uuid::new_v4());
    entry(
        instance.clone(),
        TickGroup::Ticks,
        Glyph::TerminalTick,
        TickIntent::ReopenFullScreen(instance),
    )
}

fn shell() -> MonitorEntry {
    let instance = tick_protocol::InstanceRef::shell(Uuid::new_v4());
    entry(
        instance.clone(),
        TickGroup::Shells,
        Glyph::ShellTick,
        TickIntent::OpenFullScreen(instance),
    )
}

/// A View whose focused Monitor lists `ticks` and `shells`, the first row selected.
fn fixture(ticks: Vec<MonitorEntry>, shells: Vec<MonitorEntry>) -> State {
    let mut state = State::new(true, 140, 40);
    state.switch_view_chat(Some(Uuid::new_v4()));
    state.prompt_enabled = true;
    state.monitor.replace(TickGroup::Ticks, ticks);
    state.monitor.replace(TickGroup::Shells, shells);
    state.draw_frame(&mut Vec::new()).unwrap();
    state.monitor.focus(false, None);
    state.draw_frame(&mut Vec::new()).unwrap();
    state
}

fn press(state: &mut State, code: KeyCode, modifiers: KeyModifiers) -> Option<InputEvent> {
    let event = state.key(KeyEvent::new(code, modifiers));
    state.draw_frame(&mut Vec::new()).unwrap();
    event
}

fn opens(event: Option<InputEvent>, chat: Uuid, instance: &tick_protocol::InstanceRef) {
    match event {
        Some(InputEvent::TickAction {
            chat: acted,
            action,
        }) => {
            assert_eq!(acted, chat);
            assert_eq!(action.instance, instance.to_string());
            assert_eq!(action.key, MONITOR_ENTER_KEY);
        }
        other => panic!("expected the full-screen view's action, got {other:?}"),
    }
}

#[test]
fn terminal_ticks_and_shells_open_full_screen_and_never_a_card() {
    let mut state = fixture(vec![terminal_tick()], vec![shell()]);
    let chat = state.chat.unwrap();
    let rows: Vec<_> = state
        .monitor
        .entries()
        .iter()
        .map(|entry| entry.instance.clone())
        .collect();
    for row in &rows {
        state.monitor.select(row);
        assert!(!state.has_card(row) && !state.row_toggles_card(row));
        let hint = state.monitor_list_hint();
        assert!(
            !hint.contains("space") && !hint.contains("z/Z"),
            "no card keys are hinted: {hint}"
        );
        assert!(press(&mut state, KeyCode::Char(' '), KeyModifiers::NONE).is_none());
        assert!(press(&mut state, KeyCode::Char('Z'), KeyModifiers::SHIFT).is_none());
        assert!(state.cards.cards().is_empty(), "Space and Z open no card");
        assert!(state.monitor.focused, "Space leaves the keys on the list");
        opens(
            press(&mut state, KeyCode::Enter, KeyModifiers::NONE),
            chat,
            row,
        );
        assert!(state.monitor.detail.is_none() && !state.card_focused());
    }

    state.monitor.select(&rows[1]);
    press(&mut state, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(state.monitor.selected().unwrap().instance, rows[0]);
    assert!(state.cards.cards().is_empty(), "Tab previews no card");

    state.monitor.focused = false;
    state.monitor.flash_use(rows[0].clone(), 1);
    state.monitor.flash_use(rows[1].clone(), 1);
    let (_, inputs) = state.settle_card_signals();
    assert!(
        inputs.is_empty() && state.cards.cards().is_empty(),
        "the Agent's use shows them nowhere"
    );
}

#[test]
fn space_on_an_ended_terminal_tick_says_enter_reopens_it_and_enter_does() {
    let mut state = fixture(vec![ended_terminal_tick()], Vec::new());
    let chat = state.chat.unwrap();
    let row = state.monitor.selected().unwrap().instance.clone();
    assert!(
        !state.monitor_list_hint().contains("space reopen"),
        "{}",
        state.monitor_list_hint()
    );
    assert!(press(&mut state, KeyCode::Char(' '), KeyModifiers::NONE).is_none());
    assert_eq!(
        state.monitor_key_notice(),
        Some(full_screen::ENDED_TERMINAL_NOTICE)
    );
    assert!(state.cards.cards().is_empty());
    opens(
        press(&mut state, KeyCode::Enter, KeyModifiers::NONE),
        chat,
        &row,
    );
}

fn click(state: &mut State, at: (usize, usize)) -> Option<InputEvent> {
    let (row, column) = at;
    let event = |kind| MouseEvent {
        kind,
        column: u16::try_from(column).unwrap(),
        row: u16::try_from(row).unwrap(),
        modifiers: KeyModifiers::NONE,
    };
    state.pointer(
        event(MouseEventKind::Down(MouseButton::Left)),
        &mut Vec::new(),
    );
    // The input loop takes what each mouse event asks before the next one arrives.
    let pressed = state.pointer_input.take();
    state.pointer(
        event(MouseEventKind::Up(MouseButton::Left)),
        &mut Vec::new(),
    );
    state.draw_frame(&mut Vec::new()).unwrap();
    pressed.or_else(|| state.pointer_input.take())
}

#[test]
fn a_click_selects_a_full_screen_row_and_a_double_click_opens_it() {
    let mut state = fixture(vec![terminal_tick()], vec![shell()]);
    let chat = state.chat.unwrap();
    let shell = state.monitor.entries()[1].instance.clone();
    let at = state
        .monitor_row_targets
        .iter()
        .find(|target| target.target == MonitorTarget::Tick(shell.clone()))
        .map(|target| (target.row, target.columns.start + 2))
        .expect("the Shell's row is on screen");
    assert!(click(&mut state, at).is_none(), "one click only selects");
    assert_eq!(state.monitor.selected().unwrap().instance, shell);
    opens(click(&mut state, at), chat, &shell);
}

#[test]
fn a_lent_terminal_draws_nothing_and_rings_once_for_a_waiting_question() {
    let mut state = fixture(vec![terminal_tick()], Vec::new());
    state.terminal_lent = true;

    let menu = Menu {
        id: Uuid::new_v4(),
        title: "Permission".into(),
        context: Vec::new(),
        markdown: None,
        options: vec!["Allow".into()],
        selected: 0,
        confirmation: false,
    };
    state.menu = Some(menu.clone());
    let mut output = Vec::new();
    state.draw(&mut output).unwrap();
    assert!(output.is_empty(), "nothing is drawn under the view");
    assert_eq!(state.rung_for, Some(menu.id), "the question rang");
    state.draw(&mut output).unwrap();
    assert_eq!(state.rung_for, Some(menu.id), "it rings once");
    assert!(state.menu.is_some(), "the question waits behind the view");
}
