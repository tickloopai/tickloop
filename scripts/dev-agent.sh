#!/bin/sh
set -eu

launch_directory=$(pwd -P)
repository_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd -P)
target_directory=${CARGO_TARGET_DIR:-$repository_root/target}

dev_worktree_root=$repository_root
if [ -e "$repository_root/.git" ]; then
    dev_worktree_root=$(git -C "$repository_root" rev-parse --show-toplevel)
    dev_worktree_root=$(CDPATH= cd "$dev_worktree_root" && pwd -P)
fi
development_revision() {
    if [ ! -e "$repository_root/.git" ]; then
        printf '%s\n' source-archive
    elif dev_branch=$(git -C "$repository_root" symbolic-ref --quiet HEAD); then
        printf 'branch:%s\n' "$dev_branch"
    elif dev_commit=$(git -C "$repository_root" rev-parse --verify HEAD); then
        printf 'detached:%s\n' "$dev_commit"
    else
        echo 'Cannot identify the development checkout branch or detached commit.' >&2
        return 1
    fi
}
dev_revision=$(development_revision)
dev_user_id=$(id -u)
if command -v sha256sum >/dev/null 2>&1; then
    dev_digest=$(printf '%s\000%s\000%s\000' "$dev_user_id" "$dev_worktree_root" "$dev_revision" | sha256sum)
elif command -v shasum >/dev/null 2>&1; then
    dev_digest=$(printf '%s\000%s\000%s\000' "$dev_user_id" "$dev_worktree_root" "$dev_revision" | shasum -a 256)
else
    echo 'Development runtime isolation requires sha256sum or shasum.' >&2
    exit 1
fi
dev_namespace_key=$(printf '%.16s' "$dev_digest")
dev_namespace="/tmp/tickloop-dev-$dev_user_id-$dev_namespace_key"
if [ -n "${TICKLOOP_DAEMON_RUNTIME_DIR:-}" ]; then
    dev_tickhost_namespace="$TICKLOOP_DAEMON_RUNTIME_DIR/tickhost"
else
    dev_tickhost_namespace="/tmp/tlh-$dev_namespace_key"
fi
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    target_directory="$CARGO_TARGET_DIR/tickloop-dev-$dev_namespace_key"
fi
# Nonempty caller overrides deliberately choose their own service namespace. Settings and
# installed Tick packages continue to use the caller's existing global configuration.
export TICKLOOP_DAEMON_RUNTIME_DIR="${TICKLOOP_DAEMON_RUNTIME_DIR:-$dev_namespace}"
export TICKLOOP_TICKHOST_RUNTIME_DIR="${TICKLOOP_TICKHOST_RUNTIME_DIR:-$dev_tickhost_namespace}"

cd "$repository_root"
mkdir -p "$repository_root/target/dev-runs"
run_directory=$(mktemp -d "$repository_root/target/dev-runs/run.XXXXXX")
bin_directory="$run_directory/bin"
trap 'rm -rf "$run_directory"' 0
trap 'exit 1' 1 2 15

cargo build --locked --target-dir "$target_directory" \
    -p tickloop-agent --bin tickloop -p tickloop-tickhost --bin tickhost \
    -p tickloop-tick-reader --bin tick-reader
if [ "$(development_revision)" != "$dev_revision" ]; then
    echo 'The development checkout changed branches during the build; run make dev again.' >&2
    exit 1
fi
mkdir "$bin_directory"
for program in tickloop tickhost tick-reader; do
    cp "$target_directory/debug/$program" "$bin_directory/$program"
    chmod 0500 "$bin_directory/$program"
done

# Detached daemons and workers can still exec this bundle after the View exits.
trap - 0 1 2 15
cd "$launch_directory"
if "$bin_directory/tickloop" tick list 2>/dev/null | grep -qx 'No Tick packages are installed.'; then
    echo 'No Tick packages are installed: run make dev-ticks to open Longbridge and Futu Ticks.' >&2
fi
printf 'Development executable: %s\n' "$bin_directory/tickloop" >&2
exec "$bin_directory/tickloop" --development-root "$repository_root" "$@"
