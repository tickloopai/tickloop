use std::{
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use tickloop_agent_protocol::{AccessError, ServerMessage};

pub(super) struct EncodedResponse {
    pub file: File,
    pub total_bytes: u64,
    pub sha256: String,
}

struct DigestWriter {
    file: File,
    digest: Sha256,
    bytes: u64,
    cancel: CancellationToken,
}

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "response encoding canceled",
            ));
        }
        let written = self.file.write(bytes)?;
        self.digest.update(&bytes[..written]);
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
        let mut writer = BufWriter::new(DigestWriter {
            file,
            digest: Sha256::new(),
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
            sha256: format!("{:x}", writer.digest.finalize()),
        })
    }
}

fn storage_error(error: std::io::Error) -> AccessError {
    AccessError::unavailable(format!("temporary AgentAccess response storage: {error}"))
}
