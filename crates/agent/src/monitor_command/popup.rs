//! Tick Loop's Herdr popup plugin and the explicit commands that link it into, or unlink it
//! from, one Herdr session.
//!
//! The plugin is static: its only pane runs this executable's `monitor attach`, which reads the
//! view grant `plugin.pane.open` passes in `TICKLOOP_MONITOR_ATTACH`. Herdr copies a manifest
//! when it links a plugin and silently replaces a linked plugin that has the same id, so a
//! plugin under our id counts as ours only when Herdr reports our own directory as its root.

use std::{
    io::{self, BufRead, IsTerminal, Write},
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::{Value, json};
use tick_connect::herdr::{HerdrContext, HerdrPort, SocketHerdr};
use tokio_util::sync::CancellationToken;

use crate::private_fs::{
    PRIVATE_FILE_MODE, create_private_directory, current_uid, replace_file_atomically,
    verify_private_directory,
};
use crate::settings::{MonitorPopup, UserSettings};

const PLUGIN_ID: &str = "tickloop.monitor";
const PLUGIN_NAME: &str = "Tick Loop Monitor";
const PANE_ID: &str = "attach";
const PANE_TITLE: &str = "Tick Loop";
const POPUP_PLACEMENT: &str = "popup";
const POPUP_SIZE: &str = "85%";
const ATTACH_ARGUMENTS: [&str; 2] = ["monitor", "attach"];
const PLUGIN_DIRECTORY: &str = "herdr-popup-plugin";
const MANIFEST_FILE: &str = "herdr-plugin.toml";
const MIN_HERDR_VERSION: &str = "0.9.0";
const PLUGIN_UNLINK: &str = "plugin.unlink";

const INSTALL_PROMPT: &str = "这会在指定Herdr会话注册Tick Loop popup插件；不会启动、停止或替换Herdr/Tick服务。确认安装？[y/N]";
const REMOVE_PROMPT: &str = "这会从指定Herdr会话移除Tick Loop popup插件 tickloop.monitor，并把monitorPopup设为disabled；Monitor的Enter照旧显示文字snapshot详情，运行中的Tick继续运行。确认移除？[y/N]";
const NOTHING_CHANGED: &str = "未做任何更改。";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PopupChange {
    Install,
    Remove,
}

pub(super) async fn run(change: PopupChange, herdr_socket: &Path) -> Result<(), String> {
    let root = crate::terminal::config_root_directory()
        .ok_or("无法定位Tick Loop状态目录；请设置TICKLOOP_CONFIG_DIR。")?;
    let plugin = PopupPlugin::for_this_executable(&root)?;
    let herdr = HerdrSession::verify(herdr_socket).await?;
    let settings = UserSettings::in_root(&root);
    match change {
        PopupChange::Install => install(&herdr, &plugin, &settings).await,
        PopupChange::Remove => remove(&herdr, &plugin, &settings).await,
    }
}

async fn install(
    herdr: &HerdrSession,
    plugin: &PopupPlugin,
    settings: &UserSettings,
) -> Result<(), String> {
    herdr.registration_without_foreign(plugin).await?;
    confirm(INSTALL_PROMPT)?;
    let directory = plugin.write()?;
    herdr.registration_without_foreign(plugin).await?;
    let linked = herdr
        .request("plugin.link", json!({"path": directory, "enabled": true}))
        .await?;
    let registered = serde_json::from_value::<RegisteredPlugin>(linked["plugin"].clone());
    if !registered.is_ok_and(|registered| plugin.is_registered_as_written(&registered)) {
        herdr
            .request(PLUGIN_UNLINK, json!({"plugin_id": PLUGIN_ID}))
            .await?;
        settings
            .save_monitor_popup(MonitorPopup::Disabled)
            .map_err(|error| {
                format!("已撤销不一致的注册，但无法把monitorPopup设为disabled：{error}")
            })?;
        return Err(format!(
            "Herdr注册的 {PLUGIN_ID} 与Tick Loop写入的插件不一致，已撤销注册并把monitorPopup设为disabled。"
        ));
    }
    settings
        .save_monitor_popup(MonitorPopup::Enabled)
        .map_err(|error| format!("插件已注册，但无法把monitorPopup设为enabled：{error}"))?;
    println!(
        "已在 {} 注册Tick Loop popup插件 {PLUGIN_ID}；monitorPopup已设为enabled。",
        herdr.socket.display()
    );
    Ok(())
}

async fn remove(
    herdr: &HerdrSession,
    plugin: &PopupPlugin,
    settings: &UserSettings,
) -> Result<(), String> {
    herdr.own_registration(plugin).await?;
    confirm(REMOVE_PROMPT)?;
    herdr.own_registration(plugin).await?;
    let unlinked = herdr
        .request(PLUGIN_UNLINK, json!({"plugin_id": PLUGIN_ID}))
        .await?;
    if unlinked["removed"] != true {
        return Err(format!("Herdr没有移除 {PLUGIN_ID}；monitorPopup未改变。"));
    }
    settings
        .save_monitor_popup(MonitorPopup::Disabled)
        .map_err(|error| format!("插件已移除，但无法把monitorPopup设为disabled：{error}"))?;
    println!(
        "已从 {} 移除Tick Loop popup插件 {PLUGIN_ID}；monitorPopup已设为disabled，运行中的Tick继续运行。",
        herdr.socket.display()
    );
    Ok(())
}

/// Asks `prompt` on this terminal; only an explicit yes goes ahead.
fn confirm(prompt: &str) -> Result<(), String> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return Err(format!("需要在交互终端中确认；{NOTHING_CHANGED}"));
    }
    print!("{prompt} ");
    io::stdout().flush().map_err(|error| error.to_string())?;
    match read_consent(&mut stdin.lock()) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("已取消；{NOTHING_CHANGED}")),
        Err(error) => Err(format!("无法读取确认（{error}）；{NOTHING_CHANGED}")),
    }
}

