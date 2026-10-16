---
name: usage
description: Open Longbridge Ticks with real quotes, K-line charts, the watchlist, positions, news, earnings and community posts, such as a request to see Tesla's stock price or open its K-line chart. Load it before opening tick-longbridge.
---

# Longbridge Ticks

`tick-longbridge` serves Longbridge data as protocol Ticks. Open it through the `tickloop` Skill's flow: `TickManage` with `action: "open"` and `command` from the table below. It opens as a protocol Tick by itself, so leave out `protocol`.

| Requested view | `command` |
| --- | --- |
| Stock price / 行情 / 报价, for one or more symbols | `["tick-longbridge", "quote", SYMBOL, …]` |
| K-line / K线 / candlestick chart | `["tick-longbridge", "kline", SYMBOL]` |
| Watchlist / 自选股 | `["tick-longbridge", "watchlist"]` |
| Positions / 持仓 | `["tick-longbridge", "positions"]` |
| News / 资讯 for one symbol; leave out `SYMBOL` for the whole market | `["tick-longbridge", "news", SYMBOL]` |
| Past earnings / 财报, US symbols only | `["tick-longbridge", "earnings", SYMBOL]` |
| A community member's posts / 社区动态 | `["tick-longbridge", "community", MEMBER_ID]` |

A request to see a price uses `quote`; a request for K线 or a chart uses `kline`. Add options only when the user asks for them:

- `kline`: `--period` is `day` (the default), `week`, `month`, `1m`, `5m`, `15m`, `30m` or `60m`; `--count N` (1–1000) candles; `--adjust forward` for forward-adjusted prices.
- `watchlist`: `--group GROUP`, a group ID or name; the first group otherwise.
- `news`: `--limit N` (1–100) items, 20 by default.
- `earnings`: `--count N` (1–20) released periods, newest first.

Never add `--once` or `--format`: they print a snapshot and exit, which a Tick refuses. `tick-longbridge --help` and `tick-longbridge <view> --help` list every option.

Use the broker symbol format: Tesla / 特斯拉 is `TSLA.US`, Apple / 苹果 is `AAPL.US`, and Tencent / 腾讯 is `700.HK`. Clarify an ambiguous company or market before opening. For example, “打开特斯拉K线” means `command: ["tick-longbridge", "kline", "TSLA.US"]`.

`MEMBER_ID` is the number in the member's profile link, such as 91 in `https://longbridge.com/zh-CN/profiles/91`; there is no nickname search, so ask for the link when the user names someone only by nickname. The community Tick's `reply` tool posts a public reply as the logged-in Longbridge user: call it only for a reply the user asked for, with the text they want posted.

Quotes need the logged-in account's quote permissions for the market; the watchlist and positions read that account. A Tick that Longbridge turned down because the account lacks a quote permission needs no new login: report the reason it shows, since logging in again does not add the permission. Every view needs a Longbridge login, which `longbridge`'s `login` in `workspace_host.tick_packages` reports. When it is not `logged_in`, or a Tick ends with an authorization diagnostic, report that and ask the user to log in to `longbridge` from `/ticks` in this Agent, where they choose China mainland or Global and confirm a code on their own device. Open the Tick again once they say they have logged in; no Agent restart is needed. Logging in is the user's step: never suggest `!` for it or a terminal command instead, never run the login yourself, and never read its credential files.

After opening an instance, use `TickDescribe` for its current tool catalog and revision, `TickRead` for its state, and `TickCall` for a declared operation. This Skill describes launch commands and prerequisites, not a second runtime capability catalog.
