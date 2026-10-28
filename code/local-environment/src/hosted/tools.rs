//! The native Tick tools: the Ticks a Chat opened in the Tickhost, extension Ticks and terminal
//! Ticks, and, for a worker in a Herdr View, the Herdr Panes that show them. Opening, reopening
//! and closing go through the daemon, which alone controls the Tickhost; reading, calling an
//! extension Tick and typing into a terminal Tick use the instance's data binding directly. Each
//! call is prepared once — target, catalog revision, executable, working directory, placement,
//! input and operation id frozen — and the same preparation is approved and dispatched.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_plugins::PluginStore;
use tick_protocol::{InstanceRef, TickDescriptor};
use tickhost::host::{Receipt, TerminalRead, TickAdapter, TickRow};
use tickhost_connect::{
    herdr::Pane,
    host_error::{HostError, TOOL_FAILED},
    hosted::{CALL_RESPONSE_TIMEOUT, CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, Role},
    layout::{LayoutContext, Placement},
    page::{MAX_PAGE_SIZE, PageCursor},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    registry::{FileIdentity, PreparedProgram, TARGET_GONE},
    views::{HerdrViews, WorkspaceListing, interaction_pane, not_created},
    worker_control::{DaemonControl, HostedAdmission, HostedTickPage, WorkerRequest},
};
use crate::{
    host_tools::{
        HostCallResult, HostCallScope, HostEffect, HostToolPermission, HostToolProvider,
        PreparedHostCall, WorkspaceHostContext,
    },
    tick_tools::{
        HIDDEN_PLACEMENT, READ_WORKSPACE, TICK_CALL, TICK_DESCRIBE, TICK_MANAGE, TICK_READ,
    },
};
use tickloop_harness::protocol::{
    ChatId, HostCallContext, HostEndpointIdentity, HostToolAuthority, HostedMember,
    HostedMemberState, HostedProgramExit, HostedResources, HostedViewGrant, ToolDefinition,
    ToolResult,
};

/// How results name the Tickhost that runs a Tick.
const HOSTED_HOST: &str = "tickhost";
/// What a call the Tick ran and reported failed tells the model, unlike an unknown outcome.
const TOOL_FAILED_DIAGNOSTIC: &str = "The Tick ran the call and reported this failure; its outcome is known. Correct the call before making it again.";

#[derive(Clone)]
struct DataBinding {
    endpoint: PathBuf,
    data_token: String,
    adapter: TickAdapter,
}

fn hosted_resources(resources: tickhost::host::Resources) -> HostedResources {
    let tickhost::host::Resources {
        business_pid,
        cpu_time_ms,
        rss_bytes,
        sampled_at_ms,
    } = resources;
    HostedResources {
        business_pid,
        cpu_time_ms,
        rss_bytes,
        sampled_at_ms,
    }
}

/// A Herdr view placement frozen before approval, with the Herdr it was frozen in.
struct PreparedView {
    workspace: String,
    placement: Placement,
    socket: (u64, u64),
}

enum Operation {
    Open {
        operation: Uuid,
        adapter: TickAdapter,
        command: Vec<String>,
        cwd: PathBuf,
        executable: PreparedProgram,
        directory: FileIdentity,
        title: String,
        /// Where a Herdr Pane shows the Tick once it runs.
        view: Option<PreparedView>,
    },
    /// Moves the Pane `current` that shows `instance`, or shows it in a new Pane when it has
    /// none.
    Move {
        instance: InstanceRef,
        current: Option<Pane>,
        view: PreparedView,
    },
    ReadWorkspace {
        listing: WorkspaceListing,
        socket: (u64, u64),
    },
    Close {
        operation: Uuid,
        instance: InstanceRef,
    },
    Reopen {
        operation: Uuid,
        lost: InstanceRef,
        title: Option<String>,
    },
    Describe {
        instance: InstanceRef,
    },
    Read {
        instance: InstanceRef,
    },
    /// Input typed into a terminal Tick's program.
    Input {
        instance: InstanceRef,
        data: Vec<u8>,
    },
    /// A Tick Tool call bound to the catalog revision it was prepared against: the Tick refuses
    /// it unrun once its catalog changed.
    Call {
        instance: InstanceRef,
        tool: String,
        args: Value,
        revision: String,
    },
}

struct HostedPrepared {
    operation: Operation,
}

#[derive(Clone)]
pub struct HostedTickTools {
    daemon: DaemonControl,
    environment: Vec<(String, String)>,
    endpoint: HostEndpointIdentity,
    /// Data bindings of the Ticks each Chat owns; another Chat of this worker never shares one.
    /// A binding goes when its Tick is closed, replaced, or missing from a complete listing.
    bindings: std::sync::Arc<Mutex<HashMap<(ChatId, InstanceRef), DataBinding>>>,
    /// The Herdr Panes that show Ticks, for a worker in a Herdr View.
    views: Option<std::sync::Arc<HerdrViews>>,
    /// The Tick packages installed in the worker's config root, whose programs an open finds and
    /// whose login states each model request sees.
    plugins: Option<PluginStore>,
}

impl HostedTickTools {
    pub(crate) async fn prepare_input_bytes(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
        data: &[u8],
    ) -> Result<PreparedHostCall, String> {
        if data.is_empty()
            || data.len() > tickloop_harness::runtime::environment::TERMINAL_INPUT_BYTES
        {
            return Err("terminal input is empty or exceeds the input limit".into());
        }
        if self
            .adapter(chat, instance)
            .await
            .map_err(|error| error.to_string())?
            != TickAdapter::Terminal
        {
            return Err(format!(
                "UnsupportedCapability: {instance} is an extension Tick; operate it with TickCall and the tools it declares"
            ));
        }
        let input = json!({"action":"send_input", "tick_instance_id":instance, "data":data});
        Ok(PreparedHostCall::new(
            TICK_MANAGE.into(),
            input.clone(),
            input,
            HostToolPermission::Reviewable,
            format!(
                "Type {} bytes into hosted terminal Tick {instance}",
                data.len()
            ),
        )
        .with_payload(HostedPrepared {
            operation: Operation::Input {
                instance: instance.clone(),
                data: data.to_vec(),
            },
        }))
    }

    pub(crate) fn resolved_intent(
        call: &PreparedHostCall,
        intent: tickloop_harness::runtime::environment::ExecutionIntent,
    ) -> tickloop_harness::runtime::environment::ExecutionIntent {
        use tickloop_harness::runtime::environment::{ExecutionIntent, OpenSpec, ViewPlacement};
        match call
            .payload::<HostedPrepared>()
            .map(|prepared| &prepared.operation)
        {
            Some(Operation::Open {
                command,
                cwd,
                adapter,
                view,
                ..
            }) => ExecutionIntent::Open(OpenSpec {
                command: command.clone(),
                cwd: Some(cwd.to_string_lossy().into_owned()),
                protocol: *adapter == TickAdapter::Protocol,
                view: view.as_ref().map(|view| ViewPlacement {
                    workspace_id: Some(view.workspace.clone()),
                    placement: serde_json::from_value(view.placement.summary())
                        .expect("prepared placement has a portable form"),
                }),
            }),
            _ => intent,
        }
    }

    pub(crate) fn prepared_operation(call: &PreparedHostCall) -> Option<Uuid> {
        match &call.payload::<HostedPrepared>()?.operation {
            Operation::Open { operation, .. }
            | Operation::Close { operation, .. }
            | Operation::Reopen { operation, .. } => Some(*operation),
            _ => None,
        }
    }

    pub(crate) async fn revalidate_native(&self, call: &PreparedHostCall) -> Result<(), String> {
        self.revalidate(
            call.payload::<HostedPrepared>()
                .ok_or("hosted Tick operation has no prepared binding")?,
        )
        .await
    }

    pub(crate) async fn execute_native(
        &self,
        chat: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
        dispatch: Option<crate::environment::ExecutionDispatch>,
    ) -> HostCallResult {
        if cancel.is_cancelled() {
            return HostCallResult::NotDispatched(
                "hosted Tick operation was cancelled before dispatch".into(),
            );
        }
        match call.into_payload::<HostedPrepared>() {
            Ok(prepared) => {
                self.execute_operation(chat, prepared, cancel, dispatch.as_ref())
                    .await
            }
            Err(_) => HostCallResult::NotDispatched(
                "hosted Tick operation has no prepared binding".into(),
            ),
        }
    }

