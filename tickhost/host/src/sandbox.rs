//! Converts an approved execution policy into the local operating-system sandbox.
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const DENIAL_SCAN_BYTES: u64 = 64 * 1024;
const DENIAL_NOTE: &str = "被沙箱拦截（根据权限拒绝输出推测）";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxPolicy {
    pub writable_roots: Vec<PathBuf>,
    pub deny_write: Vec<PathBuf>,
    pub protected_paths: Vec<PathBuf>,
    pub denied_path_patterns: Vec<ProtectedPathPattern>,
    pub network: SandboxNetwork,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProtectedPathPattern {
    pub pattern: String,
    pub excluded_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxNetwork {
    Disconnected,
}

pub async fn availability() -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("sandbox unavailable on this platform".into());
    }
    #[cfg(target_os = "macos")]
    {
        #[link(name = "sandbox")]
        unsafe extern "C" {
            fn sandbox_check(
                pid: libc::pid_t,
                operation: *const libc::c_char,
                filter: libc::c_int,
                ...
            ) -> libc::c_int;
        }
        // A null operation queries membership, including profiles that permit another sandbox.
        if unsafe { sandbox_check(libc::getpid(), std::ptr::null(), 0) } != 0 {
            return Err("sandbox unavailable: process is already sandboxed".into());
        }
    }
    let output = tokio::process::Command::new(SANDBOX_EXEC)
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .output()
        .await
        .map_err(|error| format!("sandbox unavailable: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "sandbox unavailable (possibly already sandboxed): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub(crate) fn command(policy: &SandboxPolicy, shell: &Path) -> io::Result<tokio::process::Command> {
    if !cfg!(target_os = "macos") {
        return Err(io::Error::other("sandbox unavailable on this platform"));
    }
    let mut profile = include_str!("sandbox_base.sbpl").to_owned();
    // F_MAKECOMPRESSED (80) and F_TRANSFEREXTENTS (110) can mutate through read-only descriptors.
    profile.push_str("\n(allow file-read*)\n(deny system-fcntl (fcntl-command 80 110))\n");
    match policy.network {
        SandboxNetwork::Disconnected => profile.push_str("(deny network*)\n"),
    }
    for root in &policy.writable_roots {
        let path = quote_path(root)?;
        profile.push_str(&format!("(allow file-write* (subpath {path}))\n"));
        profile.push_str(&format!("(deny file-write-unlink (literal {path}))\n"));
    }
    for path in &policy.protected_paths {
        profile.push_str(&format!(
            "(deny file-write* (literal {}))\n",
            quote_path(path)?
        ));
    }
    for path in &policy.deny_write {
        let quoted = quote_path(path)?;
        profile.push_str(&format!(
            "(deny file-write* (literal {quoted}) (subpath {quoted}))\n(deny file-read-metadata (require-all (vnode-type SYMLINK) (require-any (literal {quoted}) (subpath {quoted}))))\n"
        ));
        // Ancestors cannot be renamed or traversed as symlinks, even if created after preparation.
        for ancestor in path.ancestors().skip(1) {
            let ancestor = quote_path(ancestor)?;
            profile.push_str(&format!(
                "(deny file-write-unlink (literal {ancestor}))\n(deny file-read-metadata (require-all (vnode-type SYMLINK) (literal {ancestor})))\n"
            ));
        }
    }
    for denied in &policy.denied_path_patterns {
        let pattern = &denied.pattern;
        if pattern.chars().any(char::is_control) {
            return Err(io::Error::other("unsupported protected-path pattern"));
        }
        // SBPL regex literals preserve backslashes; only quotes need escaping.
        let mut filters = format!("(regex #\"{}\")", pattern.replace('"', "\\\""));
        for excluded in &denied.excluded_roots {
            let path = quote_path(excluded)?;
            filters.push_str(&format!(
                " (require-not (literal {path})) (require-not (subpath {path}))"
            ));
        }
        // Metadata checks run before following a symlink; write checks see only its final target.
        profile.push_str(&format!("(deny file-write* (require-all {filters}))\n(deny file-read-metadata (require-all (vnode-type SYMLINK) {filters}))\n"));
    }
    let mut command = tokio::process::Command::new(SANDBOX_EXEC);
    command.args(["-p", &profile]).arg(shell);
    Ok(command)
}

fn quote_path(path: &Path) -> io::Result<String> {
    if !path.is_absolute() {
        return Err(io::Error::other("sandbox paths must be absolute"));
    }
    path.to_str()
        .filter(|path| !path.chars().any(char::is_control))
        .map(quote)
        .ok_or_else(|| io::Error::other("unsupported sandbox path"))
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(crate) fn annotate_denial(output: &std::fs::File) {
    use std::{io::Write, os::unix::fs::FileExt};
    let Ok(length) = output.metadata().map(|metadata| metadata.len()) else {
        return;
    };
    let mut bytes = vec![0; length.min(DENIAL_SCAN_BYTES) as usize];
    let offset = length.saturating_sub(bytes.len() as u64);
    let Ok(read) = output.read_at(&mut bytes, offset) else {
        return;
    };
    let text = String::from_utf8_lossy(&bytes[..read]).to_ascii_lowercase();
    if [
        "operation not permitted",
        "permission denied",
        "operation not allowed",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
    {
        let _ = writeln!(&mut &*output, "\n{DENIAL_NOTE}");
    }
}
