//! `/context`: what the next conversation request of a Chat would hold, by category, counted
//! with the tokenizer, and the MCP tools that only tool search reaches. Reading it calls no
//! model, connects no MCP server and changes nothing in the Chat.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use tickloop_harness_model::{context_tokens, native_tool_search};
use tickloop_harness_policy::instructions::{InstructionFile, user_context_message};
use tickloop_harness_policy::skills::SKILL_TOOL_NAME;
use tickloop_harness_protocol::mcp::ToolExposure;
use tickloop_harness_protocol::{
    ChatId, ChatSnapshot, CompactionMarker, Content, ContextBreakdown, ContextCategory,
    ContextCategoryUsage, ContextDraft, ContextItem, ContextTokenCounter, ContextToolServer,
    GuidanceNote, HostNote, Message, Protocol, Role, ToolDefinition, ToolSearchMode,
    ToolSearchPool, UnavailableToolServer, Usage, message_content_tokens,
};

use crate::tool_exposure::{DESCRIBE_TOOLS, EXECUTE_TOOLS, SEARCH_TOOLS, SearchSupport};
use crate::{Agent, AgentError, Chat};

const FOLD_TOOLS: [&str; 3] = [SEARCH_TOOLS, DESCRIBE_TOOLS, EXECUTE_TOOLS];
const MAX_SEARCH_LABEL_CHARS: usize = 60;
/// Where the tool search pool lists the runtime's own deferred tools, beside MCP servers; no MCP
/// server can take this name.
const RUNTIME_TOOL_SOURCE: &str = "(built-in)";
const USER_MESSAGES: &str = "User messages";
const ASSISTANT_MESSAGES: &str = "Assistant messages";
const TOOL_RESULTS: &str = "Tool results";
/// How far the service-reported total may exceed the count at the measured token ratio before
/// the excess is shown apart.
const MAX_CALIBRATION_GAP_PERCENT: u64 = 15;
/// How many counted tokens of a Chat's own measurements weigh as much as the starting ratio.
const PRIOR_RATIO_WEIGHT_TOKENS: usize = 4_000;
/// The token ratios one measurement may report; beyond them it measured something else, such as
/// a system prompt that changed between the two requests.
const MIN_TOKEN_RATIO: f64 = 0.5;
const MAX_TOKEN_RATIO: f64 = 3.0;