    pub(crate) async fn execution_receipt(
        &self,
        chat: ChatId,
        operation: Uuid,
    ) -> tickloop_harness::runtime::environment::ExecutionReceipt {
        use tickloop_harness::runtime::environment::{
            ExecutionError, ExecutionReceipt, ExecutionResult, OpenedExecution,
        };
        let Ok(response) = self
            .daemon
            .request(WorkerRequest::GetHostedOperationReceipt {
                chat: chat.into(),
                operation,
            })
            .await
        else {
            return ExecutionReceipt::Unknown;
        };
        let Ok(Some(receipt)) =
            serde_json::from_value::<Option<Receipt>>(response["receipt"].clone())
        else {
            return ExecutionReceipt::Unknown;
        };
        let result = match receipt {
            Receipt::Opened(opened) => match self
                .opened(chat, json!(opened), None, None, None, false)
                .await
            {
                Ok(opened) => opened.complete(),
                Err(_) => return ExecutionReceipt::Unknown,
            },
            Receipt::Closed { instance } => completed(
                json!({"status":"closed", "host":HOSTED_HOST, "tick_instance_id":instance}),
            ),
            Receipt::Failed(error) if !error.dispatched => {
                return ExecutionReceipt::NotDispatched(ExecutionError::new(
                    error.code,
                    error.message,
                ));
            }
            _ => return ExecutionReceipt::Unknown,
        };
        let (result, effect) = result.into_parts();
        let opened = match effect {
            Some(HostEffect::OpenedTick {
                instance,
                title,
                is_terminal,
                min_rows,
            }) => Some(OpenedExecution {
                instance,
                title,
                is_terminal,
                min_rows,
            }),
            _ => None,
        };
        ExecutionReceipt::Completed(ExecutionResult {
            result,
            opened,
            shell: None,
            handoff: None,
        })
    }

    /// `environment` is the launch environment hosted Ticks start with; the host removes
    /// inherited Herdr and Tick launch context from it.
    pub fn new(daemon: DaemonControl, environment: Vec<(String, String)>) -> Self {
        Self {
            endpoint: daemon.host_endpoint(),
            daemon,
            environment,
            bindings: std::sync::Arc::new(Mutex::new(HashMap::new())),
            views: None,
            plugins: None,
        }
    }

    /// The same tools, which also open the programs of the Tick packages installed in
    /// `packages` and report their login states.
    pub fn with_plugins(mut self, packages: PluginStore) -> Self {
        self.plugins = Some(packages);
        self
    }

    /// The same tools, which also show Ticks in Herdr Panes and list a Workspace's Panes.
    pub fn with_herdr_views(mut self, views: HerdrViews) -> Self {
        self.views = Some(std::sync::Arc::new(views));
        self
    }

    fn views(&self) -> Result<&HerdrViews, String> {
        self.views
            .as_deref()
            .ok_or_else(|| "UnsupportedCapability: this Agent does not run in Herdr".into())
    }

    /// The Herdr serving `views`' socket now, which a call that uses the layout is frozen to.
    fn herdr_socket(views: &HerdrViews) -> Result<(u64, u64), String> {
        views
            .layout()
            .socket_identity()
            .ok_or_else(|| "HostUnavailable: no Herdr serves the Herdr endpoint".into())
    }

    /// Where a Herdr operation of `context` starts. A View Pane of another Herdr than this
    /// worker's is no anchor here.
    fn layout_context(
        views: &HerdrViews,
        context: Option<&HostCallContext>,
    ) -> Result<LayoutContext, String> {
        let Some(context) = context else {
            return Ok(LayoutContext::default());
        };
        let interaction = match &context.interaction {
            Some(source) if source.socket == views.layout().context().socket => {
                Some(interaction_pane(source.pane.clone()))
            }
            Some(_) => {
                return Err("interaction View belongs to a different Herdr instance".into());
            }
            None => None,
        };
        Ok(LayoutContext {
            workspace_id: context.workspace_id.clone(),
            interaction,
        })
    }

    /// The Herdr placement of an open or move, frozen in its Workspace: `current` is the Pane a
    /// move would move.
    async fn prepare_view(
        &self,
        input: &Value,
        context: Option<&HostCallContext>,
        is_open: bool,
        current: Option<&Pane>,
    ) -> Result<PreparedView, String> {
        let views = self.views()?;
        let socket = Self::herdr_socket(views)?;
        let layout_context = Self::layout_context(views, context)?;
        let relative_to_view = input["placement"]["type"] == "current_tab"
            || (!is_open && input.get("placement").is_none());
        let workspace = views
            .layout()
            .workspace(input, &layout_context, relative_to_view)
            .await
            .map_err(|error| error.to_string())?
            .or_else(|| current.map(|pane| pane.workspace_id.clone()))
            .ok_or("ScopeRequired: specify workspace_id; no default Workspace is configured")?;
        let placement = views
            .layout()
            .prepare_placement(input, &workspace, is_open, current, &layout_context)
            .await
            .map_err(|error| error.to_string())?;
        Ok(PreparedView {
            workspace,
            placement,
            socket,
        })
    }

