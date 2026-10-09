//! One newline-delimited JSON-RPC stream per Tick Instance.
//!
//! A hosted Tick uses real stdin/stdout for protocol 2 and update 3. A standalone
//! terminal run has no protocol peer. Outbound traffic holds the latest update
//! plus bounded responses so a slow consumer does not block the visible UI.

use std::{
    collections::VecDeque,
    env,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Notify, mpsc},
    task::JoinHandle,
};

use crate::{
    error::{Error, Result},
    metadata::TickKind,
};
use tick_connect::protocol::{self, TickDescriptor as NativeDescriptor, TickRead as NativeRead};

pub const LONGBRIDGE_SOURCE: &str = "longbridge";
pub const FUTU_SOURCE: &str = "futu";
pub const DOCUMENT_SOURCE: &str = "documents";
pub const DEMO_SOURCE: &str = "demo";

pub const UPDATE_NOTIFICATION: &str = "tick/update";
pub const DESCRIBE_METHOD: &str = "tick/describe";
pub const TOOL_CALL_METHOD: &str = "tool/call";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const TOOL_FAILED: i64 = -32000;
pub const VERSION_MISMATCH: i64 = -32001;
pub const INSTANCE_MISMATCH: i64 = -32002;
pub const FRAME_TOO_LARGE: i64 = -32003;

const JSON_RPC_VERSION: &str = "2.0";
const INBOUND_CAPACITY: usize = 64;

/// A `tool/call` request the runtime still has to answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRequest {
    pub id: Value,
    pub tool: String,
    pub args: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn unknown_tool(tool: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("unknown Tick Tool {tool:?}"))
    }

    fn frame(&self, id: Value) -> Value {
        let mut error = json!({"code": self.code, "message": self.message});
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        json!({"jsonrpc": JSON_RPC_VERSION, "id": id, "error": error})
    }
}

/// A `tool/call` request frame sent to a Tick Instance.
pub fn tool_call_frame(id: Value, tool: &str, args: Value) -> Value {
    json!({
        "jsonrpc": JSON_RPC_VERSION,
        "id": id,
        "method": TOOL_CALL_METHOD,
        "params": {"tool": tool, "args": args},
    })
}

/// The Tick Update records an in-memory [`TickStream`] collects. The handle can be
/// read while the TickStream itself is borrowed by a running Tick.
#[derive(Clone, Default)]
pub struct Records(Arc<Mutex<Vec<Vec<u8>>>>);

impl Records {
    pub fn all(&self) -> Vec<Value> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter_map(|frame| serde_json::from_slice::<Value>(frame).ok())
            .filter(|frame| frame["method"] == UPDATE_NOTIFICATION)
            .map(|frame| frame["params"].clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.all().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn push(&self, frame: Vec<u8>) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(frame);
    }
}

/// Inbound requests, detached from the [`TickStream`] so that a runtime loop can
/// await them in the same `select!` that publishes updates.
pub enum InboundFrame {
    Line(String),
    TooLarge,
}

pub struct Requests(Option<mpsc::Receiver<InboundFrame>>);

impl Requests {
    /// Resolves with the next raw request line, or `None` once the peer closed
    /// the stream. A Tick without an injected channel waits forever.
    pub async fn next(&mut self) -> Option<InboundFrame> {
        match &mut self.0 {
            Some(receiver) => receiver.recv().await,
            None => std::future::pending().await,
        }
    }
}

pub struct TickStream {
    tick: TickKind,
    source: &'static str,
    native_instance_id: Option<String>,
    sequence: u64,
    previous: Option<Value>,
    outbound: Outbound,
    requests: Option<mpsc::Receiver<InboundFrame>>,
    reader: Option<JoinHandle<()>>,
}

impl TickStream {
    /// Uses protocol stdio only for a hosted Tick; standalone runs have no peer.
    pub fn from_process(tick: TickKind, source: &'static str) -> Result<Self> {
        let native_instance_id = environment_value(protocol::INSTANCE_ID_ENV)?;
        if let Some(native_instance_id) = native_instance_id {
            let launched = native_instance_id
                .parse::<tick_connect::instance::InstanceRef>()
                .is_ok_and(|instance| matches!(instance.tick_type(), "herdr" | "hosted"));
            if !launched {
                return Err(Error::usage(format!(
                    "{} needs a complete herdr: or hosted: instance identity",
                    protocol::INSTANCE_ID_ENV
                )));
            }
            let mut stream = Self::stdio(tick, source);
            stream.native_instance_id = Some(native_instance_id);
            return Ok(stream);
        }
        Ok(Self::detached(tick, source))
    }

    pub fn duplex(
        tick: TickKind,
        source: &'static str,
        instance_id: String,
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        let mut stream = Self::streamed(tick, source, reader, writer);
        stream.native_instance_id = Some(instance_id);
        stream
    }

