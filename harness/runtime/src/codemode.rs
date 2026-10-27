//! Executes model-authored JavaScript with tool calls as its only host capability.
mod output;
use base64::Engine;
use futures_util::{StreamExt, stream::FuturesUnordered};
use rquickjs::{Context, Function, Object, Runtime};
use serde::Deserialize;
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    future::Future,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const MAX_CONCURRENT_CALLS: usize = 10;
const MEMORY_LIMIT: usize = 256 * 1024 * 1024;
const INTERRUPT_INTERVAL: Duration = Duration::from_millis(10);
const SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct Control {
    clock: Arc<Mutex<Clock>>,
    pub cancel: CancellationToken,
}
struct Clock {
    started: Instant,
    paused: Duration,
    pause_started: Option<Instant>,
    waits: usize,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            clock: Arc::new(Mutex::new(Clock {
                started: Instant::now(),
                paused: Duration::ZERO,
                pause_started: None,
                waits: 0,
            })),
            cancel: CancellationToken::new(),
        }
    }
}
impl Control {
    pub fn pause(&self) -> Pause {
        let mut clock = self.clock.lock().unwrap();
        if clock.waits == 0 {
            clock.pause_started = Some(Instant::now());
        }
        clock.waits += 1;
        Pause(self.clone())
    }
    fn elapsed(&self) -> Duration {
        let clock = self.clock.lock().unwrap();
        clock
            .pause_started
            .unwrap_or_else(Instant::now)
            .duration_since(clock.started)
            .saturating_sub(clock.paused)
    }
}
pub struct Pause(Control);
impl Drop for Pause {
    fn drop(&mut self) {
        let mut clock = self.0.clock.lock().unwrap();
        clock.waits -= 1;
        if clock.waits == 0 {
            let elapsed = clock.pause_started.take().unwrap().elapsed();
            clock.paused += elapsed;
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceOptions {
    max_output_tokens: Option<usize>,
    timeout_ms: Option<u64>,
}

pub struct Options {
    pub control: Control,
    pub output_directory: Option<std::path::PathBuf>,
    /// A host may lower the sandbox budget; values above 256 MB are capped.
    pub memory_limit_bytes: usize,
    /// The values `load()` returns when the script starts.
    pub store: BTreeMap<String, Value>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            control: Control::default(),
            output_directory: None,
            memory_limit_bytes: MEMORY_LIMIT,
            store: BTreeMap::new(),
        }
    }
}

#[derive(Default)]
pub struct Outcome {
    pub output: String,
    pub error: Option<String>,
    pub full_output_path: Option<std::path::PathBuf>,
    pub images: Vec<tickloop_harness_protocol::Image>,
    /// What `store()` changed; empty unless the script succeeded.
    pub store_writes: StoreWrites,
}

/// The keys a successful script set and the keys it deleted with `store(key, undefined)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoreWrites {
    pub set: BTreeMap<String, Value>,
    pub delete: BTreeSet<String>,
}

impl StoreWrites {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.delete.is_empty()
    }
    pub fn apply_to(&self, store: &mut BTreeMap<String, Value>) {
        for key in &self.delete {
            store.remove(key);
        }
        store.extend(self.set.clone());
    }
    /// The writes the prelude reports: `[key, json]` sets a key and `[key]` deletes it.
    fn parse(json: &str) -> Result<Self, String> {
        let entries: Vec<Vec<String>> =
            serde_json::from_str(json).map_err(|error| error.to_string())?;
        let mut writes = Self::default();
        for entry in entries {
            match entry.as_slice() {
                [key] => {
                    writes.delete.insert(key.clone());
                }
                [key, value] => {
                    let value = serde_json::from_str(value).map_err(|error| error.to_string())?;
                    writes.set.insert(key.clone(), value);
                }
                _ => return Err("malformed store write".into()),
            }
        }
        Ok(writes)
    }
}

#[derive(Default)]
struct State {
    outcome: Outcome,
    finished: bool,
    pending_calls: usize,
    image_bytes: usize,
}
struct Limits {
    timeout: Duration,
    memory: usize,
}

struct Call {
    id: u32,
    name: String,
    input: Value,
}

