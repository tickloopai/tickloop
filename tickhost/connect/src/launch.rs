//! The host-neutral launch and foreground contract of a visible Tick launcher. A launcher runs
//! the business Tick under a TickSession only after proving it is the foreground process group
//! of the very terminal its backend holds; it never invents Workspace, Tab or Pane facts.

use std::{io::Write, os::fd::AsRawFd, path::PathBuf, process::ExitStatus};

use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{
    herdr::{HerdrContext, HerdrPort, Pane, SocketHerdr},
    host_error::HostError,
    hosted::{self, CAPABILITY_LAUNCH, HostConnection, LaunchBinding, Role, TerminalDevice},
    session::{SessionBackend, SessionLaunch, TickSession},
};

/// The first argument that makes a bundled launcher only check its command, before an Agent
/// places it in a Pane.
pub const PREFLIGHT_ARGUMENT: &str = "__preflight";
const METADATA_TTL_MS: u64 = 30_000;
const METADATA_RENEWAL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Where this launcher's terminal lives and who must hear about the running instance.
pub enum LaunchHost {
    Herdr {
        context: HerdrContext,
        host: SocketHerdr,
        pane: Pane,
    },
    Hosted {
        binding: LaunchBinding,
        connection: HostConnection,
    },
}

/// The launch context found in the environment, before any foreground proof.
pub enum LaunchContext {
    Herdr(HerdrContext),
    Hosted(LaunchBinding),
}

impl LaunchContext {
    /// Reads the launch context. Hosted and Herdr contexts are exclusive; an incomplete or
    /// conflicting context is an error rather than absence.
    pub fn from_environment() -> Result<Option<Self>, String> {
        let hosted = LaunchBinding::from_environment()?;
        let herdr = HerdrContext::from_environment()?;
        match (hosted, herdr) {
            (Some(_), Some(_)) => Err(
                "a hosted launch cannot also carry a Herdr Pane context; refusing to start".into(),
            ),
            (Some(binding), None) => Ok(Some(Self::Hosted(binding))),
            (None, Some(context)) => Ok(Some(Self::Herdr(context))),
            (None, None) => Ok(None),
        }
    }

    /// Proves this process is the foreground process group of the backend's terminal.
    pub async fn verify_foreground(self) -> Result<LaunchHost, HostError> {
        match self {
            Self::Herdr(context) => {
                let host = SocketHerdr::new(context.clone());
                let current = host
                    .request(
                        "pane.current",
                        json!({"caller_pane_id":context.caller_pane}),
                        CancellationToken::new(),
                    )
                    .await?;
                let pane: Pane = serde_json::from_value(current["pane"].clone())
                    .map_err(|error| HostError::before("ProtocolViolation", error.to_string()))?;
                let process = host
                    .request(
                        "pane.process_info",
                        json!({"pane_id":pane.pane_id}),
                        CancellationToken::new(),
                    )
                    .await?;
                let group = unsafe { libc::getpgrp() } as u64;
                if process["process_info"]["foreground_process_group_id"].as_u64() != Some(group) {
                    return Err(HostError::before(
                        "NotForeground",
                        "the launcher must run in the foreground of its Herdr Pane",
                    ));
                }
                Ok(LaunchHost::Herdr {
                    context,
                    host,
                    pane,
                })
            }
            Self::Hosted(binding) => {
                let terminal = std::io::stderr();
                let descriptor = terminal.as_raw_fd();
                let group = unsafe { libc::getpgrp() };
                if unsafe { libc::isatty(descriptor) } != 1
                    || unsafe { libc::tcgetpgrp(descriptor) } != group
                {
                    return Err(HostError::before(
                        "NotForeground",
                        "the hosted launcher must be the foreground of its terminal",
                    ));
                }
                let device = TerminalDevice::of(descriptor)
                    .map_err(|error| HostError::before("NotForeground", error.to_string()))?;
                let mut connection = HostConnection::connect(
                    &binding.terminal.endpoint,
                    Role::Launcher {
                        operation: binding.operation,
                        launch_token: binding.launch_token.clone(),
                    },
                    &[CAPABILITY_LAUNCH],
                )
                .await?;
                if connection.epoch() != binding.terminal.epoch {
                    return Err(HostError::before(
                        "EpochChanged",
                        "the Tickhost restarted after this launch was prepared",
                    ));
                }
                connection
                    .request(
                        "verify",
                        json!({"device":device,"process_group":group}),
                        hosted::CONTROL_TIMEOUT,
                    )
                    .await?;
                Ok(LaunchHost::Hosted {
                    binding,
                    connection,
                })
            }
        }
    }
}

impl LaunchHost {
    pub fn backend(&self) -> SessionBackend {
        match self {
            Self::Herdr { context, pane, .. } => SessionBackend::Herdr {
                socket: context.socket.clone(),
                terminal_id: pane.terminal_id.clone(),
            },
            Self::Hosted { binding, .. } => SessionBackend::Hosted(binding.terminal.clone()),
        }
    }

