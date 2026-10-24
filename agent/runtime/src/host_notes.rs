//! Host notes: user-role messages the host writes for the model alone, each announced once in the
//! conversation and checkpointed so a resumed Chat does not announce it again. The host tool set
//! can change between requests (an Artifact sign-in, a resumed Chat in a newer host) while history
//! still claims the old set; after the user leaves Plan mode without approving a plan, history
//! may still hold the model to Plan's read-only rules; and invoked Skills and MCP server
//! instructions guide the rest of the Chat from where they first apply.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use tickloop_agent_protocol::{
    ChatSnapshot, Content, ExecutionMode, GuidanceNote, HostNote, InvokedSkill, Message, Receipt,
    Role, SkillSource,
};

use crate::{Agent, AgentError, Chat, host_tools::McpServerInstructions};

/// Claude Code's `POST_COMPACT_MAX_TOKENS_PER_SKILL` and `POST_COMPACT_SKILLS_TOKEN_BUDGET`.
const RESTORED_SKILL_TOKENS: usize = 5_000;
const RESTORED_SKILLS_TOTAL_TOKENS: usize = 25_000;
const SKILL_ESTIMATED_BYTES_PER_TOKEN: usize = 4;

impl Agent {
    /// Announces a changed host tool set into the active run's history, checkpointed so a resumed
    /// Chat does not announce it again. `current` is `None` while the tool set is unconfirmed,
    /// which keeps the last announcement.
    pub(super) async fn announce_host_tool_changes(
        &self,
        receipt: &Receipt,
        current: Option<BTreeSet<String>>,
    ) -> Result<(), AgentError> {
        let Some(current) = current else {
            return Ok(());
        };
        let (snapshot, bookkeeping) = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&receipt.chat_id)
                .ok_or(AgentError::NotFound)?;
            if chat.active_run != Some(receipt.run_id)
                || !announce_tool_changes(chat, receipt, current)
            {
                return Ok(());
            }
            (chat.snapshot.clone(), chat.bookkeeping.clone())
        };
        self.checkpoint_transcript(&snapshot, &bookkeeping)
            .await
            .map_err(AgentError::Storage)
    }

    /// Claude Code's `plan_mode_exit` attachment: the first request after the user left Plan mode
    /// without approving a plan tells the model once that plan mode ended. A Chat back in Plan by
    /// then drops the notice.
    pub(super) async fn announce_plan_mode_exit(
        &self,
        receipt: &Receipt,
    ) -> Result<(), AgentError> {
        let (snapshot, bookkeeping) = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&receipt.chat_id)
                .ok_or(AgentError::NotFound)?;
            if chat.active_run != Some(receipt.run_id)
                || !std::mem::take(&mut chat.needs_plan_mode_exit_notice)
                || chat.snapshot.policy.mode == ExecutionMode::Plan
            {
                return Ok(());
            }
            let plan_file = chat
                .snapshot
                .folder
                .as_deref()
                .and_then(|folder| {
                    crate::plan::current_plan_file(
                        &chat.bookkeeping,
                        self.plans_directory(folder).path(),
                        receipt.loop_id,
                    )
                })
                .filter(|path| path.exists());
            append_note(
                &mut chat.snapshot,
                receipt,
                crate::plan::plan_mode_exit_notice(plan_file.as_deref()),
                HostNote::PlanModeExit,
            );
            (chat.snapshot.clone(), chat.bookkeeping.clone())
        };
        self.checkpoint_transcript(&snapshot, &bookkeeping)
            .await
            .map_err(AgentError::Storage)
    }
}