impl Agent {
    /// The current context of `chat_id` by category, as `/context` shows it.
    pub async fn context_breakdown(&self, chat_id: ChatId) -> Result<ContextBreakdown, AgentError> {
        let instruction_files = self.instruction_files(chat_id).await?;
        let current_date = self.current_date();
        let parts = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            self.context_parts(chat, &instruction_files, &current_date)?
        };
        // A long Chat takes a while to count; the executor keeps serving other Chats meanwhile.
        tokio::task::spawn_blocking(move || {
            parts.count(&*tickloop_harness_model::context_tokens::TOKEN_COUNTER)
        })
        .await
        .map_err(|error| AgentError::Invalid(format!("counting context failed: {error}")))
    }

    /// What the first request of a New Agent working in `folder` would hold, before the Agent
    /// exists: the draft's model and settings, no messages. It creates no Chat, writes nothing
    /// and connects nothing; the tools are those this Agent's host offers now, with the file
    /// tools its Execution Environment would offer for `folder`.
    pub async fn draft_context_breakdown(
        &self,
        folder: Option<PathBuf>,
        draft: ContextDraft,
    ) -> Result<ContextBreakdown, AgentError> {
        let instruction_files = self.instruction_files_in(folder.clone()).await?;
        let current_date = self.current_date();
        let settings = self.load_user_settings(folder.clone()).await?;
        let chat = self.draft_chat(folder, draft, settings).await;
        let parts = self.context_parts(&chat, &instruction_files, &current_date)?;
        tokio::task::spawn_blocking(move || {
            parts.count(&*tickloop_harness_model::context_tokens::TOKEN_COUNTER)
        })
        .await
        .map_err(|error| AgentError::Invalid(format!("counting context failed: {error}")))
    }

    /// A Chat value as creating one for `draft` would start it, registered nowhere.
    async fn draft_chat(
        &self,
        folder: Option<PathBuf>,
        draft: ContextDraft,
        mut settings: Option<tickloop_harness_policy::settings::SettingsSnapshot>,
    ) -> Chat {
        let mut snapshot =
            crate::empty_snapshot(tickloop_ids::generate().into(), folder, draft.model);
        let defaults = tickloop_harness_policy::new_chat::new_chat_settings(
            settings.as_mut(),
            &self.inner.host_options,
            &snapshot.model,
        );
        snapshot.policy.mode = draft.execution_mode;
        snapshot.search_config = draft.search_config;
        snapshot.task_tools_enabled = draft.task_tools_enabled;
        snapshot.context_window_tokens = defaults.context_window_tokens;
        snapshot.auto_compact_window = settings
            .as_ref()
            .and_then(|settings| settings.auto_compact_window);
        if let crate::SearchDefaults::Fixed(config) = &self.inner.search {
            snapshot.search_config = config.clone();
        }
        let client = self
            .inner
            .selected_model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .client
            .clone();
        let execution =
            crate::chat_execution::ChatExecution::bind(self.inner.environment.as_ref(), &snapshot)
                .await;
        let skills = self.discover_skills(
            snapshot
                .folder
                .as_deref()
                .filter(|_| !execution.folder_in_environment()),
        );
        crate::NewChat {
            tools: self.folderless_chat_tools(&snapshot),
            execution,
            skills,
            output_style: settings
                .map(|settings| settings.output_style)
                .unwrap_or_default(),
            snapshot,
            bookkeeping: Default::default(),
            client,
            user_context: Default::default(),
            environment: Default::default(),
            announced_date: None,
            file_history: None,
        }
        .into_chat()
    }

    fn context_parts(
        &self,
        chat: &Chat,
        instruction_files: &[InstructionFile],
        current_date: &str,
    ) -> Result<ContextParts, AgentError> {
        let provider = self.inner.host_tools.read().unwrap().clone();
        let mut registry = self.tool_registry(chat);
        registry.refresh(chat);
        let search_provider = provider
            .as_ref()
            .filter(|provider| self.inner.tool_policy.artifacts && provider.has_tool_search());
        let discovery = search_provider
            .map(|provider| crate::tool_execution::chat_discovery_tools(chat, provider.as_ref()))
            .unwrap_or_default();
        let direct_servers = discovery
            .iter()
            .filter(|tool| tool.exposure == ToolExposure::Direct)
            .filter_map(|tool| Some((tool.definition.name.clone(), tool.server.clone()?)))
            .collect::<HashMap<_, _>>();
        let mut system_tools = Vec::new();
        let mut skill_tool = None;
        let mut mcp_tools = Vec::new();
        for definition in registry.definitions() {
            if definition.name == SKILL_TOOL_NAME {
                skill_tool = Some(definition);
            } else if let Some(server) = direct_servers.get(&definition.name) {
                mcp_tools.push((server.clone(), definition));
            } else {
                system_tools.push(definition);
            }
        }
        let window = crate::context::context_window(&chat.snapshot);
        let skills = skill_tool
            .as_ref()
            .map(|_| {
                chat.skills
                    .model_catalog_for_context(window)
                    .content
                    .lines()
                    .map(|line| (skill_name(line), line.to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let searched = registry.searched_definitions();
        let search = (search_provider.is_some() || !searched.is_empty()).then(|| {
            let mode = match crate::tool_execution::search_support(chat) {
                SearchSupport::Native => ToolSearchMode::Native,
                SearchSupport::Fold => ToolSearchMode::SearchTools,
            };
            let runtime_pool = searched.into_iter().map(|definition| {
                (
                    RUNTIME_TOOL_SOURCE.to_owned(),
                    crate::tool_exposure::ExposureTool::deferred_runtime_tool(definition),
                )
            });
            let mcp_pool = discovery
                .iter()
                .filter(|tool| tool.exposure == ToolExposure::Search)
                .filter_map(|tool| Some((tool.server.clone()?, tool.clone())));
            SearchParts {
                mode,
                protocol: chat.snapshot.model.adapter().protocol(),
                pool: runtime_pool
                    .chain(mcp_pool)
                    .map(|(source, tool)| {
                        let definition = match mode {
                            ToolSearchMode::Native => tool.deferred_definition(),
                            ToolSearchMode::SearchTools => (*tool.definition).clone(),
                        };
                        (source, definition)
                    })
                    .collect(),
                unavailable: search_provider
                    .map(|provider| provider.unavailable_tool_servers())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|server| UnavailableToolServer {
                        name: server.name,
                        state: server.state,
                    })
                    .collect(),
                deferred: registry.deferred_definitions(),
            }
        });
        let (mut messages, _, origins) = crate::context::model_history_with_origins(chat)?;
        let message_labels = origins
            .iter()
            .map(|index| index.and_then(|index| host_message_label(&chat.snapshot, index)))
            .collect();
        let model = chat.snapshot.model.identity();
        let mut responses = HashSet::new();
        let message_usage = origins
            .iter()
            .zip(&messages)
            .map(|(index, message)| {
                let index = (*index)?;
                (message.role == Role::Assistant
                    && chat.snapshot.message_models.get(&index) == Some(&model)
                    && responses.insert(index))
                .then(|| chat.snapshot.message_usage.get(&index).cloned())
                .flatten()
            })
            .collect();
        match chat.tools.chat_directory() {
            Some(_) => crate::tool_results::apply_tool_result_replacements(
                &mut messages,
                &chat.content_replacements,
            ),
            None => crate::tool_results::cut_tool_results_over_budget(
                &mut messages,
                crate::tool_results::UnkeptResults::of(&chat.snapshot),
            ),
        }
        let workspace_host = chat
            .snapshot
            .folder
            .is_some()
            .then(|| crate::host_tools::current_workspace_host(provider.as_deref()));
        let user_context = chat
            .user_context
            .get()
            .cloned()
            .unwrap_or_else(|| user_context_message(instruction_files, current_date, None));
        Ok(ContextParts {
            system_prompt: self.conversation_system_message(
                chat,
                workspace_host.as_ref(),
                &registry.searched_names(),
            ),
            system_tools,
            skill_tool,
            skills,
            mcp_tools,
            user_context: crate::instruction_context::context_message(user_context),
            instruction_files: instruction_files
                .iter()
                .map(|file| (file.path.clone(), file.content.clone()))
                .collect(),
            messages,
            message_labels,
            message_usage,
            protocol: chat.snapshot.model.protocol,
            prior_token_ratio: tickloop_harness_model::cards::context_token_ratio(
                &chat.snapshot.model,
            ),
            search,
            window,
            compaction_trigger: crate::context::auto_compact_tokens(&chat.snapshot),
            reported_total: crate::context::is_context_measured(&chat.snapshot)
                .then(|| {
                    tickloop_harness_protocol::current_context_tokens(
                        &chat.snapshot,
                        &chat.snapshot.request_usage,
                        &chat.snapshot.model,
                    )
                })
                .flatten(),
        })
    }
}

/// What the host wrote at snapshot message `index`, as `/context` names it among the messages:
/// host notes and compaction summaries are user-role messages the user never typed.
fn host_message_label(snapshot: &ChatSnapshot, index: usize) -> Option<String> {
    if let Some(note) = snapshot.host_notes.get(&index) {
        return Some(match note {
            HostNote::Guidance(GuidanceNote::Skill { name, .. }) => {
                format!("Skill guidance · {name}")
            }
            HostNote::Guidance(GuidanceNote::McpInstructions { server, .. }) => {
                format!("MCP server instructions · {server}")
            }
            HostNote::Guidance(GuidanceNote::McpServers { servers }) => format!(
                "MCP server instructions · {}",
                servers.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
            HostNote::Guidance(GuidanceNote::Withdrawn { .. }) => "Withdrawn guidance".into(),
            HostNote::Guidance(GuidanceNote::McpWarnings { .. }) => {
                "MCP configuration warnings".into()
            }
            HostNote::Guidance(GuidanceNote::Environment { .. }) => {
                "Execution environment guidance".into()
            }
            HostNote::Guidance(GuidanceNote::Later(_)) => "Guidance".into(),
            HostNote::HostTools(_) => "Host tool changes".into(),
            HostNote::DateChange(_) => "Date change".into(),
            HostNote::PlanModeExit => "Plan mode exit".into(),
            HostNote::ExecutionMode(note) => format!("Execution mode · {}", note.mode.label()),
            HostNote::LoopState => "Loop state".into(),
            HostNote::TaskReminder => "Task reminder".into(),
            HostNote::Hook(event) => format!("Hook · {}", event.name()),
        });
    }
    matches!(
        snapshot.message_compactions.get(&index),
        Some(CompactionMarker::Summary { .. })
    )
    .then(|| "Compaction summary".into())
}

/// The name a Skill catalog line lists: `- name [source]: description`.
fn skill_name(line: &str) -> String {
    let entry = line.trim_start_matches("- ");
    entry
        .split_once(" [")
        .map_or(entry, |(name, _)| name)
        .to_owned()
}

/// What the next conversation request of a Chat would hold, collected under the Chats lock and
/// counted outside it.
struct ContextParts {
    system_prompt: Message,
    system_tools: Vec<ToolDefinition>,
    skill_tool: Option<ToolDefinition>,
    /// Each listed Skill's name and catalog line.
    skills: Vec<(String, String)>,
    /// MCP tools sent directly, with their servers.
    mcp_tools: Vec<(String, ToolDefinition)>,
    user_context: Message,
    instruction_files: Vec<(PathBuf, String)>,
    messages: Vec<Message>,
    /// For each message the host wrote, what it is.
    message_labels: Vec<Option<String>>,
    /// For each response of the Chat's current model, the usage its service reported.
    message_usage: Vec<Option<Usage>>,
    /// The protocol of the Chat's current model, which decides what its usage counts as input.
    protocol: Protocol,
    /// Service tokens per counted token the model's catalog gives, before the Chat measures it.
    prior_token_ratio: Option<f64>,
    search: Option<SearchParts>,
    window: Option<usize>,
    compaction_trigger: Option<usize>,
    /// The current context the status line shows, once a request of the model measured it.
    reported_total: Option<u64>,
}

struct SearchParts {
    mode: ToolSearchMode,
    /// The protocol that carries the deferred tools, which decides what native search counts.
    protocol: Protocol,
    /// Tools only search reaches, with their servers or the runtime's own source, as a load would
    /// send them.
    pool: Vec<(String, ToolDefinition)>,
    unavailable: Vec<UnavailableToolServer>,
    /// The definitions native search can load, which its results name instead of carrying.
    deferred: Vec<ToolDefinition>,
}

/// How the conversation splits between tool search and everything else.
#[derive(Default)]
struct MessageSplit {
    search: Vec<(String, usize)>,
    user: usize,
    assistant: usize,
    tool_results: usize,
    /// Host notes by kind.
    notes: BTreeMap<String, usize>,
}

impl MessageSplit {
    fn messages(&self) -> usize {
        self.user + self.assistant + self.tool_results + self.notes.values().sum::<usize>()
    }
}

impl ContextParts {
    /// Service tokens per counted token for the Chat's current model: the catalog's ratio, or
    /// one, weighed against what consecutive responses measured. A request's input exceeds the
    /// one before by that request's response, which the service counted as its output, and by
    /// what was appended after it, which the counter counts.
    fn token_ratio(&self, counter: &dyn ContextTokenCounter) -> f64 {
        let mut samples = vec![(
            self.prior_token_ratio.unwrap_or(1.0),
            PRIOR_RATIO_WEIGHT_TOKENS,
        )];
        let responses = self
            .message_usage
            .iter()
            .enumerate()
            .filter_map(|(index, usage)| {
                let usage = usage.as_ref()?;
                Some((
                    index,
                    usage.context_input_tokens(self.protocol)?,
                    usage.output_tokens?,
                ))
            })
            .collect::<Vec<_>>();
        for pair in responses.windows(2) {
            let [(earlier, earlier_input, output), (later, later_input, _)] = pair else {
                continue;
            };
            let appended = self.messages[earlier + 1..*later]
                .iter()
                .map(|message| counter.message_tokens(message))
                .sum::<usize>();
            if appended == 0 {
                continue;
            }
            let grown = *later_input as f64 - *earlier_input as f64 - *output as f64;
            let ratio = grown / appended as f64;
            if (MIN_TOKEN_RATIO..=MAX_TOKEN_RATIO).contains(&ratio) {
                samples.push((ratio, appended));
            }
        }
        weighted_median(samples)
    }

    fn count(self, counter: &dyn ContextTokenCounter) -> ContextBreakdown {
        let definition_items = |definitions: &[ToolDefinition]| {
            definitions
                .iter()
                .map(|definition| {
                    (
                        definition.name.clone(),
                        counter.definition_tokens(definition),
                    )
                })
                .collect::<Vec<_>>()
        };
        let system_prompt = counter.message_tokens(&self.system_prompt);
        let system_tools = definition_items(&self.system_tools);
        let skills_total = self
            .skill_tool
            .as_ref()
            .map_or(0, |definition| counter.definition_tokens(definition));
        let skills = self
            .skills
            .iter()
            .map(|(name, line)| (name.clone(), counter.text_tokens(line)))
            .collect::<Vec<_>>();
        let mcp_tools = self
            .mcp_tools
            .iter()
            .map(|(server, definition)| {
                (
                    server.clone(),
                    definition.name.clone(),
                    counter.definition_tokens(definition),
                )
            })
            .collect::<Vec<_>>();
        let tool_search_index = self
            .search
            .as_ref()
            .filter(|search| search.mode == ToolSearchMode::Native)
            .map(|search| {
                search
                    .pool
                    .iter()
                    .map(|(server, definition)| {
                        (
                            server.clone(),
                            definition.name.clone(),
                            native_tool_search::deferred_tool_context_tokens(
                                search.protocol,
                                counter,
                                definition,
                            ),
                        )
                    })
                    .filter(|(_, _, tokens)| *tokens > 0)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let instruction_files = self
            .instruction_files
            .iter()
            .map(|(path, content)| (path.display().to_string(), counter.text_tokens(content)))
            .collect::<Vec<_>>();
        let token_ratio = self.token_ratio(counter);
        let split = split_messages(
            &self.messages,
            &self.message_labels,
            &self.message_usage,
            token_ratio,
            self.search.as_ref(),
            counter,
        );
        let sum = |items: &[(String, usize)]| items.iter().map(|(_, tokens)| tokens).sum::<usize>();
        let counted = [
            (ContextCategory::SystemPrompt, system_prompt),
            (ContextCategory::SystemTools, sum(&system_tools)),
            (
                ContextCategory::McpTools,
                mcp_tools.iter().map(|(_, _, tokens)| tokens).sum(),
            ),
            (
                ContextCategory::ToolSearchIndex,
                tool_search_index.iter().map(|(_, _, tokens)| tokens).sum(),
            ),
            (
                ContextCategory::InstructionFiles,
                counter.message_tokens(&self.user_context),
            ),
            (ContextCategory::Skills, skills_total),
            (ContextCategory::ToolSearchResults, sum(&split.search)),
            (ContextCategory::Messages, split.messages()),
        ];
        let counted_total = counted
            .iter()
            .map(|(_, tokens)| *tokens as u64)
            .sum::<u64>();
        let used_tokens = self.reported_total.unwrap_or(counted_total);
        // The count in the service's tokens; a total close to it calibrates every category, while
        // a total well beyond it holds something the count cannot see, so that part stands apart
        // instead of inflating every category.
        let expected_total = (counted_total as f64 * token_ratio).round() as u64;
        let unattributed = match self.reported_total {
            Some(reported)
                if reported
                    > expected_total.saturating_mul(100 + MAX_CALIBRATION_GAP_PERCENT) / 100 =>
            {
                reported - expected_total
            }
            _ => 0,
        };
        let scale = Scale {
            counted: counted_total,
            used: used_tokens - unattributed,
        };
        let mut categories = counted
            .iter()
            .map(|(category, _)| *category)
            .zip(scale.distribute(counted.iter().map(|(_, tokens)| *tokens)))
            .filter(|(_, tokens)| *tokens > 0)
            .map(|(category, tokens)| ContextCategoryUsage { category, tokens })
            .collect::<Vec<_>>();
        if unattributed > 0 {
            categories.push(ContextCategoryUsage {
                category: ContextCategory::Unattributed,
                tokens: unattributed,
            });
        }
        let items = |items: Vec<(String, usize)>| scale.items(items);
        let window = self.window.map(|window| window as u64);
        let autocompact_buffer_tokens =
            window
                .zip(self.compaction_trigger)
                .map(|(window, trigger)| {
                    window
                        .saturating_sub(trigger as u64)
                        .min(window.saturating_sub(used_tokens))
                });
        let free_tokens = window.map(|window| {
            window
                .saturating_sub(used_tokens)
                .saturating_sub(autocompact_buffer_tokens.unwrap_or(0))
        });
        // Not in context, so the context total never scales it.
        let tool_search = self.search.as_ref().map(|search| ToolSearchPool {
            mode: search.mode,
            servers: Scale::UNSCALED.servers(
                search
                    .pool
                    .iter()
                    .map(|(server, definition)| {
                        (
                            server.clone(),
                            definition.name.clone(),
                            counter.definition_tokens(definition),
                        )
                    })
                    .collect(),
            ),
            unavailable_servers: search.unavailable.clone(),
        });
        ContextBreakdown {
            tokenizer: counter.name().into(),
            context_window_tokens: window,
            used_tokens,
            calibrated: self.reported_total.is_some(),
            categories,
            autocompact_buffer_tokens,
            free_tokens,
            system_tools: items(system_tools),
            mcp_tools: scale.servers(mcp_tools),
            tool_search_index: scale.servers(tool_search_index),
            instruction_files: items(instruction_files),
            skills: items(skills),
            tool_search_results: items(split.search),
            messages: items(
                [
                    (USER_MESSAGES.into(), split.user),
                    (ASSISTANT_MESSAGES.into(), split.assistant),
                    (TOOL_RESULTS.into(), split.tool_results),
                ]
                .into_iter()
                .collect(),
            )
            .into_iter()
            .filter(|item| item.tokens > 0)
            .collect(),
            host_notes: items(split.notes.into_iter().collect()),
            tool_search,
            host_tools_pending: false,
        }
    }
}

/// The middle value of weighted samples: half the weight lies at or below it.
fn weighted_median(mut samples: Vec<(f64, usize)>) -> f64 {
    samples.sort_by(|left, right| left.0.total_cmp(&right.0));
    let total = samples.iter().map(|(_, weight)| weight).sum::<usize>();
    let mut seen = 0;
    for (value, weight) in &samples {
        seen += weight;
        if seen * 2 >= total {
            return *value;
        }
    }
    1.0
}

/// What a response's reasoning holds in context, in counted tokens: what its service reported,
/// else the rest of its output beside the visible part, else the reasoning text history keeps.
fn reasoning_tokens(usage: Option<&Usage>, visible: usize, text: usize, token_ratio: f64) -> usize {
    let counted = |service_tokens: u64| (service_tokens as f64 / token_ratio).round() as usize;
    match usage.map(|usage| (usage.reasoning_output_tokens, usage.output_tokens)) {
        Some((Some(reasoning), _)) => counted(reasoning),
        Some((None, Some(output))) => counted(output).saturating_sub(visible).max(text),
        _ => text,
    }
}

/// Splits the conversation into tool search's part and the rest. SearchTools, DescribeTools and
/// ExecuteTools calls count with their results, one item per call; a vendor search record counts
/// as it is, plus each deferred definition its result loads, once. Reasoning counts as its
/// response's usage says.
fn split_messages(
    messages: &[Message],
    message_labels: &[Option<String>],
    message_usage: &[Option<Usage>],
    token_ratio: f64,
    search: Option<&SearchParts>,
    counter: &dyn ContextTokenCounter,
) -> MessageSplit {
    let mut split = MessageSplit::default();
    let mut fold_calls = HashMap::<String, usize>::new();
    let mut loaded = BTreeSet::new();
    let deferred = search.map_or(&[][..], |search| search.deferred.as_slice());
    for (index, (message, label)) in messages.iter().zip(message_labels).enumerate() {
        if let Some(label) = label {
            *split.notes.entry(label.clone()).or_default() +=
                message_content_tokens(counter, message);
            continue;
        }
        let framing = message_content_tokens(
            counter,
            &Message {
                role: message.role,
                content: Vec::new(),
            },
        );
        match message.role {
            Role::Assistant => split.assistant += framing,
            _ => split.user += framing,
        }
        let mut visible = 0;
        let mut reasoning = None::<usize>;
        for block in &message.content {
            let tokens = message_content_tokens(
                counter,
                &Message {
                    role: message.role,
                    content: vec![block.clone()],
                },
            )
            .saturating_sub(framing);
            if context_tokens::is_reasoning(block) {
                *reasoning.get_or_insert(0) += tokens;
                continue;
            }
            visible += tokens;
            match block {
                Content::ToolUse { id, name, input } if FOLD_TOOLS.contains(&name.as_str()) => {
                    fold_calls.insert(id.clone(), split.search.len());
                    split.search.push((fold_call_label(name, input), tokens));
                }
                Content::ToolResult { tool_use_id, .. } if fold_calls.contains_key(tool_use_id) => {
                    split.search[fold_calls[tool_use_id]].1 += tokens;
                }
                Content::ProtocolData { protocol, value } => {
                    match native_tool_search::search_record(*protocol, value) {
                        Some(record) => {
                            split.search.push((record.label, tokens));
                            for name in record.loaded_tools {
                                if let Some(definition) = deferred
                                    .iter()
                                    .find(|definition| definition.name == name)
                                    .filter(|_| loaded.insert(name.clone()))
                                {
                                    split.search.push((
                                        format!("{name} (loaded definition)"),
                                        counter.definition_tokens(definition),
                                    ));
                                }
                            }
                        }
                        None => split.assistant += tokens,
                    }
                }
                Content::ToolResult { .. } => split.tool_results += tokens,
                _ if message.role == Role::Assistant => split.assistant += tokens,
                _ => split.user += tokens,
            }
        }
        if let Some(text) = reasoning {
            let usage = message_usage.get(index).and_then(Option::as_ref);
            let tokens = reasoning_tokens(usage, visible, text, token_ratio);
            match message.role {
                Role::Assistant => split.assistant += tokens,
                _ => split.user += tokens,
            }
        }
    }
    split
}

fn fold_call_label(name: &str, input: &serde_json::Value) -> String {
    let strings = |values: &serde_json::Value, field: Option<&str>| {
        values
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| match field {
                Some(field) => value[field].as_str(),
                None => value.as_str(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let subject = match name {
        SEARCH_TOOLS => input["query"].as_str().unwrap_or_default().to_owned(),
        DESCRIBE_TOOLS => strings(&input["names"], None),
        _ => strings(&input["calls"], Some("name")),
    };
    let mut label = format!("{name} {subject}");
    if label.chars().count() > MAX_SEARCH_LABEL_CHARS {
        label = label.chars().take(MAX_SEARCH_LABEL_CHARS - 1).collect();
        label.push('…');
    }
    label.trim_end().to_owned()
}

/// Scales counted tokens to the context total the status line shows, so the categories add up
/// to it; before a request measured the context, the counts stand as they are.
struct Scale {
    counted: u64,
    used: u64,
}

impl Scale {
    const UNSCALED: Self = Self {
        counted: 1,
        used: 1,
    };

    fn one(&self, tokens: usize) -> u64 {
        if self.counted == 0 {
            return 0;
        }
        ((tokens as u128 * self.used as u128 + self.counted as u128 / 2) / self.counted as u128)
            as u64
    }

    /// Largest remainder rounding: the parts add up to `used` exactly.
    fn distribute(&self, counts: impl Iterator<Item = usize>) -> Vec<u64> {
        let counts = counts.collect::<Vec<_>>();
        if self.counted == 0 {
            return vec![0; counts.len()];
        }
        let exact = counts
            .iter()
            .map(|tokens| *tokens as u128 * self.used as u128)
            .collect::<Vec<_>>();
        let mut parts = exact
            .iter()
            .map(|value| (value / self.counted as u128) as u64)
            .collect::<Vec<_>>();
        let mut remaining = self.used.saturating_sub(parts.iter().sum());
        let mut order = (0..counts.len()).collect::<Vec<_>>();
        order.sort_by_key(|index| std::cmp::Reverse(exact[*index] % self.counted as u128));
        for index in order {
            if remaining == 0 {
                break;
            }
            parts[index] += 1;
            remaining -= 1;
        }
        parts
    }

    /// Items with their scaled tokens, the largest first.
    fn items(&self, items: Vec<(String, usize)>) -> Vec<ContextItem> {
        let mut items = items
            .into_iter()
            .map(|(name, tokens)| ContextItem {
                name,
                tokens: self.one(tokens),
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| {
            right
                .tokens
                .cmp(&left.tokens)
                .then_with(|| left.name.cmp(&right.name))
        });
        items
    }

    /// Tools grouped by server, the servers holding the most tokens first.
    fn servers(&self, tools: Vec<(String, String, usize)>) -> Vec<ContextToolServer> {
        let mut by_server = BTreeMap::<String, Vec<(String, usize)>>::new();
        for (server, name, tokens) in tools {
            by_server.entry(server).or_default().push((name, tokens));
        }
        let mut servers = by_server
            .into_iter()
            .map(|(server, tools)| ContextToolServer {
                server,
                tools: self.items(tools),
            })
            .collect::<Vec<_>>();
        servers.sort_by(|left, right| {
            right
                .tokens()
                .cmp(&left.tokens())
                .then_with(|| left.server.cmp(&right.server))
        });
        servers
    }
}

#[cfg(test)]
#[path = "context_breakdown_tests.rs"]
mod tests;
