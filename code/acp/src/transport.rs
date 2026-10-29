//! Limits are outside the SDK's unbounded internal channels, before a frame is admitted.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    sync::{Arc, Mutex},
};

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

pub const MAX_FRAME: usize = 24 * 1024 * 1024;
pub const MAX_IN_FLIGHT: usize = 64;
const MAX_QUEUED_BYTES: usize = 48 * 1024 * 1024;
const MAX_CONTROL_FRAME: usize = 16 * 1024;

#[derive(Default)]
pub(crate) struct Admission {
    requests: HashMap<String, usize>,
    queued_bytes: usize,
    permission_requests: HashSet<String>,
    notifications: usize,
}

pub(crate) type SharedAdmission = Arc<Mutex<Admission>>;

/// Reserve before entering the SDK's unbounded outgoing channels; release at the writer.
#[derive(Clone)]
pub(crate) struct OutputBudget {
    bytes: Arc<tokio::sync::Semaphore>,
    pending: Arc<Mutex<HashMap<String, VecDeque<tokio::sync::OwnedSemaphorePermit>>>>,
}
impl Default for OutputBudget {
    fn default() -> Self {
        Self {
            bytes: Arc::new(tokio::sync::Semaphore::new(32 * 1024 * 1024)),
            pending: Arc::default(),
        }
    }
}
impl OutputBudget {
    fn emit(
        &self,
        connection: &agent_client_protocol::ConnectionTo<agent_client_protocol::Client>,
        session_id: &str,
        value: Value,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<(), agent_client_protocol::Error> {
        let notification =
            super::decode::<agent_client_protocol::schema::v1::SessionNotification>(value)?;
        let mut pending = self.pending.lock().expect("ACP output budget");
        pending
            .entry(session_id.to_owned())
            .or_default()
            .push_back(permit);
        if let Err(error) = connection.send_notification(notification) {
            pending.get_mut(session_id).unwrap().pop_back();
            return Err(error);
        }
        Ok(())
    }
    pub(crate) fn send(
        &self,
        connection: &agent_client_protocol::ConnectionTo<agent_client_protocol::Client>,
        session_id: &str,
        update: Value,
    ) -> Result<(), agent_client_protocol::Error> {
        let value = serde_json::json!({"sessionId":session_id,"update":update});
        let size = value.to_string().len().saturating_add(128);
        let permit = self
            .bytes
            .clone()
            .try_acquire_many_owned(
                size.try_into()
                    .map_err(|_| super::invalid("ACP output size limit"))?,
            )
            .map_err(|_| super::invalid("ACP output queue byte limit"))?;
        self.emit(connection, session_id, value, permit)
    }
    pub(crate) async fn send_wait(
        &self,
        connection: &agent_client_protocol::ConnectionTo<agent_client_protocol::Client>,
        session_id: &str,
        update: Value,
    ) -> Result<(), agent_client_protocol::Error> {
        let value = serde_json::json!({"sessionId":session_id,"update":update});
        let size = value.to_string().len().saturating_add(128);
        if size > MAX_FRAME {
            return Err(super::invalid("ACP output frame limit"));
        }
        let permit = self
            .bytes
            .clone()
            .acquire_many_owned(size as u32)
            .await
            .map_err(|_| super::invalid("ACP output closed"))?;
        self.emit(connection, session_id, value, permit)
    }
    fn written(&self, line: &str) {
        if let Ok(value) = serde_json::from_str::<Value>(line)
            && value["method"] == "session/update"
            && let Some(session_id) = value["params"]["sessionId"].as_str()
            && let Some(queue) = self
                .pending
                .lock()
                .expect("ACP output budget")
                .get_mut(session_id)
        {
            queue.pop_front();
        }
    }
}

impl Admission {
    /// `Ok(false)` drops a notification this agent does not handle: notifications get no reply,
    /// and ACP requires unrecognized ones to be ignored rather than end the connection.
    fn incoming(&mut self, line: &str) -> io::Result<bool> {
        if line.len() > MAX_FRAME {
            return Err(io::Error::other("ACP input frame exceeds limit"));
        }
        // Account for retained request payloads until their responses leave the SDK.
        let value: Value = serde_json::from_str(line).map_err(io::Error::other)?;
        if !value.is_object() {
            return Err(io::Error::other("ACP batches are not supported"));
        }
        if value.get("method").is_some() {
            if let Some(id) = value.get("id") {
                if !id.is_number() && !id.is_string() {
                    return Err(io::Error::other("invalid ACP request id"));
                }
                let id = id.to_string();
                if id.len() > 256
                    || self.requests.len() >= MAX_IN_FLIGHT
                    || self.requests.contains_key(&id)
                {
                    return Err(io::Error::other(
                        "ACP in-flight limit or duplicate request id",
                    ));
                }
                if self.queued_bytes.saturating_add(line.len()) > MAX_QUEUED_BYTES {
                    return Err(io::Error::other("ACP aggregate request byte limit"));
                }
                self.requests.insert(id, line.len());
                self.queued_bytes += line.len();
            } else {
                // Only cancellations are handled; the SDK itself applies `$/cancel_request` to
                // the request it names. A malformed one would never reach the handler that
                // releases its count, so it is dropped like any other unhandled notification.
                let params = &value["params"];
                let handled = match value["method"].as_str() {
                    Some("session/cancel") => params["sessionId"].is_string(),
                    Some("$/cancel_request") => {
                        params["requestId"].is_i64() || params["requestId"].is_string()
                    }
                    _ => false,
                };
                if !handled {
                    return Ok(false);
                }
                // Limit queued cancels too.
                if self.notifications >= MAX_IN_FLIGHT || line.len() > MAX_CONTROL_FRAME {
                    return Err(io::Error::other("ACP cancellation queue limit"));
                }
                self.notifications += 1;
            }
        } else if let Some(id) = value.get("id") {
            if line.len() > MAX_CONTROL_FRAME || !self.permission_requests.remove(&id.to_string()) {
                return Err(io::Error::other("unsolicited or duplicate ACP response"));
            }
        } else {
            return Err(io::Error::other(
                "ACP frame has neither method nor response id",
            ));
        }
        Ok(true)
    }

