//! Keeping a Chat's prompt cache prefix stable across requests, and noticing when it breaks.

use std::{
    collections::HashSet,
    hash::{DefaultHasher, Hash, Hasher},
};

use serde::Serialize;
use tickloop_harness_protocol::{Message, ModelIdentity, RunId, ToolDefinition};

#[derive(Clone, Default)]
pub(crate) struct CachePrefixState {
    pub(crate) held_host_tools: Option<HeldHostTools>,
    last_request: Option<ObservedRequest>,
    compacted_since_last_request: bool,
}

/// Where a tool goes in the listed tools, which keep this order so the vendor's prompt cache
/// prefix depends on neither registration order nor which host source or server came first: the
/// built-in tools, the runtime tools, the MCP resource tools, the Fold tools, then every host tool,
/// each group sorted by name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ListingGroup {
    BuiltIn,
    Runtime,
    McpResources,
    Fold,
    HostTools,
}

/// The MCP part of a Run's tools as the Run's first model request offered them. Later requests
/// of the Run offer the same MCP tools byte for byte, so servers connecting, disconnecting or
/// listing again change the listed tools only at a Run boundary.
#[derive(Clone)]
pub(crate) struct HeldHostTools {
    pub(crate) run_id: RunId,
    /// Native search defers tools that Fold folds, so a model change re-holds the tools.
    pub(crate) native_search: bool,
    /// The listed MCP resource, Fold and MCP tools, each with the group it was listed in.
    listed: Vec<(ListingGroup, ToolDefinition)>,
    /// Every deferred tool the Run has offered so far, sorted by name.
    deferred: Vec<ToolDefinition>,
}

/// What a later request of a Run offers of its MCP tools.
pub(crate) struct HeldOffer {
    /// Listed tools the Run's first request did not list: unlisted until the next Run.
    pub(crate) withheld: Vec<String>,
    /// The tools the Run's first request listed, with the definitions it listed them with.
    pub(crate) listed: Vec<(ListingGroup, ToolDefinition)>,
    /// Every deferred tool the Run has offered so far, sorted by name: deferred tools are only
    /// added to, after the native search tool, so searches still find a server that connected
    /// during the Run.
    pub(crate) deferred: Vec<ToolDefinition>,
}

impl HeldHostTools {
    /// Holds the MCP tools a request of `run_id` would offer, `listed` and `deferred`, to what the
    /// Run's first request offered. `None` when this request is the first of the Run in `held`, or
    /// the first since a model change switched native search, and `held` now records its tools.
    pub(crate) fn hold(
        held: &mut Option<Self>,
        run_id: RunId,
        native_search: bool,
        listed: Vec<(ListingGroup, ToolDefinition)>,
        deferred: Vec<ToolDefinition>,
    ) -> Option<HeldOffer> {
        let Some(current) = held
            .as_mut()
            .filter(|held| held.run_id == run_id && held.native_search == native_search)
        else {
            *held = Some(Self {
                run_id,
                native_search,
                listed,
                deferred,
            });
            return None;
        };
        let held_names: HashSet<String> = current
            .listed
            .iter()
            .map(|(_, tool)| tool.name.clone())
            .collect();
        let withheld = listed
            .into_iter()
            .map(|(_, tool)| tool.name)
            .filter(|name| !held_names.contains(name))
            .collect();
        let mut deferred_names: HashSet<String> = current
            .deferred
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        for definition in deferred {
            if deferred_names.insert(definition.name.clone()) {
                current.deferred.push(definition);
            }
        }
        current
            .deferred
            .retain(|tool| !held_names.contains(&tool.name));
        current
            .deferred
            .sort_by(|first, second| first.name.cmp(&second.name));
        Some(HeldOffer {
            withheld,
            listed: current.listed.clone(),
            deferred: current.deferred.clone(),
        })
    }
}

/// What a conversation request's prompt cache prefix depends on: the model, the system part
/// (system prompt and user context message) and the listed tools. Deferred tools follow the
/// native search tool, after the cached prefix, so they are not part of it.
#[derive(Clone)]
pub(crate) struct CachePrefix {
    model: u64,
    system: u64,
    tools: Vec<(String, u64)>,
}

impl CachePrefix {
    pub(crate) fn of(model: &ModelIdentity, system: &[Message], tools: &[ToolDefinition]) -> Self {
        Self {
            model: fingerprint(model),
            system: fingerprint(system),
            tools: tools
                .iter()
                .map(|tool| (tool.name.clone(), fingerprint(tool)))
                .collect(),
        }
    }

    fn fingerprint(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        (self.model, self.system, &self.tools).hash(&mut hasher);
        hasher.finish()
    }
}

fn fingerprint(value: &(impl Serialize + ?Sized)) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_vec(value)
        .expect("request parts serialize")
        .hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone)]
struct ObservedRequest {
    prefix: CachePrefix,
    cache_read: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CacheBreakReason {
    /// The first request after a compaction, which rewrites the conversation: expected.
    Compaction,
    PrefixChanged,
    /// The prefix is unchanged but the cache read fell below half of the last request's, for
    /// no reason the host can see (for example the vendor's cache expired).
    CacheReadDropped,
}

impl CacheBreakReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Compaction => "compaction",
            Self::PrefixChanged => "prefix_changed",
            Self::CacheReadDropped => "cache_read_dropped",
        }
    }
}

pub(crate) struct CacheBreak {
    reason: CacheBreakReason,
    changed_parts: Vec<&'static str>,
    added_tools: Vec<String>,
    removed_tools: Vec<String>,
    changed_tools: Vec<String>,
    previous_cache_read: Option<u64>,
    cache_read: Option<u64>,
    fingerprint: u64,
}

