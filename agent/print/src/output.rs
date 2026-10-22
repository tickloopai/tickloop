use std::{
    fs::File,
    io::{self, Write},
    os::fd::{AsFd, AsRawFd, OwnedFd},
    time::{Duration, Instant},
};

const WRITE_BUDGET: Duration = Duration::from_millis(250);

/// An unbuffered command-output sink. Each write_all allows at most 250ms of backpressure,
/// rather than indefinitely blocking cancellation. The descriptor's flags are never changed: a
/// dup shares its open file description with the caller's terminal or pipe, which a set
/// O_NONBLOCK would outlive (concurrent invocations, SIGKILL). Instead each write waits for
/// POLLOUT and writes at most PIPE_BUF bytes, which a pipe or socket then accepts without
/// blocking. A reader must drain output continuously; output failure can leave a partial record.
pub struct BoundedOutput {
    file: File,
}

impl BoundedOutput {
    pub fn new(fd: OwnedFd) -> Self {
        Self {
            file: File::from(fd),
        }
    }

    /// Only time spent waiting for the reader is charged to `budget`, not the writes themselves.
    fn write_within(&mut self, bytes: &[u8], budget: &mut Duration) -> io::Result<usize> {
        loop {
            if !self.writable(Duration::ZERO)? {
                let started = Instant::now();
                let writable = !budget.is_zero() && self.writable(*budget)?;
                *budget = budget.saturating_sub(started.elapsed());
                if !writable {
                    if budget.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "stdout backpressure exceeded the 250ms write budget",
                        ));
                    }
                    continue;
                }
            }
            match self.file.write(&bytes[..bytes.len().min(libc::PIPE_BUF)]) {
                // The inherited descriptor may already be nonblocking; wait for POLLOUT again.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                result => return result,
            }
        }
    }

    /// Ready also covers POLLERR/POLLHUP/POLLNVAL; the following write reports that error.
    fn writable(&self, timeout: Duration) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // Round up so that a sub-millisecond remainder still waits.
        let milliseconds = timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        match unsafe { libc::poll(&mut descriptor, 1, milliseconds) } {
            -1 => {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
            ready => Ok(ready > 0),
        }
    }
}

impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut budget = WRITE_BUDGET;
        self.write_within(bytes, &mut budget)
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        // One budget per record, not a fresh timeout for every partial write.
        // poll accommodates normally draining pipelines without unbounded queue growth.
        let mut budget = WRITE_BUDGET;
        while !bytes.is_empty() {
            match self.write_within(bytes, &mut budget)? {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                written => bytes = &bytes[written..],
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        // File is unbuffered; never introduce a blocking fsync on an inherited sink.
        Ok(())
    }
}

use serde::Serialize;
use serde_json::{Value, json};
use tickloop_agent_protocol::{ExecutionRef, Receipt};

use crate::OutputFormat;

pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct PrintResult {
    pub schema_version: u8,
    pub status: String,
    pub exit_code: i32,
    pub execution: Option<ExecutionRef>,
    pub chat_id: Option<uuid::Uuid>,
    pub loop_id: Option<uuid::Uuid>,
    pub run_id: Option<uuid::Uuid>,
    pub text: String,
    pub error: Option<String>,
    pub usage: Option<Value>,
}

impl PrintResult {
    pub fn failure(code: i32, message: impl Into<String>) -> Self {
        Self {
            schema_version: 1,
            status: match code {
                3 => "human_input_required",
                124 => "timeout",
                130 | 143 => "cancelled",
                _ => "failed",
            }
            .into(),
            exit_code: code,
            execution: None,
            chat_id: None,
            loop_id: None,
            run_id: None,
            text: String::new(),
            error: Some(message.into()),
            usage: None,
        }
    }

    pub fn identify(&mut self, execution: ExecutionRef, receipt: Option<&Receipt>) {
        self.execution = Some(execution);
        if let Some(receipt) = receipt {
            self.chat_id = Some(receipt.chat_id);
            self.loop_id = Some(receipt.loop_id);
            self.run_id = Some(receipt.run_id);
        }
    }
}

pub(crate) struct Output<'a, W> {
    pub writer: &'a mut W,
    pub format: OutputFormat,
    sequence: u64,
    bytes_written: usize,
    pub execution: Option<ExecutionRef>,
    pub receipt: Option<Receipt>,
}

impl<'a, W: Write> Output<'a, W> {
    pub fn new(writer: &'a mut W, format: OutputFormat) -> Self {
        Self {
            writer,
            format,
            sequence: 0,
            bytes_written: 0,
            execution: None,
            receipt: None,
        }
    }

    pub fn event(
        &mut self,
        kind: &str,
        source_sequence: Option<u64>,
        data: Value,
    ) -> io::Result<()> {
        if self.format != OutputFormat::StreamJson {
            return Ok(());
        }
        self.sequence += 1;
        self.write_json(&json!({
            "schema_version": 1,
            "type": kind,
            "sequence": self.sequence,
            "source_sequence": source_sequence,
            "execution": self.execution,
            "chat_id": self.receipt.as_ref().map(|receipt| receipt.chat_id),
            "loop_id": self.receipt.as_ref().map(|receipt| receipt.loop_id),
            "run_id": self.receipt.as_ref().map(|receipt| receipt.run_id),
            "data": data,
        }))
    }

    pub fn finish(&mut self, result: &PrintResult) -> io::Result<()> {
        match self.format {
            OutputFormat::Text if result.exit_code == 0 => {
                self.write_bytes(result.text.as_bytes())?;
                if !result.text.ends_with('\n') {
                    self.write_bytes(b"\n")?;
                }
                self.writer.flush()
            }
            OutputFormat::Text => Ok(()),
            OutputFormat::Json => self.write_json(result),
            OutputFormat::StreamJson => self.event("result", None, json!(result)),
        }
    }

    fn write_json(&mut self, value: &impl Serialize) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.write_bytes(&bytes)?;
        self.writer.flush()
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > MAX_OUTPUT_BYTES.saturating_sub(self.bytes_written) {
            return Err(io::Error::other("stdout exceeds the 16 MiB output limit"));
        }
        self.bytes_written += bytes.len();
        self.writer.write_all(bytes)
    }
}

pub(crate) fn report(message: &str) {
    // Diagnostics must not panic or hang when stderr is closed/full, including `2>&1`.
    let Ok(fd) = std::io::stderr().as_fd().try_clone_to_owned() else {
        return;
    };
    let mut writer = BoundedOutput::new(fd);
    let mut line: String = diagnostic(message).chars().take(16 * 1024).collect();
    line.push('\n');
    let _ = writer.write_all(line.as_bytes());
}

pub(crate) fn diagnostic(message: &str) -> String {
    message
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}
