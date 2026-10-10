//! One newline-delimited JSON-RPC stream per Tick Instance.
//!
//! A hosted Tick speaks Tick protocol 3 on real stdin/stdout. A standalone
//! terminal run has no protocol peer. Outbound traffic holds the latest update
//! plus bounded responses so a slow consumer does not block the visible UI.

use std::{
    collections::VecDeque,
    env, io,
    sync::{Arc, Mutex},
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Notify, mpsc},
    task::JoinHandle,
};

use tick_protocol::{
    CALL_METHOD, DESCRIBE_METHOD, FRAME_TOO_LARGE, INVALID_PARAMS, INVALID_REQUEST,
    JSON_RPC_VERSION, METHOD_NOT_FOUND, PARSE_ERROR, READ_METHOD, REVISION_MISMATCH, TOOL_FAILED,
    TickDescriptor, TickState, UPDATE_METHOD, VERSION_MISMATCH,
};

const INBOUND_CAPACITY: usize = 64;

/// What a Tick tells its stream about itself.
pub trait Describe: Send + Sync {
    /// What `tick/describe` answers; the error says why the Tick cannot declare itself. Its
    /// `revision` changes whenever the catalog does, and a `tick/call` naming another revision
    /// is refused before it reaches the runtime.
    fn descriptor(&self) -> Result<TickDescriptor, String>;
}

/// A `tick/call` request the runtime still has to answer.
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
        Self::new(INVALID_PARAMS, format!("unknown Tick Tool {tool:?}"))
    }

    fn frame(&self, id: Value) -> Value {
        let mut error = json!({"code": self.code, "message": self.message});
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        json!({"jsonrpc": JSON_RPC_VERSION, "id": id, "error": error})
    }
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
            .filter(|frame| frame["method"] == UPDATE_METHOD)
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
    tick: Box<dyn Describe>,
    /// The latest published state; its data is null until the first Tick Update.
    state: TickState,
    outbound: Outbound,
    requests: Option<mpsc::Receiver<InboundFrame>>,
    reader: Option<JoinHandle<()>>,
}

impl TickStream {
    /// Uses protocol stdio only for a Tick a host launched; standalone runs have no peer.
    pub fn from_process(tick: impl Describe + 'static) -> io::Result<Self> {
        match environment_value(tick_protocol::INSTANCE_ID_ENV)? {
            Some(instance_id) if !tick_protocol::is_launched_instance_id(&instance_id) => {
                Err(io::Error::other(format!(
                    "{} needs a complete hosted: instance identity",
                    tick_protocol::INSTANCE_ID_ENV
                )))
            }
            Some(_) => Ok(Self::stdio(tick)),
            None => Ok(Self::detached(tick)),
        }
    }

    pub fn duplex(
        tick: impl Describe + 'static,
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        Self::streamed(tick, reader, writer)
    }

    /// Keeps every published record in memory for tests and one-shot output.
    pub fn in_memory(tick: impl Describe + 'static) -> Self {
        Self::with_outbound(tick, Outbound::Buffered(Records::default()))
    }

    /// Whether something reads this stream: a host, or the in-memory records.
    pub fn is_native(&self) -> bool {
        !matches!(self.outbound, Outbound::Discarded)
    }

    pub fn seq(&self) -> u64 {
        self.state.seq
    }

    /// The Tick Update records published so far; only in-memory Sessions keep them.
    pub fn records(&self) -> Records {
        match &self.outbound {
            Outbound::Buffered(records) => records.clone(),
            Outbound::Streamed { .. } | Outbound::Discarded => Records::default(),
        }
    }

    /// Enqueues the next Tick Update when `data` differs from the last one.
    /// The seq is assigned here, so a replaced slot leaves a gap equal to
    /// the number of dropped updates.
    pub fn publish(&mut self, data: Value) -> io::Result<()> {
        if data.is_null() {
            return Err(io::Error::other("a Tick Update needs data"));
        }
        if self.state.data == data {
            return Ok(());
        }
        let state = TickState {
            seq: self
                .state
                .seq
                .checked_add(1)
                .ok_or_else(|| io::Error::other("Tick Update seq exhausted"))?,
            data,
            at: Some(tick_protocol::now_rfc3339()),
        };
        let frame = encode_limited(
            &json!({
                "jsonrpc": JSON_RPC_VERSION,
                "method": UPDATE_METHOD,
                "params": state,
            }),
            tick_protocol::MAX_UPDATE_BYTES,
        )?;
        self.outbound.offer_update(frame);
        self.state = state;
        Ok(())
    }

    /// Hands the inbound request lines to the runtime loop; later calls wait forever.
    pub fn take_requests(&mut self) -> Requests {
        Requests(self.requests.take())
    }

