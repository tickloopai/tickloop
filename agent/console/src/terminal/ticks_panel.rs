//! `/ticks`: the Tick packages panel of the input dialog. The Installed page lists the installed
//! packages with their versions and login states, and enter opens a package's actions there; the
//! Marketplace page lists what the Tick marketplace offers, and enter installs or updates it. The
//! panel keeps what it shows and what the user chose; the Console runs the work and hands each
//! result back through [`TicksPanel::apply`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tick_packages::{
    BrokenPackage, CURRENT_TARGET, InstallOutcome, Installation, InstalledPackage,
    InstalledPackages, LoginState, Marketplace, MarketplacePackage, PackageAuth, Uninstalled,
};
use tickloop_agent_packages::{InstalledVersion, LoggedOut, Offer, login_label, offers};
use uuid::Uuid;

use super::{
    console::wrap_text,
    conversation_picker::{ConversationPickerFrame, fit_hint},
    display::{DisplayProfile, Glyph},
    list_row::ListRow,
    permissions_surface::tab_header_line,
    safe_line,
    style::{Role, RoleLine},
};

const TITLE: &str = "Tick packages";
const PAGES: [&str; 2] = ["Installed", "Marketplace"];
const INSTALLED_PAGE: usize = 0;
const MARKETPLACE_PAGE: usize = 1;
/// The header and the key hints.
const CHROME_ROWS: usize = 2;
const SKILL_LOADS_IN_A_NEW_CHAT: &str = "its Skill loads in a new Chat (/clear)";
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
    pub(super) packages: InstalledPackages,
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
    Logout {
        name: String,
        result: Result<LoggedOut, String>,
    },
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
            Self::Installed { .. } | Self::Marketplace(_) | Self::Login { .. } => return None,
            Self::Installation {
                name,
                result: Err(error),
            } => Notice::failure(format!("Installing {name} failed: {error}")),
            Self::Installation {
                result: Ok(installation),
                ..
            } => {
                let manifest = &installation.package.manifest;
                let (name, version) = (&manifest.name, &manifest.version);
                Notice::success(match &installation.outcome {
                    InstallOutcome::Installed => {
                        format!("Installed {name} {version}; {SKILL_LOADS_IN_A_NEW_CHAT}")
                    }
                    InstallOutcome::Replaced { previous_version }
                        if is_later(version, previous_version) =>
                    {
                        format!(
                            "Updated {name} {previous_version} to {version}; {SKILL_LOADS_IN_A_NEW_CHAT}"
                        )
                    }
                    InstallOutcome::Replaced { previous_version } => format!(
                        "Replaced {name} {previous_version} with {version}; {SKILL_LOADS_IN_A_NEW_CHAT}"
                    ),
                    InstallOutcome::Reinstalled => {
                        format!("Reinstalled {name} {version}; {SKILL_LOADS_IN_A_NEW_CHAT}")
                    }
                    InstallOutcome::AlreadyInstalled => {
                        format!("{name} {version} is already installed")
                    }
                })
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
    /// Closes the menus and tells the user how to log the package in.
    ExplainLogin(String),
    /// Opens the confirmation of uninstalling the package.
    ConfirmUninstall {
        name: String,
        version: String,
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

/// A row of the Installed page.
enum InstalledEntry<'a> {
    Usable {
        package: &'a InstalledPackage,
        login: LoginState,
    },
    Unusable(&'a BrokenPackage),
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
    installed: Listing<InstalledList>,
    /// The number of the read `installed` holds; a read that started before it is stale.
    installed_read: u64,
    marketplace: Listing<Marketplace>,
    selected: [usize; 2],
    top: [usize; 2],
    menus: Vec<PanelMenu>,
    /// The operation under way, as the panel shows it.
    busy: Option<String>,
    notice: Option<Notice>,
    /// The body rows each list row or menu option was drawn on, for pointer selection.
    row_targets: Vec<(std::ops::Range<usize>, usize)>,
}

impl TicksPanel {
    pub(super) fn new(chat: Option<Uuid>) -> Self {
        Self {
            id: Uuid::new_v4(),
            chat,
            page: INSTALLED_PAGE,
            installed: Listing::Loading,
            installed_read: 0,
            marketplace: Listing::Loading,
            selected: [0; 2],
            top: [0; 2],
            menus: Vec::new(),
            busy: None,
            notice: None,
            row_targets: Vec::new(),
        }
    }

    pub(super) fn id(&self) -> Uuid {
        self.id
    }

    pub(super) fn apply(&mut self, result: TickPackageResult) {
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
                        .packages
                        .iter()
                        .position(|package| package.manifest.name == name)
                    && let Some(login) = list.logins.get_mut(index)
                {
                    *login = state;
                }
            }
            TickPackageResult::Installation { .. }
            | TickPackageResult::Uninstallation { .. }
            | TickPackageResult::Logout { .. } => self.busy = None,
        }
        for page in [INSTALLED_PAGE, MARKETPLACE_PAGE] {
            let count = self.entry_count(page);
            self.selected[page] = self.selected[page].min(count.saturating_sub(1));
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> TicksPanelEffect {
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
        if let Some(down) = movement {
            let count = self.entry_count(self.page);
            self.selected[self.page] = step(self.selected[self.page], count, down);
            return TicksPanelEffect::None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => TicksPanelEffect::Close,
            (KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right, _) => {
                self.page = 1 - self.page;
                TicksPanelEffect::None
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => self.start(TickPackageRequest::Load),
            (KeyCode::Enter, _) => self.open_selected(),
            _ => TicksPanelEffect::None,
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
            Choice::ExplainLogin(name) => {
                self.menus.clear();
                self.notice = Some(Notice::success(format!(
                    "Log in to {name} with tickloop tick login {name} in a terminal"
                )));
                TicksPanelEffect::None
            }
            Choice::ConfirmUninstall { name, version } => {
                let context = vec![
                    "Its program and Skills are removed.".to_owned(),
                    "Its saved login stays.".to_owned(),
                    "Its running Ticks keep running until they are closed.".to_owned(),
                ];
                self.menus.push(PanelMenu {
                    title: format!("Uninstall {name} {version}?"),
                    context,
                    options: vec![
                        (
                            "Uninstall".to_owned(),
                            Choice::Run(TickPackageRequest::Uninstall(name)),
                        ),
                        ("Cancel".to_owned(), Choice::Back),
                    ],
                    selected: 0,
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
    fn start(&mut self, request: TickPackageRequest) -> TicksPanelEffect {
        let busy = match &request {
            TickPackageRequest::Load => {
                self.installed = Listing::Loading;
                self.marketplace = Listing::Loading;
                return TicksPanelEffect::Work(request);
            }
            TickPackageRequest::RefreshLogin(_) => return TicksPanelEffect::Work(request),
            TickPackageRequest::Install(package) => match self.installed_version(&package.name) {
                Some(installed) if is_later(&package.version, installed) => {
                    format!("Updating {} to {}…", package.name, package.version)
                }
                Some(_) => format!("Replacing {} with {}…", package.name, package.version),
                None => format!("Installing {} {}…", package.name, package.version),
            },
            TickPackageRequest::Uninstall(name) => format!("Uninstalling {name}…"),
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
                if manifest.auth != PackageAuth::None {
                    options.push(("Log in".to_owned(), Choice::ExplainLogin(name.clone())));
                    options.push((
                        "Log out".to_owned(),
                        Choice::Run(TickPackageRequest::LogOut(name.clone())),
                    ));
                }
                if let Some(offer) =
                    offered.filter(|offer| offer.installed != InstalledVersion::Offered)
                {
                    let offered_version = &offer.package.version;
                    let label = if is_later(offered_version, &manifest.version) {
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
                    "Uninstall".to_owned(),
                    Choice::ConfirmUninstall {
                        name: name.clone(),
                        version: manifest.version.clone(),
                    },
                ));
                (
                    format!("{name} {}", manifest.version),
                    vec![
                        login_label(*login).to_owned(),
                        safe_line(&manifest.description),
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
                options.push((
                    "Uninstall".to_owned(),
                    Choice::ConfirmUninstall {
                        name: name.clone(),
                        version: package.version.clone(),
                    },
                ));
                (
                    format!("{name} {}", package.version),
                    vec![format!("Unusable: {}", safe_line(&package.reason))],
                )
            }
        };
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
            .packages
            .iter()
            .find(|package| package.manifest.name == name)
            .map(|package| package.manifest.version.as_str())
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
            .packages
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
        let empty = InstalledPackages::default();
        let installed = match &self.installed {
            Listing::Loaded(list) => &list.packages,
            _ => &empty,
        };
        offers(marketplace, installed)
    }

    fn entry_count(&self, page: usize) -> usize {
        if page == INSTALLED_PAGE {
            self.installed_entries().len()
        } else {
            self.offers().len()
        }
    }

    /// The rows this page's list takes, one or two for each package at `width`.
    fn list_rows(&self, width: usize, display: DisplayProfile) -> Vec<(usize, Vec<RoleLine>)> {
        let glyph = display.glyph(Glyph::ProtocolTick);
        let selected = self.selected[self.page];
        let row = |index: usize, name: &str, details: String, glyph_role: Role| {
            ListRow {
                selected: index == selected && self.menus.is_empty(),
                glyph,
                glyph_role,
                name,
                details,
                fill: None,
                subscription: None,
                bar_columns: None,
            }
            .lines(width)
        };
        if self.page == INSTALLED_PAGE {
            return self
                .installed_entries()
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let (details, glyph_role) = match entry {
                        InstalledEntry::Usable { package, login } => (
                            format!("{} · {}", package.manifest.version, login_label(*login)),
                            match login {
                                LoginState::LoggedIn | LoginState::NotRequired => Role::Success,
                                LoginState::LoggedOut | LoginState::Expired => Role::Warning,
                                LoginState::Unknown => Role::MutedText,
                            },
                        ),
                        InstalledEntry::Unusable(package) => {
                            (format!("{} · unusable", package.version), Role::Error)
                        }
                    };
                    (index, row(index, entry.name(), details, glyph_role))
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
                        format!("{} · {state}", package.version),
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
        if let Some(busy) = &self.busy {
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
    pub(super) fn minimum_frame_rows(&self) -> usize {
        CHROME_ROWS + 1
    }

    pub(super) fn content_rows(&self, width: usize) -> usize {
        let body = match self.menus.last() {
            Some(menu) => 1 + context_lines(menu, width).len() + menu.options.len(),
            None => match self
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
        let (hint, selected_rows) = if let Some(menu) = self.menus.last() {
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
            let hint = fit_hint(
                &[
                    (enter.to_owned(), 10),
                    ("tab/←/→ page".to_owned(), 6),
                    ("r refresh".to_owned(), 4),
                    ("esc close".to_owned(), 9),
                ],
                width,
                display,
            );
            (hint, selected_rows)
        };
        let top = &mut self.top[self.page];
        if self.menus.is_empty() {
            if selected_rows.start < *top {
                *top = selected_rows.start;
            }
            if selected_rows.end > *top + capacity {
                *top = selected_rows.end.saturating_sub(capacity);
            }
            *top = (*top).min(body.len().saturating_sub(capacity));
        }
        let skip = if self.menus.is_empty() {
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
        lines.push(RoleLine::role(hint, Role::MutedText));
        lines.truncate(height);
        for line in &mut lines {
            line.truncate(width);
        }
        ConversationPickerFrame {
            lines,
            cursor: None,
        }
    }

    /// What the page shows without packages to list, wrapped at `width`.
    fn empty_lines(&self, width: usize) -> Vec<RoleLine> {
        let (text, role) = match (self.page, &self.installed, &self.marketplace) {
            (INSTALLED_PAGE, Listing::Loading, _) => ("Loading…".to_owned(), Role::MutedText),
            (INSTALLED_PAGE, Listing::Failed(error), _) => (safe_line(error), Role::Error),
            (INSTALLED_PAGE, Listing::Loaded(_), _) => (
                "No Tick packages are installed; the Marketplace page offers them.".to_owned(),
                Role::MutedText,
            ),
            (_, _, Listing::Loading) => {
                ("Reading the Tick marketplace…".to_owned(), Role::MutedText)
            }
            (_, _, Listing::Failed(error)) => (safe_line(error), Role::Error),
            (_, _, Listing::Loaded(_)) => (
                "The Tick marketplace offers no packages yet.".to_owned(),
                Role::MutedText,
            ),
        };
        wrap_text(&text, width.saturating_sub(EMPTY_INDENT.len()).max(1))
            .into_iter()
            .map(|line| RoleLine::role(format!("{EMPTY_INDENT}{line}"), role))
            .collect()
    }
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

    use tick_packages::{OAuthIssuer, PackageManifest};

    use super::*;

    const WIDTH: usize = 90;
    const HEIGHT: usize = 14;

    fn installed(name: &str, version: &str, declares_login: bool) -> InstalledPackage {
        let auth = if declares_login {
            PackageAuth::OAuth2 {
                issuers: vec![OAuthIssuer {
                    label: "Server".into(),
                    issuer: "https://auth.example.com".into(),
                }],
                scopes: Vec::new(),
            }
        } else {
            PackageAuth::None
        };
        InstalledPackage {
            manifest: PackageManifest {
                name: name.into(),
                version: version.into(),
                description: format!("The {name} package"),
                executable: PathBuf::from(format!("bin/tick-{name}")),
                protocol: true,
                auth,
            },
            root: PathBuf::from("/packages").join(name),
            source: String::new(),
            sha256: None,
            installed_at: "2026-10-15T00:00:00Z".into(),
        }
    }

    /// The installed packages as the read numbered `read` found them, each with its login state.
    fn installed_list(
        read: u64,
        packages: Vec<(InstalledPackage, LoginState)>,
    ) -> TickPackageResult {
        let (packages, logins) = packages.into_iter().unzip();
        TickPackageResult::Installed {
            read,
            listing: Ok(InstalledList {
                packages: InstalledPackages {
                    packages,
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
        }
    }

    #[test]
    fn pages_switch_and_the_list_marks_its_selection_by_its_fill_alone() {
        let mut panel = loaded_panel();
        let shown = text(&mut panel);
        assert!(
            shown.starts_with("Tick packages · [Installed]  Marketplace"),
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
            (KeyCode::Tab, "[Marketplace]"),
            (KeyCode::BackTab, "[Installed]"),
            (KeyCode::Right, "[Marketplace]"),
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
    fn enter_opens_a_package_menu_and_esc_goes_back_one_level_at_a_time() {
        let mut panel = loaded_panel();
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            TicksPanelEffect::None
        ));
        let shown = text(&mut panel);
        assert!(
            shown.contains("alpha 1.0.0") && shown.contains("  needs login"),
            "{shown}"
        );
        let lines = frame(&mut panel);
        let options = lines
            .iter()
            .map(RoleLine::text)
            .filter(|line| {
                ["❯ ", "  "].iter().any(|lead| line.starts_with(lead)) && !line.starts_with("   ")
            })
            .filter(|line| {
                ["Log in", "Log out", "Update to 2.0.0", "Uninstall"]
                    .iter()
                    .any(|label| line.ends_with(label))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            options,
            ["❯ Log in", "  Log out", "  Update to 2.0.0", "  Uninstall"]
        );
        let cursor = lines
            .iter()
            .find(|line| line.text().starts_with('❯'))
            .unwrap();
        assert_eq!(cursor.fill, Some(Role::Selection));
        assert!(shown.contains("enter confirm · esc back"), "{shown}");
        for _ in 0..3 {
            control(&mut panel, 'n');
        }
        press(&mut panel, KeyCode::Enter);
        let shown = text(&mut panel);
        assert!(shown.contains("Uninstall alpha 1.0.0?"), "{shown}");
        assert!(shown.contains("Its saved login stays."), "{shown}");
        assert!(
            shown.contains("Its running Ticks keep running until they are closed."),
            "{shown}"
        );
        assert!(
            shown.contains("❯ Uninstall") && shown.contains("  Cancel"),
            "{shown}"
        );
        press(&mut panel, KeyCode::Esc);
        assert!(text(&mut panel).contains("❯ Uninstall") && !text(&mut panel).contains("Cancel"));
        press(&mut panel, KeyCode::Esc);
        assert!(!text(&mut panel).contains('❯'));
        assert!(matches!(
            press(&mut panel, KeyCode::Esc),
            TicksPanelEffect::Close
        ));
    }

    #[test]
    fn a_package_without_login_offers_no_login_and_cancel_returns_to_its_actions() {
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
        press(&mut panel, KeyCode::Enter);
        control(&mut panel, 'n');
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            TicksPanelEffect::None
        ));
        assert!(text(&mut panel).contains("❯ Uninstall") && !text(&mut panel).contains("Cancel"));
        press(&mut panel, KeyCode::Enter);
        assert_eq!(
            work(press(&mut panel, KeyCode::Enter)),
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
            }),
        });
        let notice = frame(&mut panel)
            .iter()
            .filter(|line| {
                line.spans
                    .first()
                    .is_some_and(|span| span.role == Some(Role::Success))
            })
            .map(RoleLine::text)
            .collect::<String>();
        assert_eq!(
            notice,
            "Uninstalled beta 1.0.0; its saved login stays in /config/credentials/beta; its running Ticks keep running until closed"
        );
    }

    #[test]
    fn logging_in_points_to_the_terminal_command_until_the_panel_logs_in_itself() {
        let mut panel = loaded_panel();
        press(&mut panel, KeyCode::Enter);
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            TicksPanelEffect::None
        ));
        let shown = text(&mut panel);
        assert!(
            shown.contains("Log in to alpha with tickloop tick login alpha in a terminal"),
            "{shown}"
        );
        assert!(!shown.contains('❯'), "the menus close:\n{shown}");
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
        assert!(text(&mut panel).contains(
            "Logged out of alpha; the authorization server did not revoke the token \
             (https://auth.example.com/revoke answered HTTP 503)"
        ));
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
        updated.source = "https://example.com/tick-alpha-2.0.0.tar.gz".into();
        panel.apply(TickPackageResult::Installation {
            name: "alpha".into(),
            result: Ok(Box::new(Installation {
                package: updated,
                outcome: InstallOutcome::Replaced {
                    previous_version: "1.0.0".into(),
                },
            })),
        });
        assert!(
            text(&mut panel)
                .contains("Updated alpha 1.0.0 to 2.0.0; its Skill loads in a new Chat (/clear)")
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
                package: installed("alpha", "0.2.0", true),
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
