use std::{sync::OnceLock, time::Duration};

use tickloop_harness_protocol::{ModelConfig, ModelRequestKind};

use crate::{GenerationBudget, cards::ModelCatalog};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuxiliaryTask {
    Title,
    ShellDescription,
    AutoSetup,
    PermissionCheck,
    WebFetch,
    PromptSuggestion,
}

impl AuxiliaryTask {
    pub fn for_request(kind: ModelRequestKind) -> Option<Self> {
        match kind {
            ModelRequestKind::Title => Some(Self::Title),
            ModelRequestKind::ShellDescription => Some(Self::ShellDescription),
            ModelRequestKind::AutoSetup => Some(Self::AutoSetup),
            ModelRequestKind::PermissionCheck => Some(Self::PermissionCheck),
            ModelRequestKind::WebFetch => Some(Self::WebFetch),
            ModelRequestKind::Conversation
            | ModelRequestKind::CompactionAutomatic
            | ModelRequestKind::CompactionManual
            | ModelRequestKind::CompactionDirected
            | ModelRequestKind::GoalCheck => None,
        }
    }

    pub fn budget(self) -> GenerationBudget {
        let (tokens, seconds) = match self {
            Self::Title => (512, 15),
            Self::ShellDescription => (256, 15),
            Self::AutoSetup => (4096, 30),
            Self::PermissionCheck => (4096, 45),
            Self::WebFetch => (8192, 60),
            Self::PromptSuggestion => (1024, 10),
        };
        GenerationBudget::new(tokens, Duration::from_secs(seconds))
    }
}

pub fn auxiliary_model_config(current: &ModelConfig, task: AuxiliaryTask) -> Option<ModelConfig> {
    static CATALOG: OnceLock<ModelCatalog> = OnceLock::new();
    let catalog = CATALOG.get_or_init(ModelCatalog::defaults);
    let provider = catalog
        .providers
        .iter()
        .find(|provider| provider.id == current.provider_id)?;
    let id = provider.selection.as_ref()?.auxiliary_model.as_deref()?;
    let mut model = catalog.get(&current.provider_id, id)?;
    model.api_key_env.clone_from(&current.api_key_env);
    if model.adapter() == current.adapter()
        || reqwest::Url::parse(&current.base_url)
            .is_ok_and(|url| tickloop_harness_base::network::is_loopback(&url))
    {
        model.base_url.clone_from(&current.base_url);
    }
    Some(task.budget().apply_model_limit(model))
}
