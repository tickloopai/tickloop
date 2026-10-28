use std::{
    collections::BTreeSet,
    convert::Infallible,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_util::sync::CancellationToken;

use super::SandboxNetwork;

const MAX_CONNECTIONS: usize = 64;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_DENIED_DOMAINS: usize = 32;
const MAX_DOMAIN_NAME_BYTES: usize = 253;
type ProxyBody = BoxBody<Bytes, hyper::Error>;

pub(crate) struct NetworkProxy {
    port: u16,
    state: Arc<ProxyState>,
}

struct ProxyState {
    allowed_domains: Vec<String>,
    stop: CancellationToken,
    denied_domains: Mutex<BTreeSet<String>>,
}

impl NetworkProxy {
    pub(crate) fn start(network: &SandboxNetwork) -> io::Result<Option<Self>> {
        let SandboxNetwork::Allowlist {
            allowed_domains,
            proxy_port,
        } = network
        else {
            return Ok(None);
        };
        if *proxy_port == 0 || allowed_domains.is_empty() {
            return Err(io::Error::other("invalid sandbox network policy"));
        }
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *proxy_port))?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let state = Arc::new(ProxyState {
            allowed_domains: allowed_domains.clone(),
            stop: CancellationToken::new(),
            denied_domains: Mutex::new(BTreeSet::new()),
        });
        let listener_state = state.clone();
        tokio::spawn(async move {
            let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
            loop {
                let accepted = tokio::select! {
                    _ = listener_state.stop.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else { break };
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    continue;
                };
                let state = listener_state.clone();
                tokio::spawn(async move {
                    let permit = Arc::new(permit);
                    let cancelled = state.stop.clone();
                    let service = service_fn(move |request| {
                        let state = state.clone();
                        let permit = permit.clone();
                        async move { Ok::<_, Infallible>(forward(request, state, permit).await) }
                    });
                    tokio::select! {
                        _ = cancelled.cancelled() => {},
                        _ = hyper::server::conn::http1::Builder::new()
                            .keep_alive(false)
                            .timer(TokioTimer::new())
                            .header_read_timeout(CONNECTION_TIMEOUT)
                            .max_buf_size(MAX_HEADER_BYTES)
                            .serve_connection(TokioIo::new(stream), service).with_upgrades() => {},
                    }
                });
            }
        });
        Ok(Some(Self {
            port: *proxy_port,
            state,
        }))
    }

    pub(crate) fn configure(&self, command: &mut tokio::process::Command) {
        let proxy = format!("http://127.0.0.1:{}", self.port);
        for name in [
            "http_proxy",
            "https_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "all_proxy",
            "ALL_PROXY",
        ] {
            command.env(name, &proxy);
        }
        for name in ["no_proxy", "NO_PROXY"] {
            command.env(name, "");
        }
    }

    pub(crate) fn stop(&self) {
        self.state.stop.cancel();
    }

    pub(crate) fn annotate_denials(&self, output: &std::fs::File) {
        use std::io::Write;
        for domain in self.state.denied_domains.lock().unwrap().iter() {
            let _ = writeln!(&mut &*output, "\n被沙箱拒绝的域名: {domain}");
        }
    }
}

impl Drop for NetworkProxy {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn forward(
    mut request: Request<Incoming>,
    state: Arc<ProxyState>,
    permit: Arc<OwnedSemaphorePermit>,
) -> Response<ProxyBody> {
    let is_connect = request.method() == Method::CONNECT;
    if !is_connect && request.uri().scheme_str() != Some("http") {
        return response(StatusCode::BAD_REQUEST, "expected an HTTP proxy request");
    }
    let Some(host) = request
        .uri()
        .host()
        .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
    else {
        return response(StatusCode::BAD_REQUEST, "missing destination host");
    };
    if !state.allowed_domains.iter().any(|domain| {
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        match domain.strip_prefix("*.") {
            Some(suffix) => host
                .strip_suffix(suffix)
                .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
            None => host == domain,
        }
    }) {
        let mut denied = state.denied_domains.lock().unwrap();
        if denied.len() < MAX_DENIED_DOMAINS && host.len() <= MAX_DOMAIN_NAME_BYTES {
            denied.insert(host.clone());
        }
        return response(StatusCode::FORBIDDEN, "sandbox network access denied");
    }
    let port = match request.uri().port_u16() {
        Some(port) if port != 0 => port,
        None if !is_connect => 80,
        _ => return response(StatusCode::BAD_REQUEST, "missing destination port"),
    };
    let connection = tokio::time::timeout(
        CONNECTION_TIMEOUT,
        TcpStream::connect((host.as_str(), port)),
    )
    .await;
    let Ok(Ok(mut stream)) = connection else {
        return response(StatusCode::BAD_GATEWAY, "sandbox proxy connection failed");
    };
    if is_connect {
        tokio::spawn(async move {
            let _permit = permit;
            tokio::select! {
                _ = state.stop.cancelled() => {},
                _ = async {
                    if let Ok(upgraded) = hyper::upgrade::on(&mut request).await {
                        let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(upgraded), &mut stream).await;
                    }
                } => {},
            }
        });
        return response(StatusCode::OK, "");
    }
    let authority = request.uri().authority().unwrap().as_str().to_owned();
    *request.uri_mut() = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .parse()
        .unwrap();
    request.headers_mut().remove("proxy-authorization");
    request.headers_mut().remove("proxy-connection");
    request
        .headers_mut()
        .insert("host", authority.parse().unwrap());
    request
        .headers_mut()
        .insert("connection", "close".parse().unwrap());
    let Ok((mut sender, connection)) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    else {
        return response(StatusCode::BAD_GATEWAY, "sandbox proxy handshake failed");
    };
    tokio::spawn(async move {
        tokio::select! {
            _ = state.stop.cancelled() => {},
            _ = connection => {},
        }
    });
    match sender.send_request(request).await {
        Ok(response) => response.map(BodyExt::boxed),
        Err(_) => response(StatusCode::BAD_GATEWAY, "sandbox proxy request failed"),
    }
}

fn response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .body(
            Full::new(Bytes::copy_from_slice(message.as_bytes()))
                .map_err(|never| match never {})
                .boxed(),
        )
        .unwrap()
}
