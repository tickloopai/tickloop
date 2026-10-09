# Longbridge OAuth local patch

This directory contains `rust/crates/oauth` from
[`longbridge/openapi`](https://github.com/longbridge/openapi/tree/59294104c48a3812049158ee52124f32e5046008/rust/crates/oauth),
revision `59294104c48a3812049158ee52124f32e5046008` (`5929410`, version 4.5.0).
The root Cargo manifest patches only `longbridge-oauth`; the main SDK and
`longbridge-geo` remain pinned to that revision.

The upstream runtime refresh path chooses its gateway from geolocation and omits
`x-dc-region`. A device token for the US data center can therefore fail to refresh
while a Tick Command is running, even if startup authorization succeeded.

Local changes:

- `OAuthBuilder::refresh_token_urls` accepts explicit AP and US token URLs.
- Every refresh selects its URL from the refresh credential's `DcRegion` and
  sends that data center in `x-dc-region`. Without explicit URLs, US uses the
  global gateway; AP retains the upstream geographic default.
- `OAuthInner` carries endpoint configuration; existing unit-test constructors
  initialize that optional field.
- `Cargo.toml` spells out the inherited upstream dependency versions and pins
  `longbridge-geo` to the same SDK commit so this subcrate builds independently.

The existing token lock, refresh-token preservation, storage writes, and runtime
refresh error propagation remain upstream behavior. Runtime refresh does not
start a browser callback. Tick Loop retains its startup authorization guard.

Regression coverage lives at the application HTTP seam in
`tick/cli/tests/all/auth.rs`: an already constructed OAuth object crosses its
refresh window, calls `access_token()`, reaches the appropriate local AP/US stub,
and persists renewed credentials. It also covers omitted refresh tokens and
revocation without including credentials in the displayed error.

The vendored crate is excluded from the root workspace because upstream unit
tests include default-home token file operations. The application's isolated
authorization tests verify this patch. Real Longbridge login and live token
renewal require separate validation.

Upstream licenses are reproduced without modification in `LICENSE-MIT` and
`LICENSE-APACHE`. Remove this patch when the pinned upstream SDK provides the
required routing and endpoint configuration, after rerunning those regressions.