    pub(crate) async fn prepare(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        context: Option<&HostCallContext>,
    ) -> Result<PreparedHostCall, String> {
        tickloop_harness::runtime::tick_tools::check_call(name, input, self.views.is_some())?;
        let folder = context.map(|context| context.folder.as_path());
        let mut external_declaration = None;
        let mut normalized_input = input.clone();
        let (operation, permission, summary) = match (name, input["action"].as_str()) {
            (READ_WORKSPACE, _) => {
                let views = self.views()?;
                let socket = Self::herdr_socket(views)?;
                let layout_context = Self::layout_context(views, context)?;
                let (cursor, limit) = page_request(input)?;
                let workspace = views
                    .layout()
                    .workspace(input, &layout_context, false)
                    .await
                    .map_err(|error| error.to_string())?;
                if let Some(workspace) = &workspace {
                    normalized_input["workspace_id"] = json!(workspace);
                }
                let summary = format!(
                    "List the Herdr Panes of {} and the views of this Chat's Ticks",
                    workspace
                        .as_deref()
                        .map_or("every Workspace".to_owned(), |workspace| format!(
                            "Workspace {workspace}"
                        ))
                );
                (
                    Operation::ReadWorkspace {
                        listing: WorkspaceListing {
                            workspace,
                            interaction: layout_context.interaction,
                            cursor,
                            limit,
                        },
                        socket,
                    },
                    HostToolPermission::ReadOnly,
                    summary,
                )
            }
            (TICK_MANAGE, Some("open")) => {
                let words = input["command"]
                    .as_array()
                    .and_then(|words| {
                        words
                            .iter()
                            .map(|word| word.as_str().map(str::to_owned))
                            .collect::<Option<Vec<_>>>()
                    })
                    .filter(|words| !words.is_empty())
                    .ok_or("command must be a nonempty array of strings")?;
                let cwd = match input["cwd"].as_str() {
                    Some(cwd) => PathBuf::from(cwd),
                    None => folder
                        .map(Path::to_path_buf)
                        .ok_or("cwd is required without an Agent work directory")?,
                }
                .canonicalize()
                .map_err(|error| format!("cannot resolve cwd: {error}"))?;
                let resolved = tickhost_connect::program::resolve_executable(
                    &words[0],
                    &cwd,
                    self.plugins.as_ref(),
                )
                .map_err(|error| error.to_string())?;
                let executable = resolved.path;
                let protocol = match input.get("protocol") {
                    None => false,
                    Some(protocol) => protocol.as_bool().ok_or("protocol must be a boolean")?,
                };
                if is_same_file(
                    &executable,
                    tickhost_connect::program::bundled_tickhost_executable(),
                ) {
                    return Err(
                        "the Tickhost launches protocol programs itself; open the program directly with protocol: true"
                            .into(),
                    );
                }
                let adapter = if protocol
                    || resolved.plugin.as_ref().is_some_and(|package| {
                        package
                            .manifest
                            .tick
                            .as_ref()
                            .is_some_and(|tick| tick.protocol)
                    }) {
                    TickAdapter::Protocol
                } else {
                    TickAdapter::Terminal
                };
                let mut command = words.clone();
                command[0] = executable.to_string_lossy().into_owned();
                let command_line = words.join(" ");
                let origin = resolved
                    .plugin
                    .as_ref()
                    .map(|package| format!(" from plugin {} {}", package.id(), package.version))
                    .unwrap_or_default();
                let command_title = tickhost_connect::naming::command_title(&words);
                normalized_input["protocol"] = json!(adapter == TickAdapter::Protocol);
                let view = match input["placement"]["type"].as_str() {
                    None | Some(HIDDEN_PLACEMENT) => None,
                    Some(_) => Some(self.prepare_view(input, context, true, None).await?),
                };
                let shown = match &view {
                    Some(view) => {
                        normalized_input["workspace_id"] = json!(view.workspace);
                        normalized_input["resolved_placement"] = view.placement.summary();
                        format!(
                            " and show it in a Herdr Pane of Workspace {} at {}",
                            view.workspace,
                            view.placement.summary()
                        )
                    }
                    None => String::new(),
                };
                (
                    Operation::Open {
                        operation: tickloop_ids::generate(),
                        adapter,
                        executable: PreparedProgram::of(&executable)?,
                        directory: FileIdentity::directory(&cwd)?,
                        command,
                        cwd: cwd.clone(),
                        title: tickhost::host::display_title(&command_title),
                        view,
                    },
                    HostToolPermission::Reviewable,
                    format!(
                        "Open `{command_line}`{origin} in the Tickhost as {} (cwd {}){shown}",
                        match adapter {
                            TickAdapter::Protocol => "an extension Tick",
                            TickAdapter::Terminal => "a terminal Tick",
                        },
                        cwd.display()
                    ),
                )
            }
            (TICK_MANAGE, Some("move")) => {
                let instance = hosted_target(input)?;
                // Only a running Tick of this Chat has views to move or show.
                self.adapter(chat, &instance)
                    .await
                    .map_err(|error| error.to_string())?;
                let current = self
                    .views()?
                    .pane(&instance)
                    .await
                    .map_err(|error| error.to_string())?;
                let view = self
                    .prepare_view(input, context, false, current.as_ref())
                    .await?;
                if let Some(pane) = current
                    .as_ref()
                    .filter(|pane| pane.workspace_id != view.workspace)
                {
                    return Err(format!(
                        "UnsupportedCapability: the Herdr Pane showing {instance} is in Workspace {}; moving a view across Workspaces is not supported",
                        pane.workspace_id
                    ));
                }
                normalized_input["workspace_id"] = json!(view.workspace);
                normalized_input["resolved_placement"] = view.placement.summary();
                let summary = match &current {
                    Some(pane) => format!(
                        "Move Herdr Pane {} showing hosted Tick {instance} to {}",
                        pane.pane_id,
                        view.placement.summary()
                    ),
                    None => format!(
                        "Show hosted Tick {instance} in a new Herdr Pane of Workspace {} at {}",
                        view.workspace,
                        view.placement.summary()
                    ),
                };
                (
                    Operation::Move {
                        instance,
                        current,
                        view,
                    },
                    HostToolPermission::Reviewable,
                    summary,
                )
            }
            (TICK_MANAGE, Some("close")) => {
                let instance = hosted_target(input)?;
                (
                    Operation::Close {
                        operation: tickloop_ids::generate(),
                        instance: instance.clone(),
                    },
                    HostToolPermission::Reviewable,
                    format!("Close hosted Tick {}", instance.public()),
                )
            }
            (TICK_MANAGE, Some("reopen")) => {
                let lost = hosted_target(input)?;
                // The daemon alone judges whether the instance can be reopened; the listing only
                // names it for the approval.
                let title = self
                    .list_page(chat, None)
                    .await
                    .map_err(|error| error.to_string())?
                    .lost
                    .into_iter()
                    .find(|listed| listed.instance == lost)
                    .and_then(|listed| listed.title);
                let summary = format!(
                    "Reopen {} as a new hidden instance from its recorded program, if it was lost with its Tickhost; its earlier subscriptions do not follow",
                    title.as_deref().unwrap_or(lost.as_str())
                );
                (
                    Operation::Reopen {
                        operation: tickloop_ids::generate(),
                        lost,
                        title,
                    },
                    HostToolPermission::Reviewable,
                    summary,
                )
            }
            (TICK_MANAGE, Some("send_input")) => {
                let instance = hosted_target(input)?;
                if self
                    .adapter(chat, &instance)
                    .await
                    .map_err(|error| error.to_string())?
                    == TickAdapter::Protocol
                {
                    return Err(format!(
                        "UnsupportedCapability: {instance} is an extension Tick; operate it with TickCall and the tools it declares"
                    ));
                }
                let text = match input.get("text") {
                    None => None,
                    Some(text) => Some(text.as_str().ok_or("text must be a string")?),
                };
                let keys = match input.get("keys") {
                    None => Vec::new(),
                    Some(keys) => serde_json::from_value::<Vec<String>>(keys.clone())
                        .map_err(|_| "keys must be an array of key names")?,
                };
                let data = tickloop_harness::runtime::terminal_input::encode(text, &keys)?;
                let mut typed = Vec::new();
                if let Some(text) = text {
                    typed.push(format!("text {text:?}"));
                }
                if !keys.is_empty() {
                    typed.push(format!("keys {}", keys.join(" ")));
                }
                (
                    Operation::Input {
                        instance: instance.clone(),
                        data,
                    },
                    HostToolPermission::Reviewable,
                    format!(
                        "Type into hosted terminal Tick {instance}: {}",
                        typed.join(", then ")
                    ),
                )
            }
            (TICK_MANAGE, _) => {
                return Err(
                    "TickManage action must be open, move, send_input, close or reopen".into(),
                );
            }
            (TICK_DESCRIBE, _) => {
                let instance = hosted_target(input)?;
                (
                    Operation::Describe {
                        instance: instance.clone(),
                    },
                    HostToolPermission::ReadOnly,
                    format!("Describe hosted Tick {}", instance.public()),
                )
            }
            (TICK_READ, _) => {
                let instance = hosted_target(input)?;
                (
                    Operation::Read {
                        instance: instance.clone(),
                    },
                    HostToolPermission::ReadOnly,
                    format!("Read hosted Tick {}", instance.public()),
                )
            }
            (TICK_CALL, _) => {
                let instance = hosted_target(input)?;
                if self
                    .adapter(chat, &instance)
                    .await
                    .map_err(|error| error.to_string())?
                    == TickAdapter::Terminal
                {
                    return Err(format!(
                        "UnsupportedCapability: {instance} is a terminal Tick and declares no tools; read it with TickRead and type into it with TickManage send_input"
                    ));
                }
                let tool = input["tool"].as_str().ok_or("tool is required")?.to_owned();
                let args = input["args"].clone();
                let catalog = self.describe(chat, &instance).await?;
                let declaration = catalog
                    .tools
                    .iter()
                    .find(|declared| declared.name == tool)
                    .ok_or_else(|| format!("{instance} declares no tool {tool:?}"))?;
                jsonschema::validator_for(&declaration.input_schema)
                    .map_err(|error| error.to_string())?
                    .validate(&args)
                    .map_err(|error| format!("invalid args for {tool}: {error}"))?;
                let permission = if declaration.mutates {
                    HostToolPermission::Reviewable
                } else {
                    HostToolPermission::ReadOnly
                };
                // The approval reviewer sees the whole catalog; the call itself is bound to its
                // revision.
                external_declaration =
                    Some(serde_json::to_value(&catalog).map_err(|error| error.to_string())?);
                (
                    Operation::Call {
                        instance: instance.clone(),
                        tool: tool.clone(),
                        args,
                        revision: catalog.revision,
                    },
                    permission,
                    format!("Call {tool} on hosted Tick {}", instance.public()),
                )
            }
            _ => return Err(format!("unknown hosted Tick tool: {name}")),
        };
        let mut facts = tickloop_harness::protocol::PreparedApprovalFacts {
            normalized_input: normalized_input.clone(),
            ..Default::default()
        };
        match &operation {
            Operation::Close { instance, .. } => {
                facts.tick_instance = Some(instance.clone());
                facts.closes_tick = true;
            }
            Operation::Input { instance, .. } | Operation::Move { instance, .. } => {
                facts.tick_instance = Some(instance.clone());
            }
            Operation::Call { instance, .. } => {
                facts.tick_instance = Some(instance.clone());
                facts.external_declaration = external_declaration;
            }
            _ => {}
        }
        // Policy and delegation checks see the call as prepared: its Workspace and placement
        // resolved.
        Ok(PreparedHostCall::new(
            name.to_owned(),
            input.clone(),
            normalized_input,
            permission,
            summary,
        )
        .with_approval_facts(facts)
        .with_payload(HostedPrepared { operation }))
    }

    /// Checks before dispatch that nothing the approval relied on changed.
    async fn revalidate(&self, prepared: &HostedPrepared) -> Result<(), String> {
        match &prepared.operation {
            Operation::Open {
                command,
                executable,
                cwd,
                directory,
                view,
                ..
            } => {
                if PreparedProgram::of(Path::new(&command[0]))? != *executable
                    || FileIdentity::directory(cwd)? != *directory
                {
                    return Err(
                        "the executable or working directory changed before dispatch; prepare and approve again"
                            .into(),
                    );
                }
                match view {
                    Some(view) => self.revalidate_view(view).await,
                    None => Ok(()),
                }
            }
            Operation::Move {
                instance,
                current,
                view,
            } => {
                self.revalidate_view(view).await?;
                let now = self
                    .views()?
                    .pane(instance)
                    .await
                    .map_err(|error| error.to_string())?;
                let unchanged = match (current, &now) {
                    (None, None) => true,
                    (Some(approved), Some(now)) => {
                        approved.pane_id == now.pane_id
                            && approved.terminal_id == now.terminal_id
                            && approved.tab_id == now.tab_id
                            && approved.workspace_id == now.workspace_id
                    }
                    _ => false,
                };
                if unchanged {
                    Ok(())
                } else {
                    Err("TargetChanged: the Pane showing the Tick changed after approval; prepare and approve again".into())
                }
            }
            Operation::ReadWorkspace { socket, .. } => self.revalidate_socket(*socket),
            // A call's catalog is checked by the Tick itself against the prepared revision.
            Operation::Close { .. }
            | Operation::Reopen { .. }
            | Operation::Read { .. }
            | Operation::Describe { .. }
            | Operation::Input { .. }
            | Operation::Call { .. } => Ok(()),
        }
    }

