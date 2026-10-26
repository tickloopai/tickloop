//! `/plugins`: the Tick packages panel of the input dialog. The Installed page lists the installed
//! packages with their versions and login states, and enter opens a package's actions there; the
//! Marketplace page lists what the Tick marketplace offers, and enter installs or updates it.
//! Logging in happens in the panel: the link and code of an OAuth device authorization, or a
//! masked entry of a key. The panel keeps what it shows and what the user chose; the Console runs
//! the work and hands each result back through [`TicksPanel::apply`].

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tick_packages::{
    ApiKey, CURRENT_TARGET, LoginState, Marketplace, MarketplacePackage, OAuthIssuer, PackageAuth,
};
use tick_plugins::{
    BrokenPlugin, InstallOutcome, Installation, InstalledPlugin, InstalledPlugins, PluginManifest,
    Uninstalled,
};
use tickloop_tick_installer::{
    InstalledVersion, LoggedOut, LoginError, Offer, login_label, offers,
};
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    console::wrap_text,
    conversation_picker::{ConversationPickerFrame, compact_line, fit_hint},
    display::{DisplayProfile, Glyph},
    editor::{Editor, EditorCommand},
    list_row::ListRow,
    permissions_surface::{edit_text, editor_window, tab_header_line},
    safe_line,
    style::{Role, RoleLine},
};

const UNINSTALL_CONFIRMATION_WINDOW: Duration = Duration::from_secs(3);
const TITLE: &str = "Plugins";
const PAGES: [&str; 2] = ["Installed", "Available"];
const INSTALLED_PAGE: usize = 0;
const MARKETPLACE_PAGE: usize = 1;
/// The header and the key hints.
const CHROME_ROWS: usize = 2;
const EMPTY_INDENT: &str = "  ";
const CONTEXT_INDENT: &str = "  ";

/// Work the panel asks the Console to do; its result comes back as a [`TickPackageResult`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TickPackageRequest {
    /// Reads the installed packages with their login states, and the Tick marketplace.
    Load,
    /// Installs or updates to the version the marketplace offers.
    Install(MarketplacePackage),
    Uninstall(String),
    SetEnabled {
        name: String,
        enabled: bool,
    },
    /// Logs the package in at `issuer` with an OAuth device authorization the user approves;
    /// the panel numbers each `attempt` as it starts, and its results carry the number.
    LogIn {
        name: String,
        issuer: OAuthIssuer,
        scopes: Vec<String>,
        attempt: u64,
    },
    /// Stops the login `attempt`.
    CancelLogIn(u64),
    /// Saves the key the user entered for the package.
    SaveKey {
        name: String,
        key: ApiKey,
    },
    LogOut(String),
    /// Reads the package's login state again.
    RefreshLogin(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TickPackageWork {
    pub(super) panel: Uuid,
    pub(super) request: TickPackageRequest,
}

/// The installed packages, with the login state of each usable one in the same order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct InstalledList {
    pub(super) packages: InstalledPlugins,
    pub(super) logins: Vec<LoginState>,
}

#[derive(Debug)]
pub(super) enum TickPackageResult {
    /// The installed packages as the read numbered `read` found them; reads are numbered in the
    /// order they start, so one that started later is the more recent.
    Installed {
        read: u64,
        listing: Result<InstalledList, String>,
    },
    Marketplace(Result<Marketplace, String>),
    Login {
        name: String,
        state: LoginState,
    },
    Installation {
        name: String,
        result: Result<Box<Installation>, String>,
    },
    Uninstallation {
        name: String,
        result: Result<Uninstalled, String>,
    },
    Enabled {
        name: String,
        enabled: bool,
        result: Result<(), String>,
    },
    Logout {
        name: String,
        result: Result<LoggedOut, String>,
    },
    /// The package's login waits for the user to open `link` and confirm `code`; `opened` says
    /// whether the browser was asked to open it.
    LoginWaiting {
        attempt: u64,
        link: String,
        code: String,
        opened: bool,
    },
    LoginEnded {
        attempt: u64,
        name: String,
        result: Result<(), LoginError>,
    },
    KeySaved {
        name: String,
        result: Result<(), String>,
    },
}

/// What became of a result handed to a panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Applied {
    Taken,
    /// It belonged to a login that is no longer shown.
    Ignored,
    /// The panel is closed.
    NoPanel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Notice {
    pub(super) text: String,
    pub(super) is_failure: bool,
}

impl Notice {
    fn success(text: String) -> Self {
        Self {
            text,
            is_failure: false,
        }
    }

    fn failure(text: String) -> Self {
        Self {
            text,
            is_failure: true,
        }
    }
}

impl TickPackageResult {
    /// What an operation's result tells the user; `None` for a result that only refreshes a list.
    pub(super) fn notice(&self) -> Option<Notice> {
        Some(match self {
            Self::Installed { .. }
            | Self::Marketplace(_)
            | Self::Login { .. }
            | Self::LoginWaiting { .. } => return None,
            Self::LoginEnded {
                name,
                result: Ok(()),
                ..
            } => Notice::success(format!("Logged in to {name}")),
            Self::LoginEnded {
                name,
                result: Err(LoginError::Cancelled),
                ..
            } => Notice::success(format!("Cancelled the login to {name}")),
            Self::LoginEnded {
                name,
                result: Err(error),
                ..
            } => Notice::failure(format!("Cannot log in to {name}: {error}")),
            Self::Enabled {
                name,
                enabled,
                result,
            } => match result {
                Ok(()) => Notice::success(format!(
                    "{} {name}",
                    if *enabled { "Enabled" } else { "Disabled" }
                )),
                Err(error) => Notice::failure(format!(
                    "Cannot {} {name}: {error}",
                    if *enabled { "enable" } else { "disable" }
                )),
            },
            Self::KeySaved {
                name,
                result: Ok(()),
            } => Notice::success(format!("Saved the key for {name}")),
            Self::KeySaved {
                name,
                result: Err(error),
            } => Notice::failure(format!("Cannot save the key for {name}: {error}")),
            Self::Installation {
                name,
                result: Err(error),
            } => Notice::failure(format!("Installing {name} failed: {error}")),
            Self::Installation {
                result: Ok(installation),
                ..
            } => {
                let manifest = &installation.plugin.manifest;
                let (name, version) = (&manifest.name, &installation.plugin.version);
                let mut text = match &installation.outcome {
                    InstallOutcome::Installed => format!("Installed {name} {version}"),
                    InstallOutcome::Replaced { previous_version }
                        if is_later(version, previous_version) =>
                    {
                        format!("Updated {name} {previous_version} to {version}")
                    }
                    InstallOutcome::Replaced { previous_version } => {
                        format!("Replaced {name} {previous_version} with {version}")
                    }
                    InstallOutcome::Reinstalled => format!("Reinstalled {name} {version}"),
                    InstallOutcome::AlreadyInstalled => {
                        format!("{name} {version} is already installed")
                    }
                };
                if manifest.tick.is_some() {
                    text.push_str("; Its Tick program runs as you when opened");
                }
                if let Some(warning) = unsupported_summary(manifest) {
                    text.push_str(&format!("; {warning}"));
                }
                Notice::success(text)
            }
            Self::Uninstallation {
                name,
                result: Err(error),
            } => Notice::failure(format!("Uninstalling {name} failed: {error}")),
            Self::Uninstallation {
                result: Ok(uninstalled),
                ..
            } => Notice::success(format!(
                "Uninstalled {} {}; {}its running Ticks keep running until closed",
                uninstalled.name,
                uninstalled.version,
                uninstalled
                    .credentials
                    .as_ref()
                    .map_or_else(String::new, |place| format!(
                        "its saved login stays in {}; ",
                        place.display()
                    )),
            )),
            Self::Logout {
                name,
                result: Ok(logged_out),
            } => Notice::success(
                match (logged_out.had_credentials, &logged_out.revocation_failure) {
                    (false, _) => format!("{name} was not logged in"),
                    (true, None) => format!("Logged out of {name}"),
                    (true, Some(failure)) => format!(
                        "Logged out of {name}; the authorization server did not revoke the token ({failure})"
                    ),
                },
            ),
            Self::Logout {
                name,
                result: Err(error),
            } => Notice::failure(format!("Logging out of {name} failed: {error}")),
        })
    }
}

pub(super) enum TicksPanelEffect {
    None,
    Close,
    EditFilter,
    Work(TickPackageRequest),
}

enum Listing<T> {
    Loading,
    Loaded(T),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Choice {
    Run(TickPackageRequest),
    /// Opens the masked entry of the key `label` names for the package.
    EnterKey {
        name: String,
        label: String,
    },
    Back,
}

/// A menu over the list, with the `❯` cursor: a package's actions, or a confirmation.
struct PanelMenu {
    title: String,
    context: Vec<String>,
    options: Vec<(String, Choice)>,
    selected: usize,
}

/// A package's login under way in the panel, in place of its list and menus; `attempt` is the
/// number of an OAuth login, 0 for a key entry.
struct LoginView {
    attempt: u64,
    name: String,
    /// The issuer's or the key's label.
    label: String,
    stage: LoginStage,
}

enum LoginStage {
    /// Reading the server's metadata, registering and asking for a device code.
    Starting,
    Waiting {
        link: String,
        code: String,
        opened: bool,
    },
    /// The key as typed so far, which only its length shows.
    Key { entered: String },
}

/// What a key does to the login view.
enum LoginAction {
    Cancel,
    Save,
    Edit,
    Ignore,
}

/// A row of the Installed page.
enum InstalledEntry<'a> {
    Usable {
        package: &'a InstalledPlugin,
        login: LoginState,
    },
    Unusable(&'a BrokenPlugin),
}

impl InstalledEntry<'_> {
    fn name(&self) -> &str {
        match self {
            Self::Usable { package, .. } => &package.manifest.name,
            Self::Unusable(package) => &package.name,
        }
    }
}

