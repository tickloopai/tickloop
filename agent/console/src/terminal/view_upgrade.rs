use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tickhost_connect::executable_lease::{self, ExecutableLease};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::console::{Console, ConsoleUpgrade, InputEvent};

const HANDOFF_SCHEMA: u16 = 2;
const MAX_HANDOFF_BYTES: u64 = 64 * 1024 * 1024;
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);
static PREDECESSOR_LEASE: Mutex<Option<ExecutableLease>> = Mutex::new(None);
static FAILED_SUCCESSOR: Mutex<Option<PathBuf>> = Mutex::new(None);
static HANDOFF_COMPLETED: AtomicBool = AtomicBool::new(false);
static HANDOFF_PENDING: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize)]
#[serde(remote = "crossterm::terminal::RawModeState")]
struct RawModeState {
    input_flags: u64,
    output_flags: u64,
    control_flags: u64,
    local_flags: u64,
    minimum_read: u8,
    read_timeout: u8,
}

#[derive(Serialize, Deserialize)]
pub(super) struct ViewSelection {
    pub navigation: crate::agent_console::UpgradeNavigation,
    pub chat: Option<Uuid>,
    pub goal: Option<Uuid>,
    pub overview: bool,
    pub selected_loops: HashMap<Uuid, Option<Uuid>>,
    pub configuration: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
pub(super) struct ViewHandoff {
    schema: u16,
    process: u32,
    pub selection: ViewSelection,
    pub console: ConsoleUpgrade,
    #[serde(with = "RawModeState")]
    raw_mode: crossterm::terminal::RawModeState,
    predecessor: PathBuf,
    predecessor_device: u64,
    predecessor_inode: u64,
    predecessor_lease: Option<i32>,
    successor: PathBuf,
    recovery_attempted: bool,
    #[serde(skip)]
    pub source_path: PathBuf,
}

fn load_handoff(path: &Path, process: u32) -> Result<ViewHandoff, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("Cannot read View handoff: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() > MAX_HANDOFF_BYTES
    {
        return Err("View handoff must be a bounded private file owned by this user".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_HANDOFF_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_HANDOFF_BYTES {
        return Err("View handoff exceeds its size limit".into());
    }
    let handoff: ViewHandoff =
        serde_json::from_slice(&bytes).map_err(|error| format!("Invalid View handoff: {error}"))?;
    if handoff.schema != HANDOFF_SCHEMA || handoff.process != process {
        return Err("View handoff schema or process does not match".into());
    }
    handoff.console.validate()?;
    handoff
        .raw_mode
        .validate()
        .map_err(|error| error.to_string())?;
    serde_json::from_value::<super::new_agent_draft::NewAgentDraft>(
        handoff.selection.configuration.clone(),
    )
    .map_err(|error| format!("Invalid saved View configuration: {error}"))?;
    Ok(handoff)
}

pub(super) fn read_handoff(path: &Path) -> Result<ViewHandoff, String> {
    let mut handoff = load_handoff(path, std::process::id())?;
    if handoff.recovery_attempted {
        *FAILED_SUCCESSOR.lock().map_err(|error| error.to_string())? =
            Some(handoff.successor.clone());
    }
    handoff.source_path = path.to_owned();
    Ok(handoff)
}

fn handoff_argument(arguments: &[OsString]) -> Option<&Path> {
    arguments
        .windows(2)
        .find(|pair| pair[0] == "--view-handoff")
        .map(|pair| Path::new(&pair[1]))
}

pub(super) fn prepare_handoff(arguments: &[OsString]) -> Result<(), String> {
    let Some(path) = handoff_argument(arguments) else {
        return Ok(());
    };
    let handoff = load_handoff(path, std::process::id())?;
    let mut retained = PREDECESSOR_LEASE
        .lock()
        .map_err(|error| error.to_string())?;
    if let Some(descriptor) = handoff.predecessor_lease
        && retained.is_none()
    {
        let directory = handoff
            .predecessor
            .parent()
            .ok_or("Predecessor has no bundle directory")?;
        *retained = Some(unsafe {
            ExecutableLease::from_inherited(
                descriptor,
                directory,
                &executable_lease::lease_directory().map_err(|error| error.to_string())?,
            )
            .map_err(|error| format!("Cannot retain predecessor View: {error}"))?
        });
    }
    crossterm::terminal::import_raw_mode_state(&handoff.raw_mode)
        .map_err(|error| error.to_string())?;
    HANDOFF_PENDING.store(true, Ordering::Release);
    Ok(())
}

pub(super) fn handoff_in_progress() -> bool {
    HANDOFF_PENDING.load(Ordering::Acquire)
}

pub(super) fn complete_handoff() {
    HANDOFF_PENDING.store(false, Ordering::Release);
    HANDOFF_COMPLETED.store(true, Ordering::Release);
    if let Ok(mut retained) = PREDECESSOR_LEASE.lock() {
        retained.take();
    }
}

pub(super) fn recover_handoff(arguments: &[OsString]) -> Result<bool, String> {
    if HANDOFF_COMPLETED.load(Ordering::Acquire) {
        return Ok(false);
    }
    let Some(path) = handoff_argument(arguments) else {
        return Ok(false);
    };
    struct RestoreTerminal;
    impl Drop for RestoreTerminal {
        fn drop(&mut self) {
            if HANDOFF_PENDING.swap(false, Ordering::AcqRel) {
                let _ = crossterm::terminal::disable_raw_mode();
            }
        }
    }
    let _restore_terminal = RestoreTerminal;
    let mut handoff = load_handoff(path, std::process::id())?;
    if handoff.recovery_attempted {
        return Ok(false);
    }
    let metadata =
        std::fs::symlink_metadata(&handoff.predecessor).map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || (metadata.dev(), metadata.ino())
            != (handoff.predecessor_device, handoff.predecessor_inode)
    {
        return Err("The predecessor View executable changed during activation".into());
    }
    if !handoff_in_progress() {
        prepare_handoff(arguments)?;
    }
    let retained = PREDECESSOR_LEASE
        .lock()
        .map_err(|error| error.to_string())?;
    if handoff.predecessor_lease.is_some() && retained.is_none() {
        return Err("The predecessor View lease is unavailable".into());
    }
    if let Some(lease) = retained.as_ref() {
        lease
            .make_inheritable()
            .map_err(|error| error.to_string())?;
        handoff.predecessor_lease = Some(lease.raw_descriptor());
    }
    handoff.recovery_attempted = true;
    let parent = path.parent().ok_or("View handoff has no parent")?;
    let mut updated = tempfile::Builder::new()
        .prefix(".view-recovery-")
        .tempfile_in(parent)
        .map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut updated, &handoff).map_err(|error| error.to_string())?;
    updated
        .flush()
        .and_then(|()| updated.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    updated.persist(path).map_err(|error| error.to_string())?;
    let error = std::process::Command::new(&handoff.predecessor)
        .args(arguments)
        .exec();
    Err(format!("Cannot restore the predecessor View: {error}"))
}

pub(super) fn check_handoff(path: &Path) -> Result<(), String> {
    load_handoff(path, unsafe { libc::getppid() } as u32).map(|_| ())
}

pub(super) async fn try_reexec(
    console: &Console,
    inputs: &mpsc::Receiver<InputEvent>,
    executable: &Path,
    arguments: Vec<OsString>,
    selection: ViewSelection,
) -> Result<bool, String> {
    if FAILED_SUCCESSOR
        .lock()
        .map_err(|error| error.to_string())?
        .as_deref()
        == Some(executable)
    {
        return Err("The new View could not start; keeping this View until another update".into());
    }
    let Some(mut pause) = console.pause_for_upgrade().await? else {
        return Ok(false);
    };
    if !inputs.is_empty() {
        return Ok(false);
    }
    let predecessor = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| error.to_string())?;
    let predecessor_metadata =
        std::fs::metadata(&predecessor).map_err(|error| error.to_string())?;
    let predecessor_lease =
        executable_lease::retain_current_executable().map_err(|error| error.to_string())?;
    let handoff = ViewHandoff {
        schema: HANDOFF_SCHEMA,
        process: std::process::id(),
        selection,
        console: pause.snapshot()?,
        raw_mode: crossterm::terminal::export_raw_mode_state()
            .map_err(|error| error.to_string())?,
        predecessor,
        predecessor_device: predecessor_metadata.dev(),
        predecessor_inode: predecessor_metadata.ino(),
        predecessor_lease: predecessor_lease
            .as_ref()
            .map(ExecutableLease::raw_descriptor),
        successor: executable.to_owned(),
        recovery_attempted: false,
        source_path: PathBuf::new(),
    };
    let bytes = serde_json::to_vec(&handoff).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_HANDOFF_BYTES {
        return Err("View state is too large to transfer automatically".into());
    }
    let mut file = tempfile::Builder::new()
        .prefix("tickloop-view-handoff-")
        .tempfile()
        .map_err(|error| error.to_string())?;
    file.write_all(&bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    let mut child = tokio::process::Command::new(executable)
        .arg("--check-view-handoff")
        .arg(file.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Cannot preflight the new View: {error}"))?;
    let status = match tokio::time::timeout(PREFLIGHT_TIMEOUT, child.wait()).await {
        Ok(status) => status.map_err(|error| error.to_string())?,
        Err(_) => {
            let _ = child.kill().await;
            return Err("The new View did not confirm it can restore this terminal".into());
        }
    };
    if !status.success() {
        return Err("The new View could not restore the current editor state".into());
    }
    if let Some(lease) = predecessor_lease.as_ref() {
        lease
            .make_inheritable()
            .map_err(|error| error.to_string())?;
    }
    pause.release_terminal()?;
    let mut retained = Vec::new();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        if argument == "--view-handoff" {
            arguments.next();
        } else {
            retained.push(argument);
        }
    }
    let error = std::process::Command::new(executable)
        .args(retained)
        .arg("--view-handoff")
        .arg(file.path())
        .exec();
    Err(format!("Cannot activate the new View: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn saved_view() -> ViewHandoff {
        let chat = Uuid::new_v4();
        let console = Console::test_for_chat(chat);
        ViewHandoff {
            schema: HANDOFF_SCHEMA,
            process: std::process::id(),
            selection: ViewSelection {
                navigation: crate::agent_console::UpgradeNavigation {
                    directory: PathBuf::from("/fixture"),
                    filter: tickloop_agent_protocol::AgentFilter::default(),
                },
                chat: Some(chat),
                goal: None,
                overview: false,
                selected_loops: HashMap::new(),
                configuration: serde_json::to_value(
                    super::super::new_agent_draft::NewAgentDraft::default(),
                )
                .unwrap(),
            },
            console: console.test_upgrade_snapshot(),
            raw_mode: crossterm::terminal::RawModeState::default(),
            predecessor: PathBuf::new(),
            predecessor_device: 0,
            predecessor_inode: 0,
            predecessor_lease: None,
            successor: PathBuf::new(),
            recovery_attempted: false,
            source_path: PathBuf::new(),
        }
    }

    #[test]
    fn view_handoff_requires_a_private_regular_file_and_matching_process_and_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state");
        let saved = saved_view();
        std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let restored = read_handoff(&path).unwrap();
        assert_eq!(restored.selection.chat, saved.selection.chat);
        assert_eq!(restored.source_path, path);
        assert!(load_handoff(&path, std::process::id() + 1).is_err());
        let linked = directory.path().join("linked");
        symlink(&path, &linked).unwrap();
        assert!(read_handoff(&linked).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_handoff(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut value = serde_json::to_value(saved).unwrap();
        value["schema"] = serde_json::json!(HANDOFF_SCHEMA + 1);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_handoff(&path).is_err());
    }

    #[test]
    fn view_handoff_rejects_invalid_utf8_cursor_before_replacing_the_process() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state");
        let mut value = serde_json::to_value(saved_view()).unwrap();
        value["console"]["current"]["editor"]["draft"] = "中文".into();
        value["console"]["current"]["editor"]["cursor"] = 1.into();
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_handoff(&path).is_err());
    }

    #[test]
    fn view_recovery_is_bounded_and_refuses_a_changed_predecessor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state");
        let mut saved = saved_view();
        saved.recovery_attempted = true;
        std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let arguments = vec![
            OsString::from("--view-handoff"),
            path.as_os_str().to_owned(),
        ];
        assert!(!recover_handoff(&arguments).unwrap());
        saved.recovery_attempted = false;
        saved.predecessor = std::env::current_exe().unwrap();
        let metadata = std::fs::metadata(&saved.predecessor).unwrap();
        saved.predecessor_device = metadata.dev();
        saved.predecessor_inode = metadata.ino() + 1;
        std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        assert!(
            recover_handoff(&arguments)
                .unwrap_err()
                .contains("changed during activation")
        );
    }
}
