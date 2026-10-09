//! Tick Loop's Herdr popup plugin: installing it into or removing it from one Herdr session,
//! whether it is ready there, and floating a hosted Tick in it.
//!
//! The plugin is static: its only pane runs this executable's `monitor attach`, which reads the
//! one-time view grant `plugin.pane.open` passes in `TICKLOOP_MONITOR_ATTACH`. Herdr copies a
//! manifest when it links a plugin and silently replaces a linked plugin that has the same id, so
//! a plugin under our id counts as ours only when Herdr reports our own directory as its root.
//! Herdr 0.9 opens a popup in front of the session's current client and cannot target one: with
//! several clients on one session it may appear where Herdr places it.

use std::{
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::{Value, json};
use tickhost_connect::{
    herdr::{HerdrContext, HerdrPort, SocketHerdr},
    host_error::HostError,
};
use tokio_util::sync::CancellationToken;

use super::attach::ATTACH_ENVIRONMENT;
use tickloop_agent_base::private_fs::{
    PRIVATE_FILE_MODE, create_private_directory, current_uid, replace_file_atomically,
    verify_private_directory,
};
use tickloop_agent_config::{MonitorPopup, UserSettings};
use tickloop_agent_protocol::HostedViewGrant;

pub(crate) const PLUGIN_ID: &str = "tickloop.monitor";
const PLUGIN_NAME: &str = "Tick Loop Monitor";
const PANE_ID: &str = "attach";
const PANE_TITLE: &str = "Tick Loop";
const POPUP_PLACEMENT: &str = "popup";
const POPUP_SIZE: &str = "85%";
/// Arguments Herdr passes to this executable to open the Monitor popup pane.
pub const ATTACH_ARGUMENTS: [&str; 2] = ["monitor", "attach"];
const PLUGIN_DIRECTORY: &str = "herdr-popup-plugin";
const MANIFEST_FILE: &str = "herdr-plugin.toml";
const MIN_HERDR_VERSION: &str = "0.9.0";
const PLUGIN_UNLINK: &str = "plugin.unlink";
const PLUGIN_PANE_OPEN: &str = "plugin.pane.open";
/// Herdr's refusal to open a popup outside its normal workspace view.
const HERDR_UI_BUSY: &str = "ui_busy";

/// Whether this Herdr session can float a hosted Tick in Tick Loop's popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PopupReadiness {
    Ready,
    NotInstalled,
    /// Ours, but disabled in Herdr.
    Disabled,
    /// Another plugin holds our id; it is never replaced.
    Foreign {
        root: PathBuf,
    },
}

/// Whether Herdr `version` is at least the plugin's minimum release; a pre-release of that
/// minimum comes before it.
fn is_supported_herdr(version: &str) -> bool {
    match (release(version), release(MIN_HERDR_VERSION)) {
        (Some(found), Some(minimum)) => found >= minimum,
        _ => false,
    }
}

/// A `MAJOR.MINOR.PATCH[-PRE][+BUILD]` version as an ordered key: the release numbers, then
/// whether it is the release itself rather than a pre-release of it. Build metadata is ignored.
fn release(version: &str) -> Option<([u64; 3], bool)> {
    let version = version
        .split_once('+')
        .map_or(version, |(version, _)| version);
    let (numbers, is_release) = match version.split_once('-') {
        Some((numbers, _)) => (numbers, false),
        None => (version, true),
    };
    let numbers = numbers
        .split('.')
        .map(|number| {
            number
                .bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| number.parse::<u64>().ok())
                .flatten()
        })
        .collect::<Option<Vec<_>>>()?;
    match numbers.as_slice() {
        &[major, minor, patch] => Some(([major, minor, patch], is_release)),
        _ => None,
    }
}

