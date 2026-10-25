# Tickloop Harness

`tickloop-harness` is the shared Agent engine API for hosts. It only re-exports implementation crates. Its module paths stay stable as those crates move.

```toml
[dependencies]
tickloop-harness = { git = "https://github.com/tickloopai/tickloop", default-features = false, features = ["engine"] }
```

| Feature | Modules |
| --- | --- |
| Always available | `protocol`, `base`, `ids` |
| `engine` | `runtime`, `tools`, and the policy, model, transcript, cron and artifacts features |
| `policy` | `policy` (Auto rules, built-in Skills and the Settings Store seam) |
| `model` | `model` (catalog, clients and provider authentication) |
| `transcript` | `transcript` (records and Display Projection) |
| `cron` | `cron` |
| `artifacts` | `artifacts` |
| `test-support` | `test_support` (including the Stateless Host reference implementation), plus `engine` |

No feature is enabled by default. A Gateway can enable just `cron` and `transcript`; a Model Proxy can enable just `model`. Use `test-support` in dev-dependencies only. The `policy` module exposes the Settings Store seam and its in-memory implementation; it does not expose the local settings file.

The `engine` implementation still includes local Tickhost dependencies until the Execution Environment extraction. Settings files and disk Skill discovery are supplied by Code through Settings Store.

The Tick public crates and `tickloop-ui` are the API for Tick authors; they do not require this facade. Harness implementation crates are internal.
