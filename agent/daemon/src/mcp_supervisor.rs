//! Private, session-scoped MCP process supervision. No descriptor reaches argv or diagnostics.
//!
//! The Worker alone owns the control writer. Its loss (including SIGKILL) closes that pipe;
//! the supervisor terminates the MCP process group before acknowledging closure. MCP bytes use
//! two independent pipes, never the bootstrap/lifetime channel.

use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    path::PathBuf,
    pin::Pin,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, ready},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tickloop_agent_protocol::{SessionMcpServer, validate_session_mcp_servers};
use tickloop_agent_runtime::session_mcp::{
    SessionMcpLauncher, SessionMcpLifetime, SessionMcpTransport,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf, unix::AsyncFd},
    process::{Child as AsyncChild, ChildStdin, ChildStdout},
};

pub const SUPERVISOR_ARGUMENT: &str = "--mcp-supervisor";
const BOOTSTRAP_MAX: usize = 129 * 1024;
const START_DEADLINE: Duration = Duration::from_secs(10);
const CLOSE_DEADLINE: Duration = Duration::from_secs(8);
const TERM_GRACE: Duration = Duration::from_secs(3);
const READY: u8 = 1;
const CLOSED: u8 = 2;
const BASE_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
static TERMINATING: AtomicBool = AtomicBool::new(false);

extern "C" fn terminate(_signal: libc::c_int) {
    // Lock-free atomic store only: safe in a signal handler, with cleanup in the main loop.
    TERMINATING.store(true, Ordering::Relaxed);
}

// Deliberately not Debug: both descriptor and serialized bootstrap are private data.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bootstrap {
    server: SessionMcpServer,
    input: i32,
    output: i32,
}

/// Launches the same pinned Tickloop executable, never a shell or an MCP command-line wrapper.
pub struct SupervisorMcpLauncher {
    executable: PathBuf,
}

impl SupervisorMcpLauncher {
    pub fn new(executable: PathBuf) -> Self {
        Self { executable }
    }
}