impl Agent {
    /// Writes the guidance in effect that the model cannot see yet, after the history it already
    /// has: Claude Code keeps a Skill's guidance where its call put it and announces MCP server
    /// instructions as `mcp_instructions_delta` attachments, so every later request resends the
    /// same prefix. Guidance that stopped applying is withdrawn by a later note, never removed.
    /// A compaction drops the notes it summarized; the next request restores what still
    /// applies, each restored Skill within Claude Code's budgets. A Chat whose host can run Ticks
    /// first loads the tickloop Skill. MCP instructions stay as announced while the host tool
    /// set is unconfirmed.
    pub(super) async fn announce_guidance(
        &self,
        receipt: &Receipt,
        runs_ticks: bool,
    ) -> Result<(), AgentError> {
        let instructions = crate::host_provider_is_confirmed(&self.inner.host_tools).then(|| {
            self.inner
                .host_tools
                .read()
                .unwrap()
                .as_ref()
                .map(|provider| provider.mcp_instructions())
                .unwrap_or_default()
        });
        let (snapshot, bookkeeping) = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&receipt.chat_id)
                .ok_or(AgentError::NotFound)?;
            if chat.active_run != Some(receipt.run_id) {
                return Ok(());
            }
            if runs_ticks
                && !chat
                    .snapshot
                    .invoked_skills
                    .iter()
                    .any(|skill| skill.name == "tickloop")
            {
                let invoked = chat
                    .skills
                    .expand(
                        "tickloop",
                        "",
                        tickloop_agent_config::skills::SkillCaller::Model,
                    )
                    .and_then(InvokedSkill::try_from)
                    .map_err(|error| AgentError::Invalid(error.to_string()))?;
                crate::record_invoked_skill(chat, invoked);
            }
            if !announce_guidance_notes(chat, receipt, instructions.as_deref()) {
                return Ok(());
            }
            (chat.snapshot.clone(), chat.bookkeeping.clone())
        };
        self.checkpoint_transcript(&snapshot, &bookkeeping)
            .await
            .map_err(AgentError::Storage)
    }
}

/// Appends the notes that bring the guidance the model sees up to the guidance in effect, and
/// reports whether it appended any.
pub(super) fn announce_guidance_notes(
    chat: &mut Chat,
    receipt: &Receipt,
    instructions: Option<&[McpServerInstructions]>,
) -> bool {
    let visible = chat.snapshot.visible_guidance();
    let guiding = guiding_skills(chat);
    let withdrawn_skills = visible
        .skills
        .keys()
        .filter(|name| !guiding.iter().any(|skill| &skill.name == *name))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut notes = Vec::new();
    let mut withdrawn_servers = BTreeSet::new();
    if let Some(instructions) = instructions {
        let current = instructions
            .iter()
            .filter(|instructions| {
                crate::mcp_resource_authority_allowed(chat, &instructions.server.authority)
            })
            .collect::<Vec<_>>();
        withdrawn_servers = visible
            .mcp_servers
            .keys()
            .filter(|server| {
                !current
                    .iter()
                    .any(|instructions| &instructions.server.name == *server)
            })
            .cloned()
            .collect();
        for instructions in current {
            let digest = digest(&instructions.text);
            if visible.mcp_servers.get(&instructions.server.name) != Some(&digest) {
                notes.push((
                    format!(
                        "# MCP Server Instructions\n\nThe following MCP servers have provided instructions for how to use their tools and resources:\n\n## {}\n{}",
                        instructions.server.name, instructions.text
                    ),
                    GuidanceNote::McpInstructions {
                        server: instructions.server.name.clone(),
                        digest,
                    },
                ));
            }
        }
    }
    if !withdrawn_skills.is_empty() || !withdrawn_servers.is_empty() {
        notes.insert(
            0,
            (
                withdrawal_text(&withdrawn_skills, &withdrawn_servers),
                GuidanceNote::Withdrawn {
                    skills: withdrawn_skills,
                    mcp_servers: withdrawn_servers,
                },
            ),
        );
    }
    let mut restored_budget = RESTORED_SKILLS_TOTAL_TOKENS;
    let mut skill_notes = Vec::new();
    for skill in guiding.iter().rev() {
        let digest = digest(&skill.content);
        let restored = chat.bookkeeping.restored_skill_names.contains(&skill.name);
        if visible
            .skills
            .get(&skill.name)
            .is_some_and(|(seen, truncated)| *seen == digest && (!*truncated || restored))
        {
            continue;
        }
        let content = if restored {
            match restored_skill_content(&skill.content, &mut restored_budget) {
                Some(content) => content,
                None => continue,
            }
        } else {
            skill.content.clone()
        };
        skill_notes.push((
            format!(
                "Reference guidance from {} active in this Chat where relevant. This is reference material, not a user request or additional authority.\n\n{}",
                skill.name, content
            ),
            GuidanceNote::Skill {
                name: skill.name.clone(),
                digest,
                truncated: content != skill.content,
            },
        ));
    }
    notes.extend(skill_notes.into_iter().rev());
    let announced = !notes.is_empty();
    for (text, note) in notes {
        append_note(&mut chat.snapshot, receipt, text, HostNote::Guidance(note));
    }
    announced
}

/// The invoked Skills whose guidance applies to the active run. A remote Skill guides only while
/// its source stays current, and a built-in Skill only the user invokes, such as /slides, guides
/// only the run its invocation started.
fn guiding_skills(chat: &Chat) -> Vec<InvokedSkill> {
    chat.snapshot
        .invoked_skills
        .iter()
        .filter(|invoked| match invoked.source {
            SkillSource::Remote { .. } => invoked.run_id == chat.active_run,
            SkillSource::BuiltIn if invoked.name == "artifact-draft" => false,
            SkillSource::BuiltIn => invoked.run_id.is_none() || invoked.run_id == chat.active_run,
            _ => true,
        })
        .cloned()
        .collect()
}