/// Reads one answer line: only `y` or `yes`, in any case, consents. An empty line or EOF is No.
fn read_consent(input: &mut impl BufRead) -> io::Result<bool> {
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
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

#[derive(Debug, PartialEq, Eq)]
enum Registration {
    Absent,
    Ours,
    Foreign { root: PathBuf },
}

/// Tick Loop's popup plugin: a private directory under the state root whose manifest runs this
/// executable's attach client and nothing else.
struct PopupPlugin {
    state_root: PathBuf,
    pane: RegisteredPane,
}

impl PopupPlugin {
    fn for_this_executable(state_root: &Path) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|error| format!("无法确定tickloop可执行文件路径：{error}"))?
            .into_os_string()
            .into_string()
            .map_err(|_| "tickloop可执行文件路径不是UTF-8，Herdr无法运行它。".to_owned())?;
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
            .map_err(|error| format!("无法写入Tick Loop popup插件：{error}"))?;
        Ok(self.directory())
    }

    fn registration(&self, registered: Vec<RegisteredPlugin>) -> Registration {
        match registered
            .into_iter()
            .find(|registered| registered.plugin_id == PLUGIN_ID)
        {
            None => Registration::Absent,
            Some(registered) if registered.plugin_root == self.directory() => Registration::Ours,
            Some(registered) => Registration::Foreign {
                root: registered.plugin_root,
            },
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
struct HerdrSession {
    socket: PathBuf,
    api: SocketHerdr,
}

impl HerdrSession {
    async fn verify(socket: &Path) -> Result<Self, String> {
        let metadata = std::fs::symlink_metadata(socket)
            .map_err(|error| format!("无法访问Herdr socket {}：{error}", socket.display()))?;
        if !metadata.file_type().is_socket() {
            return Err(format!("{} 不是Herdr socket。", socket.display()));
        }
        if metadata.uid() != current_uid() {
            return Err(format!(
                "Herdr socket {} 不属于当前用户。",
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
            .ok_or_else(|| format!("{} 上的服务没有以Herdr ping应答。", socket.display()))?;
        if !is_supported_herdr(version) {
            return Err(format!(
                "Herdr {version} 不支持Tick Loop popup插件；需要 {MIN_HERDR_VERSION} 或更高版本。"
            ));
        }
        Ok(session)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.api
            .request(method, params, CancellationToken::new())
            .await
            .map_err(|error| format!("Herdr {method} 失败（{error}）。"))
    }

    async fn registration(&self, plugin: &PopupPlugin) -> Result<Registration, String> {
        let listed = self
            .request("plugin.list", json!({"plugin_id": PLUGIN_ID}))
            .await?;
        let registered = serde_json::from_value(listed["plugins"].clone())
            .map_err(|error| format!("无法解析Herdr插件列表：{error}"))?;
        Ok(plugin.registration(registered))
    }

    /// The registration under our id, refusing one that Tick Loop did not write.
    async fn registration_without_foreign(
        &self,
        plugin: &PopupPlugin,
    ) -> Result<Registration, String> {
        match self.registration(plugin).await? {
            Registration::Foreign { root } => Err(foreign_plugin(&root)),
            registration => Ok(registration),
        }
    }

    async fn own_registration(&self, plugin: &PopupPlugin) -> Result<(), String> {
        match self.registration(plugin).await? {
            Registration::Ours => Ok(()),
            Registration::Absent => Err(format!(
                "{} 没有注册Tick Loop popup插件 {PLUGIN_ID}；{NOTHING_CHANGED}",
                self.socket.display()
            )),
            Registration::Foreign { root } => Err(foreign_plugin(&root)),
        }
    }
}

fn foreign_plugin(root: &Path) -> String {
    format!(
        "Herdr已注册的 {PLUGIN_ID} 来自 {}，不是Tick Loop安装的插件，不会被替换或移除；{NOTHING_CHANGED}",
        root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_yes_consents() {
        for (answer, consents) in [
            ("y\n", true),
            ("yes\n", true),
            ("Y\n", true),
            ("YES\r\n", true),
            (" y \n", true),
            ("y", true),
            ("\n", false),
            ("", false),
            ("n\n", false),
            ("no\n", false),
            ("yess\n", false),
            ("ok\n", false),
            ("y y\n", false),
        ] {
            assert_eq!(
                read_consent(&mut io::Cursor::new(answer)).unwrap(),
                consents,
                "{answer:?}"
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
    fn a_plugin_under_our_id_is_ours_only_from_our_directory() {
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
        assert_eq!(plugin.registration(Vec::new()), Registration::Absent);
        assert_eq!(
            plugin.registration(vec![registered("/state/herdr-popup-plugin")]),
            Registration::Ours
        );
        assert_eq!(
            plugin.registration(vec![registered("/elsewhere")]),
            Registration::Foreign {
                root: "/elsewhere".into()
            }
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
