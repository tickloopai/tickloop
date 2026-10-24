use super::*;
use crate::{ChatBookkeeping, empty_snapshot};
use serde_json::json;
use tickloop_agent_model::REQUEST_OVERHEAD_TOKENS;
use tickloop_agent_protocol::{
    ApprovalStage, ChatSnapshot, ExecutionMode, ImageMediaType, ImageSource, Message,
    OperationRecord, OperationState, Role,
};
use uuid::Uuid;

const TURNS: usize = 12;
const CALLS_PER_TURN: usize = 100;
const WINDOW_TOKENS: usize = 160_000;
/// Jev's documented 32k state-plus-question tokens at the measured 96,621 bytes per 25,000.
const JEV_REQUEST_LIMIT_BYTES: usize = 32_000 * 96_621 / 25_000;
const IMAGE_DATA: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

struct Session {
    snapshot: ChatSnapshot,
    bookkeeping: ChatBookkeeping,
    loop_id: Uuid,
}

impl Session {
    fn new() -> Self {
        let mut snapshot = empty_snapshot(
            Uuid::new_v4().into(),
            Some("/project".into()),
            tickloop_agent_test_support::config(),
        );
        snapshot.context_window_tokens = Some(WINDOW_TOKENS);
        Self {
            snapshot,
            bookkeeping: ChatBookkeeping::default(),
            loop_id: Uuid::new_v4(),
        }
    }

    fn run(&mut self) -> Uuid {
        let run = Uuid::new_v4();
        self.bookkeeping
            .run_loops
            .insert(run.into(), self.loop_id.into());
        run
    }

    fn push(&mut self, role: Role, content: Vec<Content>, run: Uuid) -> usize {
        let index = self.snapshot.messages.len();
        self.snapshot.messages.push(Message { role, content });
        self.snapshot.message_runs.insert(index, run.into());
        self.snapshot.message_ids.insert(index, Uuid::new_v4());
        index
    }

    fn prompt(&mut self, text: &str, run: Uuid) {
        let index = self.push(Role::User, vec![Content::Text { text: text.into() }], run);
        self.snapshot
            .prompt_execution_modes
            .insert(index, ExecutionMode::Auto);
    }

    fn agent_message(&mut self, content: Vec<Content>, run: Uuid) {
        let index = self.push(Role::User, content, run);
        let id = self.snapshot.message_ids[&index];
        self.snapshot
            .agent_message_origins
            .insert(id, json!({"from": "worker"}));
    }

    /// Proposes `Bash cargo test --workspace` after `prompt` and captures its approval snapshot.
    fn propose(mut self, prompt: &str) -> (Self, ApprovalSnapshot) {
        let run = self.run();
        self.prompt(prompt, run);
        let input = json!({"command": "cargo test --workspace"});
        self.push(
            Role::Assistant,
            vec![
                Content::Text {
                    text: "Running the workspace tests.".into(),
                },
                Content::ToolUse {
                    id: "proposed".into(),
                    name: "Bash".into(),
                    input: input.clone(),
                },
            ],
            run,
        );
        let operation = OperationRecord {
            task_binding: None,
            id: Uuid::new_v4(),
            loop_id: self.loop_id.into(),
            run_id: run.into(),
            call_id: "proposed".into(),
            name: "Bash".into(),
            input,
            file_change: None,
            state: OperationState::Proposed,
            result: None,
        };
        let complete = crate::approval_context::capture(
            &tickloop_ids::IdContext::new(),
            &self.snapshot,
            &self.bookkeeping,
            &operation,
            None,
            None,
            self.snapshot.folder.as_deref().unwrap(),
            tickloop_agent_config::AutoSettings::default().policy(),
        )
        .unwrap();
        (self, complete)
    }
}

/// A pasted log with distinct ends, mixing multi-byte text so cuts must respect char boundaries.
fn paste(bytes: usize) -> String {
    let line = "日志 line: compiling crate 构建中\n";
    format!("PASTE-HEAD {} PASTE-TAIL", line.repeat(bytes / line.len()))
}