    /// Keeps every published record in memory for tests and one-shot output.
    pub fn in_memory(tick: TickKind) -> Self {
        Self::in_memory_with_source(tick, LONGBRIDGE_SOURCE)
    }

    pub fn in_memory_with_source(tick: TickKind, source: &'static str) -> Self {
        Self {
            tick,
            source,
            native_instance_id: Some(format!("herdr:test-{}", generated_tick_instance_id())),
            sequence: 0,
            previous: None,
            outbound: Outbound::Buffered(Records::default()),
            requests: None,
            reader: None,
        }
    }

    pub fn is_native(&self) -> bool {
        self.native_instance_id.is_some()
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The Tick Update records published so far; only in-memory Sessions keep them.
    pub fn records(&self) -> Records {
        match &self.outbound {
            Outbound::Buffered(records) => records.clone(),
            Outbound::Streamed { .. } | Outbound::Discarded => Records::default(),
        }
    }

    /// Enqueues the next Tick Update when `data` differs from the last one.
    /// The sequence is assigned here, so a replaced slot leaves a gap equal to
    /// the number of dropped updates.
    pub fn publish(&mut self, data: Value) -> Result<()> {
        if self.previous.as_ref() == Some(&data) {
            return Ok(());
        }
        let kind = if self.sequence == 0 {
            "snapshot"
        } else {
            "update"
        };
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::usage("Tick Update sequence exhausted"))?;
        let observed_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| Error::usage(error.to_string()))?;
        let record = json!({
            "version": protocol::NATIVE_UPDATE_VERSION,
            "tick": self.tick.name(),
            "tick_instance_id": self.native_instance_id,
            "source": self.source,
            "sequence": sequence,
            "kind": kind,
            "observed_at": observed_at,
            "data": data,
        });
        let frame = encode_limited(
            &json!({
                "jsonrpc": JSON_RPC_VERSION,
                "method": UPDATE_NOTIFICATION,
                "params": record,
            }),
            protocol::MAX_UPDATE_BYTES,
        )?;
        self.outbound.offer_update(frame);
        self.sequence = sequence;
        self.previous = Some(data);
        Ok(())
    }

    /// Hands the inbound request lines to the runtime loop; later calls wait forever.
    pub fn take_requests(&mut self) -> Requests {
        Requests(self.requests.take())
    }

    /// Answers everything this protocol version owns and surfaces only the
    /// `tool/call` requests the runtime has to dispatch.
    pub fn dispatch(&mut self, incoming: InboundFrame) -> Option<ToolRequest> {
        let line = match incoming {
            InboundFrame::Line(line) => line,
            InboundFrame::TooLarge => {
                self.answer(
                    Value::Null,
                    Err(RpcError::new(
                        FRAME_TOO_LARGE,
                        format!("Tick request exceeds {} bytes", protocol::MAX_REQUEST_BYTES),
                    )),
                );
                return None;
            }
        };
        if line.len() > protocol::MAX_REQUEST_BYTES {
            self.answer(
                Value::Null,
                Err(RpcError::new(FRAME_TOO_LARGE, "Tick request too large")),
            );
            return None;
        }
        let frame: Value = match serde_json::from_str(&line) {
            Ok(frame) => frame,
            Err(error) => {
                self.answer(
                    Value::Null,
                    Err(RpcError::new(PARSE_ERROR, error.to_string())),
                );
                return None;
            }
        };
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        if frame.get("jsonrpc").and_then(Value::as_str) != Some(JSON_RPC_VERSION) {
            self.answer(
                id,
                Err(RpcError::new(
                    VERSION_MISMATCH,
                    "unsupported JSON-RPC version",
                )),
            );
            return None;
        }
        if let Some(protocol) = frame.pointer("/params/protocol").and_then(Value::as_u64)
            && protocol != u64::from(protocol::NATIVE_PROTOCOL_VERSION)
        {
            self.answer(
                id,
                Err(RpcError::new(
                    VERSION_MISMATCH,
                    "unsupported Tick protocol version",
                )),
            );
            return None;
        }
        if let Some(instance) = frame
            .pointer("/params/tick_instance_id")
            .and_then(Value::as_str)
            && Some(instance) != self.native_instance_id.as_deref()
        {
            self.answer(
                id,
                Err(RpcError::new(
                    INSTANCE_MISMATCH,
                    "Tick instance identity mismatch",
                )),
            );
            return None;
        }
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            self.answer(
                id,
                Err(RpcError::new(INVALID_REQUEST, "request has no method")),
            );
            return None;
        };
        match method {
            DESCRIBE_METHOD => {
                let answer = self.descriptor();
                self.answer(id, answer);
                None
            }
            protocol::READ_METHOD => {
                let observed_at = observed_at();
                let result = observed_at
                    .map(|observed_at| {
                        json!(NativeRead {
                            version: protocol::NATIVE_UPDATE_VERSION,
                            tick_instance_id: self
                                .native_instance_id
                                .clone()
                                .expect("native identity"),
                            initialized: self.previous.is_some(),
                            sequence: self.sequence,
                            observed_at,
                            data: self.previous.clone(),
                        })
                    })
                    .map_err(|error| RpcError::new(TOOL_FAILED, error.message));
                self.answer(id, result);
                None
            }
            TOOL_CALL_METHOD => {
                let params = frame.get("params").cloned().unwrap_or(Value::Null);
                if let Some(expected) = params
                    .get("expected_descriptor_digest")
                    .and_then(Value::as_str)
                {
                    let current = self.descriptor().and_then(|value| {
                        serde_json::from_value::<NativeDescriptor>(value)
                            .map_err(|error| RpcError::new(TOOL_FAILED, error.to_string()))
                    });
                    match current {
                        Ok(descriptor) if protocol::descriptor_digest(&descriptor) == expected => {}
                        Ok(_) => {
                            self.answer(
                                id,
                                Err(RpcError::new(
                                    INVALID_REQUEST,
                                    "Tick Tool catalog changed before dispatch",
                                )),
                            );
                            return None;
                        }
                        Err(error) => {
                            self.answer(id, Err(error));
                            return None;
                        }
                    }
                }
                match params.get("tool").and_then(Value::as_str) {
                    Some(tool) => Some(ToolRequest {
                        id,
                        tool: tool.to_owned(),
                        args: params.get("args").cloned().unwrap_or(json!({})),
                    }),
                    None => {
                        self.answer(
                            id,
                            Err(RpcError::new(
                                INVALID_PARAMS,
                                format!("{TOOL_CALL_METHOD} requires a tool name"),
                            )),
                        );
                        None
                    }
                }
            }
            unknown => {
                self.answer(
                    id,
                    Err(RpcError::new(
                        METHOD_NOT_FOUND,
                        format!("unknown method {unknown:?}"),
                    )),
                );
                None
            }
        }
    }

    pub fn answer(&mut self, id: Value, result: std::result::Result<Value, RpcError>) {
        let frame = match result {
            Ok(value) => json!({"jsonrpc": JSON_RPC_VERSION, "id": id.clone(), "result": value}),
            Err(error) => error.frame(id.clone()),
        };
        let frame =
            encode_limited(&frame, protocol::MAX_TOOL_RESPONSE_BYTES).unwrap_or_else(|_| {
                encode(&RpcError::new(FRAME_TOO_LARGE, "Tick response too large").frame(id))
                    .expect("bounded error frame")
            });
        self.outbound.enqueue_response(frame);
    }

    fn descriptor(&self) -> std::result::Result<Value, RpcError> {
        let descriptor = self.tick.descriptor();
        descriptor
            .validate()
            .map_err(|error| RpcError::new(TOOL_FAILED, error.message))?;
        let native = NativeDescriptor {
            protocol: protocol::NATIVE_PROTOCOL_VERSION,
            tick_type: self.tick.name().to_owned(),
            title: descriptor.title.to_owned(),
            description: descriptor.description.to_owned(),
            recover_with: descriptor.recover_with.map(str::to_owned),
            tools: descriptor
                .tools
                .into_iter()
                .map(|tool| protocol::TickTool {
                    name: tool.name.into_owned(),
                    description: tool.description.into_owned(),
                    params: tool.params,
                    mutates: tool.mutates,
                })
                .collect(),
        };
        native
            .validate()
            .map_err(|error| RpcError::new(INVALID_REQUEST, error))?;
        let value = serde_json::to_value(native)
            .map_err(|error| RpcError::new(TOOL_FAILED, error.to_string()))?;
        if serde_json::to_vec(&value)
            .map_or(true, |bytes| bytes.len() > protocol::MAX_DESCRIPTION_BYTES)
        {
            return Err(RpcError::new(FRAME_TOO_LARGE, "Tick description too large"));
        }
        Ok(value)
    }

    /// Flushes whatever is still queued and stops the transport tasks.
    pub async fn close(mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        self.outbound.close().await;
    }

    fn streamed(
        tick: TickKind,
        source: &'static str,
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        let queue = Arc::new(OutboundQueue::default());
        let writer = tokio::spawn(drain_outbound(Arc::clone(&queue), writer));
        let (sender, requests) = mpsc::channel(INBOUND_CAPACITY);
        let reader = tokio::spawn(read_requests(reader, sender));
        Self {
            tick,
            source,
            native_instance_id: None,
            sequence: 0,
            previous: None,
            outbound: Outbound::Streamed { queue, writer },
            requests: Some(requests),
            reader: Some(reader),
        }
    }

    fn detached(tick: TickKind, source: &'static str) -> Self {
        Self {
            tick,
            source,
            native_instance_id: None,
            sequence: 0,
            previous: None,
            outbound: Outbound::Discarded,
            requests: None,
            reader: None,
        }
    }

    fn stdio(tick: TickKind, source: &'static str) -> Self {
        Self::streamed(tick, source, tokio::io::stdin(), tokio::io::stdout())
    }
}

