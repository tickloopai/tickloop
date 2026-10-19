# Tickloop

Install the official Tickloop terminal agent on macOS:

```sh
cargo install --locked tickloop
tickloop
```

This crate contains the installer and launcher only; the application's source
code lives at <https://github.com/tickloopai/tickloop>. During `cargo install`, the build script
downloads the matching official macOS release from `tickloopai/homebrew-tap` on
GitHub, verifies SHA-256 and the archive contents, and embeds the complete
`tickloop`, `tickhost`, and `tick-reader` bundle in the installed launchers.
If downloading or validation fails, Cargo installation fails.

Apple Silicon and Intel Macs are supported. Installation needs Rust 1.89 or newer,
network access and the macOS system `curl`, `shasum`, and `tar` commands. The first
launch unpacks and activates the embedded bundle locally; it needs no additional
download, command, or environment variable. Subsequent automatic updates can
use the network as usual.

The Cargo installation includes `tickloop`, `tl`, `tickhost`, and `tick-reader`.
The verified application lives in `.tickloop` inside the Cargo installation root
(`~/.cargo/.tickloop` by default). Existing Homebrew and native installations are
not replaced; ensure your intended installation is first on PATH.

The application's normal background updater and `tickloop update` keep this
private runtime current without replacing Cargo's launcher or stopping running
Agents and Ticks. The crate version specifies the minimum initial application
version, not a pin that disables application updates. `cargo install tickloop`
also updates the launcher when a newer crate version is available.

Invalid bundles do not replace an existing runtime. Cached application versions are retained for
running processes. The launcher passes command arguments and the working
directory unchanged to the official program.

Licensed under the MIT License.
