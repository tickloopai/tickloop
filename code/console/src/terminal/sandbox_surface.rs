use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickloop_code_settings::{
    SettingsError, UserSettings,
    sandbox::{SandboxChange, SandboxMode, SandboxSettings, SandboxSnapshot},
};

use super::{
    permissions_surface::tab_header_line,
    style::{Role, RoleLine},
};

const MODE_AUTO_ALLOW: usize = 0;
const MODE_REGULAR: usize = 1;
const MODE_DISABLED: usize = 2;
const OVERRIDE_ALLOW: usize = 0;
const OVERRIDE_STRICT: usize = 1;
const PANEL_CHROME_ROWS: usize = 2;
const MINIMUM_FRAME_ROWS: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Mode,
    Overrides,
    Config,
}

impl Page {
    fn adjacent(self, backwards: bool) -> Self {
        match (self, backwards) {
            (Self::Mode, true) | (Self::Overrides, false) => Self::Config,
            (Self::Mode, false) | (Self::Config, true) => Self::Overrides,
            (Self::Overrides, true) | (Self::Config, false) => Self::Mode,
        }
    }

    fn options(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Mode => &[
                (
                    "Sandbox with auto-allow",
                    "Sandboxed Bash skips ordinary approval; Ask/Deny still apply.",
                ),
                (
                    "Sandbox with regular permissions",
                    "Keep isolation and normal approval.",
                ),
                (
                    "No sandbox",
                    "Commands use normal approval without sandbox isolation.",
                ),
            ],
            Self::Overrides => &[
                (
                    "Allow unsandboxed commands",
                    "Model escape requests and excluded commands use normal approval.",
                ),
                (
                    "Strict sandbox",
                    "All commands invoked by the model must run in the sandbox unless explicitly listed in excludedCommands.",
                ),
            ],
            Self::Config => &[],
        }
    }
}

pub(super) struct SandboxSurface {
    settings: UserSettings,
    folder: PathBuf,
    snapshot: SandboxSnapshot,
    page: Page,
    selected: usize,
    message: String,
    roots: Vec<String>,
    availability: Result<(), String>,
    offset: usize,
}

