//! Keeping a Chat's prompt cache prefix stable across requests, and noticing when it breaks.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
};

use serde::Serialize;
use tickloop_harness_protocol::{Message, ModelIdentity, RunId, ToolDefinition};

#[derive(Clone, Default)]
pub(crate) struct CachePrefixState {
    held_host_tools: Option<HeldHostTools>,
    /// The deferred tools in the order this Chat first offered them. A tool search loads deferred
    /// tools by name, and the vendor keeps their definitions in the cached prefix, so a server
    /// connecting later appends its tools instead of inserting them among the earlier ones.
    deferred_order: Vec<String>,
    last_request: Option<ObservedRequest>,
    compacted_since_last_request: bool,
    exposure_changed_since_last_request: bool,
}

/// The user's changes to how MCP servers' tools reach the model, which a Run takes up at its next
/// request: how many changes each server has seen, and the server of each MCP tool.
#[derive(Clone, Default)]
pub(crate) struct UserExposure {
    pub(crate) revisions: BTreeMap<String, u64>,
    pub(crate) tool_servers: HashMap<String, String>,
}

impl UserExposure {
    /// The servers whose exposure the user changed since `earlier`.
    fn changed_since(&self, earlier: &Self) -> BTreeSet<String> {
        self.revisions
            .keys()
            .chain(earlier.revisions.keys())
            .filter(|server| self.revisions.get(*server) != earlier.revisions.get(*server))
            .cloned()
            .collect()
    }

    fn belongs_to(&self, tool: &str, servers: &BTreeSet<String>) -> bool {
        self.tool_servers
            .get(tool)
            .is_some_and(|server| servers.contains(server))
    }
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
/// listing again change the listed tools only at a Run boundary. The user changing a server's
/// exposure is the exception: its tools change from the next request.
#[derive(Clone)]
struct HeldHostTools {
    run_id: RunId,
    /// Native search defers tools that Fold folds, so a model change re-holds the tools.
    native_search: bool,
    /// The user's exposure changes the held tools follow.
    exposure: UserExposure,
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
    /// Every deferred tool the Run has offered so far, in the order the Chat first offered them:
    /// deferred tools are only added to, after the native search tool, so searches still find a
    /// server that connected during the Run.
    pub(crate) deferred: Vec<ToolDefinition>,
}

impl CachePrefixState {
    /// Puts `deferred` in the order this Chat first offered each tool, a tool it never offered
    /// after them all.
    pub(crate) fn order_deferred(&mut self, deferred: &mut [ToolDefinition]) {
        for tool in deferred.iter() {
            if !self.deferred_order.contains(&tool.name) {
                self.deferred_order.push(tool.name.clone());
            }
        }
        deferred.sort_by_key(|tool| {
            self.deferred_order
                .iter()
                .position(|name| *name == tool.name)
        });
    }

    /// Holds the MCP tools a request of `run_id` would offer, `listed` and `deferred`, to what the
    /// Run's first request offered, except the tools of the servers whose exposure the user
    /// changed since the last request, which take the offer of this request. `None` when this
    /// request is the first of the Run, or the first since a model change switched native search,
    /// and the state now holds its tools.
    pub(crate) fn hold(
        &mut self,
        run_id: RunId,
        native_search: bool,
        listed: Vec<(ListingGroup, ToolDefinition)>,
        deferred: Vec<ToolDefinition>,
        exposure: UserExposure,
    ) -> Option<HeldOffer> {
        let changed_servers = self
            .held_host_tools
            .as_ref()
            .map(|held| exposure.changed_since(&held.exposure))
            .unwrap_or_default();
        if !changed_servers.is_empty() {
            self.exposure_changed_since_last_request = true;
        }
        let Some(current) = self
            .held_host_tools
            .as_mut()
            .filter(|held| held.run_id == run_id && held.native_search == native_search)
        else {
            self.held_host_tools = Some(HeldHostTools {
                run_id,
                native_search,
                exposure,
                listed,
                deferred,
            });
            return None;
        };
        if !changed_servers.is_empty() {
            let changed = |tool: &ToolDefinition| exposure.belongs_to(&tool.name, &changed_servers);
            current.listed.retain(|(_, tool)| !changed(tool));
            current
                .listed
                .extend(listed.iter().filter(|(_, tool)| changed(tool)).cloned());
            current.deferred.retain(|tool| !changed(tool));
            current.exposure = exposure;
        }
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
        Some(HeldOffer {
            withheld,
            listed: current.listed.clone(),
            deferred: current.deferred.clone(),
        })
    }
}

/// What a conversation request's prompt cache prefix depends on, part by part in prefix order:
/// the model and its request parameters, each `#` section of the system prompt and each key of
/// its host state, the user context message, and each listed tool. Like Claude Code's cache
/// break detection, a change names the parts that changed and the first of them. Deferred tools
/// follow the native search tool, after the cached prefix, so they are not part of it.
#[derive(Clone)]
pub(crate) struct CachePrefix {
    model: u64,
    params: u64,
    system: Vec<(String, u64)>,
    tools: Vec<(String, u64)>,
}

/// The text before the host state at the end of the system prompt.
const HOST_STATE_PREFIX: &str = "Current host state: ";

impl CachePrefix {
    pub(crate) fn of(model: &ModelIdentity, system: &[Message], tools: &[ToolDefinition]) -> Self {
        Self {
            model: fingerprint(model),
            params: fingerprint(&()),
            system: system
                .iter()
                .enumerate()
                .flat_map(|(index, message)| match index {
                    0 => system_prompt_parts(&message_text(message)),
                    1 => vec![("user_context".to_owned(), fingerprint(message))],
                    _ => vec![(format!("message {index}"), fingerprint(message))],
                })
                .collect(),
            tools: tools
                .iter()
                .map(|tool| (tool.name.clone(), fingerprint(tool)))
                .collect(),
        }
    }

