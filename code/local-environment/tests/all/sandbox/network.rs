use super::*;

#[tokio::test]
async fn sandbox_network_allowlist_reaches_http_service() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = TestHttpServer::start(
        listener,
        b"HTTP/1.1 200 OK\r\nContent-Length: 17\r\nConnection: close\r\n\r\nallowlisted hello",
    );
    let fixture = SandboxFixture::new(serde_json::json!({
        "enabled":true,"failIfUnavailable":true,
        "network":{"allowedDomains":["localhost"]}
    }));
    let prepared = fixture
        .prepare(&format!(
            "/usr/bin/curl --fail --max-time 5 http://localhost:{port}/"
        ))
        .await;
    let isolation = prepared.0.approval_facts().isolation.unwrap();
    let result = fixture.run(prepared).await;
    drop(server);
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("allowlisted hello"),
        "{}",
        result.content
    );
    assert_eq!(isolation.network, NetworkIsolation::Allowlist);
}

#[tokio::test]
async fn sandbox_network_allowlist_reports_denied_domain_even_when_shell_succeeds() {
    let fixture = SandboxFixture::new(serde_json::json!({
        "enabled":true,"failIfUnavailable":true,
        "network":{"allowedDomains":["localhost"]}
    }));
    let prepared = fixture.prepare(
        "/usr/bin/curl --fail --silent --max-time 5 http://denied.example/ >/dev/null 2>&1; exit 0"
    ).await;
    let result = fixture.run(prepared).await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("被沙箱拒绝的域名: denied.example"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn sandbox_network_allowlist_tunnels_wildcard_destinations_but_blocks_bypasses() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = TestHttpServer::start(
        listener,
        b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\ntunnel hello",
    );
    let fixture = SandboxFixture::new(serde_json::json!({
        "enabled":true,"failIfUnavailable":true,
        "network":{"allowedDomains":["*.LOCALHOST"]}
    }));
    let prepared = fixture
        .prepare(&format!(
            "/usr/bin/curl --fail --silent --max-time 5 --proxytunnel http://sub.localhost:{port}/"
        ))
        .await;
    std::fs::write(
        fixture.settings.file_path(),
        r#"{"sandbox":{"enabled":true,"network":{"allowedDomains":["other.example"]}}}"#,
    )
    .unwrap();
    let result = fixture.run(prepared).await;
    drop(server);
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("tunnel hello"),
        "{}",
        result.content
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let command = format!(
        "/usr/bin/curl --fail --silent --max-time 2 --noproxy '*' http://127.0.0.1:{port}/"
    );
    let result = fixture.run(fixture.prepare(&command).await).await;
    assert!(result.is_error, "{}", result.content);
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    let result = fixture.run(fixture.prepare(&format!(
        "/usr/bin/curl --fail --silent --max-time 2 --proxytunnel http://sub.localhost:{port}/"
    )).await).await;
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("被沙箱拒绝的域名: sub.localhost"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn sandbox_network_allowlist_does_not_expose_ipv6_at_the_proxy_port() {
    let fixture = SandboxFixture::new(serde_json::json!({
        "enabled":true,"failIfUnavailable":true,
        "network":{"allowedDomains":["localhost"]}
    }));
    let folder = fixture.workspace.folder.as_ref().unwrap();
    let prepared = fixture.prepare(
        r#"printf '%s' "${http_proxy##*:}" > proxy-port; while [ ! -f ready ]; do /bin/sleep 0.01; done; /usr/bin/curl -g --noproxy '*' --fail --silent --max-time 2 "http://[::1]:${http_proxy##*:}/""#
    ).await;
    let peer = async {
        let port = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(folder.join("proxy-port"))
                    && let Ok(port) = value.parse::<u16>()
                {
                    break port;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port))
            .await
            .unwrap();
        let server = TestHttpServer::start(
            listener,
            b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nbypass hello",
        );
        std::fs::write(folder.join("ready"), "").unwrap();
        server
    };
    let (result, server) = tokio::join!(fixture.run(prepared), peer);
    drop(server);
    assert!(result.is_error, "{}", result.content);
    assert!(
        !result.content.contains("bypass hello"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn sandbox_network_allowlist_supports_verified_https() {
    use std::{os::fd::OwnedFd, process::Stdio};
    let fixture = SandboxFixture::new(serde_json::json!({
        "enabled":true,"failIfUnavailable":true,
        "network":{"allowedDomains":["localhost"]}
    }));
    let root = fixture.root.path();
    let config = root.join("openssl.cnf");
    let certificate = root.join("cert.pem");
    let key = root.join("key.pem");
    std::fs::write(&config, "[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN=localhost\n[ext]\nsubjectAltName=DNS:localhost\nbasicConstraints=critical,CA:TRUE\n").unwrap();
    let output = tokio::process::Command::new("/usr/bin/openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-config",
        ])
        .arg(config)
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&certificate)
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut server = tokio::process::Command::new("/usr/bin/python3")
        .arg("-c").arg(r#"
import socket, ssl, sys
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(sys.argv[1], sys.argv[2])
with socket.socket(fileno=0) as listener:
    connection, _ = listener.accept()
    with context.wrap_socket(connection, server_side=True) as stream:
        stream.recv(4096)
        stream.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\ntls hello')
"#)
        .arg(&certificate).arg(&key)
        .stdin(Stdio::from(OwnedFd::from(listener)))
        .stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    let command = format!(
        "/usr/bin/curl --fail --show-error --max-time 5 --cacert '{}' https://localhost:{port}/",
        certificate.display()
    );
    let result = fixture.run(fixture.prepare(&command).await).await;
    if result.is_error {
        let _ = server.kill().await;
    }
    let server = server.wait_with_output().await.unwrap();
    assert!(
        !result.is_error,
        "{}; TLS server: {}",
        result.content,
        String::from_utf8_lossy(&server.stderr)
    );
    assert!(
        server.status.success(),
        "{}",
        String::from_utf8_lossy(&server.stderr)
    );
    assert!(result.content.contains("tls hello"), "{}", result.content);
}

#[tokio::test]
async fn sandbox_network_allowlist_rechecks_redirects_and_wildcard_boundaries() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = TestHttpServer::start(listener, b"HTTP/1.1 302 Found\r\nLocation: http://denied.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let fixture = SandboxFixture::new(serde_json::json!({
        "enabled":true,"failIfUnavailable":true,
        "network":{"allowedDomains":["localhost", "*.example.test"]}
    }));
    let result = fixture
        .run(
            fixture
                .prepare(&format!(
                    "/usr/bin/curl --fail --silent --location --max-time 5 http://localhost:{port}/"
                ))
                .await,
        )
        .await;
    drop(server);
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("被沙箱拒绝的域名: denied.invalid"),
        "{}",
        result.content
    );
    for destination in [
        "http://example.test/",
        "http://notexample.test/",
        "https://example.test/",
    ] {
        let result = fixture
            .run(
                fixture
                    .prepare(&format!(
                        "/usr/bin/curl --fail --silent --max-time 5 '{destination}'"
                    ))
                    .await,
            )
            .await;
        assert!(result.is_error, "{}", result.content);
        let host = destination
            .split_once("://")
            .unwrap()
            .1
            .trim_end_matches('/');
        assert!(
            result
                .content
                .contains(&format!("被沙箱拒绝的域名: {host}")),
            "{}",
            result.content
        );
    }
}

