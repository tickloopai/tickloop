use std::{collections::HashMap, path::PathBuf};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use tickloop_agent_model::auth::{
    AuthError, BrowserCallback, CodexAuth,
    copilot::{self, CopilotAuth},
    minimax::{MiniMaxAuth, MiniMaxRegion},
};

use super::{
    console::{Console, Menu},
    safe_text,
};
use tickloop_agent_protocol::ModelAuthSource;

#[derive(Clone, Copy)]
pub(super) enum LoginMethod {
    Artifact,
    Device,
    Browser,
    MiniMax(MiniMaxRegion),
    Copilot,
    Google,
    Subscription(tickloop_agent_model::auth::device_subscriptions::DeviceSubscription),
}

impl LoginMethod {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Artifact => "Artifacts",
            Self::MiniMax(region) => region.label(),
            Self::Copilot => "GitHub Copilot",
            Self::Google => "Google OAuth",
            Self::Subscription(provider) => provider.label(),
            Self::Device | Self::Browser => "Codex subscription",
        }
    }
}

/// The local sign-in a logout removes and a reauthentication restarts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SignInTarget {
    #[default]
    Codex,
    MiniMax(MiniMaxRegion),
    Copilot,
    Google,
    Subscription(tickloop_agent_model::auth::device_subscriptions::DeviceSubscription),
}

impl SignInTarget {
    pub(super) fn from_auth(source: ModelAuthSource) -> Option<Self> {
        if source == ModelAuthSource::GoogleOAuth {
            return Some(Self::Google);
        }
        if let Some(provider) =
            tickloop_agent_model::auth::device_subscriptions::DeviceSubscription::from_auth(source)
        {
            return Some(Self::Subscription(provider));
        }
        if source == ModelAuthSource::GithubCopilot {
            return Some(Self::Copilot);
        }
        MiniMaxRegion::from_auth(source).map(Self::MiniMax)
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::MiniMax(region) => region.label(),
            Self::Copilot => "GitHub Copilot",
            Self::Google => "Google OAuth",
            Self::Subscription(provider) => provider.label(),
        }
    }

    pub(super) fn login_method(self) -> LoginMethod {
        match self {
            Self::Codex => LoginMethod::Browser,
            Self::MiniMax(region) => LoginMethod::MiniMax(region),
            Self::Copilot => LoginMethod::Copilot,
            Self::Google => LoginMethod::Google,
            Self::Subscription(provider) => LoginMethod::Subscription(provider),
        }
    }
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
    chat: Option<Uuid>,
    method: LoginMethod,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    updates: mpsc::UnboundedReceiver<LoginUpdate>,
    browser: Option<(String, BrowserCallback)>,
    completed: bool,
}

pub(super) struct LoginSession {
    pub(super) flow: LoginFlow,
    pub(super) from_provider: bool,
    pub(super) succeeded: bool,
}

pub(super) fn active_login_id(
    sessions: &HashMap<Uuid, LoginSession>,
    console: &Console,
) -> Result<Option<Uuid>, String> {
    if console.is_interactive() {
        Ok(console
            .dialog_menu_id()?
            .filter(|id| sessions.contains_key(id)))
    } else {
        let chat = console.dialog_chat()?;
        Ok(sessions
            .iter()
            .find(|(_, session)| session.flow.chat() == chat)
            .map(|(id, _)| *id))
    }
}

pub(super) async fn next_login_update(
    sessions: &mut HashMap<Uuid, LoginSession>,
) -> (Uuid, Option<LoginUpdate>) {
    use futures_util::FutureExt;
    let updates = sessions
        .iter_mut()
        .filter(|(_, session)| session.flow.updates_pending())
        .map(|(id, session)| {
            let id = *id;
            async move { (id, session.flow.next().await) }.boxed()
        });
    futures_util::future::select_all(updates).await.0
}

pub(super) fn complete_provider_login(
    flow: &mut LoginFlow,
    console: &Console,
    result: &Result<String, String>,
) -> Result<(), String> {
    flow.completed = true;
    flow.cancel.cancel();
    flow.task.abort();
    let (status, notice) = match result {
        Ok(account) => (
            "Login saved",
            format!("Login saved · {}", safe_text(account)),
        ),
        Err(error) => (
            "Login failed",
            format!("Cannot log in: {}", safe_text(error)),
        ),
    };
    console.complete_login_menu(flow.id, status, &notice)
}