#[async_trait]
impl SessionMcpLauncher for SupervisorMcpLauncher {
    async fn launch(&self, descriptor: &SessionMcpServer) -> Result<SessionMcpTransport, String> {
        validate_session_mcp_servers(std::slice::from_ref(descriptor))?;
        let (input, writer) = io::pipe().map_err(|_| "cannot create MCP input pipe")?;
        let (reader, output) = io::pipe().map_err(|_| "cannot create MCP output pipe")?;
        let input: OwnedFd = input.into();
        let output: OwnedFd = output.into();
        let writer = Pipe::new(writer.into()).map_err(|_| "cannot register MCP input pipe")?;
        let reader = Pipe::new(reader.into()).map_err(|_| "cannot register MCP output pipe")?;
        let bootstrap = Bootstrap {
            server: descriptor.clone(),
            input: input.as_raw_fd(),
            output: output.as_raw_fd(),
        };
        let bytes = serde_json::to_vec(&bootstrap).map_err(|_| "cannot encode MCP bootstrap")?;
        if bytes.len() > BOOTSTRAP_MAX {
            return Err("MCP bootstrap exceeds the size limit".into());
        }
        let inherited = [input.as_raw_fd(), output.as_raw_fd()];
        let mut command = tokio::process::Command::new(&self.executable);
        command
            .arg(SUPERVISOR_ARGUMENT)
            .env_clear()
            .env("PATH", BASE_PATH)
            .env("LANG", "C")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Only the child's copies lose CLOEXEC. The Worker's descriptors stay CLOEXEC, so an
        // unrelated concurrent process launch cannot retain the MCP pipes or control writer.
        unsafe {
            command.pre_exec(move || {
                for fd in inherited {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|_| "cannot start MCP supervisor")?;
        drop(input);
        drop(output);
        let control = child
            .stdin
            .take()
            .ok_or("MCP supervisor control unavailable")?;
        let ack = child
            .stdout
            .take()
            .ok_or("MCP supervisor acknowledgement unavailable")?;
        let mut lifetime = Lifetime {
            control: Some(control),
            ack,
            acknowledged: false,
            child: Some(child),
        };
        let started = tokio::time::timeout(START_DEADLINE, async {
            let control = lifetime
                .control
                .as_mut()
                .ok_or("MCP supervisor control unavailable")?;
            control
                .write_all(&(bytes.len() as u32).to_be_bytes())
                .await
                .map_err(|_| "cannot write MCP bootstrap")?;
            control
                .write_all(&bytes)
                .await
                .map_err(|_| "cannot write MCP bootstrap")?;
            if lifetime
                .ack
                .read_u8()
                .await
                .map_err(|_| "MCP supervisor startup failed")?
                != READY
            {
                return Err("MCP supervisor startup failed");
            }
            Ok(())
        })
        .await;
        match started {
            Ok(Ok(())) => Ok(SessionMcpTransport {
                reader: Box::new(reader),
                writer: Box::new(writer),
                lifetime: Box::new(lifetime),
            }),
            _ => {
                // Closing remains necessary after a startup timeout: dispatch may have happened.
                let _ = lifetime.close().await;
                Err("MCP supervisor startup failed or timed out".into())
            }
        }
    }
}

struct Lifetime {
    control: Option<ChildStdin>,
    ack: ChildStdout,
    acknowledged: bool,
    child: Option<AsyncChild>,
}

#[async_trait]
impl SessionMcpLifetime for Lifetime {
    async fn close(&mut self) -> Result<(), String> {
        // Cancellation-safe: drop the sole writer before the first await. Never kill the
        // supervisor, even on timeout: it must remain alive to clean up its owned process group.
        drop(self.control.take());
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        tokio::time::timeout(CLOSE_DEADLINE, async {
            if !self.acknowledged {
                let ack = self
                    .ack
                    .read_u8()
                    .await
                    .map_err(|_| "MCP close acknowledgement lost")?;
                if ack != CLOSED {
                    return Err("MCP supervisor cleanup failed");
                }
                self.acknowledged = true;
            }
            let status = child
                .wait()
                .await
                .map_err(|_| "cannot reap MCP supervisor")?;
            if !status.success() {
                return Err("MCP supervisor cleanup failed");
            }
            Ok(())
        })
        .await
        .map_err(|_| "MCP supervisor cleanup timed out".to_owned())?
        .map_err(str::to_owned)?;
        self.child.take();
        Ok(())
    }
}

// Dropping Lifetime also drops its unique control writer. Tokio reaps a dropped Child; the
// supervisor is intentionally not kill_on_drop, since it must first reap its own MCP child.

/// Hidden process entry, called before Console setup, updater operations, or Tokio startup.
/// All failures are intentionally static; OS errors must not reveal descriptor-derived paths.
pub fn run() -> Result<(), String> {
    // This dedicated process must clean its own child group on TERM/INT, not exit immediately.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = terminate as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) == -1
            || libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) == -1
        {
            return Err("cannot register MCP supervisor shutdown".into());
        }
    }
    // Unbuffered: polling stdin between header/body reads must not overlook bytes read ahead
    // into StdinLock's internal buffer. This private entry owns fd 0 until process exit.
    let mut control = unsafe { std::fs::File::from_raw_fd(0) };
    let mut header = [0; 4];
    read_bootstrap(&mut control, &mut header).map_err(|_| "invalid MCP supervisor bootstrap")?;
    let size = u32::from_be_bytes(header) as usize;
    if size == 0 || size > BOOTSTRAP_MAX {
        return Err("invalid MCP supervisor bootstrap".into());
    }
    let mut bytes = vec![0; size];
    read_bootstrap(&mut control, &mut bytes).map_err(|_| "invalid MCP supervisor bootstrap")?;
    let bootstrap: Bootstrap =
        serde_json::from_slice(&bytes).map_err(|_| "invalid MCP supervisor bootstrap")?;
    validate_session_mcp_servers(std::slice::from_ref(&bootstrap.server))?;
    if bootstrap.input < 3 || bootstrap.output < 3 || bootstrap.input == bootstrap.output {
        return Err("invalid MCP data descriptors".into());
    }
    let input = inherited_pipe(bootstrap.input).map_err(|_| "invalid MCP input descriptor")?;
    let output = inherited_pipe(bootstrap.output).map_err(|_| "invalid MCP output descriptor")?;
    let mut group =
        spawn_server(&bootstrap.server, input, output).map_err(|_| "cannot launch MCP server")?;
    let mut ack = io::stdout().lock();
    // Any failure from this point goes through the Group guard's cleanup, including EPIPE.
    ack.write_all(&[READY])
        .map_err(|_| "MCP supervisor acknowledgement lost")?;
    ack.flush()
        .map_err(|_| "MCP supervisor acknowledgement lost")?;
    loop {
        if TERMINATING.load(Ordering::Relaxed) {
            break;
        }
        let mut pollfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut pollfd, 1, 25) };
        if polled < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if pollfd.revents != 0 {
            // No commands or second bootstrap are accepted. EOF is the normal close signal.
            let mut byte = [0];
            match control.read(&mut byte) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                _ => break,
            }
        }
        match group.exited() {
            Ok(false) => {}
            // Also clean descendants when the MCP group leader exits on its own.
            _ => break,
        }
    }
    group.close().map_err(|_| "MCP process cleanup failed")?;
    ack.write_all(&[CLOSED])
        .map_err(|_| "MCP close acknowledgement lost")?;
    ack.flush().map_err(|_| "MCP close acknowledgement lost")?;
    Ok(())
}

