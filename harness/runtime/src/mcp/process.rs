use rmcp::{
    RoleClient,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{Transport, async_rw::AsyncRwTransport},
};
use std::{process::Stdio, time::Duration};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub(super) struct ProcessTransport {
    child: Child,
    group: i32,
    io: AsyncRwTransport<RoleClient, ChildStdout, ChildStdin>,
}
impl ProcessTransport {
    pub(super) fn spawn(mut command: Command, stderr: Stdio) -> Result<Self, String> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| "MCP process could not start")?;
        let group = child.id().ok_or("MCP process has no PID")? as i32;
        let stdout = child.stdout.take().ok_or("MCP stdout unavailable")?;
        let stdin = child.stdin.take().ok_or("MCP stdin unavailable")?;
        Ok(Self {
            child,
            group,
            io: AsyncRwTransport::new(stdout, stdin),
        })
    }
}
impl Drop for ProcessTransport {
    fn drop(&mut self) {
        if self.group > 0 {
            unsafe {
                libc::kill(-self.group, libc::SIGKILL);
            }
        }
    }
}
impl Transport<RoleClient> for ProcessTransport {
    type Error = std::io::Error;
    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.io.send(item)
    }
    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.io.receive()
    }
    async fn close(&mut self) -> Result<(), Self::Error> {
        let _ = self.io.close().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        unsafe {
            libc::kill(-self.group, libc::SIGTERM);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
        let _ = self.child.wait().await;
        self.group = -1;
        Ok(())
    }
}
