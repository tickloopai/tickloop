#!/bin/sh
# Builds, publishes and verifies the one Tick package selected by RELEASE_TAG=tick-<package>-vX.Y.Z.
# Version commits and source tags are created by scripts/release.mjs ticks.
set -eu

target=aarch64-apple-darwin
source_repository=griffinqiu/tickloop
marketplace_repository=tickloopai/ticks
release_downloads=https://github.com/$marketplace_repository/releases/download
marketplace_url=https://raw.githubusercontent.com/$marketplace_repository/HEAD/marketplace.json
test_support_crate=tickloop-tick-test-support
download_attempts=6
download_retry_seconds=5
repository_root=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
dist_directory=$repository_root/dist/ticks
release_directory=$repository_root/dist/release
cd "$repository_root"

fail() {
    echo "$*" >&2
    exit 1
}

usage() {
    echo "usage: RELEASE_TAG=tick-<package>-vX.Y.Z $0 build | publish MARKETPLACE_CHECKOUT ARCHIVE_DIRECTORY | verify ARCHIVE_DIRECTORY" >&2
    exit 2
}

manifest_field() {
    python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1" "$2"
}

release_package() {
    python3 -I - "${RELEASE_TAG:-}" <<'PYTAG'
import json, re, sys
from pathlib import Path
match = re.fullmatch(r"tick-([a-z][a-z0-9-]*)-v((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))", sys.argv[1])
if not match:
    sys.exit("RELEASE_TAG must be tick-<package>-vX.Y.Z")
name, version = match.groups()
path = Path("ticks") / name / "tick-package.json"
if not path.is_file():
    sys.exit(f"Unknown Tick package {name}")
manifest = json.loads(path.read_text())
if manifest["name"] != name or manifest["version"] != version:
    sys.exit(f"{path} does not match release tag {sys.argv[1]}")
print(name)
PYTAG
}

packages() {
    release_package
}

release_version() {
    package=$(release_package)
    manifest_field "ticks/$package/tick-package.json" version
}

archive_name() {
    echo "tick-$1-$2-$target.tar.gz"
}

sha256_of() {
    shasum -a 256 "$1" | cut -d ' ' -f 1
}

summarize() {
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        printf '%s\n' "$*" >>"$GITHUB_STEP_SUMMARY"
    fi
}

build_release() {
    version=$(release_version)
    names=$(packages)
    for package in $names; do
        dependencies=$(cargo tree --locked --manifest-path "ticks/$package/Cargo.toml" \
            --edges normal --target "$target" --prefix none)
        if printf '%s\n' "$dependencies" | grep -q "^$test_support_crate "; then
            fail "the $package program depends on $test_support_crate, which holds mock data"
        fi
    done

    # shellcheck disable=SC2086 # one word per package name
    TICK_PACKAGE_QUIET=1 scripts/package-ticks.sh $names

    scratch=$(mktemp -d "${TMPDIR:-/tmp}/release-ticks.XXXXXX")
    trap 'rm -rf "$scratch"' EXIT
    rm -rf "$release_directory"
    mkdir -p "$release_directory"
    for package in $names; do
        executable=$(manifest_field "ticks/$package/tick-package.json" executable)
        name=$(archive_name "$package" "$version")
        archive=$dist_directory/$name

        unexpected=$(tar -tzf "$archive" |
            grep -Ev '^(tick-package\.json|THIRD_PARTY_NOTICES\.md|bin/.*|skills/.*|licenses/.*|vendor/([^/]+/)?|vendor/[^/]+/(LICENSE[^/]*|README\.md))$' ||
            true)
        [ -z "$unexpected" ] ||
            fail "$archive holds more than the package, its license notices and licenses: $unexpected"
        unpacked=$scratch/$package
        mkdir "$unpacked"
        tar -xzf "$archive" -C "$unpacked"
        [ -d "$unpacked/skills" ] || fail "$archive has no skills/"
        [ -f "$unpacked/THIRD_PARTY_NOTICES.md" ] && [ -d "$unpacked/licenses" ] ||
            fail "$archive has no THIRD_PARTY_NOTICES.md and licenses/"
        program=$unpacked/$executable
        [ "$(file -b "$program")" = "Mach-O 64-bit executable arm64" ] ||
            fail "$executable in $archive is not an arm64 executable: $(file -b "$program")"
        codesign --verify "$program" || fail "$executable in $archive has no valid signature"
        codesign -dv "$program" 2>&1 | grep -qx 'Signature=adhoc' ||
            fail "$executable in $archive is not ad-hoc signed"
        [ "$("$program" --version)" = "$(basename "$executable") $version" ] ||
            fail "$executable in $archive does not report version $version"
        TICKLOOP_CONFIG_DIR=$scratch/config \
            cargo run --locked --quiet -p tickloop-agent --bin tickloop -- \
            tick install --path "$archive" >/dev/null

        cp "$archive" "$release_directory/$name"
        echo "Built and checked $package $version ($name, SHA-256 $(sha256_of "$archive"))."
    done
}

