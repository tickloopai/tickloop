use super::*;
use crate::agent_console::{AgentState, AttentionGroup};
use crate::terminal::display::Glyph;
use tickloop_harness::protocol::LoopState;
use tickloop_harness::test_support::screen::SELECTED_MARK;

const SHOWN_CHAT: Uuid = Uuid::from_u128(0x10);

fn live_row(index: u128, agent_state: AgentState) -> AgentRow {
    AgentRow {
        agent_id: Uuid::from_u128(0x100 + index),
        parent_id: None,
        title: format!("{} task {index:02}", agent_state.label()),
        status: agent_state.label().into(),
        state: match agent_state {
            AgentState::Working => LoopState::Running,
            AgentState::NeedsInput => LoopState::WaitingQuestion,
            _ => LoopState::Idle,
        },
        project: "/work".into(),
        workspace: None,
        approvals: 0,
        questions: usize::from(agent_state == AgentState::NeedsInput),
        agent_state,
        group: agent_state.group(),
        glyph: status_glyph(agent_state, false),
        preview: None,
        updated_at: None,
        run_started_at: None,
        subagent: None,
        ended: None,
    }
}

fn ended_chat(index: u128) -> Uuid {
    Uuid::from_u128(0x900 + index)
}

fn ended_row(index: u128, reason: EndReason, resumable: bool) -> AgentRow {
    let agent_state = match reason {
        EndReason::DaemonStopped | EndReason::WorkerLost => AgentState::Failed,
        EndReason::DaemonOff | EndReason::UserStopped => AgentState::Stopped,
    };
    AgentRow {
        agent_id: ended_chat(index),
        parent_id: None,
        title: format!("Ended task {index:02}"),
        status: format!("{} · ended", agent_state.label()),
        state: LoopState::Idle,
        project: "/work".into(),
        workspace: None,
        approvals: 0,
        questions: 0,
        agent_state,
        group: AttentionGroup::Completed,
        glyph: match agent_state {
            AgentState::Failed => Glyph::AgentFailed,
            _ => Glyph::AgentStopped,
        },
        preview: None,
        updated_at: None,
        run_started_at: None,
        subagent: None,
        ended: Some(EndedRow {
            chat_id: ended_chat(index),
            reason,
            resumable,
        }),
    }
}

fn overview_console(width: u16, height: u16, rows: Vec<AgentRow>) -> Console {
    let mut state = State::new(true, width, height);
    state.prompt_enabled = true;
    state.chat = Some(SHOWN_CHAT);
    let console = super::tests::console_with_state(state);
    console
        .set_global_overview(Some(AgentOverview {
            rows,
            dispatch_directory: PathBuf::from("/work"),
            created: None,
            visible: None,
        }))
        .unwrap();
    console
}

