use std::{fmt, io::Write, time::Duration};

use hyper_util::client::proxy::matcher::Matcher;
use reqwest::{Client, Url, redirect::Policy};

const DIAGNOSTICS_ENV: &str = "TICKLOOP_NETWORK_DIAGNOSTICS";

pub(crate) struct HttpPolicy {
    pub timeout: Option<Duration>,
    pub connect_timeout: Option<Duration>,
    pub read_timeout: Option<Duration>,
    pub redirect: Policy,
}

impl HttpPolicy {
    pub(crate) fn new(redirect: Policy) -> Self {
        Self {
            timeout: None,
            connect_timeout: None,
            read_timeout: None,
            redirect,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProxySource {
    Direct,
    Environment,
    System,
}

impl fmt::Display for ProxySource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Direct => "direct",
            Self::Environment => "environment",
            Self::System => "system",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NetworkRoute {
    pub proxy_source: ProxySource,
    pub direct_websocket_eligible: bool,
}

pub(crate) fn build_http_client(
    target: &Url,
    policy: HttpPolicy,
) -> Result<(Client, NetworkRoute), reqwest::Error> {
    let route = route_for(target);
    let mut builder = Client::builder().redirect(policy.redirect);
    if let Some(timeout) = policy.timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(timeout) = policy.connect_timeout {
        builder = builder.connect_timeout(timeout);
    }
    if let Some(timeout) = policy.read_timeout {
        builder = builder.read_timeout(timeout);
    }
    if is_loopback(target) {
        builder = builder.no_proxy();
    }
    builder.build().map(|client| (client, route))
}

fn route_for(target: &Url) -> NetworkRoute {
    let proxy_source = if is_loopback(target) {
        ProxySource::Direct
    } else if let Ok(uri) = target.origin().ascii_serialization().parse() {
        if Matcher::from_env().intercept(&uri).is_some() {
            ProxySource::Environment
        } else if Matcher::from_system().intercept(&uri).is_some() {
            ProxySource::System
        } else {
            ProxySource::Direct
        }
    } else {
        // An unclassifiable route must never make a direct WebSocket attempt.
        ProxySource::System
    };
    NetworkRoute {
        proxy_source,
        direct_websocket_eligible: proxy_source == ProxySource::Direct,
    }
}

pub(crate) fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SafeTarget {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl SafeTarget {
    pub(crate) fn from_url(url: &Url) -> Self {
        Self {
            scheme: url.scheme().to_owned(),
            host: url.host_str().unwrap_or("unknown").to_owned(),
            port: url.port_or_known_default(),
        }
    }
}

impl fmt::Display for SafeTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(port) = self.port {
            write!(formatter, "{}://{}:{port}", self.scheme, self.host)
        } else {
            write!(formatter, "{}://{}", self.scheme, self.host)
        }
    }
}

pub(crate) struct NetworkDiagnostic {
    pub phase: &'static str,
    pub target: SafeTarget,
    pub proxy_source: ProxySource,
    pub status: Option<u16>,
    pub elapsed: Duration,
}

impl fmt::Display for NetworkDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "network phase={} target={} proxy={} status={} elapsed_ms={}",
            self.phase,
            self.target,
            self.proxy_source,
            self.status
                .map_or_else(|| "none".to_owned(), |status| status.to_string()),
            self.elapsed.as_millis()
        )
    }
}

impl NetworkDiagnostic {
    pub(crate) fn new(
        phase: &'static str,
        target: &Url,
        route: NetworkRoute,
        status: Option<u16>,
        elapsed: Duration,
    ) -> Self {
        Self {
            phase,
            target: SafeTarget::from_url(target),
            proxy_source: route.proxy_source,
            status,
            elapsed,
        }
    }

    pub(crate) fn emit(&self) {
        if std::env::var(DIAGNOSTICS_ENV).as_deref() == Ok("1") {
            self.emit_to(&mut std::io::stderr().lock());
        }
    }