fn read_bootstrap(reader: &mut impl Read, mut bytes: &mut [u8]) -> io::Result<()> {
    let deadline = Instant::now() + START_DEADLINE;
    while !bytes.is_empty() {
        if TERMINATING.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut descriptor = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match reader.read(bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => bytes = &mut bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn inherited_pipe(fd: i32) -> io::Result<OwnedFd> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFIFO {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // Validated inherited descriptor is now owned exactly once in this process.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn spawn_server(server: &SessionMcpServer, input: OwnedFd, output: OwnedFd) -> io::Result<Group> {
    let child = Command::new(&server.program)
        .args(&server.args)
        .current_dir(&server.cwd)
        .env_clear()
        .env("PATH", BASE_PATH)
        .env("LANG", "C")
        .envs(&server.env)
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(output))
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(Group { child: Some(child) })
}

struct Group {
    child: Option<Child>,
}

impl Group {
    fn exited(&self) -> io::Result<bool> {
        let Some(child) = &self.child else {
            return Ok(true);
        };
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // WNOWAIT is essential: do not release the PID/PGID before the final group signal.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        #[cfg(target_os = "linux")]
        let pid = unsafe { info.si_pid() };
        #[cfg(not(target_os = "linux"))]
        let pid = info.si_pid;
        Ok(pid != 0)
    }

    fn close(&mut self) -> io::Result<()> {
        // Reconfirm waitability before signaling. If another reaper or SIGCHLD disposition
        // released this child, the numeric PID is no longer an authority to signal its group.
        loop {
            match self.exited() {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.child.take();
                    return Err(error);
                }
            }
        }
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let pid = child.id() as i32;
        // This PID belongs to our spawned, still-unreaped child. Its zombie pins the group
        // identity even when the leader exits before descendants. Never signal after wait.
        let term = unsafe { libc::kill(-pid, libc::SIGTERM) };
        let term_error = (term == -1).then(io::Error::last_os_error);
        std::thread::sleep(TERM_GRACE);
        let kill = unsafe { libc::kill(-pid, libc::SIGKILL) };
        let kill_error = (kill == -1).then(io::Error::last_os_error);
        // The direct child is also still ours and unreaped. Ensure it cannot evade the bounded
        // wait by moving itself out of the original group; never follow it into another group.
        let direct_error = child.kill().err();
        // Darwin reports EPERM, rather than ESRCH, for a group containing only zombies.
        // Verify every remaining member before treating that particular result as cleaned.
        // Keep the leader unreaped while querying: its PID still pins our group identity.
        #[cfg(target_os = "macos")]
        let zombie_group = group_has_only_zombies(pid);
        let waited = child.wait();
        for error in [term_error, kill_error].into_iter().flatten() {
            if error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            #[cfg(target_os = "macos")]
            if error.raw_os_error() == Some(libc::EPERM) && zombie_group {
                continue;
            }
            return Err(error);
        }
        if let Some(error) = direct_error
            && error.raw_os_error() != Some(libc::ESRCH)
        {
            return Err(error);
        }
        waited.map(|_| ())
    }
}

#[cfg(target_os = "macos")]
fn group_has_only_zombies(pgid: i32) -> bool {
    // Bounded enumeration; a full buffer or inaccessible member is an unknown outcome.
    let mut pids = [0i32; 4096];
    let capacity = std::mem::size_of_val(&pids) as i32;
    // libproc's convenience wrapper returns a PID count, unlike proc_listpids' byte count.
    let count = unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), capacity) };
    if count <= 0 || count as usize >= pids.len() {
        return false;
    }
    pids[..count as usize]
        .iter()
        .filter(|pid| **pid > 0)
        .all(|pid| {
            let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
            let read = unsafe {
                libc::proc_pidinfo(
                    *pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            if read == size {
                let info = unsafe { info.assume_init() };
                info.pbi_pgid == pgid as u32 && info.pbi_status == libc::SZOMB
            } else {
                read == 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            }
        })
}

impl Drop for Group {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct Pipe(Option<AsyncFd<OwnedFd>>);

impl Pipe {
    fn new(fd: OwnedFd) -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        // OwnedFd pins this descriptor until AsyncFd is dropped; no external close or replacement.
        unsafe { AsyncFd::register(fd) }
            .map(|fd| Self(Some(fd)))
            .map_err(io::Error::from)
    }
}

impl AsyncRead for Pipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let Some(pipe) = &self.0 else {
            return Poll::Ready(Ok(()));
        };
        loop {
            let mut guard = ready!(pipe.poll_read_ready(cx))?;
            match guard.try_io(|fd| {
                let target = buf.initialize_unfilled();
                let result = unsafe {
                    libc::read(
                        fd.get_ref().as_raw_fd(),
                        target.as_mut_ptr().cast(),
                        target.len(),
                    )
                };
                if result < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(result as usize)
            }) {
                Ok(Ok(count)) => {
                    buf.advance(count);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_) => {}
            }
        }
    }
}

