# Tick Loop

Tick Loop 提供完整的终端 Agent 和 example Tick。使用原生 Herdr 的 Workspace → Tab → Pane 组织程序；Agent 可以读取 Tick、调用它的工具，并通过明确订阅让内容变化推动 Loop 继续执行。

主命令是 `tickloop`。Herdr 外仍可使用普通 Agent 功能；跨 Tick 发现、操作和订阅要求受支持的宿主，目前支持 Herdr。

| 成员 | 二进制 | 内容 |
| --- | --- | --- |
| `crates/agent` | `tickloop` | 完整 Agent Console、工具、权限、Transcript 和 Loop |
| `crates/ticks` | `tick` | 金融示例、离线 demo 和外部协议 launcher |
| `crates/tick-connect` | — | Herdr 接入、每实例 Session 与通用 Tick 协议 |
| `crates/ui` | — | 共享终端呈现规则 |

使用当前 Rust stable（edition 2024）。`make build` 构建 Agent 和示例；`make install` 安装 `tickloop` 和 `tick`；`make package` 打包这两个入口。

## 在 Herdr 中完成双向操作

Agent 会确认运行环境，并在 Herdr 接入可用时自动加载 Tick 操作指引。可以直接说“看看特斯拉股价”或“打开特斯拉 K 线”；Agent 优先用 Tick 展示，并读取实例核对结果。真实行情需要 Longbridge 授权。tmux 和 Zellij 当前仅识别并提示尚未接入；宿主外仍可普通对话，但不支持跨 Tick 操作。

要并排观察，可以说“在你当前 Tab 右边打开一个 K 线 Tick”；已有 Tick 则说“把刚才那个 Tick 移到你这个 Tab”。移动会保留实例及订阅。支持右侧或下方分屏，操作不抢焦点。

1. 在一个 Herdr Pane 运行 `tickloop`，按 [Agent 说明](crates/agent/README.md) 配置模型。
2. 在另一个 Pane 运行 `tick demo kline`。它会自动接入，使用模拟数据，无需金融账户。也可让 Agent 打开这条命令。
3. 在 Agent 中说：“读取当前 Workspace 的 Kline，并每 5 秒订阅它的变化。”Agent 先读目录，再为当前 Loop 建立订阅。
4. 在 Kline 按 `5` 切换到 5 分钟周期；订阅收到新内容后，所属 Loop 按 cadence 继续执行。
5. 在 Agent 中说：“在刚才那张图上添加一个标注，然后移除它。”Agent 读取 `mark` / `unmark` 的参数并经权限流程操作，结果显示在原来的 Tick 中。

普通日志或 CLI 可直接在 Pane 中运行。让 Agent 读取并明确订阅后，新终端文字也能推动 Loop；它不提供结构化工具目录和完整历史。外部协议程序使用 `tick run -- python3 examples/ticks/counter.py`，在 Counter Pane 输入 `+` 后回车可产生更新，也可让 Agent 调用 `increment`。

打开和读取不会自动订阅。默认范围是当前 Workspace；跨 Workspace 要明确指定。detach/attach 保留活进程，关闭 Pane 退出其中程序；重启后的实例要重新选取。操作、协议和边界说明见[原生 Herdr 接入](docs/design/native-herdr.md)。

# Tick Command `tick`

`tick` 是 Tick Loop 的 Tick Command，提供报价、自选、K 线、持仓、资讯和财报视图。它通过 Tick Instance 流输出 Tick Update，并在同一条流上声明和执行自己的 Tick Tool；既可以独立运行，也可以在 Herdr Pane 内自动接入 Agent。

## 安装与离线体验

```sh
cargo build --locked --release --workspace --bins
./target/release/tick --help
./target/release/tick demo quote --size 40x12
./target/release/tick demo kline --size 60x20
./target/release/tick demo news --size 40x12
```

安装入口使用 `make install`，把 Agent 与 `tick` 放入 PATH。交互模式要求 UTF-8 locale 和支持真彩色的终端；独立运行使用 TTY，Native 接入由 launcher 保留控制终端，并把 stdin/stdout 留给协议。macOS 为当前目标环境。`--help` 和 `describe` 无需 UTF-8 locale 或登录。默认 Unicode 图标不需要特殊字体；`--icon-theme nerd` 需要 Nerd Font v3。

所有 demo 均在真实 TUI 中运行，使用带 `Demo` 标识的模拟数据，不访问网络或读取账户。`--size` 以字符列×行限制显示区域，仍受实际终端尺寸约束。用 `q` 退出；可替换为 `quote`、`watchlist`、`kline`、`positions`、`news`。demo 支持下述全局显示参数。验收时分别缩小窗口、滚动、打开帮助并退出，检查终端恢复。