impl LoginFlow {
    pub(super) fn start(
        root: PathBuf,
        console: &Console,
        method: LoginMethod,
    ) -> Result<Self, String> {
        let id = Uuid::new_v4();
        let chat = console.dialog_chat()?;
        let cancel = CancellationToken::new();
        Self::show(
            console,
            id,
            vec![format!("Starting {} authorization…", method.label())],
            None,
            method.label(),
        )?;
        let task_cancel = cancel.clone();
        let (updates, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            if let LoginMethod::Artifact = method {
                let result = async {
                    let client = tickloop_agent_artifacts::ArtifactClient::new(root)
                        .map_err(|error| error.to_string())?;
                    let device = client
                        .start_device_login()
                        .await
                        .map_err(|error| error.to_string())?;
                    let _ = updates.send(LoginUpdate::Device {
                        address: device.verification_uri.clone(),
                        code: device.user_code.clone(),
                    });
                    client
                        .finish_device_login(device, &task_cancel)
                        .await
                        .map(|user| user.display_name().to_owned())
                        .map_err(|error| error.to_string())
                }
                .await;
                let _ = updates.send(LoginUpdate::Finished(result));
                return;
            }
            if let LoginMethod::Google = method {
                let auth = tickloop_agent_model::auth::google::GoogleAuth::new(root);
                let result = async {
                    let login = auth.start_browser_login(&task_cancel).await?;
                    let address = login.browser.authorization_url.clone();
                    let _ = updates.send(LoginUpdate::Browser { address: address.clone(), callback: login.browser.manual_callback() });
                    let launch = launch_browser(&address);
                    let finish = auth.finish_browser_login(login, &task_cancel);
                    tokio::pin!(launch, finish);
                    tokio::select! { result = &mut finish => result, () = &mut launch => finish.await }
                }.await;
                let _ = updates.send(LoginUpdate::Finished(
                    result.map_err(|error: tickloop_agent_model::ModelError| error.to_string()),
                ));
                return;
            }
            if let LoginMethod::Subscription(provider) = method {
                let auth = tickloop_agent_model::auth::device_subscriptions::SubscriptionAuth::new(
                    root, provider,
                );
                let result = async {
                    let login = auth.start_device_login(&task_cancel).await?;
                    let address = login.verification_uri.clone();
                    let _ = updates.send(LoginUpdate::Device { address: address.clone(), code: login.user_code.clone() });
                    let launch = launch_browser(&address);
                    let finish = auth.finish_device_login(login, &task_cancel);
                    tokio::pin!(launch, finish);
                    tokio::select! { result = &mut finish => result, () = &mut launch => finish.await }
                }.await;
                let _ = updates.send(LoginUpdate::Finished(
                    result
                        .map(|access| access.identity.to_string())
                        .map_err(|error: tickloop_agent_model::ModelError| error.to_string()),
                ));
                return;
            }
            if let LoginMethod::MiniMax(region) = method {
                let auth = MiniMaxAuth::new(root, region);
                let result = async {
                    let login = auth.start_device_login(&task_cancel).await?;
                    let address = login.verification_uri.clone();
                    let _ = updates.send(LoginUpdate::Device { address: address.clone(), code: login.user_code.clone() });
                    let launch = launch_browser(&address);
                    let finish = auth.finish_device_login(login, &task_cancel);
                    tokio::pin!(launch, finish);
                    tokio::select! { result = &mut finish => result, () = &mut launch => finish.await }
                }.await;
                let _ = updates.send(LoginUpdate::Finished(
                    result
                        .map(|_| region.label().to_owned())
                        .map_err(|error| error.to_string()),
                ));
                return;
            }
            if let LoginMethod::Copilot = method {
                let auth = CopilotAuth::new(root);
                let result = async {
                    let login = auth.start_device_login(&task_cancel).await?;
                    let address = login.verification_uri.clone();
                    let _ = updates.send(LoginUpdate::Device { address: address.clone(), code: login.user_code.clone() });
                    let launch = launch_browser(&address);
                    let finish = auth.finish_device_login(login, &task_cancel);
                    tokio::pin!(launch, finish);
                    tokio::select! { result = &mut finish => result, () = &mut launch => finish.await }
                }.await;
                let _ = updates.send(LoginUpdate::Finished(
                    result
                        .map(|_| method.label().to_owned())
                        .map_err(|error| error.to_string()),
                ));
                return;
            }
            let auth = terminal_auth(root);
            let result = async {
                match method {
                    LoginMethod::MiniMax(_)
                    | LoginMethod::Artifact
                    | LoginMethod::Copilot
                    | LoginMethod::Google
                    | LoginMethod::Subscription(_) => {
                        unreachable!("device-only sign-ins return before Codex login")
                    }
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
            chat,
            method,
            cancel,
            task,
            updates: receiver,
            browser: None,
            completed: false,
        })
    }

    pub(super) fn label(&self) -> &'static str {
        self.method.label()
    }

    pub(super) fn id(&self) -> Uuid {
        self.id
    }

    pub(super) fn chat(&self) -> Option<Uuid> {
        self.chat
    }

    pub(super) async fn next(&mut self) -> Option<LoginUpdate> {
        self.updates.recv().await
    }

    pub(super) fn updates_pending(&self) -> bool {
        !self.completed
    }

    pub(super) fn completed(&self) -> bool {
        self.completed
    }

