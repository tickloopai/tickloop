//! Every protocol integration test, compiled as one binary.

#[path = "all/access_round_trip.rs"]
mod access_round_trip;
#[path = "all/chat_status.rs"]
mod chat_status;
#[path = "all/local_messages.rs"]
mod local_messages;
#[path = "all/scheduled_tasks.rs"]
mod scheduled_tasks;
#[path = "all/wire_golden.rs"]
mod wire_golden;
