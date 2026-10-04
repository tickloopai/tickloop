use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::session::{SessionClient, SessionError};
use crate::{
    herdr::{HerdrContext, HerdrPort, Pane, SocketHerdr, process_birth},
    host_error::HostError,
    instance::InstanceRef,
};

pub const TERMINAL_TEXT_LIMIT: usize = 64 * 1024;
pub const TERMINAL_LINE_LIMIT: usize = 200;
pub const INSTANCE_TOKEN: &str = "tickloop_instance";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TickLocation {
    pub instance: InstanceRef,
    pub pane: Pane,
    pub protocol: bool,
}

#[derive(Clone, Debug)]
pub struct AccessContext {
    pub workdir: PathBuf,
    pub workspace_id: Option<String>,
    pub interaction: Option<Pane>,
}

#[derive(Clone, Debug)]
pub struct TickSnapshot {
    pub instance: InstanceRef,
    pub data: Value,
    pub sequence: u64,
    pub observed_at: String,
    pub initialized: bool,
    pub terminal: bool,
    pub truncated: bool,
    pub descriptor: Option<Value>,
}

#[derive(Debug)]
pub struct PreparedOperation {
    pub name: String,
    pub input: Value,
    pub permission: AccessPermission,
    pub summary: String,
    pub target: Option<TickLocation>,
    pub declaration: Option<Value>,
    pub workspace: String,
    launch: Option<PreparedLaunch>,
    placement: PreparedPlacement,
    interaction: Option<Pane>,
    uses_legacy_caller: bool,
    socket_identity: Option<(u64, u64)>,
}

#[derive(Debug)]
enum PreparedPlacement {
    NewTab,
    Beside {
        destination: Box<Pane>,
        direction: &'static str,
    },
}

#[derive(Debug)]
struct PreparedLaunch {
    command: Vec<String>,
    cwd: PathBuf,
    executable_identity: (u64, u64, i64, i64),
    directory_identity: (u64, u64),
}

struct PreparedTickTool {
    args: Value,
    permission: AccessPermission,
    declaration: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessPermission {
    ReadOnly,
    Confirmation,
}

pub struct TickAccess {
    host: Arc<dyn HerdrPort>,
    context: HerdrContext,
    runtime_dir: Option<PathBuf>,
    known: Mutex<BTreeMap<InstanceRef, TickLocation>>,
}

impl TickAccess {
    pub fn new(context: HerdrContext) -> Self {
        Self::with_host(context.clone(), Arc::new(SocketHerdr::new(context)))
    }