    #[cfg(test)]
    pub(super) fn start_fixture(
        console: &Console,
        method: LoginMethod,
    ) -> Result<(Self, mpsc::UnboundedSender<LoginUpdate>), String> {
        let id = Uuid::new_v4();
        let chat = console.dialog_chat()?;
        Self::show(
            console,
            id,
            vec!["Controlled authorization fixture".into()],
            None,
            method.label(),
        )?;
        let (sender, updates) = mpsc::unbounded_channel();
        Ok((
            Self {
                id,
                chat,
                method,
                cancel: CancellationToken::new(),
                task: tokio::spawn(std::future::pending()),
                updates,
                browser: None,
                completed: false,
            },
            sender,
        ))
    }

    pub(super) fn show_device(
        &self,
        console: &Console,
        address: &str,
        code: &str,
    ) -> Result<(), String> {
        let mut context = vec![
            if matches!(self.method, LoginMethod::Artifact) {
                "Open this Tick Loop Artifacts link in your own browser:".into()
            } else {
                "Opening your browser. If no page appears, open this link on your own device:"
                    .into()
            },
            safe_text(address),
            "Use your terminal's link shortcut (often ctrl/cmd+click), or copy the URL.".into(),
            format!("Enter code: {}", safe_text(code)),
        ];
        if let LoginMethod::Artifact = self.method {
            context.push("In your browser, sign in with Google and approve this device for your Tick Loop Artifacts account.".into());
        } else if let LoginMethod::MiniMax(_) = self.method {
            context.push("Approve access on the MiniMax Portal. The consent page identifies the registered Portal client.".into());
        } else if let LoginMethod::Subscription(provider) = self.method {
            context.push(provider.consent_notice().into());
        } else if let LoginMethod::Copilot = self.method {
            context.push(copilot::CONSENT_NOTICE.into());
        } else {
            context.push("If OpenAI asks you to enable device code authorization, enable it in ChatGPT Settings > Security and retry Device code sign-in from this Provider, or use Browser sign-in.".into());
        }
        context.push(format!(
            "Waiting for {} authorization. esc cancels.",
            self.method.label()
        ));
        Self::show(
            console,
            self.id,
            context,
            Some(address),
            self.method.label(),
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
                Some("Press tab to enter the full callback URL, including state, then enter."),
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
            "Use your terminal's link shortcut (often ctrl/cmd+click), or copy the URL.".into(),
            if matches!(self.method, LoginMethod::Google) {
                "Authorize your own Google Cloud client and project. For a remote terminal, paste the complete redirect URL here.".into()
            } else {
                "Complete authorization in your browser. For a remote terminal, paste the full redirect URL here, or cancel and choose Device code sign-in from this Provider.".into()
            },
            "tab edits the callback field; esc leaves editing, then cancels login.".into(),
        ];
        if let Some(notice) = notice {
            context.push(safe_text(notice));
        }
        if console.is_interactive() {
            console.show_login_menu(
                Menu {
                    id: self.id,
                    title: if matches!(self.method, LoginMethod::Google) {
                        "Google OAuth browser login".into()
                    } else {
                        "Codex browser login".into()
                    },
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
        label: &str,
    ) -> Result<(), String> {
        if console.is_interactive() {
            let menu = Menu {
                id,
                title: format!("Log in to {label}"),
                context,
                options: vec!["Cancel login".into()],
                selected: 0,
                confirmation: false,
            };
            match link {
                Some(address) => console.show_login_menu(menu, address, false),
                None => console.show_picker_menu(menu, super::console::PickerKind::Actions),
            }
        } else {
            console.write(&format!("{}\n", context.join("\n")))
        }
    }

    pub(super) fn close(self, console: &Console) -> Result<(), String> {
        self.cancel.cancel();
        console.finish_menu_submission(self.id)?;
        console.clear_menu(self.id)
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
    pub(super) fn start(
        root: PathBuf,
        console: &Console,
        target: SignInTarget,
    ) -> Result<Self, String> {
        console.set_status(&format!(
            "Removing {} login… esc cancels; input will run afterward",
            target.label()
        ))?;
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            match target {
                SignInTarget::MiniMax(region) => MiniMaxAuth::new(root, region)
                    .logout(&task_cancel)
                    .await
                    .map_err(|error| match error {
                        tickloop_agent_model::auth::minimax::MiniMaxAuthError::Cancelled => {
                            AuthError::Cancelled
                        }
                        _ => AuthError::MiniMax(error.to_string()),
                    }),
                SignInTarget::Copilot => {
                    CopilotAuth::new(root)
                        .logout(&task_cancel)
                        .await
                        .map_err(|error| match error {
                            copilot::CopilotAuthError::Cancelled => AuthError::Cancelled,
                            _ => AuthError::Copilot(error.to_string()),
                        })
                }
                SignInTarget::Google => tickloop_agent_model::auth::google::GoogleAuth::new(root)
                    .logout(&task_cancel)
                    .await
                    .map_err(|error| AuthError::Provider(error.to_string())),
                SignInTarget::Subscription(provider) => {
                    tickloop_agent_model::auth::device_subscriptions::SubscriptionAuth::new(
                        root, provider,
                    )
                    .logout(&task_cancel)
                    .await
                    .map_err(|error| AuthError::Provider(error.to_string()))
                }
                SignInTarget::Codex => terminal_auth(root).logout(&task_cancel).await,
            }
        });
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
        let _ = launch_browser_result(address).await;
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

pub(super) async fn open_artifact(address: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(address).map_err(|_| "Invalid artifact link".to_owned())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Only HTTPS artifact links can be opened".into());
    }
    launch_browser_result(address).await
}

async fn launch_browser_result(address: &str) -> Result<(), String> {
    let executable = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let result = tokio::time::timeout(
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
    match result {
        Ok(Ok(status)) if status.success() => Ok(()),
        _ => Err("Cannot open artifact in the browser".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_test_support::{input_dialog, screen};

    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::{
        io::Write,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Each PTY test re-executes this test binary and starts an Agent daemon and worker; on a
    /// machine busy with parallel test runs that has taken more than 20 seconds. Only a failure
    /// waits this long.
    const PTY_TIMEOUT: Duration = Duration::from_secs(90);

    const TERMINAL_CHILD_STACK_BYTES: usize = 8 * 1024 * 1024;
    const TERMINAL_ESCAPE_KEY: &[u8] = b"\x1b[27u";
    const SYNCHRONIZED_FRAME_END: &str = "\x1b[?2026l";
    const TERMINAL_FRAME_SETTLE: Duration = Duration::from_millis(40);

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

    #[tokio::test]
    async fn login_sessions_route_background_updates_without_owning_another_agent_input() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let console = Console::test_interactive_for_chat(first);
        console.set_prompt_enabled(true).unwrap();
        let (first_flow, first_sender) =
            LoginFlow::start_fixture(&console, LoginMethod::Device).unwrap();
        let first_id = first_flow.id();
        console.select_chat(Some(second)).unwrap();
        let (second_flow, second_sender) =
            LoginFlow::start_fixture(&console, LoginMethod::Device).unwrap();
        let second_id = second_flow.id();
        let mut sessions = HashMap::from([
            (
                first_id,
                LoginSession {
                    flow: first_flow,
                    from_provider: true,
                    succeeded: false,
                },
            ),
            (
                second_id,
                LoginSession {
                    flow: second_flow,
                    from_provider: true,
                    succeeded: false,
                },
            ),
        ]);
        assert_eq!(
            active_login_id(&sessions, &console).unwrap(),
            Some(second_id)
        );
        first_sender
            .send(LoginUpdate::Finished(Ok("First account".into())))
            .unwrap();
        let (id, update) = next_login_update(&mut sessions).await;
        assert_eq!(id, first_id);
        let Some(LoginUpdate::Finished(result)) = update else {
            panic!("Expected first login result");
        };
        complete_provider_login(&mut sessions.get_mut(&id).unwrap().flow, &console, &result)
            .unwrap();
        assert_eq!(console.dialog_menu_id().unwrap(), Some(second_id));
        second_sender
            .send(LoginUpdate::Finished(Ok("Second account".into())))
            .unwrap();
        let (id, _) = next_login_update(&mut sessions).await;
        assert_eq!(id, second_id, "Completed sessions must stop polling");
        console.select_chat(Some(Uuid::new_v4())).unwrap();
        assert_eq!(active_login_id(&sessions, &console).unwrap(), None);
        console.select_chat(Some(first)).unwrap();
        assert_eq!(
            active_login_id(&sessions, &console).unwrap(),
            Some(first_id)
        );
        assert_eq!(console.dialog_menu_id().unwrap(), Some(first_id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pty_device_login_then_conversation_uses_saved_subscription() {
        pty_login_conversation(false, None, false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pty_browser_login_then_conversation_uses_saved_subscription() {
        pty_login_conversation(true, None, false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pty_login_keeps_an_explicit_model_even_before_it_is_authenticated() {
        pty_login_conversation(true, Some("gpt-5.6-sol"), false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pty_login_then_conversation_uses_current_codex_sol() {
        pty_login_conversation(true, Some("gpt-6.1-sol"), false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pty_login_then_conversation_uses_current_codex_luna() {
        pty_login_conversation(true, Some("gpt-6-luna"), false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_and_failed_login_return_to_provider_before_exit() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let auth_directory = root.join("state/auth");
        std::fs::create_dir_all(&auth_directory).unwrap();
        for path in [root.join("state"), auth_directory.clone()] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let credentials = auth_directory.join("openai-codex-credentials.json");
        let original = serde_json::json!({"access_token":"old-fixture-access", "refresh_token":"old-fixture-refresh", "account_id":"old-fixture-account", "expires_at_ms":u64::MAX}).to_string();
        std::fs::write(&credentials, &original).unwrap();
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
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
        command.env("TICKLOOP_DAEMON_RUNTIME_DIR", root.join("runtime"));
        let rejected_login = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        command.env(
            "TICKLOOP_TEST_CODEX_OAUTH",
            format!("http://{}", rejected_login.local_addr().unwrap()),
        );
        let rejected_login = std::thread::spawn(move || {
            use std::io::Read;
            let (mut stream, _) = rejected_login.accept().unwrap();
            stream.set_read_timeout(Some(PTY_TIMEOUT)).unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap();
            assert!(headers.starts_with("POST "));
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|length| length.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            let response = "{\"error\":\"controlled login refusal\"}";
            stream.write_all(format!("HTTP/1.1 400 Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).unwrap();
        });
        command.env("TICKLOOP_AGENT_TUI", "inline");
        let home = root.join("home");
        let temporary = root.join("tmp");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&temporary).unwrap();
        command.env("HOME", home);
        command.env("TICKLOOP_TMPDIR", temporary);
        // The Console runs on a libtest thread instead of the CLI main thread.
        command.env("RUST_MIN_STACK", TERMINAL_CHILD_STACK_BYTES.to_string());
        for name in ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH"] {
            command.env_remove(name);
        }
        for model in tickloop_agent_model::cards::built_in_model_profiles() {
            if !model.api_key_env.is_empty() {
                command.env_remove(model.api_key_env);
            }
        }
        command.env("NO_PROXY", "127.0.0.1,localhost");
        command.env("no_proxy", "127.0.0.1,localhost");
        let reader = pty.master.try_clone_reader().unwrap();
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
            screen::capture_output(reader, captured);
        });
        let wait = |needle: &str| {
            let deadline = Instant::now() + PTY_TIMEOUT;
            loop {
                let frame = screen::visible_screen(&output.lock().unwrap(), 150, 40);
                if frame.contains(needle) {
                    break frame;
                }
                assert!(
                    Instant::now() < deadline,
                    "PTY missing {needle}; latest frame:\n{frame}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait("Ask");
        writer.write_all(b"/provider\r").unwrap();
        writer.flush().unwrap();
        wait("Choose Provider");
        writer.write_all(b"jjp").unwrap();
        writer.flush().unwrap();
        wait("Connection · OpenAI");
        writer.write_all(b"j\r").unwrap();
        writer.flush().unwrap();
        wait("Sign in · OpenAI");
        writer.write_all(b"j\r").unwrap();
        writer.flush().unwrap();
        let frame = wait("Codex browser login");
        input_dialog::assert_frame(&frame, 40, "Codex browser login");
        input_dialog::record_frame("browser-login", &frame);
        writer.write_all(b"\x1b").unwrap();
        writer.flush().unwrap();
        wait("Login cancelled");
        let deadline = Instant::now() + PTY_TIMEOUT;
        loop {
            let text = output.lock().unwrap();
            if let Some(cancelled) = text.rfind("Login cancelled")
                && text[cancelled..].contains("Sign in · OpenAI")
                && text[cancelled..].contains("\x1b[?2026l")
                && screen::visible_screen(&text, 150, 40).contains("Sign in · OpenAI")
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
        writer.write_all(b"k\r").unwrap();
        writer.flush().unwrap();
        let failed = wait("Cannot log in:");
        rejected_login.join().unwrap();
        assert!(failed.contains("Back to Provider"));
        writer.write_all(b"\x1b").unwrap();
        writer.flush().unwrap();
        let deadline = Instant::now() + PTY_TIMEOUT;
        loop {
            let text = output.lock().unwrap();
            if let Some(failed) = text.rfind("Cannot log in:")
                && text[failed..].contains("Sign in · OpenAI")
                && text[failed..].contains("\x1b[?2026l")
                && screen::visible_screen(&text, 150, 40).contains("Sign in · OpenAI")
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
        assert_eq!(std::fs::read_to_string(&credentials).unwrap(), original);
        for title in ["Connection · OpenAI", "Choose Provider"] {
            let mark = output.lock().unwrap().len();
            writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
            writer.flush().unwrap();
            let deadline = Instant::now() + PTY_TIMEOUT;
            while !output.lock().unwrap()[mark..].contains(title) {
                assert!(Instant::now() < deadline, "PTY missing fresh {title}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
        writer.flush().unwrap();
        let deadline = Instant::now() + PTY_TIMEOUT;
        loop {
            let frame = screen::visible_screen(&output.lock().unwrap(), 150, 40);
            if !frame.contains("Choose Provider")
                && !frame.contains("Connection · OpenAI")
                && frame.lines().any(|line| line.trim() == "❯")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Provider root did not close: {frame}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        writer.write_all(b"/exit\r").unwrap();
        writer.flush().unwrap();
        let deadline = Instant::now() + PTY_TIMEOUT;
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pty_browser_callback_submission_keeps_login_and_editor_through_completion() {
        pty_login_conversation(true, Some("gpt-6.1-sol"), true).await;
    }

    async fn pty_login_conversation(
        browser: bool,
        selected_model: Option<&'static str>,
        manual_callback: bool,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        if let Some(model) = selected_model {
            let config = tickloop_agent_model::cards::ModelCatalog::defaults()
                .get("openai-codex", model)
                .unwrap();
            tickloop_agent_config::UserSettings::in_root(&root.join("state"))
                .save_model(&config, false)
                .unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (callback_sender, callback_receiver) = tokio::sync::oneshot::channel();
        let (token_release, token_pending) = tokio::sync::oneshot::channel();
        let mut token_release = Some(token_release);
        let (token_requested, token_request) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut callback_sender = Some(callback_sender);
            let mut token_pending = Some(token_pending);
            let mut token_requested = Some(token_requested);
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
                    if manual_callback {
                        callback_sender
                            .take()
                            .unwrap()
                            .send(callback.to_string())
                            .unwrap();
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                        continue;
                    }
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
                    token_pending.take().unwrap().await.unwrap();
                    (
                        json!({"authorization_code":"test-code","code_verifier":"test-verifier"})
                            .to_string(),
                        "application/json",
                    )
                } else if headers.starts_with("POST /oauth/token ") {
                    if manual_callback {
                        token_requested.take().unwrap().send(()).unwrap();
                        token_pending.take().unwrap().await.unwrap();
                    }
                    (json!({"access_token":"ey.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC10ZXN0In19.sig","refresh_token":"fake-refresh-never-display","expires_in":3600}).to_string(), "application/json")
                } else if headers.starts_with("GET /codex/models?") {
                    assert!(headers.contains("chatgpt-account-id: acct-test"));
                    assert!(headers.contains("originator: tickloop"));
                    (json!({"models":[{"slug":"gpt-6-astra"},{"slug":"gpt-6.1-sol"},{"slug":"gpt-5.6-terra"},{"slug":"gpt-6-luna"}]}).to_string(), "application/json")
                } else {
                    assert!(headers.starts_with("POST /codex/responses "), "{headers}");
                    assert!(headers.contains("chatgpt-account-id: acct-test"));
                    let body: serde_json::Value =
                        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                    assert!(body["input"].to_string().contains("hello from terminal"));
                    assert_eq!(body["store"], false);
                    assert_eq!(body["model"], selected_model.unwrap_or("gpt-6-astra"));
                    let is_title = body["tools"].as_array().is_none_or(Vec::is_empty)
                        && body
                            .to_string()
                            .contains("Generate a short sentence-case Chat title");
                    model_seen = !is_title;
                    let reply = if is_title {
                        "Terminal greeting"
                    } else {
                        "Subscription terminal connected"
                    };
                    let text = json!({"type":"message","id":"msg_pty","role":"assistant","status":"completed","content":[{"type":"output_text","text":reply,"annotations":[]}]});
                    let events = [
                        json!({"type":"response.created","response":{"id":"resp_pty","status":"in_progress"}}),
                        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_pty","type":"message","role":"assistant","content":[]}}),
                        json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_pty","content_index":0,"delta":reply}),
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
        command.env("TICKLOOP_DAEMON_RUNTIME_DIR", root.join("runtime"));
        command.env("TICKLOOP_TEST_CODEX_OAUTH", &origin);
        command.env(
            "TICKLOOP_TEST_CODEX_RESPONSES",
            format!("{origin}/codex/responses"),
        );
        command.env("TICKLOOP_AGENT_TUI", "inline");
        let home = root.join("home");
        let temporary = root.join("tmp");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&temporary).unwrap();
        command.env("HOME", home);
        command.env("TICKLOOP_TMPDIR", temporary);
        command.env("RUST_MIN_STACK", TERMINAL_CHILD_STACK_BYTES.to_string());
        for name in ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH"] {
            command.env_remove(name);
        }
        for model in tickloop_agent_model::cards::built_in_model_profiles() {
            if !model.api_key_env.is_empty() {
                command.env_remove(model.api_key_env);
            }
        }
        command.env("NO_PROXY", "127.0.0.1,localhost");
        command.env("no_proxy", "127.0.0.1,localhost");
        command.env("TICKLOOP_TEST_BROWSER_AUTH", "1");
        let reader = pty.master.try_clone_reader().unwrap();
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
            screen::capture_output(reader, captured);
        });
        let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
        let wait = |needle: &str| {
            let deadline = Instant::now() + PTY_TIMEOUT;
            loop {
                let text = output.lock().unwrap().clone();
                let visible = screen::visible_screen(&text, 150, 40);
                if visible.contains(needle) {
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
            let deadline = Instant::now() + PTY_TIMEOUT;
            loop {
                let text = output.lock().unwrap().clone();
                let updates = text.get(offset..).unwrap_or("");
                if let Some(end) = updates.rfind(SYNCHRONIZED_FRAME_END) {
                    let completed = &text[..offset + end + SYNCHRONIZED_FRAME_END.len()];
                    let visible = screen::visible_screen(completed, 150, 40);
                    if visible.contains(needle) {
                        std::thread::sleep(TERMINAL_FRAME_SETTLE);
                        break;
                    }
                }
                let visible = screen::visible_screen(&text, 150, 40);
                assert!(
                    Instant::now() < deadline,
                    "PTY missing {needle} after action:\n{visible}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let wait_for_input_after = |offset: usize| {
            let deadline = Instant::now() + PTY_TIMEOUT;
            let frame_end = SYNCHRONIZED_FRAME_END;
            loop {
                let text = output.lock().unwrap().clone();
                let completed = text
                    .get(offset..)
                    .and_then(|updates| updates.rfind(frame_end))
                    .map(|end| &text[..offset + end + frame_end.len()]);
                let visible = screen::visible_screen(completed.unwrap_or(&text), 150, 40);
                let input_restored = completed.is_some()
                    && visible.contains("Tick Loop Agent v")
                    && visible.contains("Context 0% used")
                    && visible.lines().any(|line| line.trim() == "❯")
                    && [
                        "Choose model",
                        "Choose Provider",
                        "Choose connection",
                        "Codex browser login",
                        "Log in to Codex",
                    ]
                    .iter()
                    .all(|title| !visible.contains(title));
                if input_restored {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "PTY did not restore the input after action:\n{visible}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait("Ask");
        writer.write_all(b"/provider\r").unwrap();
        writer.flush().unwrap();
        wait("Choose Provider");
        writer
            .write_all(if selected_model.is_some() {
                b"\r"
            } else {
                b"jj\r"
            })
            .unwrap();
        writer.flush().unwrap();
        wait("Sign in · OpenAI");
        let before_login = output.lock().unwrap().len();
        writer
            .write_all(if browser { b"j\r" } else { b"\r" })
            .unwrap();
        writer.flush().unwrap();
        if !browser {
            wait_after("Enter code: ABCD-EFGH", before_login);
            let captured = output.lock().unwrap().clone();
            let code_offset = captured[before_login..]
                .find("Enter code: ABCD-EFGH")
                .unwrap()
                + before_login;
            let frame_end = captured[code_offset..]
                .find(SYNCHRONIZED_FRAME_END)
                .unwrap()
                + code_offset
                + SYNCHRONIZED_FRAME_END.len();
            let frame = screen::visible_screen(&captured[..frame_end], 150, 40);
            input_dialog::assert_frame(&frame, 40, "Log in to Codex");
            input_dialog::record_frame("device-login", &frame);
            token_release.take().unwrap().send(()).unwrap();
        }
        if manual_callback {
            let callback = callback_receiver.await.unwrap();
            wait_after("Submit callback", before_login);
            let before_empty = output.lock().unwrap().len();
            writer.write_all(b"\r").unwrap();
            writer.flush().unwrap();
            wait_after("Codex browser login", before_empty);
            std::thread::sleep(Duration::from_millis(100));
            let frame = screen::visible_screen(&output.lock().unwrap(), 150, 40);
            assert!(
                frame.contains("Codex browser login") && frame.contains("Submit callback"),
                "Empty submission dismissed login: {frame}"
            );
            input_dialog::record_frame("callback-empty", &frame);

            let before_invalid = output.lock().unwrap().len();
            writer.write_all(b"\tinvalid-callback\r").unwrap();
            writer.flush().unwrap();
            wait_after("invalid-callback", before_invalid);
            std::thread::sleep(Duration::from_millis(100));
            let frame = screen::visible_screen(&output.lock().unwrap(), 150, 40);
            assert!(
                frame.contains("Codex browser login") && frame.contains("invalid-callback"),
                "Invalid submission dismissed editor: {frame}"
            );
            input_dialog::record_frame("callback-invalid", &frame);

            let before_accepted = output.lock().unwrap().len();
            writer
                .write_all(&vec![0x7f; "invalid-callback".len()])
                .unwrap();
            writer.write_all(callback.as_bytes()).unwrap();
            writer.write_all(b"\r").unwrap();
            writer.flush().unwrap();
            tokio::time::timeout(PTY_TIMEOUT, token_request)
                .await
                .unwrap()
                .unwrap();
            wait_after("Codex browser login", before_accepted);
            std::thread::sleep(Duration::from_millis(100));
            let frame = screen::visible_screen(&output.lock().unwrap(), 150, 40);
            assert!(
                frame.contains("Codex browser login") && frame.contains(&callback),
                "Accepted submission dismissed editor: {frame}"
            );
            input_dialog::record_frame("callback-accepted", &frame);
            token_release.take().unwrap().send(()).unwrap();
        }
        wait_after("Back to Provider", before_login);
        wait("Login saved");
        let completed_frame = screen::visible_screen(&output.lock().unwrap(), 150, 40);
        assert!(
            completed_frame.contains("Back to Provider")
                && !completed_frame.contains("Choose model"),
            "Completed login frame: {completed_frame}"
        );
        if manual_callback {
            assert!(
                completed_frame.contains("enter return"),
                "Completed callback footer: {completed_frame}"
            );
            input_dialog::record_frame("callback-completed", &completed_frame);
            let before_field_escape = output.lock().unwrap().len();
            writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
            writer.flush().unwrap();
            wait_after("Back to Provider", before_field_escape);
            let frame = screen::visible_screen(&output.lock().unwrap(), 150, 40);
            assert!(
                frame.contains("Back to Provider") && !frame.contains("Sign in · OpenAI"),
                "First Esc must only close callback editing: {frame}"
            );
            input_dialog::record_frame("callback-field-escape", &frame);
        }
        let before_login_return = output.lock().unwrap().len();
        writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
        writer.flush().unwrap();
        wait_after("Sign in · OpenAI", before_login_return);
        let before_directory_return = output.lock().unwrap().len();
        writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
        writer.flush().unwrap();
        wait_after("Choose Provider", before_directory_return);
        writer.write_all(b"\r").unwrap();
        writer.flush().unwrap();
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let text = output.lock().unwrap().clone();
                let updates = text.get(before_login..).unwrap_or("");
                let visible = screen::visible_screen(&text, 150, 40);
                let picker_returned = updates.contains(SYNCHRONIZED_FRAME_END)
                    && visible.contains("Choose model · OpenAI")
                    && visible.contains("Codex subscription")
                    && !visible.contains("Codex browser login");
                if picker_returned {
                    input_dialog::assert_frame(&visible, 40, "Choose model · OpenAI");
                    input_dialog::record_frame(
                        if browser {
                            "browser-login-return"
                        } else {
                            "device-login-return"
                        },
                        &visible,
                    );
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
                let before_configuration = output.lock().unwrap().len();
                writer.write_all(b"p").unwrap();
                writer.flush().unwrap();
                wait_after("Connection · OpenAI", before_configuration);
                let before_reauthentication = output.lock().unwrap().len();
                writer.write_all(b"j\r").unwrap();
                writer.flush().unwrap();
                wait_after("Sign in · OpenAI", before_reauthentication);
                writer.write_all(b"j\r").unwrap();
                writer.flush().unwrap();
                wait_after("Codex browser login", before_reauthentication);
                wait_after("Back to Provider", before_reauthentication);
                let before_method_return = output.lock().unwrap().len();
                writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
                writer.flush().unwrap();
                wait_after("Sign in · OpenAI", before_method_return);
                let before_connection_return = output.lock().unwrap().len();
                writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
                writer.flush().unwrap();
                wait_after("Connection · OpenAI", before_connection_return);
                let before_model_return = output.lock().unwrap().len();
                writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
                writer.flush().unwrap();
                wait_after("Choose model · OpenAI", before_model_return);
            }
            if selected_model.is_none() {
                assert!(!root.join("state/settings.json").exists());
            }
            let before_back = output.lock().unwrap().len();
            writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
            writer.flush().unwrap();
            wait_after("Choose Provider", before_back);
            let before_cancel = output.lock().unwrap().len();
            writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
            writer.flush().unwrap();
            wait_for_input_after(before_cancel);
            let before_saved_login = output.lock().unwrap().len();
            writer.write_all(b"/provider\r").unwrap();
            writer.flush().unwrap();
            wait_after("Choose Provider", before_saved_login);
            let before_saved_connection = output.lock().unwrap().len();
            writer
                .write_all(if selected_model.is_some() {
                    b"\r"
                } else {
                    b"jj\r"
                })
                .unwrap();
            writer.flush().unwrap();
            wait_after("Choose model · OpenAI", before_saved_connection);
            assert!(
                !output.lock().unwrap()[before_saved_connection..].contains("Codex browser login"),
                "saved login must open models without authenticating again"
            );
            if selected_model == Some("gpt-5.6-sol") {
                let saved = tickloop_agent_config::UserSettings::in_root(&root.join("state"))
                    .model()
                    .unwrap();
                assert_eq!(saved.model, "gpt-5.6-sol");
            }
            if selected_model.is_none() {
                // Choosing a model closes the picker.
                let before_model_choice = output.lock().unwrap().len();
                writer.write_all(b"\r").unwrap();
                writer.flush().unwrap();
                wait("Model selection:");
                wait_for_input_after(before_model_choice);
            } else {
                let before_model_back = output.lock().unwrap().len();
                writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
                writer.flush().unwrap();
                wait_after("Choose Provider", before_model_back);
                let before_model_cancel = output.lock().unwrap().len();
                writer.write_all(TERMINAL_ESCAPE_KEY).unwrap();
                writer.flush().unwrap();
                wait_for_input_after(before_model_cancel);
            }
        }
        writer.write_all(b"hello from terminal\r").unwrap();
        writer.flush().unwrap();
        wait("Subscription terminal connected");
        writer.write_all(b"/exit\r").unwrap();
        writer.flush().unwrap();
        let deadline = Instant::now() + PTY_TIMEOUT;
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
