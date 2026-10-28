use super::*;
use crate::terminal::sandbox_surface::SandboxSurface;

impl Menus {
    pub(in crate::terminal) async fn sandbox_command(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        command: &str,
    ) -> Result<(), String> {
        let folder = match target {
            ConfigTarget::Agent(chat) => agent
                .status(chat)
                .await
                .map_err(|error| error.to_string())?
                .folder
                .ok_or("This Chat has no Folder")?,
            ConfigTarget::NewDraft => agent.launch_spec().folder,
        }
        .canonicalize()
        .map_err(|error| error.to_string())?;
        let settings = self
            .user_settings
            .clone()
            .ok_or("User settings unavailable")?;
        let arguments = shell_words::split(command).unwrap_or_default();
        if arguments.len() != 1 {
            let [_, action, pattern] = arguments.as_slice() else {
                return console.hold_status("Usage: /sandbox exclude \"COMMAND PATTERN\"");
            };
            if action != "exclude" || pattern.trim().is_empty() {
                return console.hold_status("Usage: /sandbox exclude \"COMMAND PATTERN\"");
            }
            let snapshot = settings.load().sandbox_snapshot(&folder)?;
            if snapshot.settings.excluded_commands.contains(pattern) {
                return console.hold_status("Exclusion already present; no change");
            }
            return match settings.update_sandbox(
                &folder,
                &snapshot,
                tickloop_code_settings::sandbox::SandboxChange::Exclude(pattern.clone()),
            ) {
                Ok(()) => console
                    .hold_status("Added exclusion for this Folder · normal approval still applies"),
                Err(tickloop_code_settings::SettingsError::Conflict) => {
                    console.hold_status("Permissions changed elsewhere; review /sandbox and retry")
                }
                Err(error) => Err(error.to_string()),
            };
        }
        let sources = agent
            .directory_sources(target, &folder)
            .await
            .map_err(|error| error.to_string())?;
        let mut roots = vec![format!("Folder: {} [this Folder]", folder.display())];
        for (path, sources) in sources {
            let sources = sources
                .iter()
                .map(|source| match source {
                    tickloop_harness::protocol::AdditionalDirectorySource::UserSettings => "global",
                    tickloop_harness::protocol::AdditionalDirectorySource::DirectorySettings => {
                        "this Folder"
                    }
                    tickloop_harness::protocol::AdditionalDirectorySource::LaunchArgument => {
                        "launch argument"
                    }
                    tickloop_harness::protocol::AdditionalDirectorySource::Session => "session",
                })
                .collect::<Vec<_>>()
                .join(" + ");
            roots.push(format!(
                "Additional directory: {} [{sources}]",
                path.display()
            ));
        }
        for path in tickloop_harness::base::git_worktree::sibling_linked_worktrees(&folder) {
            roots.push(format!(
                "Linked worktree: {} [verified repository link]",
                path.display()
            ));
        }
        roots.push(format!(
            "Temporary directory: {} [system]",
            std::env::temp_dir().display()
        ));
        roots.push("Scratchpad: this Agent family's private temporary directory [session]".into());
        let availability = tickhost::sandbox::availability().await;
        console.show_sandbox(SandboxSurface::open(settings, folder, roots, availability)?)
    }
}
