#!/bin/sh
set -eu

launch_directory=$(pwd -P)
repository_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd -P)
cd "$repository_root"
mkdir -p "$repository_root/target/dev-runs"
run_directory=$(mktemp -d "$repository_root/target/dev-runs/run.XXXXXX")
build_directory="$run_directory/build"
bin_directory="$run_directory/bin"
trap 'rm -rf "$run_directory"' 0
trap 'exit 1' 1 2 15

cargo build --locked --target-dir "$build_directory" \
    -p tickloop-agent --bin tickloop -p tickloop-ticks --bin tick
mkdir "$bin_directory"
cp "$build_directory/debug/tickloop" "$bin_directory/tickloop"
cp "$build_directory/debug/tick" "$bin_directory/tick"
chmod 0500 "$bin_directory/tickloop" "$bin_directory/tick"
rm -rf "$build_directory"
chmod 0500 "$bin_directory" "$run_directory"

# Detached owners and workers can still exec this bundle after the View exits.
trap - 0 1 2 15
cd "$launch_directory"
printf 'Development executable: %s\n' "$bin_directory/tickloop" >&2
exec "$bin_directory/tickloop" "$@"