剩余人工验收可使用临时终端向导：

```sh
bash .scratch/longbridge-ticks/accept.sh visual
bash .scratch/longbridge-ticks/accept.sh live
```

`visual` 依次打开离线视图、配色/图标和有限窗口场景；`live` 包含设备码登录和真实数据视图。向导由用户在终端运行，只把通过/失败/跳过写到临时结果文件，不记录账户内容，也不自动将验收项勾选为通过。

## 授权

```sh
tick auth
tick auth status
tick auth logout
```

授权在 TUI 中完成：方向键或 `j/k` 选择中国大陆或全球接入区域，Enter 确认；随后在固定画面中显示验证网址、用户码和剩余有效时间，不会逐行刷倒计时。请在自己可操作的设备打开网址确认，终端会自动完成登录。远程 agent 无法代替用户完成网页步骤；在用户终端运行 `! tick auth`。

设备码确认期间，连接中断或授权端点返回 HTTP 5xx 会按原轮询间隔退避重试，保留当前端点并只提示一次连续故障；代理返回的 502 也按同一规则处理。重试以设备码原始有效期为限，明确拒绝或过期仍结束登录。

报价、自选、K 线、持仓和资讯的交互式命令共用这套授权界面。启动时未授权会先显示授权 TUI；运行中授权失效时先清理当前订阅，再进入授权 TUI。成功后继续或重启原来的 Tick Command。`--once` 和重定向输出属于非交互调用，不打开授权界面，仍提示运行 `tick auth` 并返回退出码 3。

令牌存于 `~/.tickloop/longbridge/auth.json`，明文 JSON、0600 权限、原子替换。客户端注册信息单独保存并复用。启动及持续运行期间自动续期，授权失效会提示重新运行 `tick auth`，不会打开浏览器回调流程。`auth status` 仅读取本地状态，不发起续期；`auth logout` 删除本地令牌。

## 配置

默认读取 `~/.tickloop/config.toml`，不存在时使用默认值。`--config PATH` 指定的文件必须存在；未知字段或无效值在进入 TUI 前报错。命令行参数仅覆盖本次运行。

```toml
color_mode = "red-up"       # red-up / green-up
icon_theme = "unicode"      # unicode / nerd
language = "zh-CN"          # zh-CN / en
ambiguous_width = "narrow"  # narrow / wide
# region = "global"        # cn / global，默认未配置，授权时询问
```

对应参数为 `--color-mode`、`--icon-theme`、`--language`、`--ambiguous-width`、`--region`。语言影响 API 名称及数字单位；不承诺将新闻原文翻译为英文。区域是接入网络选择，AP/US 数据中心由账户凭证确定。

`tick` 使用集中解析的终端渲染样式，容器外观由 Herdr 配置。

## 命令

| 命令 | 参数与行为 |
| --- | --- |
| `tick auth [status\|logout]` | 设备码登录、查看本地状态、退出登录 |
| `tick quote TSLA 700` | 一个或多个标的；纯字母补 `.US`，纯数字补 `.HK`；单标的错误显示在对应行 |
| `tick watchlist [--group ID或名称]` | 默认第一个自选分组；左右方向键或 `[` / `]` 切组 |
| `tick kline 700.HK [--period day] [--count 80] [--adjust none]` | 单标的 K 线；周期 `day/week/month/1m/5m/15m/30m/60m`；数量 1–1000；复权 `none/forward` |
| `tick positions` | 股票持仓、按币种汇总市值和浮盈亏；只读账户，不执行交易 |
| `tick news [700.HK] [--limit 20]` | 个股资讯；无代码时读长桥公开 RSS；数量 1–100 |
| `tick earnings TSLA.US [--count 10]` | 已发布财报的预期、实际、意外与财报前后走势；只支持美股；期数 1–20 |
| `tick options AAPL.US [--expiry YYYY-MM-DD]` | 富途 OpenD 的期权链、买卖价、IV、持仓量和近平值跨式参考；支持美股/港股标的 |
| `tick reader URL或文件 [更多URL或文件]` | 在终端阅读 HTML、Markdown、文本或 PDF 原文，搜索、章节定位和固定版本引用 |

八个业务命令默认进入 TUI，均支持 `--once` 和 `--once --format json`。JSON 输出没有横幅或 ANSI；普通行情表按业务行输出，财报、期权、事件行情和原文还保留各自的来源、时间和结构。缺失值为 `null` / `--`，管道表格不输出颜色。

