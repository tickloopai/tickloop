# Third-party notices

This file records the source and applicable license of third-party code adapted into Tickloop. It does not select a license for Tickloop as a whole.

## Former Herdr layout adaptation

- Upstream project: [Herdr](https://github.com/herdrdev/herdr).
- Reference revision: `120c682008a85dc17d7a13cc619a2fd5d25f3924`.
- Upstream source: [src/layout.rs at the reference revision](https://github.com/herdrdev/herdr/blob/120c682008a85dc17d7a13cc619a2fd5d25f3924/src/layout.rs).
- Former adapted source: `crates/tickloop/src/layout/tree.rs` and its regression tests, removed with the in-development workspace under [ADR 0012](./docs/adr/0012-remove-in-development-workspace-compatibility.md).
- Applicable upstream license: Apache License, Version 2.0, as declared in Herdr's package metadata and [LICENSE at the reference revision](https://github.com/herdrdev/herdr/blob/120c682008a85dc17d7a13cc619a2fd5d25f3924/LICENSE). An unmodified copy is included in [licenses/Herdr-Apache-2.0.txt](./licenses/Herdr-Apache-2.0.txt).

The former adaptation changed Herdr's Pane terminology to Tickloop's Panel terminology and adjusted its layout types. This attribution and the license copy remain for historical source versions. The referenced upstream layout file contains no separate copyright attribution, and the reference checkout has no top-level NOTICE file.

Current Tickloop connects to an independently running Herdr through its API; it does not build or distribute that former layout implementation. Herdr's repository is not a Tickloop build dependency. This notice covers the former adapted source, not unrelated components or dependencies in Herdr.

## Longbridge Terminal pure functions

- Upstream: [Longbridge Terminal](https://github.com/longbridge/longbridge-terminal).
- Reference revision: `b834538` (2026-09-07).
- Upstream sources: `src/utils/text.rs`, `src/utils/number.rs`, `src/openapi/rate_limiter.rs`, `src/tui/nav.rs`, `src/cli/output.rs`.
- Adapted source: `ui/src/format.rs` (width, padding, truncation, number abbreviation), `ticks/longbridge/src/rate_limit.rs`, `tick/provider/src/api.rs` (symbol normalization), and `tick/market/src/output.rs` (header keys).
- Applicable license: Apache License, Version 2.0. An unmodified copy is included in [licenses/Longbridge-Terminal-Apache-2.0.txt](./licenses/Longbridge-Terminal-Apache-2.0.txt).

The adaptation adds configurable ambiguous width, grapheme-aware truncation, and Chinese units, and removes global state from the limiter. Longbridge Terminal is a source reference, not a build dependency. Other dependencies retain their own licenses.

## Longbridge OpenAPI OAuth runtime refresh patch

- Upstream: [Longbridge OpenAPI](https://github.com/longbridge/openapi).
- Reference revision: `59294104c48a3812049158ee52124f32e5046008` (`5929410`).
- Upstream source: `rust/crates/oauth`.
- Vendored source: `vendor/longbridge-oauth`, patched through the root Cargo manifest.
- Changes: explicit AP/US refresh endpoints, credential-derived refresh routing, and the `x-dc-region` header; standalone Cargo dependency declarations and unit-test constructor updates. Details are in [the patch README](./vendor/longbridge-oauth/README.md).
- Applicable licenses: MIT OR Apache License, Version 2.0. Unmodified copies are included in [LICENSE-MIT](./vendor/longbridge-oauth/LICENSE-MIT) and [LICENSE-APACHE](./vendor/longbridge-oauth/LICENSE-APACHE).

## CLI Candlestick Chart glyph rules

- Upstream: [cli-candlestick-chart](https://github.com/Julien-R44/cli-candlestick-chart).
- Reference copy: Longbridge Terminal revision `b834538`, `crates/cli-candlestick-chart/src/chart_renderer.rs`.
- Adapted source: `ui/src/charts.rs`, `candle_glyph`; quarter-cell wick/body boundary rules extracted as a pure function. Braille crossing attribution in the same file also follows `src/ai/chart.rs` in the Apache-2.0 Longbridge Terminal reference above.
- Applicable license for candle glyph rules: MIT. The copyright notice and license are included in [licenses/Cli-Candlestick-Chart-MIT.txt](./licenses/Cli-Candlestick-Chart-MIT.txt).
