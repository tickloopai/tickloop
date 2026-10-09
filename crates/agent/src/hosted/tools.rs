//! The Tickhost backend of the native Tick tools: the hidden Ticks a Chat opened. Opening,
//! reopening and closing go through the daemon, which alone controls the Tickhost; reading and
//! calling use the instance's data binding directly. Each call is prepared once — target,
//! catalog, executable, working directory and operation id frozen — and the same preparation is
//! approved and dispatched.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_protocol::{InstanceRef, TickDescriptor};
use tickhost::host::Receipt;
use tickhost_connect::{
    host_error::HostError,
    hosted::{CALL_RESPONSE_TIMEOUT, CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, Role},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    controller::HostedViewGrant,
    registry::{FileIdentity, TARGET_GONE},
    worker_control::{DaemonControl, HostedAdmission, HostedTickPage, WorkerRequest},
};
use crate::{
    host_tools::{
        HostCallContext, HostCallResult, HostEffect, HostEndpointIdentity, HostToolAuthority,
        HostToolPermission, HostToolProvider, PreparedHostCall,
    },
    tick_tool_router::{CALL_TICK_TOOL, MODIFY_TICK, READ_TICK, TickBackends},
};
use tickloop_agent_protocol::{
    ChatId, HostedResources, HostedScreenSnapshot, ToolDefinition, ToolResult,
};

/// How results name the Tickhost that runs a hidden Tick.
const HOSTED_HOST: &str = "tickhost";

#[derive(Clone)]
struct DataBinding {
    endpoint: PathBuf,
    data_token: String,
}

/// One hosted Tick as its Chat's Monitor shows it.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct HostedMember {
    pub instance: InstanceRef,
    pub title: Option<String>,
    pub state: HostedMemberState,
    pub resources: Option<HostedResources>,
}

/// A hosted Tick's screen as text when it was taken: the newest lines within the snapshot
/// limits, saying whether older ones were cut.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct HostedSnapshot {
    pub screen: HostedScreenSnapshot,
    pub taken_at_ms: u64,
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

