#!/bin/sh
# Prepares the release of the named Tick packages to the Tick marketplace tickloopai/ticks, whose
# checkout is the first argument. In one run, so every SHA-256 matches its archive: builds the
# packages with package-ticks.sh, checks each archive, copies the archives to dist/release/,
# records their versions, downloads and SHA-256 in the checkout's marketplace.json and its README
# package list, and prints the commands that publish them. Publishing is public, so it is left to
# the user: this script creates no release and runs no git commit or push.
set -eu

target=aarch64-apple-darwin
marketplace_repository=tickloopai/ticks
release_downloads=https://github.com/$marketplace_repository/releases/download
test_support_crate=tickloop-tick-test-support
repository_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd -P)
dist_directory=$repository_root/dist/ticks
release_directory=$repository_root/dist/release
cd "$repository_root"

if [ "$#" -lt 2 ]; then
    echo "usage: $0 MARKETPLACE_CHECKOUT PACKAGE..." >&2
    exit 2
fi
checkout=$(CDPATH= cd "$1" && pwd -P)
shift
marketplace=$checkout/marketplace.json
readme=$checkout/README.md
if [ ! -f "$marketplace" ] || [ ! -f "$readme" ]; then
    echo "$checkout is not a checkout of $marketplace_repository: it has no marketplace.json or README.md" >&2
    exit 1
fi

fail() {
    echo "$*" >&2
    exit 1
}

manifest_field() {
    python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1" "$2"
}