    /// Answers everything this protocol version owns and surfaces only the
    /// `tick/call` requests the runtime has to dispatch.
    pub fn dispatch(&mut self, incoming: InboundFrame) -> Option<ToolRequest> {
        let line = match incoming {
            InboundFrame::Line(line) => line,
            InboundFrame::TooLarge => {
                self.answer(
                    Value::Null,
                    Err(RpcError::new(
                        FRAME_TOO_LARGE,
                        format!(
                            "Tick request exceeds {} bytes",
                            tick_protocol::MAX_REQUEST_BYTES
                        ),
                    )),
                );
                return None;
            }
        };
        if line.len() > tick_protocol::MAX_REQUEST_BYTES {
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
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            self.answer(
                id,
                Err(RpcError::new(INVALID_REQUEST, "request has no method")),
            );
            return None;
        };
        match method {
            DESCRIBE_METHOD => {
                let answer = self.descriptor().and_then(|descriptor| {
                    serde_json::to_value(descriptor)
                        .map_err(|error| RpcError::new(TOOL_FAILED, error.to_string()))
                });
                self.answer(id, answer);
                None
            }
            READ_METHOD => {
                let state = json!(self.state);
                self.answer(id, Ok(state));
                None
            }
            CALL_METHOD => {
                let params = frame.get("params").cloned().unwrap_or(Value::Null);
                let checked = match (
                    params.get("revision").and_then(Value::as_str),
                    params.get("tool").and_then(Value::as_str),
                ) {
                    (Some(revision), Some(tool)) => match self.descriptor() {
                        Ok(current) if current.revision == revision => Ok(tool),
                        Ok(_) => Err(RpcError::new(
                            REVISION_MISMATCH,
                            "the Tick Tool catalog changed; describe the Tick and prepare the call again",
                        )),
                        Err(error) => Err(error),
                    },
                    _ => Err(RpcError::new(
                        INVALID_PARAMS,
                        format!("{CALL_METHOD} requires a tool name and the catalog revision"),
                    )),
                };
                match checked {
                    Ok(tool) => Some(ToolRequest {
                        id,
                        tool: tool.to_owned(),
                        args: params.get("args").cloned().unwrap_or(json!({})),
                    }),
                    Err(error) => {
                        self.answer(id, Err(error));
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

    pub fn answer(&mut self, id: Value, result: Result<Value, RpcError>) {
        let frame = match result {
            Ok(value) => json!({"jsonrpc": JSON_RPC_VERSION, "id": id.clone(), "result": value}),
            Err(error) => error.frame(id.clone()),
        };
        let frame =
            encode_limited(&frame, tick_protocol::MAX_TOOL_RESPONSE_BYTES).unwrap_or_else(|_| {
                encode(&RpcError::new(FRAME_TOO_LARGE, "Tick response too large").frame(id))
                    .expect("bounded error frame")
            });
        self.outbound.enqueue_response(frame);
    }

    fn descriptor(&self) -> Result<TickDescriptor, RpcError> {
        let descriptor = self
            .tick
            .descriptor()
            .map_err(|error| RpcError::new(TOOL_FAILED, error))?;
        descriptor
            .validate()
            .map_err(|error| RpcError::new(INVALID_REQUEST, error))?;
        tick_protocol::check_encoded_size(
            &descriptor,
            tick_protocol::MAX_DESCRIPTION_BYTES,
            "Tick description",
        )
        .map_err(|error| RpcError::new(FRAME_TOO_LARGE, error))?;
        Ok(descriptor)
    }

    /// Flushes whatever is still queued and stops the transport tasks.
    pub async fn close(mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        self.outbound.close().await;
    }

    fn streamed(
        tick: impl Describe + 'static,
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        let queue = Arc::new(OutboundQueue::default());
        let writer = tokio::spawn(drain_outbound(Arc::clone(&queue), writer));
        let (sender, requests) = mpsc::channel(INBOUND_CAPACITY);
        let reader = tokio::spawn(read_requests(reader, sender));
        Self {
            requests: Some(requests),
            reader: Some(reader),
            ..Self::with_outbound(tick, Outbound::Streamed { queue, writer })
        }
    }

    fn detached(tick: impl Describe + 'static) -> Self {
        Self::with_outbound(tick, Outbound::Discarded)
    }

    fn stdio(tick: impl Describe + 'static) -> Self {
        Self::streamed(tick, tokio::io::stdin(), tokio::io::stdout())
    }

    fn with_outbound(tick: impl Describe + 'static, outbound: Outbound) -> Self {
        Self {
            tick: Box::new(tick),
            state: TickState::empty(),
            outbound,
            requests: None,
            reader: None,
        }
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
        let oversized = line.len().saturating_add(chunk) > tick_protocol::MAX_REQUEST_BYTES;
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

fn encode(frame: &Value) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(frame)
        .map_err(|error| io::Error::other(format!("Tick stream output failed: {error}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn encode_limited(frame: &Value, maximum: usize) -> io::Result<Vec<u8>> {
    let bytes = encode(frame)?;
    if bytes.len() > maximum {
        return Err(io::Error::other(format!(
            "Tick frame exceeds {maximum} bytes"
        )));
    }
    Ok(bytes)
}

fn environment_value(name: &str) -> io::Result<Option<String>> {
    env::var_os(name)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| io::Error::other(format!("{name} must be valid UTF-8")))
        })
        .transpose()
}
