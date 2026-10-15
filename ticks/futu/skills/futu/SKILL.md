---
name: futu
description: Open the Futu options Tick with option pricing, implied volatility and saved snapshots for one US or HK underlying from a running Futu OpenD, such as 看看苹果期权. Load it before opening tick-futu.
---

# Futu Options Tick

`tick-futu` serves option pricing from Futu OpenD as a protocol Tick. Open it through the `tickloop` Skill's flow: `ModifyTick` with `action: "open"` and `command: ["tick-futu", "options", SYMBOL]`. It opens as a protocol Tick by itself, so leave out `protocol`.

`SYMBOL` is a US or HK underlying, such as `AAPL.US` or `00700.HK`. Add options only when the user asks for them:

- `--expiry YYYY-MM-DD` selects an expiration.
- `--strikes N` (1–100, 21 by default) limits how many strikes nearest the money are fetched.
- `--snapshot-dir PATH` keeps saved snapshots in another directory.

Never add `--once` or `--format`: they print a snapshot and exit, which a Tick refuses. `tick-futu options --help` lists every option.

The Tick reads a Futu OpenD the user runs and logs in to themselves, with a Futu account that has OpenAPI quote permission for the market; it uses no Longbridge login. OpenD listens on `127.0.0.1:11111` by default: pass `--host HOST` and `--port PORT` only when the user names another address, such as `["tick-futu", "options", "AAPL.US", "--port", "11112"]`. The connection is unencrypted, so only addresses on this computer are accepted. When OpenD is not running, not logged in or lacks the permission, the Tick ends with a diagnostic: report it and ask the user to start or log in to OpenD, then open the Tick again. Never start OpenD or handle its login yourself.