/// Twelve finished turns: a 300 KB paste, long replies and Agent reports, 1,200 tool calls.
fn long_session(current_prompt: &str) -> (Session, ApprovalSnapshot) {
    let mut session = Session::new();
    for turn in 0..TURNS {
        let run = session.run();
        let prompt = if turn == 0 {
            paste(300 * 1024)
        } else {
            format!("Continue with step {turn}")
        };
        session.prompt(&prompt, run);
        let mut reply = vec![Content::Text {
            text: format!("Reply {turn}: {}", "analysis ".repeat(2_200)),
        }];
        reply.extend((0..CALLS_PER_TURN).map(|call| Content::ToolUse {
            id: format!("call-{turn}-{call}"),
            name: "Bash".into(),
            input: json!({"command": format!("cargo test -p crate{call} -- step {turn}")}),
        }));
        session.push(Role::Assistant, reply, run);
        let results = (0..CALLS_PER_TURN)
            .map(|call| Content::ToolResult {
                tool_use_id: format!("call-{turn}-{call}"),
                content: "ok".into(),
                is_error: false,
            })
            .collect();
        session.push(Role::User, results, run);
        let mut report = vec![Content::Text {
            text: format!("Report {turn}: {}", "finding ".repeat(3_700)),
        }];
        if turn == 0 {
            report.push(Content::Image {
                source: ImageSource::Base64 {
                    media_type: ImageMediaType::Png,
                    data: IMAGE_DATA.repeat(1_000),
                },
                width: 1,
                height: 1,
            });
        }
        session.agent_message(report, run);
    }
    session.propose(current_prompt)
}

fn short_session() -> (Session, ApprovalSnapshot) {
    Session::new().propose("Run the workspace tests")
}

fn texts(request: &tickloop_agent_model::ModelRequest) -> (&str, &str) {
    let [system, user] = request.messages.as_slice() else {
        panic!("classifier requests have a system and a user message")
    };
    let (Content::Text { text: system }, Content::Text { text: user }) =
        (&system.content[0], &user.content[0])
    else {
        panic!("classifier messages are text")
    };
    (system, user)
}

fn same_json(left: &impl Serialize, right: &impl Serialize) -> bool {
    serde_json::to_value(left).unwrap() == serde_json::to_value(right).unwrap()
}

fn assert_kept_evidence(complete: &ApprovalSnapshot, fitted: &ApprovalSnapshot) {
    assert!(same_json(&complete.operation, &fitted.operation));
    assert!(same_json(&complete.auto_policy, &fitted.auto_policy));
    assert_eq!(complete.working_directory, fitted.working_directory);
    assert_eq!(
        complete.additional_directories,
        fitted.additional_directories
    );
    assert_eq!(
        complete.permission_policy_revision,
        fitted.permission_policy_revision
    );
    let latest = complete.original_user_inputs.last().unwrap();
    let kept = fitted.original_user_inputs.last().unwrap();
    assert_eq!(kept.message_id, latest.message_id);
    for input in complete
        .original_user_inputs
        .iter()
        .filter(|input| input.run_id == complete.operation.run_id)
    {
        assert!(
            fitted
                .original_user_inputs
                .iter()
                .any(|kept| kept.message_id == input.message_id)
        );
    }
}

