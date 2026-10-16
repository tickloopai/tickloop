#!/bin/sh
# Builds each named Tick package under ticks/ for Apple Silicon, assembles its package directory
# in dist/ticks/<package>/ and its release archive beside it, and prints the archive's SHA-256 and
# the matching marketplace.json entry. It publishes nothing. With TICK_PACKAGE_QUIET set it only
# names each program it builds, for make install-ticks.
set -eu

target=aarch64-apple-darwin
release_downloads=https://github.com/tickloopai/ticks/releases/download
repository_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd -P)
target_directory=${CARGO_TARGET_DIR:-$repository_root/target}
dist_directory=$repository_root/dist/ticks
cd "$repository_root"

if [ "$#" -eq 0 ]; then
    echo "usage: $0 PACKAGE..." >&2
    exit 2
fi

manifest_field() {
    python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1" "$2"
}

mkdir -p "$dist_directory"
for package in "$@"; do
    source_directory=ticks/$package
    manifest=$source_directory/tick-package.json
    name=$(manifest_field "$manifest" name)
    version=$(manifest_field "$manifest" version)
    executable=$(manifest_field "$manifest" executable)
    if [ "$name" != "$package" ]; then
        echo "$manifest names the package $name, not $package" >&2
        exit 1
    fi
    program=$(basename "$executable")
    if [ -n "${TICK_PACKAGE_QUIET:-}" ]; then
        echo "Building ${program}…"
        cargo_output=--quiet
    else
        cargo_output=
    fi
    cargo build --locked --release $cargo_output --target "$target" \
        --manifest-path "$source_directory/Cargo.toml" --bin "$program"

    package_directory=$dist_directory/$package
    rm -rf "$package_directory"
    mkdir -p "$package_directory/$(dirname "$executable")"
    cp "$manifest" "$package_directory/tick-package.json"
    cp "$target_directory/$target/release/$program" "$package_directory/$executable"
    cp -R "$source_directory/skills" "$package_directory/skills"

    archive_name=tick-$package-$version-$target.tar.gz
    archive=$dist_directory/$archive_name
    # No AppleDouble files, extended attributes or owner names from this machine.
    COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs --no-acls \
        --uid 0 --gid 0 --numeric-owner \
        -czf "$archive" -C "$package_directory" tick-package.json "${executable%%/*}" skills
    if [ -n "${TICK_PACKAGE_QUIET:-}" ]; then
        continue
    fi
    sha256=$(shasum -a 256 "$archive" | cut -d ' ' -f 1)
    printf '\nPackage directory: %s\nArchive: %s\nSHA-256: %s\nmarketplace.json entry:\n' \
        "$package_directory" "$archive" "$sha256"
    python3 -I -c '
import json, sys
manifest_path, target, url, sha256 = sys.argv[1:]
with open(manifest_path) as manifest_file:
    manifest = json.load(manifest_file)
entry = {
    "name": manifest["name"],
    "version": manifest["version"],
    "description": manifest["description"],
    "downloads": {target: {"url": url, "sha256": sha256}},
}
print(json.dumps(entry, indent=2, ensure_ascii=False))
' "$manifest" "$target" "$release_downloads/$package-v$version/$archive_name" "$sha256"
done