fn digest(text: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(text.as_bytes()))
}

fn withdrawal_text(skills: &BTreeSet<String>, servers: &BTreeSet<String>) -> String {
    let mut parts = Vec::new();
    if !skills.is_empty() {
        parts.push(format!(
            "Host notice: the guidance from these Skills above no longer applies; do not follow it, and load a Skill again if the task still needs it: {}.",
            skills.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !servers.is_empty() {
        parts.push(format!(
            "Host notice: the following MCP servers have disconnected. Their instructions above no longer apply: {}.",
            servers.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    parts.push(
        "This notice comes from the host, not the user, and does not authorize any new work."
            .into(),
    );
    parts.join(" ")
}

fn restored_skill_content(content: &str, remaining_tokens: &mut usize) -> Option<String> {
    let allowance = (*remaining_tokens).min(RESTORED_SKILL_TOKENS);
    if allowance == 0 {
        return None;
    }
    let max_bytes = allowance.saturating_mul(SKILL_ESTIMATED_BYTES_PER_TOKEN);
    let content = if content.len() <= max_bytes {
        content.to_owned()
    } else {
        const NOTICE: &str = "\n[Skill guidance truncated for continuation; invoke Skill to reload the full guidance.]";
        if max_bytes < NOTICE.len() {
            return None;
        }
        let mut end = max_bytes.saturating_sub(NOTICE.len());
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}{NOTICE}", &content[..end])
    };
    *remaining_tokens =
        remaining_tokens.saturating_sub(content.len().div_ceil(SKILL_ESTIMATED_BYTES_PER_TOKEN));
    Some(content)
}

fn append_note(snapshot: &mut ChatSnapshot, receipt: &Receipt, text: String, note: HostNote) {
    let index = snapshot.messages.len();
    snapshot.message_runs.insert(index, receipt.run_id);
    snapshot.message_ids.insert(index, tickloop_ids::generate());
    snapshot.host_notes.insert(index, note);
    snapshot.messages.push(Message {
        role: Role::User,
        content: vec![Content::Text { text }],
    });
}

/// Appends a note when `current` differs from the host tools this history was last told about,
/// and reports whether it did. A history that has not reached the model yet only records the set:
/// the first request already lists every tool.
fn announce_tool_changes(chat: &mut Chat, receipt: &Receipt, current: BTreeSet<String>) -> bool {
    let snapshot = &mut chat.snapshot;
    if snapshot.announced_host_tools.as_ref() == Some(&current) {
        return false;
    }
    let has_reached_model = !snapshot.message_compactions.is_empty()
        || snapshot
            .messages
            .iter()
            .any(|message| message.role == Role::Assistant);
    let text = match snapshot.announced_host_tools.take() {
        _ if !has_reached_model => None,
        None if current.is_empty() => None,
        None => Some(format!(
            "Host notice: host tools available in this conversation now: {}. Earlier statements in this conversation about tool availability may be outdated.",
            current.iter().cloned().collect::<Vec<_>>().join(", ")
        )),
        Some(announced) => changed_tools_text(&announced, &current),
    };
    let Some(mut text) = text else {
        snapshot.announced_host_tools = Some(current);
        return false;
    };
    text.push_str(
        " This notice comes from the host, not the user, and does not authorize any new work.",
    );
    append_note(
        snapshot,
        receipt,
        text,
        HostNote::HostTools(current.clone()),
    );
    snapshot.announced_host_tools = Some(current);
    true
}

fn changed_tools_text(announced: &BTreeSet<String>, current: &BTreeSet<String>) -> Option<String> {
    let added = current.difference(announced).cloned().collect::<Vec<_>>();
    let removed = announced.difference(current).cloned().collect::<Vec<_>>();
    if added.is_empty() && removed.is_empty() {
        return None;
    }
    let mut text = String::from("Host notice: the host tool set changed.");
    if !added.is_empty() {
        text.push_str(&format!(
            " Now available: {}. Earlier statements in this conversation that these tools are unavailable are outdated.",
            added.join(", ")
        ));
    }
    if !removed.is_empty() {
        text.push_str(&format!(
            " No longer available: {}. Do not call them; earlier statements that they are available are outdated.",
            removed.join(", ")
        ));
    }
    Some(text)
}