    async fn revalidate_view(&self, view: &PreparedView) -> Result<(), String> {
        self.revalidate_socket(view.socket)?;
        self.views()?
            .layout()
            .revalidate_placement(&view.placement)
            .await
            .map_err(|error| error.to_string())
    }

    /// Fails once another Herdr, or none, serves the socket a call was prepared against.
    fn revalidate_socket(&self, prepared: (u64, u64)) -> Result<(), String> {
        if self.views()?.layout().socket_identity() != Some(prepared) {
            return Err("TargetChanged: the Herdr endpoint changed after preparation".into());
        }
        Ok(())
    }

    async fn execute_operation(
        &self,
        chat: ChatId,
        prepared: HostedPrepared,
        cancel: CancellationToken,
        dispatch: Option<&crate::environment::ExecutionDispatch>,
    ) -> HostCallResult {
        if let Err(error) = self.revalidate(&prepared).await {
            return HostCallResult::NotDispatched(error);
        }
        if let (
            Some(dispatch),
            Operation::Open {
                command,
                executable,
                ..
            },
        ) = (dispatch, &prepared.operation)
        {
            let path = command[0].clone();
            let approved = *executable;
            dispatch.revalidate_before_send(move || {
                if PreparedProgram::of(Path::new(&path)).ok() != Some(approved) {
                    return Err(tickloop_harness::runtime::environment::ExecutionError::new(
                        "TargetChanged",
                        "the executable changed before dispatch; prepare and approve again",
                    ));
                }
                Ok(())
            });
        }
        match prepared.operation {
            Operation::ReadWorkspace { listing, .. } => {
                let shown = async {
                    let rows = self.running(chat).await?;
                    self.views()
                        .map_err(|error| HostError::before("UnsupportedCapability", error))?
                        .read_workspace(listing, rows)
                        .await
                };
                match shown.await {
                    Ok(listed) => completed(listed),
                    Err(error) => HostCallResult::NotDispatched(error.to_string()),
                }
            }
            Operation::Move {
                instance,
                current,
                view,
            } => {
                let views = match self.views() {
                    Ok(views) => views,
                    Err(error) => return HostCallResult::NotDispatched(error),
                };
                let Some(pane) = &current else {
                    // Without a view the move shows the Tick, and that is all it does.
                    let shown = self
                        .show(chat, &instance, &view.workspace, &view.placement, cancel)
                        .await;
                    let result = json!({"status":"completed","host":HOSTED_HOST,"tick_instance_id":instance,"view":shown});
                    return match shown["status"].as_str() {
                        Some("shown") => completed(result),
                        Some("failed") => HostCallResult::NotDispatched(result.to_string()),
                        _ => HostCallResult::OutcomeUnknown(result.to_string()),
                    };
                };
                match views.move_view(pane, &view.placement, cancel).await {
                    Ok(moved) => completed(
                        json!({"status":"completed","host":HOSTED_HOST,"tick_instance_id":instance,"view":moved}),
                    ),
                    Err(error) if error.dispatched => {
                        HostCallResult::OutcomeUnknown(error.to_string())
                    }
                    Err(error) => HostCallResult::NotDispatched(error.to_string()),
                }
            }
            Operation::Open {
                operation,
                adapter,
                command,
                cwd,
                title,
                view,
                ..
            } => {
                let plugin_login = self.plugin_login_note(&command);
                let request = WorkerRequest::OpenHostedTick {
                    chat: chat.into(),
                    operation,
                    adapter,
                    command,
                    cwd,
                    environment: self.environment.clone(),
                    title: Some(title.clone()),
                    admission: self.admission(),
                };
                let opened = match self.lifecycle(chat, operation, request, dispatch).await {
                    Ok(opened) => opened,
                    Err(unresolved) => return unresolved,
                };
                let mut opened = match self
                    .opened(chat, opened, Some(title), None, dispatch, view.is_some())
                    .await
                {
                    Ok(opened) => opened,
                    Err(unresolved) => return unresolved,
                };
                if let Some((state, note)) = plugin_login {
                    opened.result["plugin_login"] = json!(state);
                    opened.result["note"] = json!(note);
                }
                if let Some(view) = view {
                    if let Some(dispatch) = dispatch {
                        if dispatch.check().is_err()
                            && let tickloop_harness::runtime::environment::ExecutionOutcome::Completed(result) =
                                dispatch.interruption(operation)
                        {
                            return HostCallResult::CompletedWithEffect(
                                result.result,
                                Box::new(HostEffect::OpenedTick {
                                    instance: opened.instance,
                                    title: opened.title,
                                    is_terminal: opened.is_terminal,
                                    min_rows: opened.min_rows,
                                }),
                            );
                        }
                        dispatch.begin_view();
                    }
                    let shown = self
                        .show(
                            chat,
                            &opened.instance,
                            &view.workspace,
                            &view.placement,
                            cancel,
                        )
                        .await;
                    if shown["status"] == "shown" {
                        opened.result["visibility"] = json!("visible");
                    }
                    opened.result["view"] = shown;
                }
                opened.complete()
            }
            Operation::Reopen {
                operation,
                lost,
                title,
            } => {
                let request = WorkerRequest::ReopenHostedTick {
                    chat: chat.into(),
                    operation,
                    instance: lost.clone(),
                    environment: self.environment.clone(),
                    admission: self.admission(),
                };
                match self.lifecycle(chat, operation, request, dispatch).await {
                    Ok(opened) => match self
                        .opened(chat, opened, title, Some(&lost), dispatch, false)
                        .await
                    {
                        Ok(opened) => opened.complete(),
                        Err(unresolved) => unresolved,
                    },
                    Err(unresolved) => unresolved,
                }
            }
            Operation::Close {
                operation,
                instance,
            } => {
                let request = WorkerRequest::CloseHostedTick {
                    chat: chat.into(),
                    operation,
                    instance: instance.clone(),
                    admission: self.admission(),
                };
                // The views are found while they still name the Tick, before it ends them.
                let shown = match &self.views {
                    Some(views) => views.panes(&instance).await.unwrap_or_default(),
                    None => Vec::new(),
                };
                match self.lifecycle(chat, operation, request, dispatch).await {
                    Ok(_) => {
                        self.bindings.lock().await.remove(&(chat, instance.clone()));
                        let mut closed = json!({"status":"closed","host":HOSTED_HOST,"tick_instance_id":instance});
                        if let Some(views) = &self.views
                            && let Some(view) = views.close(&shown).await
                        {
                            closed["view"] = view;
                        }
                        completed(closed)
                    }
                    Err(unresolved) => unresolved,
                }
            }
            Operation::Describe { instance } => match self.describe_instance(chat, &instance).await
            {
                Ok(descriptor) => completed(descriptor),
                Err(error) => HostCallResult::NotDispatched(error),
            },
            Operation::Read { instance } => match self.read(chat, &instance).await {
                Ok(read) => completed(read),
                Err(error) => HostCallResult::NotDispatched(error),
            },
            Operation::Input { instance, data } => {
                let mut connection = match self.data(chat, &instance).await {
                    Ok(connection) => connection,
                    Err(error) => return HostCallResult::NotDispatched(error.to_string()),
                };
                if let Some(dispatch) = dispatch
                    && let Err(error) = dispatch.begin()
                {
                    return HostCallResult::NotDispatched(error.to_string());
                }
                match connection
                    .request("input", json!({"data":data}), CONTROL_TIMEOUT)
                    .await
                {
                    Ok(written) => completed(
                        json!({"status":"completed","tick_instance_id":instance,"written":written["written"]}),
                    ),
                    Err(error) if error.dispatched => {
                        HostCallResult::OutcomeUnknown(error.to_string())
                    }
                    Err(error) => HostCallResult::NotDispatched(error.to_string()),
                }
            }
            Operation::Call {
                instance,
                tool,
                args,
                revision,
            } => {
                let mut connection = match self.data(chat, &instance).await {
                    Ok(connection) => connection,
                    Err(error) => return HostCallResult::NotDispatched(error.to_string()),
                };
                if let Some(dispatch) = dispatch
                    && let Err(error) = dispatch.begin()
                {
                    return HostCallResult::NotDispatched(error.to_string());
                }
                match connection
                    .request(
                        "call",
                        json!({"tool":tool,"args":args,"revision":revision}),
                        CALL_RESPONSE_TIMEOUT,
                    )
                    .await
                {
                    Ok(result) => completed(
                        json!({"status":"completed","tick_instance_id":instance,"result":result}),
                    ),
                    Err(error) if error.code == TOOL_FAILED => {
                        HostCallResult::Completed(ToolResult {
                            content: json!({"status":"failed","tick_instance_id":instance,"error":error.message,"diagnostic":TOOL_FAILED_DIAGNOSTIC}).to_string(),
                            is_error: true,
                            output_ref: None,
                        })
                    }
                    Err(error) if error.dispatched => {
                        HostCallResult::OutcomeUnknown(error.to_string())
                    }
                    Err(error) => HostCallResult::NotDispatched(error.to_string()),
                }
            }
        }
    }