    /// Reports the running instance to its backend: Herdr Pane metadata, or the hosted launch
    /// registration with the business process the TickSession spawned.
    pub async fn report(&mut self, session: &TickSession) -> Result<(), HostError> {
        let instance = session.instance().to_string();
        match self {
            Self::Herdr { host, pane, .. } => {
                report_pane_instance(host, &pane.pane_id, Some(&instance)).await
            }
            Self::Hosted { connection, .. } => {
                let business_pid = session.process_id().ok_or_else(|| {
                    HostError::before("LaunchFailed", "the Tick exited before registration")
                })?;
                connection
                    .request(
                        "register",
                        json!({"instance":instance,"business_pid":business_pid}),
                        hosted::CONTROL_TIMEOUT,
                    )
                    .await
                    .map(|_| ())
            }
        }
    }

    /// Keeps a Herdr Pane's metadata fresh while it still shows this instance. A hosted launch
    /// has nothing to renew; the host watches its own PTY.
    pub async fn renew(&self, instance: &str) {
        let Self::Herdr {
            context,
            host,
            pane,
        } = self
        else {
            return std::future::pending().await;
        };
        loop {
            tokio::time::sleep(METADATA_RENEWAL_INTERVAL).await;
            if let Ok(current) = host
                .request(
                    "pane.current",
                    json!({"caller_pane_id":context.caller_pane}),
                    CancellationToken::new(),
                )
                .await
                && current["pane"]["terminal_id"] == pane.terminal_id
                && current["pane"]["tokens"]["tickloop_instance"]
                    .as_str()
                    .is_none_or(|owner| owner == instance)
            {
                let _ =
                    report_pane_instance(host, &current["pane"]["pane_id"], Some(instance)).await;
            }
        }
    }

    /// Clears the Herdr Pane metadata this launcher reported, if the Pane still shows it.
    pub async fn release(&self, instance: &str) {
        let Self::Herdr {
            context,
            host,
            pane,
        } = self
        else {
            return;
        };
        if let Ok(current) = host
            .request(
                "pane.current",
                json!({"caller_pane_id":context.caller_pane}),
                CancellationToken::new(),
            )
            .await
            && current["pane"]["terminal_id"] == pane.terminal_id
            && current["pane"]["tokens"]["tickloop_instance"] == instance
        {
            let _ = report_pane_instance(host, &current["pane"]["pane_id"], None).await;
        }
    }
}

/// Sets or clears the instance a Herdr Pane shows; a set value expires unless renewed.
async fn report_pane_instance(
    host: &SocketHerdr,
    pane_id: &(impl serde::Serialize + ?Sized),
    instance: Option<&str>,
) -> Result<(), HostError> {
    let metadata = match instance {
        Some(instance) => {
            json!({"pane_id":pane_id,"source":"tickloop","tokens":{"tickloop_instance":instance},"ttl_ms":METADATA_TTL_MS})
        }
        None => json!({"pane_id":pane_id,"source":"tickloop","tokens":{"tickloop_instance":null}}),
    };
    host.request("pane.report_metadata", metadata, CancellationToken::new())
        .await
        .map(|_| ())
}

/// Runs `command` as the business Tick behind a TickSession on the verified terminal, reports
/// the instance to its backend and waits until the Tick exits or the terminal goes away. The
/// launcher's terminal modes are restored even when the Tick was killed.
pub async fn run(
    mut host: LaunchHost,
    command: Vec<String>,
    cwd: PathBuf,
) -> Result<ExitStatus, HostError> {
    let _terminal = TerminalRestore::capture();
    let signal_error = |error: std::io::Error| HostError::before("LaunchFailed", error.to_string());
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(signal_error)?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(signal_error)?;
    let session = TickSession::spawn(SessionLaunch {
        command,
        cwd,
        backend: host.backend(),
        runtime_dir: None,
    })
    .await
    .map_err(|error| HostError::before("LaunchFailed", error.to_string()))?;
    let instance = session.instance().to_string();
    host.report(&session).await?;
    let result = tokio::select! {
        status = session.wait() => status.map_err(|error| HostError::after("TickFailed", error.to_string())),
        _ = tokio::signal::ctrl_c() => Ok(ExitStatus::default()),
        _ = terminate.recv() => Ok(ExitStatus::default()),
        _ = hangup.recv() => Ok(ExitStatus::default()),
        _ = host.renew(&instance) => unreachable!("renewal runs until the Tick ends"),
    };
    host.release(&instance).await;
    result
}

struct TerminalRestore(Option<libc::termios>);

impl TerminalRestore {
    fn capture() -> Self {
        let mut modes = std::mem::MaybeUninit::uninit();
        // stderr keeps the controlling terminal while the child's stdin/stdout carry JSON-RPC.
        let modes = unsafe {
            if libc::tcgetattr(libc::STDERR_FILENO, modes.as_mut_ptr()) == 0 {
                Some(modes.assume_init())
            } else {
                None
            }
        };
        Self(modes)
    }
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        if let Some(modes) = &self.0 {
            // The launcher restores its terminal even when its child was killed before cleanup.
            unsafe {
                libc::tcsetattr(libc::STDERR_FILENO, libc::TCSANOW, modes);
            }
            let _ = std::io::stderr().write_all(
                b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1049l\x1b[?25h\x1b[0m",
            );
        }
    }
}
