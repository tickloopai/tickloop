use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CONTROL_MAJOR: u16 = 1;
pub const CONTROL_FRAME_BYTES: usize = 64 * 1024;

#[derive(Deserialize, Serialize)]
pub enum Request {
    Hello {
        major: u16,
        minor: u16,
    },
    ShutdownDaemon {
        request_id: Uuid,
        daemon_epoch: Uuid,
    },
}

#[derive(Deserialize, Serialize)]
pub struct Hello {
    pub major: u16,
    pub minor: u16,
    pub daemon_epoch: Uuid,
}

#[derive(Deserialize, Serialize)]
pub struct Error {
    pub code: String,
    pub message: String,
}

#[derive(Deserialize, Serialize)]
pub enum Reply {
    Hello(Hello),
    DaemonStopped {
        request_id: Uuid,
    },
    Error {
        request_id: Option<Uuid>,
        error: Error,
    },
}
