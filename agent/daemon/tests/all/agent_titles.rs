use std::sync::Arc;
use tickloop_agent_daemon::EmbeddedAgentAccess;
use tickloop_agent_runtime::Agent;

use tickloop_agent_protocol::{
    AgentAccess, AgentCommand, AgentFilter, CommandEnvelope, LaunchSpec, ObservationRequest,
    ObservationValue, Page, UserInput,
};

struct TitleModel {
    release: tokio::sync::Notify,
    requested: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl tickloop_agent_model::ModelClient for TitleModel {
    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_test_support::config().identity()
    }
    async fn complete(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        let is_title = request.tools.is_empty();
        if is_title {
            tickloop_agent_model::validate_reasoning_effort(
                &tickloop_agent_test_support::config(),
                request.reasoning_effort,
            )?;
            let tickloop_agent_protocol::Content::Text { text } =
                &request.messages.last().unwrap().content[0]
            else {
                panic!("title input is text");
            };
            assert_eq!(text.chars().count(), 1000);
            assert!(text.ends_with(
                "Investigate why artifact recovery no longer works after restarting the app"
            ));
            assert!(request.messages.iter().all(|message| {
                message
                    .content
                    .iter()
                    .all(|content| matches!(content, tickloop_agent_protocol::Content::Text { .. }))
            }));
            self.requested.notify_one();
            tokio::select! { _=self.release.notified()=>{}, _=cancel.cancelled()=>return Err(tickloop_agent_model::ModelError::Cancelled) }
        }
        Ok(tickloop_agent_model::ModelOutput {
            private_source: None,
            content: tickloop_agent_test_support::text(if is_title {
                "Investigate artifact recovery"
            } else {
                "Primary work finished"
            }),
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn generated_title_is_background_work_and_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("state");
    let model = Arc::new(TitleModel {
        release: tokio::sync::Notify::new(),
        requested: tokio::sync::Notify::new(),
    });
    let core = Agent::open_embedded(
        store.clone(),
        tickloop_agent_test_support::config(),
        model.clone(),
    )
    .unwrap();
    let chat = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    let receipt = core
        .submit(
            chat,
            format!(
                "{}Investigate why artifact recovery no longer works after restarting the app",
                "Earlier context. ".repeat(100)
            ),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == receipt.run_id
                && matches!(
                    event.kind,
                    tickloop_agent_protocol::EventKind::Completed { .. }
                )
            {
                break;
            }
        }
    })
    .await
    .expect("title generation never blocks the primary turn");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        model.requested.notified(),
    )
    .await
    .expect("background title request started");
    assert!(
        serde_json::to_value(core.snapshot(chat).await.unwrap()).unwrap()["generated_title"]
            .is_null()
    );
    model.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if serde_json::to_value(core.snapshot(chat).await.unwrap()).unwrap()["generated_title"]
                == "Investigate artifact recovery"
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let titled = core.snapshot(chat).await.unwrap();
    let title_requests = titled
        .request_usage
        .entries()
        .iter()
        .filter(|entry| entry.kind == tickloop_agent_protocol::ModelRequestKind::Title)
        .collect::<Vec<_>>();
    assert_eq!(title_requests.len(), 1);
    assert_eq!(
        title_requests[0].model,
        tickloop_agent_test_support::config().identity()
    );
    assert!(title_requests[0].loop_id.is_none() && title_requests[0].run_id.is_none());
    core.shutdown().await;
    let restored = Agent::open_embedded(
        store,
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
    )
    .unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert_eq!(
        serde_json::to_value(snapshot).unwrap()["generated_title"],
        "Investigate artifact recovery"
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn overview_falls_back_to_a_short_prompt_without_renaming_the_root_main() {
    let directory = tempfile::tempdir().unwrap();
    let core = Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text("Ready"),
        ])),
    );
    let access = EmbeddedAgentAccess::new(core.clone());
    access.execute(CommandEnvelope { daemon_epoch:access.daemon_epoch(), command_id:uuid::Uuid::new_v4(), command:AgentCommand::Create { launch:LaunchSpec::embedded(directory.path().into()), initial:Some(UserInput::Text { text:"Investigate the first problem\nand keep this very long original user instruction out of the title column while preserving all of its details in the conversation".into(), contexts:vec![], paths:vec![] }), origin:None } }).await.unwrap();
    let observation = access
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page {
                offset: 0,
                limit: 20,
            },
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Overview(rows) = observation.initial else {
        panic!("overview expected")
    };
    assert!(
        rows[0].title.starts_with("Investigate the first problem"),
        "{}",
        rows[0].title
    );
    assert!(!rows[0].title.contains('\n'));
    assert!(unicode_width::UnicodeWidthStr::width(rows[0].title.as_str()) <= 64);
    assert!(!rows[0].title.contains("main"));
    core.shutdown().await;
}