```sh
tick quote TSLA 700 --once --format json
tick watchlist --group Technology --color-mode green-up
tick kline 700.HK --period 5m --adjust forward
tick positions --once
tick news --limit 10
tick earnings TSLA.US --count 12 --once --format json
tick kline AAPL.US --event-date 2026-07-30 --event-session after-hours --published-at 2026-07-30T16:05:00-04:00
tick options AAPL.US --host 127.0.0.1 --port 11111
tick reader ./earnings-release.pdf --symbol AAPL.US --fiscal-period FY2026Q3
tick demo options
tick demo reader
```

上面的财报事件时间只是参数示例，不是已核验的公司日程。事件行情在 Kline 内运行：`--event-date` 为交易所当地日期，`--published-at` 只填已确认的实际发布时间；未知时留空。`--chart-mode line/minute/daily` 切换事件分时、5 分钟 K、日 K，`--event-range auto/first-session/five-sessions` 切范围。窗口使用长桥交易日历与市场时区；没有精确发布时间就不画精确 T=0，未知发布时段也不生成伪造的发布前基准。

财报 `Enter` 打开单期详情，`c` 对比历史财期，`f` 查看公告来源。详情包含可取得的报表与业务分部，并分别标记接口缺失或权限失败。公告列表是公司资料入口，未核验的公告不能自动当作所选财期的原文；当前查询到的分析师预期也不等于发布前保存的历史快照。

Options 独立连接本机富途 OpenD，不使用长桥授权。OpenD 需登录 FUTU HK 账户并具备对应 API 行情权限；默认地址为 `127.0.0.1:11111`，明文连接仅接受本机地址。`--strikes 21` 限制获取的近平值执行价数量（1–100）。跨式参考使用同到期、同执行价、标准合约的 Call/Put 买卖中间价；它是到期定价的近似，不能解释成财报次日概率区间。按 `s` 显式保存当前快照，`h` 阅读已保存历史；可用 `--snapshot-dir PATH` 指定目录，没有事前快照就显示缺失。

Reader 可直接读取公司 IR / 公告链接或本地文件，不自动要求券商登录。PDF 需系统安装 Poppler 的 `pdftotext`；扫描件没有文本时明确提示需要 OCR。原文保留来源、获取时间、可选财期及内容版本；`--published-at` 是调用方提供的时间，显示为未经独立核验。旧引用带文档 ID、版本与章节，版本未载入时返回明确错误，不替换成另一份材料。`--once` 输出完整已提取原文，流式更新只携带目录元数据，Agent 可分页读取正文。

财报表默认十期：`consensus` 只保留最近八次已发布财报，更早的期由财经日历事件补齐。首次加载一个标的约需二十到三十秒，之后每天早上刷新一次。

K 线默认交互缓存 1000 根、一次性输出 80 根。K 线推送触发读取服务端历史快照，每 30 秒及网络恢复时另作校正，避免重连缺口。`forward` 读取服务端前复权数据；事件表示实际取得的快照，高频更新可能在请求等待期间合并。

综合资讯使用长桥公开 RSS，个股列表和正文使用长桥 API，仍需登录。RSS 若缺少发布时间，会读取正文信息补足。上游英文路径可能仍返回中文原文。

US 账户目前读取 US 资产持仓接口；该响应不能确认可用数量及模拟盘身份，分别输出 `null` 和 `unknown`。字段与替代接口的核对见 [US 持仓研究](docs/research/us-positions-fields.md)。其他地区按 API 的账户通道区分实盘/模拟盘。未知报价不计算为零市值；多币种分别汇总。

## 键位与有限窗口

| 场景 | 操作 |
| --- | --- |
| 通用 | `q`、Ctrl+C 退出；Esc 返回或取消当前层，根视图保持运行；`?` 帮助 |
| 表格、资讯、菜单 | `j/k`、Ctrl+N/P、上下方向键移动；Enter 确认；PageUp/PageDown 翻页；`g/G` 首尾 |
| 文本编辑 | 明确显示 INSERT / NORMAL；Esc 先退出 INSERT，再取消编辑；NORMAL 中 `i/a` 进入输入、`h/l` 移动、`x` 删除、`u` 撤销 |
| 自选 | 左右方向键、`[` / `]` 切组 |
| K 线 | 左右方向键选蜡烛；`1–8` 按上述周期顺序切换；`m` 显示/隐藏 MA5、MA10、MA20 |
| 资讯 | Enter 读取正文；Esc 回列表并保留选中项；正文可滚动 |
| 财报 | Enter 单期详情；`c` 多期对比；`[` / `]` 前后财期；`r` 市场反应；`f` 公告；`s` 切股票 |
| 事件行情 | `e` 编辑事件；`v` 选图形；`r` 选范围；Enter 查看选中行情细节 |
| 期权 | `[` / `]` 切到期日；Enter 详情；`r` 刷新；`s` 保存观察；`h` 本地历史 |
| 原文 | `d` 资料目录；`t` 章节；`[` / `]` 前后章节；`/` 搜索；`n/N` 前后匹配位置；`c` 显示引用 |

