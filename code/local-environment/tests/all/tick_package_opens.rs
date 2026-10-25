//! A TickManage open of an installed Tick package's program, prepared as an Agent's call is; the
//! running Tick is covered by `code/cli`'s hosted tests.

use tickloop_harness::runtime::host_tools::WorkspacePackageLogin;

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use serde_json::json;
use tick_packages::{InstalledPackage, MANIFEST_FILE_NAME, PackageStore};
use tickloop_code_local_environment::hosted::{
    tools::HostedTickTools,
    worker_control::{DaemonControl, WorkerControlGrant},
};
use tickloop_harness::runtime::host_tools::{HostToolProvider, WorkspaceTickPackage};
use tokio_util::sync::CancellationToken;

/// Hosted Tick tools whose daemon is never reached: preparing an open asks nothing of it.
fn tools(directory: &Path, packages: PackageStore) -> HostedTickTools {
    HostedTickTools::new(
        DaemonControl::new(WorkerControlGrant {
            endpoint: directory.join("unused-daemon.sock"),
            daemon_epoch: uuid::Uuid::new_v4(),
            binding: uuid::Uuid::new_v4(),
            capability: "unused".into(),
            host_endpoint: directory.join("unused-host.sock"),
        }),
        Vec::new(),
    )
    .with_tick_packages(packages)
}

/// Installs the package `name`, whose program runs the example counter Tick and whose manifest
/// says whether it speaks the Tick protocol.
fn install(
    directory: &Path,
    packages: &PackageStore,
    name: &str,
    protocol: bool,
) -> InstalledPackage {
    let counter = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tick/examples/counter.py")
        .canonicalize()
        .unwrap();
    let source = directory.join("sources").join(name);
    fs::create_dir_all(source.join("bin")).unwrap();
    fs::write(
        source.join(MANIFEST_FILE_NAME),
        json!({
            "name": name,
            "version": "1.0.0",
            "description": "The example counter",
            "executable": format!("bin/tick-{name}"),
            "protocol": protocol,
        })
        .to_string(),
    )
    .unwrap();
    let program = source.join("bin").join(format!("tick-{name}"));
    fs::write(
        &program,
        format!(
            "#!/bin/sh\nexec /usr/bin/python3 -u '{}' \"$@\"\n",
            counter.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    packages.install_path(&source).unwrap().package
}

async fn prepare_open(
    tools: &HostedTickTools,
    command: &[&str],
    cwd: &Path,
) -> Result<tickloop_harness::runtime::host_tools::PreparedHostCall, String> {
    tools
        .prepare_call(
            tickloop_ids::Id::from(uuid::Uuid::new_v4()),
            "TickManage",
            &json!({"action":"open","command":command,"cwd":cwd}),
            CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn a_package_program_that_declares_the_protocol_opens_as_a_protocol_tick_unasked() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let packages = PackageStore::in_config_root(&cwd.join("config"));
    let counter = install(&cwd, &packages, "counter", true);
    install(&cwd, &packages, "plain", false);
    let tools = tools(&cwd, packages);

    let prepared = prepare_open(&tools, &["tick-counter"], &cwd).await.unwrap();
    assert_eq!(prepared.input()["protocol"], true);
    assert_eq!(
        prepared.approval_summary(),
        format!(
            "Open `tick-counter` from Tick package counter 1.0.0 in the Tickhost as an extension Tick (cwd {})",
            cwd.display()
        )
    );
    let by_path = prepare_open(&tools, &[counter.executable().to_str().unwrap()], &cwd)
        .await
        .unwrap();
    assert_eq!(by_path.input()["protocol"], true);

    let plain = prepare_open(&tools, &["tick-plain"], &cwd).await.unwrap();
    assert_eq!(plain.input()["protocol"], false);
    assert!(
        plain.approval_summary().ends_with(&format!(
            "from Tick package plain 1.0.0 in the Tickhost as a terminal Tick (cwd {})",
            cwd.display()
        )),
        "{}",
        plain.approval_summary()
    );

    let context = tools.workspace_host_context().unwrap();
    assert_eq!(
        context.tick_packages,
        [
            WorkspaceTickPackage {
                name: "counter".into(),
                version: "1.0.0".into(),
                description: "The example counter".into(),
                executable: counter.executable(),
                login: WorkspacePackageLogin::NotRequired,
            },
            WorkspaceTickPackage {
                name: "plain".into(),
                version: "1.0.0".into(),
                description: "The example counter".into(),
                executable: cwd.join("config/ticks/plain/bin/tick-plain"),
                login: WorkspacePackageLogin::NotRequired,
            },
        ]
    );
}

#[tokio::test]
async fn opening_the_program_of_a_package_that_is_not_installed_names_the_install_command() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let packages = PackageStore::in_config_root(&cwd.join("config"));
    install(&cwd, &packages, "counter-sample", true);
    packages.uninstall("counter-sample").unwrap();
    let tools = tools(&cwd, packages);

    let error = prepare_open(&tools, &["tick-counter-sample", "--start", "1"], &cwd)
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "InvalidArguments: executable not found: tick-counter-sample comes from Tick package counter-sample, which is not installed; the user installs it from /ticks in the Agent (or `tickloop tick install counter-sample` in a terminal)"
    );
    assert!(
        tools
            .workspace_host_context()
            .unwrap()
            .tick_packages
            .is_empty()
    );
}