impl SandboxSurface {
    pub(super) fn open(
        settings: UserSettings,
        folder: PathBuf,
        roots: Vec<String>,
        availability: Result<(), String>,
    ) -> Result<Self, String> {
        let snapshot = settings.load().sandbox_snapshot(&folder)?;
        let selected = Self::current_selection(Page::Mode, &snapshot.settings);
        Ok(Self {
            settings,
            folder,
            snapshot,
            page: Page::Mode,
            selected,
            message: String::new(),
            roots,
            availability,
            offset: 0,
        })
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Char('c') if control => return true,
            KeyCode::Left | KeyCode::Right => {
                self.page = self.page.adjacent(key.code == KeyCode::Left);
                self.selected = Self::current_selection(self.page, &self.snapshot.settings);
                self.message.clear();
                self.offset = 0;
            }
            KeyCode::Char('j') | KeyCode::Down if self.page == Page::Config && !control => {
                self.offset = self.offset.saturating_add(1)
            }
            KeyCode::Char('n') if self.page == Page::Config && control => {
                self.offset = self.offset.saturating_add(1)
            }
            KeyCode::Char('k') | KeyCode::Up if self.page == Page::Config && !control => {
                self.offset = self.offset.saturating_sub(1)
            }
            KeyCode::Char('p') if self.page == Page::Config && control => {
                self.offset = self.offset.saturating_sub(1)
            }
            KeyCode::Home if self.page == Page::Config => self.offset = 0,
            KeyCode::End if self.page == Page::Config => self.offset = usize::MAX,
            KeyCode::Char('j') | KeyCode::Down if !control => {
                self.selected = (self.selected + 1).min(self.option_count().saturating_sub(1))
            }
            KeyCode::Char('n') if control => {
                self.selected = (self.selected + 1).min(self.option_count().saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up if !control => {
                self.selected = self.selected.saturating_sub(1)
            }
            KeyCode::Char('p') if control => self.selected = self.selected.saturating_sub(1),
            KeyCode::Enter if self.page != Page::Config => {
                if self.page == Page::Mode
                    && self.selected != MODE_DISABLED
                    && !cfg!(target_os = "macos")
                {
                    self.message = "Cannot enable sandbox: not supported on this platform".into();
                    return false;
                }
                let change = if self.page == Page::Mode {
                    SandboxChange::Mode(match self.selected {
                        MODE_AUTO_ALLOW => SandboxMode::AutoAllow,
                        MODE_REGULAR => SandboxMode::Regular,
                        _ => SandboxMode::Disabled,
                    })
                } else {
                    SandboxChange::AllowUnsandboxed(self.selected == OVERRIDE_ALLOW)
                };
                self.message =
                    match self
                        .settings
                        .update_sandbox(&self.folder, &self.snapshot, change)
                    {
                        Ok(()) => "Saved for this Folder · applies to the next command".into(),
                        Err(SettingsError::Conflict) => {
                            "Permissions changed elsewhere; refreshed. Review and confirm again"
                                .into()
                        }
                        Err(error) => format!("Cannot save sandbox: {error}"),
                    };
                match self.settings.load().sandbox_snapshot(&self.folder) {
                    Ok(snapshot) => self.snapshot = snapshot,
                    Err(error) => self.message = error,
                }
            }
            _ => {}
        }
        false
    }

    fn current_selection(page: Page, settings: &SandboxSettings) -> usize {
        match page {
            Page::Mode if !settings.enabled => MODE_DISABLED,
            Page::Mode if settings.auto_allow_bash_if_sandboxed => MODE_AUTO_ALLOW,
            Page::Mode => MODE_REGULAR,
            Page::Overrides if settings.allow_unsandboxed_commands => OVERRIDE_ALLOW,
            Page::Overrides => OVERRIDE_STRICT,
            Page::Config => 0,
        }
    }

    fn option_count(&self) -> usize {
        self.page.options().len()
    }

    fn config_lines(&self) -> Vec<String> {
        let config = &self.snapshot.settings;
        let mut lines =
            vec!["Config · read-only; shown rules apply when the sandbox is enabled".into()];
        for (name, value) in [
            ("enabled", config.enabled),
            (
                "autoAllowBashIfSandboxed",
                config.auto_allow_bash_if_sandboxed,
            ),
            (
                "allowUnsandboxedCommands",
                config.allow_unsandboxed_commands,
            ),
        ] {
            lines.push(format!(
                "{name}: {value} [{}]",
                self.snapshot.source(&format!("/{name}"), None)
            ));
        }
        lines.push("Excluded commands (normal approval outside sandbox):".into());
        if config.excluded_commands.is_empty() {
            lines.push(format!(
                "  None [{}]",
                self.snapshot.source("/excludedCommands", None)
            ));
        }
        for pattern in &config.excluded_commands {
            lines.push(format!(
                "  {pattern} [{}]",
                self.snapshot.source("/excludedCommands", Some(pattern))
            ));
        }
        lines.push("Writable scope:".into());
        lines.extend(self.roots.iter().cloned());
        for path in &config.filesystem.allow_write {
            lines.push(format!(
                "allowWrite: {path} [{}]",
                self.snapshot.source("/filesystem/allowWrite", Some(path))
            ));
        }
        lines.push("Relative paths resolve from this Folder; ~/ resolves from your home.".into());
        lines.push(format!(
            "allowGitConfig: {} [{}]",
            config.filesystem.allow_git_config,
            self.snapshot.source("/filesystem/allowGitConfig", None)
        ));
        lines.push(format!(
            "Default protected paths [built-in]: {}",
            config
                .filesystem
                .default_deny_write_paths()
                .collect::<Vec<_>>()
                .join(", ")
        ));
        lines.push(format!(
            "Sensitive paths [built-in]: {}",
            tickloop_harness::tools::credential_write_path_descriptions().join(", ")
        ));
        if let Some(root) = self.settings.root() {
            lines.push(format!(
                "User settings and state: {} [global; denied]",
                root.display()
            ));
        }
        for path in &config.filesystem.deny_write {
            lines.push(format!(
                "denyWrite: {path} [{}]",
                self.snapshot.source("/filesystem/denyWrite", Some(path))
            ));
        }
        lines.push("Deny rules take precedence over writable scope.".into());
        lines.push("Network allowlist:".into());
        if config.network.allowed_domains.is_empty() {
            lines.push(format!(
                "  None; disconnected [{}]",
                self.snapshot.source("/network/allowedDomains", None)
            ));
        }
        for domain in &config.network.allowed_domains {
            lines.push(format!(
                "  {domain} [{}]",
                self.snapshot
                    .source("/network/allowedDomains", Some(domain))
            ));
        }
        lines.push(format!(
            "failIfUnavailable: {} [{}]",
            config.fail_if_unavailable,
            self.snapshot.source("/failIfUnavailable", None)
        ));
        lines.push(format!(
            "Host support [local]: {}",
            if !cfg!(target_os = "macos") {
                "not supported on this platform"
            } else {
                match &self.availability {
                    Ok(()) => "available",
                    Err(error) => error,
                }
            }
        ));
        lines.push("Read restrictions (allowRead/denyRead): not supported yet".into());
        lines.push("Other network fields (local binding / Unix sockets): not supported yet".into());
        lines
    }

    fn body(&self, width: usize) -> (Vec<RoleLine>, Option<std::ops::Range<usize>>) {
        use super::loop_view_text::wrapped;
        if self.page == Page::Config {
            return (
                self.config_lines()
                    .iter()
                    .flat_map(|line| wrapped(line, width, Role::Text))
                    .collect(),
                None,
            );
        }
        let mut lines = wrapped(
            &format!("Folder: {}", self.folder.display()),
            width,
            Role::MutedText,
        );
        if !cfg!(target_os = "macos") {
            lines.extend(wrapped(
                "Sandbox cannot be enabled on this platform",
                width,
                Role::Warning,
            ));
        }
        let current = Self::current_selection(self.page, &self.snapshot.settings);
        let options = self.page.options();
        let mut selected_rows = None;
        for (index, (label, description)) in options.iter().enumerate() {
            let start = lines.len();
            let cursor = if index == self.selected { "❯" } else { " " };
            lines.extend(wrapped(
                &format!(
                    "{cursor} {label}{}",
                    if index == current { " · Current" } else { "" }
                ),
                width,
                if index == self.selected {
                    Role::Selection
                } else {
                    Role::Text
                },
            ));
            if index == self.selected {
                selected_rows = Some(start..lines.len());
            }
            lines.extend(wrapped(&format!("  {description}"), width, Role::MutedText));
        }
        (lines, selected_rows)
    }

    pub(super) fn minimum_frame_rows(&self, width: usize) -> usize {
        let label_rows = self.body(width).1.map_or(1, |rows| rows.len());
        MINIMUM_FRAME_ROWS
            .max(PANEL_CHROME_ROWS + label_rows + usize::from(!self.message.is_empty()))
    }

    pub(super) fn content_rows(&self, width: usize) -> usize {
        PANEL_CHROME_ROWS + self.body(width).0.len() + usize::from(!self.message.is_empty())
    }

    pub(super) fn styled_lines(&mut self, width: usize, height: usize) -> Vec<RoleLine> {
        let mut lines = vec![tab_header_line(
            "Sandbox",
            self.page as usize,
            &["Mode", "Overrides", "Config"],
            width,
        )];
        let (body, selected) = self.body(width);
        let available =
            height.saturating_sub(PANEL_CHROME_ROWS + usize::from(!self.message.is_empty()));
        if let Some(selected) = selected {
            if selected.start < self.offset {
                self.offset = selected.start;
            }
            if selected.end > self.offset.saturating_add(available) {
                self.offset = selected.end.saturating_sub(available);
            }
        }
        self.offset = self.offset.min(body.len().saturating_sub(available));
        lines.extend(body.into_iter().skip(self.offset).take(available));
        if !self.message.is_empty() {
            lines.push(RoleLine::role(
                super::safe_text(&self.message),
                Role::MutedText,
            ));
        }
        lines.push(RoleLine::role(
            if self.page == Page::Config {
                "←/→ page · ctrl+n/p scroll · home/end · esc close"
            } else {
                "enter save · esc close"
            },
            Role::MutedText,
        ));
        lines
    }
}
