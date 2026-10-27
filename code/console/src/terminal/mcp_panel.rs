//! `/mcp`: the Agent's MCP servers grouped by where they are configured, each with its
//! transport, state, tools and exposure. `e` moves the selected server to its next exposure.
//! Enter opens a server's actions: view its tools, log in or out, reconnect, turn it off or on in
//! the Agent's directory, and choose its exposure. A login happens in the panel:
//! the authorization link, and a field for the address the browser ends on when it runs on
//! another machine. After a login or a reconnect the panel follows the server until it has
//! connected and listed its tools, or failed. The panel keeps what it shows and what the user
//! chose; the Console runs the work and hands each result back through [`McpPanel::apply`].

use std::{collections::BTreeSet, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickloop_code_settings::mcp::{DIRECTORY_SOURCE, GLOBAL_SOURCE, PLUGIN_SOURCE_PREFIX};
use tickloop_harness::protocol::mcp::{McpServerState, McpServerStatus, ToolExposure};
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    console::wrap_text,
    conversation_picker::{ConversationPickerFrame, compact_line, fit_hint},
    display::{DisplayProfile, Glyph},
    editor::{Editor, EditorCommand},
    list_row::ListRow,
    permissions_surface::{edit_text, editor_window},
    safe_line,
    style::{Role, RoleLine},
    ticks_panel::{Notice, step},
};

const TITLE: &str = "MCP";
/// The title and the key hints.
const CHROME_ROWS: usize = 2;
const INDENT: &str = "  ";

/// Work the panel asks the Console to do; its result comes back as an [`McpResult`].
#[derive(Clone, PartialEq, Eq)]
pub(super) enum McpRequest {
    /// Reads the servers: from the Agent's Worker, or from the configuration before the Agent
    /// starts.
    Load,
    Reconnect(String),
    SetDisabled {
        name: String,
        disabled: bool,
    },
    /// Saves how the tools of the server `name`, configured in `source`, reach the model.
    SetExposure {
        name: String,
        source: String,
        exposure: ToolExposure,
    },
    /// Starts the login `attempt` to the server `name`; the panel numbers each attempt.
    LogIn {
        name: String,
        attempt: u64,
    },
    /// The address the browser ended on, for the login `attempt`.
    SubmitCallback {
        attempt: u64,
        address: String,
    },
    CancelLogIn(u64),
    LogOut(String),
}

/// A callback address carries an authorization code, which debug output leaves out.
impl std::fmt::Debug for McpRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Load => formatter.write_str("Load"),
            Self::Reconnect(name) => formatter.debug_tuple("Reconnect").field(name).finish(),
            Self::SetDisabled { name, disabled } => formatter
                .debug_struct("SetDisabled")
                .field("name", name)
                .field("disabled", disabled)
                .finish(),
            Self::SetExposure {
                name,
                source,
                exposure,
            } => formatter
                .debug_struct("SetExposure")
                .field("name", name)
                .field("source", source)
                .field("exposure", exposure)
                .finish(),
            Self::LogIn { name, attempt } => formatter
                .debug_struct("LogIn")
                .field("name", name)
                .field("attempt", attempt)
                .finish(),
            Self::SubmitCallback { attempt, .. } => formatter
                .debug_struct("SubmitCallback")
                .field("attempt", attempt)
                .finish_non_exhaustive(),
            Self::CancelLogIn(attempt) => {
                formatter.debug_tuple("CancelLogIn").field(attempt).finish()
            }
            Self::LogOut(name) => formatter.debug_tuple("LogOut").field(name).finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct McpWork {
    pub(super) panel: Uuid,
    pub(super) request: McpRequest,
}

/// The servers as one read found them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct McpListing {
    pub(super) servers: Vec<McpServerStatus>,
    /// Whether the states come from the Agent's Worker; before it starts they come from the
    /// configuration, and a server that can run has not connected yet.
    pub(super) live: bool,
    /// Before the Agent starts: the servers whose state comes from a connection made for the
    /// panel rather than from the configuration.
    pub(super) probed: BTreeSet<String>,
    /// The directory the Agent works in, where a server is turned off or on.
    pub(super) directory: PathBuf,
}

#[derive(Debug)]
pub(super) enum McpResult {
    /// The servers as the read numbered `read` found them; reads are numbered in the order they
    /// start, so one that started later is the more recent.
    Servers {
        read: u64,
        listing: Result<McpListing, String>,
    },
    Reconnected {
        name: String,
        result: Result<(), String>,
    },
    DisabledSet {
        name: String,
        disabled: bool,
        result: Result<(), String>,
    },
    ExposureSet {
        name: String,
        exposure: ToolExposure,
        result: Result<(), String>,
    },
    /// The login `attempt` waits for the user to approve it at `link`; `opened` says whether the
    /// browser was asked to open it.
    LoginWaiting {
        attempt: u64,
        link: String,
        opened: bool,
    },
    /// The address the user entered for the login `attempt` was not its callback.
    CallbackRefused { attempt: u64, error: String },
    LoginEnded {
        attempt: u64,
        name: String,
        result: Result<(), String>,
    },
    LoggedOut {
        name: String,
        /// Whether the server could not revoke the token, which was deleted anyway.
        result: Result<bool, String>,
    },
}

impl McpResult {
    /// What an operation's result tells the user; `None` for a result that only refreshes.
    pub(super) fn notice(&self) -> Option<Notice> {
        Some(match self {
            Self::Servers { .. } | Self::LoginWaiting { .. } | Self::CallbackRefused { .. } => {
                return None;
            }
            Self::Reconnected { result: Ok(()), .. } => return None,
            Self::Reconnected {
                name,
                result: Err(error),
            } => Notice::failure(format!("Cannot reconnect {name}: {error}")),
            Self::DisabledSet {
                name,
                disabled,
                result,
            } => match (result, disabled) {
                (Ok(()), true) => Notice::success(format!("Disabled {name} in this directory")),
                (Ok(()), false) => Notice::success(format!("Enabled {name} in this directory")),
                (Err(error), true) => Notice::failure(format!("Cannot disable {name}: {error}")),
                (Err(error), false) => Notice::failure(format!("Cannot enable {name}: {error}")),
            },
            Self::ExposureSet {
                name,
                exposure,
                result,
            } => match result {
                Ok(()) => {
                    Notice::success(format!("Exposure of {name} is now {}", exposure.as_str()))
                }
                Err(error) => {
                    Notice::failure(format!("Cannot change the exposure of {name}: {error}"))
                }
            },
            Self::LoginEnded { name, result, .. } => match result {
                Ok(()) => Notice::success(format!("Logged in to {name}")),
                Err(error) => Notice::failure(format!("Cannot log in to {name}: {error}")),
            },
            Self::LoggedOut { name, result } => match result {
                Ok(false) => Notice::success(format!("Logged out of {name}")),
                Ok(true) => Notice::success(format!(
                    "Logged out of {name}; the server could not revoke the token, which was deleted"
                )),
                Err(error) => Notice::failure(format!("Cannot log out of {name}: {error}")),
            },
        })
    }
}

pub(super) enum McpPanelEffect {
    None,
    Close,
    /// Edits the callback address in the external editor.
    EditCallback,
    Work(McpRequest),
}

