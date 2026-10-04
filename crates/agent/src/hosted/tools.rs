//! The tools of a worker without a Herdr View: hosted Ticks only. Opening and closing go through
//! the owner, which alone controls the Tick host; reading and calling use the instance's data
//! binding directly. Each call is prepared once — target, catalog, executable, working
//! directory and operation id frozen — and the same preparation is approved and dispatched.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_connect::{
    hosted::{CALL_RESPONSE_TIMEOUT, CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, Role},
    instance::InstanceRef,
    protocol::TickDescriptor,
};
use tick_host::host::Receipt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::worker_control::{HostedAdmission, OwnerControl, WorkerRequest};
use crate::{
    host_tools::{
        HostCallContext, HostCallResult, HostEffect, HostEndpointIdentity, HostToolAuthority,
        HostToolPermission, HostToolProvider, PreparedHostCall,
    },
    model::ToolDefinition,
    runtime::ChatId,
    tools::ToolResult,
};

/// The open placement that runs a Tick hidden in the Tick host.
pub const HIDDEN_PLACEMENT: &str = "hidden";
pub const MODIFY_TICK: &str = "ModifyTick";
const READ_TICK: &str = "ReadTick";
const CALL_TICK_TOOL: &str = "CallTickTool";

#[derive(Clone)]
struct DataBinding {
    endpoint: PathBuf,
    data_token: String,
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

/// Which file a path names. Executables also compare their modification time; a directory's
/// changes with every entry, so a working directory compares device and inode only.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    modified: Option<i64>,
}

impl FileIdentity {
    fn executable(path: &Path) -> Result<Self, String> {
        Self::read(path, true)
    }

    fn directory(path: &Path) -> Result<Self, String> {
        Self::read(path, false)
    }

    fn read(path: &Path, with_modification: bool) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            modified: with_modification.then(|| metadata.mtime()),
        })
    }
}

pub struct HostedTickTools {
    owner: OwnerControl,
    environment: Vec<(String, String)>,
    endpoint: HostEndpointIdentity,
    /// Data bindings of the Ticks each Chat owns; another Chat of this worker never shares one.
    bindings: Mutex<HashMap<(ChatId, InstanceRef), DataBinding>>,
}