    /// The request parameters that select how the model answers, such as its reasoning effort.
    pub(crate) fn with_params(mut self, params: &impl Serialize) -> Self {
        self.params = fingerprint(params);
        self
    }

    fn fingerprint(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        (self.model, self.params, &self.system, &self.tools).hash(&mut hasher);
        hasher.finish()
    }

    /// Every part in prefix order, named.
    fn parts(&self) -> Vec<(String, u64)> {
        [
            ("model".to_owned(), self.model),
            ("params".to_owned(), self.params),
        ]
        .into_iter()
        .chain(
            self.system
                .iter()
                .map(|(label, fingerprint)| (format!("system:{label}"), *fingerprint)),
        )
        .chain(
            self.tools
                .iter()
                .map(|(name, fingerprint)| (format!("tool:{name}"), *fingerprint)),
        )
        .collect()
    }
}

fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            tickloop_harness_protocol::Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The system prompt's parts: the text before its first heading, each `#` section, and each key
/// of the host state that ends it.
fn system_prompt_parts(text: &str) -> Vec<(String, u64)> {
    let mut parts = Vec::new();
    let mut label = "intro".to_owned();
    let mut body = String::new();
    for chunk in text.split("\n\n") {
        if let Some(state) = chunk.strip_prefix(HOST_STATE_PREFIX) {
            parts.push((std::mem::take(&mut label), fingerprint(&body)));
            body.clear();
            match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(state) {
                Ok(state) => parts.extend(
                    state
                        .iter()
                        .map(|(key, value)| (format!("host_state.{key}"), fingerprint(value))),
                ),
                Err(_) => parts.push(("host_state".to_owned(), fingerprint(state))),
            }
            continue;
        }
        if chunk.starts_with("# ") {
            if !body.is_empty() {
                parts.push((std::mem::take(&mut label), fingerprint(&body)));
            }
            label = chunk.lines().next().unwrap_or_default().to_owned();
            body = chunk.to_owned();
        } else {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(chunk);
        }
    }
    if !body.is_empty() {
        parts.push((label, fingerprint(&body)));
    }
    parts
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
    at: std::time::Instant,
}

/// Claude Code's prompt cache lifetimes: a cache read that drops after a longer gap than one of
/// them most likely found that cache expired.
const SHORT_CACHE_LIFETIME: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const LONG_CACHE_LIFETIME: std::time::Duration = std::time::Duration::from_secs(60 * 60);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CacheBreakReason {
    /// The first request after a compaction, which rewrites the conversation: expected.
    Compaction,
    /// The user changed how an MCP server's tools reach the model: expected.
    UserChangedExposure,
    PrefixChanged,
    /// The prefix is unchanged but the cache read fell below half of the last request's, for
    /// no reason the host can see (for example the vendor's cache expired).
    CacheReadDropped,
}

impl CacheBreakReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Compaction => "compaction",
            Self::UserChangedExposure => "user_changed_exposure",
            Self::PrefixChanged => "prefix_changed",
            Self::CacheReadDropped => "cache_read_dropped",
        }
    }
}

pub(crate) struct CacheBreak {
    reason: CacheBreakReason,
    changed_parts: Vec<String>,
    /// The part, in prefix order, from which the vendor's cached prefix no longer matched.
    first_changed: Option<String>,
    /// Seconds since the Chat's previous request.
    gap_seconds: u64,
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

