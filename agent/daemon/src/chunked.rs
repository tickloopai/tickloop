use std::{
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
};

use tokio_util::sync::CancellationToken;

use tickloop_agent_protocol::{AccessError, ServerMessage};

/// A response encoded to a temporary file before it goes out in one frame or in chunks. Only a
/// chunked response is digested, as its chunks are read, so the peer can verify the bytes it
/// reassembles; a single frame needs no digest.
pub(super) struct EncodedResponse {
    pub file: File,
    pub total_bytes: u64,
}

struct CountingWriter {
    file: File,
    bytes: u64,
    cancel: CancellationToken,
}

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "response encoding canceled",
            ));
        }
        let written = self.file.write(bytes)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl EncodedResponse {
    pub fn new(response: ServerMessage, cancel: CancellationToken) -> Result<Self, AccessError> {
        let file = tempfile::tempfile().map_err(storage_error)?;
        let mut writer = BufWriter::new(CountingWriter {
            file,
            bytes: 0,
            cancel,
        });
        serde_json::to_writer(&mut writer, &response)
            .map_err(|error| AccessError::invalid(error.to_string()))?;
        writer.flush().map_err(storage_error)?;
        let mut writer = writer
            .into_inner()
            .map_err(|error| storage_error(error.into_error()))?;
        writer
            .file
            .seek(SeekFrom::Start(0))
            .map_err(storage_error)?;
        Ok(Self {
            file: writer.file,
            total_bytes: writer.bytes,
        })
    }
}

fn storage_error(error: std::io::Error) -> AccessError {
    AccessError::unavailable(format!("temporary AgentAccess response storage: {error}"))
}