impl HostedTickTools {
    /// `environment` is the launch environment hosted Ticks start with; the host removes
    /// inherited Herdr and Tick launch context from it.
    pub fn new(owner: OwnerControl, environment: Vec<(String, String)>) -> Self {
        Self {
            endpoint: owner.host_endpoint(),
            owner,
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
                let executable = tick_connect::access::resolve_executable(&words[0], &cwd)
                    .map_err(|error| error.to_string())?;
                let mut command = words.clone();
                command[0] = executable.to_string_lossy().into_owned();
                let title = words.join(" ");
                (
                    Operation::Open {
                        operation: Uuid::new_v4(),
                        executable: FileIdentity::executable(&executable)?,
                        directory: FileIdentity::directory(&cwd)?,
                        command,
                        cwd: cwd.clone(),
                        title: title.clone(),
                    },
                    HostToolPermission::Reviewable,
                    format!(
                        "Open `{title}` hidden in the Tick host (cwd {})",
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
            (MODIFY_TICK, _) => {
                return Err(
                    "UnsupportedCapability: hosted Ticks support open (placement hidden) and close"
                        .into(),
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
            Operation::Close { .. } | Operation::Read { .. } => Ok(()),
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
                    Ok(opened) => self.opened(chat, opened, title).await,
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
                        completed(json!({"status":"closed","tick_instance_id":instance}))
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
                    Err(error) => return HostCallResult::NotDispatched(error),
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

    fn admission(&self) -> HostedAdmission {
        HostedAdmission {
            tool: MODIFY_TICK.into(),
            authority: HostToolAuthority::Native {
                endpoint: self.endpoint.clone(),
            },
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
        let lost = match self.owner.request(request).await {
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
            .owner
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
            Ok(Some(Receipt::Dispatched) | None) => Err(unknown(lost.to_string())),
            Err(error) => Err(unknown(format!(
                "{lost}; its receipt is unreadable: {error}"
            ))),
        }
    }

    async fn opened(&self, chat: ChatId, opened: Value, title: String) -> HostCallResult {
        let instance: InstanceRef = match serde_json::from_value(opened["instance"].clone()) {
            Ok(instance) => instance,
            Err(error) => {
                return HostCallResult::OutcomeUnknown(format!(
                    "the owner reported an opened Tick without a valid instance ({error})"
                ));
            }
        };
        if let (Some(endpoint), Some(token)) = (
            opened["terminal"]["endpoint"].as_str(),
            opened["data_token"].as_str(),
        ) {
            self.bindings.lock().await.insert(
                (chat, instance.clone()),
                DataBinding {
                    endpoint: endpoint.into(),
                    data_token: token.into(),
                },
            );
        }
        HostCallResult::CompletedWithEffect(
            ToolResult {
                content: json!({"status":"opened","host":"tick-host","tick_instance_id":instance,"business_pid":opened["business_pid"]}).to_string(),
                is_error: false,
                output_ref: None,
            },
            Box::new(HostEffect::OpenedTick {
                instance,
                title: Some(title),
            }),
        )
    }

    async fn describe(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<TickDescriptor, String> {
        let mut connection = self.data(chat, instance).await?;
        let value = connection
            .request("describe", json!({}), CONTROL_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    async fn read(&self, chat: ChatId, instance: &InstanceRef) -> Result<Value, String> {
        let mut connection = self.data(chat, instance).await?;
        let read = connection
            .request("read", json!({}), CONTROL_TIMEOUT)
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

    /// A data connection for an instance this Chat owns, recovering its binding from the
    /// owner after a worker restart.
    async fn data(&self, chat: ChatId, instance: &InstanceRef) -> Result<HostConnection, String> {
        let cached = self
            .bindings
            .lock()
            .await
            .get(&(chat, instance.clone()))
            .cloned();
        let binding = match cached {
            Some(binding) => binding,
            None => self.recover_binding(chat, instance).await?,
        };
        HostConnection::connect(
            &binding.endpoint,
            Role::Instance {
                instance: instance.clone(),
                data_token: binding.data_token,
            },
            &[CAPABILITY_DATA],
        )
        .await
        .map_err(|error| error.to_string())
    }

    async fn recover_binding(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<DataBinding, String> {
        let mut cursor = None;
        loop {
            let page = self
                .owner
                .request(WorkerRequest::ListHostedTicks { chat, cursor })
                .await
                .map_err(|error| error.to_string())?;
            let endpoint = page["endpoint"].as_str().map(PathBuf::from);
            if let Some(row) = page["items"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|row| row["instance"] == json!(instance))
            {
                let binding = DataBinding {
                    endpoint: endpoint.ok_or("the owner listed no host endpoint")?,
                    data_token: row["data_token"]
                        .as_str()
                        .ok_or("the hosted Tick is not running")?
                        .into(),
                };
                self.bindings
                    .lock()
                    .await
                    .insert((chat, instance.clone()), binding.clone());
                return Ok(binding);
            }
            match serde_json::from_value(page["next"].clone()).ok().flatten() {
                Some(next) => cursor = Some(next),
                None => {
                    return Err(format!(
                        "TargetGone: {instance} is not a running hosted Tick of this Chat"
                    ));
                }
            }
        }
    }
}

fn hosted_target(input: &Value) -> Result<InstanceRef, String> {
    let instance: InstanceRef = input["tick_instance_id"]
        .as_str()
        .ok_or("tick_instance_id is required")?
        .parse()
        .map_err(|error: &str| error.to_owned())?;
    if instance.hosted_run().is_none() {
        return Err("InvalidTickTarget: expected a hosted: instance".into());
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

fn hosted_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: READ_TICK.into(),
            description: "Read a hosted Tick using its complete hosted: instance identity. Returns its structured content and declared tool catalog. For artifacts use the installed Artifact MCP tools; for Shell tasks use Read on Bash's output_file.".into(),
            input_schema: json!({"type":"object","properties":{"tick_instance_id":{"type":"string","minLength":1}},"required":["tick_instance_id"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: CALL_TICK_TOOL.into(),
            description: "Call one tool declared by a hosted Tick using its complete hosted: instance identity. Read that Tick's catalog first; query tools read content, while modifying tools require permission.".into(),
            input_schema: json!({"type":"object","properties":{"tick_instance_id":{"type":"string","minLength":1},"tool":{"type":"string","minLength":1},"args":{"type":"object"}},"required":["tick_instance_id","tool","args"],"additionalProperties":false}),
        },
        ToolDefinition {
            name: MODIFY_TICK.into(),
            description: "Open a protocol Tick hidden in the Tick host, or close a hosted Tick. Opening runs the program in a private terminal that keeps running without any View; it joins this Chat's Monitor and is never subscribed. For command[0]='tick', prefer the bundled sibling executable, then PATH; omitted cwd uses the Agent file work directory. Closing ends that Tick's process.".into(),
            input_schema: json!({"type":"object","oneOf":[
                {"type":"object","properties":{"action":{"const":"open"},"command":{"type":"array","minItems":1,"items":{"type":"string"}},"cwd":{"type":"string","minLength":1},"placement":{"type":"object","properties":{"type":{"const":HIDDEN_PLACEMENT}},"required":["type"],"additionalProperties":false}},"required":["action","command","placement"],"additionalProperties":false},
                {"type":"object","properties":{"action":{"const":"close"},"tick_instance_id":{"type":"string","minLength":1}},"required":["action","tick_instance_id"],"additionalProperties":false}
            ]}),
        },
    ]
}

#[async_trait]
impl HostToolProvider for HostedTickTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        hosted_definitions()
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
