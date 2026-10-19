//! A host can let the model reach only what has provenance in the Chat, so injected instructions
//! cannot steer a tool to a target the model composed:
//! - `WebFetch` fetches a URL the user wrote or an earlier search result contained, a link that a
//!   page fetched from such an origin gave on that same origin, or a redirect target within the
//!   site that redirected.
//! - `ArtifactEdit` and `ArtifactComment` write without review to an Artifact the Chat's family
//!   created, the user named in their input, linked to one of the family's Loops or shared with
//!   the Chat through the host; any other target is refused.
//!
//! The Chat's delegation line counts as one conversation for what the user wrote. What the
//! Chat's own work vouches for is kept in its Transcript, so another host finds it after a
//! handover.

use std::collections::{BTreeSet, HashMap, HashSet};

use tickloop_agent_protocol::{ChatId, Content, Role};
use tickloop_agent_tools::web_provenance::{
    origin_of, origins, provenance_key, provenance_refusal, urls_in,
};
use tickloop_agent_tools::{PreparedTool, WEB_FETCH_TOOL_NAME, WEB_SEARCH_TOOL_NAME};
use tickloop_agent_transcript::ProvenanceEntry;

use crate::mcp_tools::{ARTIFACT_COMMENT_TOOL, ARTIFACT_EDIT_TOOL};
use crate::{Agent, Chat, Receipt};

/// The most page links a Chat keeps; later ones give no provenance.
const MAX_KEPT_PAGE_LINKS: usize = 4_000;
/// The most redirect targets or Artifacts a Chat keeps.
const MAX_KEPT_TARGETS: usize = 1_000;
/// The shortest word of the user's input taken for an Artifact id.
const MIN_ARTIFACT_ID_CHARACTERS: usize = 8;

/// Whether an Artifact write may skip review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ArtifactAdmission {
    /// The host does not ask for provenance, or the call writes to no existing Artifact.
    Ordinary,
    /// The target has provenance: the call is admitted without review.
    Vouched,
}

impl Agent {
    /// Refuses a `WebFetch` of a URL without provenance when the host requires it.
    pub(super) async fn check_web_fetch_provenance(
        &self,
        receipt: &Receipt,
        prepared: &PreparedTool,
    ) -> Result<(), String> {
        if !self.inner.tool_policy.web_fetch_requires_provenance
            || prepared.name != WEB_FETCH_TOOL_NAME
        {
            return Ok(());
        }
        let url = prepared.input["url"].as_str().unwrap_or_default();
        let Some(key) = provenance_key(url) else {
            return Err(provenance_refusal(url));
        };
        let chats = self.inner.chats.lock().await;
        let line = delegation_line(&chats, receipt.chat_id);
        let vouched = line
            .iter()
            .flat_map(|chat| user_texts(chat).into_iter().chain(search_results(chat)))
            .flat_map(|text| urls_in(&text))
            .collect::<HashSet<_>>();
        if vouched.contains(&key) {
            return Ok(());
        }
        let vouched_origins = origins(&vouched);
        let is_linked_on_a_vouched_origin = origin_of(&key)
            .is_some_and(|origin| vouched_origins.contains(&origin))
            && line
                .iter()
                .any(|chat| chat.bookkeeping.provenance.page_links.contains(&key));
        let is_redirected_within_its_site = line
            .iter()
            .any(|chat| chat.bookkeeping.provenance.redirect_targets.contains(&key));
        if is_linked_on_a_vouched_origin || is_redirected_within_its_site {
            Ok(())
        } else {
            Err(provenance_refusal(url))
        }
    }

    /// Keeps what the Chat's fetches saw, under a host that asks `WebFetch` for provenance.
    pub(super) async fn remember_fetch_observations(
        &self,
        chat_id: ChatId,
        tools: &tickloop_agent_tools::Tools,
    ) {
        if !self.inner.tool_policy.web_fetch_requires_provenance {
            return;
        }
        let observed = tools.take_fetch_observations();
        if observed.page_links.is_empty() && observed.redirect_targets.is_empty() {
            return;
        }
        self.change_provenance(chat_id, |provenance| {
            let added_links = extend_bounded(
                &mut provenance.page_links,
                observed.page_links,
                MAX_KEPT_PAGE_LINKS,
            );
            let added_targets = extend_bounded(
                &mut provenance.redirect_targets,
                observed.redirect_targets,
                MAX_KEPT_TARGETS,
            );
            added_links || added_targets
        })
        .await;
    }

    /// Keeps an Artifact the Chat created, under a host that asks Artifact writes for
    /// provenance.
    pub(super) async fn remember_created_artifact(&self, chat_id: ChatId, artifact_id: &str) {
        if !self.inner.tool_policy.artifact_writes_require_provenance {
            return;
        }
        self.change_provenance(chat_id, |provenance| {
            extend_bounded(
                &mut provenance.artifacts,
                [artifact_id.to_owned()],
                MAX_KEPT_TARGETS,
            )
        })
        .await;
    }

