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

## Longbridge OpenAPI OAuth token refresher patch

- Upstream: [Longbridge OpenAPI](https://github.com/longbridge/openapi).
- Reference revision: `59294104c48a3812049158ee52124f32e5046008` (`5929410`).
- Upstream source: `rust/crates/oauth`.
- Vendored source: `vendor/longbridge-oauth`, patched through the root Cargo manifest.
- Changes: a `TokenRefresher` hook through which the application renews tokens in place of the client's own refresh; standalone Cargo dependency declarations and unit-test constructor updates. Details are in [the patch README](./vendor/longbridge-oauth/README.md).
- Applicable licenses: MIT OR Apache License, Version 2.0. Unmodified copies are included in [LICENSE-MIT](./vendor/longbridge-oauth/LICENSE-MIT) and [LICENSE-APACHE](./vendor/longbridge-oauth/LICENSE-APACHE).

## CLI Candlestick Chart glyph rules

- Upstream: [cli-candlestick-chart](https://github.com/Julien-R44/cli-candlestick-chart).
- Reference copy: Longbridge Terminal revision `b834538`, `crates/cli-candlestick-chart/src/chart_renderer.rs`.
- Adapted source: `ui/src/charts.rs`, `candle_glyph`; quarter-cell wick/body boundary rules extracted as a pure function. Braille crossing attribution in the same file also follows `src/ai/chart.rs` in the Apache-2.0 Longbridge Terminal reference above.
- Applicable license for candle glyph rules: MIT. The copyright notice and license are included in [licenses/Cli-Candlestick-Chart-MIT.txt](./licenses/Cli-Candlestick-Chart-MIT.txt).

## Slides markup parser

- Package: npm `parse5` version 8.0.1 ([parse5](https://github.com/inikulin/parse5)), a runtime dependency of `artifact/`. It decodes character references with `entities` 8.1.0 ([entities](https://github.com/fb55/entities)), which the existing `markdown-it` dependency already brought in.
- Use: `artifact/shared/slides/` parses Slides markup with it in the Artifacts Worker, its Durable Objects and the browser; the build bundles both packages unmodified.
- Applicable licenses: MIT for parse5 (Copyright (c) 2013-2019 Ivan Nikulin) and BSD-2-Clause for entities (Copyright (c) Felix Böhm). Unmodified copies are included in [licenses/parse5-MIT.txt](./licenses/parse5-MIT.txt) and [licenses/entities-BSD-2-Clause.txt](./licenses/entities-BSD-2-Clause.txt).

## Lucide icon geometry

- Upstream: [Lucide](https://github.com/lucide-icons/lucide), npm package `lucide-static` version 1.58.0 (a development dependency of `artifact/`).
- Embedded source: `artifact/shared/slides/icons.ts`, the path data of the 32 icons in the Slides icon set; `artifact/tests/slides-schema.test.ts` checks each entry against the package's `icon-nodes.json`.
- Applicable licenses: ISC License for Lucide; the MIT License (Copyright (c) 2013-present Cole Bemis) for the icons Lucide derives from Feather, which include several in this set. The package's license file, with both notices and the list of Feather-derived icons, is included unmodified in [licenses/Lucide-ISC.txt](./licenses/Lucide-ISC.txt).

The geometry is copied without changes and drawn by Tickloop's own renderer; no other Lucide code, font or sprite is included.

## Slides PowerPoint download libraries

- Packages: npm `pptxgenjs` version 4.0.1 ([PptxGenJS](https://github.com/gitbrent/PptxGenJS)) and `html-to-image` version 1.11.13 ([html-to-image](https://github.com/bubkoo/html-to-image)), runtime dependencies of `artifact/`. `pptxgenjs` brings `jszip` 3.10.2, whose browser build bundles its dependencies `pako` 1.0.11, `lie` 3.3.0, `immediate` 3.0.6 and `setimmediate` 1.0.5.
- Use: `artifact/web/slides-export.ts` loads both packages only when a reader downloads Slides as a PowerPoint file. The web build places them, unmodified, in a separate chunk that the browser fetches at that moment.
- Applicable licenses: MIT for PptxGenJS (Copyright (c) 2015-2022 Brent Ely) and html-to-image (Copyright (c) 2017-2025 W.Y.); JSZip is dual-licensed under MIT or GPLv3, used here under MIT (Copyright (c) 2009-2016 Stuart Knightley, David Duponchel, Franz Buchinger, António Afonso); pako is MIT (Copyright (C) 2014-2017 by Vitaly Puzrin and Andrei Tuputcyn) with zlib-derived parts under the Zlib license, which requires no notice in compiled distributions; lie is MIT (Copyright (c) 2014-2018 Calvin Metcalf, Jordan Harband); immediate is MIT (Copyright (c) 2012 Barnesandnoble.com, llc, Donavon West, Domenic Denicola, Brian Cavalier); setimmediate is MIT (Copyright (c) 2012 Barnesandnoble.com, llc, Donavon West, and Domenic Denicola). Copies are included in [licenses/PptxGenJS-MIT.txt](./licenses/PptxGenJS-MIT.txt), [licenses/html-to-image-MIT.txt](./licenses/html-to-image-MIT.txt), [licenses/JSZip-MIT.txt](./licenses/JSZip-MIT.txt) (the MIT part of JSZip's license file), [licenses/pako-MIT.txt](./licenses/pako-MIT.txt), [licenses/lie-MIT.txt](./licenses/lie-MIT.txt), [licenses/immediate-MIT.txt](./licenses/immediate-MIT.txt) and [licenses/setImmediate-MIT.txt](./licenses/setImmediate-MIT.txt). The TypeScript helpers inlined in PptxGenJS's build are 0BSD and need no notice.
