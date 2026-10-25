#!/bin/sh
set -eu

launch_directory=$(pwd -P)
repository_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd -P)
target_directory=${CARGO_TARGET_DIR:-$repository_root/target}

cd "$repository_root"
mkdir -p "$repository_root/target/dev-runs"
run_directory=$(mktemp -d "$repository_root/target/dev-runs/run.XXXXXX")
bin_directory="$run_directory/bin"
trap 'rm -rf "$run_directory"' 0
trap 'exit 1' 1 2 15

cargo build --locked --target-dir "$target_directory" \
    -p tickloop-code --bin tickloop -p tickloop-tickhost --bin tickhost
mkdir "$bin_directory"
for program in tickloop tickhost; do
    cp "$target_directory/debug/$program" "$bin_directory/$program"
    chmod 0500 "$bin_directory/$program"
done

# Detached daemons and workers can still exec this bundle after the View exits.
trap - 0 1 2 15
cd "$launch_directory"
if "$bin_directory/tickloop" tick list 2>/dev/null | grep -qx 'No Tick packages are installed.'; then
    echo 'No Tick packages are installed: install them from /ticks in the Agent.' >&2
fi
printf 'Development executable: %s\n' "$bin_directory/tickloop" >&2
exec "$bin_directory/tickloop" --development-root "$repository_root" "$@"