# A version the marketplace lists has been released: its archive must never change.
for package in "$@"; do
    version=$(manifest_field "ticks/$package/tick-package.json" version)
    listed=$(python3 -I -c '
import json, sys
marketplace_path, name = sys.argv[1:]
with open(marketplace_path) as marketplace_file:
    marketplace = json.load(marketplace_file)
print(next((package["version"] for package in marketplace["packages"] if package["name"] == name), ""))
' "$marketplace" "$package")
    if [ "$listed" = "$version" ]; then
        fail "$package $version is already in $marketplace; raise its version (the workspace version, which ticks/$package/tick-package.json repeats) to release it again.
If $package $version was never published, restore those files (git -C $checkout restore marketplace.json README.md) and run this again."
    fi
done

for package in "$@"; do
    dependencies=$(cargo tree --locked --manifest-path "ticks/$package/Cargo.toml" \
        --edges normal --target "$target" --prefix none)
    if printf '%s\n' "$dependencies" | grep -q "^$test_support_crate "; then
        fail "the $package program depends on $test_support_crate, which holds mock data"
    fi
done

scripts/package-ticks.sh "$@"

scratch=$(mktemp -d "${TMPDIR:-/tmp}/publish-ticks.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
installer() {
    TICKLOOP_CONFIG_DIR=$scratch/config \
        cargo run --locked --quiet -p tickloop-agent --bin tickloop -- "$@"
}

mkdir -p "$release_directory"
for package in "$@"; do
    manifest=ticks/$package/tick-package.json
    version=$(manifest_field "$manifest" version)
    executable=$(manifest_field "$manifest" executable)
    archive_name=tick-$package-$version-$target.tar.gz
    archive=$dist_directory/$archive_name

    unexpected=$(tar -tzf "$archive" | grep -Ev '^(tick-package\.json|bin/.*|skills/.*)$' || true)
    [ -z "$unexpected" ] || fail "$archive holds more than tick-package.json, bin/ and skills/: $unexpected"
    unpacked=$scratch/$package
    mkdir "$unpacked"
    tar -xzf "$archive" -C "$unpacked"
    [ -d "$unpacked/skills" ] || fail "$archive has no skills/"
    program=$unpacked/$executable
    [ "$(file -b "$program")" = "Mach-O 64-bit executable arm64" ] ||
        fail "$executable in $archive is not an arm64 executable: $(file -b "$program")"
    codesign --verify "$program" || fail "$executable in $archive has no valid signature"
    codesign -dv "$program" 2>&1 | grep -qx 'Signature=adhoc' ||
        fail "$executable in $archive is not ad-hoc signed"
    [ "$("$program" --version)" = "$(basename "$executable") $version" ] ||
        fail "$executable in $archive does not report version $version"
    installer tick install --path "$archive"

    cp "$archive" "$release_directory/$archive_name"
done

commands=$(python3 -I - "$marketplace" "$readme" "$checkout" "$target" "$marketplace_repository" \
    "$release_downloads" "$release_directory" "$@" <<'EOF'
import hashlib, json, shlex, sys

(marketplace_path, readme_path, checkout, target, repository, release_downloads,
    release_directory, *names) = sys.argv[1:]

with open(marketplace_path) as marketplace_file:
    marketplace = json.load(marketplace_file)
packages = {package["name"]: package for package in marketplace["packages"]}
published = []
for name in names:
    with open(f"ticks/{name}/tick-package.json") as manifest_file:
        manifest = json.load(manifest_file)
    version = manifest["version"]
    archive_name = f"tick-{name}-{version}-{target}.tar.gz"
    archive = f"{release_directory}/{archive_name}"
    with open(archive, "rb") as archive_file:
        sha256 = hashlib.sha256(archive_file.read()).hexdigest()
    url = f"{release_downloads}/{name}-v{version}/{archive_name}"
    packages[name] = {
        "name": name,
        "version": version,
        "description": manifest["description"],
        "downloads": {target: {"url": url, "sha256": sha256}},
    }
    published.append((name, version, archive, url, sha256))
published.sort()
marketplace["packages"] = sorted(packages.values(), key=lambda package: package["name"])
with open(marketplace_path, "w") as marketplace_file:
    json.dump(marketplace, marketplace_file, indent=2, ensure_ascii=False)
    marketplace_file.write("\n")

rows = []
for package in marketplace["packages"]:
    description = package["description"].replace("|", "\\|")
    rows.append(f"| `{package['name']}` | {package['version']} | {description} |\n")
section = (
    "## Packages\n\n"
    "Install one with `tickloop tick install <package>`.\n\n"
    "| Package | Version | Description |\n| --- | --- | --- |\n" + "".join(rows)
)
with open(readme_path) as readme_file:
    readme = readme_file.read()
start = readme.find("\n## Packages\n")
layout = readme.find("\n## Layout\n")
if start >= 0:
    end = readme.find("\n## ", start + 1)
    readme = readme[: start + 1] + section + (readme[end:] if end >= 0 else "")
elif layout >= 0:
    readme = readme[: layout + 1] + section + "\n" + readme[layout + 1 :]
else:
    readme = readme.rstrip("\n") + "\n\n" + section
with open(readme_path, "w") as readme_file:
    readme_file.write(readme)

def command(*words):
    return " ".join(shlex.quote(word) for word in words)

released = ", ".join(f"{name} {version}" for name, version, *_ in published)
print(f"Prepared {marketplace_path} and {readme_path} for {released}.")
print("Nothing is published yet. Review the changes, then run these commands, or authorize them:\n")
print(command("git", "-C", checkout, "diff"))
for name, version, archive, url, sha256 in published:
    notes = (
        f"Tick package {name} {version} for {target}.\n\n"
        f"Install it with `tickloop tick install {name}`.\n\nSHA-256: {sha256}"
    )
    print(command(
        "gh", "release", "create", f"{name}-v{version}", archive,
        "--repo", repository, "--title", f"{name} {version}", "--notes", notes,
    ))
print(command("git", "-C", checkout, "add", "marketplace.json", "README.md"))
print(command("git", "-C", checkout, "commit", "-m", f"feat: release {released}"))
print(command("git", "-C", checkout, "push"))
print("\nThen check that each download is public and matches its SHA-256:\n")
for name, version, archive, url, sha256 in published:
    print(f"curl -fsSL {shlex.quote(url)} | shasum -a 256  # expect {sha256}")
EOF
)

echo
TICKLOOP_TICK_MARKETPLACE=$marketplace installer tick list --available
echo
printf '%s\n' "$commands"