/// `text` as a TOML basic string.
fn toml_basic_string(text: &str) -> String {
    let mut quoted = String::from('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            character if character.is_control() => {
                quoted.push_str(&format!("\\u{:04X}", u32::from(character)));
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

/// The pane of a linked plugin as Herdr reports it.
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct RegisteredPane {
    id: String,
    title: String,
    placement: String,
    width: Option<Value>,
    height: Option<Value>,
    command: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RegisteredPlugin {
    plugin_id: String,
    plugin_root: PathBuf,
    enabled: bool,
    panes: Vec<RegisteredPane>,
}

/// Tick Loop's popup plugin: a private directory under the state root whose manifest runs this
/// executable's attach client and nothing else.
pub(crate) struct PopupPlugin {
    state_root: PathBuf,
    pane: RegisteredPane,
}

impl PopupPlugin {
    pub(crate) fn for_this_executable(state_root: &Path) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|error| format!("cannot find the tickloop executable: {error}"))?
            .into_os_string()
            .into_string()
            .map_err(|_| {
                "the tickloop executable path is not UTF-8, so Herdr cannot run it".to_owned()
            })?;
        Ok(Self::new(state_root, executable))
    }

    fn new(state_root: &Path, executable: String) -> Self {
        Self {
            state_root: state_root.to_path_buf(),
            pane: RegisteredPane {
                id: PANE_ID.into(),
                title: PANE_TITLE.into(),
                placement: POPUP_PLACEMENT.into(),
                width: Some(POPUP_SIZE.into()),
                height: Some(POPUP_SIZE.into()),
                command: std::iter::once(executable)
                    .chain(ATTACH_ARGUMENTS.map(String::from))
                    .collect(),
            },
        }
    }

    /// The plugin directory with symbolic links resolved, as Herdr reports a plugin root.
    fn directory(&self) -> PathBuf {
        std::fs::canonicalize(&self.state_root)
            .unwrap_or_else(|_| self.state_root.clone())
            .join(PLUGIN_DIRECTORY)
    }

    fn manifest(&self) -> String {
        let command = self
            .pane
            .command
            .iter()
            .map(|part| toml_basic_string(part))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"id = "{PLUGIN_ID}"
name = "{PLUGIN_NAME}"
version = "{version}"
min_herdr_version = "{MIN_HERDR_VERSION}"
platforms = ["macos", "linux"]

[[panes]]
id = "{PANE_ID}"
title = "{PANE_TITLE}"
placement = "{POPUP_PLACEMENT}"
width = "{POPUP_SIZE}"
height = "{POPUP_SIZE}"
command = [{command}]
"#,
            version = env!("CARGO_PKG_VERSION")
        )
    }

    /// Writes the plugin into its private directory and returns the directory to link.
    fn write(&self) -> Result<PathBuf, String> {
        let directory = self.state_root.join(PLUGIN_DIRECTORY);
        create_private_directory(&directory)
            .and_then(|()| {
                verify_private_directory(&directory, "the Tick Loop popup plugin directory")
            })
            .and_then(|()| {
                replace_file_atomically(
                    &directory.join(MANIFEST_FILE),
                    self.manifest().as_bytes(),
                    PRIVATE_FILE_MODE,
                )
            })
            .map_err(|error| format!("cannot write the Tick Loop popup plugin: {error}"))?;
        Ok(self.directory())
    }

    fn readiness(&self, registered: Vec<RegisteredPlugin>) -> PopupReadiness {
        match registered
            .into_iter()
            .find(|registered| registered.plugin_id == PLUGIN_ID)
        {
            None => PopupReadiness::NotInstalled,
            Some(registered) if registered.plugin_root != self.directory() => {
                PopupReadiness::Foreign {
                    root: registered.plugin_root,
                }
            }
            Some(registered) if registered.enabled => PopupReadiness::Ready,
            Some(_) => PopupReadiness::Disabled,
        }
    }

    fn is_registered_as_written(&self, registered: &RegisteredPlugin) -> bool {
        registered.plugin_id == PLUGIN_ID
            && registered.plugin_root == self.directory()
            && registered.enabled
            && registered.panes.as_slice() == std::slice::from_ref(&self.pane)
    }
}

/// A Herdr server reached through a socket this user owns, whose version supports the plugin.
pub(crate) struct HerdrSession {
    socket: PathBuf,
    api: SocketHerdr,
}

impl HerdrSession {
    pub(crate) async fn verify(socket: &Path) -> Result<Self, String> {
        let metadata = std::fs::symlink_metadata(socket).map_err(|error| {
            format!(
                "cannot reach the Herdr socket {}: {error}",
                socket.display()
            )
        })?;
        if !metadata.file_type().is_socket() {
            return Err(format!("{} is not a Herdr socket", socket.display()));
        }
        if metadata.uid() != current_uid() {
            return Err(format!(
                "the Herdr socket {} belongs to another user",
                socket.display()
            ));
        }
        let session = Self {
            socket: socket.to_path_buf(),
            api: SocketHerdr::new(HerdrContext::without_view(socket.to_path_buf())),
        };
        let pong = session.request("ping", json!({})).await?;
        let version = pong
            .get("version")
            .and_then(Value::as_str)
            .filter(|_| pong["type"] == "pong")
            .ok_or_else(|| format!("{} did not answer a Herdr ping", socket.display()))?;
        if !is_supported_herdr(version) {
            return Err(format!(
                "Herdr {version} cannot run the Tick Loop popup plugin; it needs {MIN_HERDR_VERSION} or later"
            ));
        }
        Ok(session)
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.api
            .request(method, params, CancellationToken::new())
            .await
            .map_err(|error| format!("Herdr {method} failed: {error}"))
    }