impl AsyncWrite for Pipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let Some(pipe) = &self.0 else {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        };
        loop {
            let mut guard = ready!(pipe.poll_write_ready(cx))?;
            match guard.try_io(|fd| {
                let result = unsafe {
                    libc::write(fd.get_ref().as_raw_fd(), bytes.as_ptr().cast(), bytes.len())
                };
                if result < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(result as usize)
            }) {
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                Ok(result) => return Poll::Ready(result),
                Err(_) => {}
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().0.take();
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn spawned_server_gets_only_fixed_base_and_explicit_environment() {
        let server = SessionMcpServer {
            name: "test".into(),
            program: "/usr/bin/env".into(),
            args: vec![],
            env: BTreeMap::from([("EXPLICIT_TEST_VALUE".into(), "test-value".into())]),
            cwd: "/".into(),
        };
        let (input, writer) = io::pipe().unwrap();
        let (mut reader, output) = io::pipe().unwrap();
        let mut group = spawn_server(&server, input.into(), output.into()).unwrap();
        drop(writer);
        let mut result = String::new();
        reader.read_to_string(&mut result).unwrap();
        let mut lines: Vec<_> = result.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            [
                "EXPLICIT_TEST_VALUE=test-value",
                "LANG=C",
                "PATH=/usr/bin:/bin:/usr/sbin:/sbin"
            ]
        );
        group.close().unwrap();
        assert!(group.child.is_none());
        group.close().unwrap();
    }

    #[tokio::test]
    async fn data_pipe_is_independent_and_nonblocking() {
        let (reader, writer) = io::pipe().unwrap();
        let mut reader = Pipe::new(reader.into()).unwrap();
        let mut writer = Pipe::new(writer.into()).unwrap();
        writer.write_all(b"mcp-data").await.unwrap();
        let mut data = [0; 8];
        reader.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"mcp-data");
        drop(writer);
        assert_eq!(reader.read(&mut data).await.unwrap(), 0);
    }
}
