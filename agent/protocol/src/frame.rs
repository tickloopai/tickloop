//! AgentAccess framing: each message is one JSON document behind a big-endian `u32` byte length,
//! and no frame exceeds the connection's frame limit. Transports read and write the bytes.

use serde::{Serialize, de::DeserializeOwned};

use crate::{AccessError, AccessErrorCode};

/// The JSON body of one frame for `value`, refused when it exceeds `frame_bytes`.
pub fn encode_frame<T: Serialize>(value: &T, frame_bytes: usize) -> Result<Vec<u8>, AccessError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| AccessError::invalid(error.to_string()))?;
    check_frame_length(bytes.len(), frame_bytes)?;
    Ok(bytes)
}

/// Refuses a frame whose announced or encoded body exceeds `frame_bytes`.
pub fn check_frame_length(length: usize, frame_bytes: usize) -> Result<(), AccessError> {
    if length > frame_bytes {
        return Err(AccessError::new(
            AccessErrorCode::PayloadTooLarge,
            "AgentAccess frame exceeds the transport limit",
        ));
    }
    Ok(())
}

pub fn decode_frame<T: DeserializeOwned>(body: &[u8]) -> Result<T, AccessError> {
    serde_json::from_slice(body).map_err(|error| AccessError::invalid(error.to_string()))
}
