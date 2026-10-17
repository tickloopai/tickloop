#!/bin/sh
# Install the verified native release without requiring Rust or Homebrew.
set -eu

case "$(uname -s):$(uname -m)" in
  Darwin:arm64) target=aarch64-apple-darwin ;;
  Darwin:x86_64) target=x86_64-apple-darwin ;;
  *) echo 'Tickloop native releases currently support macOS ARM and Intel.' >&2; exit 1 ;;
esac

for tool in curl shasum tar mktemp; do
  command -v "$tool" >/dev/null 2>&1 || { echo "Missing required command: $tool" >&2; exit 1; }
done
: "${HOME:?HOME is not set}"
if existing=$(command -v tickloop 2>/dev/null); then
  if [ "$existing" != "$HOME/.local/bin/tickloop" ]; then
    echo "Tickloop is already available at $existing. Use 'tickloop update' for that installation, or remove it before installing the native release." >&2
    exit 1
  fi
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/tickloop-install.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
base=https://github.com/tickloopai/homebrew-tap/releases
release=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL \
  --connect-timeout 15 --max-time 60 -o /dev/null -w '%{url_effective}' "$base/latest")
tag=${release##*/}
if [ "$release" != "$base/tag/$tag" ] || ! printf '%s\n' "$tag" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'; then
  echo 'Cannot resolve the latest stable Tickloop release.' >&2; exit 1
fi
archive=tickloop-$target.tar.gz
for asset in "$archive" "$archive.sha256"; do
  curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL \
    --connect-timeout 15 --max-time 600 --max-filesize 268435456 \
    "$base/download/$tag/$asset" -o "$scratch/$asset"
done
cd "$scratch"
expected=$(awk -v archive="$archive" 'NF == 2 && $2 == archive && length($1) == 64 && $1 !~ /[^0-9a-fA-F]/ { print tolower($1) }' "$archive.sha256")
actual=$(shasum -a 256 "$archive" | awk '{print $1}')
[ "$actual" = "$expected" ] || { echo 'Tickloop archive SHA-256 mismatch.' >&2; exit 1; }
# Reject links and unexpected paths before tar is allowed to write anything.
tar -tzf "$archive" > entries
for program in tickloop tickhost tick-reader; do
  [ "$(grep -Fxc "$program" entries)" = 1 ] || { echo "Missing or duplicate $program in archive." >&2; exit 1; }
done
[ "$(wc -l < entries | tr -d ' ')" = 3 ] || { echo 'Unexpected archive paths.' >&2; exit 1; }
tar -tvzf "$archive" > modes
[ "$(grep -c '^-' modes)" = 3 ] || { echo 'Archive must contain only regular files.' >&2; exit 1; }
tar -xzf "$archive"
[ "$(./tickloop --version)" = "tickloop ${tag#v}" ] || { echo 'Archive reports an unexpected version.' >&2; exit 1; }
./tickloop update --install