pub(super) struct TicksPanel {
    id: Uuid,
    pub(super) chat: Option<Uuid>,
    page: usize,
    filter: Editor,
    filtering: bool,
    uninstall_confirmation: Option<(String, Instant)>,
    installed: Listing<InstalledList>,
    /// The number of the read `installed` holds; a read that started before it is stale.
    installed_read: u64,
    marketplace: Listing<Marketplace>,
    selected: [usize; 2],
    top: [usize; 2],
    menus: Vec<PanelMenu>,
    login: Option<LoginView>,
    /// The number of the last OAuth login started.
    login_attempts: u64,
    /// The operation under way, as the panel shows it.
    busy: Option<String>,
    notice: Option<Notice>,
    /// The body rows each list row or menu option was drawn on, for pointer selection.
    row_targets: Vec<(std::ops::Range<usize>, usize)>,
}

impl TicksPanel {
    pub(super) fn new(chat: Option<Uuid>) -> Self {
        Self {
            id: tickloop_ids::generate(),
            chat,
            page: INSTALLED_PAGE,
            filter: Editor::new(),
            filtering: false,
            uninstall_confirmation: None,
            installed: Listing::Loading,
            installed_read: 0,
            marketplace: Listing::Loading,
            selected: [0; 2],
            top: [0; 2],
            menus: Vec::new(),
            login: None,
            login_attempts: 0,
            busy: None,
            notice: None,
            row_targets: Vec::new(),
        }
    }

    pub(super) fn id(&self) -> Uuid {
        self.id
    }

    /// Takes `result`, unless it belongs to a login that is no longer shown: the end of a login
    /// the user cancelled was told when it was cancelled.
    pub(super) fn apply(&mut self, result: TickPackageResult) -> Applied {
        let is_current = match &result {
            TickPackageResult::LoginWaiting { attempt, .. }
            | TickPackageResult::LoginEnded { attempt, .. } => self
                .login
                .as_ref()
                .is_some_and(|login| login.attempt == *attempt),
            _ => true,
        };
        if !is_current {
            return Applied::Ignored;
        }
        if let Some(notice) = result.notice() {
            self.notice = Some(notice);
        }
        match result {
            // A read that started before the one shown may predate an install or uninstall.
            TickPackageResult::Installed { read, .. } if read < self.installed_read => {}
            TickPackageResult::Installed { read, listing } => {
                self.installed_read = read;
                self.installed = listing.map_or_else(Listing::Failed, Listing::Loaded);
            }
            TickPackageResult::Marketplace(listing) => {
                self.marketplace = listing.map_or_else(Listing::Failed, Listing::Loaded);
            }
            TickPackageResult::Login { name, state } => {
                if let Listing::Loaded(list) = &mut self.installed
                    && let Some(index) = list
                        .packages
                        .plugins
                        .iter()
                        .position(|package| package.manifest.name == name)
                    && let Some(login) = list.logins.get_mut(index)
                {
                    *login = state;
                }
            }
            TickPackageResult::LoginWaiting {
                link, code, opened, ..
            } => {
                if let Some(login) = &mut self.login {
                    login.stage = LoginStage::Waiting { link, code, opened };
                }
            }
            TickPackageResult::LoginEnded { .. } => {
                self.login = None;
                self.busy = None;
            }
            TickPackageResult::Installation { .. }
            | TickPackageResult::Uninstallation { .. }
            | TickPackageResult::Logout { .. }
            | TickPackageResult::KeySaved { .. }
            | TickPackageResult::Enabled { .. } => self.busy = None,
        }
        for page in [INSTALLED_PAGE, MARKETPLACE_PAGE] {
            let count = self.entry_count(page);
            self.selected[page] = self.selected[page].min(count.saturating_sub(1));
        }
        Applied::Taken
    }

    /// Takes pasted text into the key being entered; elsewhere the panel takes no text.
    pub(super) fn filter_text(&self) -> &str {
        self.filter.draft()
    }

    pub(super) fn replace_filter(&mut self, text: &str) {
        let text = compact_line(text);
        let cursor = text.len();
        self.filter.replace_draft(text, cursor);
        self.selected[MARKETPLACE_PAGE] = 0;
        self.top[MARKETPLACE_PAGE] = 0;
    }

    pub(super) fn report(&mut self, error: String) {
        self.notice = Some(Notice::failure(error));
    }

    pub(super) fn paste(&mut self, text: &str) {
        self.cancel_uninstall();
        if self.filtering
            && self.page == MARKETPLACE_PAGE
            && self.menus.is_empty()
            && self.login.is_none()
        {
            self.filter
                .apply(EditorCommand::InsertPasted(compact_line(text)));
            self.selected[MARKETPLACE_PAGE] = 0;
            self.top[MARKETPLACE_PAGE] = 0;
            return;
        }
        if let Some(LoginView {
            stage: LoginStage::Key { entered },
            ..
        }) = &mut self.login
        {
            entered.extend(text.chars().filter(|character| !character.is_control()));
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> TicksPanelEffect {
        let uninstall_key =
            key.code == KeyCode::Char('x') && key.modifiers == KeyModifiers::CONTROL;
        if !uninstall_key {
            self.cancel_uninstall();
        }
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
        if let Some(menu) = self.menus.last_mut() {
            if let Some(down) = movement {
                menu.selected = step(menu.selected, menu.options.len(), down);
                return TicksPanelEffect::None;
            }
            return match key.code {
                KeyCode::Esc => {
                    self.menus.pop();
                    TicksPanelEffect::None
                }
                KeyCode::Enter => {
                    let choice = menu.options[menu.selected].1.clone();
                    self.choose(choice)
                }
                _ => TicksPanelEffect::None,
            };
        }
        if uninstall_key && self.page == INSTALLED_PAGE {
            if self.busy.is_some() {
                return TicksPanelEffect::None;
            }
            let Some(name) = self
                .installed_entries()
                .get(self.selected[INSTALLED_PAGE])
                .map(|entry| entry.name().to_owned())
            else {
                return TicksPanelEffect::None;
            };
            if self
                .uninstall_confirmation
                .as_ref()
                .is_some_and(|(pending, at)| {
                    pending == &name && at.elapsed() < UNINSTALL_CONFIRMATION_WINDOW
                })
            {
                self.cancel_uninstall();
                return self.start(TickPackageRequest::Uninstall(name));
            }
            self.uninstall_confirmation = Some((name, Instant::now()));
            return TicksPanelEffect::None;
        }
        if self.filtering && self.page == MARKETPLACE_PAGE {
            return self.filter_key(key);
        }
        if let Some(down) = movement {
            let count = self.entry_count(self.page);
            self.selected[self.page] = step(self.selected[self.page], count, down);
            return TicksPanelEffect::None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('/'), KeyModifiers::NONE) if self.page == MARKETPLACE_PAGE => {
                self.filtering = true;
                TicksPanelEffect::None
            }
            (KeyCode::Esc, _) => TicksPanelEffect::Close,
            (KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right, _) => {
                self.page = 1 - self.page;
                self.selected[self.page] =
                    self.selected[self.page].min(self.entry_count(self.page).saturating_sub(1));
                TicksPanelEffect::None
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => self.start(TickPackageRequest::Load),
            (KeyCode::Enter, _) => self.open_selected(),
            _ => TicksPanelEffect::None,
        }
    }

    fn filter_key(&mut self, key: KeyEvent) -> TicksPanelEffect {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.filtering = false
            }
            (KeyCode::Char('g'), KeyModifiers::CONTROL) => return TicksPanelEffect::EditFilter,
            (KeyCode::Enter, _) => return self.open_selected(),
            (KeyCode::Char('n' | 'p'), KeyModifiers::CONTROL) => {
                self.selected[MARKETPLACE_PAGE] = step(
                    self.selected[MARKETPLACE_PAGE],
                    self.entry_count(MARKETPLACE_PAGE),
                    key.code == KeyCode::Char('n'),
                );
            }
            _ => {
                let previous = self.filter.draft().to_owned();
                edit_text(&mut self.filter, key);
                if previous != self.filter.draft() {
                    self.selected[MARKETPLACE_PAGE] = 0;
                    self.top[MARKETPLACE_PAGE] = 0;
                }
            }
        }
        TicksPanelEffect::None
    }

