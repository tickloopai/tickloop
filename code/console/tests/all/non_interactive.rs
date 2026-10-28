//! Agents on a non-interactive host, where a `Subagent` call that passes
//! `run_in_background: false` waits for the child's final report. A terminal host always runs
//! Subagents in the background, so tests that need the parent to receive the child's result in
//! the same turn use these.

use std::{path::PathBuf, sync::Arc};
use tickloop_code_settings::UserSettings;
use tickloop_harness::model::ModelClient;
use tickloop_harness::protocol::{AgentHostOptions, ModelConfig};
use tickloop_harness::runtime::{
    Agent, HarnessOptions, HarnessSeams, SubagentLaunch, ToolPolicy,
    environment::FolderEnvironment, transcript::JsonlTranscriptStore,
};
use tickloop_harness::test_support::{config, harness::ChosenModels};

/// `Agent::new_embedded` on a non-interactive host.
pub fn embedded_agent(client: Arc<dyn ModelClient>) -> Agent {
    Agent::new_embedded_with_subagent_launch(config(), client, SubagentLaunch::BackgroundByDefault)
}

/// `LocalAgent::open_embedded` on a non-interactive host.
pub fn opened_agent(root: PathBuf, model: ModelConfig, client: Arc<dyn ModelClient>) -> Agent {
    let store = JsonlTranscriptStore::open(root).unwrap();
    let root = store.root().to_owned();
    Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(Arc::new(store)),
            environment: Some(Arc::new(FolderEnvironment::bare())),
            models: ChosenModels::new(client),
        },
        HarnessOptions {
            settings: Some(Arc::new(UserSettings::in_root(&root))),
            local_state_root: Some(root),
            host: AgentHostOptions::embedded_defaults(),
            tools: ToolPolicy {
                subagent_launch: SubagentLaunch::BackgroundByDefault,
                ..ToolPolicy::local()
            },
            ..HarnessOptions::local(model)
        },
    )
    .unwrap()
}