#[tokio::test]
#[ignore = "live npm registry acceptance; run explicitly outside the agent harness sandbox"]
async fn sandbox_network_allowlist_installs_a_small_npm_package() {
    let fixture = SandboxFixture::new(serde_json::json!({
        "enabled":true,"failIfUnavailable":true,
        "network":{"allowedDomains":["registry.npmjs.org"]}
    }));
    let prepared = fixture.prepare(
        r#"npm install --ignore-scripts --no-audit --no-fund --package-lock=false --cache ./npm-cache is-number@7.0.0 && node -e "if (!require('is-number')(42)) process.exit(1)""#
    ).await;
    let result = fixture.run(prepared).await;
    assert!(!result.is_error, "{}", result.content);
}

struct TestHttpServer {
    task: tokio::task::JoinHandle<()>,
}

#[tokio::test]
async fn sandbox_network_allowlist_cannot_be_replaced_after_a_detached_descendant_survives() {
    let fixture = SandboxFixture::new(serde_json::json!({
        "enabled":true,"failIfUnavailable":true,
        "network":{"allowedDomains":["allowed.invalid"]}
    }));
    let folder = fixture.workspace.folder.as_ref().unwrap();
    std::fs::write(folder.join("detached.py"), r#"
import os, pathlib, socket, time
root = pathlib.Path.cwd()
port = int(os.environ['http_proxy'].rsplit(':', 1)[1])
deadline = time.monotonic() + 8
if os.fork():
    while not (root / 'detached-ready').exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    os._exit(0)
os.setsid()
(root / 'detached-ready').write_text(str(port))
while not (root / 'probe-now').exists() and time.monotonic() < deadline:
    time.sleep(0.01)
if not (root / 'probe-now').exists():
    (root / 'detached-result').write_text('fixture synchronization timed out')
    (root / 'detached-done').touch()
    os._exit(1)
try:
    with socket.create_connection(('127.0.0.1', port), timeout=1) as stream:
        stream.sendall(b'GET http://denied.invalid/ HTTP/1.1\r\nHost: denied.invalid\r\nConnection: close\r\n\r\n')
        chunks = []
        while True:
            chunk = stream.recv(4096)
            if not chunk:
                break
            chunks.append(chunk)
        (root / 'detached-result').write_bytes(b''.join(chunks))
except OSError as error:
    (root / 'detached-result').write_text(str(error))
(root / 'detached-done').touch()
os._exit(0)
"#).unwrap();
    let result = fixture
        .run(fixture.prepare("/usr/bin/python3 detached.py").await)
        .await;
    assert!(!result.is_error, "{}", result.content);
    let port = std::fs::read_to_string(folder.join("detached-ready"))
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let listener = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(listener) =
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
            {
                break listener;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let server = TestHttpServer::start(
        listener,
        b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\nretired proxy bypass",
    );
    std::fs::write(folder.join("probe-now"), "").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !folder.join("detached-done").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(server);
    let result = std::fs::read_to_string(folder.join("detached-result")).unwrap();
    assert!(
        !result.contains("fixture synchronization timed out"),
        "{result}"
    );
    assert!(!result.contains("retired proxy bypass"), "{result}");
}

impl TestHttpServer {
    fn start(listener: tokio::net::TcpListener, response: &'static [u8]) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < MAX_REQUEST_HEADER_BYTES);
                header.push(stream.read_u8().await.unwrap());
            }
            stream.write_all(response).await.unwrap();
        });
        Self { task }
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
