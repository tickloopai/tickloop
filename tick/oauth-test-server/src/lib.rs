//! An OAuth 2.0 authorization server on 127.0.0.1 for Tick package logins: RFC 8414 metadata,
//! RFC 7591 registration, RFC 8628 device authorization, a token endpoint that answers device
//! code polls as scripted, and RFC 7009 revocation. It records every request it receives.
//!
//! For remote MCP logins it is also a protected resource: [`RESOURCE_PATH`] answers 401 with a
//! `WWW-Authenticate` challenge naming its RFC 9728 metadata until a request carries an access
//! token it issued, and an authorization endpoint redirects with a code that the token endpoint
//! exchanges only for the matching client, redirect URI and S256 PKCE verifier.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
pub const REGISTRATION_PATH: &str = "/register";
pub const DEVICE_AUTHORIZATION_PATH: &str = "/device";
pub const TOKEN_PATH: &str = "/token";
pub const REVOCATION_PATH: &str = "/revoke";
pub const AUTHORIZATION_PATH: &str = "/authorize";
/// The protected MCP resource.
pub const RESOURCE_PATH: &str = "/mcp";
pub const PROTECTED_RESOURCE_METADATA_PATH: &str = "/.well-known/oauth-protected-resource";
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
    /// Consumes an approved device code but drops the response; replay is invalid_grant.
    LostExchange,
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
    pub authorization: Option<String>,
    pub form_pairs: Vec<(String, String)>,
}

struct Script {
    issuer_path: String,
    userinfo: (u16, Value),
    metadata: Value,
    registration: Option<(u16, Value)>,
    device: Value,
    token_replies: VecDeque<TokenReply>,
    revocation_status: u16,
    forgotten_clients: HashSet<String>,
    registered: usize,
    issued: usize,
    requests: Vec<Request>,
    authorizations: BTreeMap<String, Authorization>,
    denies_authorization: bool,
    token_lifetime: u64,
    access_tokens: HashSet<String>,
    refresh_tokens: HashSet<String>,
}

/// An authorization code waiting for its exchange.
struct Authorization {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
}