    /// The authority every hosted Tick call runs under: this worker's own Tickhost.
    pub fn authority(&self) -> HostToolAuthority {
        HostToolAuthority::Native {
            endpoint: self.endpoint.clone(),
        }
    }

    fn admission(&self) -> HostedAdmission {
        HostedAdmission {
            tool: TICK_MANAGE.into(),
            authority: self.authority(),
        }
    }

    /// Sends one lifecycle request. When its answer is lost after dispatch, the operation's
    /// receipt is the only fact consulted; the request itself is never repeated.
    async fn lifecycle(
        &self,
        chat: ChatId,
        operation: Uuid,
        request: WorkerRequest,
        dispatch: Option<&crate::environment::ExecutionDispatch>,
    ) -> Result<Value, HostCallResult> {
        let lost = match self.daemon.request_guarded(request, dispatch).await {
            Ok(answer) => return Ok(answer),
            Err(error) if !error.dispatched => {
                return Err(HostCallResult::NotDispatched(error.to_string()));
            }
            Err(error) => error,
        };
        let unknown = |reason: String| {
            HostCallResult::OutcomeUnknown(format!(
                "{reason}; the result of operation {operation} is unknown — do not repeat it"
            ))
        };
        if let Some(dispatch) = dispatch
            && let Err(error) = dispatch.check()
        {
            return Err(unknown(format!("{lost}; {error}")));
        }
        let receipt = self
            .daemon
            .request(WorkerRequest::GetHostedOperationReceipt {
                chat: chat.into(),
                operation,
            })
            .await
            .map_err(|error| unknown(format!("{lost}; its receipt is unavailable: {error}")))?;
        match serde_json::from_value::<Option<Receipt>>(receipt["receipt"].clone()) {
            Ok(Some(Receipt::Opened(opened))) => Ok(json!(opened)),
            Ok(Some(closed @ Receipt::Closed { .. })) => Ok(json!(closed)),
            Ok(Some(Receipt::Failed(failure))) if !failure.dispatched => {
                Err(HostCallResult::NotDispatched(failure.to_string()))
            }
            Ok(Some(Receipt::Failed(failure))) => Err(unknown(failure.to_string())),
            Ok(Some(Receipt::Dispatched | Receipt::OwnerEnded { .. }) | None) => {
                Err(unknown(lost.to_string()))
            }
            Err(error) => Err(unknown(format!(
                "{lost}; its receipt is unreadable: {error}"
            ))),
        }
    }

    /// What an open of a plugin's Tick program says of a login the plugin lacks. The system prompt
    /// leaves out plugin logins, which change without the model, so the open that needs one tells.
    fn plugin_login_note(&self, command: &[String]) -> Option<(&'static str, String)> {
        let packages = self.plugins.as_ref()?;
        let plugin =
            tickhost_connect::program::installed_plugin_of(Path::new(command.first()?), packages)?;
        let state = match packages.login_state(&plugin) {
            tick_packages::LoginState::LoggedIn | tick_packages::LoginState::NotRequired => {
                return None;
            }
            tick_packages::LoginState::LoggedOut => "logged_out",
            tick_packages::LoginState::Expired => "expired",
            tick_packages::LoginState::Unknown => "unknown",
        };
        Some((
            state,
            format!(
                "plugin {} is not logged in ({state}); if its Tick reports a refused login, ask the user to log in to it from /ticks, then open it again",
                plugin.name()
            ),
        ))
    }

    /// The verified facts of an open, or of a reopen in place of `replaces`.
    async fn opened(
        &self,
        chat: ChatId,
        opened: Value,
        title: Option<String>,
        replaces: Option<&InstanceRef>,
        dispatch: Option<&crate::environment::ExecutionDispatch>,
        view_requested: bool,
    ) -> Result<OpenedTick, HostCallResult> {
        let instance: InstanceRef = match serde_json::from_value(opened["instance"].clone()) {
            Ok(instance) => instance,
            Err(error) => {
                return Err(HostCallResult::OutcomeUnknown(format!(
                    "the daemon reported an opened Tick without a valid instance ({error})"
                )));
            }
        };
        let adapter = serde_json::from_value::<TickAdapter>(opened["adapter"].clone()).ok();
        let mut result = json!({"status":"opened","host":HOSTED_HOST,"visibility":"hidden","tick_instance_id":instance});
        if opened["business_pid"].is_u64() {
            result["business_pid"] = opened["business_pid"].clone();
        }
        if let Some(lost) = replaces {
            result["replaces"] = json!(lost);
        }
        let mut confirmed = OpenedTick {
            is_terminal: adapter == Some(TickAdapter::Terminal),
            instance: instance.clone(),
            title,
            min_rows: None,
            result,
        };
        // The lifecycle reply is final; cancellation of enrichment must not discard it.
        confirmed.confirm(dispatch, view_requested);
        let binding = match (
            opened["terminal"]["endpoint"].as_str(),
            opened["data_token"].as_str(),
            adapter,
        ) {
            (Some(endpoint), Some(token), Some(adapter)) => Some(DataBinding {
                endpoint: endpoint.into(),
                data_token: token.into(),
                adapter,
            }),
            _ => None,
        };
        {
            let mut bindings = self.bindings.lock().await;
            if let Some(lost) = replaces {
                bindings.remove(&(chat, lost.clone()));
            }
            if let Some(binding) = &binding {
                bindings.insert((chat, instance.clone()), binding.clone());
            }
        }
        // The command named the Tick until now; an extension Tick's declaration names it from here
        // on. Only the binding this open received is asked, so naming never costs a lookup of the
        // daemon.
        if let (Some(command), Some(binding)) = (&confirmed.title, &binding)
            && binding.adapter == TickAdapter::Protocol
            && let Some(described) = describe_opened(binding, &instance, command).await
        {
            confirmed.title = Some(described.title);
            confirmed.min_rows = Some(described.min_rows);
        }
        confirmed.confirm(dispatch, view_requested);
        Ok(confirmed)
    }

    /// Shows `chat`'s running `instance` in a new Herdr Pane at `placement`, with a view grant
    /// the daemon makes for that Pane alone.
    async fn show(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
        workspace: &str,
        placement: &Placement,
        cancel: CancellationToken,
    ) -> Value {
        let views = match self.views() {
            Ok(views) => views,
            Err(error) => return not_created(&HostError::before("UnsupportedCapability", error)),
        };
        match self.allow_view(chat, instance).await {
            Ok(grant) => views.show(&grant, workspace, placement, cancel).await,
            // Herdr is asked for a Pane only once the grant arrived, so a grant request whose
            // answer was lost created none.
            Err(error) => not_created(&HostError {
                dispatched: false,
                ..error
            }),
        }
    }

