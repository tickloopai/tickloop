use std::{
    fmt,
    io::Write,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};

use hyper_util::client::proxy::matcher::Matcher;
use reqwest::{
    Client, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    redirect::Policy,
};

const DIAGNOSTICS_ENV: &str = "TICKLOOP_NETWORK_DIAGNOSTICS";
/// The leading segment of 6to4 addresses (`2002::/16`), which carry an IPv4 address next.
const SIX_TO_FOUR: u16 = 0x2002;

pub struct HttpPolicy {
    pub timeout: Option<Duration>,
    pub connect_timeout: Option<Duration>,
    pub read_timeout: Option<Duration>,
    pub redirect: Policy,
    pub reach: Reach,
}

impl HttpPolicy {
    pub fn new(redirect: Policy) -> Self {
        Self {
            timeout: None,
            connect_timeout: None,
            read_timeout: None,
            redirect,
            reach: Reach::Anywhere,
        }
    }
}

/// Which addresses a client may reach.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reach {
    #[default]
    Anywhere,
    /// Public addresses only: a host name counts by every address it resolves to, the client
    /// connects only to those, and no proxy resolves names where they cannot be checked. The
    /// client follows no redirect itself; its caller checks each target with
    /// [`check_public_url`] before requesting it. A Chat without a Folder reaches the network
    /// this way unless its host chooses otherwise.
    PublicAddresses,
}

/// A host that is, or resolves to, an address that is not public.
#[derive(Debug)]
pub struct NotPublic {
    /// The host name that resolved to `address`; `None` when the host is the address itself.
    name: Option<String>,
    address: IpAddr,
}

impl fmt::Display for NotPublic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(formatter, "{name} resolves to {}, which", self.address)?,
            None => write!(formatter, "{}", self.address)?,
        }
        write!(
            formatter,
            " is not a public address; this Chat reaches only public addresses"
        )
    }
}

impl std::error::Error for NotPublic {}

/// Refuses a URL whose host is an address that is not public; a host name is checked when it
/// resolves.
pub fn check_public_url(url: &Url) -> Result<(), NotPublic> {
    let Some(host) = url.host_str() else {
        return Ok(());
    };
    match host.trim_matches(['[', ']']).parse::<IpAddr>() {
        Ok(address) if !is_public(address) => Err(NotPublic {
            name: None,
            address,
        }),
        _ => Ok(()),
    }
}

/// The refusal among the causes of `error`, when an address that is not public stopped it.
pub fn not_public_refusal<'a>(
    error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a NotPublic> {
    std::iter::successors(Some(error), |error| error.source())
        .find_map(|error| error.downcast_ref::<NotPublic>())
}

pub fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => match embedded_ipv4(address) {
            Some(address) => is_public_v4(address),
            None => is_public_v6(address),
        },
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    let is_this_network = first == 0;
    let is_shared = first == 100 && (64..128).contains(&second);
    let is_protocol_assignment = first == 192 && second == 0 && third == 0;
    let is_benchmarking = first == 198 && (18..20).contains(&second);
    let is_reserved = first >= 240;
    !(address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_documentation()
        || is_this_network
        || is_shared
        || is_protocol_assignment
        || is_benchmarking
        || is_reserved)
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let [first, second, third, ..] = address.segments();
    let is_unique_local = first & 0xfe00 == 0xfc00;
    let is_link_local = first & 0xffc0 == 0xfe80;
    let is_site_local = first & 0xffc0 == 0xfec0;
    let is_documentation = first == 0x2001 && second == 0x0db8;
    let is_teredo = first == 0x2001 && second == 0;
    let is_local_nat64 = first == 0x64 && second == 0xff9b && third == 1;
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || is_unique_local
        || is_link_local
        || is_site_local
        || is_documentation
        || is_teredo
        || is_local_nat64)
}

/// The IPv4 address an IPv6 address reaches: IPv4-compatible (`::/96`, which holds `::` and
/// `::1`), IPv4-mapped (`::ffff:0:0/96`), IPv4-translated (`::ffff:0:0:0/96`), NAT64
/// (`64:ff9b::/96`) or 6to4 (`2002::/16`).
fn embedded_ipv4(address: Ipv6Addr) -> Option<Ipv4Addr> {
    let ipv4 = |high: u16, low: u16| Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
    match address.segments() {
        [0, 0, 0, 0, 0, 0 | 0xffff, high, low]
        | [0, 0, 0, 0, 0xffff, 0, high, low]
        | [0x64, 0xff9b, 0, 0, 0, 0, high, low] => Some(ipv4(high, low)),
        [SIX_TO_FOUR, high, low, ..] => Some(ipv4(high, low)),
        _ => None,
    }
}

