//! Every tick/oauth integration test, compiled as one binary, against a local authorization
//! server and a private config root.

#[path = "all/device.rs"]
mod device;
#[path = "all/support.rs"]
mod support;
#[path = "all/token.rs"]
mod token;