pub async fn run<F, Fut>(source: &str, options: Options, tools: Vec<String>, call: F) -> Outcome
where
    F: Fn(String, Value, CancellationToken) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let parsed = if let Some(directive) = source
        .lines()
        .next()
        .unwrap_or_default()
        .trim_start()
        .strip_prefix("// @options:")
    {
        serde_json::from_str::<SourceOptions>(directive).map_err(|error| error.to_string())
    } else {
        Ok(SourceOptions::default())
    };
    let source_options = match parsed {
        Ok(value) if value.timeout_ms != Some(0) => value,
        Ok(_) => {
            return Outcome {
                error: Some("timeout_ms must be positive".into()),
                ..Default::default()
            };
        }
        Err(error) => {
            return Outcome {
                error: Some(format!(
                    "Invalid @options (timeout_ms, max_output_tokens): {error}"
                )),
                ..Default::default()
            };
        }
    };
    let memory_limit = options.memory_limit_bytes.min(MEMORY_LIMIT);
    let timeout = source_options
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT);
    let output_limit = source_options
        .max_output_tokens
        .unwrap_or(10_000)
        .saturating_mul(4)
        .min(memory_limit / 4);
    let output_directory = options.output_directory;
    let control = options.control;
    let store = serde_json::to_string(&options.store).unwrap();
    let _cancel_on_drop = control.cancel.clone().drop_guard();
    let worker_control = control.clone();
    let (requests, mut incoming) = tokio::sync::mpsc::channel(MAX_CONCURRENT_CALLS);
    let (responses, completed) = mpsc::channel();
    let source = source.to_owned();
    let mut worker = tokio::task::spawn_blocking(move || {
        execute(
            source,
            tools,
            store,
            requests,
            completed,
            worker_control,
            Limits {
                timeout,
                memory: memory_limit,
            },
            output::Output::new(output_directory, output_limit),
        )
    });
    let mut pending = FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;
            result = &mut worker => {
                control.cancel.cancel();
                let settled = tokio::time::timeout(SETTLEMENT_TIMEOUT, async { while pending.next().await.is_some() {} }).await;
                let mut outcome = result.unwrap_or_else(|error| Outcome {error:Some(error.to_string()), ..Default::default()});
                if settled.is_err() { outcome.error.get_or_insert_with(|| "Tool callbacks did not settle after cancellation; their outcomes are unknown".into()); }
                if outcome.error.is_some() { outcome.store_writes = StoreWrites::default(); }
                return outcome;
            },
            Some(request) = incoming.recv() => {
                let future = call(request.name, request.input, control.cancel.clone());
                pending.push(async move { (request.id, future.await) });
            }
            Some(result) = pending.next(), if !pending.is_empty() => { let _ = responses.send(result); }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute(
    source: String,
    tools: Vec<String>,
    store: String,
    requests: tokio::sync::mpsc::Sender<Call>,
    completed: mpsc::Receiver<(u32, Result<Value, String>)>,
    control: Control,
    limits: Limits,
    output: output::Output,
) -> Outcome {
    let Limits {
        timeout,
        memory: memory_limit,
    } = limits;
    let output = Rc::new(RefCell::new(output));
    let state = Rc::new(RefCell::new(State::default()));
    let result = (|| -> Result<(), String> {
        let runtime = Runtime::new().map_err(|error| error.to_string())?;
        runtime.set_memory_limit(memory_limit);
        let interrupted = control.clone();
        let finished = state.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            finished.borrow().finished
                || interrupted.cancel.is_cancelled()
                || interrupted.elapsed() >= timeout
        })));
        let context = Context::full(&runtime).map_err(|error| error.to_string())?;
        context
            .with(|ctx| -> rquickjs::Result<()> {
                let call_state = state.clone();
                let request =
                    Function::new(ctx.clone(), move |id: u32, name: String, input: String| {
                        let input = serde_json::from_str(&input).map_err(|error| {
                            rquickjs::Error::new_from_js_message(
                                "tool",
                                "arguments",
                                format!("Invalid tool JSON: {error}"),
                            )
                        })?;
                        call_state.borrow_mut().pending_calls += 1;
                        requests.try_send(Call { id, name, input }).map_err(|_| {
                            rquickjs::Error::new_from_js_message(
                                "tool",
                                "request",
                                "Tool bridge is unavailable",
                            )
                        })
                    })?;
                let output_state = output.clone();
                let output = Function::new(ctx.clone(), move |text: String| {
                    output_state.borrow_mut().push(&text).map_err(|error| {
                        rquickjs::Error::new_from_js_message("output", "text", error.to_string())
                    })
                })?;
                let done_state = state.clone();
                let done = Function::new(
                    ctx.clone(),
                    move |error: Option<String>, writes: Option<String>| {
                        let mut state = done_state.borrow_mut();
                        match writes.as_deref().map(StoreWrites::parse).transpose() {
                            Ok(writes) if error.is_none() => {
                                state.outcome.store_writes = writes.unwrap_or_default();
                            }
                            Ok(_) => {}
                            Err(invalid) => {
                                state.outcome.error =
                                    Some(format!("Invalid store writes: {invalid}"));
                                state.finished = true;
                                return;
                            }
                        }
                        state.outcome.error = error;
                        state.finished = true;
                    },
                )?;
                let image_state = state.clone();
                let image =
                    Function::new(ctx.clone(), move |json: String| -> rquickjs::Result<()> {
                        let value: Value = serde_json::from_str(&json).map_err(|error| {
                            rquickjs::Error::new_from_js_message("image", "JSON", error.to_string())
                        })?;
                        let encoded = value
                            .as_str()
                            .or_else(|| value["image_url"].as_str())
                            .map(str::to_owned)
                            .or_else(|| {
                                (value["type"] == "image")
                                    .then(|| {
                                        value["data"]
                                            .as_str()
                                            .map(|data| format!("data:;base64,{data}"))
                                    })
                                    .flatten()
                            })
                            .ok_or_else(|| {
                                rquickjs::Error::new_from_js_message(
                                    "image",
                                    "data URL",
                                    "Expected a base64 data URL or MCP image block",
                                )
                            })?;
                        let data = encoded
                            .strip_prefix("data:")
                            .and_then(|url| url.split_once(","))
                            .filter(|(header, _)| header.ends_with(";base64"))
                            .map(|(_, data)| data)
                            .filter(|data| {
                                data.len() <= tickloop_harness_base::image::MAX_BASE64_BYTES
                            })
                            .ok_or_else(|| {
                                rquickjs::Error::new_from_js_message(
                                    "image",
                                    "data URL",
                                    "Only bounded base64 data URLs are accepted",
                                )
                            })?;
                        let prepared = base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .ok()
                            .and_then(|bytes| tickloop_harness_base::image::prepare(&bytes).ok())
                            .ok_or_else(|| {
                                rquickjs::Error::new_from_js_message(
                                    "image",
                                    "image",
                                    "Invalid or unsupported image",
                                )
                            })?;
                        let mut state = image_state.borrow_mut();
                        let size =
                            prepared.data.len().saturating_add(std::mem::size_of::<
                                tickloop_harness_protocol::Image,
                            >());
                        if state.image_bytes.saturating_add(size) > memory_limit {
                            return Err(rquickjs::Error::new_from_js_message(
                                "image",
                                "output",
                                "Image output exceeds the memory budget",
                            ));
                        }
                        state.image_bytes += size;
                        state.outcome.images.push(prepared);
                        Ok(())
                    })?;
                let bootstrap: Function = ctx.eval(include_str!("codemode/prelude.js"))?;
                let bridge: Object = bootstrap.call((
                    request,
                    output,
                    image,
                    done,
                    serde_json::to_string(&tools).unwrap(),
                    MAX_CONCURRENT_CALLS,
                    store,
                ))?;
                let run: Function = bridge.get("run")?;
                let constructor: Function = ctx.eval("(async function() {}).constructor")?;
                let script: Function = constructor.call((source,))?;
                run.call::<_, ()>((script,))?;
                loop {
                    while !state.borrow().finished
                        && !control.cancel.is_cancelled()
                        && control.elapsed() < timeout
                        && ctx.execute_pending_job()
                    {}
                    if control.cancel.is_cancelled() || control.elapsed() >= timeout {
                        state.borrow_mut().outcome.error = Some(
                            if control.cancel.is_cancelled() {
                                "Script cancelled"
                            } else {
                                "Script timed out"
                            }
                            .into(),
                        );
                        break;
                    }
                    if state.borrow().finished {
                        break;
                    }
                    if state.borrow().pending_calls == 0 {
                        state.borrow_mut().outcome.error =
                            Some("Script is waiting on a promise that can never settle".into());
                        break;
                    }
                    match completed.recv_timeout(INTERRUPT_INTERVAL) {
                        Ok((id, result)) => {
                            state.borrow_mut().pending_calls -= 1;
                            let settle: Function = bridge.get("settle")?;
                            let ok = result.is_ok();
                            let payload = match result {
                                Ok(value) => value.to_string(),
                                Err(error) => error,
                            };
                            settle.call::<_, ()>((id, ok, payload))?;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                Ok(())
            })
            .map_err(|error| error.to_string())
    })();
    let mut state = state.borrow_mut();
    if let Err(error) = result
        && !state.finished
    {
        state.outcome.error = Some(if control.elapsed() >= timeout {
            "Script timed out".into()
        } else if control.cancel.is_cancelled() {
            "Script cancelled".into()
        } else {
            error
        });
    }
    let (text, path) = Rc::try_unwrap(output).ok().unwrap().into_inner().finish();
    state.outcome.output = text;
    state.outcome.full_output_path = path;
    std::mem::take(&mut state.outcome)
}