    /// Records that the user shared the Artifact `artifact_id` with the Chat through the host,
    /// such as by choosing it in the host's interface: the Chat's family may then write to it
    /// without review where Artifact writes need provenance.
    pub async fn share_artifact(
        &self,
        chat_id: ChatId,
        artifact_id: &str,
    ) -> Result<(), crate::AgentError> {
        if artifact_id.trim().is_empty() {
            return Err(crate::AgentError::Invalid(
                "an Artifact id is required".into(),
            ));
        }
        self.make_resident(chat_id).await?;
        self.change_provenance(chat_id, |provenance| {
            extend_bounded(
                &mut provenance.artifacts,
                [artifact_id.to_owned()],
                MAX_KEPT_TARGETS,
            )
        })
        .await;
        Ok(())
    }

    /// Applies `change` to the Chat's provenance and records it when it changed.
    async fn change_provenance(
        &self,
        chat_id: ChatId,
        change: impl FnOnce(&mut ProvenanceEntry) -> bool,
    ) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        if !change(&mut chat.bookkeeping.provenance) {
            return;
        }
        let entry = ProvenanceEntry {
            chat_id,
            ..chat.bookkeeping.provenance.clone()
        };
        if let Some(recorder) = &self.inner.recorder {
            recorder.record_provenance(chat_id, entry);
        }
    }

    /// Whether an `ArtifactEdit` or `ArtifactComment` call may skip review, or why it is refused,
    /// when the host asks Artifact writes for provenance.
    pub(super) async fn check_artifact_provenance(
        &self,
        receipt: &Receipt,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<ArtifactAdmission, String> {
        if !self.inner.tool_policy.artifact_writes_require_provenance
            || !matches!(name, ARTIFACT_EDIT_TOOL | ARTIFACT_COMMENT_TOOL)
        {
            return Ok(ArtifactAdmission::Ordinary);
        }
        // A new Artifact belongs to this Chat from the start.
        if name == ARTIFACT_EDIT_TOOL && input["action"].as_str() == Some("create") {
            return Ok(ArtifactAdmission::Vouched);
        }
        let Some(target) = input["artifact_id"].as_str().filter(|id| !id.is_empty()) else {
            return Ok(ArtifactAdmission::Ordinary);
        };
        let chats = self.inner.chats.lock().await;
        let names_the_target = |id: &str| tickloop_agent_artifacts::artifact_ids_match(id, target);
        let is_the_familys = family_of(&chats, receipt.chat_id).iter().any(|chat| {
            chat.bookkeeping
                .provenance
                .artifacts
                .iter()
                .any(|id| names_the_target(id))
                || chat
                    .snapshot
                    .loops
                    .iter()
                    .flat_map(|goal| &goal.event_sources)
                    .any(|source| names_the_target(&source.artifact_id))
        });
        let is_named_by_the_user = delegation_line(&chats, receipt.chat_id)
            .iter()
            .flat_map(|chat| user_texts(chat))
            .any(|text| {
                text.split(|character: char| {
                    !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
                })
                .filter(|word| word.len() >= MIN_ARTIFACT_ID_CHARACTERS)
                .any(names_the_target)
            });
        if is_the_familys || is_named_by_the_user {
            Ok(ArtifactAdmission::Vouched)
        } else {
            Err(format!(
                "{name} only changes an Artifact this conversation created or the user shared; \
                 {target} is neither. Ask the user to share the Artifact's link in the \
                 conversation."
            ))
        }
    }
}

/// Adds `items` to `kept` up to `limit` entries; whether anything was added.
fn extend_bounded(
    kept: &mut BTreeSet<String>,
    items: impl IntoIterator<Item = String>,
    limit: usize,
) -> bool {
    let before = kept.len();
    for item in items {
        if kept.len() >= limit {
            break;
        }
        kept.insert(item);
    }
    kept.len() != before
}

/// The Chat and its delegation ancestors, the Chat first.
fn delegation_line(chats: &HashMap<ChatId, Chat>, chat_id: ChatId) -> Vec<&Chat> {
    let ancestors = crate::family_tree::lineage(chats, chat_id).unwrap_or_default();
    std::iter::once(chat_id)
        .chain(ancestors.iter().map(|parent| parent.chat_id))
        .filter_map(|chat_id| chats.get(&chat_id))
        .collect()
}

/// Every member of the Chat's family held here.
fn family_of(chats: &HashMap<ChatId, Chat>, chat_id: ChatId) -> Vec<&Chat> {
    let Ok(root) = crate::family_tree::root(chats, chat_id) else {
        return chats.get(&chat_id).into_iter().collect();
    };
    std::iter::once(root)
        .chain(crate::family_tree::descendants(chats, root).unwrap_or_default())
        .filter_map(|member| chats.get(&member))
        .collect()
}

/// What the user wrote in the Chat's current chain, answers to its questions included. A chain
/// whose user inputs cannot be told apart gives nothing.
fn user_texts(chat: &Chat) -> Vec<String> {
    crate::approval_context::originals_for_active_chain(&chat.snapshot, &chat.bookkeeping)
        .map(|inputs| inputs.into_iter().map(|input| input.text).collect())
        .unwrap_or_default()
}

/// The results of the Chat's `WebSearch` calls.
fn search_results(chat: &Chat) -> Vec<String> {
    let messages = &chat.snapshot.messages;
    let searches = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolUse { id, name, .. } if name == WEB_SEARCH_TOOL_NAME => Some(id),
            _ => None,
        })
        .collect::<HashSet<_>>();
    messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error: false,
            } if searches.contains(tool_use_id) => Some(content.text().into_owned()),
            _ => None,
        })
        .collect()
}
