use super::*;
use crate::agent_console::AttentionGroup;
use crate::terminal::display::Glyph;
use tickloop_harness::protocol::LoopState;

const SHOWN_CHAT: Uuid = Uuid::from_u128(0x10);

fn working_row(index: u128) -> AgentRow {
    AgentRow {
        agent_id: Uuid::from_u128(0x100 + index),
        parent_id: None,
        title: format!("Working task {index:02}"),
        status: "Running".into(),
        state: LoopState::Running,
        project: "/work".into(),
        workspace: None,
        approvals: 0,
        questions: 0,
        group: AttentionGroup::Working,
        glyph: status_glyph(AttentionGroup::Working, false),
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

fn ended_row(index: u128, outcome: EndedOutcome, resumable: bool) -> AgentRow {
    let (word, glyph) = match outcome {
        EndedOutcome::Failed => ("Failed · daemon stopped", Glyph::AgentFailed),
        EndedOutcome::Stopped => (
            "Stopped · ended while the daemon was off",
            Glyph::AgentStopped,
        ),
    };
    AgentRow {
        agent_id: ended_chat(index),
        parent_id: None,
        title: format!("Ended task {index:02}"),
        status: word.into(),
        state: LoopState::Idle,
        project: "/work".into(),
        workspace: None,
        approvals: 0,
        questions: 0,
        group: AttentionGroup::Ended,
        glyph,
        preview: None,
        updated_at: None,
        run_started_at: None,
        subagent: None,
        ended: Some(EndedRow {
            chat_id: ended_chat(index),
            outcome,
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

#[test]
fn ended_agents_keep_three_rows_on_a_full_screen_and_fold_the_rest() {
    let mut rows = (0..30).map(working_row).collect::<Vec<_>>();
    rows.extend((0..8).map(|index| ended_row(index, EndedOutcome::Failed, true)));
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
        frame.iter().any(|line| line.contains("… 5 more ended")),
        "{frame:#?}"
    );
    assert!(
        frame
            .iter()
            .any(|line| line.contains("Ended task 00") && line.contains("Failed · daemon stopped")),
        "{frame:#?}"
    );
}

#[test]
fn ended_agents_take_the_rows_a_tall_screen_has() {
    let mut rows = vec![working_row(0)];
    rows.extend((0..8).map(|index| ended_row(index, EndedOutcome::Failed, true)));
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
    assert!(!frame.iter().any(|line| line.contains("more ended")));
}

#[test]
fn enter_resumes_a_failed_agent_at_once() {
    let console = overview_console(
        100,
        30,
        vec![working_row(0), ended_row(0, EndedOutcome::Failed, true)],
    );
    let mut state = console.state.lock().unwrap();
    select(&mut state, ended_chat(0));
    drawn(&mut state);
    assert!(
        state
            .overview
            .row_hints()
            .contains(&"enter resume".to_owned()),
        "{:?}",
        state.overview.row_hints()
    );

    let event = state.key(key(KeyCode::Enter));

    assert!(
        matches!(event, Some(InputEvent::ResumeEndedAgent(chat)) if chat == ended_chat(0)),
        "{event:?}"
    );
}

#[test]
fn a_stopped_agent_needs_a_second_enter_that_explains_the_first() {
    let console = overview_console(
        100,
        30,
        vec![working_row(0), ended_row(0, EndedOutcome::Stopped, true)],
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
        vec![working_row(0), ended_row(0, EndedOutcome::Failed, false)],
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
fn two_presses_of_x_remove_an_ended_agent_and_the_first_warns_in_red() {
    let console = overview_console(
        100,
        30,
        vec![working_row(0), ended_row(0, EndedOutcome::Failed, true)],
    );
    let mut state = console.state.lock().unwrap();
    select(&mut state, ended_chat(0));

    assert!(state.key(key(KeyCode::Char('x'))).is_none());
    let (lines, _, _) = state.normal_styled_frame();
    let warning = lines
        .iter()
        .find(|line| {
            line.text()
                .contains("Press x again to remove Ended task 00 from the list")
        })
        .unwrap_or_else(|| panic!("{lines:#?}"));
    assert!(
        warning
            .spans
            .iter()
            .any(|span| span.role == Some(Role::Error)),
        "{warning:?}"
    );

    let event = state.key(key(KeyCode::Char('x')));
    assert!(
        matches!(event, Some(InputEvent::ForgetEndedAgent(chat)) if chat == ended_chat(0)),
        "{event:?}"
    );
}

#[test]
fn another_key_between_the_presses_of_x_cancels_the_removal() {
    let console = overview_console(
        100,
        30,
        vec![working_row(0), ended_row(0, EndedOutcome::Failed, true)],
    );
    let mut state = console.state.lock().unwrap();
    select(&mut state, ended_chat(0));

    assert!(state.key(key(KeyCode::Char('x'))).is_none());
    state.key(key(KeyCode::Char('k')));
    select(&mut state, ended_chat(0));
    assert!(state.key(key(KeyCode::Char('x'))).is_none());
}

#[test]
fn x_on_a_running_agent_removes_nothing() {
    let console = overview_console(
        100,
        30,
        vec![working_row(0), ended_row(0, EndedOutcome::Failed, true)],
    );
    let mut state = console.state.lock().unwrap();
    select(&mut state, working_row(0).agent_id);

    assert!(state.key(key(KeyCode::Char('x'))).is_none());
    assert!(state.key(key(KeyCode::Char('x'))).is_none());
    assert!(
        state.status.contains("Only an ended Agent"),
        "{}",
        state.status
    );
    assert_eq!(state.overview.row_hints(), ["enter open"]);
}
