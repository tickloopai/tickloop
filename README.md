# Tickloop

Tickloop 提供完整的终端 Agent 和 example Tick。Tick 由 Tickhost 运行，可以在 Monitor 中或原生 Herdr 的 Pane 里查看；Agent 可以读取 Tick、调用它的工具，并通过明确订阅让内容变化推动 Loop 继续执行。

主命令是 `tickloop`。Herdr 内外都能打开、操作和订阅 Tick；在 Pane 中显示 Tick 目前只支持 Herdr。

| 成员 | 二进制 | 内容 |
| --- | --- | --- |
| `agent/tickloop` | `tickloop` | 完整 Agent Console、工具、权限、Transcript 和 Loop（由 `agent/` 下 console、client、daemon、runtime 等 crate 组成） |
| `tickhost/host` | `tickhost` | Tick 宿主服务（`tickhost run`，由 daemon 按需拉起，运行协议、终端与 Shell Tick）与 `tickhost stop` |
| `tick/reader` | `tick-reader` | Reader，随 Tickloop 安装 |
| `ticks/longbridge` | `tick-longbridge` | Tick 包 `longbridge`：Longbridge 接入，以及报价、自选、K 线、持仓、资讯、财报、社区动态与 `auth` |
| `ticks/futu` | `tick-futu` | Tick 包 `futu`：Futu OpenD 接入与期权 |
| `tick/packages` | — | Tick 包的清单 `tick-package.json`、Tick 市场的 `marketplace.json`，以及 `tickloop tick` 的安装、卸载与列出 |
| `tick/command` | — | Tick Command 的共享入口：全局参数、配置、locale 检查、`describe` 与运行 Tick |
| `tick/market` | — | 金融 Tick 本体，只经 provider 接口读取数据 |
| `tick/provider` | — | provider 接口，由 `ticks/` 下的包与测试 mock 实现 |
| `tick/test-support` | `tick-mock`（仅测试） | 测试用的模拟数据，以及用它整程序运行行情 Tick 的 `tick-mock`；`make build`／`install`／`package` 不包含它 |
| `tick/protocol`、`tick/sdk` | — | Tick 标准，以及 Tick 侧的协议实现 |
| `tickhost/connect` | — | Tickhost 业务通道、每实例 Session、launch 契约与 Herdr 视图布局 |
| `ui` | — | 共享终端呈现规则 |