fn drawn(state: &mut State) -> Vec<String> {
    state.draw(&mut std::io::sink()).unwrap();
    super::tests::selection_marked_screen(state)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn select(state: &mut State, agent: Uuid) {
    assert!(state.overview.select_agent(agent), "{agent} is not listed");
}

/// The red key-hint row the first `x` leaves.
fn red_hint(state: &mut State, hint: &str) {
    let (lines, _, _) = state.normal_styled_frame();
    let line = lines
        .iter()
        .find(|line| line.text().contains(hint))
        .unwrap_or_else(|| panic!("{hint:?} is not shown: {lines:#?}"));
    assert!(
        line.spans.iter().any(|span| span.role == Some(Role::Error)),
        "{line:?}"
    );
}

#[test]
fn groups_list_needs_input_then_working_then_completed() {
    let console = overview_console(
        100,
        40,
        vec![
            live_row(0, AgentState::Completed),
            ended_row(0, EndReason::DaemonStopped, true),
            live_row(1, AgentState::Working),
            live_row(2, AgentState::Idle),
            live_row(3, AgentState::NeedsInput),
            live_row(4, AgentState::Failed),
        ],
    );
    let mut state = console.state.lock().unwrap();

    let frame = drawn(&mut state);

    let headings = frame
        .iter()
        .filter_map(|line| {
            let line = line.trim_start_matches([SELECTED_MARK, ' ']);
            line.strip_prefix("▾ ").map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        headings,
        ["Needs input · 1", "Working · 1", "Completed · 4"],
        "{frame:#?}"
    );
}

#[test]
fn completed_keeps_three_rows_on_a_full_screen_and_folds_the_rest() {
    let mut rows = (0..30)
        .map(|index| live_row(index, AgentState::Working))
        .collect::<Vec<_>>();
    rows.extend((0..8).map(|index| ended_row(index, EndReason::DaemonStopped, true)));
    let console = overview_console(100, 24, rows);
    let mut state = console.state.lock().unwrap();
    for _ in 0..60 {
        state.key(key(KeyCode::Char('j')));
    }

    let frame = drawn(&mut state);

    let shown = frame
        .iter()
        .filter(|line| line.contains("Ended task"))
        .count();
    assert_eq!(shown, 3, "{frame:#?}");
    assert!(
        frame.iter().any(|line| line.contains("… 5 more completed")),
        "{frame:#?}"
    );
}

#[test]
fn completed_takes_the_rows_a_tall_screen_has() {
    let mut rows = vec![live_row(0, AgentState::Working)];
    rows.extend((0..8).map(|index| ended_row(index, EndReason::DaemonStopped, true)));
    let console = overview_console(100, 40, rows);
    let mut state = console.state.lock().unwrap();

    let frame = drawn(&mut state);

    assert_eq!(
        frame
            .iter()
            .filter(|line| line.contains("Ended task"))
            .count(),
        8,
        "{frame:#?}"
    );
    assert!(!frame.iter().any(|line| line.contains("more completed")));
}

#[test]
fn a_waiting_agent_says_how_long_it_has_waited() {
    let now = tickloop_harness::base::private_fs::unix_time_milliseconds();
    let mut waiting = live_row(0, AgentState::NeedsInput);
    waiting.status = "Needs answer".into();
    waiting.updated_at = Some(now - 3 * 60 * 1000 - 5_000);
    let mut fresh = live_row(1, AgentState::NeedsInput);
    fresh.status = "Needs answer".into();
    fresh.updated_at = Some(now);
    let console = overview_console(120, 30, vec![waiting, fresh]);
    let mut state = console.state.lock().unwrap();

    let frame = drawn(&mut state);

    let line = |title: &str| {
        frame
            .iter()
            .find(|line| line.contains(title))
            .unwrap_or_else(|| panic!("{frame:#?}"))
            .clone()
    };
    assert!(
        line("task 00").contains("Needs answer · waiting 3m"),
        "{frame:#?}"
    );
    assert!(
        line("task 01").contains("Needs answer · waiting <1m"),
        "{frame:#?}"
    );
}

#[test]
fn enter_resumes_an_agent_that_failed_or_was_stopped_at_once() {
    for reason in [
        EndReason::DaemonStopped,
        EndReason::WorkerLost,
        EndReason::UserStopped,
    ] {
        let console = overview_console(
            100,
            30,
            vec![live_row(0, AgentState::Working), ended_row(0, reason, true)],
        );
        let mut state = console.state.lock().unwrap();
        select(&mut state, ended_chat(0));
        assert_eq!(
            state.overview.row_hints(),
            ["enter resume", "space peek", "x remove"]
        );

        let event = state.key(key(KeyCode::Enter));

        assert!(
            matches!(event, Some(InputEvent::ResumeEndedAgent(chat)) if chat == ended_chat(0)),
            "{reason:?}: {event:?}"
        );
    }
}

#[test]
fn an_agent_that_ended_while_the_daemon_was_off_needs_a_second_enter() {
    let console = overview_console(
        100,
        30,
        vec![
            live_row(0, AgentState::Working),
            ended_row(0, EndReason::DaemonOff, true),
        ],
    );
    let mut state = console.state.lock().unwrap();
    select(&mut state, ended_chat(0));

    assert!(state.key(key(KeyCode::Enter)).is_none());
    assert!(
        state.status.contains("Press enter again to resume"),
        "{}",
        state.status
    );
    assert!(state.status.contains("x twice"), "{}", state.status);

    // Moving away withdraws the offer, so a later Enter asks again.
    state.key(key(KeyCode::Char('k')));
    select(&mut state, ended_chat(0));
    assert!(state.key(key(KeyCode::Enter)).is_none());

    let event = state.key(key(KeyCode::Enter));
    assert!(
        matches!(event, Some(InputEvent::ResumeEndedAgent(chat)) if chat == ended_chat(0)),
        "{event:?}"
    );
}

#[test]
fn an_agent_whose_history_is_gone_can_only_be_removed() {
    let console = overview_console(
        100,
        30,
        vec![
            live_row(0, AgentState::Working),
            ended_row(0, EndReason::DaemonStopped, false),
        ],
    );
    let mut state = console.state.lock().unwrap();
    select(&mut state, ended_chat(0));

    assert!(state.key(key(KeyCode::Enter)).is_none());
    assert!(state.key(key(KeyCode::Enter)).is_none());

    assert!(
        state.status.contains("no longer on disk"),
        "{}",
        state.status
    );
    assert_eq!(state.overview.row_hints(), ["x remove"]);
}

#[test]
fn two_presses_of_x_remove_an_ended_row_and_the_first_warns_in_red() {
    let console = overview_console(
        100,
        30,
        vec![
            live_row(0, AgentState::Working),
            ended_row(0, EndReason::DaemonStopped, true),
        ],
    );
    let mut state = console.state.lock().unwrap();
    select(&mut state, ended_chat(0));

    assert!(state.key(key(KeyCode::Char('x'))).is_none());
    red_hint(&mut state, "Press x again to remove this row");

    let event = state.key(key(KeyCode::Char('x')));
    assert!(
        matches!(event, Some(InputEvent::ForgetEndedAgent(chat)) if chat == ended_chat(0)),
        "{event:?}"
    );
}

#[test]
fn two_presses_of_x_stop_a_running_agent_and_the_first_warns_in_red() {
    let running = live_row(0, AgentState::Working);
    let console = overview_console(
        100,
        30,
        vec![
            running.clone(),
            ended_row(0, EndReason::DaemonStopped, true),
        ],
    );
    let mut state = console.state.lock().unwrap();
    select(&mut state, running.agent_id);
    assert_eq!(
        state.overview.row_hints(),
        ["enter open", "space peek", "x stop"]
    );

    assert!(state.key(key(KeyCode::Char('x'))).is_none());
    red_hint(&mut state, "Press x again to stop this Agent");

    let event = state.key(key(KeyCode::Char('x')));
    assert!(
        matches!(event, Some(InputEvent::StopAgent(agent)) if agent == running.agent_id),
        "{event:?}"
    );
}

#[test]
fn another_key_or_another_row_between_the_presses_of_x_cancels() {
    let running = live_row(0, AgentState::Working);
    let console = overview_console(
        100,
        30,
        vec![
            running.clone(),
            ended_row(0, EndReason::DaemonStopped, true),
        ],
    );
    let mut state = console.state.lock().unwrap();
    select(&mut state, ended_chat(0));

    assert!(state.key(key(KeyCode::Char('x'))).is_none());
    state.key(key(KeyCode::Char('k')));
    select(&mut state, ended_chat(0));
    assert!(state.key(key(KeyCode::Char('x'))).is_none());

    // A first press on one row and a second on another stops nothing.
    select(&mut state, running.agent_id);
    assert!(state.key(key(KeyCode::Char('x'))).is_none());
}