impl CachePrefixState {
    pub(crate) fn compacted(&mut self) {
        self.compacted_since_last_request = true;
    }

    /// Records a request of this Chat that the vendor answered, and returns how it broke the
    /// prompt cache of the Chat's previous request, if it did.
    pub(crate) fn observe(
        &mut self,
        prefix: CachePrefix,
        cache_read: Option<u64>,
    ) -> Option<CacheBreak> {
        let is_after_compaction = std::mem::take(&mut self.compacted_since_last_request);
        let previous = self.last_request.replace(ObservedRequest {
            prefix: prefix.clone(),
            cache_read,
        })?;
        let mut changed_parts = Vec::new();
        if previous.prefix.model != prefix.model {
            changed_parts.push("model");
        }
        if previous.prefix.system != prefix.system {
            changed_parts.push("system");
        }
        if previous.prefix.tools != prefix.tools {
            changed_parts.push("tools");
        }
        let fingerprint_of = |tools: &[(String, u64)], name: &str| {
            tools
                .iter()
                .find(|(tool, _)| tool == name)
                .map(|(_, fingerprint)| *fingerprint)
        };
        let added_tools = prefix
            .tools
            .iter()
            .filter(|(name, _)| fingerprint_of(&previous.prefix.tools, name).is_none())
            .map(|(name, _)| name.clone())
            .collect();
        let removed_tools = previous
            .prefix
            .tools
            .iter()
            .filter(|(name, _)| fingerprint_of(&prefix.tools, name).is_none())
            .map(|(name, _)| name.clone())
            .collect();
        let changed_tools = prefix
            .tools
            .iter()
            .filter(|(name, fingerprint)| {
                fingerprint_of(&previous.prefix.tools, name)
                    .is_some_and(|previous| previous != *fingerprint)
            })
            .map(|(name, _)| name.clone())
            .collect();
        let has_collapsed = matches!(
            (previous.cache_read, cache_read),
            (Some(previous), Some(current)) if current.saturating_mul(2) < previous
        );
        let reason = if is_after_compaction {
            CacheBreakReason::Compaction
        } else if !changed_parts.is_empty() {
            CacheBreakReason::PrefixChanged
        } else if has_collapsed {
            CacheBreakReason::CacheReadDropped
        } else {
            return None;
        };
        Some(CacheBreak {
            reason,
            changed_parts,
            added_tools,
            removed_tools,
            changed_tools,
            previous_cache_read: previous.cache_read,
            cache_read,
            fingerprint: prefix.fingerprint(),
        })
    }
}

impl CacheBreak {
    /// Writes the break to the Agent log only: it never reaches the model or the Transcript.
    pub(crate) fn log(&self, chat: tickloop_harness_protocol::ChatId, run: RunId) {
        let tokens =
            |tokens: Option<u64>| tokens.map_or("unknown".to_owned(), |count| count.to_string());
        tracing::debug!(
            target: "tickloop::cache_break",
            chat = ?chat,
            run = ?run,
            reason = %self.reason.as_str(),
            expected = self.reason == CacheBreakReason::Compaction,
            changed = %joined_or_none(&self.changed_parts),
            added_tools = %joined_or_none(&self.added_tools),
            removed_tools = %joined_or_none(&self.removed_tools),
            changed_tools = %joined_or_none(&self.changed_tools),
            previous_cache_read = %tokens(self.previous_cache_read),
            cache_read = %tokens(self.cache_read),
            fingerprint = %format!("{:016x}", self.fingerprint),
            "cache break"
        );
    }
}

fn joined_or_none(items: &[impl AsRef<str>]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tickloop_harness_protocol::{Content, ModelProvider, Protocol, Role};

    fn model() -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    fn system(text: &str) -> Vec<Message> {
        vec![Message {
            role: Role::System,
            content: vec![Content::Text { text: text.into() }],
        }]
    }

    fn tool(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            input_schema: json!({"type":"object"}),
        }
    }

    #[test]
    fn a_changed_definition_is_named_and_the_first_request_after_compaction_is_expected() {
        let mut state = CachePrefixState::default();
        let tools = [tool("Read", "Read a file"), tool("mcp__a__x", "Old")];
        assert!(
            state
                .observe(CachePrefix::of(&model(), &system("s"), &tools), Some(900))
                .is_none()
        );
        let changed = [tool("Read", "Read a file"), tool("mcp__a__x", "New")];
        let cache_break = state
            .observe(CachePrefix::of(&model(), &system("s"), &changed), Some(10))
            .unwrap();
        assert_eq!(cache_break.reason, CacheBreakReason::PrefixChanged);
        assert_eq!(cache_break.changed_parts, ["tools"]);
        assert_eq!(cache_break.changed_tools, ["mcp__a__x"]);
        assert!(cache_break.added_tools.is_empty() && cache_break.removed_tools.is_empty());
        assert!(
            state
                .observe(CachePrefix::of(&model(), &system("s"), &changed), Some(900))
                .is_none()
        );
        state.compacted();
        let after_compaction = state
            .observe(CachePrefix::of(&model(), &system("s"), &changed), Some(0))
            .unwrap();
        assert_eq!(after_compaction.reason, CacheBreakReason::Compaction);
        assert!(after_compaction.changed_parts.is_empty());
        assert!(
            state
                .observe(CachePrefix::of(&model(), &system("t"), &changed), Some(0))
                .is_some_and(|cache_break| cache_break.changed_parts == ["system"])
        );
    }
}
