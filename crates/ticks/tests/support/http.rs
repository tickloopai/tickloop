use serde_json::Value;
use std::sync::{Arc, Mutex};
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

impl Stub {
    pub async fn start(respond: impl Fn(&str) -> (u16, Value) + Send + Sync + 'static) -> Self {
        Self::start_with_disconnect(move |request| Some(respond(request))).await
    }

    pub async fn start_with_disconnect(
        respond: impl Fn(&str) -> Option<(u16, Value)> + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let respond = Arc::new(respond);
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let respond = respond.clone();
                let recorded = recorded.clone();
                tokio::spawn(async move {
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
                    let response = respond(&request);
                    recorded.lock().unwrap().push(request);
                    let Some((status, body)) = response else {
                        return;
                    };
                    let body = body.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
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