enum Outbound {
    Streamed {
        queue: Arc<OutboundQueue>,
        writer: JoinHandle<()>,
    },
    Buffered(Records),
    /// A standalone terminal run: nothing reads the stream, so nothing is kept.
    Discarded,
}

impl Outbound {
    fn offer_update(&mut self, frame: Vec<u8>) {
        match self {
            Self::Streamed { queue, .. } => {
                queue.state().update = Some(frame);
                queue.ready.notify_one();
            }
            Self::Buffered(records) => records.push(frame),
            Self::Discarded => (),
        }
    }

    fn enqueue_response(&mut self, frame: Vec<u8>) {
        match self {
            Self::Streamed { queue, .. } => {
                queue.state().responses.push_back(frame);
                queue.ready.notify_one();
            }
            Self::Buffered(records) => records.push(frame),
            Self::Discarded => (),
        }
    }

    async fn close(&mut self) {
        let Self::Streamed { queue, writer } = self else {
            return;
        };
        queue.state().closed = true;
        queue.ready.notify_one();
        let _ = writer.await;
    }
}

#[derive(Default)]
struct OutboundQueue {
    pending: Mutex<Pending>,
    ready: Notify,
}

#[derive(Default)]
struct Pending {
    update: Option<Vec<u8>>,
    responses: VecDeque<Vec<u8>>,
    closed: bool,
}

