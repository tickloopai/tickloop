//! Transport limits of an AgentAccess connection. The serving side chooses them and announces
//! them in its Hello; every later frame on the connection stays within them.

use serde::{Deserialize, Serialize};

/// Every valid frame limit admits a frame of this size, so peers exchange their Hellos within it
/// before the connection's limits are known.
pub const HANDSHAKE_FRAME_BYTES: usize = 64 * 1024;

pub const FEED_CAPACITY: usize = 128;
pub const FEED_MAX_BYTES: usize = 1024 * 1024;
/// The largest encoded change a feed forwards; a larger change is replaced by a resync.
pub const FEED_CHANGE_BYTES: usize = FEED_MAX_BYTES / FEED_CAPACITY;

/// Room for the message around a response chunk or a feed change within one frame.
const FRAME_ENVELOPE_BYTES: usize = 4 * 1024;

// Every valid frame limit holds one feed change with its envelope.
const _: () = assert!(FEED_CHANGE_BYTES + FRAME_ENVELOPE_BYTES <= HANDSHAKE_FRAME_BYTES);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedAccessLimits")]
pub struct AccessLimits {
    frame_bytes: usize,
    chunk_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AccessLimitsError {
    #[error("the frame limit is below the {HANDSHAKE_FRAME_BYTES}-byte handshake frame")]
    FrameBelowHandshake,
    #[error("the frame limit exceeds the 32-bit frame length prefix")]
    FrameBeyondLengthPrefix,
    #[error("the response chunk is empty")]
    EmptyChunk,
    #[error("an encoded response chunk does not fit in one frame")]
    ChunkBeyondFrame,
}

impl AccessLimits {
    pub const DEFAULT: Self = match Self::new(128 * 1024 * 1024, 4 * 1024 * 1024) {
        Ok(limits) => limits,
        Err(_) => panic!("the default AccessLimits are valid"),
    };

    pub const fn new(frame_bytes: usize, chunk_bytes: usize) -> Result<Self, AccessLimitsError> {
        if frame_bytes < HANDSHAKE_FRAME_BYTES {
            return Err(AccessLimitsError::FrameBelowHandshake);
        }
        if frame_bytes as u64 > u32::MAX as u64 {
            return Err(AccessLimitsError::FrameBeyondLengthPrefix);
        }
        if chunk_bytes == 0 {
            return Err(AccessLimitsError::EmptyChunk);
        }
        if chunk_bytes.div_ceil(3) * 4 + FRAME_ENVELOPE_BYTES > frame_bytes {
            return Err(AccessLimitsError::ChunkBeyondFrame);
        }
        Ok(Self {
            frame_bytes,
            chunk_bytes,
        })
    }

    pub const fn frame_bytes(self) -> usize {
        self.frame_bytes
    }

    pub const fn chunk_bytes(self) -> usize {
        self.chunk_bytes
    }
}

/// Small limits that let unit tests cross the frame and chunk boundaries with little data.
#[cfg(test)]
pub(crate) const TRANSPORT_LIMITS: AccessLimits = match AccessLimits::new(64 << 10, 16 << 10) {
    Ok(limits) => limits,
    Err(_) => panic!("the test AccessLimits are valid"),
};

#[derive(Deserialize)]
struct UncheckedAccessLimits {
    frame_bytes: usize,
    chunk_bytes: usize,
}

impl TryFrom<UncheckedAccessLimits> for AccessLimits {
    type Error = AccessLimitsError;

    fn try_from(unchecked: UncheckedAccessLimits) -> Result<Self, Self::Error> {
        Self::new(unchecked.frame_bytes, unchecked.chunk_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_a_128_mib_frame_with_4_mib_chunks() {
        assert_eq!(AccessLimits::DEFAULT.frame_bytes(), 128 * 1024 * 1024);
        assert_eq!(AccessLimits::DEFAULT.chunk_bytes(), 4 * 1024 * 1024);
    }

    #[test]
    fn a_frame_must_hold_the_handshake_and_fit_the_length_prefix() {
        assert_eq!(
            AccessLimits::new(HANDSHAKE_FRAME_BYTES - 1, 1024),
            Err(AccessLimitsError::FrameBelowHandshake)
        );
        assert!(AccessLimits::new(HANDSHAKE_FRAME_BYTES, 1024).is_ok());
        assert!(AccessLimits::new(u32::MAX as usize, 4 * 1024 * 1024).is_ok());
        assert_eq!(
            AccessLimits::new(u32::MAX as usize + 1, 4 * 1024 * 1024),
            Err(AccessLimitsError::FrameBeyondLengthPrefix)
        );
    }

    #[test]
    fn an_encoded_chunk_and_its_envelope_must_fit_one_frame() {
        let largest_chunk = (HANDSHAKE_FRAME_BYTES - FRAME_ENVELOPE_BYTES) / 4 * 3;
        assert!(AccessLimits::new(HANDSHAKE_FRAME_BYTES, largest_chunk).is_ok());
        assert_eq!(
            AccessLimits::new(HANDSHAKE_FRAME_BYTES, largest_chunk + 1),
            Err(AccessLimitsError::ChunkBeyondFrame)
        );
        assert_eq!(
            AccessLimits::new(HANDSHAKE_FRAME_BYTES, 0),
            Err(AccessLimitsError::EmptyChunk)
        );
    }

    #[test]
    fn deserializing_validates_like_new() {
        let limits = AccessLimits::new(HANDSHAKE_FRAME_BYTES, 16 * 1024).unwrap();
        let encoded = serde_json::to_value(limits).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({"frame_bytes": HANDSHAKE_FRAME_BYTES, "chunk_bytes": 16 * 1024})
        );
        assert_eq!(
            serde_json::from_value::<AccessLimits>(encoded).unwrap(),
            limits
        );
        let error = serde_json::from_value::<AccessLimits>(
            serde_json::json!({"frame_bytes": 1024, "chunk_bytes": 16}),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            AccessLimitsError::FrameBelowHandshake.to_string()
        );
    }
}