    #[cfg(test)]
    fn exposure_changed(&mut self) {
        self.exposure_changed_since_last_request = true;
    }

    /// Records a request of this Chat that the vendor answered, and returns how it broke the
    /// prompt cache of the Chat's previous request, if it did.
    pub(crate) fn observe(
        &mut self,
        prefix: CachePrefix,
        cache_read: Option<u64>,
    ) -> Option<CacheBreak> {
        let is_after_compaction = std::mem::take(&mut self.compacted_since_last_request);
        let is_after_exposure_change =
            std::mem::take(&mut self.exposure_changed_since_last_request);
        let at = std::time::Instant::now();
        let previous = self.last_request.replace(ObservedRequest {
            prefix: prefix.clone(),
            cache_read,
            at,
        })?;
        let (before, after) = (previous.prefix.parts(), prefix.parts());
        let first_changed = after
            .iter()
            .zip(&before)
            .position(|(current, earlier)| current != earlier)
            .or((after.len() != before.len()).then(|| after.len().min(before.len())))
            .map(|index| {
                after
                    .get(index)
                    .or(before.get(index))
                    .map(|(label, _)| label.clone())
                    .unwrap_or_default()
            });
        let changed_parts = after
            .iter()
            .filter(|part| !before.contains(part))
            .map(|(label, _)| label.clone())
            .chain(
                before
                    .iter()
                    .filter(|(label, _)| !after.iter().any(|(current, _)| current == label))
                    .map(|(label, _)| label.clone()),
            )
            .filter(|label| !label.starts_with("tool:"))
            .chain((previous.prefix.tools != prefix.tools).then(|| "tools".to_owned()))
            .collect::<Vec<_>>();
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
        } else if !changed_parts.is_empty() && is_after_exposure_change {
            CacheBreakReason::UserChangedExposure
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
            first_changed,
            gap_seconds: at.duration_since(previous.at).as_secs(),
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
    /// Why the cache most likely broke, as Claude Code tells a changed prefix, an expired cache
    /// and the vendor's side apart.
    fn likely_cause(&self) -> &'static str {
        match self.reason {
            CacheBreakReason::Compaction => "compaction",
            CacheBreakReason::UserChangedExposure => "user_changed_exposure",
            CacheBreakReason::PrefixChanged => "prefix_changed",
            CacheBreakReason::CacheReadDropped
                if self.gap_seconds >= LONG_CACHE_LIFETIME.as_secs() =>
            {
                "cache_expired_after_1h"
            }
            CacheBreakReason::CacheReadDropped
                if self.gap_seconds >= SHORT_CACHE_LIFETIME.as_secs() =>
            {
                "cache_expired_after_5m"
            }
            CacheBreakReason::CacheReadDropped => "vendor_side",
        }
    }

    /// Writes the break to the Agent log only: it never reaches the model or the Transcript.
    pub(crate) fn log(&self, chat: tickloop_harness_protocol::ChatId, run: RunId) {
        let tokens =
            |tokens: Option<u64>| tokens.map_or("unknown".to_owned(), |count| count.to_string());
        tracing::debug!(
            target: "tickloop::cache_break",
            chat = ?chat,
            run = ?run,
            reason = %self.reason.as_str(),
            expected = matches!(
                self.reason,
                CacheBreakReason::Compaction | CacheBreakReason::UserChangedExposure
            ),
            changed = %joined_or_none(&self.changed_parts),
            first_changed = %self.first_changed.as_deref().unwrap_or("none"),
            gap_seconds = self.gap_seconds,
            likely_cause = %self.likely_cause(),
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
                .is_some_and(|cache_break| cache_break.changed_parts == ["system:intro"])
        );
    }