fn hosted_screen_snapshot(screen: tickhost::screen::ScreenSnapshot) -> HostedScreenSnapshot {
    let tickhost::screen::ScreenSnapshot {
        lines,
        truncated,
        rows,
        columns,
        alternate_screen,
    } = screen;
    HostedScreenSnapshot {
        lines,
        truncated,
        rows,
        columns,
        alternate_screen,
    }
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostedMemberState {
    Running,
    /// Its host died with it; only an explicit reopen starts a new instance in its place.
    Lost,
}

enum Operation {
    Open {
        operation: Uuid,
        command: Vec<String>,
        cwd: PathBuf,
        executable: FileIdentity,
        directory: FileIdentity,
        title: String,
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
    Read {
        instance: InstanceRef,
    },
    Call {
        instance: InstanceRef,
        tool: String,
        args: Value,
        catalog: TickDescriptor,
    },
}

struct HostedPrepared {
    operation: Operation,
}

pub struct HostedTickTools {
    daemon: DaemonControl,
    environment: Vec<(String, String)>,
    endpoint: HostEndpointIdentity,
    /// Data bindings of the Ticks each Chat owns; another Chat of this worker never shares one.
    /// A binding goes when its Tick is closed, replaced, or missing from a complete listing.
    bindings: Mutex<HashMap<(ChatId, InstanceRef), DataBinding>>,
}

impl HostedTickTools {
    /// `environment` is the launch environment hosted Ticks start with; the host removes
    /// inherited Herdr and Tick launch context from it.
    pub fn new(daemon: DaemonControl, environment: Vec<(String, String)>) -> Self {
        Self {
            endpoint: daemon.host_endpoint(),
            daemon,
            environment,
            bindings: Mutex::new(HashMap::new()),
        }
    }

    async fn prepare(
        &self,
        chat: ChatId,
        name: &str,
        input: &Value,
        workdir: Option<&Path>,
    ) -> Result<PreparedHostCall, String> {
        TickBackends::HOSTED.route(name, input)?;
        let (operation, permission, summary) = match (name, input["action"].as_str()) {
            (MODIFY_TICK, Some("open")) => {
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
                    None => workdir
                        .map(Path::to_path_buf)
                        .ok_or("cwd is required without an Agent work directory")?,
                }
                .canonicalize()
                .map_err(|error| format!("cannot resolve cwd: {error}"))?;
                let executable = tickhost_connect::access::resolve_executable(&words[0], &cwd)
                    .map_err(|error| error.to_string())?;
                let mut command = words.clone();
                command[0] = executable.to_string_lossy().into_owned();
                let command_line = words.join(" ");
                (
                    Operation::Open {
                        operation: Uuid::new_v4(),
                        executable: FileIdentity::executable(&executable)?,
                        directory: FileIdentity::directory(&cwd)?,
                        command,
                        cwd: cwd.clone(),
                        title: tickhost::host::display_title(&command_line),
                    },
                    HostToolPermission::Reviewable,
                    format!(
                        "Open `{command_line}` hidden in the Tickhost (cwd {})",
                        cwd.display()
                    ),
                )
            }
            (MODIFY_TICK, Some("close")) => {
                let instance = hosted_target(input)?;
                (
                    Operation::Close {
                        operation: Uuid::new_v4(),
                        instance: instance.clone(),
                    },
                    HostToolPermission::Reviewable,
                    format!("Close hosted Tick {instance}"),
                )
            }
            (MODIFY_TICK, Some("reopen")) => {
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
                        operation: Uuid::new_v4(),
                        lost,
                        title,
                    },
                    HostToolPermission::Reviewable,
                    summary,
                )
            }
            (MODIFY_TICK, _) => {
                return Err(
                    "UnsupportedCapability: hosted Ticks support open, close and reopen".into(),
                );
            }
            (READ_TICK, _) => {
                let instance = hosted_target(input)?;
                (
                    Operation::Read {
                        instance: instance.clone(),
                    },
                    HostToolPermission::ReadOnly,
                    format!("Read hosted Tick {instance}"),
                )
            }
            (CALL_TICK_TOOL, _) => {
                let instance = hosted_target(input)?;
                let tool = input["tool"].as_str().ok_or("tool is required")?.to_owned();
                let args = input["args"].clone();
                let catalog = self.describe(chat, &instance).await?;
                let declaration = catalog
                    .tools
                    .iter()
                    .find(|declared| declared.name == tool)
                    .ok_or_else(|| format!("{instance} declares no tool {tool:?}"))?;
                jsonschema::validator_for(&declaration.params)
                    .map_err(|error| error.to_string())?
                    .validate(&args)
                    .map_err(|error| format!("invalid args for {tool}: {error}"))?;
                let permission = if declaration.mutates {
                    HostToolPermission::Reviewable
                } else {
                    HostToolPermission::ReadOnly
                };
                (
                    Operation::Call {
                        instance: instance.clone(),
                        tool: tool.clone(),
                        args,
                        catalog,
                    },
                    permission,
                    format!("Call {tool} on hosted Tick {instance}"),
                )
            }
            _ => return Err(format!("unknown hosted Tick tool: {name}")),
        };
        Ok(PreparedHostCall::new(
            name.to_owned(),
            input.clone(),
            input.clone(),
            permission,
            summary,
        )
        .with_payload(HostedPrepared { operation }))
    }

    /// Checks before dispatch that nothing the approval relied on changed.
    async fn revalidate(&self, chat: ChatId, prepared: &HostedPrepared) -> Result<(), String> {
        match &prepared.operation {
            Operation::Open {
                command,
                executable,
                cwd,
                directory,
                ..
            } => {
                if FileIdentity::executable(Path::new(&command[0]))? != *executable
                    || FileIdentity::directory(cwd)? != *directory
                {
                    return Err(
                        "the executable or working directory changed before dispatch; prepare and approve again"
                            .into(),
                    );
                }
                Ok(())
            }
            Operation::Close { .. } | Operation::Reopen { .. } | Operation::Read { .. } => Ok(()),
            Operation::Call {
                instance, catalog, ..
            } => {
                if self.describe(chat, instance).await? != *catalog {
                    return Err(
                        "the Tick's tool catalog changed before dispatch; read it and call again"
                            .into(),
                    );
                }
                Ok(())
            }
        }
    }

    async fn execute_operation(&self, chat: ChatId, prepared: HostedPrepared) -> HostCallResult {
        if let Err(error) = self.revalidate(chat, &prepared).await {
            return HostCallResult::NotDispatched(error);
        }
        match prepared.operation {
            Operation::Open {
                operation,
                command,
                cwd,
                title,
                ..
            } => {
                let request = WorkerRequest::OpenHostedTick {
                    chat,
                    operation,
                    command,
                    cwd,
                    environment: self.environment.clone(),
                    title: Some(title.clone()),
                    admission: self.admission(),
                };
                match self.lifecycle(chat, operation, request).await {
                    Ok(opened) => self.opened(chat, opened, Some(title), None).await,
                    Err(unresolved) => unresolved,
                }
            }
            Operation::Reopen {
                operation,
                lost,
                title,
            } => {
                let request = WorkerRequest::ReopenHostedTick {
                    chat,
                    operation,
                    instance: lost.clone(),
                    environment: self.environment.clone(),
                    admission: self.admission(),
                };
                match self.lifecycle(chat, operation, request).await {
                    Ok(opened) => self.opened(chat, opened, title, Some(&lost)).await,
                    Err(unresolved) => unresolved,
                }
            }
            Operation::Close {
                operation,
                instance,
            } => {
                let request = WorkerRequest::CloseHostedTick {
                    chat,
                    operation,
                    instance: instance.clone(),
                    admission: self.admission(),
                };
                match self.lifecycle(chat, operation, request).await {
                    Ok(_) => {
                        self.bindings.lock().await.remove(&(chat, instance.clone()));
                        completed(
                            json!({"status":"closed","host":HOSTED_HOST,"tick_instance_id":instance}),
                        )
                    }
                    Err(unresolved) => unresolved,
                }
            }
            Operation::Read { instance } => match self.read(chat, &instance).await {
                Ok(read) => completed(read),
                Err(error) => HostCallResult::NotDispatched(error),
            },
            Operation::Call {
                instance,
                tool,
                args,
                ..
            } => {
                let mut connection = match self.data(chat, &instance).await {
                    Ok(connection) => connection,
                    Err(error) => return HostCallResult::NotDispatched(error.to_string()),
                };
                match connection
                    .request(
                        "call",
                        json!({"tool":tool,"args":args}),
                        CALL_RESPONSE_TIMEOUT,
                    )
                    .await
                {
                    Ok(result) => completed(
                        json!({"status":"completed","tick_instance_id":instance,"result":result}),
                    ),
                    Err(error) if error.dispatched => {
                        HostCallResult::OutcomeUnknown(error.to_string())
                    }
                    Err(error) => HostCallResult::NotDispatched(error.to_string()),
                }
            }
        }
    }

    /// The authority every hosted Tick call runs under: this worker's own Tickhost.
    pub(crate) fn authority(&self) -> HostToolAuthority {
        HostToolAuthority::Native {
            endpoint: self.endpoint.clone(),
        }
    }

    fn admission(&self) -> HostedAdmission {
        HostedAdmission {
            tool: MODIFY_TICK.into(),
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
    ) -> Result<Value, HostCallResult> {
        let lost = match self.daemon.request(request).await {
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
        let receipt = self
            .daemon
            .request(WorkerRequest::GetHostedOperationReceipt { chat, operation })
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

    /// The verified facts of an open, or of a reopen in place of `replaces`.
    async fn opened(
        &self,
        chat: ChatId,
        opened: Value,
        title: Option<String>,
        replaces: Option<&InstanceRef>,
    ) -> HostCallResult {
        let instance: InstanceRef = match serde_json::from_value(opened["instance"].clone()) {
            Ok(instance) => instance,
            Err(error) => {
                return HostCallResult::OutcomeUnknown(format!(
                    "the daemon reported an opened Tick without a valid instance ({error})"
                ));
            }
        };
        {
            let mut bindings = self.bindings.lock().await;
            if let Some(lost) = replaces {
                bindings.remove(&(chat, lost.clone()));
            }
            if let (Some(endpoint), Some(token)) = (
                opened["terminal"]["endpoint"].as_str(),
                opened["data_token"].as_str(),
            ) {
                bindings.insert(
                    (chat, instance.clone()),
                    DataBinding {
                        endpoint: endpoint.into(),
                        data_token: token.into(),
                    },
                );
            }
        }
        let mut result = json!({"status":"opened","host":HOSTED_HOST,"visibility":"hidden","tick_instance_id":instance});
        if opened["business_pid"].is_u64() {
            result["business_pid"] = opened["business_pid"].clone();
        }
        if let Some(lost) = replaces {
            result["replaces"] = json!(lost);
        }
        HostCallResult::CompletedWithEffect(
            ToolResult {
                content: result.to_string(),
                is_error: false,
                output_ref: None,
            },
            Box::new(HostEffect::OpenedTick { instance, title }),
        )
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

    async fn read(&self, chat: ChatId, instance: &InstanceRef) -> Result<Value, String> {
        let mut connection = self
            .data(chat, instance)
            .await
            .map_err(|error| error.to_string())?;
        let read = connection
            .request("read", json!({"activity":"explicit"}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        let descriptor = connection
            .request("describe", json!({}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "tick_instance_id": instance,
            "source": "protocol",
            "initialized": read["initialized"],
            "sequence": read["sequence"],
            "observed_at": read["observed_at"],
            "content": read["data"],
            "descriptor": descriptor,
            "tools": descriptor["tools"],
        }))
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
                };
                let resources = match binding.connect(&instance).await {
                    Ok(mut connection) => connection
                        .request("resources", json!({}), CONTROL_TIMEOUT)
                        .await
                        .ok()
                        .and_then(|resources| serde_json::from_value(resources).ok())
                        .map(hosted_resources),
                    Err(_) => None,
                };
                self.bindings
                    .lock()
                    .await
                    .insert((chat, instance.clone()), binding);
                members.push(HostedMember {
                    instance,
                    title: row.title,
                    state: HostedMemberState::Running,
                    resources,
                });
            }
            members.extend(page.lost.into_iter().map(|lost| HostedMember {
                instance: lost.instance,
                title: lost.title,
                state: HostedMemberState::Lost,
                resources: None,
            }));
            match page.page.next {
                Some(next) => cursor = Some(next),
                None => {
                    let running = members
                        .iter()
                        .filter(|member| member.state == HostedMemberState::Running)
                        .map(|member| member.instance.clone())
                        .collect();
                    self.forget_unlisted(chat, &running).await;
                    return Ok(members);
                }
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
            .request(WorkerRequest::ListHostedTicks { chat, cursor })
            .await?;
        serde_json::from_value(page)
            .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))
    }

    /// Starts a new instance in place of a lost one because its user asked to, through the same
    /// prepared reopen a model's approved ModifyTick runs. The daemon checks the recorded launch
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
                MODIFY_TICK,
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
            HostCallResult::Completed(result) => {
                Err(HostError::after("OutcomeUnknown", result.content))
            }
        }
    }

    /// Closes one of the Chat's hosted Ticks because its user asked to, through the same
    /// prepared close a model's approved ModifyTick runs.
    pub async fn close_for_user(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<(), HostError> {
        let cancel = CancellationToken::new();
        let prepared = self
            .prepare_call(
                chat,
                MODIFY_TICK,
                &json!({"action":"close","tick_instance_id":instance}),
                cancel.clone(),
            )
            .await
            .map_err(|error| HostError::before("Invalid", error))?;
        match self.execute_prepared(chat, prepared, cancel).await {
            HostCallResult::Completed(result) | HostCallResult::CompletedWithEffect(result, _)
                if !result.is_error =>
            {
                Ok(())
            }
            HostCallResult::Completed(result) | HostCallResult::CompletedWithEffect(result, _) => {
                Err(HostError::after("ToolError", result.content))
            }
            HostCallResult::NotDispatched(message) => Err(HostError::before("Refused", message)),
            HostCallResult::OutcomeUnknown(message) => {
                Err(HostError::after("OutcomeUnknown", message))
            }
        }
    }

    /// How often Watch intents must be reported again to keep protecting their Ticks.
    pub(crate) fn intent_renewal(&self) -> std::time::Duration {
        self.daemon.intent_renewal()
    }

    /// The current screen of one of the Chat's hosted Ticks as text. Taking it reads host state
    /// only: it never counts as using the Tick, subscribes or calls the model.
    pub async fn snapshot(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<HostedSnapshot, HostError> {
        let screen = self
            .data(chat, instance)
            .await?
            .request("snapshot", json!({}), CONTROL_TIMEOUT)
            .await?;
        Ok(HostedSnapshot {
            screen: serde_json::from_value(screen)
                .map(hosted_screen_snapshot)
                .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))?,
            taken_at_ms: tickloop_agent_base::private_fs::unix_time_milliseconds(),
        })
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
                chat,
                instance: instance.clone(),
            })
            .await?;
        serde_json::from_value(granted)
            .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))
    }

    /// Tells the daemon which approved Watches of `chat` keep `instance` from idling.
    pub(crate) async fn set_watch_intents(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
        watches: BTreeSet<Uuid>,
    ) -> Result<(), HostError> {
        self.daemon
            .request(WorkerRequest::SetWatchIntents {
                chat,
                instance: instance.clone(),
                watches,
            })
            .await
            .map(drop)
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
    fn definitions(&self) -> Vec<ToolDefinition> {
        TickBackends::HOSTED.definitions()
    }

    fn host_endpoint(&self) -> Result<Option<HostEndpointIdentity>, String> {
        Ok(Some(self.endpoint.clone()))
    }

    fn permission(&self, _: ChatId, name: &str, _: &Value) -> HostToolPermission {
        if name == READ_TICK {
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
        self.prepare(chat, name, input, None).await
    }

    async fn prepare_call_in_context(
        &self,
        chat: ChatId,
        context: &HostCallContext,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        if cancel.is_cancelled() {
            return Err("hosted Tick operation was cancelled before preparation".into());
        }
        self.prepare(chat, name, input, Some(&context.workdir))
            .await
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
        let prepared = call
            .payload::<HostedPrepared>()
            .ok_or("hosted Tick operation has no prepared binding")?;
        self.revalidate(chat, prepared).await
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
        match call.into_payload::<HostedPrepared>() {
            Ok(prepared) => self.execute_operation(chat, prepared).await,
            Err(_) => HostCallResult::NotDispatched(
                "hosted Tick operation has no prepared binding".into(),
            ),
        }
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
    use std::collections::HashSet;

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
                terminal: Uuid::new_v4(),
                data_token: Some("token".into()),
                owner: TickOwner {
                    root: "fixture-root".into(),
                    chat,
                },
                title: None,
                state: TickState::Running,
                business_pid: None,
                attached: false,
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
            intent_renewal: std::time::Duration::from_secs(100),
        })
    }

    async fn bound(tools: &HostedTickTools) -> HashSet<(ChatId, InstanceRef)> {
        tools.bindings.lock().await.keys().cloned().collect()
    }

    async fn bind(tools: &HostedTickTools, chat: ChatId, instance: &InstanceRef) {
        tools.bindings.lock().await.insert(
            (chat, instance.clone()),
            DataBinding {
                endpoint: "/nonexistent/host.sock".into(),
                data_token: "token".into(),
            },
        );
    }

    /// The approval names the whole command and the launch keeps it, while the title the Chat
    /// keeps for its Monitor is bounded like the host's listing.
    #[tokio::test]
    async fn an_open_keeps_its_whole_command_under_a_bounded_title() {
        let directory = tempfile::Builder::new()
            .prefix("hto")
            .tempdir_in("/private/tmp")
            .unwrap();
        let chat = Uuid::new_v4();
        let tools = HostedTickTools::new(listing_daemon(directory.path(), chat, &[]), Vec::new());
        let argument = "x".repeat(tickhost::host::TITLE_CHARACTERS * 4);
        let prepared = tools
            .prepare(
                chat,
                MODIFY_TICK,
                &json!({"action":"open","command":["/bin/echo", argument],"cwd":directory.path()}),
                None,
            )
            .await
            .unwrap();
        let whole = format!("/bin/echo {argument}");
        assert!(prepared.approval_summary().contains(&whole));
        let Some(HostedPrepared {
            operation: Operation::Open { command, title, .. },
        }) = prepared.payload::<HostedPrepared>()
        else {
            panic!("an open prepares an open");
        };
        assert_eq!(command, &["/bin/echo".to_owned(), argument]);
        assert_eq!(title, &tickhost::host::display_title(&whole));
    }

    /// A binding goes once its Tick is no longer listed running, is looked up as gone, or was
    /// lost and reopened as a new instance; another Chat's bindings are never touched.
    #[tokio::test]
    async fn bindings_of_ticks_that_ended_or_were_replaced_are_forgotten() {
        let directory = tempfile::Builder::new()
            .prefix("htb")
            .tempdir_in("/private/tmp")
            .unwrap();
        let (chat, other) = (Uuid::new_v4(), Uuid::new_v4());
        let hosted = || InstanceRef::hosted(Uuid::new_v4());
        let (running, ended, elsewhere) = (hosted(), hosted(), hosted());
        let tools = HostedTickTools::new(
            listing_daemon(directory.path(), chat, std::slice::from_ref(&running)),
            Vec::new(),
        );

        bind(&tools, chat, &ended).await;
        bind(&tools, other, &elsewhere).await;
        tools.members(chat).await.unwrap();
        assert_eq!(
            bound(&tools).await,
            HashSet::from([(chat, running.clone()), (other, elsewhere.clone())])
        );

        bind(&tools, chat, &ended).await;
        let gone = tools.data(chat, &hosted()).await.err().unwrap();
        assert_eq!(gone.code, TARGET_GONE);
        assert_eq!(
            bound(&tools).await,
            HashSet::from([(chat, running.clone()), (other, elsewhere.clone())]),
            "looking up a gone Tick forgets every unlisted one of the Chat"
        );

        let (lost, reopened) = (hosted(), hosted());
        bind(&tools, chat, &lost).await;
        let opened = json!({"instance":reopened,"terminal":{"endpoint":"/nonexistent/host.sock"},"data_token":"token"});
        tools.opened(chat, opened, None, Some(&lost)).await;
        assert_eq!(
            bound(&tools).await,
            HashSet::from([(chat, running), (chat, reopened), (other, elsewhere)])
        );
    }
}
