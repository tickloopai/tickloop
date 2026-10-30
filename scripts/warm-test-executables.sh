#!/bin/sh
# Every build links the workspace executables anew, and macOS checks a new executable on its
# first launch in time that grows with its size: about five seconds for the debug `tickloop`.
# Launching each once before the tests keeps that delay out of their startup deadlines.
target="${CARGO_TARGET_DIR:-target}"
for executable in "$target"/debug/tickloop "$target"/debug/tickhost \
  "$target"/release/tickloop "$target"/release/tickhost; do
  if [ -x "$executable" ]; then
    "$executable" --version >/dev/null
  fi
done
exit 0
