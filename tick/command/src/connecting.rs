//! A Tick its host launched whose data source needs the network. It answers its host at once and
//! connects while its view shows how, retrying with a capped backoff while the source cannot be
//! reached and waiting for the user to log in again while the source refuses the login.

use std::{future::Future, pin::Pin, time::Duration};

use crossterm::event::Event;
use futures_util::{Stream, StreamExt};
use ratatui::{Terminal, backend::Backend, layout::Size};
use serde_json::{Value, json};
use tick_market::{
    config::Settings,
    runtime::{self, Connecting},
    terminal::VisibleTerminalSession,
};
use tick_protocol::TOOL_FAILED;
use tick_provider::{
    error::{Error, Result},
    exit_code,
};
use tick_sdk::stream::{Requests, RpcError, TickStream};

use crate::{
    cli::TickCommand,
    entry::{self, Running, Services},
};

/// How long the Tick waits after each consecutive failure to reach its source; every later
/// failure waits [`RETRY_DELAY_CAP`].
const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];
const RETRY_DELAY_CAP: Duration = Duration::from_secs(30);
/// How often a Tick whose login was refused reads the stored login again.
const LOGIN_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// The data source a hosted Tick reads through, connected only after the Tick answered its host.
pub trait DataSource {
    type Connection;
    /// Its name in the view, such as "Longbridge".
    fn name(&self) -> &str;
    /// How the user logs in again, shown and published while the source refuses the login.
    fn login_hint(&self) -> &str;
    /// A fingerprint of the login stored on this computer, read without the network, that
    /// changes when the user logs in again.
    fn stored_login(&self) -> Option<u64>;
    /// Connects from scratch. Not reaching the source is a network error and a refused login an
    /// authorization error; any other error ends the Tick.
    fn connect(&mut self) -> impl Future<Output = Result<Self::Connection>>;
    fn services(connection: &Self::Connection) -> Services<'_>;
}

/// Runs the live Tick its host launched over the services of `source`: the host finds it ready
/// at once, and its view shows the connection until the first content arrives.
pub async fn run_hosted<D: DataSource>(
    tick: TickCommand,
    settings: &Settings,
    size: Option<Size>,
    source: D,
) -> Result<()> {
    let mut updates = TickStream::from_process(tick.kind())?;
    let mut session = VisibleTerminalSession::enter(size)?;
    let input = tick_market::terminal::visible_input_events()?;
    let result = connect_and_run(
        tick,
        settings,
        &mut session.terminal,
        input,
        &mut updates,
        source,
    )
    .await;
    updates.close().await;
    result
}

/// Why the Tick has no content from its source yet.
enum Waiting {
    /// The last `failures` attempts in a row could not reach the source, the last one failing
    /// with `error`.
    Unreachable {
        failures: usize,
        error: Option<String>,
    },
    /// The source refused the login stored as `login`.
    Refused { login: Option<u64> },
    /// The source turned the Tick's request down with `error`, such as an unknown symbol; asking
    /// again would get the same answer, so the Tick only shows why.
    Rejected { error: String },
}

impl Waiting {
    /// The Tick's data while it waits, which the Agent reads and is told of as it changes.
    fn data(&self, hint: &str) -> Value {
        match self {
            Self::Unreachable { error, .. } => {
                json!({"connection": {"state": "retrying", "error": error}})
            }
            Self::Refused { .. } => {
                json!({"connection": {"state": "not_authorized", "hint": hint}})
            }
            Self::Rejected { error } => {
                json!({"connection": {"state": "failed", "error": error}})
            }
        }
    }

    /// Why a Tick Tool call cannot run yet.
    fn refusal(&self, name: &str, hint: &str) -> RpcError {
        let error = match self {
            Self::Unreachable { error: None, .. } => {
                Error::network(format!("{name} is not connected yet"))
            }
            Self::Unreachable {
                error: Some(error), ..
            } => Error::network(format!("{name} is not connected yet: {error}")),
            Self::Refused { .. } => Error::auth(format!("{name} refused the login; {hint}")),
            Self::Rejected { error } => {
                Error::rejected(format!("{name} turned the Tick's request down: {error}"))
            }
        };
        RpcError::new(TOOL_FAILED, error.message).with_data(json!({"code": error.code}))
    }
}

/// What the view shows while the Tick waits on its source.
struct Screen<'a> {
    settings: &'a Settings,
    name: &'a str,
    hint: &'a str,
}

impl Screen<'_> {
    fn draw<B: Backend>(&self, terminal: &mut Terminal<B>, waiting: &Waiting) -> Result<()> {
        let connecting = match waiting {
            Waiting::Unreachable { failures, error } => Connecting::Retrying {
                attempt: failures + 1,
                last_error: error.as_deref(),
            },
            Waiting::Refused { .. } => Connecting::NotAuthorized { hint: self.hint },
            Waiting::Rejected { error } => Connecting::Rejected { error },
        };
        runtime::draw_connecting(terminal, self.settings, self.name, &connecting)
    }
}

