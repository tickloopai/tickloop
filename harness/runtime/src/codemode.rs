//! Executes model-authored JavaScript with tool calls as its only host capability.
mod output;
use base64::Engine;
use futures_util::{StreamExt, stream::FuturesUnordered};
use rquickjs::{Context, Function, Object, Runtime};
use serde::Deserialize;
use serde_json::Value;
use std::{
    cell::RefCell,
    future::Future,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const MEMORY_LIMIT: usize = 256 * 1024 * 1024;
const INTERRUPT_INTERVAL: Duration = Duration::from_millis(10);
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

#[derive(Default)]
pub struct Options {
    pub control: Control,
    pub output_directory: Option<std::path::PathBuf>,
}

#[derive(Default)]
pub struct Outcome {
    pub output: String,
    pub error: Option<String>,
    pub full_output_path: Option<std::path::PathBuf>,
    pub images: Vec<tickloop_harness_protocol::Image>,
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
    let timeout = source_options
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT);
    let output_limit = source_options
        .max_output_tokens
        .unwrap_or(10_000)
        .saturating_mul(4)
        .min(MEMORY_LIMIT);
    let output_directory = options.output_directory;
    let control = options.control;
    let _cancel_on_drop = control.cancel.clone().drop_guard();
    let worker_control = control.clone();
    let (requests, mut incoming) = tokio::sync::mpsc::unbounded_channel();
    let (responses, completed) = mpsc::channel();
    let source = source.to_owned();
    let mut worker = tokio::task::spawn_blocking(move || {
        execute(
            source,
            tools,
            requests,
            completed,
            worker_control,
            timeout,
            output::Output::new(output_directory, output_limit),
        )
    });
    let mut pending = FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;
            result = &mut worker => {
                control.cancel.cancel();
                while pending.next().await.is_some() {}
                return result.unwrap_or_else(|error| Outcome {error:Some(error.to_string()), ..Default::default()});
            },
            Some(request) = incoming.recv() => {
                let future = call(request.name, request.input, control.cancel.clone());
                pending.push(async move { (request.id, future.await) });
            }
            Some(result) = pending.next(), if !pending.is_empty() => { let _ = responses.send(result); }
        }
    }
}

fn execute(
    source: String,
    tools: Vec<String>,
    requests: tokio::sync::mpsc::UnboundedSender<Call>,
    completed: mpsc::Receiver<(u32, Result<Value, String>)>,
    control: Control,
    timeout: Duration,
    output: output::Output,
) -> Outcome {
    let output = Rc::new(RefCell::new(output));
    let state = Rc::new(RefCell::new((Outcome::default(), false, 0usize)));
    let result = (|| -> Result<(), String> {
        let runtime = Runtime::new().map_err(|error| error.to_string())?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        let interrupted = control.clone();
        let finished = state.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            finished.borrow().1
                || interrupted.cancel.is_cancelled()
                || interrupted.elapsed() >= timeout
        })));
        let context = Context::full(&runtime).map_err(|error| error.to_string())?;
        context
            .with(|ctx| -> rquickjs::Result<()> {
                let call_state = state.clone();
                let request =
                    Function::new(ctx.clone(), move |id: u32, name: String, input: String| {
                        let input = serde_json::from_str(&input).unwrap_or(Value::Null);
                        call_state.borrow_mut().2 += 1;
                        let _ = requests.send(Call { id, name, input });
                    })?;
                let output_state = output.clone();
                let output = Function::new(ctx.clone(), move |text: String| {
                    output_state.borrow_mut().push(&text).map_err(|error| {
                        rquickjs::Error::new_from_js_message("output", "text", error.to_string())
                    })
                })?;
                let done_state = state.clone();
                let done = Function::new(ctx.clone(), move |error: Option<String>| {
                    let mut state = done_state.borrow_mut();
                    state.0.error = error;
                    state.1 = true;
                })?;
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
                        image_state.borrow_mut().0.images.push(prepared);
                        Ok(())
                    })?;
                let bootstrap: Function = ctx.eval(include_str!("codemode/prelude.js"))?;
                let bridge: Object = bootstrap.call((
                    request,
                    output,
                    image,
                    done,
                    serde_json::to_string(&tools).unwrap(),
                ))?;
                let run: Function = bridge.get("run")?;
                let script: Function = ctx.eval(format!("(async function() {{\n{source}\n}})"))?;
                run.call::<_, ()>((script,))?;
                loop {
                    while !state.borrow().1
                        && !control.cancel.is_cancelled()
                        && control.elapsed() < timeout
                        && ctx.execute_pending_job()
                    {}
                    if control.cancel.is_cancelled() || control.elapsed() >= timeout {
                        state.borrow_mut().0.error = Some(
                            if control.cancel.is_cancelled() {
                                "Script cancelled"
                            } else {
                                "Script timed out"
                            }
                            .into(),
                        );
                        break;
                    }
                    if state.borrow().1 {
                        break;
                    }
                    if state.borrow().2 == 0 {
                        state.borrow_mut().0.error =
                            Some("Script is waiting on a promise that can never settle".into());
                        break;
                    }
                    match completed.recv_timeout(INTERRUPT_INTERVAL) {
                        Ok((id, result)) => {
                            state.borrow_mut().2 -= 1;
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
        && !state.1
    {
        state.0.error = Some(if control.elapsed() >= timeout {
            "Script timed out".into()
        } else if control.cancel.is_cancelled() {
            "Script cancelled".into()
        } else {
            error
        });
    }
    let (text, path) = Rc::try_unwrap(output).ok().unwrap().into_inner().finish();
    state.0.output = text;
    state.0.full_output_path = path;
    std::mem::take(&mut state.0)
}
