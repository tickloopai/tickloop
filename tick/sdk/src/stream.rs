//! One newline-delimited JSON-RPC stream per Tick Instance.
//!
//! A hosted Tick speaks Tick protocol 3 on real stdin/stdout. A standalone
//! terminal run has no protocol peer. Outbound traffic holds the latest update
//! plus bounded responses so a slow consumer does not block the visible UI.
//!
//! The stream answers `tick/describe` and `tick/read` itself as they arrive, from the
//! catalog and the latest published state, so a Tick still connecting or fetching its
//! first content is ready for its host at once and reads as null data until then. Only
//! `tick/call` waits for the runtime.

use std::{
    collections::VecDeque,
    env, io,
    sync::{Arc, Mutex},
};

use serde::{
    Deserializer as _,
    de::{IgnoredAny, MapAccess, Visitor},
};
use serde_json::{Value, json, value::RawValue};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Notify, mpsc},
    task::JoinHandle,
};

use tick_protocol::{
    CALL_METHOD, DESCRIBE_METHOD, FRAME_TOO_LARGE, INVALID_PARAMS, INVALID_REQUEST,
    JSON_RPC_VERSION, METHOD_NOT_FOUND, READ_METHOD, REVISION_MISMATCH, TOOL_FAILED,
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
    /// A request line over the request limit, with the bytes read of it up to that limit.
    TooLarge(Vec<u8>),
}

type RequestQueue = Arc<tokio::sync::Mutex<mpsc::Receiver<InboundFrame>>>;

pub struct Requests(Option<RequestQueue>);

impl Requests {
    /// Resolves with the next raw request line, or `None` once the peer closed
    /// the stream. A Tick without an injected channel waits forever.
    pub async fn next(&mut self) -> Option<InboundFrame> {
        match &self.0 {
            Some(queue) => queue.lock().await.recv().await,
            None => std::future::pending().await,
        }
    }
}

/// What the stream answers from without its runtime: the Tick's catalog and its latest state.
struct Published {
    tick: Box<dyn Describe>,
    /// The latest published state; its data is null until the first Tick Update.
    state: Mutex<TickState>,
}

impl Published {
    fn state(&self) -> std::sync::MutexGuard<'_, TickState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
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

    /// Answers `method` through `respond` when it is `tick/describe` or `tick/read`, which
    /// need nothing from the runtime; any other method is left to the caller. A read is
    /// answered under the state lock, so its answer is queued only after the update it reports.
    fn answer(&self, method: &str, id: Value, respond: impl FnOnce(Vec<u8>)) -> bool {
        match method {
            DESCRIBE_METHOD => {
                let answer = self.descriptor().and_then(|descriptor| {
                    serde_json::to_value(descriptor)
                        .map_err(|error| RpcError::new(TOOL_FAILED, error.to_string()))
                });
                respond(response_frame(id, answer));
                true
            }
            READ_METHOD => {
                let state = self.state();
                respond(response_frame(id, Ok(json!(*state))));
                true
            }
            _ => false,
        }
    }
}

