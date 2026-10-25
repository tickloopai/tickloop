use std::path::Path;

use tickloop_agent_access::{ExecutionRef, RuntimeVersion, WorkerRuntimeVersion};

use tickloop_agent_updater::{Build, SelectedRuntime, UpdateNotice};

use super::Agent;

pub(super) const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

pub(super) struct Activation {
    pub target: Option<SelectedRuntime>,
    pub runtime: Option<RuntimeVersion>,
    pub error: Option<String>,
}

impl Activation {
    pub fn worker(&self, execution: ExecutionRef) -> Option<&WorkerRuntimeVersion> {
        self.runtime
            .as_ref()?
            .workers
            .iter()
            .find(|worker| worker.executions.contains(&execution))
    }

    pub fn notice_for(&self, execution: Option<ExecutionRef>) -> Option<UpdateNotice> {
        let worker = self.worker(execution?)?;
        if !worker.upgrading {
            return None;
        }
        let reason = worker.error.as_ref().or(self.error.as_ref()).or_else(|| {
            self.runtime
                .as_ref()
                .and_then(|runtime| runtime.error.as_ref())
        });
        Some(match reason {
            Some(reason) => UpdateNotice::Deferred {
                reason: reason.clone(),
            },
            None => UpdateNotice::PendingAgent,
        })
    }
}

pub(super) async fn check(agent: Agent, initial_launch: bool) -> Result<Activation, String> {
    let mut selected = tokio::task::spawn_blocking(tickloop_agent_updater::selected_runtime)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("{error:#}"))?;
    if !agent.supports_runtime_upgrade() {
        return Ok(Activation {
            runtime: None,
            target: None,
            error: None,
        });
    }
    let running = agent
        .runtime_version()
        .await
        .map_err(|error| error.to_string())?;
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
        None if initial_launch => tickloop_agent_access::build_identity()
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
            runtime: Some(running),
            error: None,
        });
    };
    if running.build_id == target.build.identity {
        return Ok(Activation {
            target: Some(target),
            runtime: Some(running),
            error: None,
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
            runtime: Some(running),
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
        runtime: Some(running),
        error: None,
    })
}

pub(super) fn is_current_executable(target: &Path) -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        == target.canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn one_pending_worker_does_not_mark_nine_upgraded_agents_as_pending() {
        let epoch = Uuid::new_v4();
        let executions = (0..10)
            .map(|_| ExecutionRef {
                daemon_epoch: epoch,
                agent_id: Uuid::new_v4(),
                generation: 1,
            })
            .collect::<Vec<_>>();
        let mut activation = Activation {
            target: None,
            error: None,
            runtime: Some(RuntimeVersion {
                executable: "/new/tickloop".into(),
                build_id: "0.2.12+new".into(),
                requested_executable: None,
                upgrading: false,
                error: None,
                workers: executions
                    .iter()
                    .enumerate()
                    .map(|(index, execution)| WorkerRuntimeVersion {
                        process_id: index as u32 + 1,
                        executable: if index == 9 {
                            "/old/tickloop"
                        } else {
                            "/new/tickloop"
                        }
                        .into(),
                        build_id: Some(
                            if index == 9 {
                                "0.2.11+old"
                            } else {
                                "0.2.12+new"
                            }
                            .into(),
                        ),
                        upgrading: index == 9,
                        executions: vec![*execution],
                        error: None,
                    })
                    .collect(),
            }),
        };
        for execution in &executions[..9] {
            assert_eq!(activation.notice_for(Some(*execution)), None);
        }
        assert_eq!(activation.notice_for(None), None);
        assert_eq!(
            activation.notice_for(Some(executions[9])),
            Some(UpdateNotice::PendingAgent)
        );
        activation.runtime.as_mut().unwrap().workers[9].error =
            Some("Tickhost cannot transfer a retained Shell".into());
        assert!(matches!(
            activation.notice_for(Some(executions[9])),
            Some(UpdateNotice::Deferred { .. })
        ));
        assert_eq!(activation.notice_for(Some(executions[0])), None);
        activation.runtime.as_mut().unwrap().workers[9].upgrading = false;
        assert_eq!(activation.notice_for(Some(executions[9])), None);
        activation.runtime.as_mut().unwrap().workers[9].upgrading = true;
        activation.runtime.as_mut().unwrap().workers[9]
            .executions
            .clear();
        assert_eq!(activation.notice_for(Some(executions[9])), None);
    }
}
