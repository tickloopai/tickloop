use std::path::PathBuf;
use tickloop_harness::base::private_fs::{PrivateDirectory, current_uid};
const TEMP_DIRECTORY_ENV: &str = "TICKLOOP_TMPDIR";
const DEFAULT_TEMP_DIRECTORY: &str = "/tmp";
const PRIVATE_TEMP_DIRECTORY_PREFIX: &str = "tickloop-private-";
const PRIVATE_TICKHOST_PREFIX: &str = "tickhost-";
pub(crate) const TICKHOST_RUNTIME_SUBDIRECTORY: &str = "host";
/// `<temp dir>/tickloop-private-<uid>/tickhost-<random>`: the private runtime directory of the
/// Tickhost used by an Agent with no daemon that has its own Tickhost installed (such as in tests),
/// one per installation. Falls back to the default `/tmp` when the temp dir is too long to fit the
/// socket inside it.
pub(crate) fn private_tickhost_directory() -> PrivateDirectory {
    const NAME_CHARACTERS: usize = 8;
    let name = format!(
        "{PRIVATE_TICKHOST_PREFIX}{}",
        &tickloop_ids::random_token()[..NAME_CHARACTERS]
    );
    let preferred = private_temp_root().join(&name);
    let socket = preferred
        .path()
        .join(TICKHOST_RUNTIME_SUBDIRECTORY)
        .join(tickhost::paths::SOCKET_NAME);
    if socket.as_os_str().len() <= tickhost::paths::MAX_SOCKET_PATH_BYTES {
        return preferred;
    }
    let base = PathBuf::from(DEFAULT_TEMP_DIRECTORY);
    PrivateDirectory::anchored(
        base.canonicalize().unwrap_or(base),
        format!("{PRIVATE_TEMP_DIRECTORY_PREFIX}{}", current_uid()),
    )
    .join(name)
}

fn private_temp_root() -> PrivateDirectory {
    let base = std::env::var_os(TEMP_DIRECTORY_ENV)
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_TEMP_DIRECTORY), PathBuf::from);
    let base = base.canonicalize().unwrap_or(base);
    PrivateDirectory::anchored(
        base,
        format!("{PRIVATE_TEMP_DIRECTORY_PREFIX}{}", current_uid()),
    )
}