/// Connects to `source` and runs `tick` over it, connecting again whenever an attempt cannot
/// reach the source before the Tick showed content, or the source refuses the login at any time.
/// A request the source turns down before content is shown with its reason and not asked again.
/// Once content was shown, any other failure ends the Tick as it always has.
pub async fn connect_and_run<
    B: Backend,
    S: Stream<Item = std::io::Result<Event>> + Unpin,
    D: DataSource,
>(
    tick: TickCommand,
    settings: &Settings,
    terminal: &mut Terminal<B>,
    mut input: S,
    updates: &mut TickStream,
    mut source: D,
) -> Result<()> {
    let name = source.name().to_owned();
    let hint = source.login_hint().to_owned();
    let screen = Screen {
        settings,
        name: &name,
        hint: &hint,
    };
    let mut requests = updates.take_requests();
    let mut waiting = Waiting::Unreachable {
        failures: 0,
        error: None,
    };
    loop {
        let login = source.stored_login();
        let seq = updates.seq();
        screen.draw(terminal, &waiting)?;
        let Some(connected) = wait_on(
            source.connect(),
            (&screen, &waiting),
            terminal,
            &mut input,
            &mut requests,
            updates,
        )
        .await?
        else {
            return Ok(());
        };
        let failure = match connected {
            Ok(connection) => {
                let running = Running::Provider(tick.clone(), D::services(&connection));
                match entry::run_live(running, settings, terminal, &mut input, updates).await {
                    Err(error) if keeps_running(&error, updates.seq() == seq) => error,
                    result => return result,
                }
            }
            Err(error) if keeps_running(&error, true) => error,
            Err(error) => return Err(error),
        };
        waiting = if failure.code == exit_code::NOT_AUTHORIZED {
            Waiting::Refused { login }
        } else if failure.rejected {
            Waiting::Rejected {
                error: failure.message,
            }
        } else {
            let failures = match waiting {
                Waiting::Unreachable { failures, .. } => failures + 1,
                _ => 1,
            };
            Waiting::Unreachable {
                failures,
                error: Some(failure.message),
            }
        };
        // An unchanged state publishes nothing, so each change reaches the Agent once.
        updates.publish(waiting.data(&hint))?;
        screen.draw(terminal, &waiting)?;
        let pause: Pin<Box<dyn Future<Output = ()> + '_>> = match &waiting {
            Waiting::Unreachable { failures, .. } => Box::pin(tokio::time::sleep(
                RETRY_DELAYS
                    .get(failures - 1)
                    .copied()
                    .unwrap_or(RETRY_DELAY_CAP),
            )),
            Waiting::Refused { login } => Box::pin(login_renewed(&source, *login)),
            // Asking again would get the same answer, so the Tick shows why until it is closed.
            Waiting::Rejected { .. } => Box::pin(std::future::pending()),
        };
        let paused = wait_on(
            pause,
            (&screen, &waiting),
            terminal,
            &mut input,
            &mut requests,
            updates,
        )
        .await?;
        if paused.is_none() {
            return Ok(());
        }
    }
}

/// Resolves once the login stored on this computer is no longer the refused `login`.
async fn login_renewed(source: &impl DataSource, login: Option<u64>) {
    loop {
        tokio::time::sleep(LOGIN_POLL_INTERVAL).await;
        if source.stored_login() != login {
            return;
        }
    }
}

/// Whether the Tick keeps running after `error` instead of ending with it: after a refused login
/// always, and after a source that could not be reached or turned the request down only when the
/// attempt showed no content.
fn keeps_running(error: &Error, nothing_shown: bool) -> bool {
    error.code == exit_code::NOT_AUTHORIZED || (error.code == exit_code::NETWORK && nothing_shown)
}

/// Waits for `future` while the view shows `waiting`: answers each Tick Tool call with why it
/// cannot run yet and redraws when the terminal resizes. `None` when the user quit or the host
/// closed the stream first.
async fn wait_on<T, B: Backend, S: Stream<Item = std::io::Result<Event>> + Unpin>(
    future: impl Future<Output = T>,
    (screen, waiting): (&Screen<'_>, &Waiting),
    terminal: &mut Terminal<B>,
    input: &mut S,
    requests: &mut Requests,
    updates: &mut TickStream,
) -> Result<Option<T>> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => return Ok(Some(output)),
            event = input.next() => match event {
                None => return Ok(None),
                Some(Err(error)) => return Err(error.into()),
                Some(Ok(event)) if runtime::is_exit(&event) => return Ok(None),
                Some(Ok(Event::Resize(..))) => {
                    terminal.clear().map_err(|error| Error::usage(error.to_string()))?;
                    screen.draw(terminal, waiting)?;
                }
                Some(Ok(_)) => (),
            },
            line = requests.next() => match line {
                None => return Ok(None),
                Some(line) => {
                    if let Some(call) = updates.dispatch(line) {
                        updates.answer(call.id, Err(waiting.refusal(screen.name, screen.hint)));
                    }
                }
            },
        }
    }
}
