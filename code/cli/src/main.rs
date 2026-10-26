use std::io::{self, IsTerminal, Write};

use crossterm::style::Stylize;

const TERMINAL_WORKER_THREADS: usize = 2;

fn main() {
    let mut arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let build = tickloop_code_updater::Build {
        version: env!("CARGO_PKG_VERSION").into(),
        identity: env!("TICKLOOP_BUILD_IDENTITY").into(),
    };
    let _development_lease = if arguments
        .first()
        .is_some_and(|argument| argument == "--development-root")
    {
        let root = arguments
            .get(1)
            .cloned()
            .unwrap_or_else(|| fail_startup(&arguments, "Missing development checkout"));
        let lease =
            tickloop_code_updater::register_development(std::path::Path::new(&root), &build)
                .unwrap_or_else(|error| {
                    fail_startup(
                        &arguments,
                        &format!("Cannot retain development runtime: {error:#}"),
                    )
                });
        arguments.drain(..2);
        Some(lease)
    } else {
        None
    };
    if let [command, path] = arguments.as_slice()
        && command == "--check-view-handoff"
    {
        if let Err(error) =
            tickloop_code_console::terminal::check_view_handoff(std::path::Path::new(path))
        {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    if arguments.len() == 1 {
        match arguments[0].to_str() {
            Some("--version" | "-V") => {
                println!("tickloop {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            Some("--build-identity") => {
                println!("{}", env!("TICKLOOP_BUILD_IDENTITY"));
                return;
            }
            Some("--runtime-info") => {
                let info = tickloop_code_access::RuntimeBinaryInfo {
                    product_version: env!("CARGO_PKG_VERSION").into(),
                    build_id: env!("TICKLOOP_BUILD_IDENTITY").into(),
                    protocol_major: tickloop_code_access::PROTOCOL_MAJOR,
                    protocol_minor: tickloop_code_access::PROTOCOL_MINOR,
                    handoff_version: tickloop_code_access::RUNTIME_HANDOFF_VERSION,
                };
                println!(
                    "{}",
                    serde_json::to_string(&info).expect("runtime metadata")
                );
                return;
            }
            _ => {}
        }
    }
    let is_console = !arguments.first().is_some_and(|arg| {
        matches!(
            arg.to_str(),
            Some("update" | "daemon" | "worker" | "monitor" | "tick" | "plugin")
        )
    }) && !arguments
        .iter()
        .any(|arg| matches!(arg.to_str(), Some("--help" | "-h")));
    if is_console {
        match tickloop_code_updater::prepare_launch(&build) {
            Ok(Some(executable)) => {
                use std::os::unix::process::CommandExt;
                let error = std::process::Command::new(executable)
                    .args(&arguments)
                    .exec();
                fail_startup(
                    &arguments,
                    &format!("Cannot open the pinned Tickloop runtime: {error}"),
                );
            }
            Ok(None) => {}
            Err(error) => {
                fail_startup(
                    &arguments,
                    &format!("Cannot preserve this Tickloop runtime: {error:#}"),
                );
            }
        }
    }
    tickloop_code::name_this_build();
    if let Err(error) = tickloop_code_console::terminal::prepare_view_handoff(&arguments) {
        fail_startup(
            &arguments,
            &format!("Cannot prepare View restoration: {error}"),
        );
    }
    let _lease = match tickhost_connect::executable_lease::retain_current_executable() {
        Ok(lease) => lease,
        Err(error) => {
            fail_startup(
                &arguments,
                &format!("Cannot retain running executable: {error}"),
            );
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(TERMINAL_WORKER_THREADS)
        .enable_all()
        .build()
        .unwrap_or_else(|error| {
            fail_startup(
                &arguments,
                &format!("Cannot start the Agent terminal runtime: {error}"),
            )
        });
    let result = runtime.block_on(async {
        if arguments.first().is_some_and(|arg| arg == "update") {
            let result = tickloop_code_updater::command(&arguments[1..], build)
                .await
                .map_err(|error| format!("{error:#}"));
            if result.is_ok()
                && arguments
                    .get(1)
                    .is_some_and(|argument| argument == "--status")
            {
                report_running_versions().await;
            }
            return result;
        }
        if is_console {
            drop(tokio::task::spawn_blocking(
                tickloop_code_updater::cleanup_unused_runtimes,
            ));
            // Automatic checks must never prevent opening a Console.
            let _ = tickloop_code_updater::start_background(
                &arguments,
                tickloop_code_updater::BackgroundCheck::Startup,
            );
            let background_arguments = arguments.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(tickloop_code_updater::CHECK_INTERVAL).await;
                    let _ = tickloop_code_updater::start_background(
                        &background_arguments,
                        tickloop_code_updater::BackgroundCheck::Periodic,
                    );
                }
            });
        }
        match tickloop_code::run_internal(&arguments).await {
            Some(result) => result,
            None => {
                let console_arguments = arguments
                    .iter()
                    .map(|argument| {
                        argument
                            .clone()
                            .into_string()
                            .map_err(|_| "Console arguments must be valid UTF-8".to_owned())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                tickloop_code_console::terminal::run_with_arguments(console_arguments.into_iter())
                    .await
            }
        }
    });
    if let Err(error) = result {
        if let Err(recovery_error) =
            tickloop_code_console::terminal::recover_view_handoff(&arguments)
        {
            eprintln!("View recovery unavailable: {recovery_error}");
        }
        let stderr = io::stderr();
        let use_color = stderr.is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").is_ok_and(|term| term != "dumb");
        let error = tickloop_code_console::terminal::safe_text(&error);
        let (title, body) = error.split_once('\n').unwrap_or((&error, ""));
        let mut output = stderr.lock();
        if use_color {
            let _ = writeln!(output, "{}", format!("Error: {title}").red().bold());
        } else {
            let _ = writeln!(output, "Error: {title}");
        }
        if !body.is_empty() {
            let _ = writeln!(output, "{body}");
        }
        std::process::exit(1);
    }
}

fn fail_startup(arguments: &[std::ffi::OsString], message: &str) -> ! {
    if let Err(error) = tickloop_code_console::terminal::recover_view_handoff(arguments) {
        eprintln!("View recovery unavailable: {error}");
    }
    eprintln!(
        "Error: {}",
        tickloop_code_console::terminal::safe_text(message)
    );
    std::process::exit(1);
}

async fn report_running_versions() {
    use tickloop_code_client::daemon_endpoint::STARTUP_DEADLINE;

    use tickloop_code_access::{
        AccessErrorCode, AgentAccess, ObservationRequest, ObservationValue,
    };

    let endpoint = tickloop_code_client::daemon_endpoint::runtime_directory()
        .join(tickloop_code_client::daemon_endpoint::SOCKET_NAME);
    let client = match tokio::time::timeout(
        STARTUP_DEADLINE,
        tickloop_code_client::LocalAgentClient::connect(&endpoint),
    )
    .await
    {
        Ok(Ok(client)) => Some(client),
        Ok(Err(error)) if error.code == AccessErrorCode::VersionMismatch => None,
        _ => {
            println!("Daemon: not reachable");
            return;
        }
    };
    let executable = tokio::time::timeout(
        STARTUP_DEADLINE,
        tickloop_code_client::running_daemon_executable(&endpoint),
    )
    .await
    .ok()
    .flatten();
    let installation = if let Some(executable) = executable.clone() {
        match tokio::task::spawn_blocking(move || {
            tickloop_code_updater::Installation::detect(&executable)
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result.map_err(|error| format!("{error:#}")))
        {
            Ok(installation) => installation,
            Err(error) => {
                println!(
                    "Daemon installation details unavailable: {}",
                    tickloop_code_console::terminal::safe_text(&error)
                );
                None
            }
        }
    } else {
        None
    };
    let build = client
        .as_ref()
        .and_then(|client| client.protocol().build)
        .or_else(|| {
            installation
                .as_ref()
                .map(|installation| installation.build.identity.clone())
        })
        .unwrap_or_else(|| "unknown build".into());
    println!(
        "Daemon: {}",
        tickloop_code_console::terminal::safe_text(&build)
    );
    println!(
        "Daemon source: {}",
        installation
            .as_ref()
            .map_or("unmanaged/unknown", |installation| installation
                .source
                .label())
    );
    println!(
        "Daemon executable: {}",
        executable.as_ref().map_or_else(
            || "unknown".into(),
            |path| tickloop_code_console::terminal::safe_text(&path.display().to_string())
        )
    );
    let Some(client) = client else {
        println!("Daemon compatibility: incompatible; manual migration required");
        return;
    };
    if client.protocol().upgrade_protocol != Some(tickloop_code_access::RUNTIME_HANDOFF_VERSION) {
        println!("Automatic runtime handoff: unavailable in this daemon");
        return;
    }
    if let Ok(Ok(observation)) = tokio::time::timeout(
        STARTUP_DEADLINE,
        client.observe(ObservationRequest::RuntimeVersion),
    )
    .await
        && let ObservationValue::RuntimeVersion(version) = observation.initial
    {
        if let Some(target) = version.requested_executable {
            println!("Pending activation: {}", target.display());
        }
        for worker in version.workers {
            println!(
                "Worker {}: {}{}",
                worker.process_id,
                worker.build_id.as_deref().unwrap_or("unknown build"),
                if worker.upgrading {
                    " (pending switch)"
                } else {
                    ""
                }
            );
            if let Some(error) = worker.error {
                println!(
                    "Worker {} activation deferred: {}",
                    worker.process_id,
                    tickloop_code_console::terminal::safe_text(&error)
                );
            }
        }
        if let Some(error) = version.error {
            println!(
                "Activation deferred: {}",
                tickloop_code_console::terminal::safe_text(&error)
            );
        }
    }
}
