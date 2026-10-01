use std::{sync::Arc, time::Duration};
use tickloop_agent::{
    model::{Content, HttpModelClient, InputModality, ModelConfig, Protocol},
    runtime::{Agent, EventKind, LoopState, QuestionAnswer},
};

#[tokio::test]
#[ignore = "uses the real billed DeepSeek API to validate natural goal routing and clarification"]
async fn natural_language_creates_an_independent_goal_and_waits_for_its_answer() {
    let directory = tempfile::tempdir().unwrap();
    let config = ModelConfig {
        provider: tickloop_agent::model::ModelProvider::Api,
        auth_source: tickloop_agent::model::ModelAuthSource::ApiKeyEnv,
        name: "DeepSeek routing validation".into(),
        protocol: Protocol::Messages,
        base_url: "https://api.deepseek.com/anthropic".into(),
        model: "deepseek-flash".into(),
        api_key_env: "DEEPSEEK_API_KEY".into(),
        max_output_tokens: 4096,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text],
    };
    let agent = Agent::new_embedded(
        config.clone(),
        Arc::new(HttpModelClient::new(config).unwrap()),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let original = agent
        .submit(
            chat,
            None,
            "Remember that my selected color is red. Acknowledge briefly; no tools are needed."
                .into(),
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Completed {..} => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        agent.submit(chat, Some(original.loop_id), "I have a separate, independent task: help me choose the format for a short report. Keep this task separate from remembering my color. Before preparing anything, ask me to choose Markdown or plain text and wait for my answer. Do not create any files.".into()).await.unwrap();
        let mut new_goal = None;
        let mut saw_creation = false;
        loop {
            let event = events.recv().await.unwrap();
            match event.kind {
                EventKind::ToolStarted {name, ..} if name == "StartGoal" => saw_creation = true,
                EventKind::QuestionAsked {request} => {
                    assert!(saw_creation);
                    assert_ne!(request.loop_id, original.loop_id);
                    new_goal = Some(request.loop_id);
                    let snapshot = agent.snapshot(chat).await.unwrap();
                    assert_eq!(snapshot.loops.len(), 2);
                    assert_eq!(snapshot.loops.iter().find(|goal| goal.id == request.loop_id).unwrap().state, LoopState::WaitingQuestion);
                    let reminder = agent.submit(chat, Some(original.loop_id), "While the report format is waiting for my answer, repeat my selected color. Reply with only the color; use no tools.".into()).await.unwrap();
                    let mut color = String::new();
                    loop {
                        let event = events.recv().await.unwrap();
                        if event.run_id != reminder.run_id { continue; }
                        match event.kind {
                            EventKind::Text(text) => color.push_str(&text),
                            EventKind::Completed {..} => break,
                            EventKind::Failed(error) => panic!("live request while another goal waited failed: {error}"),
                            _ => {}
                        }
                    }
                    assert!(color.to_lowercase().contains("red"));
                    assert_eq!(agent.snapshot(chat).await.unwrap().loops.iter().find(|goal| goal.id == request.loop_id).unwrap().state, LoopState::WaitingQuestion);
                    let answers = request.questions.iter().map(|_| QuestionAnswer::other("Markdown, please. Just confirm my choice in your reply; do not write files.")).collect();
                    agent.answer_question(chat, request.id, answers).await.unwrap();
                }
                EventKind::Completed {usage} if Some(event.loop_id) == new_goal => {
                    eprintln!("Natural goal + clarification completed, usage={usage:?}");
                    break;
                }
                EventKind::Failed(error) => panic!("{error}"),
                EventKind::PermissionRequested {..} => panic!("this conversation should not need external effects"),
                _ => {}
            }
        }
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.loops.len(), 2);
        assert!(snapshot.questions.is_empty());
        assert!(snapshot.messages.last().unwrap().content.iter().any(|content| matches!(content, Content::Text {text} if text.to_lowercase().contains("markdown"))));
        assert!(snapshot.loops.iter().find(|goal| goal.id == original.loop_id).unwrap().goal.contains("red"));
    }).await;
    agent.shutdown().await;
    result.expect("natural goal routing live validation timed out");
}
