use serde_json::json;
use std::sync::Arc;
use tickloop_harness_protocol::{EventKind, ExecutionMode, PermissionScope};
use tickloop_harness_runtime::Agent;
use tickloop_harness_test_support::{
    ScriptedModel, auto_classification, call, config, next_event, text,
};

/// An Agent like [`Agent::new_embedded`] whose user settings set `useAutoModeDuringPlan`.
pub(crate) fn agent_with_plan_setting(
    model: Arc<ScriptedModel>,
    use_auto_mode_during_plan: bool,
) -> Agent {
    use tickloop_harness_policy::settings::{MemorySettingsStore, SettingsSnapshot};
    use tickloop_harness_runtime::{HarnessOptions, HarnessSeams, environment::FolderEnvironment};
    use tickloop_harness_test_support::harness::ChosenModels;
    let settings = MemorySettingsStore::new(SettingsSnapshot {
        use_auto_mode_during_plan,
        ..SettingsSnapshot::default()
    });
    Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(FolderEnvironment::bare())),
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            settings: Some(Arc::new(settings)),
            host: tickloop_harness_protocol::AgentHostOptions::embedded_defaults(),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
}

/// What Plan would ask about goes through Auto's admission: an outside `Read` is admitted and a
/// `WebFetch` is reviewed, whose rejection returns to the model; with the setting off both ask.
#[tokio::test]
async fn plan_admits_and_reviews_as_auto_unless_the_user_turned_it_off() {
    for use_auto in [true, false] {
        let external = tempfile::tempdir().unwrap();
        let notes = external.path().join("notes.txt");
        std::fs::write(&notes, "PLAN_NOTES").unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call("read", "Read", json!({"file_path": notes})),
            call(
                "fetch",
                "WebFetch",
                json!({"url":"https://docs.example.com/guide","prompt":"Summarize"}),
            ),
            text("done"),
        ]));
        let agent = agent_with_plan_setting(model.clone(), use_auto);
        let reviewer = Arc::new(ScriptedModel::new(vec![
            text(
                &auto_classification(true).to_string()
            );
            2
        ]));
        agent.set_auto_reviewer(reviewer.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(dir.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Plan)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Plan from the notes".into())
            .await
            .unwrap();
        let mut prompts = Vec::new();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { request } => {
                    let allow = request.name == "Read";
                    prompts.push(request.name.clone());
                    agent
                        .answer_permission_with_comment(
                            chat,
                            request.id,
                            allow,
                            PermissionScope::Once,
                            (!allow).then(|| "not now".into()),
                        )
                        .await
                        .unwrap();
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let reviews = reviewer.requests.lock().unwrap().len();
        if use_auto {
            assert!(prompts.is_empty(), "{prompts:?}");
            assert!(reviews > 0);
        } else {
            assert_eq!(prompts, ["Read", "WebFetch"]);
            assert_eq!(reviews, 0);
        }
        let last = serde_json::to_string(&model.requests.lock().unwrap().last().unwrap().messages)
            .unwrap();
        assert!(last.contains("PLAN_NOTES"), "{use_auto}");
        assert_eq!(
            last.contains("external-code restriction"),
            use_auto,
            "the review's rejection reaches the model"
        );
        agent.shutdown().await;
    }
}
