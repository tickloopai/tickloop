use super::*;
use tickloop_harness_protocol::{
    Content, Message, ModelConfig, Role, SharedTask, TaskAssignment, TaskClaim, TaskOwner,
};
use tickloop_harness_transcript::{RetainedSnapshotEntry, TaskBoardsEntry};

fn id(value: u64) -> Uuid {
    Uuid::from_u128(value.into())
}

fn root() -> ChatId {
    id(1000).into()
}

fn loop_id() -> LoopId {
    id(2000).into()
}

fn node(number: u64, parent: Option<u64>, role: Role) -> Line {
    let entry = serde_json::from_value::<ChainEntry>(serde_json::json!({
        "uuid":id(number), "parentUuid":parent.map(id), "chatId":root().uuid(),
        "timestamp":tickloop_harness_base::timestamp::format_timestamp(number * 1000),
        "version":"fixture",
        "message":Message {role, content: vec![Content::Text { text: "text".into() }]},
    }))
    .unwrap();
    Line::chain(role, entry)
}

fn prompt(number: u64, parent: Option<u64>) -> Line {
    node(number, parent, Role::User)
}

fn reply(number: u64, parent: u64) -> Line {
    node(number, Some(parent), Role::Assistant)
}

fn task(number: u64, title: &str) -> SharedTask {
    SharedTask {
        id: id(number).into(),
        title: title.into(),
        description: title.into(),
        dependencies: Vec::new(),
        status: SharedTaskStatus::Pending,
        owner: None,
        claim: None,
        delegated_runs: Vec::new(),
        revision: 1,
        result: None,
        report: None,
    }
}

fn board(revision: u64, titles: &[&str]) -> TaskBoard {
    TaskBoard {
        revision,
        tasks: titles
            .iter()
            .enumerate()
            .map(|(index, title)| task(index as u64 + 1, title))
            .collect(),
        ..TaskBoard::new(root(), loop_id())
    }
}

fn checkpoint(leaf: u64, board: TaskBoard) -> Line {
    Line::TaskBoardCheckpoint(TaskBoardCheckpointEntry {
        chat_id: root().uuid(),
        leaf: id(leaf),
        boards: BTreeMap::from([(loop_id().uuid(), board)]),
    })
}

fn latest(board: TaskBoard) -> Line {
    Line::TaskBoards(TaskBoardsEntry {
        chat_id: root().uuid(),
        boards: BTreeMap::from([(loop_id().uuid(), board)]),
    })
}

fn titles_at(lines: &[Line], leaf: u64) -> Vec<String> {
    task_boards_at(lines, id(leaf), true)
        .unwrap()
        .iter()
        .flat_map(|board| &board.tasks)
        .map(|task| task.title.clone())
        .collect()
}

/// Two answers to an edited prompt, each with the boards recorded where it ended.
fn two_branches() -> Vec<Line> {
    vec![
        prompt(1, None),
        reply(2, 1),
        checkpoint(2, board(2, &["plan"])),
        latest(board(2, &["plan"])),
        prompt(3, Some(2)),
        reply(4, 3),
        checkpoint(4, board(3, &["plan", "first take"])),
        latest(board(3, &["plan", "first take"])),
        prompt(5, Some(2)),
        reply(6, 5),
        checkpoint(6, board(5, &["plan", "second take"])),
        latest(board(5, &["plan", "second take"])),
    ]
}

#[test]
fn each_branch_shows_the_boards_recorded_nearest_above_the_entry() {
    let lines = two_branches();
    assert!(titles_at(&lines, 1).is_empty());
    assert_eq!(titles_at(&lines, 2), ["plan"]);
    assert_eq!(titles_at(&lines, 3), ["plan"]);
    assert_eq!(titles_at(&lines, 4), ["plan", "first take"]);
    assert_eq!(titles_at(&lines, 5), ["plan"]);
    assert_eq!(titles_at(&lines, 6), ["plan", "second take"]);
    assert_eq!(task_boards_at(&lines, id(4), true).unwrap()[0].revision, 3);
    assert_eq!(task_boards_at(&lines, id(4), false), Ok(Vec::new()));
}

#[test]
fn a_later_record_of_the_same_entry_wins_and_boards_without_tasks_are_left_out() {
    let mut lines = two_branches();
    lines.push(checkpoint(4, board(6, &[])));
    assert!(task_boards_at(&lines, id(4), true).unwrap().is_empty());
    assert_eq!(titles_at(&lines, 6), ["plan", "second take"]);
}

