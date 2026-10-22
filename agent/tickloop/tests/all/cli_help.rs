use std::{path::Path, process::Stdio, time::Duration};

async fn invoke(root: &Path, arguments: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tickloop"));
    command
        .args(arguments)
        .current_dir(root)
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TICKLOOP_CONFIG_DIR", root.join("config"))
        .env("TICKLOOP_DAEMON_RUNTIME_DIR", root.join("daemon"))
        .env("TICKLOOP_TICKHOST_RUNTIME_DIR", root.join("tickhost"))
        .env_remove("HERDR_SOCKET")
        .env("NO_COLOR", "1")
        .env("CLICOLOR_FORCE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("help must not wait for input or start a service")
        .unwrap()
}

#[tokio::test]
async fn root_help_aliases_are_identical_and_create_no_runtime_or_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let mut previous = None;
    for argument in ["-h", "--help"] {
        let output = invoke(directory.path(), &[argument]).await;
        assert!(output.status.success(), "{:?}", output);
        assert!(output.stderr.is_empty(), "{:?}", output);
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(
            help.starts_with("Tickloop - terminal AI agent\n\nUsage:"),
            "{help}"
        );
        assert!(
            help.contains("Commands:") && help.contains("Permission options:"),
            "{help}"
        );
        assert!(help.contains("tickloop -p --help"), "{help}");
        assert!(!help.contains("Commands and Skills:"), "{help}");
        assert!(!help.contains('\x1b'), "{help}");
        if let Some(previous) = previous {
            assert_eq!(help, previous);
        }
        previous = Some(help);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn root_help_does_not_need_a_readable_settings_directory() {
    let directory = tempfile::tempdir().unwrap();
    let blocked_config = directory.path().join("config");
    std::fs::write(&blocked_config, "not a directory or provider configuration").unwrap();
    let output = invoke(directory.path(), &["--help"]).await;
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stderr.is_empty(), "{:?}", output);
    assert!(!output.stdout.is_empty());
    assert_eq!(
        std::fs::read_to_string(blocked_config).unwrap(),
        "not a directory or provider configuration"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn help_is_not_intercepted_inside_values_or_before_adapter_validation() {
    let directory = tempfile::tempdir().unwrap();
    for (arguments, code, error) in [
        (
            vec!["--cwd", "--help"],
            1,
            "cannot use working directory --help",
        ),
        (vec!["--cwd", "-p"], 1, "cannot use working directory -p"),
        (vec!["--", "--help"], 1, "unknown argument --"),
        (vec!["--unknown", "--help"], 1, "unknown argument --unknown"),
        (
            vec!["-p", "--help", "--output-format", "bad"],
            2,
            "--output-format requires",
        ),
        (vec!["--p", "--help"], 2, "use -p or --print, not --p"),
        (
            vec!["acp", "--help", "--unknown"],
            1,
            "unknown ACP argument",
        ),
    ] {
        let output = invoke(directory.path(), &arguments).await;
        assert_eq!(
            output.status.code(),
            Some(code),
            "{arguments:?}: {output:?}"
        );
        assert!(output.stdout.is_empty(), "{arguments:?}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(error), "{arguments:?}: {stderr}");
        assert!(!stderr.contains('\x1b'), "{arguments:?}: {stderr}");
    }
}