# After a failure that left releases in the marketplace repository: what exists, and the exact
# commands that finish or withdraw the release. Nothing is ever deleted here.
report_published() {
    stage=$1
    releases=$2
    [ -n "$releases" ] || return 0
    {
        echo
        echo "Releases in $marketplace_repository:$releases. Nothing was deleted."
        echo "To finish, fix the cause and re-run the failed job (Re-run failed jobs in GitHub Actions); it"
        echo "keeps these releases. Locally, the same stage is:"
        echo "  RELEASE_TAG=$tag RELEASE_COMMIT=$commit GH_TOKEN=<token> $0 $stage"
        echo "To withdraw the release instead, revert its commit in $marketplace_repository if marketplace.json"
        echo "lists it, then run:"
        for release in $releases; do
            echo "  gh release delete $release --repo $marketplace_repository --cleanup-tag --yes"
        done
    } >&2
}

publish_release() {
    [ "$#" -eq 2 ] || usage
    checkout=$(CDPATH='' cd "$1" && pwd -P)
    archives=$(CDPATH='' cd "$2" && pwd -P)
    marketplace=$checkout/marketplace.json
    readme=$checkout/README.md
    [ -f "$marketplace" ] && [ -f "$readme" ] ||
        fail "$checkout is not a checkout of $marketplace_repository: it has no marketplace.json or README.md"
    version=$(release_version)
    tag=$RELEASE_TAG
    commit=${RELEASE_COMMIT:-$(git rev-parse HEAD)}
    source=${GITHUB_REPOSITORY:-$source_repository}
    names=$(packages)
    for package in $names; do
        [ -f "$archives/$(archive_name "$package" "$version")" ] ||
            fail "$archives has no archive of $package $version"
    done
    scratch=$(mktemp -d "${TMPDIR:-/tmp}/release-ticks.XXXXXX")
    published=""
    trap 'status=$?; rm -rf "$scratch"; if [ "$status" -ne 0 ]; then report_published "publish $checkout $archives" "$published"; fi' EXIT

    for package in $names; do
        name=$(archive_name "$package" "$version")
        archive=$archives/$name
        sha256=$(sha256_of "$archive")
        release=$package-v$version
        if view=$(gh release view "$release" --repo "$marketplace_repository" 2>&1); then
            gh release download "$release" --repo "$marketplace_repository" --pattern "$name" \
                --dir "$scratch/$release" ||
                fail "Could not download $name from the existing release $release."
            [ "$(sha256_of "$scratch/$release/$name")" = "$sha256" ] ||
                fail "The release $release already exists with another $name; raise this package version to release again."
            published="$published $release"
            echo "The release $release already carries this archive."
        elif printf '%s\n' "$view" | grep -qi 'not found'; then
            gh release create "$release" "$archive" --repo "$marketplace_repository" \
                --title "$package $version" --notes "Tick package $package $version for $target.

Install it with \`tickloop tick install $package\`.

Built from $source@$commit ($tag).
SHA-256: $sha256" >/dev/null
            published="$published $release"
            echo "Created the release $release."
        else
            fail "Could not check the release $release: $(printf '%s\n' "$view" | head -n 1)"
        fi
        summarize "- $package $version: https://github.com/$marketplace_repository/releases/tag/$release (SHA-256 $sha256)"
    done

    # shellcheck disable=SC2086 # one word per package name
    python3 -I - "$marketplace" "$readme" "$archives" "$target" "$release_downloads" "$version" \
        $names <<'EOF'
import hashlib, json, sys

marketplace_path, readme_path, archives, target, release_downloads, version, *names = sys.argv[1:]

with open(marketplace_path) as marketplace_file:
    marketplace = json.load(marketplace_file)
packages = {package["name"]: package for package in marketplace["packages"]}
for name in names:
    with open(f"ticks/{name}/tick-package.json") as manifest_file:
        description = json.load(manifest_file)["description"]
    archive_name = f"tick-{name}-{version}-{target}.tar.gz"
    with open(f"{archives}/{archive_name}", "rb") as archive_file:
        sha256 = hashlib.sha256(archive_file.read()).hexdigest()
    listed = packages.get(name)
    if listed and tuple(map(int, listed["version"].split("."))) > tuple(map(int, version.split("."))):
        sys.exit(f"marketplace.json already lists newer {name} {listed['version']}; refusing to downgrade to {version}")
    if listed and listed["version"] == version:
        listed_sha256 = listed["downloads"].get(target, {}).get("sha256")
        if listed_sha256 != sha256:
            sys.exit(f"marketplace.json lists {name} {version} with another archive ({listed_sha256})")
    packages[name] = {
        "name": name,
        "version": version,
        "description": description,
        "downloads": {
            target: {
                "url": f"{release_downloads}/{name}-v{version}/{archive_name}",
                "sha256": sha256,
            }
        },
    }
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
readme = readme.replace(
    "a data source's login stays with the package's own command, such as `tick-longbridge auth`.",
    "you log in to a package's data source with `/ticks` in the Agent, or with "
    "`tickloop tick login <package>` in a terminal.",
)
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
EOF
    released=$(for package in $names; do printf '%s %s, ' "$package" "$version"; done)
    released=${released%, }
    if git -C "$checkout" diff --quiet -- marketplace.json README.md; then
        echo "The marketplace already lists $released."
    else
        branch=$(git -C "$checkout" symbolic-ref --short HEAD) ||
            fail "$checkout is not on a branch"
        if [ -z "$(git -C "$checkout" config user.email || true)" ]; then
            export GIT_AUTHOR_NAME='github-actions[bot]'
            export GIT_AUTHOR_EMAIL='41898282+github-actions[bot]@users.noreply.github.com'
            export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME" GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"
        fi
        git -C "$checkout" add marketplace.json README.md
        git -C "$checkout" commit --quiet \
            -m "feat: release $released" -m "Built from $source@$commit ($tag)."
        git -C "$checkout" push --quiet origin "HEAD:refs/heads/$branch"
        echo "Pushed the marketplace update ($(git -C "$checkout" rev-parse --short HEAD))."
    fi
    summarize "- marketplace.json lists $released."
}

verify_release() {
    [ "$#" -eq 1 ] || usage
    archives=$(CDPATH='' cd "$1" && pwd -P)
    version=$(release_version)
    tag=$RELEASE_TAG
    commit=${RELEASE_COMMIT:-$(git rev-parse HEAD)}
    published=$(for package in $(packages); do printf ' %s' "$package-v$version"; done)
    scratch=$(mktemp -d "${TMPDIR:-/tmp}/release-ticks.XXXXXX")
    trap 'status=$?; rm -rf "$scratch"; if [ "$status" -ne 0 ]; then report_published "verify $archives" "$published"; fi' EXIT
    for package in $(packages); do
        name=$(archive_name "$package" "$version")
        expected=$(sha256_of "$archives/$name")
        url=$release_downloads/$package-v$version/$name
        attempt=1
        # Without credentials, as anyone installing the package downloads it.
        until curl -fsSL -o "$scratch/$name" "$url"; do
            [ "$attempt" -lt "$download_attempts" ] || fail "Could not download $url."
            attempt=$((attempt + 1))
            sleep "$download_retry_seconds"
        done
        downloaded=$(sha256_of "$scratch/$name")
        [ "$downloaded" = "$expected" ] || fail "$url has SHA-256 $downloaded, not $expected."
        echo "Verified $url."
    done
    summarize "- Every download matches its SHA-256."
}

[ "$#" -ge 1 ] || usage
command=$1
shift
case $command in
build)
    [ "$#" -eq 0 ] || usage
    build_release
    ;;
publish) publish_release "$@" ;;
verify) verify_release "$@" ;;
*) usage ;;
esac
