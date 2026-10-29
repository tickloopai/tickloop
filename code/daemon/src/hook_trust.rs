//! `/hooks` and the review a New Agent offers: the hooks configured for the Folder a launch starts
//! in, read and trusted through the Settings Store of the configuration root it names. Hooks
//! belong to the Folder, not to one Agent, so the same request serves before and after an Agent
//! starts there.

use std::path::PathBuf;

use tickloop_code_access::{AccessError, AccessErrorCode, LaunchSpec};
use tickloop_code_settings::UserSettings;
use tickloop_harness::policy::settings::{SettingsStore, SettingsStoreError};
use tickloop_harness::protocol::hooks::{HookList, HookTrustChange};

fn settings_store(launch: &LaunchSpec) -> Result<(UserSettings, PathBuf), AccessError> {
    let root = launch
        .config_root
        .as_ref()
        .ok_or_else(|| AccessError::unavailable("hooks need a state directory"))?;
    let settings = UserSettings::in_root(root).with_home_directory(std::env::home_dir());
    Ok((settings, launch.folder.clone()))
}

pub(crate) async fn hooks(launch: &LaunchSpec) -> Result<HookList, AccessError> {
    let (settings, folder) = settings_store(launch)?;
    tokio::task::spawn_blocking(move || {
        let store: &dyn SettingsStore = &settings;
        store.hooks(Some(&folder))
    })
    .await
    .map_err(|error| AccessError::unavailable(error.to_string()))
}

/// Applies `changes` and returns the hooks as they then stand.
pub(crate) async fn write_trust(
    launch: &LaunchSpec,
    changes: Vec<HookTrustChange>,
    expected_revision: Option<String>,
) -> Result<HookList, AccessError> {
    let (settings, folder) = settings_store(launch)?;
    tokio::task::spawn_blocking(move || {
        let store: &dyn SettingsStore = &settings;
        store
            .write_hook_trust(Some(&folder), &changes, expected_revision.as_deref())
            .map_err(|error| match error {
                SettingsStoreError::Conflict => AccessError::new(
                    AccessErrorCode::VersionConflict,
                    "hook trust changed while reviewing; look at the hooks again",
                ),
                SettingsStoreError::Storage(message) => AccessError::storage(message),
            })?;
        Ok(store.hooks(Some(&folder)))
    })
    .await
    .map_err(|error| AccessError::unavailable(error.to_string()))?
}