使用当前 Rust stable（edition 2024）。`make build` 构建 Agent、Tickhost 与 Reader；`make install` 安装 `tickloop`、`tickhost` 和 `tick-reader`；`make package` 打包这三个入口。`tick-longbridge` 与 `tick-futu` 不随 Tickloop 安装，作为 [Tick 包](#tick-包)单独安装。daemon 只拉起与自己同目录、同版本的 `tickhost`，版本不一致时拒绝启动并报错；`tickhost stop` 结束 Tickhost 运行的全部 Tick，下次 Agent 打开 Tick 时自动启动新的 Tickhost，被结束的 Tick 显示为丢失，只在明确要求时重开；它只找默认或 `TICKLOOP_TICKHOST_RUNTIME_DIR` 指定的 Tickhost，运行目录非默认的 daemon 使用自己目录下的 `tickhost/`。

## 双向操作

Agent 会确认运行环境，并在 Tickhost 可用时自动加载 Tick 操作指引；Herdr 内外都可以打开、操作和订阅 Tick。可以直接说“看看特斯拉股价”或“打开特斯拉 K 线”；Agent 优先用 Tick 展示，并读取实例核对结果。真实行情需要 Longbridge 授权。Tick 由 Tickhost 运行并出现在 Monitor 中；在 Monitor 对它按 Enter 即可查看，Herdr 内为浮窗，否则全屏，Esc 隐藏视图而 Tick 继续运行。

在 Herdr 中要并排观察，可以说“在你当前 Tab 右边打开一个 K 线 Tick”；已有 Tick 则说“把刚才那个 Tick 显示到你这个 Tab”。Pane 只是视图：移动它保留实例及订阅，关闭它 Tick 继续运行。支持上下左右四个方向分屏，操作不抢焦点。tmux 和 Zellij 当前仅识别并提示不能在其 Pane 中显示 Tick。

1. 运行 `tickloop`，按 [Agent 说明](agent/tickloop/README.md) 配置模型。
2. 在 Agent 中说：“打开 `tick-longbridge kline 700.HK`，并高频订阅它的变化。”Agent 在 Tickhost 中打开它（需要先安装 [Tick 包](#tick-包) `longbridge`，并完成 Longbridge [授权](#授权)），再为当前 Loop 建立订阅。
3. 在 Monitor 中对它按 Enter 打开视图，按 `5` 切换到 5 分钟周期，再按 Esc 隐藏；一分钟内所属 Loop 收到新内容并继续执行。
4. 在 Agent 中说：“在刚才那张图上添加一个标注，然后移除它。”Agent 读取 `mark` / `unmark` 的参数并经权限流程操作，结果显示在同一个 Tick 中。

不讲 Tick 协议的交互式程序（日志查看、REPL 等）也可以让 Agent 打开为 Terminal Tick：Agent 读取屏幕文字并输入；订阅后屏幕文字的变化推动 Loop，但它不提供结构化工具目录和完整历史。外部协议程序让 Agent 以 `protocol: true` 打开，例如 `python3 tick/examples/counter.py`；在 Counter 的视图中输入 `+` 后回车可产生更新，也可让 Agent 调用 `increment`。人在 Pane 或终端里自己运行的程序不是 Tick，Agent 不读取它。

打开和读取不会自动订阅。默认范围是当前 Workspace；跨 Workspace 要明确指定。detach/attach 保留活进程；结束 Tick 要让 Agent 关闭它或在 Monitor 中关闭；重开后的实例要重新选取。操作、协议和边界说明见 [Tickhost 与 Herdr 视图](docs/design/native-herdr.md)。

# Tick Command

Tick Command 以终端命令形式提供协议 Tick，一个可执行文件可以提供几个 Tick。每个程序只有自己的子命令，`describe` 只列出自己的 Tick 描述符：

| 可执行文件 | 命令 | 来源 |
| --- | --- | --- |
| `tick-reader` | `tick-reader URL或文件 […]`、`tick-reader describe` | 随 Tickloop 安装 |
| `tick-longbridge` | `tick-longbridge <quote\|watchlist\|kline\|positions\|news\|earnings\|community> …`、`tick-longbridge auth [status\|logout]`、`tick-longbridge describe` | Longbridge |
| `tick-futu` | `tick-futu options SYMBOL […]`、`tick-futu describe` | Futu OpenD |

Reader 读取原文与本地原文缓存，不需要登录；`describe` 是它唯一的子命令，名为 describe 的本地文件写成 `./describe`。每个 Tick 通过 Tick Instance 流输出 Tick Update，并在同一条流上声明和执行自己的 Tick Tool；既可以独立运行，也可以由 Agent 打开为 Tick。

## 安装

```sh
cargo build --locked --release --workspace --bins
./target/release/tick-reader --help
./target/release/tick-longbridge --help
```

安装入口使用 `make install`，把 `tickloop`、`tickhost` 与 `tick-reader` 放入 PATH；`tick-longbridge` 与 `tick-futu` 见下面的 [Tick 包](#tick-包)。交互模式要求 UTF-8 locale 和支持真彩色的终端；独立运行使用 TTY，由 Agent 打开时 Tickhost 的 launcher 保留控制终端，并把 stdin/stdout 留给协议。macOS 为当前目标环境。`--help` 和 `describe` 无需 UTF-8 locale 或登录。图标只有一套 Unicode 字形，不需要特殊字体。

`--size` 以字符列×行限制显示区域，仍受实际终端尺寸约束，三个程序都可用。

剩余人工验收可使用临时终端向导：

```sh
bash .scratch/longbridge-ticks/accept.sh live
```

`live` 包含设备码登录和真实数据视图。向导由用户在终端运行，只把通过/失败/跳过写到临时结果文件，不记录账户内容，也不自动将验收项勾选为通过。

## Tick 包

Tick 包是一个预编译的可执行文件、包清单 `tick-package.json` 和 `skills/` 目录，按数据来源划分：`longbridge` 包提供 `tick-longbridge`，`futu` 包提供 `tick-futu`。包装在配置根（`TICKLOOP_CONFIG_DIR`，未设置时为 `~/.tickloop`）的 `ticks/<包>/` 下，登记在 `ticks/installed.json`。安装只放置程序与 Skill：不运行包里的程序，不打开、不订阅 Tick，也不授予行情、账户或交易权限；数据来源的登录仍由包自己的命令完成（`tick-longbridge auth`）。

从官方 Tick 市场 [`tickloopai/ticks`](https://github.com/tickloopai/ticks) 安装预编译包，不需要 Rust 工具链：

```sh
tickloop tick list --available    # 市场中的包、最新版本、是否已安装及已装版本，以及当前平台能否安装
tickloop tick install longbridge
tickloop tick uninstall longbridge
```

`install` 按当前平台（构建 `tickloop` 时的目标三元组）选择下载，存到 `ticks/` 下的临时文件，用 `marketplace.json` 中的 SHA-256 校验后安装，登记的来源是下载地址。目前只提供 macOS Apple Silicon（`aarch64-apple-darwin`）的预编译包，其他平台安装时报错，不回退到源码编译。已装市场中的同一版本时报告已安装、不做改动，版本不同则替换。市场与下载只接受 HTTPS（http 只限本机地址），连接和下载停顿都有超时，下载不超过 256 MiB；市场取不到、内容无效、包不在市场中、平台不支持、下载失败或 SHA-256 不符时都不留临时文件，原安装与登记不变。`TICKLOOP_TICK_MARKETPLACE` 可以把市场换成别的 HTTPS 地址或本地文件，供开发与测试使用。首个版本发布到市场之前，市场中还没有包，请用下面的本机安装。

开发者在本机从源码安装：

```sh
make install-ticks                # make package-ticks，再对每个包 tickloop tick install --path dist/ticks/<包>
tickloop tick list
tickloop tick uninstall longbridge
```

`make package-ticks` 为每个包构建 `aarch64-apple-darwin` release，在 `dist/ticks/<包>/` 组装包目录，打出 `dist/ticks/tick-<包>-<版本>-aarch64-apple-darwin.tar.gz`（解开后就是包目录），并输出它的 sha256 与对应的 `marketplace.json` 条目；它不发布任何内容。`make install-ticks` 用这份源码构建的 `tickloop`（`cargo run`）安装，不使用 PATH 上的 `tickloop`。

发布到 Tick 市场用 `make publish-ticks`（市场仓库的本地克隆由 `TICKS_CHECKOUT` 指定，默认 `~/github/ticks`）。它在同一次运行中执行 `package-ticks`，检查每个压缩包只含 `tick-package.json`、`bin/` 与 `skills/`、程序是带 ad-hoc 签名的 arm64 可执行文件且版本与清单一致、发布程序的普通依赖中没有 `tick/test-support`，并用本源码的 `tickloop` 从压缩包试装；再把压缩包复制到 `dist/release/`，更新克隆中 `marketplace.json` 的版本、说明、下载地址与 SHA-256 以及 README 的包列表，最后打印 `gh release create`、`git commit` 与 `git push` 命令。它不创建 Release，也不提交或推送：发布是对外操作，由用户自己执行或当次明确授权。市场中已列出的版本不会被覆盖，要重新发布须先升版本。

`tickloop tick install --path` 接受包目录或上面的压缩包，并总是替换已装的同名包：本地构建不一定改版本号，所以版本相同时也重新安装并报告“Reinstalled”。清单缺字段或无效、可执行文件不存在或不可执行、路径或符号链接越出包目录时安装失败，原安装与登记保持不变。`uninstall` 删除包目录与登记；数据来源的登录凭据（如 `~/.tickloop/longbridge/`）保留，输出会说明位置。

清单字段：`name`（小写字母、数字和 `-`）、`version`（与工作区版本相同，测试会核对）、`description`、`executable`（包内相对路径，文件名为 `tick-<name>`）、`protocol`，以及可选的 `credentials`（程序保存数据来源凭据的位置，卸载时提示）。

Agent 从同一个配置根读取已安装的包，不需要改 PATH。每个新建的 Chat（以及换工作目录时）加载各包 `skills/` 中的 Skill，名字带包前缀，如 `longbridge:longbridge`，正文中的 `${TICK_PACKAGE_ROOT}` 换成包的安装目录；包 Skill 与用户 Skill 一样不带额外权限，带前缀的名字也不会与内置、用户或项目 Skill 重名，格式错误或重名的只产生 warning。

打开 Tick 时，裸程序名依次在 Agent 旁的自带程序（`tick-reader`、`tickhost`）、已安装包的程序（`tick-<包>`）和 PATH 中查找；清单 `protocol` 为 true 的包程序不用传 `protocol: true` 就按协议 Tick 打开，审批摘要写明来自哪个包。新装的包马上可以打开，它的 Skill 在之后新建的 Chat 中出现。打开没装的包的程序（如 `tick-longbridge`）时报告包未安装并给出 `tickloop tick install <包>`，Agent 只提示用户运行，不自己安装。`make dev` 启动的开发版 Agent 同样使用配置根中已安装的包，先用 `make install-ticks` 安装。

## 授权

```sh
tick-longbridge auth
tick-longbridge auth status
tick-longbridge auth logout
```

授权在 TUI 中完成：方向键或 `j/k` 选择中国大陆或全球接入区域，Enter 确认；随后在固定画面中显示验证网址、用户码和剩余有效时间，不会逐行刷倒计时。请在自己可操作的设备打开网址确认，终端会自动完成登录。远程 agent 无法代替用户完成网页步骤；在用户终端运行 `! tick-longbridge auth`。

设备码确认期间，连接中断或授权端点返回 HTTP 5xx 会按原轮询间隔退避重试，保留当前端点并只提示一次连续故障；代理返回的 502 也按同一规则处理。重试以设备码原始有效期为限，明确拒绝或过期仍结束登录。

报价、自选、K 线、持仓和资讯的交互式命令共用这套授权界面。启动时未授权会先显示授权 TUI；运行中授权失效时先清理当前订阅，再进入授权 TUI。成功后继续或重启原来的 Tick Command。`--once` 和重定向输出属于非交互调用，不打开授权界面，仍提示运行 `tick-longbridge auth` 并返回退出码 3。

令牌存于 `~/.tickloop/longbridge/auth.json`，明文 JSON、0600 权限、原子替换。客户端注册信息单独保存并复用。启动及持续运行期间自动续期，授权失效会提示重新运行 `tick-longbridge auth`，不会打开浏览器回调流程。`auth status` 仅读取本地状态，不发起续期；`auth logout` 删除本地令牌。

## 配置

默认读取 `~/.tickloop/config.toml`，不存在时使用默认值。`--config PATH` 指定的文件必须存在；未知字段或无效值在进入 TUI 前报错。命令行参数仅覆盖本次运行。

```toml
color_mode = "red-up"       # red-up / green-up
language = "zh-CN"          # zh-CN / en
ambiguous_width = "narrow"  # narrow / wide
# region = "global"        # cn / global，默认未配置，授权时询问
```

对应参数为 `--color-mode`、`--language`、`--ambiguous-width`（三个程序一致）与 `--region`（只属于 `tick-longbridge`）。语言影响 API 名称及数字单位；不承诺将新闻原文翻译为英文。区域是接入网络选择，AP/US 数据中心由账户凭证确定。

Tick Command 使用集中解析的终端渲染样式，容器外观由 Herdr 配置。

## 命令

| 命令 | 参数与行为 |
| --- | --- |
| `tick-longbridge auth [status\|logout]` | 设备码登录、查看本地状态、退出登录 |
| `tick-longbridge quote TSLA 700` | 一个或多个标的；纯字母补 `.US`，纯数字补 `.HK`；单标的错误显示在对应行 |
| `tick-longbridge watchlist [--group ID或名称]` | 默认第一个自选分组；左右方向键或 `[` / `]` 切组 |
| `tick-longbridge kline 700.HK [--period day] [--count 80] [--adjust none]` | 单标的 K 线；周期 `day/week/month/1m/5m/15m/30m/60m`；数量 1–1000；复权 `none/forward` |
| `tick-longbridge positions` | 股票持仓、按币种汇总市值和浮盈亏；只读账户，不执行交易 |
| `tick-longbridge news [700.HK] [--limit 20]` | 个股资讯；无代码时读长桥公开 RSS；数量 1–100 |
| `tick-longbridge earnings TSLA.US [--count 10]` | 已发布财报的预期、实际、意外与财报前后走势；只支持美股；期数 1–20 |
| `tick-longbridge community 91` | 一位社区用户最新的帖子，参数为个人主页链接中的数字 member_id；每 30 秒刷新。资料与动态读 longbridge.com 公开接口，Tick Tool `reply` 以当前登录账号在列出的帖子下公开回复，只发送一次，不自动重试 |
| `tick-futu options AAPL.US [--expiry YYYY-MM-DD] [--host 127.0.0.1] [--port 11111]` | 富途 OpenD 的期权链、买卖价、IV、持仓量和近平值跨式参考；支持美股/港股标的 |
| `tick-reader URL或文件 [更多URL或文件]` | 在终端阅读 HTML、Markdown、文本或 PDF 原文，搜索、章节定位和固定版本引用 |

九个业务命令默认进入 TUI，均支持 `--once` 和 `--once --format json`。JSON 输出没有横幅或 ANSI；普通行情表按业务行输出，财报、期权、事件行情和原文还保留各自的来源、时间和结构。缺失值为 `null` / `--`，管道表格不输出颜色。

```sh
tick-longbridge quote TSLA 700 --once --format json
tick-longbridge watchlist --group Technology --color-mode green-up
tick-longbridge kline 700.HK --period 5m --adjust forward
tick-longbridge positions --once
tick-longbridge news --limit 10
tick-longbridge earnings TSLA.US --count 12 --once --format json
tick-longbridge community 91 --once --format json
tick-longbridge kline AAPL.US --event-date 2026-07-30 --event-session after-hours --published-at 2026-07-30T16:05:00-04:00
tick-futu options AAPL.US --host 127.0.0.1 --port 11111
tick-reader ./earnings-release.pdf --symbol AAPL.US --fiscal-period FY2026Q3
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
| 通用 | `q`、Ctrl+C 退出；Backspace 返回上一层，根视图保持运行；`?` 帮助（Backspace 也关闭）；Esc 与 Ctrl+B 留给附着视图，Tick 不绑定 |
| 表格、资讯、菜单 | `j/k`、Ctrl+N/P、上下方向键移动；Enter 确认；PageUp/PageDown 翻页；`g/G` 首尾 |
| 文本编辑 | 没有模式：可见字符（含 `q`、`j`、`k`）直接输入；←/→、Home/End 移动；Backspace/Delete 删除；Enter 应用；Ctrl+C 取消编辑；事件编辑用 Tab/Shift+Tab 切换字段 |
| 自选 | 左右方向键、`[` / `]` 切组 |
| K 线 | 左右方向键选蜡烛；`1–8` 按上述周期顺序切换；`m` 显示/隐藏 MA5、MA10、MA20 |
| 资讯 | Enter 读取正文；Backspace 回列表并保留选中项；正文可滚动 |
| 财报 | Enter 单期详情；`c` 多期对比；`[` / `]` 前后财期；`r` 市场反应；`f` 公告；`s` 切股票 |
| 事件行情 | `e` 编辑事件；`v` 选图形；`r` 选范围；Enter 查看选中行情细节 |
| 期权 | `[` / `]` 切到期日；Enter 详情；`r` 刷新；`s` 保存观察；`h` 本地历史 |
| 原文 | `d` 资料目录；`t` 章节；`[` / `]` 前后章节；`/` 搜索；`n/N` 前后匹配位置；`c` 显示引用 |

宽度不足时优先保留核心列；高度不足时滚动。各程序的 `describe` 输出其 Tick 的最小尺寸，即关键字段完整显示的最窄宽度：报价 52×6、自选 44×6、K 线 45×14（事件图 60×14）、持仓 49×8、资讯/Reader 40×10、财报 62×14、Options 58×14；80 列下所有 Tick 完整可用。窗口小于最小尺寸时，Tick 仍按最小尺寸布局，底行显示可见范围与滚动键：`<`／`>` 左右滚动，`{`／`}` 上下滚动，选中行和光标自动保持在可见范围内；数据接收继续，容器尺寸由 Herdr 控制。

网络临时错误保留最后可用内容并提示恢复状态；授权或参数错误仍会退出。显示状态依据实际 API 成功/失败与重试，不能据此推断 SDK WebSocket 的精确连接生命周期。报价昨收基准过旧或交易时段变化时暂不显示涨跌，等权威快照补齐。

## Tick Instance 流、Tick Tool 与 Tick Subscription

协议 Tick 使用真实 stdin/stdout 上按行 JSON-RPC 2.0 的 Tick 协议 3：`tick/describe` 返回 `kind`、`revision` 与工具目录，`tick/read` 返回当前状态 `{seq, data, at?}`，`tick/call` 带审批时的 `revision` 操作同一实例，`tick/update` 推送业务变化。可见示例在同一业务进程的控制终端上绘制，stdout 专用于协议。

Agent 通过一个 `CallTickTool` 调用声明的能力。`SubscribeTick` 属于当前 Loop：协议 Tick 与 Terminal Tick 选高频（每分钟，默认）、中频（每 10 分钟）或低频（每 30 分钟），到期投递最新内容；Shell 固定及时，只在退出时投递；产物不接受订阅，经产物关联逐条交付 @tickloop 请求。打开、读取和发现不会自动订阅。Terminal Tick 到期读一次屏幕并与上次送出的文字比较，不保证完整输出或即时响应。

外部接入示例见 [Counter](tick/examples/counter.py)，协议约束见 [Tickhost 与 Herdr 视图](docs/design/native-herdr.md#tick-协议-3)。

## 退出码

| 退出码 | 含义 |
| --- | --- |
| 0 | 成功、主动退出或已呈现的单标的错误 |
| 2 | 参数、配置、TTY 或流错误 |
| 3 | 未授权或授权失效（`tick-longbridge`） |
| 4 | 网络、整体 API 或订阅清理失败 |

---

# 验证

模型请求失败时，Agent 显示 Provider、HTTP 状态、脱敏后的服务原文与下一步；状态行和连接 Details 保留最近一次失败，直到该连接的模型请求成功。额度／余额错误不会自动重试，也不表示未登录。可通过 `/provider` 切换连接，或打开服务返回的额度／订阅链接。Kimi Code 默认使用 `kimi-for-coding`；模型目录列出 K3 不代表当前套餐可用，403 的 monthly usage limit 也可能表示套餐不含该模型。详见 [Agent 排查说明](agent/tickloop/README.md#provider-请求失败排查)。

```sh
cargo fmt --all --check
cargo check --workspace --all-targets
cargo nextest run --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

整仓测试由 cargo-nextest 运行（`cargo install --locked cargo-nextest`），每个测试独立进程，慢测试超时与 PTY 分组见 `.config/nextest.toml`。`make check` 执行格式、严格 Clippy 和整仓测试（`make test`），并为测试使用隔离状态目录、清除继承的 Herdr 调用者环境。测试的本地 HTTP mock 使用 NO_PROXY，避免开发机代理干扰回环请求。

主要测试入口是 Agent 工具与权限、Tick Subscription 驱动 Loop。真实 stdio 子进程和 PTY 验证协议、可见界面与终端恢复；真实 Tickhost 与隔离 Herdr 验证完整 Console 的双向操作与视图 Pane。可复核步骤和当前结果见[本机 Tick 统一规格的工单](.scratch/tickhost-unification/issues/)。

Tick Command 的自动测试使用离线数据、授权 HTTP 桩与本机假 OpenD，默认不连接真实账户。真实 API 测试有 `live-api` feature 且默认忽略。测试模型自动操作、真实模型、用户亲手操作与金融账户验证分别记录。

# 后续范围

当前以完整 Agent、Tickhost、Herdr 视图和 example Tick 为交付中心。tmux、Zellij 中的视图、通用 Tick SDK、订阅字段条件和交易执行不在本轮范围内。关闭 Agent Pane 只关闭界面，Agent 在本机 daemon 中继续运行。

金融数据 Tick 保留既有来源和授权约束。本轮无需真实金融账户的接入验收不能代表行情权限、线上数据或真实交易验证。

运行与视图见 [Tickhost 与 Herdr 视图](docs/design/native-herdr.md)，Tick 契约见 [Tick 设计记录](docs/design/tick.md)，领域术语见 [CONTEXT.md](CONTEXT.md)。参考源码归属与第三方许可证见 [第三方声明](THIRD_PARTY_NOTICES.md)。
