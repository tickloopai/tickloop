use std::path::Path;
use std::process::{Command, Stdio};

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
    let actual = output(
        Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .arg(archive),
    )?;
    if actual
        .split_whitespace()
        .next()
        .is_none_or(|value| !value.eq_ignore_ascii_case(fields[0]))
    {
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