    fn emit_to(&self, output: &mut impl Write) {
        let _ = writeln!(output, "{self}");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        process::Command,
        thread,
    };

    use super::*;

    const CHILD_CASE: &str = "TICKLOOP_NETWORK_TEST_CASE";
    const CHILD_TARGET: &str = "TICKLOOP_NETWORK_TEST_TARGET";
    const TEST_NAME: &str = "network::tests::proxy_routing_and_redacted_diagnostics";

    #[test]
    fn proxy_routing_and_redacted_diagnostics() {
        if let Ok(case) = std::env::var(CHILD_CASE) {
            run_child_case(&case);
            return;
        }

        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let proxy_server = thread::spawn(move || serve_one_request(proxy));
        run_isolated_case(
            "environment",
            "http://example.invalid/private?code=secret",
            proxy_address,
            "",
        );
        let proxy_request = proxy_server.join().unwrap();
        assert!(
            proxy_request.starts_with("GET http://example.invalid/private?code=secret HTTP/1.1")
        );

        run_isolated_case(
            "no_proxy",
            "http://example.invalid/private",
            proxy_address,
            "example.invalid",
        );

        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_address = origin.local_addr().unwrap();
        let origin_server = thread::spawn(move || serve_one_request(origin));
        run_isolated_case(
            "loopback",
            &format!("http://{origin_address}/local"),
            proxy_address,
            "",
        );
        let direct_request = origin_server.join().unwrap();
        assert!(direct_request.starts_with("GET /local HTTP/1.1"));

        let secret_url =
            Url::parse("https://user:password@example.invalid:8443/private?code=secret#fragment")
                .unwrap();
        let event = NetworkDiagnostic::new(
            "oauth_refresh",
            &secret_url,
            NetworkRoute {
                proxy_source: ProxySource::Environment,
                direct_websocket_eligible: false,
            },
            Some(401),
            Duration::from_millis(12),
        );
        let rendered = event.to_string();
        assert!(rendered.contains("target=https://example.invalid:8443"));
        for secret in ["user", "password", "private", "code", "secret", "fragment"] {
            assert!(
                !rendered.contains(secret),
                "diagnostic disclosed {secret}: {rendered}"
            );
        }
        event.emit_to(&mut BrokenSink);
    }

    fn run_isolated_case(
        case: &str,
        target: &str,
        proxy_address: std::net::SocketAddr,
        no_proxy: &str,
    ) {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_CASE, case)
            .env(CHILD_TARGET, target)
            .env("HTTP_PROXY", format!("http://{proxy_address}"))
            .env("http_proxy", format!("http://{proxy_address}"))
            .env("NO_PROXY", no_proxy)
            .env("no_proxy", no_proxy)
            .env_remove("ALL_PROXY")
            .env_remove("all_proxy")
            .env_remove("HTTPS_PROXY")
            .env_remove("https_proxy")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated {case} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_child_case(case: &str) {
        let target = Url::parse(&std::env::var(CHILD_TARGET).unwrap()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut policy = HttpPolicy::new(Policy::none());
            policy.timeout = Some(Duration::from_secs(3));
            let (client, route) = build_http_client(&target, policy).unwrap();
            let expected = match case {
                "environment" => ProxySource::Environment,
                "no_proxy" | "loopback" => ProxySource::Direct,
                _ => panic!("unknown fixture case"),
            };
            assert_eq!(route.proxy_source, expected);
            assert_eq!(
                route.direct_websocket_eligible,
                expected == ProxySource::Direct
            );
            if case != "no_proxy" {
                let response = client.get(target).send().await.unwrap();
                assert_eq!(response.status(), reqwest::StatusCode::OK);
            }
        });
    }

    fn serve_one_request(listener: TcpListener) -> String {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0; 1024];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        String::from_utf8(request).unwrap()
    }

    struct BrokenSink;

    impl Write for BrokenSink {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }
}
