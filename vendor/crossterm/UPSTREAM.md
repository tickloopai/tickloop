# Crossterm input patch

This directory contains the published MIT-licensed crossterm 0.29.0 source.
The crate archive SHA-256 is
`d8b9f2e4c67f833b660cdb0a3523065869fb35570177239812ed4c905aeff87b`,
matching the original Cargo.lock checksum. `UPSTREAM.json` records the original
hashes of the 69 copied source, documentation, example, and manifest files.
Each copied file was compared byte for byte with that archive before patching.
The upstream `LICENSE` is retained unchanged.

The workspace uses this copy through a relative `[patch.crates-io]` entry.
It is excluded from workspace membership so upstream examples and development
dependencies do not become Tick Loop's workspace targets.

## Reason for the patch

Crossterm 0.29.0 consumes both bytes of `ESC ESC` as one Escape event. If the
second Escape starts a terminal sequence, the remainder becomes ordinary text.
For example, `ESC ESC[5~ ESC[5~` produces Escape, `[5~`, and only one PageUp.
The latest published release and upstream master were checked on 2026-09-11;
neither contained a fix, and the parser is not a public extension interface.

- [Published release](https://docs.rs/crate/crossterm/0.29.0)
- [Upstream parser at the inspected master revision](https://github.com/crossterm-rs/crossterm/blob/cdc30a9cd7f89d97a847c182feab66147eb50223/src/event/sys/unix/parse.rs)
- [Upstream default Unix event source](https://github.com/crossterm-rs/crossterm/blob/cdc30a9cd7f89d97a847c182feab66147eb50223/src/event/source/unix/mio.rs)

## Local changes

- Both Unix event sources use `event/source/unix/parser.rs` instead of their
  duplicated buffered parsers. A second Escape emits the first event and remains
  a new pending prefix. Existing CSI, SS3, UTF-8, paste, mouse, focus, terminal
  replies, and keyboard enhancement parsing still use the upstream parser.
- With an active event consumer, a lone Escape waits up to 50 ms for a possible
  continuation. Both backends combine this deadline with the caller's timeout;
  another input byte is not required. A read boundary does not end a prefix.
- An Escape queued behind already decoded keys defers that timeout while the
  consumer drains those keys. Each subsequent call may read one continuation
  byte before delivering another queued key. If the consumer was paused, the
  arrival time of a continuation cannot be recovered; readable continuation
  bytes take precedence over expiration in this deferred case. This prevents
  rendering delays from splitting a CSI prefix into literal input. It does not
  claim to distinguish late Alt input from an earlier fragmented sequence.
- `events` enables the existing Unix `filedescriptor` dependency. A zero-timeout
  readiness check (select on macOS) protects every read of a blocking descriptor.
  Successful reads are bounded to one buffer, with at most one deferred-prefix
  byte per call. No descriptor flags are changed, including flags shared by
  stdin and stdout. Mio also checks level readiness for unread buffered input.
- Both backends poll control descriptors before handing out queued keys, so a
  continuous Escape stream cannot starve wake or resize events. EOF delivers
  already decoded keys and a final pending Escape before reporting the end.
- `backend_tests.rs` exercises blocking and nonblocking descriptors, caller
  deadlines, read boundaries, delayed consumers, bounded read-ahead, EOF, and
  event-stream wake. Parser tests control fragmentation and time explicitly.
- One redundant pair of parentheses in `terminal/sys/unix.rs` was removed to
  avoid an existing compiler warning for this local dependency.

Legacy encodings cannot distinguish every Escape/Alt intention. Repeated
Escape bytes represent separate Escape presses, with the last byte still able
to prefix the next sequence. Alt characters retain their existing meaning
inside the 50 ms window. Standard CSI modifier encodings and CSI-u retain Alt
navigation; an additional legacy Escape prefix cannot simultaneously represent
both Alt-navigation and a preceding standalone Escape.

## Validation and replacement

Run focused upstream-library tests for the default backend and `use-dev-tty`:

```sh
cargo test --manifest-path vendor/crossterm/Cargo.toml --lib event::source::unix
cargo test --manifest-path vendor/crossterm/Cargo.toml --lib --features use-dev-tty event::source::unix
```

Tick Loop's end-to-end regression is
`.scratch/agent/validation/escape-input-ui.py`. Keep the single-write regression,
fragment tests, lone-Escape deadline, and ordinary modifier/paste controls when
evaluating a future upstream release. Remove the workspace patch only after
those same cases pass against the replacement release.

The changed upstream files are the two Cargo manifests,
`src/event/source/unix.rs`, its `mio.rs` and `tty.rs` backends, and
`src/terminal/sys/unix.rs`. The shared parser, backend tests, provenance files,
and local ignore file are additions. Original hashes in `UPSTREAM.json` remain
unchanged so this list can be audited against the published archive.
