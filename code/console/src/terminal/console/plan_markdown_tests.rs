//! Plans read as Markdown in the interactive Console: the approval and saved-plan dialogs and
//! the conversation lay the plan out for the current width, as they do an assistant reply.
use super::*;
use crate::terminal::{execution::ConsoleExecution, menus::Menus, workflows::Workflow};
use tickloop_harness::protocol::{EventKind, ExecutionMode};

const PLAN_MARKDOWN: &str =
    "## Steps\n\n* Read the **release notes** before changing anything\n* Write the summary";

fn rows(lines: &[RoleLine]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.text().trim_end().to_owned())
        .collect()
}

fn assert_no_markdown_syntax(rows: &[String]) {
    for row in rows {
        assert!(!row.contains("## "), "{rows:#?}");
        assert!(!row.contains("**"), "{rows:#?}");
        assert!(!row.trim_start().starts_with("* "), "{rows:#?}");
    }
}

/// An embedded Agent whose Chat waits for approval of `markdown`, with its approval dialog open
/// in a Console of `width` columns.
async fn plan_approval(markdown: &str, width: u16) -> (ConsoleExecution, Console) {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
        tickloop_harness::test_support::submit_plan("plan", markdown),
    ]));
    let agent = ConsoleExecution::new_embedded(tickloop_harness::test_support::config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Plan the task".into()).await.unwrap();
    while !matches!(
        tickloop_harness::test_support::next_event(&mut events)
            .await
            .kind,
        EventKind::PermissionRequested { .. }
    ) {}
    let mut state = State::new(true, 80, 24);
    state.resize(width, 24);
    let console = tests::console_with_state(state);
    console.select_chat(Some(chat)).unwrap();
    let mut menus = Menus::default();
    menus
        .workflow(&console, Workflow::plan(&agent, chat).await.unwrap())
        .unwrap();
    (agent, console)
}

#[tokio::test]
async fn plan_approval_dialog_lays_the_plan_out_as_markdown_and_reflows_it_when_narrowed() {
    let (agent, console) = plan_approval(PLAN_MARKDOWN, 80).await;
    {
        let mut state = console.state.lock().unwrap();
        assert!(state.menu_plan);
        let dialog = rows(&state.input_dialog_frame().0);
        assert_no_markdown_syntax(&dialog);
        assert!(
            dialog.contains(&"  Plan version: 1".to_owned()),
            "{dialog:#?}"
        );
        assert!(dialog.contains(&"  Steps".to_owned()), "{dialog:#?}");
        assert!(
            dialog.contains(&"  - Read the release notes before changing anything".to_owned()),
            "{dialog:#?}"
        );
        assert!(
            dialog.contains(&"  - Write the summary".to_owned()),
            "{dialog:#?}"
        );

        // After the terminal narrows, the full plan lays the same Markdown out again: a wrapped
        // item continues under its text, not under its marker.
        state.resize(25, 24);
        state.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(state.full_review);
        let full = rows(&state.full_review_frame());
        assert_no_markdown_syntax(&full);
        let item = full
            .iter()
            .position(|row| row == "- Read the release notes")
            .unwrap_or_else(|| panic!("{full:#?}"));
        assert_eq!(full[item + 1], "  before changing", "{full:#?}");
        assert_eq!(full[item + 2], "  anything", "{full:#?}");
        assert_eq!(full[item + 3], "- Write the summary", "{full:#?}");
    }
    agent.shutdown().await;
}

#[test]
fn a_plan_in_the_conversation_is_laid_out_as_markdown_for_the_current_width() {
    let console = tests::console_with_state(State::new(true, 80, 24));
    console
        .write_plan("Plan version 1:", PLAN_MARKDOWN)
        .unwrap();
    let mut state = console.state.lock().unwrap();
    let conversation = |state: &mut State, width| {
        state
            .scrollback
            .frame(width, 20)
            .lines
            .iter()
            .map(|row| row.trim_end().to_owned())
            .collect::<Vec<_>>()
    };

    let wide = conversation(&mut state, 80);
    assert_no_markdown_syntax(&wide);
    let heading = wide
        .iter()
        .position(|row| row == "Plan version 1:")
        .unwrap_or_else(|| panic!("{wide:#?}"));
    assert_eq!(
        wide[heading + 1..heading + 5],
        [
            "  Steps",
            "",
            "  - Read the release notes before changing anything",
            "  - Write the summary",
        ],
        "{wide:#?}"
    );

    let narrow = conversation(&mut state, 24);
    assert_no_markdown_syntax(&narrow);
    let item = narrow
        .iter()
        .position(|row| row == "  - Read the release")
        .unwrap_or_else(|| panic!("{narrow:#?}"));
    assert_eq!(
        narrow[item + 1..item + 3],
        ["    notes before", "    changing anything"],
        "{narrow:#?}"
    );
}

#[tokio::test]
async fn a_plan_taller_than_the_dialog_scrolls_to_its_last_markdown_row() {
    let markdown = std::iter::once("## Long plan\n".to_owned())
        .chain((1..=30).map(|step| format!("* Step **{step}**")))
        .collect::<Vec<_>>()
        .join("\n");
    let (agent, console) = plan_approval(&markdown, 80).await;
    {
        let mut state = console.state.lock().unwrap();
        let dialog = rows(&state.input_dialog_frame().0);
        assert!(!dialog.contains(&"  - Step 30".to_owned()), "{dialog:#?}");
        assert!(
            dialog.last().unwrap().contains("pgup/pgdn plan"),
            "{dialog:#?}"
        );
        for _ in 0..40 {
            state.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        }
        let dialog = rows(&state.input_dialog_frame().0);
        assert_no_markdown_syntax(&dialog);
        assert!(dialog.contains(&"  - Step 30".to_owned()), "{dialog:#?}");
        assert!(dialog.contains(&"  - Step 29".to_owned()), "{dialog:#?}");
        state.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        let dialog = rows(&state.input_dialog_frame().0);
        assert!(dialog.contains(&"  - Step 28".to_owned()), "{dialog:#?}");
        assert!(!dialog.contains(&"  - Step 30".to_owned()), "{dialog:#?}");
    }
    agent.shutdown().await;
}