#[test]
fn a_long_session_fits_each_reviewer_and_marks_what_was_cut() {
    let current = "Run the workspace tests now";
    let (session, complete) = long_session(current);
    let complete_json = serde_json::to_string(&complete).unwrap();
    assert!(complete_json.len() > 1024 * 1024);
    assert!(!complete_json.contains(IMAGE_DATA));
    assert!(complete_json.contains(crate::approval_context::IMAGE_PLACEHOLDER));

    let budget = crate::auto_mode::review_budget_bytes(
        &session.snapshot.model,
        crate::context::context_window(&session.snapshot),
    );
    let fitted = fit(&complete, budget);
    assert!(json_len(&fitted) <= budget);
    assert_kept_evidence(&complete, &fitted);
    assert_eq!(fitted.original_user_inputs.last().unwrap().text, current);
    let output_tokens = session.snapshot.model.max_output_tokens as usize;
    for stage in [ApprovalStage::RiskScreen, ApprovalStage::Authorization] {
        let request =
            crate::auto_mode::classifier_request(&tickloop_ids::IdContext::new(), &fitted, stage);
        let (system, user) = texts(&request);
        let estimated_tokens = (system.len() + user.len())
            .div_ceil(crate::context::ESTIMATED_UTF8_BYTES_PER_TOKEN)
            + REQUEST_OVERHEAD_TOKENS;
        assert!(estimated_tokens <= WINDOW_TOKENS - output_tokens);
        assert!(system.ends_with(crate::auto_rules::TRUNCATION_GUIDANCE));
        assert!(user.starts_with(r#"{"auto_policy":"#));
    }

    let truncation = fitted.context_truncation.as_ref().unwrap();
    assert_eq!(truncation.agent_messages.omitted_oldest, TURNS);
    assert!(fitted.agent_messages.is_empty());
    assert!(truncation.reply_history.omitted_oldest >= TURNS);
    assert!(!fitted.reply_history.complete);
    assert!(truncation.tool_history.omitted_oldest > 0);
    assert!(!fitted.tool_history.complete);
    let calls = &fitted.tool_history.calls;
    assert!(!calls.is_empty() && calls.len() < TURNS * CALLS_PER_TURN);
    assert_eq!(
        calls.last().unwrap().call_id,
        complete.tool_history.calls.last().unwrap().call_id,
        "the most recent calls are kept"
    );
    let pasted = &fitted.original_user_inputs[0].text;
    assert_eq!(truncation.original_user_inputs.shortened, 1);
    assert!(pasted.starts_with("PASTE-HEAD") && pasted.ends_with("PASTE-TAIL"));
    assert!(pasted.contains("bytes omitted by the host to fit the review context"));
    assert!(truncation.original_user_inputs.omitted_bytes > 200 * 1024);
    assert!(complete.context_truncation.is_none() && complete.tool_history.complete);

    let jev_budget = crate::jev_review::review_budget_bytes();
    let fitted = fit(&complete, jev_budget);
    assert!(json_len(&fitted) <= jev_budget);
    assert_kept_evidence(&complete, &fitted);
    for stage in [ApprovalStage::RiskScreen, ApprovalStage::Authorization] {
        let request =
            crate::jev_review::review_request(&tickloop_ids::IdContext::new(), &fitted, stage)
                .unwrap();
        assert!(serde_json::to_vec(&request).unwrap().len() <= JEV_REQUEST_LIMIT_BYTES);
        let tickloop_agent_model::judgment::Question::Choice { instructions, .. } =
            &request.questions["decision"]
        else {
            panic!("Jev reviews one Choice")
        };
        assert_eq!(
            instructions["context"],
            crate::auto_rules::TRUNCATION_GUIDANCE
        );
    }
}

#[test]
fn a_short_session_is_reviewed_unchanged() {
    let (session, complete) = short_session();
    let complete_json = serde_json::to_string(&complete).unwrap();
    assert!(!complete_json.contains("context_truncation"));
    assert!(!complete_json.contains("capacity_exceeded"));
    for budget in [
        crate::auto_mode::review_budget_bytes(
            &session.snapshot.model,
            crate::context::context_window(&session.snapshot),
        ),
        crate::jev_review::review_budget_bytes(),
    ] {
        let fitted = fit(&complete, budget);
        assert_eq!(serde_json::to_string(&fitted).unwrap(), complete_json);
        for stage in [ApprovalStage::RiskScreen, ApprovalStage::Authorization] {
            let fitted_request = crate::auto_mode::classifier_request(
                &tickloop_ids::IdContext::new(),
                &fitted,
                stage,
            );
            let complete_request = crate::auto_mode::classifier_request(
                &tickloop_ids::IdContext::new(),
                &complete,
                stage,
            );
            assert_eq!(texts(&fitted_request), texts(&complete_request));
            assert!(
                !texts(&fitted_request)
                    .0
                    .contains(crate::auto_rules::TRUNCATION_GUIDANCE)
            );
            let jev =
                crate::jev_review::review_request(&tickloop_ids::IdContext::new(), &fitted, stage)
                    .unwrap();
            assert_eq!(
                serde_json::to_value(&jev).unwrap(),
                serde_json::to_value(
                    crate::jev_review::review_request(
                        &tickloop_ids::IdContext::new(),
                        &complete,
                        stage
                    )
                    .unwrap()
                )
                .unwrap()
            );
            assert!(!serde_json::to_string(&jev).unwrap().contains("\"context\""));
        }
    }
}

#[test]
fn trimming_follows_the_fixed_oldest_first_order_at_every_budget() {
    let (_, complete) = long_session("Run the workspace tests now");
    let complete_size = json_len(&complete);
    let complete_turns = complete
        .reply_history
        .contexts
        .iter()
        .flat_map(|context| &context.preceding)
        .map(|turn| turn.message_id)
        .collect::<Vec<_>>();
    let mandatory_size = json_len(&fit(&complete, 0));
    let mut phases_seen = [false; 4];
    for percent in [80, 55, 40, 30, 22, 16, 12, 9, 7, 3, 0] {
        let budget = complete_size * percent / 100;
        let fitted = fit(&complete, budget);
        assert_kept_evidence(&complete, &fitted);
        let Some(truncation) = &fitted.context_truncation else {
            panic!("a smaller budget always cuts this session")
        };
        let size = json_len(&fitted);
        assert!(
            size <= budget || size == mandatory_size,
            "only the never-omitted evidence may exceed the budget"
        );
        let turns = fitted
            .reply_history
            .contexts
            .iter()
            .flat_map(|context| &context.preceding)
            .map(|turn| turn.message_id)
            .collect::<Vec<_>>();
        assert!(
            complete_turns.ends_with(&turns),
            "reply turns keep the newest"
        );
        assert!(
            complete
                .tool_history
                .calls
                .iter()
                .map(|call| &call.call_id)
                .collect::<Vec<_>>()
                .ends_with(
                    &fitted
                        .tool_history
                        .calls
                        .iter()
                        .map(|call| &call.call_id)
                        .collect::<Vec<_>>()
                ),
            "tool history keeps the newest calls"
        );
        if truncation.reply_history.omitted_oldest > 0 {
            assert!(fitted.agent_messages.is_empty());
        }
        if truncation.tool_history.omitted_oldest > 0 {
            assert!(turns.is_empty());
        }
        if truncation.original_user_inputs.omitted_oldest > 0 {
            assert!(fitted.tool_history.calls.is_empty());
        }
        let section_partly_kept = [
            truncation.agent_messages.omitted_oldest > 0 && !fitted.agent_messages.is_empty(),
            truncation.reply_history.omitted_oldest > 0 && !turns.is_empty(),
            truncation.tool_history.omitted_oldest > 0 && !fitted.tool_history.calls.is_empty(),
            truncation.original_user_inputs.omitted_oldest > 0,
        ];
        for (seen, partly_kept) in phases_seen.iter_mut().zip(section_partly_kept) {
            *seen |= partly_kept;
        }
    }
    assert_eq!(
        phases_seen, [true; 4],
        "the budgets exercise every omission phase"
    );
}

#[test]
fn an_oversized_current_input_keeps_its_head_and_tail_when_nothing_else_fits() {
    let current = paste(200 * 1024);
    let (_, complete) = long_session(&current);
    let fitted = fit(&complete, 0);
    assert_kept_evidence(&complete, &fitted);
    assert!(fitted.agent_messages.is_empty());
    assert!(fitted.tool_history.calls.is_empty());
    assert!(
        fitted
            .reply_history
            .contexts
            .iter()
            .all(|context| context.preceding.is_empty())
    );
    let [kept] = fitted.original_user_inputs.as_slice() else {
        panic!("only the current input remains")
    };
    assert!(kept.text.starts_with("PASTE-HEAD") && kept.text.ends_with("PASTE-TAIL"));
    assert!(
        kept.text
            .contains("bytes omitted by the host to fit the review context")
    );
    assert!(kept.text.len() < 2 * MIN_KEPT_INPUT_BYTES);
    let truncation = fitted.context_truncation.unwrap();
    assert_eq!(truncation.original_user_inputs.omitted_oldest, TURNS);
}

#[test]
fn a_context_limit_rejection_shrinks_the_evidence_once_to_half_of_what_was_sent() {
    let (session, complete) = long_session("Run the workspace tests now");
    let mut evidence = ReviewEvidence::new(
        &complete,
        crate::auto_mode::review_budget_bytes(
            &session.snapshot.model,
            crate::context::context_window(&session.snapshot),
        ),
    );
    let sent = json_len(evidence.snapshot());
    assert!(evidence.shrink());
    assert!(json_len(evidence.snapshot()) <= sent / 2);
    assert_kept_evidence(&complete, evidence.snapshot());
    assert!(!evidence.shrink(), "the smaller budget is tried only once");

    let (session, complete) = short_session();
    let mut evidence = ReviewEvidence::new(
        &complete,
        crate::auto_mode::review_budget_bytes(
            &session.snapshot.model,
            crate::context::context_window(&session.snapshot),
        ),
    );
    assert!(
        !evidence.shrink(),
        "a snapshot with nothing to omit is not resent"
    );
    assert!(same_json(evidence.snapshot(), &complete));
}
