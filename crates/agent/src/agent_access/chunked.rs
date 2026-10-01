use std::{
    fs::File,
    io::{BufReader, BufWriter, Seek, SeekFrom, Write},
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::protocol::*;

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

pub(super) struct IncomingTransfer {
    pub transfer_id: Uuid,
    pub request_id: Uuid,
    kind: ChunkedResponseKind,
    file: File,
    digest: Sha256,
    total_bytes: u64,
    received_bytes: u64,
    chunk_bytes: usize,
}

impl IncomingTransfer {
    pub fn new(
        transfer_id: Uuid,
        request_id: Uuid,
        kind: ChunkedResponseKind,
        total_bytes: u64,
        chunk_bytes: usize,
    ) -> Result<Self, AccessError> {
        if total_bytes <= MAX_FRAME_BYTES as u64 || chunk_bytes != RESPONSE_CHUNK_BYTES {
            return Err(AccessError::invalid("invalid chunked response limits"));
        }
        Ok(Self {
            transfer_id,
            request_id,
            kind,
            file: tempfile::tempfile().map_err(storage_error)?,
            digest: Sha256::new(),
            total_bytes,
            received_bytes: 0,
            chunk_bytes,
        })
    }

    pub fn append(
        &mut self,
        transfer_id: Uuid,
        offset: u64,
        base64: String,
    ) -> Result<(), AccessError> {
        if transfer_id != self.transfer_id
            || offset != self.received_bytes
            || base64.len() > self.chunk_bytes.div_ceil(3) * 4
        {
            return Err(AccessError::invalid("chunked response is out of sequence"));
        }
        let bytes = BASE64
            .decode(base64)
            .map_err(|_| AccessError::invalid("invalid response chunk encoding"))?;
        let remaining = self.total_bytes - self.received_bytes;
        let expected = remaining.min(self.chunk_bytes as u64);
        if bytes.len() as u64 != expected || bytes.is_empty() {
            return Err(AccessError::invalid(
                "chunked response has an invalid chunk length",
            ));
        }
        self.file.write_all(&bytes).map_err(storage_error)?;
        self.digest.update(&bytes);
        self.received_bytes += bytes.len() as u64;
        Ok(())
    }

    pub fn finish(
        mut self,
        transfer_id: Uuid,
        total_bytes: u64,
        sha256: String,
    ) -> Result<ServerMessage, AccessError> {
        if transfer_id != self.transfer_id
            || total_bytes != self.total_bytes
            || self.received_bytes != self.total_bytes
            || sha256 != format!("{:x}", self.digest.finalize())
        {
            return Err(AccessError::invalid(
                "chunked response is incomplete or its digest differs",
            ));
        }
        self.file.seek(SeekFrom::Start(0)).map_err(storage_error)?;
        let response: ServerMessage = serde_json::from_reader(BufReader::new(self.file))
            .map_err(|error| AccessError::invalid(error.to_string()))?;
        let request_id = match &response {
            ServerMessage::Opened { request_id, .. }
            | ServerMessage::Receipt { request_id, .. } => *request_id,
            _ => return Err(AccessError::invalid("invalid chunked response kind")),
        };
        if request_id != self.request_id || response_kind(&response) != Some(self.kind) {
            return Err(AccessError::invalid(
                "chunked response does not match its request",
            ));
        }
        Ok(response)
    }
}

pub(super) fn response_kind(response: &ServerMessage) -> Option<ChunkedResponseKind> {
    match response {
        ServerMessage::Opened { initial, .. } if history_value(initial) => {
            Some(ChunkedResponseKind::Opened)
        }
        ServerMessage::Receipt { receipt, .. } if history_receipt(receipt) => {
            Some(ChunkedResponseKind::Receipt)
        }
        _ => None,
    }
}

fn history_value(value: &ObservationValue) -> bool {
    match value {
        ObservationValue::Agent(_)
        | ObservationValue::Transcript(_)
        | ObservationValue::RewindPoints(_) => true,
        ObservationValue::Receipt(Some(receipt)) => history_receipt(receipt),
        _ => false,
    }
}

fn history_receipt(receipt: &CommandReceipt) -> bool {
    matches!(
        receipt.result,
        Some(
            CommandResult::Created { .. }
                | CommandResult::Loaded { .. }
                | CommandResult::Branch { .. }
                | CommandResult::Compact { .. }
                | CommandResult::DirectedSummary { .. }
                | CommandResult::Rewind { .. }
                | CommandResult::QueueTaken(_)
        )
    )
}

fn storage_error(error: std::io::Error) -> AccessError {
    AccessError::unavailable(format!("temporary AgentAccess response storage: {error}"))
}
