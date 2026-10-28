use super::*;
use crate::agent_console::{AgentState, AttentionGroup};
use tickloop_harness::protocol::LoopState;
use tickloop_harness::test_support::screen::SELECTED_MARK;

const SHOWN_CHAT: Uuid = Uuid::from_u128(0x10);
const WAITING: Uuid = Uuid::from_u128(0x101);
const IDLE: Uuid = Uuid::from_u128(0x102);
const ENDED_CHAT: Uuid = Uuid::from_u128(0x901);
const QUESTION: Uuid = Uuid::from_u128(0x501);

fn row(agent_id: Uuid, title: &str, agent_state: AgentState) -> AgentRow {
    AgentRow {
        agent_id,
        parent_id: None,
        title: title.into(),
        status: match agent_state {
            AgentState::NeedsInput => "Needs answer".into(),
            state => state.label().into(),
        },
        state: match agent_state {
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
        preview: Some(format!("Latest from {title}")),
        updated_at: Some(
            tickloop_harness::base::private_fs::unix_time_milliseconds() - 3 * 60 * 1000 - 5_000,
        ),
        run_started_at: None,
        subagent: None,
        ended: None,
    }
}

fn ended_row(resumable: bool) -> AgentRow {
    let mut ended = row(ENDED_CHAT, "Old review", AgentState::Failed);
    ended.status = "Failed · ended when the daemon stopped".into();
    ended.ended = Some(EndedRow {
        chat_id: ENDED_CHAT,
        reason: EndReason::DaemonStopped,
        resumable,
    });
    ended
}

fn overview_console(rows: Vec<AgentRow>) -> Console {
    let mut state = State::new(true, 100, 32);
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

fn fleet() -> Console {
    overview_console(vec![
        row(WAITING, "Research NVDA", AgentState::NeedsInput),
        row(IDLE, "Fix the parser", AgentState::Completed),
        ended_row(true),
    ])
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

fn space(state: &mut State) -> Option<InputEvent> {
    state.key(key(KeyCode::Char(' ')))
}

fn type_text(state: &mut State, text: &str) {
    for character in text.chars() {
        state.key(key(KeyCode::Char(character)));
    }
}

fn question() -> PeekRequest {
    PeekRequest {
        request_id: QUESTION,
        prompt: "Which market should the research cover first?".into(),
        options: vec!["US".into(), "HK".into()],
    }
}

/// The panel on the waiting Agent, showing `request` as drawn.
fn peeking_at(request: PeekRequest, height: u16) -> Console {
    let mut state = State::new(true, 100, height);
    state.prompt_enabled = true;
    state.chat = Some(SHOWN_CHAT);
    let console = super::tests::console_with_state(state);
    console
        .set_global_overview(Some(AgentOverview {
            rows: vec![row(WAITING, "Research NVDA", AgentState::NeedsInput)],
            dispatch_directory: PathBuf::from("/work"),
            created: None,
            visible: None,
        }))
        .unwrap();
    {
        let mut state = console.state.lock().unwrap();
        select(&mut state, WAITING);
        space(&mut state);
    }
    console.set_peek_request(WAITING, Some(request)).unwrap();
    console
}

#[test]
fn space_opens_the_peek_panel_and_space_or_esc_closes_it_keeping_the_dispatch_draft() {
    let console = fleet();
    let mut state = console.state.lock().unwrap();
    state.key(key(KeyCode::Tab));
    state.insert("dispatch draft");
    state.key(key(KeyCode::Tab));
    select(&mut state, IDLE);

    let event = space(&mut state);

    assert!(
        matches!(event, Some(InputEvent::PeekOpened(id)) if id == IDLE),
        "{event:?}"
    );
    assert_eq!(state.editor.draft(), "", "the reply box starts empty");
    let frame = drawn(&mut state);
    assert!(
        frame
            .iter()
            .any(|line| line.contains("Fix the parser · Completed")),
        "{frame:#?}"
    );
    assert!(
        frame
            .iter()
            .any(|line| line.contains("Latest from Fix the parser")),
        "{frame:#?}"
    );
    assert!(
        frame
            .iter()
            .any(|line| line.contains("Reply to Fix the parser…")),
        "{frame:#?}"
    );
    assert!(
        frame
            .iter()
            .any(|line| line.contains("enter send · ↑/↓ Agent · → open · space/esc close")),
        "{frame:#?}"
    );

    assert!(space(&mut state).is_none());
    assert!(state.overview.peek.is_none());
    assert_eq!(state.editor.draft(), "dispatch draft");

    space(&mut state);
    type_text(&mut state, "half a reply");
    assert!(state.key(key(KeyCode::Esc)).is_none());
    assert!(state.overview.peek.is_none());
    assert_eq!(state.editor.draft(), "dispatch draft");
    assert!(
        matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::CloseOverview)
        ),
        "Esc closes the panel first, then the overview"
    );
}

#[test]
fn a_waiting_agent_shows_its_question_how_long_it_waited_and_a_number_answers_it() {
    let console = fleet();
    let mut state = console.state.lock().unwrap();
    select(&mut state, WAITING);
    space(&mut state);
    drop(state);
    console.set_peek_request(WAITING, Some(question())).unwrap();
    let mut state = console.state.lock().unwrap();

    let frame = drawn(&mut state);

    for expected in [
        "Research NVDA · Needs answer · waiting 3m",
        "Which market should the research cover first?",
        "1. US",
        "2. HK",
    ] {
        assert!(
            frame.iter().any(|line| line.contains(expected)),
            "{expected:?}: {frame:#?}"
        );
    }
    type_text(&mut state, "2");
    let event = state.key(key(KeyCode::Enter));
    assert!(
        matches!(
            &event,
            Some(InputEvent::PeekAnswer { agent_id, request_id, option })
                if *agent_id == WAITING && *request_id == QUESTION && option == "HK"
        ),
        "{event:?}"
    );
    assert_eq!(state.editor.draft(), "");
    assert!(state.overview.peek.is_some(), "the panel stays open");

    type_text(&mut state, "3");
    let event = state.key(key(KeyCode::Enter));
    assert!(
        matches!(&event, Some(InputEvent::PeekReply { text, .. }) if text == "3"),
        "a number no choice has is a message: {event:?}"
    );
}

#[test]
fn a_permission_request_takes_no_number_and_points_to_the_agents_own_view() {
    let console = peeking_at(
        PeekRequest {
            request_id: QUESTION,
            prompt: "Permission required · Bash".into(),
            options: Vec::new(),
        },
        32,
    );
    let mut state = console.state.lock().unwrap();
    let frame = drawn(&mut state);
    assert!(
        frame
            .iter()
            .any(|line| line.contains("→ opens the Agent to answer")),
        "{frame:#?}"
    );
    assert!(
        !frame
            .iter()
            .any(|line| line.contains("1. ") || line.contains("for this session")),
        "the panel offers no choice the Agent's own view decides: {frame:#?}"
    );

    for number in ["1", "2", "3"] {
        type_text(&mut state, number);
        assert!(
            state.key(key(KeyCode::Enter)).is_none(),
            "{number} must not approve anything"
        );
        assert!(
            state.status.contains("Only the Agent's own view answers"),
            "{}",
            state.status
        );
        assert_eq!(state.editor.draft(), number, "the draft is kept");
        state.editor.apply(EditorCommand::Clear);
    }
}

#[test]
fn a_number_answers_nothing_when_an_option_did_not_fit_on_screen() {
    let long = "A market whose description runs far past the width of the panel, so that its line \
                is cut before the user can read where it ends";
    let console = peeking_at(
        PeekRequest {
            request_id: QUESTION,
            prompt: "Which market?".into(),
            options: vec!["US".into(), long.into()],
        },
        32,
    );
    let mut state = console.state.lock().unwrap();
    drawn(&mut state);
    type_text(&mut state, "1");

    assert!(state.key(key(KeyCode::Enter)).is_none());
    assert!(
        state.status.contains("Not every option fits"),
        "{}",
        state.status
    );
    assert_eq!(state.editor.draft(), "1");
    drop(state);

    // A panel too short for its options answers nothing either.
    let many = (1..=9).map(|index| format!("Market {index}")).collect();
    let console = peeking_at(
        PeekRequest {
            request_id: QUESTION,
            prompt: "Which market?".into(),
            options: many,
        },
        32,
    );
    let mut state = console.state.lock().unwrap();
    drawn(&mut state);
    type_text(&mut state, "1");
    assert!(state.key(key(KeyCode::Enter)).is_none());
    assert!(
        state.status.contains("Not every option fits"),
        "{}",
        state.status
    );

    // Every option whole on screen: the number answers.
    let console = peeking_at(question(), 32);
    let mut state = console.state.lock().unwrap();
    drawn(&mut state);
    type_text(&mut state, "1");
    assert!(matches!(
        state.key(key(KeyCode::Enter)),
        Some(InputEvent::PeekAnswer { option, .. }) if option == "US"
    ));
}

#[test]
fn enter_sends_the_reply_to_the_peeked_agent_and_keeps_the_panel() {
    let console = fleet();
    let mut state = console.state.lock().unwrap();
    select(&mut state, IDLE);
    space(&mut state);
    type_text(&mut state, "Run the tests again");

    let event = state.key(key(KeyCode::Enter));

    assert!(
        matches!(&event, Some(InputEvent::PeekReply { agent_id, text })
            if *agent_id == IDLE && text == "Run the tests again"),
        "{event:?}"
    );
    assert_eq!(state.editor.draft(), "");
    assert!(state.overview.peek.is_some());
    assert!(
        state.key(key(KeyCode::Enter)).is_none(),
        "an empty reply sends nothing"
    );

    type_text(&mut state, "/compact");
    assert!(state.key(key(KeyCode::Enter)).is_none());
    assert!(
        state
            .status
            .contains("Commands run in the Agent's own view")
    );
    assert_eq!(state.editor.draft(), "/compact");
}

#[test]
fn a_reply_to_an_ended_agent_goes_to_its_row_and_one_without_history_is_refused() {
    let console = fleet();
    let mut state = console.state.lock().unwrap();
    select(&mut state, ENDED_CHAT);
    space(&mut state);
    let frame = drawn(&mut state);
    assert!(
        frame
            .iter()
            .any(|line| line.contains("A reply loads its saved history, then sends")),
        "{frame:#?}"
    );
    type_text(&mut state, "Continue the review");

    let event = state.key(key(KeyCode::Enter));

    assert!(
        matches!(&event, Some(InputEvent::PeekReply { agent_id, .. }) if *agent_id == ENDED_CHAT),
        "{event:?}"
    );
    drop(state);

    let console = overview_console(vec![ended_row(false)]);
    let mut state = console.state.lock().unwrap();
    select(&mut state, ENDED_CHAT);
    space(&mut state);
    type_text(&mut state, "Continue");
    assert!(state.key(key(KeyCode::Enter)).is_none());
    assert!(
        state.status.contains("no longer on disk"),
        "{}",
        state.status
    );
}

#[test]
fn up_and_down_move_the_panel_to_the_neighbouring_agent_and_right_opens_it() {
    let console = fleet();
    let mut state = console.state.lock().unwrap();
    select(&mut state, WAITING);
    space(&mut state);

    let event = state.key(key(KeyCode::Down));

    assert!(
        matches!(event, Some(InputEvent::PeekOpened(id)) if id == IDLE),
        "{event:?}"
    );
    assert_eq!(state.overview.selected_agent(), Some(IDLE));
    assert_eq!(state.overview.peek.as_ref().unwrap().agent_id, IDLE);
    let frame = drawn(&mut state);
    assert!(
        frame
            .iter()
            .any(|line| line.starts_with(SELECTED_MARK) && line.contains("Fix the parser")),
        "{frame:#?}"
    );
    assert!(
        matches!(state.key(key(KeyCode::Up)), Some(InputEvent::PeekOpened(id)) if id == WAITING)
    );
    assert!(
        state.key(key(KeyCode::Up)).is_none(),
        "the first Agent row has no row above"
    );

    let event = state.key(key(KeyCode::Right));
    assert!(
        matches!(event, Some(InputEvent::OpenAgent(id)) if id == WAITING),
        "{event:?}"
    );
    assert!(state.overview.peek.is_none());
}

#[test]
fn headings_still_fold_with_enter_and_right_and_space_on_them_does_nothing() {
    let console = fleet();
    let mut state = console.state.lock().unwrap();
    state.overview.selected = Some(OverviewTarget::Heading(AttentionGroup::Completed));
    assert_eq!(state.overview.row_hints(), ["enter/→ collapse"]);

    assert!(space(&mut state).is_none());
    assert!(state.overview.peek.is_none());
    assert!(
        drawn(&mut state)
            .iter()
            .any(|line| line.contains("▾ Completed"))
    );

    assert!(state.key(key(KeyCode::Enter)).is_none());
    assert!(
        drawn(&mut state)
            .iter()
            .any(|line| line.contains("▸ Completed"))
    );
    assert!(state.key(key(KeyCode::Right)).is_none());
    assert!(
        drawn(&mut state)
            .iter()
            .any(|line| line.contains("▾ Completed"))
    );
}