pub struct TickStream {
    published: Arc<Published>,
    outbound: Outbound,
    requests: Option<RequestQueue>,
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
        self.published.state().seq
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
        // The update is offered under the state lock, so a read answered meanwhile never
        // reaches the host ahead of the update it reports.
        let mut state = self.published.state();
        if state.data == data {
            return Ok(());
        }
        let next = TickState {
            seq: state
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
                "params": next,
            }),
            tick_protocol::MAX_UPDATE_BYTES,
        )?;
        self.outbound.offer_update(frame);
        *state = next;
        Ok(())
    }

    /// Hands the inbound request lines to a runtime loop. Every call shares one queue, so a
    /// runtime that ends before the peer closed leaves the unread requests to the next one.
    pub fn take_requests(&mut self) -> Requests {
        Requests(self.requests.clone())
    }

    /// Answers everything this protocol version owns and surfaces only the
    /// `tick/call` requests the runtime has to dispatch. A request over the limit is refused
    /// under the `id` read from its leading bytes. A line naming no request `id` (not JSON,
    /// without an `id` or with a null one, or over the limit with its `id` unread) cannot be
    /// answered and is dropped without a trace.
    pub fn dispatch(&mut self, incoming: InboundFrame) -> Option<ToolRequest> {
        let line = match incoming {
            InboundFrame::Line(line) => line,
            InboundFrame::TooLarge(leading) => {
                if let Some(id) = leading_id(&leading) {
                    self.answer(
                        id,
                        Err(RpcError::new(
                            FRAME_TOO_LARGE,
                            format!(
                                "Tick request exceeds {} bytes",
                                tick_protocol::MAX_REQUEST_BYTES
                            ),
                        )),
                    );
                }
                return None;
            }
        };
        let frame: Value = serde_json::from_str(&line).ok()?;
        let id = frame.get("id").filter(|id| !id.is_null())?.clone();
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
        let Self {
            published,
            outbound,
            ..
        } = self;
        if published.answer(method, id.clone(), |frame| outbound.enqueue_response(frame)) {
            return None;
        }
        match method {
            CALL_METHOD => {
                let params = frame.get("params").cloned().unwrap_or(Value::Null);
                let checked = match (
                    params.get("revision").and_then(Value::as_str),
                    params.get("tool").and_then(Value::as_str),
                ) {
                    (Some(revision), Some(tool)) => match self.published.descriptor() {
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

    /// Sends the answer to request `id`. One too large to send becomes a failure of the request,
    /// which has already run by then.
    pub fn answer(&mut self, id: Value, result: Result<Value, RpcError>) {
        self.outbound.enqueue_response(response_frame(id, result));
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
        let mut stream = Self::with_outbound(
            tick,
            Outbound::Streamed {
                queue: Arc::clone(&queue),
                writer,
            },
        );
        stream.requests = Some(Arc::new(tokio::sync::Mutex::new(requests)));
        stream.reader = Some(tokio::spawn(read_requests(
            reader,
            sender,
            Arc::clone(&stream.published),
            queue,
        )));
        stream
    }

    fn detached(tick: impl Describe + 'static) -> Self {
        Self::with_outbound(tick, Outbound::Discarded)
    }

    fn stdio(tick: impl Describe + 'static) -> Self {
        Self::streamed(tick, tokio::io::stdin(), tokio::io::stdout())
    }

    fn with_outbound(tick: impl Describe + 'static, outbound: Outbound) -> Self {
        Self {
            published: Arc::new(Published {
                tick: Box::new(tick),
                state: Mutex::new(TickState::empty()),
            }),
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
            Self::Streamed { queue, .. } => queue.respond(frame),
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

    fn respond(&self, frame: Vec<u8>) {
        self.state().responses.push_back(frame);
        self.ready.notify_one();
    }
}

/// Writes the latest update before the responses queued with it: a read answered after that
/// update was published reports its seq, and must not reach the host ahead of it.
async fn drain_outbound<W: AsyncWrite + Unpin>(queue: Arc<OutboundQueue>, mut writer: W) {
    loop {
        let (frames, closed) = {
            let mut pending = queue.state();
            let mut frames: Vec<Vec<u8>> = pending.update.take().into_iter().collect();
            frames.extend(pending.responses.drain(..));
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

/// Reads request lines, answering `tick/describe` and `tick/read` at once and handing every
/// other line to the runtime.
async fn read_requests<R: AsyncRead + Unpin>(
    reader: R,
    sender: mpsc::Sender<InboundFrame>,
    published: Arc<Published>,
    queue: Arc<OutboundQueue>,
) {
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
        line.extend_from_slice(&bytes[..chunk.min(tick_protocol::MAX_REQUEST_BYTES - line.len())]);
        reader.consume(chunk);
        if oversized {
            let leading = std::mem::take(&mut line);
            if sender.send(InboundFrame::TooLarge(leading)).await.is_err() {
                return;
            }
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
            if answer_at_once(&published, &queue, &text) {
                continue;
            }
            if sender.send(InboundFrame::Line(text)).await.is_err() {
                return;
            }
        }
    }
}

/// The non-null `id` of a request from the leading bytes of its line. Members are read in order
/// up to the `id`, so a line cut off at the request limit still names it when it comes first, as
/// a host encodes it. The cut may shorten a number, so the `id` counts only when a delimiter
/// follows it within the bytes.
fn leading_id(leading: &[u8]) -> Option<Value> {
    struct IdMember<'a, 'de>(&'a mut Option<&'de RawValue>);

    impl<'de> Visitor<'de> for IdMember<'_, 'de> {
        type Value = ();

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a JSON-RPC request object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut members: A) -> Result<(), A::Error> {
            while let Some(name) = members.next_key::<String>()? {
                if name == "id" {
                    *self.0 = Some(members.next_value()?);
                    return Ok(());
                }
                members.next_value::<IgnoredAny>()?;
            }
            Ok(())
        }
    }

    let mut id = None;
    // Stopping at the `id`, or the cut, leaves the object unclosed and the parse fails; the
    // `id` read before that is kept.
    let _ = serde_json::Deserializer::from_slice(leading).deserialize_map(IdMember(&mut id));
    let id = id?.get();
    // The raw `id` borrows from `leading`, so its end is where its bytes end within them.
    let end = id.as_ptr() as usize - leading.as_ptr() as usize + id.len();
    let delimited = leading
        .get(end)
        .is_some_and(|byte| matches!(byte, b',' | b'}' | b' ' | b'\t' | b'\n' | b'\r'));
    serde_json::from_str::<Value>(id)
        .ok()
        .filter(|id| delimited && !id.is_null())
}

/// Answers a well-formed `tick/describe` or `tick/read` request line; anything else, including
/// a malformed line, is left for the runtime's dispatch to answer.
fn answer_at_once(published: &Published, queue: &OutboundQueue, line: &str) -> bool {
    let Ok(frame) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    if frame.get("jsonrpc").and_then(Value::as_str) != Some(JSON_RPC_VERSION) {
        return false;
    }
    let Some(method) = frame.get("method").and_then(Value::as_str) else {
        return false;
    };
    let Some(id) = frame.get("id").filter(|id| !id.is_null()).cloned() else {
        return true;
    };
    published.answer(method, id, |answer| queue.respond(answer))
}

fn response_frame(id: Value, result: Result<Value, RpcError>) -> Vec<u8> {
    let frame = match result {
        Ok(value) => json!({"jsonrpc": JSON_RPC_VERSION, "id": id.clone(), "result": value}),
        Err(error) => error.frame(id.clone()),
    };
    encode_limited(&frame, tick_protocol::MAX_TOOL_RESPONSE_BYTES).unwrap_or_else(|_| {
        encode(&RpcError::new(TOOL_FAILED, "Tick response too large").frame(id))
            .expect("bounded error frame")
    })
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
