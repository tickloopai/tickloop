use std::path::Path;
use std::process::{Command, Stdio};

#[cfg(target_os = "linux")]
const SHA256_COMMAND: (&str, &[&str]) = ("/usr/bin/sha256sum", &[]);
#[cfg(not(target_os = "linux"))]
const SHA256_COMMAND: (&str, &[&str]) = ("/usr/bin/shasum", &["-a", "256"]);

pub fn unpack(
    archive: &Path,
    checksum: &str,
    target: &str,
    directory: &Path,
) -> Result<(), String> {
    let fields = checksum.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 2
        || fields[0].len() != 64
        || !fields[0].bytes().all(|byte| byte.is_ascii_hexdigit())
        || fields[1].trim_start_matches('*') != format!("tickloop-{target}.tar.gz")
    {
        return Err("Invalid release checksum file.".into());
    }
    if !sha256(archive)?.eq_ignore_ascii_case(fields[0]) {
        return Err("Release SHA-256 mismatch; the existing installation was not replaced.".into());
    }
    let entries = output(Command::new("/usr/bin/tar").arg("-tzf").arg(archive))?;
    let mut names = entries.lines().collect::<Vec<_>>();
    names.sort_unstable();
    if names != ["tickhost", "tickloop"] {
        return Err("The release archive contains unexpected or missing files.".into());
    }
    let modes = output(Command::new("/usr/bin/tar").arg("-tvzf").arg(archive))?;
    if modes.lines().count() != 2 || modes.lines().any(|line| !line.starts_with('-')) {
        return Err("The release archive must contain only regular programs.".into());
    }
    let status = Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(directory)
        .stdin(Stdio::null())
        .status()
        .map_err(display)?;
    if !status.success() {
        return Err("Cannot extract the verified release.".into());
    }
    Ok(())
}

pub fn sha256(file: &Path) -> Result<String, String> {
    let (program, arguments) = SHA256_COMMAND;
    let digest = output(Command::new(program).args(arguments).arg(file))?;
    digest
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "The SHA-256 command printed no digest.".into())
}

pub fn output(command: &mut Command) -> Result<String, String> {
    let result = command.stdin(Stdio::null()).output().map_err(display)?;
    if !result.status.success() {
        return Err(format!("Release validation failed: {}", result.status));
    }
    String::from_utf8(result.stdout).map_err(display)
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}
