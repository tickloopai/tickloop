//! Leaf utilities shared by every Agent crate: private files, credentials, timestamps, HTTP policy, the
//! loopback OAuth callback, images and output styles.

pub mod credentials;
pub mod image;
pub mod network;
pub mod oauth_callback;
pub mod output_style;
pub mod private_fs;
pub mod timestamp;
