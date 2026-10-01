use std::process::ExitCode;

pub const SUCCESS: u8 = 0;
pub const USAGE: u8 = 2;
pub const NOT_AUTHORIZED: u8 = 3;
pub const NETWORK: u8 = 4;

pub const REFERENCE: &str = "Exit codes:
  0  success
  2  usage or configuration error
  3  not authorized; run `tick auth`
  4  network or API error";

pub fn exit(code: u8) -> ExitCode {
    ExitCode::from(code)
}
