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

cargo build --locked -p tickloop-agent --bin tickloop -p tickloop-tickhost --bin tickhost \
    -p tickloop-tick-reader --bin tick-reader
mkdir "$bin_directory"
for program in tickloop tickhost tick-reader; do
    cp "$target_directory/debug/$program" "$bin_directory/$program"
    chmod 0500 "$bin_directory/$program"
done
chmod 0500 "$bin_directory" "$run_directory"

# Detached daemons and workers can still exec this bundle after the View exits.
trap - 0 1 2 15
cd "$launch_directory"
printf 'Development executable: %s\n' "$bin_directory/tickloop" >&2
exec "$bin_directory/tickloop" "$@"
