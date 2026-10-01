use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::auth::{AuthError, BrowserCallback, CodexAuth};

use super::{
    console::{Console, Menu},
    safe_text,
};

#[derive(Clone, Copy)]
pub(super) enum LoginMethod {
    Device,
    Browser,
}

pub(super) enum LoginUpdate {
    Device {
        address: String,
        code: String,
    },
    Browser {
        address: String,
        callback: BrowserCallback,
    },
    Finished(Result<String, String>),
}

pub(super) struct LoginFlow {
    id: Uuid,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    updates: mpsc::UnboundedReceiver<LoginUpdate>,
    browser: Option<(String, BrowserCallback)>,
}

impl LoginFlow {
    pub(super) fn start(
        root: PathBuf,
        console: &Console,
        method: LoginMethod,
    ) -> Result<Self, String> {
        let id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        Self::show(
            console,
            id,
            vec!["Starting official OpenAI authorization…".into()],
            None,
        )?;
        let task_cancel = cancel.clone();
        let (updates, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let auth = terminal_auth(root);
            let result = async {
                match method {
                    LoginMethod::Device => {
                        let login = auth.start_device_login(&task_cancel).await?;
                        let address = login.verification_uri.clone();
                        let _ = updates.send(LoginUpdate::Device {
                            address: address.clone(),
                            code: login.user_code.clone(),
                        });
                        let launch = launch_browser(&address);
                        let finish = auth.finish_device_login(login, &task_cancel);
                        tokio::pin!(launch, finish);
                        tokio::select! {
                            result = &mut finish => result,
                            () = &mut launch => finish.await,
                        }
                    }
                    LoginMethod::Browser => {
                        let login = auth.start_browser_login(&task_cancel).await?;
                        let address = login.authorization_url.clone();
                        let _ = updates.send(LoginUpdate::Browser {
                            address: address.clone(),
                            callback: login.manual_callback(),
                        });
                        let launch = launch_browser(&address);
                        let finish = auth.finish_browser_login(login, &task_cancel);
                        tokio::pin!(launch, finish);
                        tokio::select! {
                            result = &mut finish => result,
                            () = &mut launch => finish.await,
                        }
                    }
                }
            }
            .await;
            let _ = updates.send(LoginUpdate::Finished(
                result
                    .map(|credentials| credentials.account_id)
                    .map_err(|error| error.to_string()),
            ));
        });
        Ok(Self {
            id,
            cancel,
            task,
            updates: receiver,
            browser: None,
        })
    }

    pub(super) fn id(&self) -> Uuid {
        self.id
    }

    pub(super) async fn next(&mut self) -> Option<LoginUpdate> {
        self.updates.recv().await
    }

    pub(super) fn show_device(
        &self,
        console: &Console,
        address: &str,
        code: &str,
    ) -> Result<(), String> {
        Self::show(
            console,
            self.id,
            vec![
                "Opening your browser. If no page appears, open this link on your own device:".into(),
                safe_text(address),
                "Use your terminal's link shortcut (often Ctrl/Cmd+click), or copy the URL.".into(),
                format!("Enter code: {}", safe_text(code)),
                "If OpenAI asks you to enable device code authorization, enable it in ChatGPT Settings > Security and retry Device code sign-in from this Provider, or use Browser sign-in.".into(),
                "Waiting for official OpenAI authorization. Esc cancels.".into(),
            ],
            Some(address),
        )
    }

    pub(super) fn show_browser(
        &mut self,
        console: &Console,
        address: String,
        callback: BrowserCallback,
    ) -> Result<(), String> {
        self.browser = Some((address, callback));
        self.show_callback(console, None)
    }

    pub(super) fn submit_callback(
        &self,
        console: &Console,
        value: Option<&str>,
    ) -> Result<(), String> {
        let Some((_, callback)) = &self.browser else {
            return Ok(());
        };
        let result = value
            .filter(|value| !value.trim().is_empty())
            .map(|value| callback.submit(value.trim()));
        match result {
            Some(Ok(())) => {
                self.show_callback(console, Some("Callback accepted. Completing login…"))
            }
            Some(Err(error)) => self.show_callback(console, Some(&error.to_string())),
            None => self.show_callback(
                console,
                Some("Press Tab to enter the full callback URL, including state, then Enter."),
            ),
        }
    }

    pub(super) fn is_browser(&self) -> bool {
        self.browser.is_some()
    }

    fn show_callback(&self, console: &Console, notice: Option<&str>) -> Result<(), String> {
        let Some((address, _)) = &self.browser else {
            return Ok(());
        };
        let mut context = vec![
            "Opening your browser. If no page appears, open this link:".into(),
            safe_text(address),
            "Use your terminal's link shortcut (often Ctrl/Cmd+click), or copy the URL.".into(),
            "Complete authorization in your browser. For a remote terminal, paste the full redirect URL here, or cancel and choose Device code sign-in from this Provider.".into(),
            "Tab edits the callback field; Esc leaves editing, then cancels login.".into(),
        ];
        if let Some(notice) = notice {
            context.push(safe_text(notice));
        }
        if console.is_interactive() {
            console.show_login_menu(
                Menu {
                    id: self.id,
                    title: "Codex browser login".into(),
                    context,
                    options: vec!["Submit callback URL…".into(), "Cancel login".into()],
                    selected: 0,
                    confirmation: false,
                },
                address,
                true,
            )
        } else {
            console.write(&format!("{}\n", context.join("\n")))
        }
    }

    fn show(
        console: &Console,
        id: Uuid,
        context: Vec<String>,
        link: Option<&str>,
    ) -> Result<(), String> {
        if console.is_interactive() {
            console.set_prompt_enabled(false)?;
            let menu = Menu {
                id,
                title: "Log in to Codex subscription".into(),
                context,
                options: vec!["Cancel login".into()],
                selected: 0,
                confirmation: false,
            };
            match link {
                Some(address) => console.show_login_menu(menu, address, false),
                None => console.show_focused_menu(menu),
            }
        } else {
            console.write(&format!("{}\n", context.join("\n")))
        }
    }

    pub(super) fn close(self, console: &Console) -> Result<(), String> {
        self.cancel.cancel();
        console.finish_menu_submission(self.id)?;
        console.clear_menu(self.id)?;
        console.set_prompt_enabled(true)
    }
}