    pub fn with_host(context: HerdrContext, host: Arc<dyn HerdrPort>) -> Self {
        Self {
            host,
            context,
            runtime_dir: None,
            known: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn with_runtime_dir(mut self, directory: PathBuf) -> Self {
        self.runtime_dir = Some(directory);
        self
    }

    pub fn context(&self) -> &HerdrContext {
        &self.context
    }

    pub async fn caller(&self) -> Result<Pane, HostError> {
        if self.context.caller_pane.is_empty() {
            return Err(HostError::before(
                "MissingAnchor",
                "this Agent has no interaction View",
            ));
        }
        let response = self
            .host
            .request(
                "pane.current",
                json!({"caller_pane_id":self.context.caller_pane}),
                CancellationToken::new(),
            )
            .await?;
        serde_json::from_value(response["pane"].clone()).map_err(invalid_host_response)
    }

    pub async fn workspace(&self, input: &Value) -> Result<String, HostError> {
        match input.get("workspace_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => Ok(id.to_owned()),
            Some(_) => Err(HostError::before(
                "InvalidArguments",
                "workspace_id cannot be empty",
            )),
            None => Ok(self.caller().await?.workspace_id),
        }
    }

    async fn panes(&self, workspace: Option<&str>) -> Result<Vec<Pane>, HostError> {
        let response = self
            .host
            .request(
                "pane.list",
                json!({"workspace_id":workspace}),
                CancellationToken::new(),
            )
            .await?;
        serde_json::from_value(response["panes"].clone()).map_err(invalid_host_response)
    }

    async fn identify(&self, pane: Pane) -> Result<TickLocation, HostError> {
        if let Some(instance) = pane.tokens.get(INSTANCE_TOKEN) {
            if instance.len() > 80 || !instance.starts_with("herdr:") {
                return Err(HostError::before(
                    "ProtocolViolation",
                    "invalid Tick discovery identity",
                ));
            }
            let instance = InstanceRef::native(instance)
                .map_err(|error| HostError::before("ProtocolViolation", error))?;
            return Ok(TickLocation {
                instance,
                pane,
                protocol: true,
            });
        }
        let response = self
            .host
            .request(
                "pane.process_info",
                json!({"pane_id":pane.pane_id}),
                CancellationToken::new(),
            )
            .await?;
        let process_info = response.get("process_info").unwrap_or(&response);
        let group = process_info["foreground_process_group_id"]
            .as_u64()
            .ok_or_else(|| {
                HostError::before(
                    "IdentityUncertain",
                    "Herdr cannot identify the foreground process group",
                )
            })?;
        let processes = process_info["foreground_processes"]
            .as_array()
            .ok_or_else(|| {
                HostError::before("IdentityUncertain", "foreground processes are unavailable")
            })?;
        let mut births = processes
            .iter()
            .map(|process| {
                let pid = process["pid"]
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())
                    .ok_or_else(|| {
                        HostError::before("IdentityUncertain", "invalid process identity")
                    })?;
                process_birth(pid)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if births.is_empty() {
            return Err(HostError::before(
                "IdentityUncertain",
                "no live foreground process",
            ));
        }
        births.sort();
        let instance = InstanceRef::native(format!(
            "terminal:{}:{group}:{}",
            pane.terminal_id,
            births.join("/")
        ))
        .map_err(|error| HostError::before("IdentityUncertain", error))?;
        Ok(TickLocation {
            instance,
            pane,
            protocol: false,
        })
    }

    /// Identifies every running program once, without reading any Tick content. The second
    /// value counts programs whose identity could not be proven for any reason, so an absent
    /// instance is only gone when every program was identified.
    pub async fn locate_all(&self) -> Result<(Vec<TickLocation>, usize), HostError> {
        let mut located = Vec::new();
        let mut uncertain = 0;
        for pane in self.panes(None).await? {
            match self.identify(pane).await {
                Ok(location) => located.push(location),
                Err(_) => uncertain += 1,
            }
        }
        Ok((located, uncertain))
    }

    pub async fn discover(&self, workspace: &str) -> Result<Value, HostError> {
        self.discover_scope(Some(workspace)).await
    }

    async fn discover_scope(&self, workspace: Option<&str>) -> Result<Value, HostError> {
        let mut ticks = Vec::new();
        let mut workspaces = std::collections::BTreeSet::new();
        for pane in self.panes(workspace).await? {
            workspaces.insert(pane.workspace_id.clone());
            match self.identify(pane.clone()).await {
                Ok(location) => {
                    self.known.lock().await.insert(location.instance.clone(), location.clone());
                    let mut tick = json!({"tick_instance_id":location.instance,"pane_id":pane.pane_id,"workspace_id":pane.workspace_id,"tab_id":pane.tab_id,"title":pane.title,"source":if location.protocol {"protocol"} else {"terminal"},"state":"live","capabilities":["read","send_input","subscribe"]});
                    if location.protocol {
                        match self.read(&location.instance).await {
                            Ok(snapshot) => {
                                tick["state"] = json!(if snapshot.initialized { "live" } else { "initializing" });
                                tick["capabilities"] = json!(["read","call_tick_tool","subscribe"]);
                            }
                            Err(error) => {
                                tick["state"] = json!(if error.code == "TargetGone" { "gone" } else { "disconnected" });
                                tick["capabilities"] = json!([]);
                                tick["error"] = json!(error.to_string());
                            }
                        }
                    }
                    ticks.push(tick);
                }
                Err(error) => ticks.push(json!({"pane_id":pane.pane_id,"workspace_id":pane.workspace_id,"state":"identity_uncertain","error":error.to_string()})),
            }
        }
        Ok(
            json!({"host":"herdr","host_instance":self.context.socket,"workspace_id":workspace,"workspaces":workspaces,"ticks":ticks}),
        )
    }

    async fn prepare_placement(
        &self,
        input: &Value,
        workspace: &str,
        is_open: bool,
        source: Option<&TickLocation>,
        context: Option<&AccessContext>,
    ) -> Result<PreparedPlacement, HostError> {
        let placement = input.get("placement");
        let kind = match placement {
            None => {
                if is_open {
                    "new_tab"
                } else {
                    "current_tab"
                }
            }
            Some(Value::Object(object)) => {
                object.get("type").and_then(Value::as_str).ok_or_else(|| {
                    HostError::before("InvalidArguments", "placement.type is required")
                })?
            }
            _ => {
                return Err(HostError::before(
                    "InvalidArguments",
                    "placement must be an object",
                ));
            }
        };
        let object = placement.and_then(Value::as_object);
        if input.get("destination_workspace_id").is_some() {
            return Err(HostError::before(
                "UnsupportedCapability",
                "moving a Tick across Workspaces is not supported",
            ));
        }
        let direction = match object.and_then(|object| object.get("direction")) {
            None => "right",
            Some(Value::String(direction)) => match direction.as_str() {
                "left" => "left",
                "right" => "right",
                "up" => "up",
                "down" => "down",
                _ => {
                    return Err(HostError::before(
                        "InvalidArguments",
                        "placement.direction must be left, right, up or down",
                    ));
                }
            },
            _ => {
                return Err(HostError::before(
                    "InvalidArguments",
                    "placement.direction must be left, right, up or down",
                ));
            }
        };
        if kind == "new_tab" {
            if !is_open
                || object.is_some_and(|object| {
                    object.get("pane_id").is_some() || object.get("direction").is_some()
                })
            {
                return Err(HostError::before(
                    "InvalidArguments",
                    "new_tab is only valid for open and has no pane_id or direction",
                ));
            }
            return Ok(PreparedPlacement::NewTab);
        }
        let destination = match kind {
            "current_tab" => {
                if object.is_some_and(|object| object.get("pane_id").is_some()) {
                    return Err(HostError::before(
                        "InvalidArguments",
                        "current_tab cannot specify pane_id",
                    ));
                }
                match context {
                    Some(context) => context.interaction.clone().ok_or_else(|| {
                        HostError::before("MissingAnchor", "this operation has no interaction View; specify a target Pane or use new_tab")
                    })?,
                    None => self.caller().await?,
                }
            }
            "pane" => {
                let pane_id = object
                    .and_then(|object| object.get("pane_id"))
                    .and_then(Value::as_str)
                    .filter(|pane_id| !pane_id.is_empty())
                    .ok_or_else(|| {
                        HostError::before("InvalidArguments", "placement.pane_id is required")
                    })?;
                self.get_pane(pane_id).await?
            }
            _ => {
                return Err(HostError::before(
                    "InvalidArguments",
                    "placement.type must be new_tab, current_tab or pane",
                ));
            }
        };
        if destination.workspace_id != workspace {
            return Err(HostError::before(
                "ScopeRequired",
                "destination Pane must be in the selected Workspace",
            ));
        }
        if source.is_some_and(|source| source.pane.pane_id == destination.pane_id) {
            return Err(HostError::before(
                "InvalidArguments",
                "cannot move a Tick beside itself",
            ));
        }
        Ok(PreparedPlacement::Beside {
            destination: Box::new(destination),
            direction,
        })
    }

    async fn get_pane(&self, pane_id: &str) -> Result<Pane, HostError> {
        let response = self
            .host
            .request(
                "pane.get",
                json!({"pane_id":pane_id}),
                CancellationToken::new(),
            )
            .await?;
        serde_json::from_value(response["pane"].clone()).map_err(invalid_host_response)
    }

    async fn revalidate_placement(&self, placement: &PreparedPlacement) -> Result<(), HostError> {
        if let PreparedPlacement::Beside { destination, .. } = placement {
            let current = self.get_pane(&destination.pane_id).await.map_err(|_| {
                HostError::before(
                    "TargetChanged",
                    "destination Pane disappeared after approval",
                )
            })?;
            if current.terminal_id != destination.terminal_id
                || current.tab_id != destination.tab_id
                || current.workspace_id != destination.workspace_id
            {
                return Err(HostError::before(
                    "TargetChanged",
                    "destination Pane moved or changed after approval",
                ));
            }
        }
        Ok(())
    }

    pub async fn resolve(&self, instance: &InstanceRef) -> Result<TickLocation, HostError> {
        let previous = self.known.lock().await.get(instance).cloned();
        let mut uncertain = false;
        for pane in self.panes(None).await? {
            let possible = match &previous {
                Some(old) if !old.protocol => pane.terminal_id == old.pane.terminal_id,
                _ => true,
            };
            if !possible {
                continue;
            }
            match self.identify(pane).await {
                Ok(location) if &location.instance == instance => {
                    self.known
                        .lock()
                        .await
                        .insert(instance.clone(), location.clone());
                    return Ok(location);
                }
                Err(error) if error.code == "IdentityUncertain" => uncertain = true,
                _ => {}
            }
        }
        if uncertain && previous.is_some() {
            Err(HostError::before(
                "IdentityUncertain",
                "cannot prove that the selected Tick is still the same running program",
            ))
        } else {
            Err(HostError::before(
                "TargetGone",
                "the selected Tick is no longer running; select its replacement explicitly",
            ))
        }
    }

    pub async fn read(&self, instance: &InstanceRef) -> Result<TickSnapshot, HostError> {
        let target = self.resolve(instance).await?;
        if target.protocol {
            let client = self.session(instance).await?;
            let descriptor = client.describe().await.map_err(session_error)?;
            let read = client.read().await.map_err(session_error)?;
            return Ok(TickSnapshot {
                instance: instance.clone(),
                data: read.data.unwrap_or(Value::Null),
                sequence: read.sequence,
                observed_at: read.observed_at,
                initialized: read.initialized,
                terminal: false,
                truncated: false,
                descriptor: Some(serde_json::to_value(descriptor).map_err(invalid_host_response)?),
            });
        }
        let response = self.host.request("pane.read", json!({"pane_id":target.pane.pane_id,"source":"recent_unwrapped","format":"text","strip_ansi":true,"lines":TERMINAL_LINE_LIMIT}), CancellationToken::new()).await?;
        let response = response.get("read").unwrap_or(&response);
        if response["source"] != "recent_unwrapped" || response["format"] != "text" {
            return Err(HostError::before(
                "IncompatibleHost",
                "Herdr must support recent-unwrapped plain text",
            ));
        }
        let text = response["text"]
            .as_str()
            .ok_or_else(|| HostError::before("ProtocolViolation", "Herdr read omitted text"))?;
        let (text, truncated) = limit_terminal_text(text);
        self.resolve(instance).await?;
        Ok(TickSnapshot {
            instance: instance.clone(),
            data: json!({"text":text}),
            sequence: 0,
            observed_at: observed_now(),
            initialized: true,
            terminal: true,
            truncated: truncated || response["truncated"].as_bool().unwrap_or(false),
            descriptor: None,
        })
    }

    pub async fn session(&self, instance: &InstanceRef) -> Result<SessionClient, HostError> {
        let target = self.resolve(instance).await?;
        if !target.protocol {
            return Err(HostError::before(
                "UnsupportedCapability",
                "ordinary programs do not provide Tick Tools",
            ));
        }
        SessionClient::connect(
            instance.clone(),
            crate::session::SessionBackend::Herdr {
                socket: self.context.socket.clone(),
                terminal_id: target.pane.terminal_id,
            },
            self.runtime_dir.clone(),
        )
        .await
        .map_err(session_error)
    }

    async fn prepare_launch(
        &self,
        input: &Value,
        context: Option<&AccessContext>,
    ) -> Result<PreparedLaunch, HostError> {
        let command: Vec<String> =
            serde_json::from_value(input["command"].clone()).map_err(|error| {
                HostError::before(
                    "InvalidArguments",
                    format!("command must be a nonempty argv array: {error}"),
                )
            })?;
        if command.is_empty() || command.iter().any(|arg| arg.contains('\0')) {
            return Err(HostError::before(
                "InvalidArguments",
                "command must contain an executable and no NUL bytes",
            ));
        }
        let cwd = match input.get("cwd").and_then(Value::as_str) {
            Some(cwd) => Some(PathBuf::from(cwd)),
            None => match context {
                Some(context) => Some(context.workdir.clone()),
                None => {
                    let caller = self.caller().await?;
                    caller.foreground_cwd.or(caller.cwd).map(PathBuf::from)
                }
            },
        }
        .ok_or_else(|| {
            HostError::before(
                "InvalidArguments",
                "specify cwd; caller working directory is unavailable",
            )
        })?;
        let cwd = cwd.canonicalize().map_err(|error| {
            HostError::before(
                "InvalidArguments",
                format!("invalid working directory: {error}"),
            )
        })?;
        if !cwd.is_dir() {
            return Err(HostError::before(
                "InvalidArguments",
                "cwd must be a directory",
            ));
        }
        let executable = resolve_executable(&command[0], &cwd)?;
        let mut command = command;
        command[0] = executable.to_string_lossy().into_owned();
        let executable_identity = executable_identity(&executable)?;
        let directory = std::fs::metadata(&cwd)
            .map_err(|error| HostError::before("InvalidArguments", error.to_string()))?;
        Ok(PreparedLaunch {
            command,
            cwd,
            executable_identity,
            directory_identity: (directory.dev(), directory.ino()),
        })
    }

    async fn prepare_tool(
        &self,
        location: &TickLocation,
        input: &Value,
    ) -> Result<PreparedTickTool, HostError> {
        let descriptor = self
            .session(&location.instance)
            .await?
            .describe()
            .await
            .map_err(session_error)?;
        let tool_name = input["tool"].as_str().ok_or_else(|| {
            HostError::before(
                "InvalidArguments",
                "tool is required; read the Tick catalog first",
            )
        })?;
        let tool = descriptor
            .tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .ok_or_else(|| {
                HostError::before(
                    "UnknownTool",
                    "the Tick does not declare that tool; read its catalog first",
                )
            })?;
        let args = input.get("args").cloned().unwrap_or_else(|| json!({}));
        let validator = jsonschema::validator_for(&tool.params)
            .map_err(|error| HostError::before("InvalidDeclaration", error.to_string()))?;
        validator.validate(&args).map_err(|error| {
            HostError::before(
                "InvalidArguments",
                format!("{error}; schema: {}", tool.params),
            )
        })?;

        let permission = if tool.mutates {
            AccessPermission::Confirmation
        } else {
            AccessPermission::ReadOnly
        };
        Ok(PreparedTickTool {
            args,
            permission,
            declaration: serde_json::to_value(descriptor).map_err(invalid_host_response)?,
        })
    }

    async fn resolve_target(
        &self,
        input: &Value,
        workspace: &str,
    ) -> Result<TickLocation, HostError> {
        let instance: InstanceRef =
            serde_json::from_value(input.get("tick_instance_id").cloned().ok_or_else(|| {
                HostError::before(
                    "InvalidArguments",
                    "tick_instance_id is required; read_workspace first",
                )
            })?)
            .map_err(|error| HostError::before("InvalidArguments", error.to_string()))?;
        let location = self.resolve(&instance).await?;
        if location.pane.workspace_id != workspace {
            return Err(HostError::before(
                "ScopeRequired",
                "specify workspace_id explicitly for a target outside the current Workspace",
            ));
        }
        Ok(location)
    }

    fn validate_modification(
        input: &Value,
        target: Option<&TickLocation>,
    ) -> Result<(), HostError> {
        match input["action"].as_str().unwrap_or("") {
            "open" | "close" | "move" => Ok(()),
            "send_input" => {
                if target.is_some_and(|target| target.protocol) {
                    return Err(HostError::before(
                        "UnsupportedCapability",
                        "use the declared Tick Tools for protocol programs",
                    ));
                }
                let has_text = input.get("text").is_some_and(Value::is_string);
                let valid_keys = input
                    .get("keys")
                    .and_then(Value::as_array)
                    .is_some_and(|keys| !keys.is_empty() && keys.iter().all(Value::is_string));
                if !has_text && !valid_keys {
                    return Err(HostError::before(
                        "InvalidArguments",
                        "send_input requires text or keys",
                    ));
                }
                Ok(())
            }
            _ => Err(HostError::before(
                "InvalidArguments",
                "supported action is open, move, send_input or close",
            )),
        }
    }

    pub async fn prepare(&self, name: &str, input: &Value) -> Result<PreparedOperation, HostError> {
        self.prepare_internal(name, input, None).await
    }

    pub async fn prepare_in_context(
        &self,
        name: &str,
        input: &Value,
        context: &AccessContext,
    ) -> Result<PreparedOperation, HostError> {
        self.prepare_internal(name, input, Some(context)).await
    }

    async fn prepare_internal(
        &self,
        name: &str,
        input: &Value,
        context: Option<&AccessContext>,
    ) -> Result<PreparedOperation, HostError> {
        let mut normalized = input.as_object().cloned().ok_or_else(|| {
            HostError::before(
                "InvalidArguments",
                "host operation arguments must be an object",
            )
        })?;
        let workspace = match context {
            None => self.workspace(input).await?,
            Some(context) => {
                if input.get("workspace_id").is_some()
                    && input["workspace_id"].as_str().is_none_or(str::is_empty)
                {
                    return Err(HostError::before(
                        "InvalidArguments",
                        "workspace_id must be a nonempty string",
                    ));
                }
                let relative_to_view = input["placement"]["type"] == "current_tab"
                    || (name == "modify_tick"
                        && input["action"] == "move"
                        && input.get("placement").is_none());
                let workspace = input["workspace_id"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| {
                        relative_to_view
                            .then(|| {
                                context
                                    .interaction
                                    .as_ref()
                                    .map(|pane| pane.workspace_id.clone())
                            })
                            .flatten()
                    })
                    .or_else(|| context.workspace_id.clone());
                match workspace {
                    Some(workspace) => workspace,
                    None if name == "read_workspace" => String::new(),
                    None if input.get("tick_instance_id").is_some() => {
                        let instance: InstanceRef =
                            serde_json::from_value(input["tick_instance_id"].clone()).map_err(
                                |error| HostError::before("InvalidArguments", error.to_string()),
                            )?;
                        self.resolve(&instance).await?.pane.workspace_id
                    }
                    None if input["placement"]["type"] == "pane" => {
                        let pane = input["placement"]["pane_id"].as_str().ok_or_else(|| {
                            HostError::before("InvalidArguments", "placement.pane_id is required")
                        })?;
                        self.get_pane(pane).await?.workspace_id
                    }
                    None => {
                        return Err(HostError::before(
                            "ScopeRequired",
                            "specify workspace_id; no default Workspace is configured",
                        ));
                    }
                }
            }
        };
        if !workspace.is_empty() {
            normalized.insert("workspace_id".into(), json!(workspace));
        }
        let mut target = None;
        let mut launch = None;
        let mut placement = PreparedPlacement::NewTab;
        let mut permission = match name {
            "read_workspace" => {
                discovery_page_request(input)?;
                AccessPermission::ReadOnly
            }
            "modify_tick" if input["action"] == "open" => {
                placement = self
                    .prepare_placement(input, &workspace, true, None, context)
                    .await?;
                let prepared_launch = self.prepare_launch(input, context).await?;
                normalized.insert("command".into(), json!(prepared_launch.command));
                normalized.insert("cwd".into(), json!(prepared_launch.cwd));
                launch = Some(prepared_launch);
                AccessPermission::Confirmation
            }
            "read_tick" | "modify_tick" | "call_tick_tool" => {
                let location = self.resolve_target(input, &workspace).await?;
                normalized.insert("tick_instance_id".into(), json!(location.instance));
                normalized.insert("pane_id".into(), json!(location.pane.pane_id));
                target = Some(location);
                if name == "read_tick" {
                    AccessPermission::ReadOnly
                } else {
                    AccessPermission::Confirmation
                }
            }
            _ => return Err(HostError::before("UnknownTool", "unknown Native host tool")),
        };
        let mut declaration = None;
        if name == "call_tick_tool" {
            let location = target
                .as_ref()
                .ok_or_else(|| HostError::before("InvalidArguments", "missing call target"))?;
            let prepared_tool = self.prepare_tool(location, input).await?;
            normalized.insert("args".into(), prepared_tool.args);
            permission = prepared_tool.permission;
            declaration = Some(prepared_tool.declaration);
        }
        if name == "modify_tick" {
            Self::validate_modification(input, target.as_ref())?;
            if input["action"] == "move" {
                placement = self
                    .prepare_placement(input, &workspace, false, target.as_ref(), context)
                    .await?;
            } else if input["action"] != "open" && input.get("placement").is_some() {
                return Err(HostError::before(
                    "InvalidArguments",
                    "placement is only valid for open or move",
                ));
            }
        }
        if context.is_some() {
            self.revalidate_placement(&placement).await?;
        }
        let placement_summary = match &placement {
            PreparedPlacement::NewTab => json!({"type":"new_tab"}),
            PreparedPlacement::Beside {
                destination,
                direction,
            } => {
                json!({"type":"pane","pane_id":destination.pane_id,"terminal_id":destination.terminal_id,"tab_id":destination.tab_id,"workspace_id":destination.workspace_id,"direction":direction})
            }
        };
        if name == "modify_tick" && (input["action"] == "open" || input["action"] == "move") {
            normalized.insert("resolved_placement".into(), placement_summary.clone());
        }
        let input = Value::Object(normalized);
        Ok(PreparedOperation {
            name: name.into(),
            summary: format!(
                "{name} in Herdr Workspace {workspace}: {input}; destination: {placement_summary}"
            ),
            input,
            permission,
            target,
            declaration,
            workspace,
            launch,
            placement,
            interaction: context.and_then(|context| context.interaction.clone()),
            uses_legacy_caller: context.is_none(),
            socket_identity: std::fs::metadata(&self.context.socket)
                .ok()
                .map(|metadata| (metadata.dev(), metadata.ino())),
        })
    }

    pub async fn revalidate(&self, prepared: &PreparedOperation) -> Result<(), HostError> {
        if let Some(expected) = prepared.socket_identity {
            let current = std::fs::metadata(&self.context.socket)
                .ok()
                .map(|metadata| (metadata.dev(), metadata.ino()));
            if current != Some(expected) {
                return Err(HostError::before(
                    "TargetChanged",
                    "host endpoint changed after preparation",
                ));
            }
        }
        if let Some(launch) = &prepared.launch
            && (executable_identity(std::path::Path::new(&launch.command[0]))?
                != launch.executable_identity
                || std::fs::metadata(&launch.cwd)
                    .ok()
                    .filter(|metadata| metadata.is_dir())
                    .map(|metadata| (metadata.dev(), metadata.ino()))
                    != Some(launch.directory_identity))
        {
            return Err(HostError::before(
                "TargetChanged",
                "executable or working directory changed after approval",
            ));
        }
        if let Some(target) = &prepared.target {
            let current = self.resolve(&target.instance).await?;
            if current.pane.workspace_id != prepared.workspace {
                return Err(HostError::before(
                    "TargetChanged",
                    "target moved outside the approved Workspace; prepare and approve again",
                ));
            }
            if prepared.name == "modify_tick"
                && prepared.input["action"] == "move"
                && (current.pane.pane_id != target.pane.pane_id
                    || current.pane.terminal_id != target.pane.terminal_id
                    || current.pane.tab_id != target.pane.tab_id)
            {
                return Err(HostError::before(
                    "TargetChanged",
                    "source Tick moved after approval",
                ));
            }
            if let Some(expected) = &prepared.declaration {
                let descriptor = self
                    .session(&target.instance)
                    .await?
                    .describe()
                    .await
                    .map_err(session_error)?;
                if serde_json::to_value(descriptor).map_err(invalid_host_response)? != *expected {
                    return Err(HostError::before(
                        "CapabilitiesChanged",
                        "Tick declarations changed after preparation; prepare and approve again",
                    ));
                }
            }
        }
        self.revalidate_placement(&prepared.placement).await?;
        Ok(())
    }

    pub async fn execute(
        &self,
        prepared: PreparedOperation,
        cancel: CancellationToken,
    ) -> Result<Value, HostError> {
        if cancel.is_cancelled() {
            return Err(HostError::before(
                "Cancelled",
                "operation was not dispatched",
            ));
        }
        self.revalidate(&prepared).await?;
        match prepared.name.as_str() {
            "read_workspace" => {
                let mut result = self
                    .discover_scope(
                        (!prepared.workspace.is_empty()).then_some(prepared.workspace.as_str()),
                    )
                    .await?;
                let caller = match prepared.interaction {
                    Some(expected) => self.get_pane(&expected.pane_id).await.ok().filter(|pane| {
                        pane.terminal_id == expected.terminal_id
                            && pane.tab_id == expected.tab_id
                            && pane.workspace_id == expected.workspace_id
                    }),
                    None if prepared.uses_legacy_caller => self.caller().await.ok(),
                    None => None,
                };
                result["caller"] = caller.map_or(Value::Null, |caller| json!({"pane_id":caller.pane_id,"tab_id":caller.tab_id,"workspace_id":caller.workspace_id}));
                let (cursor, limit) = discovery_page_request(&prepared.input)?;
                let ticks = match result["ticks"].take() {
                    Value::Array(ticks) => ticks,
                    _ => Vec::new(),
                };
                let page = crate::page::paginate(ticks, discovery_key, cursor.as_ref(), limit)
                    .map_err(|error| HostError::before(error.code(), error.to_string()))?;
                result["ticks"] = json!(page.items);
                result["total"] = json!(page.total);
                result["generation"] = json!(page.generation);
                result["next_cursor"] = json!(page.next);
                Ok(result)
            }
            "read_tick" => {
                let target = prepared.target.ok_or_else(|| {
                    HostError::before("InvalidArguments", "missing prepared target")
                })?;
                let snapshot = self.read(&target.instance).await?;
                let tools = snapshot
                    .descriptor
                    .as_ref()
                    .and_then(|descriptor| descriptor.get("tools"))
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                Ok(
                    json!({"tick_instance_id":snapshot.instance,"source":if snapshot.terminal {"terminal"} else {"protocol"},"initialized":snapshot.initialized,"sequence":snapshot.sequence,"observed_at":snapshot.observed_at,"content":snapshot.data,"truncated":snapshot.truncated,"descriptor":snapshot.descriptor,"tools":tools}),
                )
            }
            "call_tick_tool" => {
                let target = prepared
                    .target
                    .ok_or_else(|| HostError::before("InvalidArguments", "missing call target"))?;
                let client = self.session(&target.instance).await?;
                let tool = prepared.input["tool"]
                    .as_str()
                    .ok_or_else(|| HostError::before("InvalidArguments", "missing tool name"))?;
                let descriptor = serde_json::from_value(prepared.declaration.ok_or_else(|| {
                    HostError::before("InvalidDeclaration", "prepared Tick call has no catalog")
                })?)
                .map_err(invalid_host_response)?;
                client
                    .call_prepared(
                        tool,
                        prepared.input["args"].clone(),
                        &crate::protocol::descriptor_digest(&descriptor),
                        cancel,
                    )
                    .await
                    .map_err(session_error)
            }
            "modify_tick" => {
                if let Some(launch) = prepared.launch {
                    return self
                        .open(launch, &prepared.workspace, prepared.placement, cancel)
                        .await;
                }
                let target = prepared.target.ok_or_else(|| {
                    HostError::before("InvalidArguments", "missing prepared target")
                })?;
                let current = self.resolve(&target.instance).await?;
                let action = prepared.input["action"].as_str().unwrap_or("");
                if action == "move" {
                    if current.pane.pane_id != target.pane.pane_id
                        || current.pane.terminal_id != target.pane.terminal_id
                        || current.pane.tab_id != target.pane.tab_id
                    {
                        return Err(HostError::before(
                            "TargetChanged",
                            "source Tick moved after approval",
                        ));
                    }
                    return self
                        .move_pane(&target, &current.pane.pane_id, &prepared.placement, cancel)
                        .await;
                }
                let method = if action == "close" {
                    "pane.close"
                } else {
                    "pane.send_input"
                };
                let mut params = json!({"pane_id":current.pane.pane_id});
                for key in ["text", "keys"] {
                    if let Some(value) = prepared.input.get(key) {
                        params[key] = value.clone();
                    }
                }
                let result = self.host.request(method, params, cancel).await?;
                Ok(
                    json!({"status":"completed","tick_instance_id":target.instance,"result":result,"input_guarantee":if action=="send_input" {Some("checked before sending; Herdr cannot atomically compare the running program and send input")} else {None}}),
                )
            }
            _ => Err(HostError::before(
                "UnsupportedCapability",
                "host operation is not available",
            )),
        }
    }

    async fn open(
        &self,
        launch: PreparedLaunch,
        workspace: &str,
        placement: PreparedPlacement,
        cancel: CancellationToken,
    ) -> Result<Value, HostError> {
        let is_tick = std::path::Path::new(&launch.command[0])
            .file_name()
            .is_some_and(|name| name == "tick");
        if is_tick {
            let mut command = tokio::process::Command::new(&launch.command[0]);
            command
                .arg("__preflight")
                .args(&launch.command[1..])
                .current_dir(&launch.cwd)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            let result = tokio::select! {
                _ = cancel.cancelled() => return Err(HostError::before("Cancelled", "open cancelled before creating a Pane")),
                result = tokio::time::timeout(crate::herdr::HOST_TIMEOUT, command.output()) => result,
            }.map_err(|_| HostError::before("Timeout", "Tick local preflight exceeded 10 seconds"))?
                .map_err(|error| HostError::before("PreflightFailed", error.to_string()))?;
            if !result.status.success() {
                return Err(HostError::before(
                    "PreflightFailed",
                    String::from_utf8_lossy(&result.stderr).into_owned(),
                ));
            }
        }
        let environment: BTreeMap<_, _> = [
            "TICKLOOP_CONFIG_DIR",
            "TICKLOOP_TICK_RUNTIME_DIR",
            "LANG",
            "LC_CTYPE",
            "LC_ALL",
            "LC_COLLATE",
            "LC_MESSAGES",
            "LC_MONETARY",
            "LC_NUMERIC",
            "LC_TIME",
        ]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name, value)))
        .collect();
        let response = self.host.request("layout.apply", json!({"workspace_id":workspace,"focus":false,"root":{"type":"pane","command":launch.command,"cwd":launch.cwd,"env":environment}}), cancel.clone()).await?;
        let mut pane_id = response["layout"]["root"]["pane_id"].as_str()
            .ok_or_else(|| HostError {code:"OutcomeUnknown".into(),message:"Herdr created a layout but returned no Pane identity; inspect the Workspace before retrying".into(),dispatched:true})?.to_owned();
        if let PreparedPlacement::Beside { .. } = &placement {
            let staged = match self.get_pane(&pane_id).await {
                Ok(staged) if staged.workspace_id == workspace => staged,
                Ok(_) => {
                    return Ok(self
                        .created_not_placed(
                            &pane_id,
                            workspace,
                            &HostError::before(
                                "TargetChanged",
                                "created Pane was not in the approved Workspace",
                            ),
                        )
                        .await);
                }
                Err(error) => {
                    return Ok(self.created_not_placed(&pane_id, workspace, &error).await);
                }
            };
            if let Err(error) = self.revalidate_placement(&placement).await {
                return Ok(self.created_not_placed(&pane_id, workspace, &error).await);
            }
            if cancel.is_cancelled() {
                return Ok(self
                    .created_not_placed(
                        &pane_id,
                        workspace,
                        &HostError::before("Cancelled", "placement cancelled after Pane creation"),
                    )
                    .await);
            }
            let moved = match self
                .request_pane_move(&pane_id, &placement, cancel.clone())
                .await
            {
                Ok(moved) => moved,
                Err(error) => {
                    return Ok(self.created_not_placed(&pane_id, workspace, &error).await);
                }
            };
            let reported = match moved_pane(&moved) {
                Ok(reported) => reported,
                Err(error) => {
                    return Ok(self.created_not_placed(&pane_id, workspace, &error).await);
                }
            };
            if reported.terminal_id != staged.terminal_id
                || !matches_destination(&reported, &placement)
            {
                return Ok(self.created_not_placed(
                    &pane_id,
                    workspace,
                    &HostError { code:"OutcomeUnknown".into(), message:"Herdr move reply did not identify the created Pane at the approved destination".into(), dispatched:true },
                ).await);
            }
            pane_id = reported.pane_id.clone();
            if let Err(error) = self
                .orient_placement(&reported, &placement, cancel.clone())
                .await
            {
                return Ok(self.created_not_placed(&pane_id, workspace, &error).await);
            }
        }
        let deadline = tokio::time::Instant::now() + crate::herdr::HOST_TIMEOUT;
        loop {
            if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
                let mut result = self.created_location(&pane_id, workspace).await;
                result["status"] = json!("initializing");
                result["stage"] = json!("discover_instance");
                result["diagnostic"] = json!(
                    "Pane was created; inspect it before retrying. Do not create a second instance."
                );
                return Ok(result);
            }
            let response = self
                .host
                .request(
                    "pane.get",
                    json!({"pane_id":pane_id}),
                    CancellationToken::new(),
                )
                .await;
            if let Ok(response) = response
                && let Ok(pane) = serde_json::from_value::<Pane>(response["pane"].clone())
                && matches_destination(&pane, &placement)
                && (!is_tick || pane.tokens.contains_key(INSTANCE_TOKEN))
                && let Ok(target) = self.identify(pane).await
            {
                self.known
                    .lock()
                    .await
                    .insert(target.instance.clone(), target.clone());
                return Ok(
                    json!({"host":"herdr","host_instance":self.context.socket,"observed_at":observed_now(),"status":"opened","tick_instance_id":target.instance,"pane_id":target.pane.pane_id,"terminal_id":target.pane.terminal_id,"tab_id":target.pane.tab_id,"workspace_id":target.pane.workspace_id}),
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    async fn created_not_placed(&self, pane_id: &str, workspace: &str, error: &HostError) -> Value {
        let mut result = self.created_location(pane_id, workspace).await;
        result["status"] = json!("created_not_placed");
        result["stage"] = json!("move_pane");
        result["error"] =
            json!({"code":error.code,"message":error.message,"dispatched":error.dispatched});
        result["diagnostic"] = json!(
            "Pane was created, but placement was not confirmed; inspect it before retrying. Do not create a second instance."
        );
        result
    }

    async fn created_location(&self, pane_id: &str, workspace: &str) -> Value {
        let mut result = json!({"host":"herdr","host_instance":self.context.socket,"pane_id":pane_id,"requested_workspace_id":workspace,"workspace_id":null,"tab_id":null,"terminal_id":null,"tick_instance_id":null});
        if let Ok(pane) = self.get_pane(pane_id).await {
            result["workspace_id"] = json!(pane.workspace_id);
            result["tab_id"] = json!(pane.tab_id);
            result["terminal_id"] = json!(pane.terminal_id);
            if let Ok(target) = self.identify(pane).await {
                result["tick_instance_id"] = json!(target.instance);
            }
        }
        result["observed_at"] = json!(observed_now());
        result
    }

    async fn request_pane_move(
        &self,
        source_pane_id: &str,
        placement: &PreparedPlacement,
        cancel: CancellationToken,
    ) -> Result<Value, HostError> {
        let PreparedPlacement::Beside {
            destination,
            direction,
        } = placement
        else {
            return Err(HostError::before(
                "InvalidArguments",
                "move requires an existing destination Pane",
            ));
        };
        let split = match *direction {
            "left" | "right" => "right",
            _ => "down",
        };
        self.host.request("pane.move", json!({"pane_id":source_pane_id,"focus":false,"destination":{"type":"tab","tab_id":destination.tab_id,"target_pane_id":destination.pane_id,"split":split}}), cancel).await
    }

    async fn orient_placement(
        &self,
        source: &Pane,
        placement: &PreparedPlacement,
        cancel: CancellationToken,
    ) -> Result<(), HostError> {
        let PreparedPlacement::Beside {
            destination,
            direction,
        } = placement
        else {
            return Ok(());
        };
        let initial_direction = match *direction {
            "left" => "right",
            "up" => "down",
            direction => direction,
        };
        let result = async {
            self.revalidate_placement(placement).await?;
            let current = self.get_pane(&source.pane_id).await?;
            if current.terminal_id != source.terminal_id
                || !matches_destination(&current, placement)
            {
                return Err(HostError::before(
                    "TargetChanged",
                    "Tick changed before arranging its position",
                ));
            }
            self.verify_neighbor(&destination.pane_id, initial_direction, &source.pane_id)
                .await?;
            if initial_direction == *direction {
                return Ok(());
            }
            if cancel.is_cancelled() {
                return Err(HostError::before(
                    "Cancelled",
                    "positioning cancelled after Pane move",
                ));
            }
            // Herdr swap focuses its source; keep the anchor (the Agent for current_tab) focused.
            let response = self
                .host
                .request(
                    "pane.swap",
                    json!({"source_pane_id":destination.pane_id,"target_pane_id":source.pane_id}),
                    cancel,
                )
                .await?;
            if response["swap"]["changed"] != true
                || response["swap"]["source_pane_id"] != destination.pane_id
                || response["swap"]["target_pane_id"] != source.pane_id
            {
                return Err(HostError::before(
                    "PlacementNotApplied",
                    format!(
                        "Herdr did not confirm the requested position: {}",
                        response["swap"]
                    ),
                ));
            }
            self.revalidate_placement(placement).await?;
            let current = self.get_pane(&source.pane_id).await?;
            if current.terminal_id != source.terminal_id
                || !matches_destination(&current, placement)
            {
                return Err(HostError::before(
                    "TargetChanged",
                    "Tick changed while arranging its position",
                ));
            }
            self.verify_neighbor(&destination.pane_id, direction, &source.pane_id)
                .await
        }
        .await;
        result.map_err(|mut error: HostError| {
            error.dispatched = true;
            error.message = format!("{}; Pane {} was already moved into the target Tab, but its requested side was not confirmed; inspect before retrying", error.message, source.pane_id);
            error
        })
    }

    async fn verify_neighbor(
        &self,
        anchor: &str,
        direction: &str,
        source: &str,
    ) -> Result<(), HostError> {
        let response = self
            .host
            .request(
                "pane.neighbor",
                json!({"pane_id":anchor,"direction":direction}),
                CancellationToken::new(),
            )
            .await?;
        if response["neighbor"]["pane_id"] != anchor
            || response["neighbor"]["direction"] != direction
            || response["neighbor"]["neighbor_pane_id"] != source
        {
            return Err(HostError::before(
                "TargetChanged",
                "Pane adjacency no longer matches the requested placement",
            ));
        }
        Ok(())
    }

    async fn verify_moved_instance(
        &self,
        target: &TickLocation,
        expected: &Pane,
    ) -> Result<(), HostError> {
        let result = self.resolve(&target.instance).await.and_then(|current| {
            if current.pane.pane_id == expected.pane_id
                && current.pane.terminal_id == expected.terminal_id
                && current.pane.workspace_id == expected.workspace_id
                && current.pane.tab_id == expected.tab_id
            {
                Ok(())
            } else {
                Err(HostError::before(
                    "TargetChanged",
                    "Tick changed after the preceding Pane move",
                ))
            }
        });
        result.map_err(|mut error| {
            error.dispatched = true;
            error.message = format!(
                "{}; Tick {} was last observed in Pane {} of Tab {}; inspect before retrying",
                error.message, target.instance, expected.pane_id, expected.tab_id
            );
            error
        })
    }

    async fn move_pane(
        &self,
        target: &TickLocation,
        source_pane_id: &str,
        placement: &PreparedPlacement,
        cancel: CancellationToken,
    ) -> Result<Value, HostError> {
        self.revalidate_placement(placement).await?;
        let mut source_pane_id = source_pane_id.to_owned();
        let staged = matches_destination(&target.pane, placement);
        if staged {
            // Herdr treats moves within one Tab as no-ops; retain the live Pane in a temporary Tab.
            let response = self.host.request("pane.move", json!({"pane_id":source_pane_id,"focus":false,"destination":{"type":"new_tab","workspace_id":target.pane.workspace_id}}), cancel.clone()).await?;
            let pane = moved_pane(&response)?;
            if pane.terminal_id != target.pane.terminal_id
                || pane.workspace_id != target.pane.workspace_id
                || matches_destination(&pane, placement)
            {
                return Err(HostError {code:"OutcomeUnknown".into(),message:"Herdr did not confirm the same instance in a temporary Tab; inspect before retrying".into(),dispatched:true});
            }
            self.verify_moved_instance(target, &pane).await?;
            source_pane_id = pane.pane_id;
            self.revalidate_placement(placement)
                .await
                .map_err(|mut error| {
                    error.dispatched = true;
                    error.message = format!(
                        "{}; Tick {} is in temporary Pane {}; inspect before retrying",
                        error.message, target.instance, source_pane_id
                    );
                    error
                })?;
        }
        let moved = self.request_pane_move(&source_pane_id, placement, cancel.clone()).await.map_err(|mut error| {
            error.dispatched |= staged;
            error.message = format!("{}; Tick {} was last observed in Pane {}; its final location must be inspected before retrying", error.message, target.instance, source_pane_id);
            error
        })?;
        let reported = moved_pane(&moved)?;
        if reported.terminal_id != target.pane.terminal_id
            || !matches_destination(&reported, placement)
        {
            return Err(HostError {
                code: "OutcomeUnknown".into(),
                message: format!(
                    "Herdr accepted move of Pane {source_pane_id}, but reported an unexpected terminal or destination; inspect before retrying"
                ),
                dispatched: true,
            });
        }
        self.verify_moved_instance(target, &reported).await?;
        self.orient_placement(&reported, placement, cancel).await?;
        let current = self.resolve(&target.instance).await.map_err(|mut error| {
            error.message = format!("Herdr accepted move of Pane {source_pane_id}, but Tick identity could not be verified: {}; inspect before retrying", error.message);
            error.dispatched = true;
            error
        })?;
        if current.pane.pane_id != reported.pane_id
            || !matches_destination(&current.pane, placement)
            || current.pane.terminal_id != target.pane.terminal_id
        {
            return Err(HostError {
                code: "OutcomeUnknown".into(),
                message: format!(
                    "Herdr accepted move of Pane {source_pane_id}, but its new location did not match the approved destination; inspect before retrying"
                ),
                dispatched: true,
            });
        }
        Ok(
            json!({"host":"herdr","host_instance":self.context.socket,"observed_at":observed_now(),"status":"moved","tick_instance_id":target.instance,"pane_id":current.pane.pane_id,"terminal_id":current.pane.terminal_id,"tab_id":current.pane.tab_id,"workspace_id":current.pane.workspace_id}),
        )
    }
}

fn moved_pane(response: &Value) -> Result<Pane, HostError> {
    if response["move_result"]["changed"] == false {
        return Err(HostError {
            code: "PlacementNotApplied".into(),
            message: format!(
                "Herdr left the Pane unchanged: {}; inspect its location before retrying",
                response["move_result"]["reason"]
            ),
            dispatched: true,
        });
    }
    serde_json::from_value(response["move_result"]["pane"].clone()).map_err(|error| HostError {
        code: "OutcomeUnknown".into(),
        message: format!("Herdr accepted Pane move but returned no valid Pane identity: {error}; inspect before retrying"),
        dispatched: true,
    })
}

fn matches_destination(pane: &Pane, placement: &PreparedPlacement) -> bool {
    match placement {
        PreparedPlacement::NewTab => true,
        PreparedPlacement::Beside { destination, .. } => {
            pane.workspace_id == destination.workspace_id && pane.tab_id == destination.tab_id
        }
    }
}

/// Resolves a program the way an Agent open does: the bundled `tick` beside the Agent first, then
/// PATH relative to `cwd`.
pub fn resolve_executable(program: &str, cwd: &std::path::Path) -> Result<PathBuf, HostError> {
    resolve_executable_from(
        program,
        cwd,
        std::env::current_exe().ok().as_deref(),
        &std::env::var_os("PATH").unwrap_or_default(),
    )
}

/// A discovery page request; an absent limit uses the default page size.
fn discovery_page_request(
    input: &Value,
) -> Result<(Option<crate::page::PageCursor>, Option<usize>), HostError> {
    let cursor = input
        .get("cursor")
        .map(|cursor| serde_json::from_value(cursor.clone()))
        .transpose()
        .map_err(|_| {
            HostError::before(
                "InvalidArguments",
                "cursor must be the next_cursor object returned by the previous page",
            )
        })?;
    let limit = match input.get("limit") {
        None => None,
        Some(limit) => Some(
            limit
                .as_u64()
                .and_then(|limit| usize::try_from(limit).ok())
                .filter(|limit| (1..=crate::page::MAX_PAGE_SIZE).contains(limit))
                .ok_or_else(|| {
                    HostError::before(
                        "InvalidArguments",
                        format!("limit must be between 1 and {}", crate::page::MAX_PAGE_SIZE),
                    )
                })?,
        ),
    };
    Ok((cursor, limit))
}

/// Instances sort by identity; panes whose identity is uncertain sort by Pane.
fn discovery_key(tick: &Value) -> String {
    match tick["tick_instance_id"].as_str() {
        Some(instance) => format!("instance:{instance}"),
        None => format!("pane:{}", tick["pane_id"].as_str().unwrap_or_default()),
    }
}

pub fn bundled_tick_executable() -> Option<PathBuf> {
    resolve_executable("tick", &std::env::current_dir().ok()?).ok()
}

fn resolve_executable_from(
    program: &str,
    cwd: &std::path::Path,
    agent_executable: Option<&std::path::Path>,
    search_path: &std::ffi::OsStr,
) -> Result<PathBuf, HostError> {
    let candidates = if program.contains('/') {
        vec![cwd.join(program)]
    } else {
        let companion = (program == "tick")
            .then(|| agent_executable.and_then(std::path::Path::parent))
            .flatten()
            .map(|directory| directory.join(program));
        companion
            .into_iter()
            .chain(
                std::env::split_paths(search_path)
                    .map(|directory| cwd.join(directory).join(program)),
            )
            .collect()
    };
    candidates
        .into_iter()
        .find(|path| {
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .and_then(|path| path.canonicalize().ok())
        .ok_or_else(|| {
            HostError::before(
                "InvalidArguments",
                format!("executable not found: {program}"),
            )
        })
}

fn executable_identity(path: &std::path::Path) -> Result<(u64, u64, i64, i64), HostError> {
    let metadata = path
        .metadata()
        .map_err(|error| HostError::before("TargetChanged", error.to_string()))?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    ))
}

pub fn observed_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

fn invalid_host_response(error: serde_json::Error) -> HostError {
    HostError::before(
        "ProtocolViolation",
        format!("invalid Herdr response: {error}"),
    )
}

fn session_error(error: SessionError) -> HostError {
    let code = match &error {
        SessionError::Protocol(_) => "ProtocolViolation",
        SessionError::Io(_) => "HostUnavailable",
        SessionError::Gone => "TargetGone",
        SessionError::Busy => "Busy",
        SessionError::CancelledBeforeSend => "Cancelled",
        SessionError::OutcomeUnknown => "OutcomeUnknown",
        SessionError::ChildError(_) => "TickToolFailed",
    };
    HostError {
        code: code.into(),
        message: error.to_string(),
        dispatched: matches!(
            error,
            SessionError::OutcomeUnknown | SessionError::ChildError(_)
        ),
    }
}

fn limit_terminal_text(text: &str) -> (String, bool) {
    let line_start = text
        .strip_suffix('\n')
        .unwrap_or(text)
        .match_indices('\n')
        .rev()
        .nth(TERMINAL_LINE_LIMIT - 1)
        .map(|(index, _)| index + 1)
        .unwrap_or(0);
    let mut start = line_start.max(text.len().saturating_sub(TERMINAL_TEXT_LIMIT));
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (text[start..].into(), start != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_tick_resolution_uses_its_installed_companion_before_path() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("installed");
        let path_bin = directory.path().join("path-bin");
        std::fs::create_dir(&installed).unwrap();
        std::fs::create_dir(&path_bin).unwrap();
        for executable in [installed.join("tick"), path_bin.join("tick")] {
            std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let agent = installed.join("tickloop");
        let resolve = |program| {
            resolve_executable_from(
                program,
                directory.path(),
                Some(&agent),
                path_bin.as_os_str(),
            )
            .unwrap()
        };

        assert_eq!(
            resolve("tick"),
            installed.join("tick").canonicalize().unwrap()
        );
        assert_eq!(
            resolve("path-bin/tick"),
            path_bin.join("tick").canonicalize().unwrap()
        );
        std::fs::remove_file(installed.join("tick")).unwrap();
        assert_eq!(
            resolve("tick"),
            path_bin.join("tick").canonicalize().unwrap()
        );
        std::fs::remove_file(path_bin.join("tick")).unwrap();
        assert!(
            resolve_executable_from("tick", directory.path(), Some(&agent), path_bin.as_os_str())
                .is_err()
        );
    }

    #[test]
    fn terminal_excerpt_keeps_the_latest_two_hundred_lines() {
        let lines: Vec<_> = (1..=201).map(|line| format!("line {line}")).collect();
        for trailing_newline in ["", "\n"] {
            let input = format!("{}{trailing_newline}", lines.join("\n"));
            let (text, truncated) = limit_terminal_text(&input);
            assert!(truncated);
            assert_eq!(text, format!("{}{trailing_newline}", lines[1..].join("\n")));
            assert_eq!(limit_terminal_text(&text), (text, false));
        }
    }

    #[test]
    fn terminal_excerpt_bounds_bytes_without_splitting_utf8() {
        let input = format!("{}latest", "界".repeat(30_000));
        let (text, truncated) = limit_terminal_text(&input);
        assert!(truncated);
        assert!(text.len() <= TERMINAL_TEXT_LIMIT);
        assert!(text.len() > TERMINAL_TEXT_LIMIT - "界".len());
        assert!(input.ends_with(&text));
        assert!(text.ends_with("latest"));
        assert_eq!(limit_terminal_text(&text), (text, false));
        let exact_limit = "x".repeat(TERMINAL_TEXT_LIMIT);
        assert_eq!(limit_terminal_text(&exact_limit), (exact_limit, false));
    }
}