#[test]
fn records_a_retained_snapshot_repeats_are_found() {
    let lines = vec![
        prompt(1, None),
        reply(2, 1),
        Line::RetainedSnapshot(RetainedSnapshotEntry {
            chat_id: root().uuid(),
            lines: vec![checkpoint(2, board(2, &["kept"]))],
        }),
        prompt(3, Some(2)),
    ];
    assert_eq!(titles_at(&lines, 3), ["kept"]);
}

#[test]
fn a_transcript_without_records_by_place_shows_its_latest_boards() {
    let lines = vec![
        prompt(1, None),
        reply(2, 1),
        latest(board(2, &["old"])),
        prompt(3, Some(2)),
        reply(4, 3),
        latest(board(3, &["old", "newest"])),
    ];
    for leaf in [1, 2, 4] {
        assert_eq!(
            task_boards_at(&lines, id(leaf), true).unwrap(),
            display::task_boards(&lines, true)
        );
    }
    assert_eq!(titles_at(&lines, 1), ["old", "newest"]);
}

#[test]
fn a_missing_leaf_or_ancestry_is_an_error() {
    assert_eq!(
        task_boards_at(&[], id(1), true),
        Err(TaskBoardQueryError::LeafNotFound(id(1)))
    );
    assert_eq!(
        task_boards_at(&two_branches(), id(99), false),
        Err(TaskBoardQueryError::LeafNotFound(id(99)))
    );
    assert_eq!(
        task_boards_at(&[prompt(1, Some(999))], id(1), true),
        Err(TaskBoardQueryError::IncompleteHistory(id(1)))
    );
}

#[test]
fn a_record_of_an_entry_between_messages_stands_at_the_message_before_it() {
    let chain = [prompt(1, None), reply(2, 1), node(3, Some(2), Role::System)]
        .iter()
        .map(|line| line.chain_entry().unwrap().clone())
        .collect::<Vec<_>>();
    let messages = HashMap::from([(id(1), 0), (id(2), 1)]);
    let checkpoints = [checkpoint(3, board(4, &["after"]))]
        .into_iter()
        .map(|line| match line {
            Line::TaskBoardCheckpoint(entry) => (entry.leaf, entry),
            _ => unreachable!(),
        })
        .collect();
    let by_message = checkpoints_by_message(&chain, &messages, &checkpoints, root().uuid());
    assert_eq!(by_message.keys().copied().collect::<Vec<_>>(), [1]);
    assert_eq!(by_message[&1][&loop_id()].tasks[0].title, "after");
    assert!(checkpoints_by_message(&chain, &messages, &checkpoints, id(7)).is_empty());
}

fn chat(current: TaskBoard) -> (ChatSnapshot, ChatBookkeeping) {
    let mut snapshot = crate::empty_snapshot(root(), None, ModelConfig::unselected());
    snapshot.chat_loop.id = loop_id();
    snapshot.task_boards.insert(loop_id(), current);
    let bookkeeping = ChatBookkeeping {
        has_task_board_checkpoints: true,
        ..ChatBookkeeping::default()
    };
    (snapshot, bookkeeping)
}

fn recorded_at(index: usize, board: TaskBoard) -> (usize, TaskBoards) {
    (index, BTreeMap::from([(loop_id(), board)]))
}

fn receipt(task: u64) -> TaskMutationReceipt {
    TaskMutationReceipt {
        actor: root(),
        input: format!("TaskCreate:{task}"),
        task_id: id(task).into(),
        task_cleared: false,
        notification: Some(TaskAssignment {
            target: id(3000).into(),
            message: "assigned".into(),
            delivered: false,
        }),
    }
}

fn follow(
    chat: &mut (ChatSnapshot, ChatBookkeeping),
    before: usize,
    change: ConversationChange,
    live: &LiveRuns,
) -> Result<bool, String> {
    follow_conversation(&mut chat.0, &mut chat.1, before, change, live)
}

