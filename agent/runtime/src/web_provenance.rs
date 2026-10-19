//! A host can let `WebFetch` fetch only URLs with provenance in the Chat: written by the user, or
//! contained in an earlier search result or fetched page. The Chat's delegation line counts as
//! one conversation, so a Subagent may fetch what its ancestors' users gave.

use tickloop_agent_protocol::{Content, Role};
use tickloop_agent_tools::web_provenance::{provenance_key, provenance_refusal, urls_in};
use tickloop_agent_tools::{PreparedTool, WEB_FETCH_TOOL_NAME, WEB_SEARCH_TOOL_NAME};

use crate::{Agent, Chat, Receipt};

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
        let ancestors = crate::family_tree::lineage(&chats, receipt.chat_id).unwrap_or_default();
        let has_provenance = std::iter::once(receipt.chat_id)
            .chain(ancestors.iter().map(|parent| parent.chat_id))
            .filter_map(|chat_id| chats.get(&chat_id))
            .any(|chat| has_provenance(chat, url, &key));
        if has_provenance {
            Ok(())
        } else {
            Err(provenance_refusal(url))
        }
    }
}

fn has_provenance(chat: &Chat, url: &str, key: &str) -> bool {
    let mentions = |text: &str| urls_in(text).iter().any(|found| found == key);
    chat.tools.has_observed_url(url)
        || user_texts(chat).iter().any(|text| mentions(text))
        || search_results(chat).iter().any(|text| mentions(text))
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
        .collect::<std::collections::HashSet<_>>();
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
