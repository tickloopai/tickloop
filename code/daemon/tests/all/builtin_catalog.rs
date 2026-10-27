use tickloop_code_access::AgentAccess;
use tickloop_harness::model::cards::ModelCatalog;

#[tokio::test]
async fn builtin_worker_refuses_context_tuning_even_without_a_settings_root() {
    use std::sync::Arc;
    use tickloop_code_daemon::EmbeddedAgentAccess;

    use tickloop_code_access::{
        AgentCommand, CommandEnvelope, CommandResult, ConfigurationChange, LaunchSpec,
    };

    use tickloop_harness::runtime::Agent;
    use uuid::Uuid;
    let directory = tempfile::tempdir().unwrap();
    let model = ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-4-6")
        .unwrap();
    assert!(model.card.as_ref().unwrap().context_window_tokens.is_none());
    let runtime = Agent::new_embedded(
        model,
        Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
    );
    let epoch = Uuid::new_v4();
    let access = EmbeddedAgentAccess::for_worker(runtime.clone(), epoch);
    let result = access
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch_rules: Vec::new(),
                launch: LaunchSpec::embedded(directory.path().into()),
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created { execution, .. } = result else {
        panic!("built-in Agent created")
    };
    let result = access
        .execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Configure {
                execution,
                change: ConfigurationChange::ContextWindow { tokens: 1_000_000 },
                provider: None,
            },
        })
        .await
        .unwrap()
        .into_result();
    runtime.shutdown().await;
    assert!(
        result.is_err(),
        "Even a card without a numeric window uses version-owned parameters"
    );
    assert!(result.unwrap_err().message.contains("version-owned"));
}