    pub(crate) async fn readiness(&self, plugin: &PopupPlugin) -> Result<PopupReadiness, String> {
        let listed = self
            .request("plugin.list", json!({"plugin_id": PLUGIN_ID}))
            .await?;
        let registered = serde_json::from_value(listed["plugins"].clone())
            .map_err(|error| format!("cannot read Herdr's plugin list: {error}"))?;
        Ok(plugin.readiness(registered))
    }

    /// Refuses when another plugin holds our id; it is never replaced.
    pub(crate) async fn refuse_foreign(&self, plugin: &PopupPlugin) -> Result<(), String> {
        match self.readiness(plugin).await? {
            PopupReadiness::Foreign { root } => Err(foreign_plugin(&root)),
            _ => Ok(()),
        }
    }

    /// Refuses unless the plugin under our id is our own.
    pub(crate) async fn require_ours(&self, plugin: &PopupPlugin) -> Result<(), String> {
        match self.readiness(plugin).await? {
            PopupReadiness::Ready | PopupReadiness::Disabled => Ok(()),
            PopupReadiness::NotInstalled => Err(format!(
                "{} has no Tick Loop popup plugin {PLUGIN_ID}",
                self.socket.display()
            )),
            PopupReadiness::Foreign { root } => Err(foreign_plugin(&root)),
        }
    }

    /// Links our plugin, enabled, and sets `monitorPopup` to enabled. The caller has the user's
    /// consent; another plugin under our id is refused, and a registration that does not match
    /// what was written is undone and leaves `monitorPopup` disabled.
    pub(crate) async fn install(
        &self,
        plugin: &PopupPlugin,
        settings: &UserSettings,
    ) -> Result<(), String> {
        self.refuse_foreign(plugin).await?;
        let directory = plugin.write()?;
        self.refuse_foreign(plugin).await?;
        let linked = self
            .request("plugin.link", json!({"path": directory, "enabled": true}))
            .await?;
        let registered = serde_json::from_value::<RegisteredPlugin>(linked["plugin"].clone());
        if !registered.is_ok_and(|registered| plugin.is_registered_as_written(&registered)) {
            self.request(PLUGIN_UNLINK, json!({"plugin_id": PLUGIN_ID}))
                .await?;
            settings
                .save_monitor_popup(MonitorPopup::Disabled)
                .map_err(|error| format!("undid a mismatched registration but cannot set monitorPopup to disabled: {error}"))?;
            return Err(format!(
                "Herdr registered {PLUGIN_ID} differently from what Tick Loop wrote; the registration was undone and monitorPopup set to disabled"
            ));
        }
        settings
            .save_monitor_popup(MonitorPopup::Enabled)
            .map_err(|error| {
                format!("the plugin is linked but monitorPopup cannot be set to enabled: {error}")
            })
    }

    /// Unlinks our own plugin and sets `monitorPopup` to disabled; running Ticks continue. The
    /// caller has the user's consent.
    pub(crate) async fn remove(
        &self,
        plugin: &PopupPlugin,
        settings: &UserSettings,
    ) -> Result<(), String> {
        self.require_ours(plugin).await?;
        let unlinked = self
            .request(PLUGIN_UNLINK, json!({"plugin_id": PLUGIN_ID}))
            .await?;
        if unlinked["removed"] != true {
            return Err(format!(
                "Herdr did not remove {PLUGIN_ID}; monitorPopup is unchanged"
            ));
        }
        settings
            .save_monitor_popup(MonitorPopup::Disabled)
            .map_err(|error| {
                format!("the plugin is removed but monitorPopup cannot be set to disabled: {error}")
            })
    }

    /// Floats the hosted Tick `grant` names in our popup: its attach client shows the same
    /// process until the user closes it.
    pub(crate) async fn float(&self, grant: &HostedViewGrant) -> Result<(), FloatError> {
        let grant =
            serde_json::to_string(grant).map_err(|error| FloatError::Refused(error.to_string()))?;
        self.api
            .request(
                PLUGIN_PANE_OPEN,
                json!({"plugin_id": PLUGIN_ID, "entrypoint": PANE_ID, "env": {ATTACH_ENVIRONMENT: grant}}),
                CancellationToken::new(),
            )
            .await
            .map(drop)
            .map_err(FloatError::from)
    }
}

/// Why our popup did not open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FloatError {
    /// Herdr is showing something other than its workspace view, such as another popup.
    Busy,
    /// Herdr refused the popup or was never reached; nothing opened.
    Refused(String),
    /// The request reached Herdr but its answer did not come back; the popup may be showing.
    Unknown(String),
}