/// Resolves a host name to its addresses only when every one of them is public.
struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .collect::<Vec<_>>();
            if let Some(address) = addresses.iter().find(|address| !is_public(address.ip())) {
                return Err(Box::new(NotPublic {
                    name: Some(host),
                    address: address.ip(),
                }) as _);
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxySource {
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
pub struct NetworkRoute {
    pub proxy_source: ProxySource,
    pub direct_websocket_eligible: bool,
}

pub fn build_http_client(
    target: &Url,
    policy: HttpPolicy,
) -> Result<(Client, NetworkRoute), reqwest::Error> {
    let route = match policy.reach {
        Reach::Anywhere => route_for(target),
        Reach::PublicAddresses => NetworkRoute {
            proxy_source: ProxySource::Direct,
            direct_websocket_eligible: true,
        },
    };
    let redirect = match policy.reach {
        Reach::Anywhere => policy.redirect,
        Reach::PublicAddresses => Policy::none(),
    };
    let mut builder = Client::builder().redirect(redirect);
    if let Some(timeout) = policy.timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(timeout) = policy.connect_timeout {
        builder = builder.connect_timeout(timeout);
    }
    if let Some(timeout) = policy.read_timeout {
        builder = builder.read_timeout(timeout);
    }
    if is_loopback(target) || policy.reach == Reach::PublicAddresses {
        builder = builder.no_proxy();
    }
    if policy.reach == Reach::PublicAddresses {
        builder = builder.dns_resolver(Arc::new(PublicResolver));
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

pub fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafeTarget {
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

pub struct NetworkDiagnostic {
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
    pub fn new(
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

    pub fn emit(&self) {
        if std::env::var(DIAGNOSTICS_ENV).as_deref() == Ok("1") {
            self.emit_to(&mut std::io::stderr().lock());
        }
    }

    /// [`Self::emit`] for a client that may reach `reach`. Diagnostics follow the process's
    /// configuration, which a client reaching public addresses only never reads, so it emits
    /// nothing.
    pub fn emit_for(&self, reach: Reach) {
        if reach == Reach::Anywhere {
            self.emit();
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

    #[test]
    fn only_public_addresses_are_public() {
        for address in [
            "127.0.0.1",
            "127.255.0.9",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "192.0.0.8",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "64:ff9b::10.0.0.1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "64:ff9b:1::808:808",
            "2002:7f00:1::",
            "2002:a00:1::1",
            "2002:c0a8:101::1",
            "2001::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "::ffff:0:127.0.0.1",
            "::ffff:0:10.0.0.1",
        ] {
            assert!(!is_public(address.parse().unwrap()), "{address}");
        }
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "100.128.0.1",
            "172.32.0.1",
            "::ffff:8.8.8.8",
            "::ffff:0:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
        ] {
            assert!(is_public(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn a_literal_address_that_is_not_public_is_refused_by_url() {
        for url in [
            "http://127.0.0.1:8080/",
            "https://10.0.0.1/",
            "https://[::1]/",
            "https://[fe80::1]/",
            "https://[2002:7f00:1::]/",
        ] {
            assert!(
                check_public_url(&Url::parse(url).unwrap()).is_err(),
                "{url}"
            );
        }
        assert!(check_public_url(&Url::parse("https://8.8.8.8/").unwrap()).is_ok());
        assert!(check_public_url(&Url::parse("https://example.com/").unwrap()).is_ok());
    }

    #[tokio::test]
    async fn a_name_resolving_to_an_address_that_is_not_public_is_refused_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let target = Url::parse(&format!("http://localhost:{port}/")).unwrap();
        let mut policy = HttpPolicy::new(Policy::limited(5));
        policy.reach = Reach::PublicAddresses;
        policy.timeout = Some(Duration::from_secs(3));
        let (client, route) = build_http_client(&target, policy).unwrap();
        assert_eq!(route.proxy_source, ProxySource::Direct);
        let error = client.get(target).send().await.unwrap_err();
        let refusal = not_public_refusal(&error).map(ToString::to_string);
        assert!(
            refusal.is_some_and(|refusal| refusal.starts_with("localhost resolves to ")),
            "{error:?}"
        );
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "no connection was made");
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
