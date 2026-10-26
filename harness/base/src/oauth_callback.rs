//! Receiving an OAuth authorization response on this machine: a loopback listener that the
//! browser redirects to, and the redirect URL a user pastes back when the browser runs on
//! another machine. Whichever arrives first with the expected `state` completes the login.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const CALLBACK_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CALLBACK_REQUEST_BYTES: usize = 8 * 1024;
const ALREADY_COMPLETED: &str = "Callback already completed.";
const ROUTE_NOT_FOUND: &str = "Callback route not found.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CallbackError {
    #[error("the callback port {0} on 127.0.0.1 is already in use")]
    PortUnavailable(u16),
    #[error("the callback URL is invalid or missing an authorization code")]
    InvalidCallback,
    #[error("the callback belongs to a different login")]
    StateMismatch,
    #[error("the authorization was denied")]
    Denied,
    #[error("the authorization server reported an error")]
    AuthorizationFailed,
    #[error("this login has already ended")]
    Ended,
    #[error("the login was cancelled")]
    Cancelled,
    #[error("the login timed out")]
    TimedOut,
}

/// The texts the browser shows after redirecting to the listener. They never repeat the
/// callback's query, which holds the code and state.
#[derive(Clone, Copy, Debug)]
pub struct CallbackPages {
    pub received: &'static str,
    pub state_mismatch: &'static str,
    pub invalid_url: &'static str,
    pub invalid_request: &'static str,
    pub denied: &'static str,
    pub failed: &'static str,
}

/// What the authorization server sent back: the code and, when present, the RFC 9207 issuer.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthorizationResponse {
    pub code: String,
    pub issuer: Option<String>,
}

impl fmt::Debug for AuthorizationResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizationResponse")
            .field("code", &"[redacted]")
            .field("issuer", &self.issuer)
            .finish()
    }
}

/// A bound loopback port, known before the authorization URL is built because the redirect
/// URI names it.
pub struct CallbackListener {
    listener: TcpListener,
    redirect_uri: String,
}

impl CallbackListener {
    /// Binds 127.0.0.1 on `port`, or on a free port when `port` is 0; the redirect URI is
    /// `http://localhost:<port><path>`.
    pub async fn bind(port: u16, path: &str) -> Result<Self, CallbackError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|_| CallbackError::PortUnavailable(port))?;
        let bound_port = listener
            .local_addr()
            .map_err(|_| CallbackError::PortUnavailable(port))?
            .port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://localhost:{bound_port}{path}"),
        })
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Starts answering the browser, accepting only `state`, until a response arrives, the
    /// login is dropped, or `timeout` passes.
    pub fn receive(
        self,
        state: String,
        pages: CallbackPages,
        timeout: Duration,
    ) -> PendingCallback {
        let (sender, responses) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let completed = Arc::new(AtomicBool::new(false));
        let expected = Expected {
            redirect_uri: self.redirect_uri.clone(),
            state,
        };
        let task = tokio::spawn(serve_callbacks(
            self.listener,
            expected.clone(),
            pages,
            sender.clone(),
            completed.clone(),
            stop.clone(),
        ));
        PendingCallback {
            deadline: Instant::now() + timeout,
            responses,
            paste: CallbackPaste {
                expected,
                sender,
                completed,
            },
            task,
            stop,
        }
    }
}

impl fmt::Debug for CallbackListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CallbackListener")
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

#[derive(Clone)]
struct Expected {
    redirect_uri: String,
    state: String,
}

type Completion = Result<AuthorizationResponse, CallbackError>;

/// A login waiting for its authorization response. Dropping it closes the listener.
pub struct PendingCallback {
    deadline: Instant,
    responses: mpsc::UnboundedReceiver<Completion>,
    paste: CallbackPaste,
    task: tokio::task::JoinHandle<()>,
    stop: CancellationToken,
}

impl PendingCallback {
    pub fn redirect_uri(&self) -> &str {
        &self.paste.expected.redirect_uri
    }

    pub fn state(&self) -> &str {
        &self.paste.expected.state
    }

    /// A handle for the redirect URL the user pastes; it can move to another task.
    pub fn paste(&self) -> CallbackPaste {
        self.paste.clone()
    }

