# Longbridge OAuth local patch

This directory contains `rust/crates/oauth` from
[`longbridge/openapi`](https://github.com/longbridge/openapi/tree/59294104c48a3812049158ee52124f32e5046008/rust/crates/oauth),
revision `59294104c48a3812049158ee52124f32e5046008` (`5929410`, version 4.5.0).
The root Cargo manifest patches only `longbridge-oauth`; the main SDK and
`longbridge-geo` remain pinned to that revision.

Tickloop keeps the Longbridge login in the standard token file the Agent saves for the
package (`<config root>/credentials/longbridge/oauth2.json`), shared by every
`tick-longbridge` process. Renewing it must hold that file's lock and write the
rotated token back, so the client's own refresh, which renews only its in-memory
copy, cannot be used.

Local changes:

- `TokenRefresher` (`src/refresher.rs`) renews a token in place of the client's
  own refresh; `OAuthBuilder::token_refresher` sets one.
- `OAuth::refresh_token` asks the refresher when one is set, both while building
  and when `access_token()` finds the token in its five-minute refresh window;
  without one it is upstream's code. What the refresher returns is saved through
  the configured `TokenStorage` as before.
- `OAuthInner` carries the optional refresher; the unit-test constructors set it
  to `None`.
- `Cargo.toml` spells out the inherited upstream dependency versions and pins
  `longbridge-geo` to the same SDK commit so this subcrate builds independently.

The in-memory token lock, browser callback and storage behaviour are otherwise upstream's.
`tick-longbridge` builds the client from a token it has just checked, so building
never refreshes or starts a browser callback, and keeps its guard against one.

Regression coverage lives at the application seam in
`ticks/longbridge/tests/all/auth.rs`: an already constructed OAuth object crosses its
refresh window, calls `access_token()`, and renews through the shared token file at
a local token endpoint, including rotation, a refresh another process made, and a
refused refresh.

The vendored crate is excluded from the root workspace because upstream unit
tests include default-home token file operations. The application's isolated
authorization tests verify this patch. Real Longbridge login and live token
renewal require separate validation.

Upstream licenses are reproduced without modification in `LICENSE-MIT` and
`LICENSE-APACHE`. Remove this patch when the pinned upstream SDK lets an
application renew tokens itself, after rerunning those regressions.
