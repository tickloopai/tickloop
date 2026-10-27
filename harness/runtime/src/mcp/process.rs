use crate::mcp_http::{DirectoryRequests, REQUEST_TIMEOUT, ResponseRedactor, redact_message};
use rmcp::{
    RoleClient,
    model::{ClientJsonRpcMessage, ClientRequest},
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{Transport, async_rw::AsyncRwTransport},
};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader, Empty},
    process::{Child, ChildStdin, ChildStdout, Command},
};

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct ProcessTransport {
    child: Child,
    group: i32,
    io: AsyncRwTransport<RoleClient, Empty, ChildStdin>,
    stdout: BufReader<ChildStdout>,
    frame: Vec<u8>,
    redactor: ResponseRedactor,
    directory_requests: DirectoryRequests,
}
impl ProcessTransport {
    pub(super) fn spawn(
        mut command: Command,
        stderr: Stdio,
        redactor: ResponseRedactor,
    ) -> Result<Self, String> {
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
            io: AsyncRwTransport::new(tokio::io::empty(), stdin),
            stdout: BufReader::new(stdout),
            frame: Vec::new(),
            redactor,
            directory_requests: Default::default(),
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
        if let ClientJsonRpcMessage::Request(request) = &item
            && matches!(request.request, ClientRequest::ListToolsRequest(_))
        {
            let mut pending = self.directory_requests.lock().unwrap();
            pending.retain(|_, started| started.elapsed() <= REQUEST_TIMEOUT);
            pending.insert(request.id.clone(), tokio::time::Instant::now());
        }
        self.io.send(item)
    }
    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            // Keep partial frames across cancellation; never let the SDK parse raw peer bytes.
            let remaining = MAX_FRAME_BYTES.checked_sub(self.frame.len())?;
            let read = (&mut self.stdout)
                .take(remaining as u64 + 1)
                .read_until(b'\n', &mut self.frame)
                .await
                .ok()?;
            if read == 0 || self.frame.len() > MAX_FRAME_BYTES {
                return None;
            }
            if self.frame.iter().all(u8::is_ascii_whitespace) {
                self.frame.clear();
                continue;
            }
            let text = std::str::from_utf8(&self.frame).ok()?;
            let redacted = redact_message(text, &self.redactor, &self.directory_requests).ok()?;
            self.frame.clear();
            return serde_json::from_str(&redacted).ok();
        }
    }
    async fn close(&mut self) -> Result<(), Self::Error> {
        let _ = tokio::time::timeout(Duration::from_millis(200), self.io.close()).await;
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
