use std::{
    io::{BufWriter, Seek, SeekFrom, Write},
    pin::Pin,
    task::{Context, Poll},
};

use tempfile::{SpooledData, SpooledTempFile};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::sync::CancellationToken;

use tickloop_agent_protocol::{AccessError, ServerMessage};

pub(super) const RESPONSE_MEMORY_BYTES: usize = 8 * 1024 * 1024;

/// A response buffered in memory, spilling to a temporary file above the memory limit. Only a
/// chunked response is digested, as its chunks are read, so the peer can verify the bytes it
/// reassembles; a single frame needs no digest.
pub(super) struct EncodedResponse {
    pub storage: SpooledTempFile,
    pub total_bytes: u64,
}

struct CountingWriter {
    storage: SpooledTempFile,
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
        let written = self.storage.write(bytes)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.storage.flush()
    }
}

impl EncodedResponse {
    pub fn new(response: ServerMessage, cancel: CancellationToken) -> Result<Self, AccessError> {
        let storage = tempfile::spooled_tempfile(RESPONSE_MEMORY_BYTES);
        let mut writer = BufWriter::new(CountingWriter {
            storage,
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
            .storage
            .seek(SeekFrom::Start(0))
            .map_err(storage_error)?;
        Ok(Self {
            storage: writer.storage,
            total_bytes: writer.bytes,
        })
    }

    pub fn into_reader(self) -> impl AsyncRead + Unpin {
        match self.storage.into_inner() {
            SpooledData::InMemory(cursor) => ResponseReader::Memory(cursor),
            SpooledData::OnDisk(file) => ResponseReader::File(tokio::fs::File::from_std(file)),
        }
    }
}

enum ResponseReader {
    Memory(std::io::Cursor<Vec<u8>>),
    File(tokio::fs::File),
}

impl AsyncRead for ResponseReader {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Memory(reader) => Pin::new(reader).poll_read(context, buffer),
            Self::File(reader) => Pin::new(reader).poll_read(context, buffer),
        }
    }
}

fn storage_error(error: std::io::Error) -> AccessError {
    AccessError::unavailable(format!("temporary AgentAccess response storage: {error}"))
}

#[cfg(test)]
mod tests {
    use tickloop_agent_protocol::{
        Content, Cursor, Message, ObservationValue, RecordedMessage, RecordedTranscript, Role,
        TranscriptRef,
    };
    use tokio::io::AsyncReadExt;
    use uuid::Uuid;

    use super::*;

    fn history_response(text: String) -> ServerMessage {
        ServerMessage::Opened {
            request_id: Uuid::new_v4(),
            observation_id: Uuid::new_v4(),
            initial: ObservationValue::Transcript(Box::new(RecordedTranscript {
                goal_history: Vec::new(),
                record: TranscriptRef {
                    store_root: "/tmp/transcripts".into(),
                    chat_id: Uuid::new_v4(),
                },
                title: None,
                folder: "/tmp/workdir".into(),
                messages: vec![RecordedMessage {
                    id: Uuid::new_v4(),
                    message: Message {
                        role: Role::User,
                        content: vec![Content::Text { text }],
                    },
                    loop_id: None,
                    run_id: None,
                    timestamp: String::new(),
                }],
                total_messages: 1,
                warnings: Vec::new(),
            })),
            cursor: Cursor {
                daemon_epoch: Uuid::new_v4(),
                execution: None,
                sequence: 0,
            },
            reset: None,
            feed: false,
        }
    }

    #[tokio::test]
    async fn ordinary_history_responses_stay_in_memory() {
        let response = history_response("conversation text 中文\n".repeat(1024));
        let expected = serde_json::to_vec(&response).unwrap();
        let encoded = EncodedResponse::new(response, CancellationToken::new()).unwrap();

        assert!(
            !encoded.storage.is_rolled(),
            "ordinary history must not write a temporary file"
        );
        assert_eq!(encoded.total_bytes, expected.len() as u64);
        let mut actual = Vec::new();
        encoded
            .into_reader()
            .read_to_end(&mut actual)
            .await
            .unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn response_memory_limit_spills_only_above_the_boundary_without_losing_bytes() {
        let overhead = serde_json::to_vec(&history_response(String::new()))
            .unwrap()
            .len();
        for (total_bytes, spills) in [
            (RESPONSE_MEMORY_BYTES, false),
            (RESPONSE_MEMORY_BYTES + 1, true),
        ] {
            let response = history_response("x".repeat(total_bytes - overhead));
            let expected = serde_json::to_vec(&response).unwrap();
            assert_eq!(expected.len(), total_bytes);
            let encoded = EncodedResponse::new(response, CancellationToken::new()).unwrap();

            assert_eq!(encoded.storage.is_rolled(), spills);
            assert_eq!(encoded.total_bytes, total_bytes as u64);
            let mut actual = Vec::new();
            encoded
                .into_reader()
                .read_to_end(&mut actual)
                .await
                .unwrap();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn canceled_response_encoding_returns_no_response() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(EncodedResponse::new(history_response("text".into()), cancel).is_err());
    }
}
