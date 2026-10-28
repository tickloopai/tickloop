use super::*;

const KEEP_PLANNING: usize = 3;
const FEEDBACK_APPROVAL: usize = 1;

fn plan_review(selected: usize) -> (Console, Uuid, Arc<Mutex<Vec<String>>>) {
    let console = tests::console_with_state(State::new(true, 80, 24));
    let id = Uuid::new_v4();
    console
        .show_plan_menu(
            Menu {
                id,
                title: "Plan approval".into(),
                context: vec!["Plan version: 1".into(), "Original plan".into()],
                options: vec![
                    "Approve and start · Auto".into(),
                    "Approve and start · Ask".into(),
                    "Approve and start · Full Access".into(),
                    "No, keep planning".into(),
                ],
                selected,
                confirmation: true,
                markdown: None,
            },
            true,
        )
        .unwrap();
    console
        .offer_plan_review(PlanReview {
            menu_id: id,
            markdown: "Original plan".into(),
            feedback_approval: FEEDBACK_APPROVAL,
        })
        .unwrap();
    let opened = Arc::new(Mutex::new(Vec::new()));
    console.state.lock().unwrap().external_editor = ExternalEditorLauncher::Scripted {
        opened_drafts: Arc::clone(&opened),
        outcome: ExternalEditorOutcome::Updated {
            content: "Edited text".into(),
        },
    };
    (console, id, opened)
}

fn control(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
}