宽度不足时优先保留核心列；高度不足时滚动。`tick describe` 输出各 Tick Command 的最小尺寸：报价/自选 32×6、K 线 40×14、持仓 40×8、资讯/Reader 40×10、财报 100×14、Options 60×14。低于最小尺寸时显示所需/当前尺寸，数据接收继续；容器尺寸由 Herdr 控制。

网络临时错误保留最后可用内容并提示恢复状态；授权或参数错误仍会退出。显示状态依据实际 API 成功/失败与重试，不能据此推断 SDK WebSocket 的精确连接生命周期。报价昨收基准过旧或交易时段变化时暂不显示涨跌，等权威快照补齐。

## Tick Instance 流、Tick Tool 与 Tick Subscription

原生 Herdr 接入使用真实 stdin/stdout JSON-RPC 2.0：Native 协议 2、更新记录 3。`tick/describe` 返回开放类型和工具目录，`tick/read` 返回当前内容，`tool/call` 操作同一实例，`tick/update` 推送业务变化。可见示例在同一业务进程的控制终端上绘制，stdout 专用于协议。

Agent 通过一个 `CallTickTool` 调用声明的能力。`SubscribeTick` 属于当前 Loop，默认每 60 秒投递最新内容，范围 5–86400 秒；打开、读取和发现不会自动订阅。普通程序按 5 秒采集终端文字，之后等待 Loop cadence；采样不保证完整输出或即时响应。

外部接入示例见 [Counter](examples/ticks/counter.py)，协议约束见[原生 Herdr 接入](docs/design/native-herdr.md)。

## 退出码

| 退出码 | 含义 |
| --- | --- |
| 0 | 成功、主动退出或已呈现的单标的错误 |
| 2 | 参数、配置、TTY 或流错误 |
| 3 | 未授权或授权失效 |
| 4 | 网络、整体 API 或订阅清理失败 |

---

# 验证

模型请求失败时，Agent 显示 Provider、HTTP 状态、脱敏后的服务原文与下一步；状态行和连接 Details 保留最近一次失败，直到该连接的模型请求成功。额度／余额错误不会自动重试，也不表示未登录。可通过 `/provider` 切换连接，或打开服务返回的额度／订阅链接。Kimi Code 默认使用 `kimi-for-coding`；模型目录列出 K3 不代表当前套餐可用，403 的 monthly usage limit 也可能表示套餐不含该模型。详见 [Agent 排查说明](crates/agent/README.md#provider-请求失败排查)。

```sh
cargo fmt --all --check
cargo check --workspace --all-targets
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

`make check` 执行格式、严格 Clippy 和整仓测试，并为测试使用隔离状态目录、清除继承的 Herdr 调用者环境。测试的本地 HTTP mock 使用 NO_PROXY，避免开发机代理干扰回环请求。

主要测试入口是 Agent 工具与权限、Tick Subscription 驱动 Loop。真实 stdio 子进程和 PTY 验证协议、可见界面与终端恢复；隔离 Herdr 验证完整 Console 的双向操作。可复核步骤和当前结果见[实施记录](.scratch/herdr-workspace/implementation.md)。

`tick` 的自动测试使用离线数据与授权 HTTP 桩，默认不连接真实账户。真实 API 测试有 `live-api` feature 且默认忽略。测试模型自动操作、真实模型、用户亲手操作与金融账户验证分别记录。

# 后续范围

当前迁移以完整 Agent、原生 Herdr 接入和 example Tick 为交付中心。tmux、Zellij 适配、通用 Tick SDK、订阅字段条件和交易执行不在本轮范围内。关闭 Agent Pane 会退出该 Agent，未增加独立的后台 Loop 宿主。

金融数据 Tick 保留既有来源和授权约束。本轮无需真实金融账户的接入验收不能代表行情权限、线上数据或真实交易验证。

原生操作见 [Herdr 接入](docs/design/native-herdr.md)，Tick 契约见 [Tick 设计记录](docs/design/tick.md)，领域术语见 [CONTEXT.md](CONTEXT.md)。参考源码归属与第三方许可证见 [第三方声明](THIRD_PARTY_NOTICES.md)。
