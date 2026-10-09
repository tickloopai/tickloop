use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use serde_json::{Value, json};
use tickloop_agent_model::judgment::{
    Answer, Evaluation, EvaluationRequest, JudgmentClient, JudgmentError, Question,
};
use tickloop_agent_protocol::{Content, Decision, EventKind, ExecutionMode, PermissionRule, Usage};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::routed::{RoutedModel, has_tool_result, history_text, state};
use tickloop_agent_test_support::{
    ScriptedModel, auto_classification, call, config, next_event, text,
};
use tokio_util::sync::CancellationToken;

struct ReviewingJudgment {
    block: bool,
    requests: Mutex<Vec<EvaluationRequest>>,
}

#[async_trait::async_trait]
impl JudgmentClient for ReviewingJudgment {
    fn model(&self) -> &str {
        "fixture-jev"
    }

    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        _: CancellationToken,
    ) -> Result<Evaluation, JudgmentError> {
        self.requests.lock().unwrap().push(request.clone());
        let choice = if self.block { "block_soft" } else { "allow" };
        let Question::Choice { criteria, .. } = &request.questions["decision"] else {
            panic!()
        };
        Ok(Evaluation {
            model: self.model().into(),
            usage: Usage::default(),
            answers: BTreeMap::from([(
                "decision".into(),
                Answer::Choice {
                    choice: choice.into(),
                    confidence: 1.0,
                    probabilities: criteria
                        .keys()
                        .map(|option| (option.clone(), f64::from(option == choice)))
                        .collect(),
                },
            )]),
        })
    }
}