    pub async fn wait(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<AuthorizationResponse, CallbackError> {
        tokio::select! {
            () = cancellation.cancelled() => Err(CallbackError::Cancelled),
            () = tokio::time::sleep_until(self.deadline) => Err(CallbackError::TimedOut),
            received = self.responses.recv() => received.ok_or(CallbackError::Ended)?,
        }
    }
}

impl Drop for PendingCallback {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

impl fmt::Debug for PendingCallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingCallback")
            .field("redirect_uri", &self.paste.expected.redirect_uri)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct CallbackPaste {
    expected: Expected,
    sender: mpsc::UnboundedSender<Completion>,
    completed: Arc<AtomicBool>,
}

impl CallbackPaste {
    /// Completes the login with the full redirect URL from the browser's address bar. A URL
    /// for another address or login is refused and leaves the login waiting.
    pub fn submit(&self, pasted_redirect_url: &str) -> Result<(), CallbackError> {
        let completion = parse_callback(pasted_redirect_url, &self.expected);
        if matches!(
            completion,
            Err(CallbackError::InvalidCallback | CallbackError::StateMismatch)
        ) {
            return completion.map(|_| ());
        }
        if self.completed.swap(true, Ordering::AcqRel) {
            return Err(CallbackError::Ended);
        }
        let outcome = completion.as_ref().map(|_| ()).map_err(|error| *error);
        self.sender
            .send(completion)
            .map_err(|_| CallbackError::Ended)?;
        outcome
    }
}

impl fmt::Debug for CallbackPaste {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CallbackPaste")
            .finish_non_exhaustive()
    }
}

fn parse_callback(pasted_redirect_url: &str, expected: &Expected) -> Completion {
    let callback =
        Url::parse(pasted_redirect_url.trim()).map_err(|_| CallbackError::InvalidCallback)?;
    let redirect = Url::parse(&expected.redirect_uri).expect("bound redirect URI");
    if callback.scheme() != redirect.scheme()
        || callback.host_str() != redirect.host_str()
        || callback.port() != redirect.port()
        || callback.path() != redirect.path()
        || callback.fragment().is_some()
        || !callback.username().is_empty()
        || callback.password().is_some()
    {
        return Err(CallbackError::InvalidCallback);
    }
    let mut state = None;
    let mut code = None;
    let mut error = None;
    let mut issuer = None;
    for (name, value) in callback.query_pairs() {
        let field = match name.as_ref() {
            "state" => &mut state,
            "code" => &mut code,
            "error" => &mut error,
            "iss" => &mut issuer,
            _ => continue,
        };
        if field.replace(value.into_owned()).is_some() {
            return Err(CallbackError::InvalidCallback);
        }
    }
    if state.as_deref() != Some(expected.state.as_str()) {
        return Err(CallbackError::StateMismatch);
    }
    if let Some(error) = error {
        return Err(if error == "access_denied" {
            CallbackError::Denied
        } else {
            CallbackError::AuthorizationFailed
        });
    }
    let code = code
        .filter(|code| !code.is_empty())
        .ok_or(CallbackError::InvalidCallback)?;
    Ok(AuthorizationResponse { code, issuer })
}

async fn serve_callbacks(
    listener: TcpListener,
    expected: Expected,
    pages: CallbackPages,
    sender: mpsc::UnboundedSender<Completion>,
    completed: Arc<AtomicBool>,
    stop: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            () = stop.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else {
            return;
        };
        answer_callback(stream, &expected, &pages, &sender, &completed).await;
    }
}

async fn answer_callback(
    mut stream: TcpStream,
    expected: &Expected,
    pages: &CallbackPages,
    sender: &mpsc::UnboundedSender<Completion>,
    completed: &AtomicBool,
) {
    let (status, body, completion) = match read_request_target(&mut stream).await {
        Err(_) => (400, pages.invalid_request, None),
        Ok(target) => {
            let url = if target.starts_with('/') {
                format!(
                    "http://localhost:{}{target}",
                    stream.local_addr().map_or(0, |address| address.port())
                )
            } else {
                target
            };
            let redirect_path = Url::parse(&expected.redirect_uri)
                .map(|redirect| redirect.path().to_owned())
                .unwrap_or_default();
            if Url::parse(&url).is_ok_and(|parsed| parsed.path() != redirect_path) {
                (404, ROUTE_NOT_FOUND, None)
            } else {
                match parse_callback(&url, expected) {
                    Err(CallbackError::StateMismatch) => (400, pages.state_mismatch, None),
                    Err(CallbackError::InvalidCallback) => (400, pages.invalid_url, None),
                    _ if completed.swap(true, Ordering::AcqRel) => (409, ALREADY_COMPLETED, None),
                    Ok(response) => (200, pages.received, Some(Ok(response))),
                    Err(CallbackError::Denied) => {
                        (400, pages.denied, Some(Err(CallbackError::Denied)))
                    }
                    Err(error) => (400, pages.failed, Some(Err(error))),
                }
            }
        }
    };
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        match status {
            200 => "OK",
            404 => "Not Found",
            409 => "Conflict",
            _ => "Bad Request",
        },
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    if let Some(completion) = completion {
        let _ = sender.send(completion);
    }
}

async fn read_request_target(stream: &mut TcpStream) -> Result<String, CallbackError> {
    tokio::time::timeout(CALLBACK_REQUEST_TIMEOUT, async {
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let count = stream
                .read(&mut chunk)
                .await
                .map_err(|_| CallbackError::InvalidCallback)?;
            if count == 0 || bytes.len().saturating_add(count) > MAX_CALLBACK_REQUEST_BYTES {
                return Err(CallbackError::InvalidCallback);
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.windows(2).any(|pair| pair == b"\r\n") {
                break;
            }
        }
        let line_end = bytes
            .windows(2)
            .position(|pair| pair == b"\r\n")
            .ok_or(CallbackError::InvalidCallback)?;
        let line =
            std::str::from_utf8(&bytes[..line_end]).map_err(|_| CallbackError::InvalidCallback)?;
        let mut parts = line.split(' ');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some("GET"), Some(target), Some("HTTP/1.1"), None) => Ok(target.to_owned()),
            _ => Err(CallbackError::InvalidCallback),
        }
    })
    .await
    .map_err(|_| CallbackError::InvalidCallback)?
}