fn plain(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn hint_row(state: &mut State) -> String {
    state.draw(&mut Vec::new()).unwrap();
    state
        .rendered_screen
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .cloned()
        .unwrap_or_default()
}

#[test]
fn ctrl_g_in_plan_review_opens_the_plan_in_the_external_editor() {
    let (console, id, opened) = plan_review(0);
    let mut state = console.state.lock().unwrap();
    let hint = hint_row(&mut state);
    assert!(hint.contains("ctrl+g edit plan"), "{hint}");

    let event = state.key(control('g'));
    assert!(
        matches!(&event, Some(InputEvent::PlanEdited { menu_id, markdown }) if *menu_id == id && markdown == "Edited text"),
        "{event:?}"
    );
    assert_eq!(*opened.lock().unwrap(), ["Original plan"]);
    assert!(state.menu.is_some(), "the review stays open for the edit");

    state.external_editor = ExternalEditorLauncher::Scripted {
        opened_drafts: Arc::clone(&opened),
        outcome: ExternalEditorOutcome::Unchanged,
    };
    assert!(state.key(control('g')).is_none());
}

#[test]
fn a_plan_edit_requested_by_command_runs_once_in_the_open_review() {
    let (console, id, opened) = plan_review(0);
    assert!(
        console
            .state
            .lock()
            .unwrap()
            .take_requested_plan_edit()
            .is_none()
    );
    console
        .request_plan_edit(RequestedPlanEdit::Review)
        .unwrap();
    let mut state = console.state.lock().unwrap();
    assert!(matches!(
        state.take_requested_plan_edit(),
        Some(Some(InputEvent::PlanEdited { menu_id, .. })) if menu_id == id
    ));
    assert!(state.take_requested_plan_edit().is_none());
    assert_eq!(opened.lock().unwrap().len(), 1);
}

#[test]
fn the_feedback_field_needs_text_to_keep_planning_and_shift_tab_approves_with_it() {
    let (console, id, opened) = plan_review(KEEP_PLANNING);
    let mut state = console.state.lock().unwrap();
    assert!(state.key(plain(KeyCode::Enter)).is_none());
    assert!(state.menu_comment_is_editing());
    assert!(
        state.key(plain(KeyCode::Enter)).is_none(),
        "empty feedback neither keeps planning nor cancels"
    );
    assert!(state.menu_comment_is_editing());
    assert!(state.menu.is_some());
    let hint = hint_row(&mut state);
    for part in [
        "enter keep planning",
        "shift+tab approve in Ask",
        "esc close",
    ] {
        assert!(hint.contains(part), "{part}: {hint}");
    }

    // Ctrl+G edits the field being typed in, not the plan.
    assert!(state.key(control('g')).is_none());
    assert_eq!(*opened.lock().unwrap(), [""]);
    assert_eq!(
        state.selected_menu_comment().as_deref(),
        Some("Edited text")
    );
    assert!(state.menu_comment_is_editing());

    let event = state.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert!(
        matches!(&event, Some(InputEvent::Choice { menu_id, index: FEEDBACK_APPROVAL, comment: Some(feedback) }) if *menu_id == id && feedback == "Edited text"),
        "{event:?}"
    );
}

#[test]
fn a_plan_file_edit_requested_by_command_opens_the_file_itself() {
    let (console, _, opened) = plan_review(0);
    console
        .request_plan_edit(RequestedPlanEdit::File("/plans/plan.md".into()))
        .unwrap();
    let mut state = console.state.lock().unwrap();
    assert!(matches!(state.take_requested_plan_edit(), Some(None)));
    assert_eq!(*opened.lock().unwrap(), ["/plans/plan.md"]);
    assert_eq!(state.status, "Plan file saved");
}

#[tokio::test]
async fn plan_enters_plan_mode_despite_an_earlier_plan_and_then_shows_or_opens_it() {
    use crate::terminal::{execution::ConsoleExecution, menus::Menus, run_plan_command};
    use tickloop_harness::protocol::{EventKind, ExecutionMode};
    let directory = tempfile::tempdir().unwrap();
    let agent = ConsoleExecution::new_embedded(
        tickloop_harness::test_support::config(),
        Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::submit_plan("plan", "Earlier plan."),
            tickloop_harness::test_support::text("Earlier work done."),
        ])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Plan it".into()).await.unwrap();
    loop {
        match tickloop_harness::test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::PermissionRequested { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent
        .approve_plan(chat, 1, ExecutionMode::Auto, None)
        .await
        .unwrap();
    loop {
        match tickloop_harness::test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let mode = |agent: &ConsoleExecution| {
        let agent = agent.clone();
        async move { agent.snapshot(chat).await.unwrap().policy.mode }
    };
    assert_eq!(mode(&agent).await, ExecutionMode::Auto);
    let (console, _, opened) = plan_review(0);
    console.state.lock().unwrap().menu = None;
    let mut menus = Menus::default();

    assert_eq!(
        run_plan_command(&agent, &console, &mut menus, Some(chat.into()), "")
            .await
            .unwrap(),
        None
    );
    assert_eq!(mode(&agent).await, ExecutionMode::Plan);
    assert!(menus.id().is_none(), "entering Plan opens no dialog");

    run_plan_command(&agent, &console, &mut menus, Some(chat.into()), "")
        .await
        .unwrap();
    assert_eq!(
        console
            .menu_snapshot(menus.id().expect("in Plan, /plan shows the plan"))
            .unwrap()
            .map(|menu| menu.title),
        Some("Saved plan".into())
    );

    run_plan_command(&agent, &console, &mut menus, Some(chat.into()), " open ")
        .await
        .unwrap();
    assert!(matches!(
        console.state.lock().unwrap().take_requested_plan_edit(),
        Some(None)
    ));
    let opened_file = opened.lock().unwrap().last().cloned().unwrap();
    assert!(opened_file.ends_with(".md"), "{opened_file}");
    assert_eq!(
        std::fs::read_to_string(&opened_file)
            .unwrap()
            .lines()
            .nth(1),
        Some("Earlier plan.")
    );

    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    assert_eq!(
        run_plan_command(
            &agent,
            &console,
            &mut menus,
            Some(chat.into()),
            " Plan the follow-up "
        )
        .await
        .unwrap()
        .as_deref(),
        Some("Plan the follow-up")
    );
    assert_eq!(mode(&agent).await, ExecutionMode::Plan);

    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    assert_eq!(
        run_plan_command(
            &agent,
            &console,
            &mut menus,
            Some(chat.into()),
            "open the notes first"
        )
        .await
        .unwrap()
        .as_deref(),
        Some("open the notes first"),
        "only a bare open opens the plan"
    );
    agent.shutdown().await;
}