    async fn describe(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<TickDescriptor, String> {
        let mut connection = self
            .data(chat, instance)
            .await
            .map_err(|error| error.to_string())?;
        let value = connection
            .request("describe", json!({}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    async fn describe_instance(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<Value, String> {
        let adapter = self
            .adapter(chat, instance)
            .await
            .map_err(|error| error.to_string())?;
        let mut connection = self
            .data(chat, instance)
            .await
            .map_err(|error| error.to_string())?;
        let descriptor = connection
            .request("describe", json!({}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        Ok(
            json!({"tick_instance_id":instance,"source":if adapter == TickAdapter::Terminal {"terminal"} else {"protocol"},"descriptor":descriptor}),
        )
    }

    pub(crate) async fn read(&self, chat: ChatId, instance: &InstanceRef) -> Result<Value, String> {
        let adapter = self
            .adapter(chat, instance)
            .await
            .map_err(|error| error.to_string())?;
        let mut connection = self
            .data(chat, instance)
            .await
            .map_err(|error| error.to_string())?;
        let read = connection
            .request("read", json!({}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        match adapter {
            TickAdapter::Protocol => Ok(json!({
                "tick_instance_id": instance,
                "source": "protocol",
                "seq": read["seq"],
                "at": read["at"],
                "data": read["data"],
            })),
            TickAdapter::Terminal => {
                let read: TerminalRead =
                    serde_json::from_value(read).map_err(|error| error.to_string())?;
                Ok(json!({
                    "tick_instance_id": instance,
                    "source": "terminal",
                    "seq": read.seq,
                    "at": read.at,
                    "data": read.data(),
                    "truncated": read.truncated,
                }))
            }
        }
    }

    /// Whether `instance` is a protocol or a terminal Tick, as the binding this Chat holds for it
    /// says; a Tick keeps its adapter for its whole life.
    pub(crate) async fn adapter(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<TickAdapter, HostError> {
        let cached = self
            .bindings
            .lock()
            .await
            .get(&(chat, instance.clone()))
            .map(|binding| binding.adapter);
        match cached {
            Some(adapter) => Ok(adapter),
            None => Ok(self.recover_binding(chat, instance).await?.adapter),
        }
    }

    /// A data connection for an instance this Chat owns. A binding is recovered from the daemon,
    /// which lists only this Chat's running Ticks, after a worker restart or once a cached one
    /// stops connecting because its host was replaced.
    pub(crate) async fn data(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<HostConnection, HostError> {
        let key = (chat, instance.clone());
        let cached = self.bindings.lock().await.get(&key).cloned();
        if let Some(binding) = cached {
            if let Ok(connection) = binding.connect(instance).await {
                return Ok(connection);
            }
            self.bindings.lock().await.remove(&key);
        }
        self.recover_binding(chat, instance)
            .await?
            .connect(instance)
            .await
    }

    /// The Chat's running hosted Ticks for its Monitor, each with a resource sample when the
    /// host could take one, and those its host was lost with. Sampling reads host process facts
    /// only, never Tick content, and does not count as using the Tick.
    pub async fn members(&self, chat: ChatId) -> Result<Vec<HostedMember>, HostError> {
        let mut members = Vec::new();
        let mut cursor = None;
        loop {
            let page = self.list_page(chat, cursor).await?;
            for row in page.page.items {
                let (Some(instance), Some(data_token)) = (row.instance, row.data_token) else {
                    continue;
                };
                let binding = DataBinding {
                    endpoint: page.endpoint.clone(),
                    data_token,
                    adapter: row.adapter,
                };
                let (resources, updated_at_ms) = match binding.connect(&instance).await {
                    // An exited program has nothing left to sample.
                    Ok(_) if row.exit.is_some() => (None, None),
                    Ok(mut connection) => {
                        let resources = connection
                            .request("resources", json!({}), CONTROL_TIMEOUT)
                            .await
                            .ok()
                            .and_then(|resources| serde_json::from_value(resources).ok())
                            .map(hosted_resources);
                        // A terminal Tick updates only as its subscriptions read it.
                        let status = match binding.adapter {
                            TickAdapter::Protocol => connection
                                .request("status", json!({}), CONTROL_TIMEOUT)
                                .await
                                .ok()
                                .and_then(|status| {
                                    serde_json::from_value::<
                                        tickhost_connect::session::SessionStatus,
                                    >(status)
                                    .ok()
                                }),
                            TickAdapter::Terminal => None,
                        };
                        (resources, status.and_then(|status| status.updated_at_ms))
                    }
                    Err(_) => (None, None),
                };
                let is_terminal = binding.adapter == TickAdapter::Terminal;
                self.bindings
                    .lock()
                    .await
                    .insert((chat, instance.clone()), binding);
                let exit = row.exit.map(|exit| HostedProgramExit {
                    code: exit.code,
                    signal: exit.signal,
                });
                members.push(HostedMember {
                    is_terminal,
                    instance,
                    title: row.title,
                    state: if exit.is_some() {
                        HostedMemberState::Exited
                    } else {
                        HostedMemberState::Running
                    },
                    resources,
                    updated_at_ms,
                    exit,
                });
            }
            members.extend(page.lost.into_iter().map(|lost| HostedMember {
                is_terminal: lost.is_terminal,
                instance: lost.instance,
                title: lost.title,
                state: HostedMemberState::Lost,
                resources: None,
                updated_at_ms: None,
                exit: None,
            }));
            match page.page.next {
                Some(next) => cursor = Some(next),
                None => {
                    let listed = members
                        .iter()
                        .filter(|member| member.state != HostedMemberState::Lost)
                        .map(|member| member.instance.clone())
                        .collect();
                    self.forget_unlisted(chat, &listed).await;
                    return Ok(members);
                }
            }
        }
    }

    /// Every Tick the host runs for `chat`, from one complete listing.
    async fn running(&self, chat: ChatId) -> Result<Vec<TickRow>, HostError> {
        let mut rows = Vec::new();
        let mut cursor = None;
        loop {
            let page = self.list_page(chat, cursor).await?;
            rows.extend(page.page.items);
            match page.page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(rows),
            }
        }
    }

    /// Forgets `chat`'s bindings of every instance a complete listing did not show running.
    async fn forget_unlisted(&self, chat: ChatId, running: &HashSet<InstanceRef>) {
        self.bindings
            .lock()
            .await
            .retain(|(bound, instance), _| *bound != chat || running.contains(instance));
    }

    async fn list_page(
        &self,
        chat: ChatId,
        cursor: Option<tickhost_connect::page::PageCursor>,
    ) -> Result<HostedTickPage, HostError> {
        let page = self
            .daemon
            .request(WorkerRequest::ListHostedTicks {
                chat: chat.into(),
                cursor,
            })
            .await?;
        serde_json::from_value(page)
            .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))
    }

    /// Starts a new instance in place of a lost one because its user asked to, through the same
    /// prepared reopen a model's approved TickManage runs. The daemon checks the recorded launch
    /// again under the Chat's current authority; the lost instance and its Watches stay gone, and
    /// nothing reopens on its own. Returns the new instance.
    pub async fn reopen_for_user(
        &self,
        chat: ChatId,
        lost: &InstanceRef,
    ) -> Result<InstanceRef, HostError> {
        let cancel = CancellationToken::new();
        let prepared = self
            .prepare_call(
                chat,
                TICK_MANAGE,
                &json!({"action":"reopen","tick_instance_id":lost}),
                cancel.clone(),
            )
            .await
            .map_err(|error| HostError::before("Refused", error))?;
        match self.execute_prepared(chat, prepared, cancel).await {
            HostCallResult::CompletedWithEffect(_, effect) => match *effect {
                HostEffect::OpenedTick { instance, .. } => Ok(instance),
                _ => Err(HostError::after(
                    "OutcomeUnknown",
                    "unexpected reopen effect",
                )),
            },
            HostCallResult::NotDispatched(message) => Err(HostError::before("Refused", message)),
            HostCallResult::OutcomeUnknown(message) => {
                Err(HostError::after("OutcomeUnknown", message))
            }
            HostCallResult::Completed(result) | HostCallResult::CompletedWithImages(result, _) => {
                Err(HostError::after("OutcomeUnknown", result.content))
            }
        }
    }

    /// Closes one of the Chat's hosted Ticks because its user asked to, through the same
    /// prepared close a model's approved TickManage runs.
    pub async fn close_for_user(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<(), HostError> {
        let cancel = CancellationToken::new();
        let prepared = self
            .prepare_call(
                chat,
                TICK_MANAGE,
                &json!({"action":"close","tick_instance_id":instance}),
                cancel.clone(),
            )
            .await
            .map_err(|error| HostError::before("Invalid", error))?;
        match self.execute_prepared(chat, prepared, cancel).await {
            HostCallResult::Completed(result)
            | HostCallResult::CompletedWithImages(result, _)
            | HostCallResult::CompletedWithEffect(result, _)
                if !result.is_error =>
            {
                Ok(())
            }
            HostCallResult::Completed(result)
            | HostCallResult::CompletedWithImages(result, _)
            | HostCallResult::CompletedWithEffect(result, _) => {
                Err(HostError::after("ToolError", result.content))
            }
            HostCallResult::NotDispatched(message) => Err(HostError::before("Refused", message)),
            HostCallResult::OutcomeUnknown(message) => {
                Err(HostError::after("OutcomeUnknown", message))
            }
        }
    }

    /// Lets one View of the Chat attach to its hosted Tick once, without its data token.
    pub async fn allow_view(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<HostedViewGrant, HostError> {
        let granted = self
            .daemon
            .request(WorkerRequest::AllowHostedView {
                chat: chat.into(),
                instance: instance.clone(),
            })
            .await?;
        serde_json::from_value(granted)
            .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))
    }

    async fn recover_binding(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<DataBinding, HostError> {
        let mut running = HashSet::new();
        let mut cursor = None;
        loop {
            let page = self.list_page(chat, cursor).await?;
            for row in page.page.items {
                let Some(listed) = row.instance else {
                    continue;
                };
                if listed == *instance {
                    let binding = DataBinding {
                        endpoint: page.endpoint,
                        data_token: row.data_token.ok_or_else(|| {
                            HostError::before(TARGET_GONE, "the hosted Tick is not running")
                        })?,
                        adapter: row.adapter,
                    };
                    self.bindings
                        .lock()
                        .await
                        .insert((chat, instance.clone()), binding.clone());
                    return Ok(binding);
                }
                running.insert(listed);
            }
            match page.page.next {
                Some(next) => cursor = Some(next),
                None => {
                    self.forget_unlisted(chat, &running).await;
                    return Err(HostError::before(
                        TARGET_GONE,
                        format!("{instance} is not a running hosted Tick of this Chat"),
                    ));
                }
            }
        }
    }
}

impl DataBinding {
    async fn connect(&self, instance: &InstanceRef) -> Result<HostConnection, HostError> {
        HostConnection::connect(
            &self.endpoint,
            Role::Instance {
                instance: instance.clone(),
                data_token: self.data_token.clone(),
            },
            &[CAPABILITY_DATA],
        )
        .await
    }
}

/// A just opened Tick's name from its declaration and `command`, asked through the binding the
/// open received; `None` when the declaration cannot be read that way.
/// What a just opened extension Tick declares about itself for its Monitor row.
struct DescribedTick {
    title: String,
    min_rows: u16,
}

async fn describe_opened(
    binding: &DataBinding,
    instance: &InstanceRef,
    command: &str,
) -> Option<DescribedTick> {
    let mut connection = binding.connect(instance).await.ok()?;
    let descriptor = connection
        .request("describe", json!({}), CONTROL_TIMEOUT)
        .await
        .ok()?;
    let descriptor: TickDescriptor = serde_json::from_value(descriptor).ok()?;
    Some(DescribedTick {
        title: tickhost_connect::naming::tick_title(&descriptor.title, &descriptor.kind, command),
        min_rows: descriptor.min_rows(),
    })
}

/// A just opened Tick: its instance, the name it joins the Monitor under and what the call reports.
struct OpenedTick {
    is_terminal: bool,
    instance: InstanceRef,
    title: Option<String>,
    min_rows: Option<u16>,
    result: Value,
}

impl OpenedTick {
    fn confirm(
        &self,
        dispatch: Option<&crate::environment::ExecutionDispatch>,
        view_requested: bool,
    ) {
        if let Some(dispatch) = dispatch {
            dispatch.confirm_opened(
                tickloop_harness::runtime::environment::ExecutionResult {
                    result: ToolResult {
                        content: self.result.to_string(),
                        is_error: false,
                        output_ref: None,
                    },
                    opened: Some(tickloop_harness::runtime::environment::OpenedExecution {
                        instance: self.instance.clone(),
                        title: self.title.clone(),
                        is_terminal: self.is_terminal,
                        min_rows: self.min_rows,
                    }),
                    shell: None,
                    handoff: None,
                },
                view_requested,
            );
        }
    }

    fn complete(self) -> HostCallResult {
        HostCallResult::CompletedWithEffect(
            ToolResult {
                content: self.result.to_string(),
                is_error: false,
                output_ref: None,
            },
            Box::new(HostEffect::OpenedTick {
                min_rows: self.min_rows,
                is_terminal: self.is_terminal,
                instance: self.instance,
                title: self.title,
            }),
        )
    }
}

/// A ReadWorkspace page request; an absent limit uses the default page size.
fn page_request(input: &Value) -> Result<(Option<PageCursor>, Option<usize>), String> {
    let cursor = input
        .get("cursor")
        .map(|cursor| serde_json::from_value(cursor.clone()))
        .transpose()
        .map_err(|_| {
            "InvalidArguments: cursor must be the next_cursor object returned by the previous page"
        })?;
    let limit = match input.get("limit") {
        None => None,
        Some(limit) => Some(
            limit
                .as_u64()
                .and_then(|limit| usize::try_from(limit).ok())
                .filter(|limit| (1..=MAX_PAGE_SIZE).contains(limit))
                .ok_or_else(|| {
                    format!("InvalidArguments: limit must be between 1 and {MAX_PAGE_SIZE}")
                })?,
        ),
    };
    Ok((cursor, limit))
}

/// Whether `program` is the `bundled` executable, however either path names it.
fn is_same_file(program: &Path, bundled: Option<PathBuf>) -> bool {
    bundled
        .and_then(|bundled| bundled.canonicalize().ok())
        .is_some_and(|bundled| {
            program
                .canonicalize()
                .is_ok_and(|program| program == bundled)
        })
}

fn hosted_target(input: &Value) -> Result<InstanceRef, String> {
    let instance: InstanceRef = input["tick_instance_id"]
        .as_str()
        .ok_or("tick_instance_id is required")?
        .parse()
        .map_err(|error: &str| error.to_owned())?;
    if instance.hosted_run().is_none() {
        return Err(format!(
            "InvalidTickTarget: {instance} is not a valid hosted: instance"
        ));
    }
    Ok(instance)
}

fn completed(value: Value) -> HostCallResult {
    HostCallResult::Completed(ToolResult {
        content: value.to_string(),
        is_error: false,
        output_ref: None,
    })
}

#[async_trait]
impl HostToolProvider for HostedTickTools {
    fn catalog_generation(&self) -> Option<u64> {
        Some(0)
    }
    fn is_concurrency_safe(&self, name: &str, _input: &Value) -> bool {
        matches!(
            name,
            tickloop_harness::runtime::tick_tools::TICK_READ
                | tickloop_harness::runtime::tick_tools::TICK_DESCRIBE
                | READ_WORKSPACE
        )
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        tickloop_harness::runtime::tick_tools::definitions(self.views.is_some())
    }

    fn validate_input(&self, name: &str, schema: &Value, input: &Value) -> Result<(), String> {
        tickloop_harness::runtime::tick_tools::validate_input(
            name,
            schema,
            input,
            self.views.is_some(),
        )
    }

    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        Ok(Some(self.endpoint.clone()))
    }

    /// Ticks run under this worker's Tickhost; ReadWorkspace, and the views TickManage places or
    /// moves, under its Herdr.
    fn tool_authorities(&self, name: &str) -> Result<BTreeSet<HostToolAuthority>, String> {
        let mut authorities = BTreeSet::new();
        if name != READ_WORKSPACE {
            authorities.insert(self.authority());
        }
        if let Some(views) = &self.views
            && matches!(name, READ_WORKSPACE | TICK_MANAGE)
            && let Some(endpoint) = views.endpoint()?
        {
            authorities.insert(HostToolAuthority::Native { endpoint });
        }
        Ok(authorities)
    }

    fn call_authority(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<Option<HostToolAuthority>, String> {
        tickloop_harness::runtime::tick_tools::check_call(name, input, self.views.is_some())?;
        if tickloop_harness::runtime::tick_tools::uses_herdr_layout(name, input) {
            return Ok(self
                .views()?
                .endpoint()?
                .map(|endpoint| HostToolAuthority::Native { endpoint }));
        }
        Ok(Some(self.authority()))
    }

    fn workspace_host_context(&self) -> Option<WorkspaceHostContext> {
        let mut context = WorkspaceHostContext::from_environment().with_tickhost();
        if let Some(packages) = &self.plugins {
            context.plugins = packages
                .installed()
                .map(|installed| {
                    installed
                        .plugins
                        .into_iter()
                        .map(|package| {
                            use tickloop_harness::runtime::host_tools::{
                                WorkspacePlugin, WorkspacePluginLogin,
                            };
                            let login = match packages.login_state(&package) {
                                tick_packages::LoginState::LoggedIn => {
                                    WorkspacePluginLogin::LoggedIn
                                }
                                tick_packages::LoginState::LoggedOut => {
                                    WorkspacePluginLogin::LoggedOut
                                }
                                tick_packages::LoginState::Expired => WorkspacePluginLogin::Expired,
                                tick_packages::LoginState::NotRequired => {
                                    WorkspacePluginLogin::NotRequired
                                }
                                tick_packages::LoginState::Unknown => WorkspacePluginLogin::Unknown,
                            };
                            WorkspacePlugin {
                                id: package.id(),
                                enabled: package.enabled,
                                has_tick: package.manifest.tick.is_some(),
                                login,
                                executable: package.executable(),
                                name: package.manifest.name,
                                version: package.version,
                                description: package.manifest.description,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        Some(if self.views.is_some() {
            context.with_herdr_views()
        } else {
            context
        })
    }

    fn permission(&self, _: ChatId, name: &str, _: &Value) -> HostToolPermission {
        if matches!(name, TICK_DESCRIBE | TICK_READ | READ_WORKSPACE) {
            HostToolPermission::ReadOnly
        } else {
            HostToolPermission::Reviewable
        }
    }

    async fn prepare_call(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if cancel.is_cancelled() {
            return Err("hosted Tick operation was cancelled before preparation".into());
        }
        if matches!(name, TICK_READ | TICK_DESCRIBE | TICK_CALL)
            || name == TICK_MANAGE
                && matches!(
                    input["action"].as_str(),
                    Some("open" | "close" | "send_input" | "reopen")
                )
        {
            return crate::environment::prepare_hosted_call(
                std::sync::Arc::new(self.clone()),
                chat,
                name,
                input,
                None,
                cancel,
                self.views.is_some(),
            )
            .await;
        }
        self.prepare(chat, name, input, None).await
    }

    async fn prepare_call_in_scope(
        &self,
        chat: ChatId,
        scope: &HostCallScope<'_>,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if cancel.is_cancelled() {
            return Err("hosted Tick operation was cancelled before preparation".into());
        }
        if matches!(name, TICK_READ | TICK_DESCRIBE | TICK_CALL)
            || name == TICK_MANAGE
                && matches!(
                    input["action"].as_str(),
                    Some("open" | "close" | "send_input" | "reopen")
                )
        {
            return crate::environment::prepare_hosted_call(
                std::sync::Arc::new(self.clone()),
                chat,
                name,
                input,
                scope.context,
                cancel,
                self.views.is_some(),
            )
            .await;
        }
        self.prepare(chat, name, input, scope.context).await
    }

    async fn revalidate_call(
        &self,
        chat: ChatId,
        call: &PreparedHostCall,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("hosted Tick operation was cancelled before dispatch".into());
        }
        if tickloop_harness::runtime::environment::tools::is_environment_call(call) {
            return tickloop_harness::runtime::environment::tools::revalidate_call(chat, call)
                .await;
        }
        self.revalidate_native(call).await
    }

    async fn execute_prepared(
        &self,
        chat: ChatId,
        call: PreparedHostCall,
        cancel: CancellationToken,
    ) -> HostCallResult {
        if cancel.is_cancelled() {
            return HostCallResult::NotDispatched(
                "hosted Tick operation was cancelled before dispatch".into(),
            );
        }
        if tickloop_harness::runtime::environment::tools::is_environment_call(&call) {
            return tickloop_harness::runtime::environment::tools::execute_call(chat, call, cancel)
                .await;
        }
        self.execute_native(chat, call, cancel, None).await
    }

    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        ToolResult {
            content: "hosted Tick calls require preparation".into(),
            is_error: true,
            output_ref: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use tickhost::host::{TickOwner, TickRow, TickState};
    use tickhost_connect::{
        hosted::{BUSINESS_FRAME_BYTES, read_frame, write_frame},
        page::Page,
    };

    use super::*;
    use crate::hosted::worker_control::WorkerControlGrant;

    /// A daemon that answers every request with one complete listing of `chat`'s `running`
    /// Ticks.
    fn listing_daemon(directory: &Path, chat: ChatId, running: &[InstanceRef]) -> DaemonControl {
        let endpoint = directory.join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
        let rows = running
            .iter()
            .map(|instance| TickRow {
                instance: Some(instance.clone()),
                adapter: TickAdapter::Protocol,
                terminal: Uuid::new_v4(),
                data_token: Some("token".into()),
                owner: TickOwner {
                    root: "fixture-root".into(),
                    chat: chat.into(),
                },
                title: None,
                state: TickState::Running,
                business_pid: None,
                views: 0,
                exit: None,
            })
            .collect::<Vec<_>>();
        let page = HostedTickPage {
            page: Page {
                generation: "listing".into(),
                total: rows.len(),
                items: rows,
                next: None,
            },
            endpoint: directory.join("host.sock"),
            lost: Vec::new(),
        };
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(reader);
                if let Ok(Some(request)) = read_frame(&mut reader, BUSINESS_FRAME_BYTES).await {
                    let answer = json!({"id":request["id"],"result":page});
                    let _ = write_frame(&mut writer, &answer, BUSINESS_FRAME_BYTES).await;
                }
            }
        });
        DaemonControl::new(WorkerControlGrant {
            endpoint,
            daemon_epoch: Uuid::new_v4(),
            binding: Uuid::new_v4(),
            capability: "capability".into(),
            host_endpoint: directory.join("host.sock"),
        })
    }

    /// A daemon that takes each request and closes without an answer, as when its reply is lost.
    fn silent_daemon(directory: &Path) -> DaemonControl {
        let endpoint = directory.join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, _writer) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(reader);
                let _ = read_frame(&mut reader, BUSINESS_FRAME_BYTES).await;
            }
        });
        DaemonControl::new(WorkerControlGrant {
            endpoint,
            daemon_epoch: Uuid::new_v4(),
            binding: Uuid::new_v4(),
            capability: "capability".into(),
            host_endpoint: directory.join("host.sock"),
        })
    }

    /// A Herdr whose Workspaces show no Panes, which records every other request it refuses.
    #[derive(Default)]
    struct EmptyHerdr {
        refused: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl tickhost_connect::herdr::HerdrPort for EmptyHerdr {
        async fn request(
            &self,
            method: &str,
            _: Value,
            _: CancellationToken,
        ) -> Result<Value, HostError> {
            if method == "pane.list" {
                return Ok(json!({"panes":[]}));
            }
            self.refused.lock().unwrap().push(method.to_owned());
            Err(HostError::before("UnknownMethod", method))
        }
    }

    fn herdr_views(socket: &Path, herdr: std::sync::Arc<EmptyHerdr>) -> HerdrViews {
        HerdrViews::new(
            tickhost_connect::layout::HerdrLayout::with_host(
                tickhost_connect::herdr::HerdrContext::without_view(socket.to_path_buf()),
                herdr,
            ),
            Path::new("/private/tmp/unused/tickloop"),
        )
    }

    /// A view grant is asked for before Herdr is: when its answer is lost, no Pane can exist, so
    /// the view failed and a move that would only have shown it did not happen.
    #[tokio::test]
    async fn a_view_grant_whose_answer_was_lost_created_no_pane() {
        let directory = tempfile::Builder::new()
            .prefix("htv")
            .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
            .unwrap();
        let socket = directory.path().join("herdr.sock");
        let _herdr_listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let herdr = std::sync::Arc::new(EmptyHerdr::default());
        let tools = HostedTickTools::new(silent_daemon(directory.path()), Vec::new())
            .with_herdr_views(herdr_views(&socket, herdr.clone()));
        let instance = InstanceRef::hosted(Uuid::new_v4());
        let moved = tools
            .execute_operation(
                Uuid::new_v4().into(),
                HostedPrepared {
                    operation: Operation::Move {
                        instance,
                        current: None,
                        view: PreparedView {
                            workspace: "W1".into(),
                            placement: Placement::NewTab,
                            socket: tools.views().unwrap().layout().socket_identity().unwrap(),
                        },
                    },
                },
                CancellationToken::new(),
                None,
            )
            .await;
        let HostCallResult::NotDispatched(result) = moved else {
            panic!("a lost view grant leaves nothing to inspect");
        };
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["view"]["status"], "failed", "{result}");
        assert!(herdr.refused.lock().unwrap().is_empty());
    }

    /// A call that uses the Herdr layout is frozen to the Herdr serving its socket; with none
    /// serving it, there is nothing to freeze and no call is prepared.
    #[tokio::test]
    async fn a_herdr_call_is_not_prepared_while_no_herdr_serves_its_socket() {
        let directory = tempfile::Builder::new()
            .prefix("hte")
            .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
            .unwrap();
        let chat = Uuid::new_v4();
        let herdr = std::sync::Arc::new(EmptyHerdr::default());
        let tools = HostedTickTools::new(
            listing_daemon(directory.path(), chat.into(), &[]),
            Vec::new(),
        )
        .with_herdr_views(herdr_views(
            &directory.path().join("herdr.sock"),
            herdr.clone(),
        ));
        for (name, input) in [
            (READ_WORKSPACE, json!({"workspace_id":"W1"})),
            (
                TICK_MANAGE,
                json!({"action":"open","command":["/bin/cat"],"cwd":directory.path(),"workspace_id":"W1","placement":{"type":"new_tab"}}),
            ),
        ] {
            let refused = tools.prepare(chat.into(), name, &input, None).await.err();
            assert!(
                refused
                    .as_deref()
                    .is_some_and(|refused| refused.contains("Herdr endpoint")),
                "{name}: {refused:?}"
            );
        }
        assert!(herdr.refused.lock().unwrap().is_empty());
    }

    /// The approval names the whole command and the launch keeps it, while the title the Chat
    /// keeps for its Monitor names the program by file name, bounded like the host's listing.
    #[tokio::test]
    async fn an_open_keeps_its_whole_command_under_a_bounded_title() {
        let directory = tempfile::Builder::new()
            .prefix("hto")
            .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
            .unwrap();
        let chat = Uuid::new_v4();
        let tools = HostedTickTools::new(
            listing_daemon(directory.path(), chat.into(), &[]),
            Vec::new(),
        );
        let argument = "x".repeat(tickhost::host::TITLE_CHARACTERS * 4);
        let prepared = tools
            .prepare(
                chat.into(),
                TICK_MANAGE,
                &json!({"action":"open","command":["/bin/echo", argument],"cwd":directory.path()}),
                None,
            )
            .await
            .unwrap();
        let whole = format!("/bin/echo {argument}");
        assert!(prepared.approval_summary().contains(&whole));
        let named = format!("echo {argument}");
        let Some(HostedPrepared {
            operation: Operation::Open { command, title, .. },
        }) = prepared.payload::<HostedPrepared>()
        else {
            panic!("an open prepares an open");
        };
        // The approved command runs its canonical program, `/usr/bin/echo` where `/bin` is a link.
        let echo = std::fs::canonicalize("/bin/echo").unwrap();
        assert_eq!(command, &[echo.to_str().unwrap().to_owned(), argument]);
        assert_eq!(title, &tickhost::host::display_title(&named));
    }

    #[test]
    fn a_bundled_executable_is_recognised_through_any_path_that_names_it() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let bundled = directory.path().join("tickhost");
        std::fs::write(&bundled, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bundled, std::fs::Permissions::from_mode(0o755)).unwrap();
        let linked = directory.path().join("linked-tickhost");
        std::os::unix::fs::symlink(&bundled, &linked).unwrap();
        let other = directory.path().join("other");
        std::fs::write(&other, "#!/bin/sh\n").unwrap();
        assert!(is_same_file(&linked, Some(bundled.clone())));
        assert!(is_same_file(
            &directory.path().join("./tickhost"),
            Some(linked.clone())
        ));
        assert!(!is_same_file(&other, Some(bundled.clone())));
        assert!(!is_same_file(&bundled, None));
    }
}
