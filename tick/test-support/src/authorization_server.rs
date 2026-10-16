//! An OAuth 2.0 authorization server on 127.0.0.1 for Tick package logins: RFC 8414 metadata,
//! RFC 7591 registration, RFC 8628 device authorization, a token endpoint that answers device
//! code polls as scripted, and RFC 7009 revocation. It records every request it receives.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};

use serde_json::{Value, json};

pub const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
pub const REGISTRATION_PATH: &str = "/register";
pub const DEVICE_AUTHORIZATION_PATH: &str = "/device";
pub const TOKEN_PATH: &str = "/token";
pub const REVOCATION_PATH: &str = "/revoke";
pub const USER_CODE: &str = "WDJB-MJHT";
pub const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// How the token endpoint answers one device code poll.
#[derive(Clone, Debug)]
pub enum TokenReply {
    Pending,
    SlowDown,
    Denied,
    Expired,
    /// A token response with `access-<n>` and `refresh-<n>`, valid for an hour.
    Approve,
    /// The token response as given.
    Token(Value),
    /// An OAuth error response with this status and code.
    Error(u16, String),
    /// This status with an empty body.
    Status(u16),
}

/// A request the server received.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// The fields of a form body.
    pub form: BTreeMap<String, String>,
    /// A JSON body.
    pub json: Option<Value>,
    pub at: Instant,
}

struct Script {
    metadata: Value,
    registration: Option<(u16, Value)>,
    device: Value,
    token_replies: VecDeque<TokenReply>,
    revocation_status: u16,
    forgotten_clients: HashSet<String>,
    registered: usize,
    issued: usize,
    requests: Vec<Request>,
}

pub struct AuthorizationServer {
    issuer: String,
    script: Arc<Mutex<Script>>,
}

impl AuthorizationServer {
    /// Starts a server whose device codes last ten minutes and are polled every second; polls
    /// are pending until [`Self::answer_polls`] says otherwise.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let script = Arc::new(Mutex::new(Script {
            metadata: json!({
                "issuer": issuer,
                "token_endpoint": format!("{issuer}{TOKEN_PATH}"),
                "device_authorization_endpoint": format!("{issuer}{DEVICE_AUTHORIZATION_PATH}"),
                "registration_endpoint": format!("{issuer}{REGISTRATION_PATH}"),
                "revocation_endpoint": format!("{issuer}{REVOCATION_PATH}"),
                "grant_types_supported": ["refresh_token", DEVICE_CODE_GRANT],
                "token_endpoint_auth_methods_supported": ["none"],
            }),
            registration: None,
            device: json!({
                "user_code": USER_CODE,
                "verification_uri": format!("{issuer}/verify"),
                "verification_uri_complete": format!("{issuer}/verify?user_code={USER_CODE}"),
                "expires_in": 600,
                "interval": 1,
            }),
            token_replies: VecDeque::new(),
            revocation_status: 200,
            forgotten_clients: HashSet::new(),
            registered: 0,
            issued: 0,
            requests: Vec::new(),
        }));
        let shared = script.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let shared = shared.clone();
                thread::spawn(move || answer(stream, &shared));
            }
        });
        Self { issuer, script }
    }

    /// The issuer identifier, also the origin of every endpoint.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Changes the metadata the server publishes.
    pub fn edit_metadata(&self, edit: impl FnOnce(&mut Value)) {
        edit(&mut self.script.lock().unwrap().metadata);
    }

    /// Changes the device authorization response, apart from its device code.
    pub fn edit_device_authorization(&self, edit: impl FnOnce(&mut Value)) {
        edit(&mut self.script.lock().unwrap().device);
    }

    /// Answers registrations with `status` and `body` instead of a new client.
    pub fn answer_registrations(&self, status: u16, body: Value) {
        self.script.lock().unwrap().registration = Some((status, body));
    }

    /// Answers the next device code polls in turn; later ones are pending.
    pub fn answer_polls(&self, replies: impl IntoIterator<Item = TokenReply>) {
        self.script.lock().unwrap().token_replies.extend(replies);
    }

    pub fn answer_revocations(&self, status: u16) {
        self.script.lock().unwrap().revocation_status = status;
    }

    /// Makes the server answer `invalid_client` for the clients it registered so far.
    pub fn forget_clients(&self) {
        let mut script = self.script.lock().unwrap();
        let registered = script.registered;
        script
            .forgotten_clients
            .extend((1..=registered).map(|number| format!("client-{number}")));
    }

    pub fn requests(&self) -> Vec<Request> {
        self.script.lock().unwrap().requests.clone()
    }

    /// The requests to `path`.
    pub fn requests_to(&self, path: &str) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| request.path == path)
            .collect()
    }
}

fn answer(mut stream: TcpStream, script: &Mutex<Script>) {
    let Some(request) = read_request(&stream) else {
        return;
    };
    let (status, body) = {
        let mut script = script.lock().unwrap();
        script.requests.push(request.clone());
        reply(&mut script, &request)
    };
    let body = body.map_or_else(String::new, |body| body.to_string());
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
    let _ = stream.shutdown(Shutdown::Both);
}

fn reply(script: &mut Script, request: &Request) -> (u16, Option<Value>) {
    let client = request.form.get("client_id").cloned().unwrap_or_default();
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", METADATA_PATH) => (200, Some(script.metadata.clone())),
        ("POST", REGISTRATION_PATH) => {
            if let Some((status, body)) = &script.registration {
                return (*status, Some(body.clone()));
            }
            script.registered += 1;
            let mut registered = request.json.clone().unwrap_or_else(|| json!({}));
            registered["client_id"] = json!(format!("client-{}", script.registered));
            (201, Some(registered))
        }
        ("POST", DEVICE_AUTHORIZATION_PATH) if script.forgotten_clients.contains(&client) => {
            (401, Some(json!({"error": "invalid_client"})))
        }
        ("POST", DEVICE_AUTHORIZATION_PATH) => {
            let mut device = script.device.clone();
            device["device_code"] = json!(format!("device-secret-{client}"));
            (200, Some(device))
        }
        ("POST", TOKEN_PATH) => {
            let error = |status, code: &str| (status, Some(json!({"error": code})));
            match script
                .token_replies
                .pop_front()
                .unwrap_or(TokenReply::Pending)
            {
                TokenReply::Pending => error(400, "authorization_pending"),
                TokenReply::SlowDown => error(400, "slow_down"),
                TokenReply::Denied => error(400, "access_denied"),
                TokenReply::Expired => error(400, "expired_token"),
                TokenReply::Error(status, code) => error(status, &code),
                TokenReply::Status(status) => (status, None),
                TokenReply::Token(token) => (200, Some(token)),
                TokenReply::Approve => {
                    script.issued += 1;
                    let number = script.issued;
                    (
                        200,
                        Some(json!({
                            "access_token": format!("access-{number}"),
                            "token_type": "Bearer",
                            "expires_in": 3600,
                            "refresh_token": format!("refresh-{number}"),
                        })),
                    )
                }
            }
        }
        ("POST", REVOCATION_PATH) => (script.revocation_status, None),
        _ => (404, None),
    }
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut words = request_line.split_whitespace();
    let method = words.next()?.to_owned();
    let path = words.next()?.to_owned();
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 || header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok()?;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        form: url::form_urlencoded::parse(&body).into_owned().collect(),
        json: serde_json::from_slice(&body).ok(),
        at: Instant::now(),
    })
}
