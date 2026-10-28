use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickloop_code_settings::{
    SettingsError, UserSettings,
    sandbox::{SandboxChange, SandboxMode, SandboxSnapshot},
};

use super::{
    permissions_surface::tab_header_line,
    style::{Role, RoleLine},
};

pub(super) struct SandboxSurface {
    settings: UserSettings,
    folder: PathBuf,
    snapshot: SandboxSnapshot,
    page: usize,
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
        let selected = if !snapshot.settings.enabled {
            2
        } else {
            usize::from(!snapshot.settings.auto_allow_bash_if_sandboxed)
        };
        Ok(Self {
            settings,
            folder,
            snapshot,
            page: 0,
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
                self.page = (self.page + if key.code == KeyCode::Right { 1 } else { 2 }) % 3;
                self.selected = match self.page {
                    0 if !self.snapshot.settings.enabled => 2,
                    0 => usize::from(!self.snapshot.settings.auto_allow_bash_if_sandboxed),
                    1 => usize::from(!self.snapshot.settings.allow_unsandboxed_commands),
                    _ => 0,
                };
                self.message.clear();
                self.offset = 0;
            }
            KeyCode::Char('j') | KeyCode::Down if self.page == 2 && !control => {
                self.offset = self.offset.saturating_add(1)
            }
            KeyCode::Char('n') if self.page == 2 && control => {
                self.offset = self.offset.saturating_add(1)
            }
            KeyCode::Char('k') | KeyCode::Up if self.page == 2 && !control => {
                self.offset = self.offset.saturating_sub(1)
            }
            KeyCode::Char('p') if self.page == 2 && control => {
                self.offset = self.offset.saturating_sub(1)
            }
            KeyCode::Home if self.page == 2 => self.offset = 0,
            KeyCode::End if self.page == 2 => self.offset = usize::MAX,
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
            KeyCode::Enter if self.page < 2 => {
                if self.page == 0 && self.selected < 2 && !cfg!(target_os = "macos") {
                    self.message = "本平台暂不支持，不能打开沙箱".into();
                    return false;
                }
                let change = if self.page == 0 {
                    SandboxChange::Mode(match self.selected {
                        0 => SandboxMode::AutoAllow,
                        1 => SandboxMode::Regular,
                        _ => SandboxMode::Disabled,
                    })
                } else {
                    SandboxChange::AllowUnsandboxed(self.selected == 0)
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

    fn option_count(&self) -> usize {
        match self.page {
            0 => 3,
            1 => 2,
            _ => 0,
        }
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
            lines.push("  None [default]".into());
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
            "Sensitive paths [built-in]: {}",
            tickloop_harness::tools::sensitive_write_path_descriptions().join(", ")
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
            lines.push("  None; disconnected [default]".into());
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
                "本平台暂不支持"
            } else {
                match &self.availability {
                    Ok(()) => "available",
                    Err(error) => error,
                }
            }
        ));
        lines.push("Read restrictions (allowRead/denyRead): 暂不支持".into());
        lines.push("Other network fields (local binding / Unix sockets): 暂不支持".into());
        lines
    }

    fn body(&self, width: usize) -> (Vec<RoleLine>, Option<std::ops::Range<usize>>) {
        use super::loop_view_text::wrapped;
        if self.page == 2 {
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
                "本平台暂不支持，不能打开沙箱",
                width,
                Role::Warning,
            ));
        }
        let current = match self.page {
            0 if !self.snapshot.settings.enabled => 2,
            0 => usize::from(!self.snapshot.settings.auto_allow_bash_if_sandboxed),
            _ => usize::from(!self.snapshot.settings.allow_unsandboxed_commands),
        };
        let options: &[(&str, &str)] = if self.page == 0 {
            &[
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
            ]
        } else {
            &[
                (
                    "Allow unsandboxed commands",
                    "Model escape requests and excluded commands use normal approval.",
                ),
                (
                    "Strict sandbox",
                    "All commands invoked by the model must run in the sandbox unless explicitly listed in excludedCommands.",
                ),
            ]
        };
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
            lines.extend(wrapped(&format!("  {description}"), width, Role::MutedText));
            if index == self.selected {
                selected_rows = Some(start..lines.len());
            }
        }
        (lines, selected_rows)
    }

    pub(super) fn content_rows(&self, width: usize) -> usize {
        2 + self.body(width).0.len() + usize::from(!self.message.is_empty())
    }

    pub(super) fn styled_lines(&mut self, width: usize, height: usize) -> Vec<RoleLine> {
        let mut lines = vec![tab_header_line(
            "Sandbox",
            self.page,
            &["Mode", "Overrides", "Config"],
            width,
        )];
        let (body, selected) = self.body(width);
        let available = height.saturating_sub(2 + usize::from(!self.message.is_empty()));
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
            if self.page == 2 {
                "←/→ page · j/k ctrl+n/p scroll · home/end · esc close"
            } else {
                "←/→ page · j/k ctrl+n/p move · enter save · esc close"
            },
            Role::MutedText,
        ));
        lines
    }
}