    /// Like Claude Code's cache break detection, a change names each changed section of the
    /// system prompt and key of its host state, and the first changed part in prefix order.
    #[test]
    fn a_break_names_the_changed_sections_host_state_keys_and_the_first_change() {
        let prompt = |environment: &str, state: &str| {
            system(&format!(
                "Intro.\n\n# System\n - rules\n\n# Environment\n{environment}\n\nCurrent host state: {state}"
            ))
        };
        let tools = [tool("Read", "Read a file")];
        let mut state = CachePrefixState::default();
        state.observe(
            CachePrefix::of(
                &model(),
                &prompt("cwd /a", r#"{"workdir":"/a","x":1}"#),
                &tools,
            ),
            Some(900),
        );
        let cache_break = state
            .observe(
                CachePrefix::of(
                    &model(),
                    &prompt("cwd /b", r#"{"workdir":"/a","x":2}"#),
                    &tools,
                ),
                Some(10),
            )
            .unwrap();
        assert_eq!(
            cache_break.changed_parts,
            ["system:# Environment", "system:host_state.x"]
        );
        assert_eq!(
            cache_break.first_changed.as_deref(),
            Some("system:# Environment")
        );
        let params = state
            .observe(
                CachePrefix::of(
                    &model(),
                    &prompt("cwd /b", r#"{"workdir":"/a","x":2}"#),
                    &tools,
                )
                .with_params(&"high"),
                Some(10),
            )
            .unwrap();
        assert_eq!(params.first_changed.as_deref(), Some("params"));
        let dropped = state
            .observe(
                CachePrefix::of(
                    &model(),
                    &prompt("cwd /b", r#"{"workdir":"/a","x":2}"#),
                    &tools,
                )
                .with_params(&"high"),
                Some(10),
            )
            .map(|cache_break| cache_break.reason);
        assert_eq!(
            dropped, None,
            "an unchanged prefix with a steady cache read"
        );
    }

    #[test]
    fn a_dropped_cache_read_tells_an_expired_cache_from_the_vendors_side() {
        let cache_break = |gap_seconds| CacheBreak {
            reason: CacheBreakReason::CacheReadDropped,
            changed_parts: Vec::new(),
            first_changed: None,
            gap_seconds,
            added_tools: Vec::new(),
            removed_tools: Vec::new(),
            changed_tools: Vec::new(),
            previous_cache_read: Some(900),
            cache_read: Some(0),
            fingerprint: 0,
        };
        assert_eq!(cache_break(30).likely_cause(), "vendor_side");
        assert_eq!(cache_break(6 * 60).likely_cause(), "cache_expired_after_5m");
        assert_eq!(
            cache_break(2 * 60 * 60).likely_cause(),
            "cache_expired_after_1h"
        );
    }

    #[test]
    fn a_users_exposure_change_explains_the_next_prefix_change_only() {
        let mut state = CachePrefixState::default();
        let before = [tool("Read", "Read a file")];
        let after = [tool("Read", "Read a file"), tool("mcp__a__x", "X")];
        state.observe(CachePrefix::of(&model(), &system("s"), &before), Some(900));
        state.exposure_changed();
        assert!(
            state
                .observe(CachePrefix::of(&model(), &system("s"), &before), Some(900))
                .is_none(),
            "a change that leaves the prefix alone breaks nothing"
        );
        state.exposure_changed();
        let cache_break = state
            .observe(CachePrefix::of(&model(), &system("s"), &after), Some(10))
            .unwrap();
        assert_eq!(cache_break.reason, CacheBreakReason::UserChangedExposure);
        assert_eq!(cache_break.added_tools, ["mcp__a__x"]);
        let unexplained = state
            .observe(CachePrefix::of(&model(), &system("s"), &before), Some(10))
            .unwrap();
        assert_eq!(unexplained.reason, CacheBreakReason::PrefixChanged);
    }
}

#[cfg(test)]
mod deferred_order_tests {
    use super::*;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn names(tools: &[ToolDefinition]) -> Vec<&str> {
        tools.iter().map(|tool| tool.name.as_str()).collect()
    }

    /// A server that connects later appends its deferred tools after those offered before, in
    /// this Run and the next, so the definitions already sent keep their place.
    #[test]
    fn deferred_tools_keep_the_order_they_were_first_offered_in() {
        let mut state = CachePrefixState::default();
        let first_run = RunId::new();
        let mut deferred = vec![tool("mcp__b__one"), tool("mcp__d__two")];
        state.order_deferred(&mut deferred);
        assert!(
            state
                .hold(
                    first_run,
                    true,
                    Vec::new(),
                    deferred,
                    UserExposure::default()
                )
                .is_none()
        );
        let mut deferred = vec![
            tool("mcp__a__new"),
            tool("mcp__b__one"),
            tool("mcp__d__two"),
        ];
        state.order_deferred(&mut deferred);
        let held = state
            .hold(
                first_run,
                true,
                Vec::new(),
                deferred,
                UserExposure::default(),
            )
            .unwrap();
        assert_eq!(
            names(&held.deferred),
            ["mcp__b__one", "mcp__d__two", "mcp__a__new"]
        );
        let mut next_run = vec![
            tool("mcp__a__new"),
            tool("mcp__b__one"),
            tool("mcp__c__later"),
            tool("mcp__d__two"),
        ];
        state.order_deferred(&mut next_run);
        assert_eq!(
            names(&next_run),
            ["mcp__b__one", "mcp__d__two", "mcp__a__new", "mcp__c__later"]
        );
    }
}