async fn finish(agent: &Agent, chat: uuid::Uuid, prompt: &str) -> usize {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, prompt.into()).await.unwrap();
    let mut prompts = 0;
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                prompts += 1;
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => return prompts,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn delegation_and_messages_follow_classifier_modes_and_explicit_rules_before_effects() {
    for tool in ["Subagent", "SendMessage"] {
        for use_jev in [false, true] {
            for mode in [
                ExecutionMode::Auto,
                ExecutionMode::Plan,
                ExecutionMode::AcceptEdits,
                ExecutionMode::FullAccess,
            ] {
                for rule in [
                    None,
                    Some(Decision::Allow),
                    Some(Decision::Ask),
                    Some(Decision::Deny),
                ] {
                    let reviewed = matches!(mode, ExecutionMode::Auto | ExecutionMode::Plan)
                        && match rule {
                            None => true,
                            Some(Decision::Allow) => tool == "Subagent",
                            _ => false,
                        };
                    for block in [false, true].into_iter().filter(|block| !block || reviewed) {
                        let directory = tempfile::tempdir().unwrap();
                        let model = Arc::new(RoutedModel::new(|request| {
                            if !state(request)["delegation"].is_null() {
                                return text("Child received its bounded input");
                            }
                            if !has_tool_result(request, "delegate") {
                                call(
                                    "delegate",
                                    "Subagent",
                                    json!({"task":"Return a bounded fixture result", "name":"peer"}),
                                )
                            } else if history_text(request).contains("MESSAGE_PROBE_REQUEST")
                                && !has_tool_result(request, "send")
                            {
                                call(
                                    "send",
                                    "SendMessage",
                                    json!({"to":"peer", "message":"MESSAGE_PROBE_BODY"}),
                                )
                            } else {
                                text("Root finished")
                            }
                        }));
                        let agent = Agent::new_embedded(config(), model);
                        let chat = agent.create_chat(directory.path().into()).await.unwrap();
                        let child = if tool == "SendMessage" {
                            assert_eq!(
                                finish(&agent, chat, "Create the bounded fixture child").await,
                                0
                            );
                            Some(agent.subagents(chat).await.unwrap()[0].child_chat_id)
                        } else {
                            None
                        };
                        agent.set_execution_mode(chat, mode).await.unwrap();
                        if let Some(decision) = rule {
                            agent
                                .set_permission_rules(
                                    chat,
                                    vec![PermissionRule {
                                        id: uuid::Uuid::new_v4(),
                                        decision,
                                        tool: tool.into(),
                                        specifier: None,
                                        input: None,
                                        loop_id: None,
                                        persistent: false,
                                    }],
                                )
                                .await
                                .unwrap();
                        }
                        let llm = Arc::new(ScriptedModel::new(vec![
                            text(
                                &auto_classification(block).to_string(),
                            );
                            if block { 2 } else { 1 }
                        ]));
                        let jev = Arc::new(ReviewingJudgment {
                            block,
                            requests: Mutex::new(Vec::new()),
                        });
                        if use_jev {
                            agent.set_judgment_client(jev.clone()).await;
                        } else {
                            agent.set_auto_reviewer(llm.clone()).await;
                        }
                        let prompts = finish(
                            &agent,
                            chat,
                            if child.is_some() {
                                "MESSAGE_PROBE_REQUEST: Send the bounded follow-up to peer"
                            } else {
                                "Delegate the bounded fixture task"
                            },
                        )
                        .await;
                        let admitted = rule != Some(Decision::Deny) && !(reviewed && block);
                        let snapshot = agent.snapshot(chat).await.unwrap();
                        let children = agent.subagents(chat).await.unwrap();
                        let requests: Vec<Value> = if use_jev {
                            jev.requests
                                .lock()
                                .unwrap()
                                .iter()
                                .map(|request| request.state.clone())
                                .collect()
                        } else {
                            llm.requests
                                .lock()
                                .unwrap()
                                .iter()
                                .map(|request| {
                                    let Content::Text { text } = &request.messages[1].content[0]
                                    else {
                                        panic!()
                                    };
                                    serde_json::from_str(text).unwrap()
                                })
                                .collect()
                        };
                        agent.shutdown().await;
                        let label = format!("{tool} {mode:?} {rule:?} jev={use_jev} block={block}");
                        assert_eq!(prompts, usize::from(rule == Some(Decision::Ask)), "{label}");
                        assert_eq!(
                            requests.len(),
                            usize::from(reviewed) * if block { 2 } else { 1 },
                            "{label}"
                        );
                        if let Some(request) = requests.first() {
                            assert_eq!(request["operation"]["tool"], tool, "{label}");
                            if let Some(child) = child {
                                assert_eq!(request["operation"]["input"]["to"], child.to_string());
                            }
                        }
                        if child.is_some() {
                            assert_eq!(
                                snapshot.sent_agent_messages.len(),
                                usize::from(admitted),
                                "{label}"
                            );
                        } else {
                            assert_eq!(children.len(), usize::from(admitted), "{label}");
                        }
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn malformed_delegation_is_rejected_before_classifier_or_child_creation() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("invalid", "Subagent", json!({"task":"  "})),
            text("stopped"),
        ])),
    );
    let reviewer = Arc::new(ScriptedModel::new(vec![text(
        &auto_classification(false).to_string(),
    )]));
    agent.set_auto_reviewer(reviewer.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let prompts = finish(&agent, chat, "Delegate the bounded task").await;
    let children = agent.subagents(chat).await.unwrap();
    agent.shutdown().await;
    assert_eq!(prompts, 0);
    assert!(children.is_empty());
    assert!(reviewer.requests.lock().unwrap().is_empty());
}

struct HeldReviewer {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl tickloop_agent_model::ModelClient for HeldReviewer {
    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        ScriptedModel::new(Vec::new()).identity()
    }

    async fn complete(
        &self,
        _: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        self.entered.notify_one();
        tokio::select! {
            _ = self.release.notified() => {},
            _ = cancel.cancelled() => return Err(tickloop_agent_model::ModelError::Cancelled),
        }
        Ok(tickloop_agent_model::ModelOutput {
            private_source: None,
            content: text(&auto_classification(false).to_string()),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn classifier_allow_cannot_create_or_message_after_cancellation_or_policy_tightening() {
    for tool in ["Subagent", "SendMessage"] {
        for cancel in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut replies = vec![call(
                "delegate",
                "Subagent",
                json!({"task":"Return the bounded result", "name":"peer"}),
            )];
            if tool == "SendMessage" {
                replies.extend([
                    text("child ready"),
                    text("root ready"),
                    call(
                        "send",
                        "SendMessage",
                        json!({"to":"peer", "message":"MUST_NOT_ARRIVE"}),
                    ),
                ]);
            }
            replies.push(text("stopped"));
            let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(replies)));
            let chat = agent.create_chat(directory.path().into()).await.unwrap();
            if tool == "SendMessage" {
                finish(&agent, chat, "Create the bounded peer").await;
            }
            let before = agent.subagents(chat).await.unwrap().len();
            agent
                .set_execution_mode(chat, ExecutionMode::Auto)
                .await
                .unwrap();
            let reviewer = Arc::new(HeldReviewer {
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            });
            agent.set_auto_reviewer(reviewer.clone()).await;
            let mut events = agent.subscribe();
            let receipt = agent
                .submit(chat, None, "Perform the bounded family operation".into())
                .await
                .unwrap();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                reviewer.entered.notified(),
            )
            .await
            .unwrap();
            assert_eq!(agent.subagents(chat).await.unwrap().len(), before);
            assert!(
                agent
                    .snapshot(chat)
                    .await
                    .unwrap()
                    .sent_agent_messages
                    .is_empty()
            );
            if cancel {
                agent.interrupt(chat).await.unwrap();
            } else {
                agent
                    .set_permission_rules(
                        chat,
                        vec![PermissionRule {
                            id: uuid::Uuid::new_v4(),
                            decision: Decision::Deny,
                            tool: tool.into(),
                            specifier: None,
                            input: None,
                            loop_id: None,
                            persistent: false,
                        }],
                    )
                    .await
                    .unwrap();
            }
            reviewer.release.notify_one();
            loop {
                let event = next_event(&mut events).await;
                if event.run_id != receipt.run_id {
                    continue;
                }
                match event.kind {
                    EventKind::Completed { .. } | EventKind::Interrupted => break,
                    EventKind::PermissionRequested { .. } => {
                        panic!("a canceled or denied action must not ask")
                    }
                    EventKind::Failed(error) => {
                        assert!(!cancel, "{error}");
                        assert!(
                            error.contains("permission changed before admission"),
                            "{error}"
                        );
                        break;
                    }
                    _ => {}
                }
            }
            let children = agent.subagents(chat).await.unwrap();
            let snapshot = agent.snapshot(chat).await.unwrap();
            agent.shutdown().await;
            assert_eq!(children.len(), before, "{tool}, canceled={cancel}");
            assert!(
                snapshot.sent_agent_messages.is_empty(),
                "{tool}, canceled={cancel}"
            );
        }
    }
}