    /// A key while a login is shown: the key entry takes text, enter saves it and esc cancels;
    /// an OAuth login only cancels, with esc or with enter on its one option.
    fn login_key(&mut self, key: KeyEvent) -> TicksPanelEffect {
        let Some(login) = &mut self.login else {
            return TicksPanelEffect::None;
        };
        let action = match (&mut login.stage, key.code) {
            (_, KeyCode::Esc) => LoginAction::Cancel,
            (LoginStage::Key { .. }, KeyCode::Enter) => LoginAction::Save,
            (LoginStage::Key { entered }, code) => {
                match (code, key.modifiers) {
                    (KeyCode::Backspace, _) => {
                        entered.pop();
                    }
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => entered.clear(),
                    (KeyCode::Char(character), modifiers)
                        if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        entered.push(character);
                    }
                    _ => {}
                }
                LoginAction::Edit
            }
            (_, KeyCode::Enter) => LoginAction::Cancel,
            _ => LoginAction::Ignore,
        };
        match action {
            LoginAction::Edit | LoginAction::Ignore => TicksPanelEffect::None,
            LoginAction::Save => {
                let Some(LoginView {
                    name,
                    stage: LoginStage::Key { entered },
                    ..
                }) = self.login.take()
                else {
                    return TicksPanelEffect::None;
                };
                match ApiKey::new(&entered) {
                    Ok(key) => self.start(TickPackageRequest::SaveKey { name, key }),
                    Err(error) => {
                        self.notice = Some(Notice::failure(error.to_string()));
                        self.login = Some(LoginView {
                            attempt: 0,
                            label: self.key_label(&name),
                            name,
                            stage: LoginStage::Key { entered },
                        });
                        TicksPanelEffect::None
                    }
                }
            }
            LoginAction::Cancel => {
                let Some(LoginView {
                    attempt,
                    name,
                    stage,
                    ..
                }) = self.login.take()
                else {
                    return TicksPanelEffect::None;
                };
                if matches!(stage, LoginStage::Key { .. }) {
                    return TicksPanelEffect::None;
                }
                self.busy = None;
                self.notice = Some(Notice::success(format!("Cancelled the login to {name}")));
                TicksPanelEffect::Work(TickPackageRequest::CancelLogIn(attempt))
            }
        }
    }

    /// The label of the key the package `name` logs in with.
    fn key_label(&self, name: &str) -> String {
        self.installed_entries()
            .into_iter()
            .find_map(|entry| match entry {
                InstalledEntry::Usable { package, .. } if package.manifest.name == name => {
                    match &package.manifest.auth {
                        PackageAuth::ApiKey { label, .. } => Some(label.clone()),
                        _ => None,
                    }
                }
                _ => None,
            })
            .unwrap_or_else(|| "key".to_owned())
    }

    pub(super) fn cancel_uninstall(&mut self) {
        self.uninstall_confirmation = None;
    }

    pub(super) fn expire_confirmation(&mut self) -> bool {
        if self
            .uninstall_confirmation
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= UNINSTALL_CONFIRMATION_WINDOW)
        {
            self.cancel_uninstall();
            true
        } else {
            false
        }
    }

    pub(super) fn select_at_row(&mut self, row: usize) -> bool {
        self.cancel_uninstall();
        if self.login.is_some() {
            // The login's one option is the only target.
            return self.row_targets.iter().any(|(rows, _)| rows.contains(&row));
        }
        let Some(index) = self
            .row_targets
            .iter()
            .find(|(rows, _)| rows.contains(&row))
            .map(|(_, index)| *index)
        else {
            return false;
        };
        match self.menus.last_mut() {
            Some(menu) => menu.selected = index,
            None => self.selected[self.page] = index,
        }
        true
    }

    fn choose(&mut self, choice: Choice) -> TicksPanelEffect {
        match choice {
            Choice::Back => {
                self.menus.pop();
                TicksPanelEffect::None
            }
            Choice::EnterKey { name, label } => {
                if self.busy.is_some() {
                    self.notice = Some(Notice::failure(
                        "Wait for it to finish before starting another".to_owned(),
                    ));
                    return TicksPanelEffect::None;
                }
                self.menus.clear();
                self.notice = None;
                self.login = Some(LoginView {
                    attempt: 0,
                    name,
                    label,
                    stage: LoginStage::Key {
                        entered: String::new(),
                    },
                });
                TicksPanelEffect::None
            }
            Choice::Run(request) => {
                let effect = self.start(request);
                if matches!(effect, TicksPanelEffect::Work(_)) {
                    self.menus.clear();
                }
                effect
            }
        }
    }

    /// Starts `request` unless an operation is under way; reading lists never waits.
    fn start(&mut self, mut request: TickPackageRequest) -> TicksPanelEffect {
        let busy = match &request {
            TickPackageRequest::Load => {
                self.installed = Listing::Loading;
                self.marketplace = Listing::Loading;
                return TicksPanelEffect::Work(request);
            }
            TickPackageRequest::RefreshLogin(_) | TickPackageRequest::CancelLogIn(_) => {
                return TicksPanelEffect::Work(request);
            }
            TickPackageRequest::Install(package) => match self.installed_version(&package.name) {
                Some(installed) if is_later(&package.version, installed) => {
                    format!("Updating {} to {}…", package.name, package.version)
                }
                Some(_) => format!("Replacing {} with {}…", package.name, package.version),
                None => format!("Installing {} {}…", package.name, package.version),
            },
            TickPackageRequest::SetEnabled { name, enabled } => format!(
                "{} {name}…",
                if *enabled { "Enabling" } else { "Disabling" }
            ),
            TickPackageRequest::Uninstall(name) => format!("Uninstalling {name}…"),
            TickPackageRequest::LogIn { name, .. } => format!("Logging in to {name}…"),
            TickPackageRequest::SaveKey { name, .. } => format!("Saving the key for {name}…"),
            TickPackageRequest::LogOut(name) => format!("Logging out of {name}…"),
        };
        if self.busy.is_some() {
            self.notice = Some(Notice::failure(
                "Wait for it to finish before starting another".to_owned(),
            ));
            return TicksPanelEffect::None;
        }
        self.busy = Some(busy);
        self.notice = None;
        if let TickPackageRequest::LogIn {
            name,
            issuer,
            attempt,
            ..
        } = &mut request
        {
            self.login_attempts += 1;
            *attempt = self.login_attempts;
            self.login = Some(LoginView {
                attempt: *attempt,
                name: name.clone(),
                label: issuer.label.clone(),
                stage: LoginStage::Starting,
            });
        }
        TicksPanelEffect::Work(request)
    }

    fn open_selected(&mut self) -> TicksPanelEffect {
        let index = self.selected[self.page];
        if self.page == MARKETPLACE_PAGE {
            let Some(offer) = self.offers().into_iter().nth(index) else {
                return TicksPanelEffect::None;
            };
            let package = &offer.package;
            if !offer.is_installable {
                self.notice = Some(Notice::failure(format!(
                    "{} {} has no prebuilt program for {CURRENT_TARGET}",
                    package.name, package.version
                )));
                return TicksPanelEffect::None;
            }
            if offer.installed == InstalledVersion::Offered {
                self.notice = Some(Notice::success(format!(
                    "{} {} is already installed",
                    package.name, package.version
                )));
                return TicksPanelEffect::None;
            }
            return self.start(TickPackageRequest::Install(offer.package));
        }
        let Some(menu) = self.actions_menu(index) else {
            return TicksPanelEffect::None;
        };
        self.menus.push(menu);
        TicksPanelEffect::None
    }

    /// The actions of the installed package at `index`.
    fn actions_menu(&self, index: usize) -> Option<PanelMenu> {
        let entries = self.installed_entries();
        let entry = entries.get(index)?;
        let name = entry.name().to_owned();
        let offered = self
            .offers()
            .into_iter()
            .find(|offer| offer.package.name == name && offer.is_installable);
        let mut options = Vec::new();
        let (title, context) = match entry {
            InstalledEntry::Usable { package, login } => {
                let manifest = &package.manifest;
                match &manifest.auth {
                    PackageAuth::None => {}
                    PackageAuth::ApiKey { label, .. } => options.push((
                        "Log in".to_owned(),
                        Choice::EnterKey {
                            name: name.clone(),
                            label: label.clone(),
                        },
                    )),
                    PackageAuth::OAuth2 { issuers, scopes } => {
                        for issuer in issuers {
                            let text = if issuers.len() == 1 {
                                "Log in".to_owned()
                            } else {
                                format!("Log in · {}", issuer.label)
                            };
                            options.push((
                                text,
                                Choice::Run(TickPackageRequest::LogIn {
                                    name: name.clone(),
                                    issuer: issuer.clone(),
                                    scopes: scopes.clone(),
                                    attempt: 0,
                                }),
                            ));
                        }
                    }
                }
                if manifest.auth != PackageAuth::None {
                    options.push((
                        "Log out".to_owned(),
                        Choice::Run(TickPackageRequest::LogOut(name.clone())),
                    ));
                }
                if let Some(offer) =
                    offered.filter(|offer| offer.installed != InstalledVersion::Offered)
                {
                    let offered_version = &offer.package.version;
                    let label = if is_later(offered_version, &package.version) {
                        format!("Update to {offered_version}")
                    } else {
                        format!("Replace with {offered_version}")
                    };
                    options.push((
                        label,
                        Choice::Run(TickPackageRequest::Install(offer.package)),
                    ));
                }
                options.push((
                    if package.enabled { "Disable" } else { "Enable" }.to_owned(),
                    Choice::Run(TickPackageRequest::SetEnabled {
                        name: name.clone(),
                        enabled: !package.enabled,
                    }),
                ));
                (
                    format!("{name} {}", package.version),
                    vec![
                        login_label(*login).to_owned(),
                        safe_line(manifest.description.as_deref().unwrap_or_default()),
                    ],
                )
            }
            InstalledEntry::Unusable(package) => {
                if let Some(offer) = offered {
                    options.push((
                        format!("Install {} again", offer.package.version),
                        Choice::Run(TickPackageRequest::Install(offer.package)),
                    ));
                }
                (
                    format!("{name} {}", package.version),
                    vec![format!("Unusable: {}", safe_line(&package.reason))],
                )
            }
        };
        if options.is_empty() {
            options.push(("Back".into(), Choice::Back));
        }
        Some(PanelMenu {
            title,
            context,
            options,
            selected: 0,
        })
    }

    fn installed_version(&self, name: &str) -> Option<&str> {
        let Listing::Loaded(list) = &self.installed else {
            return None;
        };
        list.packages
            .plugins
            .iter()
            .find(|package| package.manifest.name == name)
            .map(|package| package.version.as_str())
            .or_else(|| {
                list.packages
                    .broken
                    .iter()
                    .find(|package| package.name == name)
                    .map(|package| package.version.as_str())
            })
    }

    fn installed_entries(&self) -> Vec<InstalledEntry<'_>> {
        let Listing::Loaded(list) = &self.installed else {
            return Vec::new();
        };
        let mut entries = list
            .packages
            .plugins
            .iter()
            .zip(list.logins.iter().copied())
            .map(|(package, login)| InstalledEntry::Usable { package, login })
            .chain(list.packages.broken.iter().map(InstalledEntry::Unusable))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.name().cmp(right.name()));
        entries
    }

    fn offers(&self) -> Vec<Offer> {
        let Listing::Loaded(marketplace) = &self.marketplace else {
            return Vec::new();
        };
        let empty = InstalledPlugins::default();
        let installed = match &self.installed {
            Listing::Loaded(list) => &list.packages,
            _ => &empty,
        };
        let query = self.filter.draft().to_lowercase();
        offers(marketplace, installed)
            .into_iter()
            .filter(|offer| {
                self.page != MARKETPLACE_PAGE
                    || format!(
                        "{} {} tickloop",
                        offer.package.name, offer.package.description
                    )
                    .to_lowercase()
                    .contains(&query)
            })
            .collect()
    }

    fn entry_count(&self, page: usize) -> usize {
        if page == INSTALLED_PAGE {
            self.installed_entries().len()
        } else {
            self.offers().len()
        }
    }

    /// One clipped row for each package at `width`.
    fn list_rows(&self, width: usize, display: DisplayProfile) -> Vec<(usize, Vec<RoleLine>)> {
        let glyph = display.glyph(Glyph::ProtocolTick);
        let selected = self.selected[self.page];
        let row = |index: usize, name: &str, details: String, glyph_role: Role| {
            let separate_details = glyph.width() + name.width() + details.width() + 3 > width;
            let mut lines = ListRow {
                selected: index == selected && self.menus.is_empty(),
                glyph,
                glyph_role,
                name,
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
                        .map(|line| RoleLine::role(format!("  {line}"), Role::MutedText)),
                );
            }
            lines
        };
        if self.page == INSTALLED_PAGE {
            return self
                .installed_entries()
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let (details, glyph_role) = match entry {
                        InstalledEntry::Usable { package, login } => (
                            format!(
                                "{} · {} · {} · {}",
                                package.marketplace,
                                if package.enabled {
                                    "enabled"
                                } else {
                                    "disabled"
                                },
                                package.version,
                                login_label(*login)
                            ),
                            match login {
                                LoginState::LoggedIn | LoginState::NotRequired => Role::Success,
                                LoginState::LoggedOut | LoginState::Expired => Role::Warning,
                                LoginState::Unknown => Role::MutedText,
                            },
                        ),
                        InstalledEntry::Unusable(package) => (
                            format!(
                                "{} · {} · unusable; reinstall",
                                package.marketplace, package.version
                            ),
                            Role::Error,
                        ),
                    };
                    let mut lines = row(index, entry.name(), details, glyph_role);
                    if let InstalledEntry::Usable { package, .. } = entry
                        && let Some(warning) = unsupported_summary(&package.manifest)
                    {
                        lines.push(RoleLine::role(format!("  {warning}"), Role::Warning));
                    }
                    (index, lines)
                })
                .collect();
        }
        self.offers()
            .iter()
            .enumerate()
            .map(|(index, offer)| {
                let package = &offer.package;
                let (mut state, glyph_role) = match &offer.installed {
                    InstalledVersion::Offered => ("installed".to_owned(), Role::Success),
                    InstalledVersion::Other(version) => {
                        (format!("installed {version}"), Role::Warning)
                    }
                    InstalledVersion::Unusable(version) => {
                        (format!("installed {version} unusable"), Role::Error)
                    }
                    InstalledVersion::NotInstalled => ("not installed".to_owned(), Role::MutedText),
                };
                if !offer.is_installable {
                    state.push_str(" · not for this platform");
                }
                let name = format!("{} · {}", package.name, safe_line(&package.description));
                (
                    index,
                    row(
                        index,
                        &name,
                        format!("tickloop · {} · {state}", package.version),
                        glyph_role,
                    ),
                )
            })
            .collect()
    }

    /// The status above the hints, wrapped at `width`: the operation under way, and the last
    /// result, which while an operation runs can only be why another did not start.
    fn status_lines(&self, width: usize) -> Vec<RoleLine> {
        let mut lines = Vec::new();
        let mut add = |text: &str, role: Role| {
            lines.extend(
                wrap_text(&safe_line(text), width)
                    .into_iter()
                    .map(|line| RoleLine::role(line, role)),
            );
        };
        if self.page == MARKETPLACE_PAGE
            && self.menus.is_empty()
            && self.login.is_none()
            && (self.filtering || !self.filter.draft().is_empty())
        {
            add(
                &format!(
                    "Filter: {}",
                    if self.filtering {
                        editor_window(&self.filter, width.saturating_sub(8))
                    } else {
                        self.filter.draft().to_owned()
                    }
                ),
                Role::Text,
            );
        }
        if self.uninstall_confirmation.is_some() {
            add(
                "Uninstall removes plugin files and data. Saved login and running Ticks stay.",
                Role::MutedText,
            );
        }
        // A login under way shows itself in place of the list.
        if let Some(busy) = self.busy.as_ref().filter(|_| self.login.is_none()) {
            add(busy, Role::Accent);
        }
        if let Some(notice) = &self.notice {
            let role = if notice.is_failure {
                Role::Error
            } else {
                Role::Success
            };
            add(&notice.text, role);
        }
        lines
    }

    /// The header, one row of the list or menu, and the key hints.
    pub(super) fn minimum_frame_rows(&self, width: usize) -> usize {
        let selected_rows = if self.menus.is_empty() && self.login.is_none() {
            self.list_rows(width, DisplayProfile::default())
                .into_iter()
                .find(|(index, _)| *index == self.selected[self.page])
                .map_or(1, |(_, lines)| lines.len())
        } else {
            1
        };
        CHROME_ROWS + selected_rows + self.status_lines(width).len()
    }

    pub(super) fn content_rows(&self, width: usize) -> usize {
        let body = match (&self.login, self.menus.last()) {
            (Some(login), _) => login_lines(login, width, DisplayProfile::default())
                .lines
                .len(),
            (None, Some(menu)) => 1 + context_lines(menu, width).len() + menu.options.len(),
            (None, None) => match self
                .list_rows(width, DisplayProfile::default())
                .iter()
                .map(|(_, lines)| lines.len())
                .sum::<usize>()
            {
                0 => self.empty_lines(width).len(),
                rows => rows,
            },
        };
        CHROME_ROWS + body + self.status_lines(width).len()
    }

    pub(super) fn frame(
        &mut self,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> ConversationPickerFrame {
        let mut lines = vec![tab_header_line(TITLE, self.page, &PAGES, width)];
        let status = self.status_lines(width);
        let capacity = height.saturating_sub(CHROME_ROWS + status.len());
        let mut body = Vec::new();
        let mut targets = Vec::new();
        let mut cursor = None;
        let (hint, selected_rows) = if let Some(login) = &self.login {
            let shown = login_lines(login, width, display);
            body = shown.lines;
            if shown.has_option {
                targets.push((shown.focus..shown.focus + 1, 0));
            }
            cursor = shown.cursor.map(|column| (shown.focus, column));
            let hint = if shown.has_option {
                "enter cancel login · esc cancel"
            } else {
                "enter save · esc cancel"
            };
            (hint.to_owned(), shown.focus..shown.focus + 1)
        } else if let Some(menu) = self.menus.last() {
            body.push(RoleLine::role(safe_line(&menu.title), Role::Text));
            body.extend(context_lines(menu, width));
            for (index, (label, _)) in menu.options.iter().enumerate() {
                targets.push((body.len()..body.len() + 1, index));
                body.push(if index == menu.selected {
                    RoleLine::role(
                        format!("{} {label}", display.glyph(Glyph::Prompt)),
                        Role::Text,
                    )
                    .filled(Role::Selection)
                } else {
                    RoleLine::role(format!("  {label}"), Role::Text)
                });
            }
            let selected = body.len() - menu.options.len() + menu.selected;
            (
                "enter confirm · esc back".to_owned(),
                selected..selected + 1,
            )
        } else {
            let mut selected_rows = 0..0;
            for (index, rows) in self.list_rows(width, display) {
                let range = body.len()..body.len() + rows.len();
                if index == self.selected[self.page] {
                    selected_rows = range.clone();
                }
                targets.push((range, index));
                body.extend(rows);
            }
            if body.is_empty() {
                body = self.empty_lines(width);
            }
            let enter = if self.page == INSTALLED_PAGE {
                "enter actions"
            } else {
                "enter install/update"
            };
            let hint = if self.filtering && self.page == MARKETPLACE_PAGE {
                fit_hint(
                    &[
                        ("ctrl+n/p select".into(), 8),
                        ("enter install/update".into(), 7),
                        ("ctrl+g editor".into(), 3),
                        ("esc leave filter".into(), 9),
                    ],
                    width,
                    display,
                )
            } else {
                fit_hint(
                    &[
                        (enter.to_owned(), 10),
                        (
                            if self.page == INSTALLED_PAGE {
                                "ctrl+x uninstall"
                            } else {
                                "r refresh"
                            }
                            .to_owned(),
                            5,
                        ),
                        ("tab/←/→ page".to_owned(), 6),
                        (
                            if self.page == MARKETPLACE_PAGE {
                                "/ filter"
                            } else {
                                "r refresh"
                            }
                            .to_owned(),
                            4,
                        ),
                        ("esc close".to_owned(), 9),
                    ],
                    width,
                    display,
                )
            };
            (hint, selected_rows)
        };
        let top = &mut self.top[self.page];
        let shows_list = self.menus.is_empty() && self.login.is_none();
        if shows_list {
            if selected_rows.start < *top {
                *top = selected_rows.start;
            }
            if selected_rows.end > *top + capacity {
                *top = selected_rows.end.saturating_sub(capacity);
            }
            *top = (*top).min(body.len().saturating_sub(capacity));
        }
        let skip = if shows_list {
            *top
        } else {
            selected_rows.end.saturating_sub(capacity)
        };
        self.row_targets = targets
            .into_iter()
            .filter_map(|(rows, index)| {
                let start = rows.start.max(skip);
                let end = rows.end.min(skip + capacity);
                (start < end).then(|| (start - skip + 1..end - skip + 1, index))
            })
            .collect();
        lines.extend(body.into_iter().skip(skip).take(capacity));
        lines.resize(height.saturating_sub(1 + status.len()), RoleLine::default());
        lines.extend(status);
        lines.push(match &self.uninstall_confirmation {
            Some((name, _)) => RoleLine::role(
                format!("Press ctrl+x again to uninstall {name}"),
                Role::Error,
            ),
            None => RoleLine::role(hint, Role::MutedText),
        });
        lines.truncate(height);
        for line in &mut lines {
            line.truncate(width);
        }
        ConversationPickerFrame {
            lines,
            // Below the header, while the field is in view.
            cursor: cursor
                .filter(|(row, _)| (skip..skip + capacity).contains(row))
                .map(|(row, column)| (row - skip + 1, column)),
        }
    }

    /// What the page shows without packages to list, wrapped at `width`.
    fn empty_lines(&self, width: usize) -> Vec<RoleLine> {
        let (text, role) = match (self.page, &self.installed, &self.marketplace) {
            (INSTALLED_PAGE, Listing::Loading, _) => ("Loading…".to_owned(), Role::MutedText),
            (INSTALLED_PAGE, Listing::Failed(error), _) => (safe_line(error), Role::Error),
            (INSTALLED_PAGE, Listing::Loaded(_), _) => (
                "No plugins are installed; the Available page offers them.".to_owned(),
                Role::MutedText,
            ),
            (_, _, Listing::Loading) => {
                ("Reading the Tick marketplace…".to_owned(), Role::MutedText)
            }
            (_, _, Listing::Failed(error)) => (safe_line(error), Role::Error),
            (_, _, Listing::Loaded(_)) => (
                if self.filter.draft().is_empty() {
                    "No plugins available".to_owned()
                } else {
                    format!("No plugins match \"{}\"", self.filter.draft())
                },
                Role::MutedText,
            ),
        };
        wrap_text(&text, width.saturating_sub(EMPTY_INDENT.len()).max(1))
            .into_iter()
            .map(|line| RoleLine::role(format!("{EMPTY_INDENT}{line}"), role))
            .collect()
    }
}