    fn outgoing(&mut self, line: &str) -> io::Result<()> {
        if line.len() > MAX_FRAME {
            return Err(io::Error::other("ACP output frame exceeds limit"));
        }
        let value: Value = serde_json::from_str(line).map_err(io::Error::other)?;
        if value.get("method").is_none()
            && let Some(id) = value.get("id")
        {
            if let Some(bytes) = self.requests.remove(&id.to_string()) {
                self.queued_bytes = self.queued_bytes.saturating_sub(bytes);
            }
        } else if let Some(id) = value.get("id") {
            if self.permission_requests.len() >= MAX_IN_FLIGHT {
                return Err(io::Error::other("ACP outbound request limit"));
            }
            self.permission_requests.insert(id.to_string());
        }
        Ok(())
    }

    pub(crate) fn acknowledge_cancel(&mut self) {
        self.notifications = self.notifications.saturating_sub(1);
    }
}

pub(crate) async fn connect<I, O>(
    input: I,
    output: O,
    admission: SharedAdmission,
    output_budget: OutputBudget,
    builder: impl agent_client_protocol::ConnectTo<agent_client_protocol::Client>,
) -> Result<(), agent_client_protocol::Error>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
{
    let incoming_admission = admission.clone();
    let input = FramedRead::new(input, LinesCodec::new_with_max_length(MAX_FRAME)).filter_map(
        move |line| {
            let admitted = line.map_err(io::Error::other).and_then(|line| {
                let admitted = incoming_admission
                    .lock()
                    .expect("ACP admission lock")
                    .incoming(&line)?;
                Ok(admitted.then_some(line))
            });
            std::future::ready(admitted.transpose())
        },
    );
    let framed = FramedWrite::new(output, LinesCodec::new());
    let output =
        <_ as SinkExt<String>>::sink_map_err(framed, io::Error::other).with(move |line: String| {
            let result = admission
                .lock()
                .expect("ACP admission lock")
                .outgoing(&line);
            // At most one additional bounded frame is now held by FramedWrite.
            output_budget.written(&line);
            async move { result.map(|()| line) }
        });
    builder
        .connect_to(agent_client_protocol::Lines::new(output, input))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_bytes_are_released_only_by_matching_responses() {
        let mut admission = Admission::default();
        let line = r#"{"id":1,"method":"session/prompt","params":{"prompt":[]}}"#;
        assert!(admission.incoming(line).unwrap());
        assert_eq!(admission.queued_bytes, line.len());
        admission.outgoing(r#"{"id":2,"result":{}}"#).unwrap();
        assert_eq!(admission.queued_bytes, line.len());
        admission.outgoing(r#"{"id":1,"result":{}}"#).unwrap();
        assert_eq!(admission.queued_bytes, 0);
        admission.queued_bytes = MAX_QUEUED_BYTES;
        assert!(admission.incoming(line).is_err());
        assert!(admission.requests.is_empty());
    }

    #[test]
    fn bounds_and_duplicate_ids() {
        let mut admission = Admission::default();
        admission
            .incoming(r#"{"id":1,"method":"initialize"}"#)
            .unwrap();
        assert!(
            admission
                .incoming(r#"{"id":1,"method":"initialize"}"#)
                .is_err()
        );
        admission.outgoing(r#"{"id":1,"result":{}}"#).unwrap();
        admission
            .incoming(r#"{"id":1,"method":"initialize"}"#)
            .unwrap();
        for id in 2..=MAX_IN_FLIGHT {
            admission
                .incoming(&format!(r#"{{"id":{id},"method":"initialize"}}"#))
                .unwrap();
        }
        assert!(
            admission
                .incoming(r#"{"id":999,"method":"initialize"}"#)
                .is_err()
        );
        assert!(admission.outgoing(&"x".repeat(MAX_FRAME + 1)).is_err());
    }

    #[test]
    fn unknown_notifications_are_dropped_and_cancellations_are_bounded() {
        let mut admission = Admission::default();
        for ignored in [
            r#"{"jsonrpc":"2.0","method":"_vendor/ping","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"$/progress"}"#,
            r#"{"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":1.5}}"#,
            r#"{"jsonrpc":"2.0","method":"session/cancel","params":{}}"#,
        ] {
            assert!(!admission.incoming(ignored).unwrap(), "{ignored}");
        }
        assert_eq!(admission.notifications, 0);
        let cancel = r#"{"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":"a"}}"#;
        for _ in 0..MAX_IN_FLIGHT {
            assert!(admission.incoming(cancel).unwrap());
        }
        assert!(admission.incoming(cancel).is_err());
        admission.acknowledge_cancel();
        assert!(
            admission
                .incoming(
                    r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#
                )
                .unwrap()
        );
    }
}