impl From<HostError> for FloatError {
    fn from(error: HostError) -> Self {
        if error.dispatched {
            Self::Unknown(error.message)
        } else if error.code == HERDR_UI_BUSY {
            Self::Busy
        } else {
            Self::Refused(error.message)
        }
    }
}

fn foreign_plugin(root: &Path) -> String {
    format!(
        "{PLUGIN_ID} in this Herdr session comes from {}, not from Tick Loop; it is never replaced or removed",
        root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refusal_before_dispatch_counts_as_nothing_opened() {
        let refused = |code: &str| HostError::before(code, "refused");
        assert_eq!(FloatError::from(refused(HERDR_UI_BUSY)), FloatError::Busy);
        assert_eq!(
            FloatError::from(refused("plugin_not_found")),
            FloatError::Refused("refused".into())
        );
        for code in ["OutcomeUnknown", HERDR_UI_BUSY] {
            assert_eq!(
                FloatError::from(HostError {
                    code: code.into(),
                    message: "no answer".into(),
                    dispatched: true,
                }),
                FloatError::Unknown("no answer".into()),
                "{code} after dispatch"
            );
        }
    }

    #[test]
    fn the_plugin_needs_herdr_0_9_0_or_later() {
        for (version, supported) in [
            ("0.9.0", true),
            ("0.9.1", true),
            ("0.10.0", true),
            ("1.0.0", true),
            ("0.9.1-preview.2", true),
            ("0.9.0+build.5", true),
            ("0.9.0-rc.1", false),
            ("0.8.9", false),
            ("0.9", false),
            ("0.9.0.1", false),
            ("0.+9.0", false),
            ("v0.9.0", false),
            ("", false),
        ] {
            assert_eq!(is_supported_herdr(version), supported, "{version:?}");
        }
    }

    #[test]
    fn manifest_strings_are_quoted_for_toml() {
        assert_eq!(
            toml_basic_string("/opt/tick loop/tickloop"),
            r#""/opt/tick loop/tickloop""#
        );
        assert_eq!(
            toml_basic_string("a\"b\\c\td\ne\u{7f}é"),
            r#""a\"b\\c\u0009d\u000Ae\u007Fé""#
        );
    }

    #[test]
    fn the_manifest_runs_only_the_attach_client_in_an_85_percent_popup() {
        let plugin = PopupPlugin::new(Path::new("/state"), "/opt/tick \"loop\"/tickloop".into());
        let manifest = plugin.manifest();
        for line in [
            "id = \"tickloop.monitor\"",
            "min_herdr_version = \"0.9.0\"",
            "id = \"attach\"",
            "placement = \"popup\"",
            "width = \"85%\"",
            "height = \"85%\"",
            r#"command = ["/opt/tick \"loop\"/tickloop", "monitor", "attach"]"#,
        ] {
            assert!(
                manifest.lines().any(|candidate| candidate == line),
                "{line}\n{manifest}"
            );
        }
        assert_eq!(manifest.matches("[[panes]]").count(), 1, "{manifest}");
    }

    #[test]
    fn a_plugin_under_our_id_is_ours_only_from_our_directory_and_ready_only_when_enabled() {
        let plugin = PopupPlugin::new(Path::new("/state"), "/bin/tickloop".into());
        let registered = |root: &str| RegisteredPlugin {
            plugin_id: PLUGIN_ID.into(),
            plugin_root: root.into(),
            enabled: true,
            panes: vec![RegisteredPane {
                id: PANE_ID.into(),
                title: PANE_TITLE.into(),
                placement: POPUP_PLACEMENT.into(),
                width: Some(POPUP_SIZE.into()),
                height: Some(POPUP_SIZE.into()),
                command: vec!["/bin/tickloop".into(), "monitor".into(), "attach".into()],
            }],
        };
        assert_eq!(plugin.readiness(Vec::new()), PopupReadiness::NotInstalled);
        assert_eq!(
            plugin.readiness(vec![registered("/state/herdr-popup-plugin")]),
            PopupReadiness::Ready
        );
        assert_eq!(
            plugin.readiness(vec![registered("/elsewhere")]),
            PopupReadiness::Foreign {
                root: "/elsewhere".into()
            }
        );
        let mut switched_off = registered("/state/herdr-popup-plugin");
        switched_off.enabled = false;
        assert_eq!(
            plugin.readiness(vec![switched_off]),
            PopupReadiness::Disabled
        );
        assert!(plugin.is_registered_as_written(&registered("/state/herdr-popup-plugin")));
        let mut changed = registered("/state/herdr-popup-plugin");
        changed.panes[0].command[0] = "/tmp/other".into();
        assert!(!plugin.is_registered_as_written(&changed));
        let mut disabled = registered("/state/herdr-popup-plugin");
        disabled.enabled = false;
        assert!(!plugin.is_registered_as_written(&disabled));
    }
}
