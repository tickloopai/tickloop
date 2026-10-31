//! What happened to a Chat's hosted Ticks outside the model's own calls reaches it in a note at
//! the end of its next request, once, without waking the Agent.

use std::sync::Arc;

use serde_json::json;
use tick_protocol::InstanceRef;
use tickloop_harness_protocol::{
    ChatId, Content, EventKind, HostEndpointIdentity, HostToolAuthority,
};
use tickloop_harness_runtime::{
    Agent, HarnessOptions, HarnessSeams,
    environment::{ExecutionTools, memory::MemoryEnvironment},
};
use tickloop_harness_test_support::{
    ScriptedModel, call, config, harness::ChosenModels, next_event, text,
};

const NOTE: &str = "changed outside your tool calls";

struct Bench {
    agent: Agent,
    chat: ChatId,
    environment: Arc<MemoryEnvironment>,
    model: Arc<ScriptedModel>,
    events: tokio::sync::broadcast::Receiver<tickloop_harness_protocol::Event>,
    _folder: tempfile::TempDir,
}

impl Bench {
    /// An Agent whose model opens the terminal program `/top` in its first turn, then replies
    /// with `replies`.
    async fn start(replies: Vec<Vec<Content>>) -> Self {
        let folder = tempfile::tempdir().unwrap();
        let environment = Arc::new(MemoryEnvironment::new());
        environment.install_terminal_program("/top", "top", |_, _| {});
        let mut script = vec![
            call(
                "open",
                "TickManage",
                json!({"action":"open","command":["/top"],"cwd":"/"}),
            ),
            text("opened"),
        ];
        script.extend(replies);
        let model = Arc::new(ScriptedModel::new(script));
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(environment.clone()),
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions::local(config()),
        )
        .unwrap();
        agent
            .install_host_tools(Arc::new(ExecutionTools::new(
                environment.clone(),
                HostToolAuthority::Native {
                    endpoint: HostEndpointIdentity::TrustedAdapter {
                        scope: tickloop_ids::generate(),
                    },
                },
            )))
            .unwrap();
        let chat = agent.create_chat(folder.path().into()).await.unwrap();
        let events = agent.subscribe();
        let mut bench = Self {
            agent,
            chat,
            environment,
            model,
            events,
            _folder: folder,
        };
        bench.turn("Open top.").await;
        bench
    }

    async fn turn(&mut self, input: &str) {
        let receipt = self.agent.submit(self.chat, input.into()).await.unwrap();
        loop {
            let event = next_event(&mut self.events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::PermissionRequested { request } => self
                    .agent
                    .answer_permission(self.chat, request.id, true)
                    .await
                    .unwrap(),
                EventKind::Completed { .. } => return,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    }

    async fn opened(&self) -> InstanceRef {
        let snapshot = self.agent.snapshot(self.chat).await.unwrap();
        snapshot.monitor.members[0].instance.clone()
    }

    /// The host notes about Ticks in the latest request.
    fn last_notes(&self) -> Vec<String> {
        let requests = self.model.requests.lock().unwrap();
        let request = requests.last().unwrap();
        request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                Content::Text { text } if text.contains(NOTE) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

#[tokio::test]
async fn an_exited_program_is_told_once_at_the_next_request() {
    let mut bench = Bench::start(vec![text("first"), text("second")]).await;
    assert!(bench.last_notes().is_empty(), "opening it says nothing");
    let instance = bench.opened().await;
    bench.environment.exit_terminal(&instance, 3);
    bench.turn("What now?").await;
    let notes = bench.last_notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains(&instance.model_text(&tickloop_ids::IdContext::default()))
            && notes[0].contains("its program exited with code 3"),
        "{}",
        notes[0]
    );
    bench.turn("And now?").await;
    assert_eq!(bench.last_notes().len(), 1, "told once");
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn the_models_own_close_is_not_told_again() {
    let mut bench = Bench::start(vec![]).await;
    let ids = tickloop_ids::IdContext::default();
    let instance = bench.opened().await.model_text(&ids);
    bench.model.enqueue(call(
        "close",
        "TickManage",
        json!({"action":"close","tick_instance_id":instance}),
    ));
    bench.model.enqueue(text("closed"));
    bench.model.enqueue(text("fine"));
    bench.turn("Close it.").await;
    bench.turn("Anything else?").await;
    assert!(bench.last_notes().is_empty(), "{:?}", bench.last_notes());
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn the_users_close_and_a_lost_host_are_told() {
    let mut bench = Bench::start(vec![text("first"), text("second")]).await;
    let instance = bench.opened().await;
    bench
        .agent
        .record_user_closed_tick(bench.chat, &instance)
        .await
        .unwrap();
    bench.turn("What now?").await;
    let notes = bench.last_notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("was closed by the user"), "{}", notes[0]);
    assert!(
        !notes[0].contains("no longer runs") && !notes[0].contains("lost"),
        "the user's close is told once, as a close: {}",
        notes[0]
    );
    bench.agent.shutdown().await;

    let mut bench = Bench::start(vec![text("first")]).await;
    bench.environment.restart();
    bench.turn("What now?").await;
    let notes = bench.last_notes();
    assert!(
        notes.len() == 1 && notes[0].contains("was lost with its Tickhost"),
        "{notes:?}"
    );
    bench.agent.shutdown().await;
}