type Headers = Vec<(&'static str, String)>;

pub struct AuthorizationServer {
    issuer: String,
    script: Arc<Mutex<Script>>,
}

impl AuthorizationServer {
    /// Starts a server whose device codes last ten minutes and are polled every second; polls
    /// are pending until [`Self::answer_polls`] says otherwise.
    pub fn start() -> Self {
        Self::start_with_path("")
    }

    pub fn start_with_path(path: &str) -> Self {
        assert!(path.is_empty() || path.starts_with('/') && !path.ends_with('/'));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://{}{path}", listener.local_addr().unwrap());
        let script = Arc::new(Mutex::new(Script {
            issuer_path: path.to_owned(),
            userinfo: (
                200,
                json!({"sub": "account-1", "name": "A User", "email": "a@example.test"}),
            ),
            metadata: json!({
                "issuer": issuer,
                "token_endpoint": format!("{issuer}{TOKEN_PATH}"),
                "device_authorization_endpoint": format!("{issuer}{DEVICE_AUTHORIZATION_PATH}"),
                "registration_endpoint": format!("{issuer}{REGISTRATION_PATH}"),
                "revocation_endpoint": format!("{issuer}{REVOCATION_PATH}"),
                "userinfo_endpoint": format!("{issuer}/userinfo"),
                "authorization_endpoint": format!("{issuer}{AUTHORIZATION_PATH}"),
                "grant_types_supported": ["authorization_code", "refresh_token", DEVICE_CODE_GRANT],
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"],
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
            authorizations: BTreeMap::new(),
            denies_authorization: false,
            token_lifetime: 3600,
            access_tokens: HashSet::new(),
            refresh_tokens: HashSet::new(),
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

    pub fn answer_userinfo(&self, status: u16, body: Value) {
        self.script.lock().unwrap().userinfo = (status, body);
    }

    pub fn answer_revocations(&self, status: u16) {
        self.script.lock().unwrap().revocation_status = status;
    }

    /// Makes the server answer `invalid_client` for the clients it registered so far.
    pub fn reject_client(&self, client_id: &str) {
        self.script
            .lock()
            .unwrap()
            .forgotten_clients
            .insert(client_id.to_owned());
    }

    pub fn forget_clients(&self) {
        let mut script = self.script.lock().unwrap();
        let registered = script.registered;
        script
            .forgotten_clients
            .extend((1..=registered).map(|number| format!("client-{number}")));
    }

    /// The protected MCP resource, whose metadata names this server.
    pub fn resource(&self) -> String {
        format!("{}{RESOURCE_PATH}", self.issuer)
    }

    /// Makes the authorization endpoint redirect with `error=access_denied`.
    pub fn deny_authorizations(&self) {
        self.script.lock().unwrap().denies_authorization = true;
    }

    /// How long the tokens issued from now on last, in seconds.
    pub fn issue_tokens_lasting(&self, seconds: u64) {
        self.script.lock().unwrap().token_lifetime = seconds;
    }

    /// Acts as the browser of a user who approves: requests `authorization_url` and returns the
    /// address the server redirects to.
    pub fn approve(&self, authorization_url: &str) -> String {
        let url = url::Url::parse(authorization_url).unwrap();
        let mut stream = TcpStream::connect(url.socket_addrs(|| None).unwrap()[0]).unwrap();
        let target = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
            .lines()
            .find_map(|line| line.strip_prefix("Location: "))
            .unwrap_or_else(|| panic!("the authorization endpoint did not redirect: {response}"))
            .trim()
            .to_owned()
    }

    pub fn requests(&self) -> Vec<Request> {
        self.script.lock().unwrap().requests.clone()
    }

    /// The requests to `path`.
    pub fn requests_to(&self, path: &str) -> Vec<Request> {
        let issuer_path = self.script.lock().unwrap().issuer_path.clone();
        let path = if path == METADATA_PATH {
            format!("{path}{issuer_path}")
        } else {
            format!("{issuer_path}{path}")
        };
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
    let (status, body, headers) = {
        let mut script = script.lock().unwrap();
        script.requests.push(request.clone());
        reply(&mut script, &request)
    };
    if status == 0 {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    let body = body.map_or_else(String::new, |body| body.to_string());
    let headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             {headers}Connection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
    let _ = stream.shutdown(Shutdown::Both);
}

fn reply(script: &mut Script, request: &Request) -> (u16, Option<Value>, Headers) {
    let issuer = script.metadata["issuer"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let origin = issuer
        .strip_suffix(&script.issuer_path)
        .unwrap_or(&issuer)
        .to_owned();
    let resource_metadata_path = format!("{PROTECTED_RESOURCE_METADATA_PATH}{RESOURCE_PATH}");
    let path_only = request.path.split('?').next().unwrap_or_default();
    if request.method == "GET" && path_only == resource_metadata_path {
        let metadata = json!({
            "resource": format!("{origin}{RESOURCE_PATH}"),
            "authorization_servers": [issuer],
        });
        return (200, Some(metadata), Vec::new());
    }
    if path_only == RESOURCE_PATH {
        let bearer = request
            .authorization
            .as_deref()
            .and_then(|value| value.strip_prefix("Bearer "));
        if bearer.is_some_and(|token| script.access_tokens.contains(token)) {
            return (200, Some(json!({})), Vec::new());
        }
        let challenge = format!(
            "Bearer error=\"invalid_token\", resource_metadata=\"{origin}{resource_metadata_path}\""
        );
        return (401, None, vec![("WWW-Authenticate", challenge)]);
    }
    let path = request
        .path
        .strip_prefix(&script.issuer_path)
        .unwrap_or(&request.path);
    if let Some(query) = path.strip_prefix(&format!("{AUTHORIZATION_PATH}?")) {
        return authorize(script, query);
    }
    let (status, body) = reply_to_client(script, request);
    (status, body, Vec::new())
}

fn reply_to_client(script: &mut Script, request: &Request) -> (u16, Option<Value>) {
    let client = request.form.get("client_id").cloned().unwrap_or_default();
    if request.method == "GET" && request.path == format!("{METADATA_PATH}{}", script.issuer_path) {
        return (200, Some(script.metadata.clone()));
    }
    let path = request
        .path
        .strip_prefix(&script.issuer_path)
        .unwrap_or(&request.path);
    let grant = request.form.get("grant_type").map(String::as_str);
    if path == TOKEN_PATH && grant == Some("authorization_code") {
        return exchange_code(script, request);
    }
    if path == TOKEN_PATH
        && grant == Some("refresh_token")
        && script.token_replies.is_empty()
        && let Some(refresh_token) = request.form.get("refresh_token")
        && script.refresh_tokens.remove(refresh_token)
    {
        return (200, Some(issue_token(script)));
    }
    match (request.method.as_str(), path) {
        ("GET", "/userinfo") => (script.userinfo.0, Some(script.userinfo.1.clone())),
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
                TokenReply::LostExchange => {
                    script.issued += 1;
                    script
                        .token_replies
                        .push_front(TokenReply::Error(400, "invalid_grant".into()));
                    (0, None)
                }
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
        ("POST", REVOCATION_PATH) => {
            if let Some(token) = request.form.get("token") {
                script.refresh_tokens.remove(token);
                script.access_tokens.remove(token);
            }
            (script.revocation_status, None)
        }
        _ => (404, None),
    }
}

/// Answers the browser with a redirect to the client's callback, as a user who approves.
fn authorize(script: &mut Script, query: &str) -> (u16, Option<Value>, Headers) {
    let fields: BTreeMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let field = |name: &str| fields.get(name).cloned().unwrap_or_default();
    let invalid = (400, Some(json!({"error": "invalid_request"})), Vec::new());
    if field("response_type") != "code"
        || field("code_challenge_method") != "S256"
        || field("code_challenge").is_empty()
        || field("client_id").is_empty()
    {
        return invalid;
    }
    let Ok(mut location) = url::Url::parse(&field("redirect_uri")) else {
        return invalid;
    };
    if script.denies_authorization {
        location
            .query_pairs_mut()
            .append_pair("error", "access_denied")
            .append_pair("state", &field("state"));
    } else {
        let code = format!("code-{}", script.authorizations.len() + 1);
        script.authorizations.insert(
            code.clone(),
            Authorization {
                client_id: field("client_id"),
                redirect_uri: field("redirect_uri"),
                code_challenge: field("code_challenge"),
            },
        );
        location
            .query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", &field("state"));
    }
    (302, None, vec![("Location", location.into())])
}

fn exchange_code(script: &mut Script, request: &Request) -> (u16, Option<Value>) {
    let field = |name: &str| request.form.get(name).cloned().unwrap_or_default();
    let verifier_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(field("code_verifier")));
    let valid = script
        .authorizations
        .remove(&field("code"))
        .is_some_and(|authorization| {
            authorization.client_id == field("client_id")
                && authorization.redirect_uri == field("redirect_uri")
                && authorization.code_challenge == verifier_challenge
        });
    if valid {
        (200, Some(issue_token(script)))
    } else {
        (400, Some(json!({"error": "invalid_grant"})))
    }
}

/// A token response with `access-<n>` and `refresh-<n>`, which the resource and the refresh
/// grant accept.
fn issue_token(script: &mut Script) -> Value {
    script.issued += 1;
    let number = script.issued;
    script.access_tokens.insert(format!("access-{number}"));
    script.refresh_tokens.insert(format!("refresh-{number}"));
    json!({
        "access_token": format!("access-{number}"),
        "token_type": "Bearer",
        "expires_in": script.token_lifetime,
        "refresh_token": format!("refresh-{number}"),
    })
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut words = request_line.split_whitespace();
    let method = words.next()?.to_owned();
    let path = words.next()?.to_owned();
    let mut length = 0;
    let mut authorization = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 || header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("authorization")
        {
            authorization = Some(value.trim().to_owned());
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
        authorization,
        form_pairs: url::form_urlencoded::parse(&body).into_owned().collect(),
    })
}
