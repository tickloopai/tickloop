use std::path::Path;

use tickloop_agent_updater::{Build, SelectedRuntime};

use super::Agent;

pub(super) const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

pub(super) struct Activation {
    pub target: Option<SelectedRuntime>,
    pub waiting: bool,
    pub error: Option<String>,
}

pub(super) async fn check(agent: Agent, initial_launch: bool) -> Result<Activation, String> {
    let mut selected = tokio::task::spawn_blocking(tickloop_agent_updater::selected_runtime)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("{error:#}"))?;
    if !agent.supports_runtime_upgrade() {
        return Ok(Activation {
            waiting: selected.is_some(),
            target: None,
            error: None,
        });
    }
    let running = agent
        .runtime_version()
        .await
        .map_err(|error| error.to_string())?;
    let pending_work = running.upgrading || running.workers.iter().any(|worker| worker.upgrading);
    if selected.is_none() && !initial_launch {
        let executable = running.executable.clone();
        let build = running.build_id.clone();
        selected = tokio::task::spawn_blocking(move || {
            tickloop_agent_updater::follow_daemon_runtime(&executable, &build)
        })
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("{error:#}"))?;
    }
    let requested = match selected {
        Some(target) => Some(target),
        None if initial_launch => tickloop_agent_protocol::build_identity()
            .filter(|own| *own != running.build_id)
            .map(|identity| {
                std::env::current_exe().map(|executable| SelectedRuntime {
                    executable,
                    build: Build {
                        version: env!("CARGO_PKG_VERSION").into(),
                        identity: identity.into(),
                    },
                })
            })
            .transpose()
            .map_err(|error| error.to_string())?,
        None => None,
    };
    let Some(target) = requested else {
        return Ok(Activation {
            target: None,
            waiting: pending_work,
            error: running.error,
        });
    };
    if running.build_id == target.build.identity {
        return Ok(Activation {
            target: Some(target),
            waiting: pending_work,
            error: running.error,
        });
    }
    let target_path = target
        .executable
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let current_executable = running.executable.clone();
    let candidate_executable = target_path.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || {
        tickloop_agent_updater::ensure_same_runtime_source(
            &current_executable,
            &candidate_executable,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    {
        return Ok(Activation {
            target: None,
            waiting: false,
            error: Some(format!("{error:#}")),
        });
    }
    if running.requested_executable.as_ref() != Some(&target_path) && !running.upgrading {
        agent
            .request_runtime_upgrade(target_path)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(Activation {
        target: None,
        waiting: true,
        error: running.error,
    })
}

pub(super) fn is_current_executable(target: &Path) -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        == target.canonicalize().ok()
}