impl Drop for LoginFlow {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

pub(super) struct LogoutFlow {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), AuthError>>,
}

impl LogoutFlow {
    pub(super) fn start(root: PathBuf, console: &Console) -> Result<Self, String> {
        console.set_status("Removing Codex login… Esc cancels; input will run afterward")?;
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move { terminal_auth(root).logout(&task_cancel).await });
        Ok(Self { cancel, task })
    }

    pub(super) async fn next(&mut self) -> Result<(), AuthError> {
        (&mut self.task)
            .await
            .unwrap_or(Err(AuthError::Storage("logout task stopped")))
    }

    pub(super) fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for LogoutFlow {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

async fn launch_browser(address: &str) {
    #[cfg(not(test))]
    {
        let executable = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::process::Command::new(executable)
                .arg(address)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
    }
    #[cfg(test)]
    if std::env::var_os("TICKLOOP_TEST_BROWSER_AUTH").is_some() {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        client
            .get(address)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
}

pub(super) fn terminal_auth(root: PathBuf) -> CodexAuth {
    #[cfg(test)]
    if let Ok(origin) = std::env::var("TICKLOOP_TEST_CODEX_OAUTH") {
        return CodexAuth::new_for_loopback_test(root, &origin)
            .expect("test loopback OAuth origin");
    }
    CodexAuth::new(root)
}

#[cfg(test)]
mod tests {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use serde_json::json;
    use std::{
        io::{Read, Write},
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn terminal_child() {
        if std::env::var_os("TICKLOOP_TEST_CODEX_OAUTH").is_none() {
            return;
        }
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(super::super::run_with_arguments(
                ["--working-directory-is-trusted", "--mode", "acceptEdits"]
                    .into_iter()
                    .map(str::to_owned),
            ))
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pty_device_login_then_conversation_uses_saved_subscription() {
        pty_login_conversation(false, None).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pty_browser_login_then_conversation_uses_saved_subscription() {
        pty_login_conversation(true, None).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pty_login_keeps_an_explicit_model_even_before_it_is_authenticated() {
        pty_login_conversation(true, Some("gpt-5.6-sol")).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_and_failed_login_return_to_provider_before_exit() {
        let directory = tempfile::tempdir().unwrap();
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 150,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "terminal::login::tests::terminal_child",
            "--nocapture",
        ]);
        command.cwd(directory.path());
        command.env("TICKLOOP_CONFIG_DIR", directory.path().join("state"));
        command.env("TICKLOOP_TEST_CODEX_OAUTH", "http://127.0.0.1:37981");
        command.env("TICKLOOP_AGENT_TUI", "inline");
        let mut reader = pty.master.try_clone_reader().unwrap();
        let mut writer = pty.master.take_writer().unwrap();
        struct ChildGuard(Box<dyn portable_pty::Child + Send + Sync>);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
            }
        }
        let mut child = ChildGuard(pty.slave.spawn_command(command).unwrap());
        drop(pty.slave);
        let output = Arc::new(Mutex::new(String::new()));
        let captured = output.clone();
        let reading = std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                captured
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buffer[..count]));
            }
        });
        let wait = |needle: &str| {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !output.lock().unwrap().contains(needle) {
                assert!(Instant::now() < deadline, "PTY missing {needle}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait("Ask");
        writer.write_all(b"/model\r").unwrap();
        writer.flush().unwrap();
        wait("Choose Provider");
        writer.write_all(b"j\r").unwrap();
        writer.flush().unwrap();
        wait("Codex browser login");
        writer.write_all(b"\x1b").unwrap();
        writer.flush().unwrap();
        wait("Login cancelled");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let text = output.lock().unwrap();
            if let Some(cancelled) = text.rfind("Login cancelled")
                && text[cancelled..].contains("Choose Provider")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cancelled login did not return to Provider"
            );
            drop(text);
            std::thread::sleep(Duration::from_millis(20));
        }
        writer.write_all(b"d").unwrap();
        writer.flush().unwrap();
        wait("Cannot log in:");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let text = output.lock().unwrap();
            if let Some(failed) = text.rfind("Cannot log in:")
                && text[failed..].contains("Choose Provider")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "failed login did not return to Provider"
            );
            drop(text);
            std::thread::sleep(Duration::from_millis(20));
        }
        writer.write_all(b"\x1b").unwrap();
        writer.flush().unwrap();
        wait("Selection cancelled");
        writer.write_all(b"/exit\r").unwrap();
        writer.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "cancelled login blocked /exit");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(writer);
        drop(pty.master);
        reading.join().unwrap();
    }

    async fn pty_login_conversation(browser: bool, selected_model: Option<&'static str>) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        if let Some(model) = selected_model {
            let config = crate::model::cards::built_in_model_profiles()
                .into_iter()
                .find(|config| config.model == model)
                .unwrap();
            crate::settings::UserSettings::in_root(&root.join("state"))
                .save_model(&config, false)
                .unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut model_seen = false;
            let mut device_page_opened = false;
            while !model_seen {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let mut buffer = [0; 4096];
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                if headers.starts_with("GET /oauth/authorize?") {
                    let target = headers.split_whitespace().nth(1).unwrap();
                    let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
                    let params: std::collections::HashMap<_, _> =
                        url.query_pairs().into_owned().collect();
                    assert_eq!(params["code_challenge_method"], "S256");
                    assert!(!params["code_challenge"].is_empty());
                    let mut callback = reqwest::Url::parse(&params["redirect_uri"]).unwrap();
                    callback
                        .query_pairs_mut()
                        .append_pair("code", "browser-code")
                        .append_pair("state", &params["state"]);
                    stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {callback}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                    continue;
                }
                let (body, content_type) = if headers
                    .starts_with("POST /api/accounts/deviceauth/usercode ")
                {
                    (json!({"device_auth_id":"device-test","user_code":"ABCD-EFGH","interval":1}).to_string(), "application/json")
                } else if headers.starts_with("GET /codex/device ") {
                    device_page_opened = true;
                    ("Device authorization".into(), "text/html")
                } else if headers.starts_with("POST /api/accounts/deviceauth/token ") {
                    if !device_page_opened {
                        stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                        continue;
                    }
                    (
                        json!({"authorization_code":"test-code","code_verifier":"test-verifier"})
                            .to_string(),
                        "application/json",
                    )
                } else if headers.starts_with("POST /oauth/token ") {
                    (json!({"access_token":"ey.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC10ZXN0In19.sig","refresh_token":"fake-refresh-never-display","expires_in":3600}).to_string(), "application/json")
                } else {
                    assert!(headers.starts_with("POST /codex/responses "), "{headers}");
                    assert!(headers.contains("chatgpt-account-id: acct-test"));
                    let body: serde_json::Value =
                        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                    assert!(body["input"].to_string().contains("hello from terminal"));
                    assert_eq!(body["store"], false);
                    assert_eq!(body["model"], selected_model.unwrap_or("gpt-5.5"));
                    model_seen = true;
                    let text = json!({"type":"message","id":"msg_pty","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Subscription terminal connected","annotations":[]}]});
                    let events = [
                        json!({"type":"response.created","response":{"id":"resp_pty","status":"in_progress"}}),
                        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_pty","type":"message","role":"assistant","content":[]}}),
                        json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_pty","content_index":0,"delta":"Subscription terminal connected"}),
                        json!({"type":"response.output_item.done","output_index":0,"item":text}),
                        json!({"type":"response.completed","response":{"id":"resp_pty","status":"completed","output":[text],"usage":{"input_tokens":10,"output_tokens":3}}}),
                    ];
                    (
                        events
                            .iter()
                            .map(|event| format!("data: {event}\n\n"))
                            .collect::<String>(),
                        "text/event-stream",
                    )
                };
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
            assert!(model_seen);
            assert!(browser || device_page_opened);
        });
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 150,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "terminal::login::tests::terminal_child",
            "--nocapture",
        ]);
        command.cwd(&root);
        command.env("TICKLOOP_CONFIG_DIR", root.join("state"));
        command.env("TICKLOOP_TEST_CODEX_OAUTH", &origin);
        command.env(
            "TICKLOOP_TEST_CODEX_RESPONSES",
            format!("{origin}/codex/responses"),
        );
        command.env("TICKLOOP_AGENT_TUI", "inline");
        command.env("TICKLOOP_TEST_BROWSER_AUTH", "1");
        let mut reader = pty.master.try_clone_reader().unwrap();
        let mut writer = pty.master.take_writer().unwrap();
        struct ChildGuard(Box<dyn portable_pty::Child + Send + Sync>);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
            }
        }
        let mut child = ChildGuard(pty.slave.spawn_command(command).unwrap());
        drop(pty.slave);
        let output = Arc::new(Mutex::new(String::new()));
        let captured = output.clone();
        let reading = std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                captured
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
        let wait = |needle: &str| {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let text = output.lock().unwrap().clone();
                if ansi.replace_all(&text, "").contains(needle) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "PTY missing {needle}: {}",
                    ansi.replace_all(&text, "")
                        .chars()
                        .rev()
                        .take(2500)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect::<String>()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let wait_after = |needle: &str, offset: usize| {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let text = output.lock().unwrap().clone();
                if ansi
                    .replace_all(text.get(offset..).unwrap_or(""), "")
                    .contains(needle)
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "PTY missing {needle} after action"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait("Ask");
        writer.write_all(b"/model\r").unwrap();
        writer.flush().unwrap();
        wait("Choose Provider");
        let keys: &[u8] = match (selected_model.is_some(), browser) {
            (true, _) => b"\r",
            (false, true) => b"j\r",
            (false, false) => b"jd",
        };
        writer.write_all(keys).unwrap();
        writer.flush().unwrap();
        wait("login saved");
        wait("Codex subscription");
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let text = output.lock().unwrap().clone();
                let plain = ansi.replace_all(&text, "");
                // A frame carries only the rows that changed, so the picker is
                // back once its title was drawn after the login dialog last was.
                let picker_returned = plain.rfind("Choose model").is_some_and(|picker| {
                    plain
                        .rfind("Codex browser login")
                        .is_none_or(|login| login < picker)
                });
                if picker_returned {
                    assert!(
                        !text.contains("Model selection:"),
                        "login must not apply a model before confirmation"
                    );
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "login did not return to Provider models"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            if browser && selected_model.is_none() {
                let before_back = output.lock().unwrap().len();
                writer.write_all(b"\x1b").unwrap();
                writer.flush().unwrap();
                wait_after("Choose Provider", before_back);
                let before_reauthentication = output.lock().unwrap().len();
                writer.write_all(b"r").unwrap();
                writer.flush().unwrap();
                wait_after("Codex browser login", before_reauthentication);
                wait_after("login saved", before_reauthentication);
                wait_after("Choose model", before_reauthentication);
            }
            writer.write_all(b"\r").unwrap();
            writer.flush().unwrap();
            wait("Model selection:");
        }
        writer.write_all(b"hello from terminal\r").unwrap();
        writer.flush().unwrap();
        wait("Subscription terminal connected");
        writer.write_all(b"/exit\r").unwrap();
        writer.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() > deadline {
                child.0.kill().unwrap();
                panic!("terminal did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(writer);
        drop(pty.master);
        reading.join().unwrap();
        assert!(
            !output
                .lock()
                .unwrap()
                .contains("fake-refresh-never-display")
        );
        server.await.unwrap();
    }
}