fn unsupported_summary(manifest: &PluginManifest) -> Option<String> {
    let unsupported = manifest.unsupported();
    (!unsupported.is_empty()).then(|| {
        format!(
            "Not supported yet: {}",
            unsupported
                .iter()
                .map(|component| component.label())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Whether `offered` is a later version than `installed`, both read as dot-separated numbers; a
/// version that is not one is never later, so replacing it is not called an update.
fn is_later(offered: &str, installed: &str) -> bool {
    let numbers = |version: &str| {
        version
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    };
    matches!((numbers(offered), numbers(installed)), (Some(offered), Some(installed)) if offered > installed)
}

/// A login view's rows, with the row of its option or field.
struct LoginLines {
    lines: Vec<RoleLine>,
    focus: usize,
    /// Whether the focused row is the option that cancels the login.
    has_option: bool,
    /// The column after the masked key on the focused row, for the text cursor.
    cursor: Option<usize>,
}

/// The rows of `login` at `width`, worded as the Console's other device logins.
fn login_lines(login: &LoginView, width: usize, display: DisplayProfile) -> LoginLines {
    let LoginView {
        name, label, stage, ..
    } = login;
    let title = match stage {
        LoginStage::Key { .. } => format!("Log in to {name}"),
        _ => format!("Log in to {name} · {label}"),
    };
    let mut lines = vec![RoleLine::role(safe_line(&title), Role::Text)];
    let context_width = width.saturating_sub(CONTEXT_INDENT.len()).max(1);
    let context = |lines: &mut Vec<RoleLine>, text: &str| {
        lines.extend(
            wrap_text(&safe_line(text), context_width)
                .into_iter()
                .map(|line| RoleLine::role(format!("{CONTEXT_INDENT}{line}"), Role::MutedText)),
        );
    };
    match stage {
        LoginStage::Starting => {
            context(&mut lines, &format!("Starting the {label} authorization…"));
        }
        LoginStage::Waiting { link, code, opened } => {
            context(
                &mut lines,
                if *opened {
                    "Opening your browser. If no page appears, open this link on your own device:"
                } else {
                    "Open this link on your own device:"
                },
            );
            // Every row of a wrapped link opens the whole link.
            lines.extend(wrap_text(link, context_width).into_iter().map(|piece| {
                RoleLine::linked(format!("{CONTEXT_INDENT}{piece}"), Role::Accent, link)
            }));
            context(
                &mut lines,
                "Use your terminal's link shortcut (often ctrl/cmd+click), or copy the URL.",
            );
            context(&mut lines, &format!("Enter code: {code}"));
            context(
                &mut lines,
                &format!("Waiting for the authorization of {name}. esc cancels."),
            );
        }
        LoginStage::Key { entered } => {
            context(
                &mut lines,
                &format!("Enter the {label}. Only {name}'s programs get it; it is never shown."),
            );
            // Wrapped like the Console's other secret fields, with the cursor after the last `*`.
            let field = format!(
                "{CONTEXT_INDENT}{}: {}",
                safe_line(label),
                "*".repeat(entered.chars().count())
            );
            let rows = wrap_text(&field, width.max(1));
            let column = rows.last().map_or(0, |row| row.width());
            lines.extend(rows.into_iter().map(|row| RoleLine::role(row, Role::Text)));
            return LoginLines {
                focus: lines.len() - 1,
                lines,
                has_option: false,
                cursor: Some(column.min(width.saturating_sub(1))),
            };
        }
    }
    lines.push(
        RoleLine::role(
            format!("{} Cancel login", display.glyph(Glyph::Prompt)),
            Role::Text,
        )
        .filled(Role::Selection),
    );
    LoginLines {
        focus: lines.len() - 1,
        lines,
        has_option: true,
        cursor: None,
    }
}

/// The menu's context, indented and wrapped at `width`.
fn context_lines(menu: &PanelMenu, width: usize) -> Vec<RoleLine> {
    menu.context
        .iter()
        .flat_map(|line| wrap_text(line, width.saturating_sub(CONTEXT_INDENT.len()).max(1)))
        .map(|line| RoleLine::role(format!("{CONTEXT_INDENT}{line}"), Role::MutedText))
        .collect()
}

fn step(selected: usize, count: usize, down: bool) -> usize {
    if down {
        (selected + 1).min(count.saturating_sub(1))
    } else {
        selected.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tick_plugins::{Component, ManifestFormat, PluginSource, TickProgram};

    use super::*;

    const WIDTH: usize = 90;
    const HEIGHT: usize = 14;

    fn installed(name: &str, version: &str, declares_login: bool) -> InstalledPlugin {
        let auth = if declares_login {
            PackageAuth::OAuth2 {
                issuers: vec![server("Server")],
                scopes: Vec::new(),
            }
        } else {
            PackageAuth::None
        };
        InstalledPlugin {
            manifest: PluginManifest {
                name: name.into(),
                display_name: None,
                version: Some(version.into()),
                description: Some(format!("The {name} package")),
                format: ManifestFormat::TickPackage,
                tick: Some(TickProgram {
                    executable: PathBuf::from(format!("bin/tick-{name}")),
                    protocol: true,
                }),
                skill_paths: Vec::new(),
                components: [Component::Tick].into_iter().collect(),
                warnings: Vec::new(),
                auth,
            },
            root: PathBuf::from("/packages").join(name),
            source: PluginSource::Download {
                url: "https://example.test/package.tar.gz".into(),
            },
            version: version.into(),
            revision: version.into(),
            marketplace: "tickloop".into(),
            enabled: true,
            sha256: None,
            installed_at: "2026-10-15T00:00:00Z".into(),
        }
    }

    /// The installed packages as the read numbered `read` found them, each with its login state.
    fn installed_list(
        read: u64,
        packages: Vec<(InstalledPlugin, LoginState)>,
    ) -> TickPackageResult {
        let (packages, logins) = packages.into_iter().unzip();
        TickPackageResult::Installed {
            read,
            listing: Ok(InstalledList {
                packages: InstalledPlugins {
                    plugins: packages,
                    broken: Vec::new(),
                },
                logins,
            }),
        }
    }

    /// The marketplace offering each `(name, version, installable)`.
    fn marketplace(offered: &[(&str, &str, bool)]) -> Marketplace {
        let packages = offered
            .iter()
            .map(|(name, version, installable)| {
                let target = if *installable {
                    CURRENT_TARGET
                } else {
                    "x86_64-unknown-linux-gnu"
                };
                serde_json::json!({
                    "name": name,
                    "version": version,
                    "description": format!("The {name} package"),
                    "downloads": { target: {
                        "url": format!("https://example.com/tick-{name}-{version}.tar.gz"),
                        "sha256": "0".repeat(64),
                    }},
                })
            })
            .collect::<Vec<_>>();
        let marketplace = serde_json::json!({
            "name": "tickloop",
            "owner": { "name": "Tickloop AI" },
            "packages": packages,
        });
        Marketplace::parse(marketplace.to_string().as_bytes()).unwrap()
    }

    /// A panel listing alpha 1.0.0, which logs in, and beta 1.0.0, which needs no login, beside a
    /// marketplace offering alpha 2.0.0 and sample 1.0.0.
    fn loaded_panel() -> TicksPanel {
        let mut panel = TicksPanel::new(None);
        panel.apply(installed_list(
            1,
            vec![
                (installed("alpha", "1.0.0", true), LoginState::LoggedOut),
                (installed("beta", "1.0.0", false), LoginState::NotRequired),
            ],
        ));
        panel.apply(TickPackageResult::Marketplace(Ok(marketplace(&[
            ("alpha", "2.0.0", true),
            ("sample", "1.0.0", true),
        ]))));
        panel
    }

    fn press(panel: &mut TicksPanel, code: KeyCode) -> TicksPanelEffect {
        panel.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn control(panel: &mut TicksPanel, character: char) -> TicksPanelEffect {
        panel.key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::CONTROL,
        ))
    }

    fn frame(panel: &mut TicksPanel) -> Vec<RoleLine> {
        panel.frame(WIDTH, HEIGHT, DisplayProfile::default()).lines
    }

    fn text(panel: &mut TicksPanel) -> String {
        frame(panel)
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The text of the rows drawn with the selection fill.
    fn selected(panel: &mut TicksPanel) -> Vec<String> {
        frame(panel)
            .iter()
            .filter(|line| line.fill == Some(Role::Selection))
            .map(|line| line.text().trim().to_owned())
            .collect()
    }

    fn work(effect: TicksPanelEffect) -> TickPackageRequest {
        match effect {
            TicksPanelEffect::Work(request) => request,
            TicksPanelEffect::None => panic!("no work started"),
            TicksPanelEffect::Close => panic!("the panel closed"),
            TicksPanelEffect::EditFilter => panic!("the external editor opened"),
        }
    }

    #[test]
    fn pages_switch_and_the_list_marks_its_selection_by_its_fill_alone() {
        let mut panel = loaded_panel();
        let shown = text(&mut panel);
        assert!(
            shown.starts_with("Plugins · [Installed]  Available"),
            "{shown}"
        );
        assert!(
            shown.contains("alpha") && shown.contains("1.0.0 · needs login"),
            "{shown}"
        );
        assert!(
            shown.contains("beta") && shown.contains("1.0.0 · no login needed"),
            "{shown}"
        );
        assert!(
            shown.contains("enter actions") && shown.contains("esc close"),
            "{shown}"
        );
        assert!(!shown.contains('❯'), "lists have no cursor:\n{shown}");
        assert!(
            !shown
                .lines()
                .any(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit())),
            "rows have no numbers:\n{shown}"
        );
        let control_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        let control_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for (key, expected) in [
            (None, "alpha"),
            (Some(plain(KeyCode::Char('j'))), "beta"),
            (Some(plain(KeyCode::Char('k'))), "alpha"),
            (Some(control_n), "beta"),
            (Some(control_p), "alpha"),
            (Some(plain(KeyCode::Down)), "beta"),
            (Some(plain(KeyCode::Down)), "beta"),
            (Some(plain(KeyCode::Up)), "alpha"),
        ] {
            if let Some(key) = key {
                assert!(matches!(panel.key(key), TicksPanelEffect::None));
            }
            let rows = selected(&mut panel);
            assert!(
                rows.len() == 1 && rows[0].starts_with(&format!("◉ {expected}")),
                "{rows:?}"
            );
        }
        for (code, page) in [
            (KeyCode::Tab, "[Available]"),
            (KeyCode::BackTab, "[Installed]"),
            (KeyCode::Right, "[Available]"),
            (KeyCode::Left, "[Installed]"),
        ] {
            press(&mut panel, code);
            assert!(text(&mut panel).lines().next().unwrap().contains(page));
        }
        press(&mut panel, KeyCode::Tab);
        let shown = text(&mut panel);
        assert!(
            shown.contains("alpha · The alpha package")
                && shown.contains("2.0.0 · installed 1.0.0"),
            "{shown}"
        );
        assert!(
            shown.contains("sample · The sample package")
                && shown.contains("1.0.0 · not installed"),
            "{shown}"
        );
        assert!(shown.contains("enter install/update"), "{shown}");
    }

    #[test]
    fn enter_opens_plugin_actions_and_escape_returns_to_the_list() {
        let mut panel = loaded_panel();
        press(&mut panel, KeyCode::Enter);
        let shown = text(&mut panel);
        for label in ["❯ Log in", "  Log out", "  Update to 2.0.0", "  Disable"] {
            assert!(shown.contains(label), "{shown}");
        }
        assert!(shown.contains("enter confirm · esc back"));
        press(&mut panel, KeyCode::Esc);
        assert!(!text(&mut panel).contains('❯'));
        assert!(matches!(
            press(&mut panel, KeyCode::Esc),
            TicksPanelEffect::Close
        ));
    }

    #[test]
    fn a_plugin_without_login_offers_no_login_and_uninstall_retains_credentials() {
        let mut panel = loaded_panel();
        press(&mut panel, KeyCode::Char('j'));
        press(&mut panel, KeyCode::Enter);
        let shown = text(&mut panel);
        assert!(
            shown.contains("beta 1.0.0") && shown.contains("  no login needed"),
            "{shown}"
        );
        assert!(
            !shown.contains("Log in") && !shown.contains("Update"),
            "{shown}"
        );
        press(&mut panel, KeyCode::Esc);
        control(&mut panel, 'x');
        assert!(text(&mut panel).contains("Press ctrl+x again to uninstall beta"));
        assert_eq!(
            work(control(&mut panel, 'x')),
            TickPackageRequest::Uninstall("beta".into())
        );
        let shown = text(&mut panel);
        assert!(
            shown.contains("Uninstalling beta…") && !shown.contains('❯'),
            "{shown}"
        );
        panel.apply(TickPackageResult::Uninstallation {
            name: "beta".into(),
            result: Ok(Uninstalled {
                name: "beta".into(),
                version: "1.0.0".into(),
                credentials: Some("/config/credentials/beta".into()),
                marketplace: "tickloop".into(),
                data: None,
            }),
        });
        let notice = frame(&mut panel)
            .iter()
            .filter(|line| {
                !line.spans.is_empty()
                    && line
                        .spans
                        .iter()
                        .all(|span| span.role == Some(Role::Success))
            })
            .map(RoleLine::text)
            .collect::<String>();
        assert_eq!(
            notice,
            "Uninstalled beta 1.0.0; its saved login stays in /config/credentials/beta; its running Ticks keep running until closed"
        );
    }

    fn server(label: &str) -> OAuthIssuer {
        OAuthIssuer {
            label: label.into(),
            issuer: format!(
                "https://{}.example.com",
                label.to_lowercase().replace(' ', "-")
            ),
        }
    }

    fn with_auth(mut package: InstalledPlugin, auth: PackageAuth) -> InstalledPlugin {
        package.manifest.auth = auth;
        package
    }

    const LINK: &str = "https://auth.example.com/oauth2/device/verify?user_code=WDJB-MJHT&client_name=a-rather-long-client-name";

    #[test]
    fn an_oauth_login_shows_its_link_and_code_in_the_panel_until_it_ends() {
        let mut panel = loaded_panel();
        press(&mut panel, KeyCode::Enter);
        assert_eq!(
            work(press(&mut panel, KeyCode::Enter)),
            TickPackageRequest::LogIn {
                name: "alpha".into(),
                issuer: server("Server"),
                scopes: Vec::new(),
                attempt: 1,
            }
        );
        let shown = text(&mut panel);
        assert!(
            shown.contains("Log in to alpha · Server")
                && shown.contains("Starting the Server authorization…")
                && shown.contains("❯ Cancel login")
                && shown.contains("enter cancel login · esc cancel"),
            "{shown}"
        );
        assert!(!shown.contains("Logging in to alpha…"), "{shown}");

        panel.apply(TickPackageResult::LoginWaiting {
            attempt: 1,
            link: LINK.into(),
            code: "WDJB-MJHT".into(),
            opened: true,
        });
        let lines = frame(&mut panel);
        let shown = lines
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            shown.contains(
                "Opening your browser. If no page appears, open this link on your own device:"
            ) && shown.contains("Enter code: WDJB-MJHT")
                && shown.contains("Waiting for the authorization of alpha. esc cancels."),
            "{shown}"
        );
        let linked = lines
            .iter()
            .filter(|line| line.link.as_deref() == Some(LINK))
            .map(|line| line.text().trim().to_owned())
            .collect::<Vec<_>>();
        assert!(linked.len() > 1, "the long link wraps:\n{shown}");
        assert_eq!(
            linked.concat(),
            LINK,
            "every row of the link opens all of it"
        );

        // Another operation waits for the login.
        press(&mut panel, KeyCode::Tab);
        assert!(matches!(
            press(&mut panel, KeyCode::Tab),
            TicksPanelEffect::None
        ));
        assert!(text(&mut panel).contains("Log in to alpha · Server"));

        panel.apply(TickPackageResult::LoginEnded {
            attempt: 1,
            name: "alpha".into(),
            result: Ok(()),
        });
        let shown = text(&mut panel);
        assert!(
            shown.contains("Logged in to alpha") && shown.contains("◉ alpha"),
            "{shown}"
        );
        assert!(!shown.contains("Cancel login"), "{shown}");

        press(&mut panel, KeyCode::Enter);
        work(press(&mut panel, KeyCode::Enter));
        panel.apply(TickPackageResult::LoginEnded {
            attempt: 2,
            name: "alpha".into(),
            result: Err(LoginError::Denied),
        });
        assert!(text(&mut panel).contains("Cannot log in to alpha: the authorization was denied"));
        press(&mut panel, KeyCode::Enter);
        work(press(&mut panel, KeyCode::Enter));
        panel.apply(TickPackageResult::LoginEnded {
            attempt: 3,
            name: "alpha".into(),
            result: Err(LoginError::Expired),
        });
        assert!(text(&mut panel).contains(
            "Cannot log in to alpha: the code expired before the authorization was approved"
        ));
    }

    #[test]
    fn esc_cancels_a_login_and_what_it_still_reports_is_ignored() {
        let mut panel = loaded_panel();
        press(&mut panel, KeyCode::Enter);
        work(press(&mut panel, KeyCode::Enter));
        panel.apply(TickPackageResult::LoginWaiting {
            attempt: 1,
            link: "http://127.0.0.1:9/verify".into(),
            code: "CODE".into(),
            opened: false,
        });
        let shown = text(&mut panel);
        assert!(
            shown.contains("Open this link on your own device:") && !shown.contains("Opening"),
            "{shown}"
        );
        for key in ['j', 'k'] {
            press(&mut panel, KeyCode::Char(key));
        }
        assert!(text(&mut panel).contains("Enter code: CODE"));
        assert_eq!(
            work(press(&mut panel, KeyCode::Esc)),
            TickPackageRequest::CancelLogIn(1)
        );
        let shown = text(&mut panel);
        assert!(
            shown.contains("Cancelled the login to alpha") && shown.contains("◉ alpha"),
            "{shown}"
        );
        panel.apply(TickPackageResult::LoginEnded {
            attempt: 1,
            name: "alpha".into(),
            result: Err(LoginError::Cancelled),
        });
        panel.apply(TickPackageResult::LoginWaiting {
            attempt: 1,
            link: "http://127.0.0.1:9/verify".into(),
            code: "LATE".into(),
            opened: false,
        });
        let shown = text(&mut panel);
        assert!(
            shown.contains("Cancelled the login to alpha") && !shown.contains("LATE"),
            "{shown}"
        );
        // A later login ignores what the cancelled one still reports; enter on Cancel login
        // cancels too, and a new operation may start at once.
        press(&mut panel, KeyCode::Enter);
        work(press(&mut panel, KeyCode::Enter));
        panel.apply(TickPackageResult::LoginEnded {
            attempt: 1,
            name: "alpha".into(),
            result: Ok(()),
        });
        let shown = text(&mut panel);
        assert!(
            shown.contains("Starting the Server authorization…") && !shown.contains("Logged in"),
            "{shown}"
        );
        assert_eq!(
            work(press(&mut panel, KeyCode::Enter)),
            TickPackageRequest::CancelLogIn(2)
        );
        press(&mut panel, KeyCode::Enter);
        control(&mut panel, 'n');
        assert_eq!(
            work(press(&mut panel, KeyCode::Enter)),
            TickPackageRequest::LogOut("alpha".into())
        );
    }

    #[test]
    fn a_package_with_several_issuers_offers_a_login_at_each() {
        let mut panel = TicksPanel::new(None);
        let issuers = vec![server("China mainland"), server("Global")];
        panel.apply(installed_list(
            1,
            vec![(
                with_auth(
                    installed("longbridge", "0.2.0", true),
                    PackageAuth::OAuth2 {
                        issuers: issuers.clone(),
                        scopes: vec!["4".into()],
                    },
                ),
                LoginState::LoggedOut,
            )],
        ));
        press(&mut panel, KeyCode::Enter);
        let shown = text(&mut panel);
        assert!(
            shown.contains("❯ Log in · China mainland")
                && shown.contains("  Log in · Global")
                && shown.contains("  Log out"),
            "{shown}"
        );
        control(&mut panel, 'n');
        assert_eq!(
            work(press(&mut panel, KeyCode::Enter)),
            TickPackageRequest::LogIn {
                name: "longbridge".into(),
                issuer: issuers[1].clone(),
                scopes: vec!["4".into()],
                attempt: 1,
            }
        );
        assert!(text(&mut panel).contains("Log in to longbridge · Global"));
    }

    #[test]
    fn a_login_result_the_panel_no_longer_shows_is_reported_ignored() {
        let mut panel = loaded_panel();
        press(&mut panel, KeyCode::Enter);
        work(press(&mut panel, KeyCode::Enter));
        let waiting = |attempt| TickPackageResult::LoginWaiting {
            attempt,
            link: LINK.into(),
            code: "CODE".into(),
            opened: true,
        };
        assert_eq!(panel.apply(waiting(1)), Applied::Taken);
        work(press(&mut panel, KeyCode::Esc));
        // Cancelled before its device code arrived: the Console then ends it without opening
        // the browser or polling.
        assert_eq!(panel.apply(waiting(1)), Applied::Ignored);
        press(&mut panel, KeyCode::Enter);
        work(press(&mut panel, KeyCode::Enter));
        assert_eq!(panel.apply(waiting(1)), Applied::Ignored);
        assert_eq!(panel.apply(waiting(2)), Applied::Taken);
    }

    #[test]
    fn a_long_key_wraps_with_the_cursor_after_its_last_star() {
        let mut panel = TicksPanel::new(None);
        panel.apply(installed_list(
            1,
            vec![(
                with_auth(
                    installed("keyed", "1.0.0", false),
                    PackageAuth::ApiKey {
                        env: "KEYED_KEY".into(),
                        label: "Keyed API key".into(),
                    },
                ),
                LoginState::LoggedOut,
            )],
        ));
        press(&mut panel, KeyCode::Enter);
        press(&mut panel, KeyCode::Enter);
        panel.paste(&"k".repeat(70));
        let width = 40;
        let rendered = panel.frame(width, 20, DisplayProfile::default());
        let rows = rendered
            .lines
            .iter()
            .map(RoleLine::text)
            .filter(|row| row.contains('*'))
            .collect::<Vec<_>>();
        assert_eq!(
            rows.concat().matches('*').count(),
            70,
            "every character shows: {rows:?}"
        );
        assert!(
            rows.len() > 1 && rows.iter().all(|row| row.width() <= width),
            "{rows:?}"
        );
        let (row, column) = rendered.cursor.unwrap();
        assert_eq!(rendered.lines[row].text(), *rows.last().unwrap());
        assert_eq!(column, rows.last().unwrap().width().min(width - 1));
    }

    #[test]
    fn a_key_is_entered_masked_and_saved() {
        let mut panel = TicksPanel::new(None);
        panel.apply(installed_list(
            1,
            vec![(
                with_auth(
                    installed("keyed", "1.0.0", false),
                    PackageAuth::ApiKey {
                        env: "KEYED_KEY".into(),
                        label: "Keyed API key".into(),
                    },
                ),
                LoginState::LoggedOut,
            )],
        ));
        press(&mut panel, KeyCode::Enter);
        assert!(text(&mut panel).contains("❯ Log in"));
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            TicksPanelEffect::None
        ));
        let shown = text(&mut panel);
        assert!(
            shown.contains("Log in to keyed")
                && shown.contains("Enter the Keyed API key.")
                && shown.contains("enter save · esc cancel"),
            "{shown}"
        );
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            TicksPanelEffect::None
        ));
        assert!(text(&mut panel).contains("a key holds 1 to 4096 bytes"));
        for key in "jk-old".chars() {
            press(&mut panel, KeyCode::Char(key));
        }
        control(&mut panel, 'u');
        for key in "jk-secrets".chars() {
            press(&mut panel, KeyCode::Char(key));
        }
        press(&mut panel, KeyCode::Backspace);
        panel.paste("-2\n");
        let rendered = panel.frame(WIDTH, HEIGHT, DisplayProfile::default());
        let shown = rendered
            .lines
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            shown.contains("Keyed API key: ***********") && !shown.contains("secret"),
            "{shown}"
        );
        let (row, column) = rendered.cursor.unwrap();
        assert!(rendered.lines[row].text().contains("Keyed API key: "));
        assert_eq!(column, "  Keyed API key: ".len() + 11);
        let TickPackageRequest::SaveKey { name, key } = work(press(&mut panel, KeyCode::Enter))
        else {
            panic!("enter does not save the key");
        };
        assert_eq!((name.as_str(), key.expose()), ("keyed", "jk-secret-2"));
        assert!(text(&mut panel).contains("Saving the key for keyed…"));
        panel.apply(TickPackageResult::KeySaved {
            name: "keyed".into(),
            result: Ok(()),
        });
        assert!(text(&mut panel).contains("Saved the key for keyed"));

        press(&mut panel, KeyCode::Enter);
        press(&mut panel, KeyCode::Enter);
        press(&mut panel, KeyCode::Char('x'));
        assert!(matches!(
            press(&mut panel, KeyCode::Esc),
            TicksPanelEffect::None
        ));
        let shown = text(&mut panel);
        assert!(
            shown.contains("◉ keyed") && !shown.contains("Enter the Keyed API key."),
            "esc leaves the key entry without saving:\n{shown}"
        );
    }

    #[test]
    fn one_operation_runs_at_a_time_and_its_result_refreshes_the_row() {
        let mut panel = loaded_panel();
        press(&mut panel, KeyCode::Enter);
        control(&mut panel, 'n');
        assert_eq!(
            work(press(&mut panel, KeyCode::Enter)),
            TickPackageRequest::LogOut("alpha".into())
        );
        assert!(text(&mut panel).contains("Logging out of alpha…"));
        press(&mut panel, KeyCode::Tab);
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            TicksPanelEffect::None
        ));
        let shown = text(&mut panel);
        assert!(shown.contains("Logging out of alpha…"), "{shown}");
        assert!(
            shown.contains("Wait for it to finish before starting another"),
            "{shown}"
        );
        panel.apply(TickPackageResult::Logout {
            name: "alpha".into(),
            result: Ok(LoggedOut {
                had_credentials: true,
                revocation_failure: None,
            }),
        });
        panel.apply(TickPackageResult::Login {
            name: "alpha".into(),
            state: LoginState::LoggedOut,
        });
        press(&mut panel, KeyCode::Tab);
        let shown = text(&mut panel);
        assert!(shown.contains("Logged out of alpha"), "{shown}");
        assert!(shown.contains("1.0.0 · needs login"), "{shown}");
        panel.apply(TickPackageResult::Login {
            name: "alpha".into(),
            state: LoginState::LoggedIn,
        });
        assert!(text(&mut panel).contains("1.0.0 · logged in"));
        panel.apply(TickPackageResult::Logout {
            name: "alpha".into(),
            result: Ok(LoggedOut {
                had_credentials: true,
                revocation_failure: Some(
                    "https://auth.example.com/revoke answered HTTP 503".into(),
                ),
            }),
        });
        let shown = text(&mut panel);
        assert!(
            shown
                .contains("Logged out of alpha; the authorization server did not revoke the token")
                && shown.contains("answered HTTP 503"),
            "{shown}"
        );
        panel.apply(TickPackageResult::Logout {
            name: "alpha".into(),
            result: Ok(LoggedOut {
                had_credentials: false,
                revocation_failure: None,
            }),
        });
        assert!(text(&mut panel).contains("alpha was not logged in"));
    }

    #[test]
    fn the_marketplace_installs_and_updates_and_says_a_new_chat_loads_the_skill() {
        let mut panel = loaded_panel();
        press(&mut panel, KeyCode::Tab);
        let TickPackageRequest::Install(update) = work(press(&mut panel, KeyCode::Enter)) else {
            panic!("enter does not update alpha");
        };
        assert_eq!(
            (update.name.as_str(), update.version.as_str()),
            ("alpha", "2.0.0")
        );
        assert!(text(&mut panel).contains("Updating alpha to 2.0.0…"));
        let mut updated = installed("alpha", "2.0.0", true);
        updated.source = PluginSource::Download {
            url: "https://example.com/tick-alpha-2.0.0.tar.gz".into(),
        };
        panel.apply(TickPackageResult::Installation {
            name: "alpha".into(),
            result: Ok(Box::new(Installation {
                plugin: updated,
                outcome: InstallOutcome::Replaced {
                    previous_version: "1.0.0".into(),
                },
            })),
        });
        let shown = text(&mut panel);
        assert!(shown.contains("Updated alpha 1.0.0 to 2.0.0"), "{shown}");
        assert!(
            !shown.contains("new Chat"),
            "the Skill reaches this Chat at once: {shown}"
        );
        press(&mut panel, KeyCode::Char('j'));
        let TickPackageRequest::Install(new) = work(press(&mut panel, KeyCode::Enter)) else {
            panic!("enter does not install sample");
        };
        assert_eq!(new.name, "sample");
        assert!(text(&mut panel).contains("Installing sample 1.0.0…"));
        panel.apply(TickPackageResult::Installation {
            name: "sample".into(),
            result: Err("cannot download sample 1.0.0 from https://example.com: timed out".into()),
        });
        assert!(text(&mut panel).contains(
            "Installing sample failed: cannot download sample 1.0.0 from https://example.com: timed out"
        ));
    }

    #[test]
    fn the_marketplace_explains_what_cannot_be_installed() {
        let mut panel = TicksPanel::new(None);
        panel.apply(installed_list(
            1,
            vec![(installed("alpha", "1.0.0", true), LoginState::LoggedIn)],
        ));
        panel.apply(TickPackageResult::Marketplace(Ok(marketplace(&[
            ("alpha", "1.0.0", true),
            ("other", "1.0.0", false),
        ]))));
        press(&mut panel, KeyCode::Tab);
        assert!(text(&mut panel).contains("1.0.0 · not installed · not for this platform"));
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            TicksPanelEffect::None
        ));
        assert!(text(&mut panel).contains("alpha 1.0.0 is already installed"));
        press(&mut panel, KeyCode::Char('j'));
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            TicksPanelEffect::None
        ));
        assert!(text(&mut panel).contains(&format!(
            "other 1.0.0 has no prebuilt program for {CURRENT_TARGET}"
        )));
    }

    #[test]
    fn an_unavailable_marketplace_leaves_the_installed_page_as_it_is() {
        let mut panel = TicksPanel::new(None);
        assert!(text(&mut panel).contains("Loading…"));
        panel.apply(TickPackageResult::Marketplace(Err(
            "cannot read the Tick marketplace at /missing.json: No such file or directory".into(),
        )));
        panel.apply(installed_list(
            1,
            vec![(installed("alpha", "1.0.0", true), LoginState::Expired)],
        ));
        let shown = text(&mut panel);
        assert!(
            shown.contains("1.0.0 · login expired") && !shown.contains("missing.json"),
            "{shown}"
        );
        press(&mut panel, KeyCode::Enter);
        assert!(
            !text(&mut panel).contains("Update"),
            "nothing to update from"
        );
        press(&mut panel, KeyCode::Esc);
        press(&mut panel, KeyCode::Tab);
        assert!(text(&mut panel).contains(
            "cannot read the Tick marketplace at /missing.json: No such file or directory"
        ));
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            TicksPanelEffect::None
        ));
        assert_eq!(
            work(press(&mut panel, KeyCode::Char('r'))),
            TickPackageRequest::Load
        );
        assert!(text(&mut panel).contains("Reading the Tick marketplace…"));
    }

    #[test]
    fn a_read_that_started_before_the_shown_one_does_not_bring_back_what_it_listed() {
        let mut panel = loaded_panel();
        let before_uninstall = || {
            installed_list(
                2,
                vec![
                    (installed("alpha", "1.0.0", true), LoginState::LoggedOut),
                    (installed("beta", "1.0.0", false), LoginState::NotRequired),
                ],
            )
        };
        let after_uninstall = installed_list(
            3,
            vec![(installed("alpha", "1.0.0", true), LoginState::LoggedOut)],
        );
        panel.apply(after_uninstall);
        panel.apply(before_uninstall());
        assert!(!text(&mut panel).contains("◉ beta"), "{}", text(&mut panel));

        let mut panel = loaded_panel();
        panel.apply(before_uninstall());
        panel.apply(installed_list(
            3,
            vec![(installed("alpha", "1.0.0", true), LoginState::LoggedOut)],
        ));
        assert!(!text(&mut panel).contains("◉ beta"), "{}", text(&mut panel));
    }

    #[test]
    fn a_marketplace_version_older_than_the_installed_one_replaces_it_rather_than_updating() {
        let mut panel = TicksPanel::new(None);
        panel.apply(installed_list(
            1,
            vec![(installed("alpha", "0.3.0", true), LoginState::LoggedIn)],
        ));
        panel.apply(TickPackageResult::Marketplace(Ok(marketplace(&[(
            "alpha", "0.2.0", true,
        )]))));
        press(&mut panel, KeyCode::Enter);
        let shown = text(&mut panel);
        assert!(
            shown.contains("  Replace with 0.2.0") && !shown.contains("Update to"),
            "{shown}"
        );
        for _ in 0..2 {
            control(&mut panel, 'n');
        }
        assert!(matches!(
            work(press(&mut panel, KeyCode::Enter)),
            TickPackageRequest::Install(package) if package.version == "0.2.0"
        ));
        assert!(text(&mut panel).contains("Replacing alpha with 0.2.0…"));
        panel.apply(TickPackageResult::Installation {
            name: "alpha".into(),
            result: Ok(Box::new(Installation {
                plugin: installed("alpha", "0.2.0", true),
                outcome: InstallOutcome::Replaced {
                    previous_version: "0.3.0".into(),
                },
            })),
        });
        assert!(text(&mut panel).contains("Replaced alpha 0.3.0 with 0.2.0"));
        assert!(is_later("0.10.0", "0.9.1") && !is_later("0.2.0", "0.3.0"));
        assert!(!is_later("1.0.0-beta", "0.9.0") && !is_later("1.0.0", "1.0.0"));
    }
}
