mod device;
mod oauth;
pub mod storage;
mod ui;
pub use device::{AuthEndpoints, prepare_region, run_login};
pub use oauth::authorized_oauth;
pub use ui::{LoginOutcome, run as run_tui};

use std::io::Write;

use crate::{
    cli::AuthAction,
    config::Region,
    error::{Error, Result},
};
use storage::{AuthStorage, now_seconds};

pub const NONINTERACTIVE_AUTH_ENV: &str = "TICKLOOP_NONINTERACTIVE_AUTH";

pub fn check_noninteractive(storage: &AuthStorage) -> Result<()> {
    let token = storage
        .token()?
        .ok_or_else(|| Error::auth("not authorized"))?;
    if token.expires_at <= now_seconds().saturating_add(oauth::SDK_REFRESH_WINDOW_SECONDS + 1) {
        return Err(Error::auth("authorization expired"));
    }
    Ok(())
}

pub fn inspect(
    action: AuthAction,
    storage: &AuthStorage,
    region: Option<Region>,
    output: &mut impl Write,
) -> Result<()> {
    match action {
        AuthAction::Status => {
            match storage.token()? {
                None => writeln!(output, "Not logged in. Run `tick auth`.")?,
                Some(token) => {
                    writeln!(
                        output,
                        "{}",
                        if token.expires_at > now_seconds() {
                            "Logged in"
                        } else {
                            "Authorization expired; run `tick auth`"
                        }
                    )?;
                    let expires =
                        time::OffsetDateTime::from_unix_timestamp(token.expires_at as i64)
                            .ok()
                            .and_then(|date| {
                                date.format(&time::format_description::well_known::Rfc3339)
                                    .ok()
                            })
                            .unwrap_or_else(|| token.expires_at.to_string());
                    writeln!(output, "Expires: {expires}")?;
                }
            }
            writeln!(
                output,
                "Region: {}",
                region.map_or("not configured", Region::as_str)
            )?;
        }
        AuthAction::Check => check_noninteractive(storage)?,
        AuthAction::Logout => {
            storage.logout()?;
            writeln!(output, "Logged out")?;
        }
    }
    Ok(())
}

pub fn default_storage() -> Result<AuthStorage> {
    let home =
        std::env::home_dir().ok_or_else(|| Error::usage("cannot determine home directory"))?;
    Ok(AuthStorage::in_home(&home))
}