#[test]
fn a_new_answer_takes_the_board_before_its_prompt_and_moves_its_revision_on() {
    let mut before = board(3, &["plan"]);
    before.task_number_offset = 4;
    before.mutations.insert(id(11), receipt(1));
    let mut current = board(7, &["plan", "abandoned"]);
    current.is_team = true;
    current.mutations = BTreeMap::from([(id(11), receipt(1)), (id(12), receipt(2))]);
    let mut chat = chat(current.clone());
    chat.1.task_board_checkpoints =
        BTreeMap::from([recorded_at(1, before.clone()), recorded_at(3, current)]);
    let moved = follow(
        &mut chat,
        2,
        ConversationChange::NewAnswer,
        &LiveRuns::new(),
    );
    assert_eq!(moved, Ok(true));
    let restored = &chat.0.task_boards[&loop_id()];
    assert_eq!(restored.tasks, before.tasks);
    assert_eq!(restored.revision, 8);
    assert_eq!(restored.task_number_offset, 4);
    assert!(restored.is_team);
    assert_eq!(restored.mutations[&id(11)], receipt(1));
    assert!(restored.mutations[&id(12)].task_cleared);
    assert_eq!(restored.mutations[&id(12)].notification, None);
    assert_eq!(
        chat.1
            .task_board_checkpoints
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [1]
    );
}

#[test]
fn a_restored_board_never_goes_back_and_an_equal_one_is_left_alone() {
    let live = LiveRuns::new();
    let mut chat = chat(board(4, &["now"]));
    chat.1.task_board_checkpoints = BTreeMap::from([recorded_at(0, board(10, &["then"]))]);
    assert_eq!(
        follow(&mut chat, 1, ConversationChange::Moved, &live),
        Ok(true)
    );
    assert_eq!(chat.0.task_boards[&loop_id()].revision, 11);

    let recorded = chat.0.task_boards[&loop_id()].clone();
    chat.1.task_board_checkpoints = BTreeMap::from([recorded_at(0, recorded.clone())]);
    assert_eq!(
        follow(&mut chat, 1, ConversationChange::Moved, &live),
        Ok(false)
    );
    assert_eq!(chat.0.task_boards[&loop_id()], recorded);
}

#[test]
fn a_restored_claim_needs_attention_unless_its_run_still_runs() {
    let member: ChatId = id(3000).into();
    let claimed = |number: u64, run: u64| SharedTask {
        status: SharedTaskStatus::InProgress,
        owner: Some(TaskOwner {
            chat_id: member,
            name: "worker".into(),
        }),
        claim: Some(TaskClaim {
            run_id: id(run).into(),
            generation: 1,
        }),
        ..task(number, "claimed")
    };
    let mut target = board(2, &[]);
    target.tasks = vec![claimed(1, 41), claimed(2, 42)];
    let mut chat = chat(board(2, &[]));
    chat.1.task_board_checkpoints = BTreeMap::from([recorded_at(0, target)]);
    let live = LiveRuns::from([((member, id(41).into()), 5)]);
    follow(&mut chat, 1, ConversationChange::Moved, &live).unwrap();
    let tasks = &chat.0.task_boards[&loop_id()].tasks;
    assert_eq!(tasks[0].status, SharedTaskStatus::InProgress);
    assert_eq!(tasks[0].claim.as_ref().unwrap().generation, 5);
    assert_eq!(tasks[0].revision, 1);
    assert_eq!(tasks[1].status, SharedTaskStatus::NeedsAttention);
    assert_eq!(tasks[1].revision, 2);
    assert!(tasks[1].result.as_deref().unwrap().contains("reconcile"));
}

#[test]
fn with_nothing_recorded_a_new_answer_clears_and_an_older_transcript_keeps_its_board_on_a_move() {
    let live = LiveRuns::new();
    let mut current = board(5, &["a", "b"]);
    current.task_number_offset = 1;
    current.mutations.insert(id(11), receipt(1));
    let mut older = chat(current.clone());
    older.1.has_task_board_checkpoints = false;
    assert_eq!(
        follow(&mut older, 2, ConversationChange::Moved, &live),
        Ok(false)
    );
    assert_eq!(older.0.task_boards[&loop_id()], current);

    assert_eq!(
        follow(&mut older, 2, ConversationChange::NewAnswer, &live),
        Ok(true)
    );
    let cleared = &older.0.task_boards[&loop_id()];
    assert!(cleared.tasks.is_empty());
    assert_eq!(cleared.task_number_offset, 3);
    assert_eq!(cleared.revision, 6);
    assert!(cleared.mutations[&id(11)].task_cleared);

    let mut recording = chat(current);
    assert_eq!(
        follow(&mut recording, 2, ConversationChange::Moved, &live),
        Ok(true)
    );
    assert!(recording.0.task_boards[&loop_id()].tasks.is_empty());
}