enum Listing {
    Loading,
    Loaded(McpListing),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Choice {
    Tools(String),
    Run(McpRequest),
    /// Opens the choice of the server's exposure.
    Exposure,
    Back,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuKind {
    Actions,
    Exposure,
}

/// The exposures in the order `e` moves through them, starting from the default.
const EXPOSURE_CYCLE: [ToolExposure; 3] = [
    ToolExposure::Search,
    ToolExposure::Direct,
    ToolExposure::Hidden,
];
/// The exposures in the order their menu lists them.
const EXPOSURE_OPTIONS: [ToolExposure; 3] = [
    ToolExposure::Direct,
    ToolExposure::Search,
    ToolExposure::Hidden,
];
const CHANGE_EXPOSURE: &str = "Change exposure";

/// A server's actions, or the choice of its exposure, with the `❯` cursor.
struct ActionsMenu {
    kind: MenuKind,
    title: String,
    context: Vec<String>,
    options: Vec<(String, Choice)>,
    selected: usize,
}

struct ToolsView {
    server: String,
    selected: usize,
    top: usize,
}

struct LoginView {
    attempt: u64,
    name: String,
    stage: LoginStage,
}

/// A server the panel follows after a login or a reconnect, to tell how its connection ends.
struct Following {
    name: String,
    after_login: bool,
    /// Whether the connection was asked for; a read before that still shows the old state.
    requested: bool,
}

impl Following {
    fn status_text(&self) -> String {
        if self.after_login {
            format!("Logged in to {} · connecting…", self.name)
        } else {
            format!("Connecting to {}…", self.name)
        }
    }

    fn connected(&self, tools: usize) -> Notice {
        let tools = tool_count(tools);
        Notice::success(if self.after_login {
            format!("Logged in to {} · connected · {tools}", self.name)
        } else {
            format!("Connected to {} · {tools}", self.name)
        })
    }

    fn failed(&self, cause: &str) -> Notice {
        Notice::failure(if self.after_login {
            format!("Logged in to {}, but cannot connect: {cause}", self.name)
        } else {
            format!("Cannot connect to {}: {cause}", self.name)
        })
    }
}

enum LoginStage {
    /// Finding the authorization server and registering.
    Starting,
    Waiting {
        link: String,
        opened: bool,
        callback: Box<Editor>,
        error: Option<String>,
    },
}

pub(super) struct McpPanel {
    id: Uuid,
    pub(super) chat: Option<Uuid>,
    listing: Listing,
    /// The number of the read `listing` holds; a read that started before it is stale.
    read: u64,
    selected: usize,
    top: usize,
    menu: Option<ActionsMenu>,
    tools: Option<ToolsView>,
    login: Option<LoginView>,
    login_attempts: u64,
    following: Option<Following>,
    /// The operation under way, as the panel shows it.
    busy: Option<String>,
    notice: Option<Notice>,
    /// The body rows each list row or option was drawn on, for pointer selection.
    row_targets: Vec<(std::ops::Range<usize>, usize)>,
}

impl McpPanel {
    pub(super) fn new(chat: Option<Uuid>) -> Self {
        Self {
            id: tickloop_ids::generate(),
            chat,
            listing: Listing::Loading,
            read: 0,
            selected: 0,
            top: 0,
            menu: None,
            tools: None,
            login: None,
            login_attempts: 0,
            following: None,
            busy: None,
            notice: None,
            row_targets: Vec::new(),
        }
    }

    pub(super) fn id(&self) -> Uuid {
        self.id
    }

    pub(super) fn title(&self) -> &'static str {
        TITLE
    }

    /// Whether the shown servers are still connecting, so a later read would show more.
    pub(super) fn awaits_connections(&self) -> bool {
        matches!(&self.listing, Listing::Loaded(listing) if listing.live
            && listing.servers.iter().any(|server| server.state == McpServerState::Connecting))
    }

    /// Takes `result`, unless it belongs to a login that is no longer shown.
    pub(super) fn apply(&mut self, result: McpResult) -> bool {
        let current_login = |attempt: &u64| {
            self.login
                .as_ref()
                .is_some_and(|login| login.attempt == *attempt)
        };
        match &result {
            McpResult::LoginWaiting { attempt, .. }
            | McpResult::CallbackRefused { attempt, .. }
            | McpResult::LoginEnded { attempt, .. }
                if !current_login(attempt) =>
            {
                return false;
            }
            _ => {}
        }
        if let Some(notice) = result.notice() {
            self.notice = Some(notice);
        }
        match result {
            McpResult::Servers { read, .. } if read < self.read => {}
            McpResult::Servers { read, mut listing } => {
                if let Ok(listing) = &mut listing {
                    listing.servers.sort_by(|left, right| {
                        (source_rank(&left.source), &left.source, &left.name).cmp(&(
                            source_rank(&right.source),
                            &right.source,
                            &right.name,
                        ))
                    });
                }
                let shown = self
                    .servers()
                    .get(self.selected)
                    .map(|server| server.name.clone());
                self.read = read;
                self.listing = listing.map_or_else(Listing::Failed, Listing::Loaded);
                self.follow_connection();
                self.selected = shown
                    .and_then(|name| self.servers().iter().position(|server| server.name == name))
                    .unwrap_or(self.selected)
                    .min(self.servers().len().saturating_sub(1));
                // An open menu follows the new states of its server, on the option it was on.
                if let Some(menu) = self.menu.take() {
                    let option = menu.options[menu.selected].0.clone();
                    self.menu = self
                        .menu_of(menu.kind)
                        .filter(|next| next.title == menu.title)
                        .map(|mut next| {
                            next.selected = next
                                .options
                                .iter()
                                .position(|(label, _)| *label == option)
                                .unwrap_or(0);
                            next
                        });
                }
            }
            McpResult::LoginWaiting { link, opened, .. } => {
                if let Some(login) = &mut self.login {
                    login.stage = LoginStage::Waiting {
                        link,
                        opened,
                        callback: Box::new(Editor::new()),
                        error: None,
                    };
                }
            }
            McpResult::CallbackRefused { error, .. } => {
                if let Some(LoginView {
                    stage: LoginStage::Waiting { error: shown, .. },
                    ..
                }) = &mut self.login
                {
                    *shown = Some(error);
                }
            }
            McpResult::LoginEnded { name, result, .. } => {
                self.login = None;
                self.busy = None;
                if result.is_ok() {
                    self.notice = None;
                    self.following = Some(Following {
                        name,
                        after_login: true,
                        requested: false,
                    });
                }
            }
            McpResult::Reconnected { name, result } => {
                self.busy = None;
                let following = self.following.take_if(|following| following.name == name);
                match (following, result) {
                    (Some(mut following), Ok(())) => {
                        following.requested = true;
                        self.following = Some(following);
                    }
                    (Some(following), Err(error)) if following.after_login => {
                        self.notice = Some(following.failed(&error));
                    }
                    _ => {}
                }
            }
            McpResult::ExposureSet {
                name,
                exposure,
                result,
            } => {
                self.busy = None;
                if result.is_ok()
                    && let Listing::Loaded(listing) = &mut self.listing
                    && let Some(server) = listing
                        .servers
                        .iter_mut()
                        .find(|server| server.name == name)
                {
                    server.exposure = exposure.as_str().into();
                }
            }
            McpResult::DisabledSet { .. } | McpResult::LoggedOut { .. } => self.busy = None,
        }
        true
    }

    /// Tells how the followed server's connection ended once a read shows it connected or
    /// failed; a read that still shows it connecting, or shows its state from the configuration,
    /// keeps following it.
    fn follow_connection(&mut self) {
        let Some(following) = self
            .following
            .as_ref()
            .filter(|following| following.requested)
        else {
            return;
        };
        let Listing::Loaded(listing) = &self.listing else {
            return;
        };
        let Some(server) = listing
            .servers
            .iter()
            .find(|server| server.name == following.name)
        else {
            self.following = None;
            return;
        };
        if !connection_backed(listing, server) {
            return;
        }
        let notice = match &server.state {
            McpServerState::Connecting => return,
            McpServerState::Connected => following.connected(server.tools.len()),
            McpServerState::Failed | McpServerState::NeedsLogin => following.failed(
                server
                    .failure
                    .as_deref()
                    .unwrap_or(state_label(&server.state, true)),
            ),
            _ => {
                self.following = None;
                return;
            }
        };
        self.notice = Some(notice);
        self.following = None;
    }

    pub(super) fn report(&mut self, error: String) {
        self.notice = Some(Notice::failure(error));
    }

    /// The callback address being entered, for the external editor.
    pub(super) fn callback_text(&self) -> &str {
        match &self.login {
            Some(LoginView {
                stage: LoginStage::Waiting { callback, .. },
                ..
            }) => callback.draft(),
            _ => "",
        }
    }

    pub(super) fn replace_callback(&mut self, text: &str) {
        if let Some(callback) = self.callback_editor() {
            let text = compact_line(text);
            let cursor = text.len();
            callback.replace_draft(text, cursor);
        }
    }

    /// Takes pasted text into the callback field; elsewhere the panel takes no text.
    pub(super) fn paste(&mut self, text: &str) {
        if let Some(callback) = self.callback_editor() {
            callback.apply(EditorCommand::InsertPasted(compact_line(text)));
        }
    }

    fn callback_editor(&mut self) -> Option<&mut Editor> {
        match &mut self.login {
            Some(LoginView {
                stage: LoginStage::Waiting { callback, .. },
                ..
            }) => Some(callback),
            _ => None,
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> McpPanelEffect {
        if self.login.is_some() {
            return self.login_key(key);
        }
        let movement = match (key.code, key.modifiers) {
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(false),
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(true),
            _ => None,
        };
        if let Some(server) = self.tools.as_ref().map(|view| view.server.clone()) {
            let count = self
                .servers()
                .iter()
                .find(|listed| listed.name == server)
                .map_or(0, |listed| listed.tools.len());
            let view = self.tools.as_mut().expect("the tools view is shown");
            if let Some(down) = movement {
                view.selected = step(view.selected, count, down);
            } else if key.code == KeyCode::Esc {
                self.tools = None;
            }
            return McpPanelEffect::None;
        }
        if let Some(menu) = &mut self.menu {
            if let Some(down) = movement {
                menu.selected = step(menu.selected, menu.options.len(), down);
                return McpPanelEffect::None;
            }
            return match key.code {
                KeyCode::Esc => {
                    let kind = menu.kind;
                    self.menu = None;
                    if kind == MenuKind::Exposure {
                        self.open_actions_at(CHANGE_EXPOSURE);
                    }
                    McpPanelEffect::None
                }
                KeyCode::Enter => {
                    let choice = menu.options[menu.selected].1.clone();
                    self.choose(choice)
                }
                _ => McpPanelEffect::None,
            };
        }
        if let Some(down) = movement {
            self.selected = step(self.selected, self.servers().len(), down);
            return McpPanelEffect::None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => McpPanelEffect::Close,
            (KeyCode::Char('r'), KeyModifiers::NONE) => self.start(McpRequest::Load),
            (KeyCode::Char('e'), KeyModifiers::NONE) => match self.servers().get(self.selected) {
                Some(server) => {
                    let current = EXPOSURE_CYCLE
                        .iter()
                        .position(|exposure| exposure.as_str() == server.exposure)
                        .unwrap_or(0);
                    let request = McpRequest::SetExposure {
                        name: server.name.clone(),
                        source: server.source.clone(),
                        exposure: EXPOSURE_CYCLE[(current + 1) % EXPOSURE_CYCLE.len()],
                    };
                    self.start(request)
                }
                None => McpPanelEffect::None,
            },
            (KeyCode::Enter, _) => {
                self.menu = self.actions_menu();
                McpPanelEffect::None
            }
            _ => McpPanelEffect::None,
        }
    }

    /// A key while a login is shown: the callback field takes text, enter submits it, ctrl+g
    /// edits it in the external editor, and esc cancels the login.
    fn login_key(&mut self, key: KeyEvent) -> McpPanelEffect {
        let Some(login) = &mut self.login else {
            return McpPanelEffect::None;
        };
        match (&mut login.stage, key.code, key.modifiers) {
            (_, KeyCode::Esc, _) | (_, KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                let attempt = login.attempt;
                let name = login.name.clone();
                self.login = None;
                self.busy = None;
                self.notice = Some(Notice::success(format!("Cancelled the login to {name}")));
                McpPanelEffect::Work(McpRequest::CancelLogIn(attempt))
            }
            (LoginStage::Waiting { .. }, KeyCode::Char('g'), KeyModifiers::CONTROL) => {
                McpPanelEffect::EditCallback
            }
            (
                LoginStage::Waiting {
                    callback, error, ..
                },
                KeyCode::Enter,
                _,
            ) => {
                let address = callback.draft().trim().to_owned();
                if address.is_empty() {
                    return McpPanelEffect::None;
                }
                *error = None;
                McpPanelEffect::Work(McpRequest::SubmitCallback {
                    attempt: login.attempt,
                    address,
                })
            }
            (LoginStage::Waiting { callback, .. }, _, _) => {
                edit_text(callback, key);
                McpPanelEffect::None
            }
            (LoginStage::Starting, _, _) => McpPanelEffect::None,
        }
    }

    pub(super) fn select_at_row(&mut self, row: usize) -> bool {
        let Some(index) = self
            .row_targets
            .iter()
            .find(|(rows, _)| rows.contains(&row))
            .map(|(_, index)| *index)
        else {
            return false;
        };
        match (&mut self.menu, &mut self.tools) {
            (_, Some(view)) => view.selected = index,
            (Some(menu), None) => menu.selected = index,
            (None, None) => self.selected = index,
        }
        // Only a list row or an option acts on the click that selects it.
        self.tools.is_none() && self.login.is_none()
    }

    fn servers(&self) -> &[McpServerStatus] {
        match &self.listing {
            Listing::Loaded(listing) => &listing.servers,
            _ => &[],
        }
    }

    /// Whether the state of `server` comes from a connection, the Agent's or one made for the
    /// panel, rather than from the configuration.
    fn is_backed(&self, server: &McpServerStatus) -> bool {
        matches!(&self.listing, Listing::Loaded(listing) if connection_backed(listing, server))
    }

    fn choose(&mut self, choice: Choice) -> McpPanelEffect {
        match choice {
            Choice::Back => {
                let kind = self.menu.take().map(|menu| menu.kind);
                if kind == Some(MenuKind::Exposure) {
                    self.open_actions_at(CHANGE_EXPOSURE);
                }
                McpPanelEffect::None
            }
            Choice::Exposure => {
                self.menu = self.exposure_menu();
                McpPanelEffect::None
            }
            Choice::Tools(server) => {
                self.tools = Some(ToolsView {
                    server,
                    selected: 0,
                    top: 0,
                });
                McpPanelEffect::None
            }
            Choice::Run(request) => {
                let effect = self.start(request);
                if matches!(effect, McpPanelEffect::Work(_)) {
                    self.menu = None;
                }
                effect
            }
        }
    }

    /// Starts `request` unless an operation is under way; reading the servers never waits.
    fn start(&mut self, mut request: McpRequest) -> McpPanelEffect {
        let busy = match &request {
            McpRequest::Load => {
                return McpPanelEffect::Work(request);
            }
            McpRequest::SubmitCallback { .. } | McpRequest::CancelLogIn(_) => {
                return McpPanelEffect::Work(request);
            }
            McpRequest::Reconnect(name) => format!("Reconnecting {name}…"),
            McpRequest::SetDisabled { name, disabled } => format!(
                "{} {name}…",
                if *disabled { "Disabling" } else { "Enabling" }
            ),
            McpRequest::SetExposure { name, .. } => {
                format!("Changing the exposure of {name}…")
            }
            McpRequest::LogIn { name, .. } => format!("Logging in to {name}…"),
            McpRequest::LogOut(name) => format!("Logging out of {name}…"),
        };
        if self.busy.is_some() {
            self.notice = Some(Notice::failure(
                "Wait for it to finish before starting another".to_owned(),
            ));
            return McpPanelEffect::None;
        }
        self.busy = Some(busy);
        self.notice = None;
        self.following = match &request {
            McpRequest::Reconnect(name) => Some(Following {
                name: name.clone(),
                after_login: false,
                requested: false,
            }),
            _ => None,
        };
        if let McpRequest::LogIn { name, attempt } = &mut request {
            self.login_attempts += 1;
            *attempt = self.login_attempts;
            self.login = Some(LoginView {
                attempt: *attempt,
                name: name.clone(),
                stage: LoginStage::Starting,
            });
        }
        McpPanelEffect::Work(request)
    }

    fn menu_of(&self, kind: MenuKind) -> Option<ActionsMenu> {
        match kind {
            MenuKind::Actions => self.actions_menu(),
            MenuKind::Exposure => self.exposure_menu(),
        }
    }

    /// Opens the actions of the selected server on the option `label`.
    fn open_actions_at(&mut self, label: &str) {
        self.menu = self.actions_menu().map(|mut menu| {
            menu.selected = menu
                .options
                .iter()
                .position(|(option, _)| option == label)
                .unwrap_or(0);
            menu
        });
    }

    /// The exposures the selected server can take, with the cursor on its current one.
    fn exposure_menu(&self) -> Option<ActionsMenu> {
        let server = self.servers().get(self.selected)?;
        let mut options = EXPOSURE_OPTIONS
            .iter()
            .map(|exposure| {
                (
                    exposure.as_str().to_owned(),
                    Choice::Run(McpRequest::SetExposure {
                        name: server.name.clone(),
                        source: server.source.clone(),
                        exposure: *exposure,
                    }),
                )
            })
            .collect::<Vec<_>>();
        let selected = options
            .iter()
            .position(|(label, _)| *label == server.exposure)
            .unwrap_or(0);
        options.push(("Back".to_owned(), Choice::Back));
        Some(ActionsMenu {
            kind: MenuKind::Exposure,
            title: server.name.clone(),
            context: vec![
                "How its tools reach the model: direct lists them, search finds them when needed, hidden never offers them.".to_owned(),
            ],
            options,
            selected,
        })
    }

    /// The actions of the selected server.
    fn actions_menu(&self) -> Option<ActionsMenu> {
        let server = self.servers().get(self.selected)?;
        let name = server.name.clone();
        let backed = self.is_backed(server);
        let mut options = Vec::new();
        if !server.tools.is_empty() {
            options.push(("View tools".to_owned(), Choice::Tools(name.clone())));
        }
        if server.oauth && server.state != McpServerState::Disabled {
            let login = McpRequest::LogIn {
                name: name.clone(),
                attempt: 0,
            };
            if server.logged_in {
                options.push(("Log in again".to_owned(), Choice::Run(login)));
                options.push((
                    "Log out".to_owned(),
                    Choice::Run(McpRequest::LogOut(name.clone())),
                ));
            } else {
                options.push(("Log in".to_owned(), Choice::Run(login)));
            }
        }
        // Before the Agent starts, a server that can run connects for the panel; one whose
        // configuration failed cannot connect.
        let can_connect = match server.state {
            McpServerState::Connecting => true,
            McpServerState::Connected | McpServerState::Failed | McpServerState::NeedsLogin => {
                backed
            }
            _ => false,
        };
        if can_connect {
            options.push((
                if backed { "Reconnect" } else { "Connect" }.to_owned(),
                Choice::Run(McpRequest::Reconnect(name.clone())),
            ));
        }
        let disabled = server.state == McpServerState::Disabled;
        options.push((
            if disabled {
                "Enable in this directory"
            } else {
                "Disable in this directory"
            }
            .to_owned(),
            Choice::Run(McpRequest::SetDisabled {
                name: name.clone(),
                disabled: !disabled,
            }),
        ));
        options.push((CHANGE_EXPOSURE.to_owned(), Choice::Exposure));
        options.push(("Back".to_owned(), Choice::Back));
        let mut context = vec![row_details(server, backed)];
        if let Some(failure) = &server.failure {
            context.push(safe_line(failure));
        }
        if server.state == McpServerState::NeedsConfiguration
            && let Some(plugin) = server.source.strip_prefix(PLUGIN_SOURCE_PREFIX)
        {
            context.push(format!(
                "Configure the plugin in /plugins or with tickloop plugin configure {plugin}"
            ));
        }
        Some(ActionsMenu {
            kind: MenuKind::Actions,
            title: name,
            context,
            options,
            selected: 0,
        })
    }

    /// The rows of the list: a heading for each source, then its servers, each tagged with its
    /// index among the servers.
    fn list_rows(
        &self,
        width: usize,
        display: DisplayProfile,
    ) -> Vec<(Option<usize>, Vec<RoleLine>)> {
        let glyph = display.glyph(Glyph::AgentViewed);
        let mut rows = Vec::new();
        let mut heading = None;
        for (index, server) in self.servers().iter().enumerate() {
            let group = source_heading(&server.source);
            if heading.as_ref() != Some(&group) {
                rows.push((None, vec![RoleLine::role(safe_line(&group), Role::Accent)]));
                heading = Some(group);
            }
            let backed = self.is_backed(server);
            let details = row_details(server, backed);
            let separate_details =
                glyph.width() + server.name.width() + details.width() + 3 > width;
            let mut lines = ListRow {
                selected: index == self.selected && self.menu.is_none(),
                glyph,
                glyph_role: state_role(&server.state, backed),
                name: &server.name,
                details: if separate_details {
                    String::new()
                } else {
                    details.clone()
                },
                fill: None,
                subscription: None,
                bar_columns: None,
            }
            .lines(width);
            if separate_details {
                lines.extend(
                    wrap_text(&details, width.saturating_sub(2).max(1))
                        .into_iter()
                        .map(|line| RoleLine::role(format!("{INDENT}{line}"), Role::MutedText)),
                );
            }
            rows.push((Some(index), lines));
        }
        rows
    }

    fn status_lines(&self, width: usize) -> Vec<RoleLine> {
        let mut lines = Vec::new();
        let mut add = |text: &str, role: Role| {
            lines.extend(
                wrap_text(&safe_line(text), width)
                    .into_iter()
                    .map(|line| RoleLine::role(line, role)),
            );
        };
        if let Some(busy) = self.busy.as_ref().filter(|_| self.login.is_none()) {
            add(busy, Role::Accent);
        } else if let Some(following) = &self.following {
            add(&following.status_text(), Role::Accent);
        }
        if let Some(notice) = &self.notice {
            add(
                &notice.text,
                if notice.is_failure {
                    Role::Error
                } else {
                    Role::Success
                },
            );
        }
        lines
    }

    /// The body, the rows of its focus, the clickable rows, the text cursor and the key hint.
    fn body(&self, width: usize, display: DisplayProfile) -> Body {
        let mut body = Body::default();
        if let Some(login) = &self.login {
            login_lines(login, width, display, &mut body);
            return body;
        }
        if let Some(view) = &self.tools {
            let tools = self
                .servers()
                .iter()
                .find(|server| server.name == view.server)
                .map(|server| server.tools.as_slice())
                .unwrap_or_default();
            body.lines.push(RoleLine::role(
                format!("Tools of {}", safe_line(&view.server)),
                Role::Text,
            ));
            for (index, tool) in tools.iter().enumerate() {
                let start = body.lines.len();
                let mut line = RoleLine::role(
                    format!("{INDENT}{}", safe_line(&tool.name)),
                    if index == view.selected {
                        Role::Text
                    } else {
                        Role::MutedText
                    },
                );
                if !tool.description.is_empty() {
                    line.push(
                        format!(" · {}", safe_line(&compact_line(&tool.description))),
                        Some(Role::MutedText),
                    );
                }
                if index == view.selected {
                    line = line.filled(Role::Selection);
                    body.focus = start..start + 1;
                }
                body.lines.push(line);
                body.targets.push((start..start + 1, index));
            }
            body.hint = fit_hint(&[("esc back".into(), 9)], width, display);
            return body;
        }
        if let Some(menu) = &self.menu {
            body.lines
                .push(RoleLine::role(safe_line(&menu.title), Role::Text));
            for line in &menu.context {
                body.lines.extend(
                    wrap_text(line, width.saturating_sub(INDENT.len()).max(1))
                        .into_iter()
                        .map(|line| RoleLine::role(format!("{INDENT}{line}"), Role::MutedText)),
                );
            }
            for (index, (label, _)) in menu.options.iter().enumerate() {
                let row = body.lines.len();
                body.targets.push((row..row + 1, index));
                body.lines.push(if index == menu.selected {
                    body.focus = row..row + 1;
                    RoleLine::role(
                        format!("{} {label}", display.glyph(Glyph::Prompt)),
                        Role::Text,
                    )
                    .filled(Role::Selection)
                } else {
                    RoleLine::role(format!("{INDENT}{label}"), Role::Text)
                });
            }
            body.hint = fit_hint(
                &[("enter confirm".into(), 10), ("esc back".into(), 9)],
                width,
                display,
            );
            return body;
        }
        for (index, rows) in self.list_rows(width, display) {
            let range = body.lines.len()..body.lines.len() + rows.len();
            if let Some(index) = index {
                if index == self.selected {
                    body.focus = range.clone();
                }
                body.targets.push((range, index));
            }
            body.lines.extend(rows);
        }
        if body.lines.is_empty() {
            let (text, role) = match &self.listing {
                Listing::Loading => ("Loading…".to_owned(), Role::MutedText),
                Listing::Failed(error) => (safe_line(error), Role::Error),
                Listing::Loaded(_) => (
                    "No MCP servers are configured; add one with tickloop mcp add.".to_owned(),
                    Role::MutedText,
                ),
            };
            body.lines = wrap_text(&text, width.saturating_sub(INDENT.len()).max(1))
                .into_iter()
                .map(|line| RoleLine::role(format!("{INDENT}{line}"), role))
                .collect();
        }
        let mut hints = vec![("r refresh".to_owned(), 4), ("esc close".to_owned(), 9)];
        if !self.servers().is_empty() {
            hints.insert(0, ("enter actions".to_owned(), 10));
            hints.insert(1, ("e exposure".to_owned(), 5));
        }
        body.hint = fit_hint(&hints, width, display);
        body
    }

    /// The title, one row of the focus, and the key hints.
    pub(super) fn minimum_frame_rows(&self, width: usize) -> usize {
        let body = self.body(width, DisplayProfile::default());
        CHROME_ROWS + body.focus.len().max(1) + self.status_lines(width).len()
    }

    pub(super) fn content_rows(&self, width: usize) -> usize {
        CHROME_ROWS
            + self.body(width, DisplayProfile::default()).lines.len()
            + self.status_lines(width).len()
    }

    pub(super) fn frame(
        &mut self,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> ConversationPickerFrame {
        let mut lines = vec![RoleLine::role(TITLE, Role::DialogTitle)];
        let status = self.status_lines(width);
        let capacity = height.saturating_sub(CHROME_ROWS + status.len());
        let body = self.body(width, display);
        // A menu or login scrolls to its focus alone; lists keep their place.
        let mut focus_top = 0;
        let top = match (&mut self.tools, self.menu.is_some() || self.login.is_some()) {
            (Some(view), _) => &mut view.top,
            (None, false) => &mut self.top,
            (None, true) => &mut focus_top,
        };
        if body.focus.start < *top {
            *top = body.focus.start;
        }
        if body.focus.end > *top + capacity {
            *top = body.focus.end.saturating_sub(capacity);
        }
        *top = (*top).min(body.lines.len().saturating_sub(capacity));
        let skip = *top;
        self.row_targets = body
            .targets
            .into_iter()
            .filter_map(|(rows, index)| {
                let start = rows.start.max(skip);
                let end = rows.end.min(skip + capacity);
                (start < end).then(|| (start - skip + 1..end - skip + 1, index))
            })
            .collect();
        lines.extend(body.lines.into_iter().skip(skip).take(capacity));
        lines.resize(height.saturating_sub(1 + status.len()), RoleLine::default());
        lines.extend(status);
        lines.push(RoleLine::role(body.hint, Role::MutedText));
        lines.truncate(height);
        for line in &mut lines {
            line.truncate(width);
        }
        ConversationPickerFrame {
            lines,
            cursor: body
                .cursor
                .filter(|(row, _)| (skip..skip + capacity).contains(row))
                .map(|(row, column)| (row - skip + 1, column)),
        }
    }
}

#[derive(Default)]
struct Body {
    lines: Vec<RoleLine>,
    focus: std::ops::Range<usize>,
    targets: Vec<(std::ops::Range<usize>, usize)>,
    cursor: Option<(usize, usize)>,
    hint: String,
}

fn login_lines(login: &LoginView, width: usize, display: DisplayProfile, body: &mut Body) {
    let context_width = width.saturating_sub(INDENT.len()).max(1);
    let context = |body: &mut Body, text: &str, role: Role| {
        body.lines.extend(
            wrap_text(&safe_line(text), context_width)
                .into_iter()
                .map(|line| RoleLine::role(format!("{INDENT}{line}"), role)),
        );
    };
    body.lines.push(RoleLine::role(
        format!("Log in to {}", safe_line(&login.name)),
        Role::Text,
    ));
    match &login.stage {
        LoginStage::Starting => {
            context(body, "Finding the server's authorization…", Role::MutedText);
            body.focus = 0..body.lines.len();
            body.hint = fit_hint(&[("esc cancel".into(), 9)], width, display);
        }
        LoginStage::Waiting {
            link,
            opened,
            callback,
            error,
        } => {
            context(
                body,
                if *opened {
                    "Opening your browser. If no page appears, open this link:"
                } else {
                    "Open this link in a browser:"
                },
                Role::MutedText,
            );
            // Every row of a wrapped link opens the whole link.
            body.lines.extend(
                wrap_text(link, context_width)
                    .into_iter()
                    .map(|piece| RoleLine::linked(format!("{INDENT}{piece}"), Role::Accent, link)),
            );
            context(
                body,
                "After you approve, the browser returns here. When the browser runs on another machine, paste the address it ends on:",
                Role::MutedText,
            );
            if let Some(error) = error {
                context(body, error, Role::Error);
            }
            let input = editor_window(callback, width.saturating_sub(2));
            let before_cursor = input.split('▏').next().unwrap_or_default();
            let row = body.lines.len();
            body.lines
                .push(RoleLine::role(format!("> {input}"), Role::Text));
            body.cursor = Some((
                row,
                (2 + before_cursor.width()).min(width.saturating_sub(1)),
            ));
            body.focus = row..row + 1;
            body.hint = fit_hint(
                &[
                    ("enter submit".into(), 10),
                    ("ctrl+g editor".into(), 3),
                    ("esc cancel".into(), 9),
                ],
                width,
                display,
            );
        }
    }
}

/// Global servers first, then this directory's, then each plugin's.
fn source_rank(source: &str) -> u8 {
    match source {
        GLOBAL_SOURCE => 0,
        DIRECTORY_SOURCE => 1,
        _ => 2,
    }
}

fn source_heading(source: &str) -> String {
    match source.strip_prefix(PLUGIN_SOURCE_PREFIX) {
        Some(plugin) => format!("Plugin {plugin}"),
        None if source == DIRECTORY_SOURCE => "This directory".into(),
        None => "Global".into(),
    }
}

/// Whether the state of `server` in `listing` comes from a connection rather than from the
/// configuration.
fn connection_backed(listing: &McpListing, server: &McpServerStatus) -> bool {
    listing.live || listing.probed.contains(&server.name)
}

fn tool_count(count: usize) -> String {
    match count {
        1 => "1 tool".to_owned(),
        count => format!("{count} tools"),
    }
}

fn state_label(state: &McpServerState, live: bool) -> &'static str {
    match state {
        McpServerState::Connected => "connected",
        McpServerState::Connecting if live => "connecting",
        McpServerState::Connecting => "starts with the Agent",
        McpServerState::Failed => "failed",
        McpServerState::NeedsLogin => "needs login",
        McpServerState::NeedsConfiguration => "needs configuration",
        McpServerState::Disabled => "disabled",
        McpServerState::Unsupported => "not supported yet",
    }
}

fn state_role(state: &McpServerState, live: bool) -> Role {
    match state {
        McpServerState::Connected => Role::Success,
        McpServerState::Connecting if live => Role::Accent,
        McpServerState::Failed => Role::Error,
        McpServerState::NeedsLogin | McpServerState::NeedsConfiguration => Role::Warning,
        _ => Role::MutedText,
    }
}

/// A server's transport, state, tool count and exposure, as its row shows them.
fn row_details(server: &McpServerStatus, live: bool) -> String {
    let mut parts = Vec::new();
    if !server.transport.is_empty() {
        parts.push(server.transport.clone());
    }
    parts.push(state_label(&server.state, live).to_owned());
    if server.oauth && server.logged_in && server.state != McpServerState::Connected {
        parts.push("logged in".to_owned());
    }
    if live && server.state == McpServerState::Connected {
        parts.push(tool_count(server.tools.len()));
    }
    parts.push(server.exposure.clone());
    safe_line(&parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_harness::protocol::mcp::McpToolSummary;

    const WIDTH: usize = 90;
    const HEIGHT: usize = 16;

    fn server(name: &str, source: &str, state: McpServerState, tools: usize) -> McpServerStatus {
        McpServerStatus {
            name: name.into(),
            source: source.into(),
            transport: "http".into(),
            state,
            exposure: "direct".into(),
            tools: (0..tools)
                .map(|index| McpToolSummary {
                    name: format!("mcp__{name}__tool{index}"),
                    description: format!("Tool {index}"),
                })
                .collect(),
            failure: None,
            oauth: true,
            logged_in: false,
        }
    }

    fn loaded(servers: Vec<McpServerStatus>, live: bool) -> McpPanel {
        let mut panel = McpPanel::new(None);
        panel.apply(McpResult::Servers {
            read: 1,
            listing: Ok(McpListing {
                servers,
                live,
                directory: "/work".into(),
                probed: BTreeSet::new(),
            }),
        });
        panel
    }

    fn press(panel: &mut McpPanel, code: KeyCode) -> McpPanelEffect {
        panel.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn text(panel: &mut McpPanel) -> String {
        panel
            .frame(WIDTH, HEIGHT, DisplayProfile::default())
            .lines
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn selected(panel: &mut McpPanel) -> Vec<String> {
        panel
            .frame(WIDTH, HEIGHT, DisplayProfile::default())
            .lines
            .iter()
            .filter(|line| line.fill == Some(Role::Selection))
            .map(|line| line.text().trim().to_owned())
            .collect()
    }

    /// Whether a row shows `name` with `details` beside it.
    fn row_shows(frame: &str, name: &str, details: &str) -> bool {
        frame
            .lines()
            .any(|line| line.contains(&format!("● {name} ")) && line.trim_end().ends_with(details))
    }

    fn work(effect: McpPanelEffect) -> McpRequest {
        match effect {
            McpPanelEffect::Work(request) => request,
            _ => panic!("no work started"),
        }
    }

    #[test]
    fn servers_are_grouped_by_source_with_state_tools_and_exposure() {
        let mut panel = loaded(
            vec![
                server("local", "directory", McpServerState::Failed, 0),
                server("notes", "global", McpServerState::Connected, 2),
                server(
                    "plugin_docs_search",
                    "plugin:docs",
                    McpServerState::NeedsLogin,
                    0,
                ),
            ],
            true,
        );
        let frame = text(&mut panel);
        for (name, details) in [
            ("notes", "http · connected · 2 tools · direct"),
            ("local", "http · failed · direct"),
            ("plugin_docs_search", "http · needs login · direct"),
        ] {
            assert!(
                row_shows(&frame, name, details),
                "{name}: {details:?} in\n{frame}"
            );
        }
        let headings = frame
            .lines()
            .filter(|line| !line.contains('●'))
            .collect::<Vec<_>>();
        assert_eq!(
            headings[..4],
            ["MCP", "Global", "This directory", "Plugin docs"],
            "{frame}"
        );
        assert!(
            frame.contains("enter actions · e exposure · r refresh · esc close"),
            "{frame}"
        );
        assert!(selected(&mut panel)[0].contains("notes"));
        press(&mut panel, KeyCode::Char('j'));
        assert!(selected(&mut panel)[0].contains("local"));
        panel.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(selected(&mut panel)[0].contains("plugin_docs_search"));
        panel.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        press(&mut panel, KeyCode::Char('k'));
        assert!(selected(&mut panel)[0].contains("notes"));
        assert!(matches!(
            press(&mut panel, KeyCode::Esc),
            McpPanelEffect::Close
        ));
    }

    #[test]
    fn before_the_agent_starts_runnable_servers_wait_for_it() {
        let mut panel = loaded(
            vec![server("notes", "global", McpServerState::Connecting, 0)],
            false,
        );
        assert!(row_shows(
            &text(&mut panel),
            "notes",
            "http · starts with the Agent · direct"
        ));
        press(&mut panel, KeyCode::Enter);
        let frame = text(&mut panel);
        assert!(!frame.contains("Reconnect"), "{frame}");
        assert!(frame.contains("Log in"), "{frame}");
        assert!(frame.contains("  Connect\n"), "{frame}");
    }

    #[test]
    fn a_server_whose_configuration_failed_cannot_connect_before_the_agent_starts() {
        let mut panel = loaded(
            vec![server("notes", "global", McpServerState::Failed, 0)],
            false,
        );
        press(&mut panel, KeyCode::Enter);
        let frame = text(&mut panel);
        assert!(!frame.contains("Connect"), "{frame}");
    }

    /// A listing from the configuration, with `probed` naming the servers connected for the
    /// panel.
    fn listing(servers: Vec<McpServerStatus>, live: bool, probed: &[&str]) -> McpListing {
        McpListing {
            servers,
            live,
            directory: "/work".into(),
            probed: probed.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    fn logged_in(mut status: McpServerStatus) -> McpServerStatus {
        status.logged_in = true;
        status
    }

    /// Logs in to the first server from its actions, as the Console would finish it.
    fn log_in(panel: &mut McpPanel) {
        press(panel, KeyCode::Enter);
        assert_eq!(selected(panel), ["❯ Log in"]);
        let McpRequest::LogIn { attempt, name } = work(press(panel, KeyCode::Enter)) else {
            panic!("a login starts");
        };
        assert!(panel.apply(McpResult::LoginEnded {
            attempt,
            name,
            result: Ok(()),
        }));
    }

    fn status_line(panel: &mut McpPanel) -> String {
        let frame = panel.frame(WIDTH, HEIGHT, DisplayProfile::default());
        frame.lines[HEIGHT - 2].text().trim().to_owned()
    }

    #[test]
    fn after_a_login_the_agent_connects_the_server_and_the_row_shows_its_tools() {
        let mut panel = loaded(
            vec![server("notes", "global", McpServerState::NeedsLogin, 0)],
            true,
        );
        log_in(&mut panel);
        assert_eq!(status_line(&mut panel), "Logged in to notes · connecting…");
        // A read before the Agent took up the login still shows the old state.
        panel.apply(McpResult::Servers {
            read: 2,
            listing: Ok(listing(
                vec![logged_in(server(
                    "notes",
                    "global",
                    McpServerState::NeedsLogin,
                    0,
                ))],
                true,
                &[],
            )),
        });
        assert_eq!(status_line(&mut panel), "Logged in to notes · connecting…");
        panel.apply(McpResult::Reconnected {
            name: "notes".into(),
            result: Ok(()),
        });
        panel.apply(McpResult::Servers {
            read: 3,
            listing: Ok(listing(
                vec![logged_in(server(
                    "notes",
                    "global",
                    McpServerState::Connecting,
                    0,
                ))],
                true,
                &[],
            )),
        });
        let frame = text(&mut panel);
        assert!(
            row_shows(&frame, "notes", "http · connecting · logged in · direct"),
            "{frame}"
        );
        assert!(panel.awaits_connections());
        assert_eq!(status_line(&mut panel), "Logged in to notes · connecting…");
        panel.apply(McpResult::Servers {
            read: 4,
            listing: Ok(listing(
                vec![logged_in(server(
                    "notes",
                    "global",
                    McpServerState::Connected,
                    191,
                ))],
                true,
                &[],
            )),
        });
        let frame = text(&mut panel);
        assert!(
            row_shows(&frame, "notes", "http · connected · 191 tools · direct"),
            "{frame}"
        );
        assert_eq!(
            status_line(&mut panel),
            "Logged in to notes · connected · 191 tools"
        );
    }

    #[test]
    fn before_the_agent_starts_a_login_connects_for_the_panel_and_shows_a_failure_with_its_cause() {
        let mut panel = loaded(
            vec![server("notes", "global", McpServerState::Connecting, 0)],
            false,
        );
        log_in(&mut panel);
        panel.apply(McpResult::Reconnected {
            name: "notes".into(),
            result: Ok(()),
        });
        let connecting = logged_in(server("notes", "global", McpServerState::Connecting, 0));
        panel.apply(McpResult::Servers {
            read: 2,
            listing: Ok(listing(vec![connecting.clone()], false, &["notes"])),
        });
        let frame = text(&mut panel);
        assert!(
            row_shows(&frame, "notes", "http · connecting · logged in · direct"),
            "{frame}"
        );
        assert!(!panel.awaits_connections());
        let mut failed = logged_in(server("notes", "global", McpServerState::Failed, 0));
        failed.failure = Some("MCP connection failed: HTTP 500".into());
        panel.apply(McpResult::Servers {
            read: 3,
            listing: Ok(listing(vec![failed], false, &["notes"])),
        });
        let frame = text(&mut panel);
        assert!(
            row_shows(&frame, "notes", "http · failed · logged in · direct"),
            "{frame}"
        );
        assert_eq!(
            status_line(&mut panel),
            "Logged in to notes, but cannot connect: MCP connection failed: HTTP 500"
        );
        press(&mut panel, KeyCode::Enter);
        let frame = text(&mut panel);
        assert!(frame.contains("MCP connection failed: HTTP 500"), "{frame}");
        assert!(frame.contains("Reconnect"), "{frame}");
    }

    #[test]
    fn before_the_agent_starts_a_server_connected_for_the_panel_shows_its_tools() {
        let mut panel = loaded(
            vec![
                server("notes", "global", McpServerState::Connecting, 0),
                server("local", "directory", McpServerState::Connecting, 0),
            ],
            false,
        );
        panel.apply(McpResult::Servers {
            read: 2,
            listing: Ok(listing(
                vec![
                    server("notes", "global", McpServerState::Connected, 2),
                    server("local", "directory", McpServerState::Connecting, 0),
                ],
                false,
                &["notes"],
            )),
        });
        let frame = text(&mut panel);
        assert!(
            row_shows(&frame, "notes", "http · connected · 2 tools · direct"),
            "{frame}"
        );
        assert!(
            row_shows(&frame, "local", "http · starts with the Agent · direct"),
            "{frame}"
        );
        press(&mut panel, KeyCode::Enter);
        assert_eq!(selected(&mut panel), ["❯ View tools"]);
        assert!(text(&mut panel).contains("Reconnect"));
    }

    #[test]
    fn a_reconnect_tells_how_the_connection_ended() {
        let mut panel = loaded(
            vec![server("notes", "global", McpServerState::Failed, 0)],
            true,
        );
        press(&mut panel, KeyCode::Enter);
        press(&mut panel, KeyCode::Char('j'));
        assert_eq!(selected(&mut panel), ["❯ Reconnect"]);
        assert_eq!(
            work(press(&mut panel, KeyCode::Enter)),
            McpRequest::Reconnect("notes".into())
        );
        assert_eq!(status_line(&mut panel), "Reconnecting notes…");
        panel.apply(McpResult::Reconnected {
            name: "notes".into(),
            result: Ok(()),
        });
        assert_eq!(status_line(&mut panel), "Connecting to notes…");
        panel.apply(McpResult::Servers {
            read: 2,
            listing: Ok(listing(
                vec![server("notes", "global", McpServerState::Connected, 1)],
                true,
                &[],
            )),
        });
        assert_eq!(status_line(&mut panel), "Connected to notes · 1 tool");
    }

    #[test]
    fn actions_list_tools_reconnect_and_toggle_the_directory() {
        let mut panel = loaded(
            vec![server("notes", "global", McpServerState::Connected, 2)],
            true,
        );
        press(&mut panel, KeyCode::Enter);
        assert_eq!(selected(&mut panel), ["❯ View tools"]);
        let frame = text(&mut panel);
        for option in [
            "Log in",
            "Reconnect",
            "Disable in this directory",
            "Back",
            "enter confirm · esc back",
        ] {
            assert!(frame.contains(option), "{option:?} in\n{frame}");
        }
        assert!(!frame.contains("1."), "options carry no numbers");
        press(&mut panel, KeyCode::Enter);
        let frame = text(&mut panel);
        assert!(frame.contains("mcp__notes__tool0 · Tool 0"), "{frame}");
        assert_eq!(selected(&mut panel), ["mcp__notes__tool0 · Tool 0"]);
        press(&mut panel, KeyCode::Char('j'));
        assert_eq!(selected(&mut panel), ["mcp__notes__tool1 · Tool 1"]);
        press(&mut panel, KeyCode::Esc);
        assert_eq!(selected(&mut panel), ["❯ View tools"]);
        panel.apply(McpResult::Servers {
            read: 2,
            listing: Ok(McpListing {
                servers: vec![server("notes", "global", McpServerState::Connected, 2)],
                live: true,
                directory: "/work".into(),
                probed: BTreeSet::new(),
            }),
        });
        assert_eq!(
            selected(&mut panel),
            ["❯ View tools"],
            "a new read keeps the menu open"
        );
        for _ in 0..2 {
            press(&mut panel, KeyCode::Char('j'));
        }
        assert_eq!(
            work(press(&mut panel, KeyCode::Enter)),
            McpRequest::Reconnect("notes".into())
        );
        assert!(text(&mut panel).contains("Reconnecting notes…"));
        press(&mut panel, KeyCode::Enter);
        press(&mut panel, KeyCode::Char('j'));
        press(&mut panel, KeyCode::Char('j'));
        press(&mut panel, KeyCode::Char('j'));
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            McpPanelEffect::None
        ));
        assert!(text(&mut panel).contains("Wait for it to finish"));
        panel.apply(McpResult::Reconnected {
            name: "notes".into(),
            result: Ok(()),
        });
        press(&mut panel, KeyCode::Esc);
        press(&mut panel, KeyCode::Enter);
        for _ in 0..3 {
            press(&mut panel, KeyCode::Char('j'));
        }
        assert_eq!(
            work(press(&mut panel, KeyCode::Enter)),
            McpRequest::SetDisabled {
                name: "notes".into(),
                disabled: true
            }
        );
    }

    #[test]
    fn a_login_shows_its_link_takes_a_pasted_callback_and_esc_cancels() {
        let mut panel = loaded(
            vec![server("notes", "global", McpServerState::NeedsLogin, 0)],
            true,
        );
        press(&mut panel, KeyCode::Enter);
        assert_eq!(selected(&mut panel), ["❯ Log in"]);
        let McpRequest::LogIn { attempt, .. } = work(press(&mut panel, KeyCode::Enter)) else {
            panic!("a login starts");
        };
        assert!(text(&mut panel).contains("Finding the server's authorization…"));
        assert!(panel.apply(McpResult::LoginWaiting {
            attempt,
            link: "https://auth.example.com/authorize?state=1".into(),
            opened: false,
        }));
        let frame = panel.frame(WIDTH, HEIGHT, DisplayProfile::default());
        let shown = frame
            .lines
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            shown.contains("https://auth.example.com/authorize?state=1"),
            "{shown}"
        );
        assert!(
            shown.contains("enter submit · ctrl+g editor · esc cancel"),
            "{shown}"
        );
        assert!(frame.cursor.is_some());
        for character in "jk".chars() {
            press(&mut panel, KeyCode::Char(character));
        }
        panel.paste("http://localhost:1/callback?code=c");
        assert_eq!(
            panel.callback_text(),
            "jkhttp://localhost:1/callback?code=c"
        );
        assert!(matches!(
            panel.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            McpPanelEffect::EditCallback
        ));
        panel.replace_callback("http://localhost:1/callback?code=c");
        assert_eq!(
            work(press(&mut panel, KeyCode::Enter)),
            McpRequest::SubmitCallback {
                attempt,
                address: "http://localhost:1/callback?code=c".into()
            }
        );
        panel.apply(McpResult::CallbackRefused {
            attempt,
            error: "that is not this login's callback".into(),
        });
        assert!(text(&mut panel).contains("that is not this login's callback"));
        assert_eq!(
            work(press(&mut panel, KeyCode::Esc)),
            McpRequest::CancelLogIn(attempt)
        );
        let frame = text(&mut panel);
        assert!(frame.contains("Cancelled the login to notes"), "{frame}");
        assert!(!panel.apply(McpResult::LoginEnded {
            attempt,
            name: "notes".into(),
            result: Err("the login was cancelled".into()),
        }));
    }

    #[test]
    fn a_stale_read_does_not_replace_a_newer_one() {
        let mut panel = loaded(
            vec![server("notes", "global", McpServerState::Connected, 0)],
            true,
        );
        panel.apply(McpResult::Servers {
            read: 3,
            listing: Ok(McpListing {
                servers: vec![server("notes", "global", McpServerState::Disabled, 0)],
                live: true,
                directory: "/work".into(),
                probed: BTreeSet::new(),
            }),
        });
        panel.apply(McpResult::Servers {
            read: 2,
            listing: Ok(McpListing {
                servers: Vec::new(),
                live: true,
                directory: "/work".into(),
                probed: BTreeSet::new(),
            }),
        });
        assert!(row_shows(
            &text(&mut panel),
            "notes",
            "http · disabled · direct"
        ));
    }

    fn exposed(name: &str, source: &str, exposure: &str) -> McpServerStatus {
        McpServerStatus {
            exposure: exposure.into(),
            ..server(name, source, McpServerState::Connected, 1)
        }
    }

    fn set_exposure(name: &str, source: &str, exposure: ToolExposure) -> McpRequest {
        McpRequest::SetExposure {
            name: name.into(),
            source: source.into(),
            exposure,
        }
    }

    #[test]
    fn e_cycles_the_selected_servers_exposure_and_its_row_follows() {
        let mut panel = loaded(
            vec![
                exposed("notes", "global", "search"),
                exposed("plugin_docs_search", "plugin:docs", "search"),
            ],
            true,
        );
        press(&mut panel, KeyCode::Char('j'));
        for (shown, next) in [
            ("search", ToolExposure::Direct),
            ("direct", ToolExposure::Hidden),
            ("hidden", ToolExposure::Search),
        ] {
            assert!(row_shows(
                &text(&mut panel),
                "plugin_docs_search",
                &format!("connected · 1 tool · {shown}")
            ));
            assert_eq!(
                work(press(&mut panel, KeyCode::Char('e'))),
                set_exposure("plugin_docs_search", "plugin:docs", next)
            );
            assert!(text(&mut panel).contains("Changing the exposure of plugin_docs_search…"));
            panel.apply(McpResult::ExposureSet {
                name: "plugin_docs_search".into(),
                exposure: next,
                result: Ok(()),
            });
        }
        let frame = text(&mut panel);
        assert!(
            row_shows(&frame, "plugin_docs_search", "connected · 1 tool · search"),
            "{frame}"
        );
        assert!(
            frame.contains("Exposure of plugin_docs_search is now search"),
            "{frame}"
        );
        assert!(row_shows(&frame, "notes", "connected · 1 tool · search"));

        panel.apply(McpResult::ExposureSet {
            name: "notes".into(),
            exposure: ToolExposure::Direct,
            result: Err("settings file is not valid JSON".into()),
        });
        let frame = text(&mut panel);
        assert!(row_shows(&frame, "notes", "connected · 1 tool · search"));
        assert!(
            frame.contains("Cannot change the exposure of notes: settings file is not valid JSON"),
            "{frame}"
        );
    }

    #[test]
    fn e_acts_only_on_the_list() {
        let mut panel = loaded(vec![exposed("notes", "global", "search")], true);
        press(&mut panel, KeyCode::Enter);
        assert!(matches!(
            press(&mut panel, KeyCode::Char('e')),
            McpPanelEffect::None
        ));
        assert_eq!(selected(&mut panel), ["❯ View tools"]);
        press(&mut panel, KeyCode::Enter);
        assert!(matches!(
            press(&mut panel, KeyCode::Char('e')),
            McpPanelEffect::None
        ));
        assert!(text(&mut panel).contains("mcp__notes__tool0"));
        let mut empty = loaded(Vec::new(), true);
        assert!(matches!(
            press(&mut empty, KeyCode::Char('e')),
            McpPanelEffect::None
        ));
        assert!(!text(&mut empty).contains("e exposure"));
    }

    #[test]
    fn the_actions_menu_offers_each_exposure_with_the_current_one_under_the_cursor() {
        let mut panel = loaded(vec![exposed("notes", "directory", "search")], true);
        press(&mut panel, KeyCode::Enter);
        for _ in 0..4 {
            press(&mut panel, KeyCode::Char('j'));
        }
        assert_eq!(selected(&mut panel), ["❯ Change exposure"]);
        press(&mut panel, KeyCode::Enter);
        assert_eq!(selected(&mut panel), ["❯ search"]);
        let frame = text(&mut panel);
        let options = ["  direct", "❯ search", "  hidden", "  Back"].map(|option| {
            frame
                .find(option)
                .unwrap_or_else(|| panic!("{option:?} in\n{frame}"))
        });
        assert!(options.windows(2).all(|pair| pair[0] < pair[1]), "{frame}");
        assert!(frame.contains("enter confirm · esc back"), "{frame}");
        press(&mut panel, KeyCode::Esc);
        assert_eq!(
            selected(&mut panel),
            ["❯ Change exposure"],
            "esc goes back to the actions"
        );
        press(&mut panel, KeyCode::Enter);
        panel.apply(McpResult::Servers {
            read: 2,
            listing: Ok(McpListing {
                servers: vec![exposed("notes", "directory", "search")],
                live: true,
                directory: "/work".into(),
                probed: BTreeSet::new(),
            }),
        });
        assert_eq!(
            selected(&mut panel),
            ["❯ search"],
            "a new read keeps the choice open"
        );
        press(&mut panel, KeyCode::Char('j'));
        assert_eq!(
            work(press(&mut panel, KeyCode::Enter)),
            set_exposure("notes", "directory", ToolExposure::Hidden)
        );
        assert!(text(&mut panel).contains("Changing the exposure of notes…"));
    }
}
