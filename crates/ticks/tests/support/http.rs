use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

pub struct Stub {
    pub url: String,
    pub requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

#[allow(dead_code)]
pub struct Observation {
    pub at: Instant,
    pub connection: usize,
    pub phase: &'static str,
    pub endpoint: &'static str,
    pub center: &'static str,
    pub status: Option<u16>,
    pub write_ok: Option<bool>,
}

type Observer = Arc<dyn Fn(Observation) + Send + Sync>;

impl Stub {
    #[allow(dead_code)]
    pub async fn start(respond: impl Fn(&str) -> (u16, Value) + Send + Sync + 'static) -> Self {
        Self::start_with_disconnect(move |request| Some(respond(request))).await
    }

    #[allow(dead_code)]
    pub async fn start_with_disconnect(
        respond: impl Fn(&str) -> Option<(u16, Value)> + Send + Sync + 'static,
    ) -> Self {
        Self::start_observed(respond, None).await
    }

    #[allow(dead_code)]
    pub async fn start_with_observer(
        respond: impl Fn(&str) -> Option<(u16, Value)> + Send + Sync + 'static,
        observe: impl Fn(Observation) + Send + Sync + 'static,
    ) -> Self {
        Self::start_observed(respond, Some(Arc::new(observe))).await
    }

    async fn start_observed(
        respond: impl Fn(&str) -> Option<(u16, Value)> + Send + Sync + 'static,
        observer: Option<Observer>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let respond = Arc::new(respond);
        let task = tokio::spawn(async move {
            let mut connection = 0;
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                connection += 1;
                let accepted_at = Instant::now();
                let respond = respond.clone();
                let recorded = recorded.clone();
                let observer = observer.clone();
                tokio::spawn(async move {
                    if let Some(observer) = &observer {
                        observer(Observation {
                            at: accepted_at,
                            connection,
                            phase: "tcp-accepted",
                            endpoint: "unknown",
                            center: "unknown",
                            status: None,
                            write_ok: None,
                        });
                    }
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 4096];
                    loop {
                        let count = socket.read(&mut buffer).await.unwrap();
                        if count == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                        if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&bytes[..end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if bytes.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    let request = String::from_utf8(bytes).unwrap();
                    let endpoint = match request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                    {
                        Some("/oauth2/register") => "register",
                        Some("/oauth2/device/authorize") => "device-authorize",
                        Some("/oauth2/token") => "token",
                        _ => "other",
                    };
                    let center = if endpoint == "token" && request.contains("x-dc-region: ap") {
                        "ap"
                    } else if endpoint == "token" && request.contains("x-dc-region: us") {
                        "us"
                    } else {
                        "unknown"
                    };
                    let observe = |phase, status, write_ok| {
                        if let Some(observer) = &observer {
                            observer(Observation {
                                at: Instant::now(),
                                connection,
                                phase,
                                endpoint,
                                center,
                                status,
                                write_ok,
                            });
                        }
                    };
                    observe("request-accepted", None, None);
                    let response = respond(&request);
                    recorded.lock().unwrap().push(request);
                    let Some((status, body)) = response else {
                        drop(socket);
                        observe("connection-dropped", None, None);
                        return;
                    };
                    observe("response-planned", Some(status), None);
                    let body = body.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let result = socket.write_all(response.as_bytes()).await;
                    observe("response-write-ended", Some(status), Some(result.is_ok()));
                });
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.task.abort();
    }
}