impl OutboundQueue {
    fn state(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

async fn drain_outbound<W: AsyncWrite + Unpin>(queue: Arc<OutboundQueue>, mut writer: W) {
    loop {
        let (frames, closed) = {
            let mut pending = queue.state();
            let mut frames: Vec<Vec<u8>> = pending.responses.drain(..).collect();
            frames.extend(pending.update.take());
            (frames, pending.closed)
        };
        if frames.is_empty() {
            if closed {
                return;
            }
            queue.ready.notified().await;
            continue;
        }
        for frame in frames {
            if writer.write_all(&frame).await.is_err() {
                return;
            }
        }
        if writer.flush().await.is_err() {
            return;
        }
    }
}

async fn read_requests<R: AsyncRead + Unpin>(reader: R, sender: mpsc::Sender<InboundFrame>) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        let bytes = match reader.fill_buf().await {
            Ok(bytes) => bytes,
            Err(_) => return,
        };
        if bytes.is_empty() {
            return;
        }
        let chunk = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1);
        let complete = bytes[..chunk].last() == Some(&b'\n');
        let oversized = line.len().saturating_add(chunk) > protocol::MAX_REQUEST_BYTES;
        if !oversized {
            line.extend_from_slice(&bytes[..chunk]);
        }
        reader.consume(chunk);
        if oversized {
            if sender.send(InboundFrame::TooLarge).await.is_err() {
                return;
            }
            line.clear();
            if !complete {
                loop {
                    let bytes = match reader.fill_buf().await {
                        Ok(bytes) => bytes,
                        Err(_) => return,
                    };
                    if bytes.is_empty() {
                        return;
                    }
                    let chunk = bytes
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .map_or(bytes.len(), |index| index + 1);
                    let complete = bytes[..chunk].last() == Some(&b'\n');
                    reader.consume(chunk);
                    if complete {
                        break;
                    }
                }
            }
            continue;
        }
        if complete {
            let text = String::from_utf8(std::mem::take(&mut line)).unwrap_or_default();
            if sender.send(InboundFrame::Line(text)).await.is_err() {
                return;
            }
        }
    }
}

fn encode(frame: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(frame)
        .map_err(|error| Error::usage(format!("Tick stream output failed: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn encode_limited(frame: &Value, maximum: usize) -> Result<Vec<u8>> {
    let bytes = encode(frame)?;
    if bytes.len() > maximum {
        return Err(Error::usage(format!("Tick frame exceeds {maximum} bytes")));
    }
    Ok(bytes)
}

fn observed_at() -> Result<String> {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|error| Error::usage(error.to_string()))
}

fn environment_value(name: &str) -> Result<Option<String>> {
    env::var_os(name)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| Error::usage(format!("{name} must be valid UTF-8")))
        })
        .transpose()
}

fn generated_tick_instance_id() -> u64 {
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    u64::try_from(started).unwrap_or(u64::MAX) ^ (u64::from(std::process::id()) << 32)
}
