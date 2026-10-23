use crate::support;
use std::{fs, path::Path};

fn command(root: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_tickloop"));
    support::cli_runtime::adopt(&root.join("state"));
    fs::create_dir_all(root.join("home")).unwrap();
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("TICKLOOP_CONFIG_DIR", root.join("state"))
        .current_dir(root);
    command
}

#[test]
fn startup_rejects_removed_config_argument_before_loading_its_file() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let path = root.join("provider.yaml");
    fs::write(&path, "id: deepseek-api\nmodels:\n  - id: deepseek-flash\n").unwrap();
    let original = fs::read(&path).unwrap();
    let result = command(&root)
        .args(["--working-directory-is-trusted", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !result.status.success(),
        "Removed --config must be rejected instead of loading a definition"
    );
    assert!(
        text.contains("built-in") && text.contains("not supported"),
        "{text}"
    );
    assert_eq!(fs::read(path).unwrap(), original);
}
