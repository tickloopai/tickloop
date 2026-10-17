<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="site/src/site/brand/lockup-on-dark.svg">
    <img src="site/src/site/brand/lockup.svg" alt="Tickloop" width="265" height="50">
  </picture>
</h1>

Tickloop 提供完整的终端 Agent 和 example Tick。Tick 由 Tickhost 运行，可以在 Monitor 中或原生 Herdr 的 Pane 里查看；Agent 可以读取 Tick、调用它的工具，并通过明确订阅让内容变化推动 Loop 继续执行。

主命令是 `tickloop`。Herdr 内外都能打开、操作和订阅 Tick；在 Pane 中显示 Tick 目前只支持 Herdr。

| 成员 | 二进制 | 内容 |
| --- | --- | --- |
| `agent/tickloop` | `tickloop` | 完整 Agent Console、工具、权限、Transcript 和 Loop（由 `agent/` 下 console、client、daemon、runtime 等 crate 组成） |
| `tickhost/host` | `tickhost` | Tick 宿主服务（`tickhost run`，由 daemon 按需拉起，运行协议、终端与 Shell Tick）与 `tickhost stop` |
| `tick/reader` | `tick-reader` | Reader，随 Tickloop 安装 |
| `ticks/longbridge` | `tick-longbridge` | Tick 包 `longbridge`：Longbridge 接入，以及报价、自选、K 线、持仓、资讯、财报、社区动态与 `auth` |
| `ticks/futu` | `tick-futu` | Tick 包 `futu`：Futu OpenD 接入与期权 |
| `tick/packages` | — | Tick 包的清单 `tick-package.json`、Tick 市场的 `marketplace.json`，以及配置根中的安装、卸载、列出与登录状态 |
| `agent/packages` | — | Tick 市场的读取与下载，以及 `/ticks` 与 `tickloop tick` 共用的安装、更新、卸载、登录与退出登录（OAuth 经 `tick/oauth`，按 Agent 的 HTTP 策略） |
| `tick/oauth` | — | Tick 包登录的标准 OAuth 2.0 客户端（RFC 8414 发现、7591 注册、8628 设备码、6749 刷新、7009 撤销），Tickloop 与包的程序共用，HTTP 客户端由调用方提供 |
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

Tick Command 以终端命令形式提供扩展 Tick，一个可执行文件可以提供几个 Tick。每个程序只有自己的子命令，`describe` 只列出自己的 Tick 描述符：

| 可执行文件 | 命令 | 来源 |
| --- | --- | --- |
| `tick-reader` | `tick-reader URL或文件 […]`、`tick-reader describe` | 随 Tickloop 安装 |
| `tick-longbridge` | `tick-longbridge <quote\|watchlist\|kline\|positions\|news\|earnings\|community> …`、`tick-longbridge auth [status\|logout]`、`tick-longbridge describe` | Longbridge |
| `tick-futu` | `tick-futu options SYMBOL […]`、`tick-futu describe` | Futu OpenD |

Reader 读取原文与本地原文缓存，不需要登录；`describe` 是它唯一的子命令，名为 describe 的本地文件写成 `./describe`。每个 Tick 通过 Tick Instance 流输出 Tick Update，并在同一条流上声明和执行自己的 Tick Tool；既可以独立运行，也可以由 Agent 打开为 Tick。

## 安装

在 macOS 上用 Homebrew 安装预编译的二进制（Apple silicon 与 Intel），不需要本机 Rust：

```sh
brew install tickloopai/tap/tickloop
```

Homebrew 同时安装短命令 `tl`，与 `tickloop` 等效，例如 `tl`、`tl --help`、`tl tick list`。它是可执行文件的软链接，不修改 Shell 配置。

原生安装器随官网发布到 `https://tickloop.ai/install.sh`（本次代码尚未部署）：

```sh
curl -fsSL https://tickloop.ai/install.sh | sh
```

原生安装使用 `~/.local/bin` 下的入口和 `~/.local/share/tickloop/versions/` 下的完整版本目录。安装器选择当前 Mac 架构，固定发行标签、校验 SHA-256 后安装三个程序；遇到其他来源的已有安装会报告冲突，不自动接管。

开发者也可以用 cargo 从源码安装（源码公开后可用），默认安装默认分支的最新提交，加 `--tag vX.Y.Z` 安装某个发布版本：

```sh
cargo install --locked --git https://github.com/tickloopai/tickloop tickloop-agent tickloop-tickhost tickloop-tick-reader
```

这些安装方式都把 `tickloop`、`tickhost` 与 `tick-reader` 装进同一目录；daemon 只拉起与自己同目录、同构建的 `tickhost`，所以三者要一起安装和升级。

已有源码时，可以直接在仓库根目录用 Cargo 安装（需要当前 Rust stable）：

```sh
cargo install --locked --path agent/tickloop --bin tickloop
cargo install --locked --path tickhost/host --bin tickhost
cargo install --locked --path tick/reader --bin tick-reader
```

这三个目标也可用 `make install` 一起执行。默认安装到 `~/.cargo/bin`，安装三个程序期间保持源码不变。workspace 当前不发布 crates.io，不能省略 `--git`／`--path` 改用 `cargo install tickloop`。

### 更新

```sh
tickloop update           # 立即按当前安装来源升级完整套件
tickloop update --check   # 检查，不安装；brew 会刷新渠道元数据
tickloop update --status  # 查看最近一次自动／主动更新结果
```

原生和 Homebrew 安装在交互启动后及运行中每 6 小时后台更新；失败至少等待 1 小时再尝试，不阻塞界面。后台结果保存在安装来源独立的状态目录，用 `--status` 查看。

原生更新从官方公开 tap Release 下载整包，校验后原子切换 `current`；旧版本暂不自动清理。Homebrew 更新只调用 `brew update` 与指定 Tickloop 的 `brew upgrade`，尊重 brew pin，并为本次升级禁用自动 cleanup。Cargo Git 安装只主动更新：先在临时目录按目标发行标签编译并核对三个程序，再按已核实的 commit 经 Cargo 安装并检查结果；保留原 Git 来源，不转换成预编译安装。Cargo 最终写入由 Cargo 管理，不承诺三包原子替换，失败须检查报告后重试。本地 `--path` 安装提示回原源码目录更新并执行 `make install`，不自动改工作区。

Homebrew／Cargo 安装的 Console 在启动前保留完整固定运行副本，防止升级或 brew cleanup 删除后台 daemon 仍需执行的程序。更新不停止 daemon、Worker、Shell 或 Tickhost；新启动遇到旧版本时仍按现有界面选择。每次发行继续成套安装三个程序，即使 Tickhost 无单独业务改动；安装文件与替换有实例的运行中 Tickhost 分别处理。安装及运行副本不包含独立 Tick 包，更新也不自动升级它们。设计与验证边界见[自动更新设计](docs/design/auto-updates.md)。

`make release` 默认把 workspace 产品版本 patch 加一，在临时 checkout 中构建验证后创建专门的版本提交和 `vX.Y.Z` 标签并推送；`VERSION=X.Y.Z` 可指定更高版本。GitHub Actions 构建两个 macOS 压缩包，先将完整附件发布到公开仓库 `tickloopai/homebrew-tap` 的 draft Release，再公开发行并更新 Formula；同版本附件不可替换为不同内容。源码仓库私有期间，Homebrew 和原生更新器均从公开 tap 下载。详细步骤见[发布设计](docs/design/releases.md)。

在仓库检出中可直接构建并离线体验：

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

Tick 包是一个预编译的可执行文件、包清单 `tick-package.json` 和 `skills/` 目录，按数据来源划分：`longbridge` 包提供 `tick-longbridge`，`futu` 包提供 `tick-futu`。包装在配置根（`TICKLOOP_CONFIG_DIR`，未设置时为 `~/.tickloop`）的 `ticks/<包>/` 下，登记在 `ticks/installed.json`。安装只放置程序与 Skill：不运行包里的程序，不打开、不订阅 Tick，也不授予行情、账户或交易权限。包在清单的 `auth` 中声明数据来源的认证方式，与 MCP 一样只有三种：`none`（无需认证）、`api_key`（用户提供的密钥）与 `oauth2`（标准 OAuth 2.0 设备码授权）。Tickloop 是包的认证客户端：用户在 Agent 的 `/ticks` 或终端里的 `tickloop tick login|logout <包>` 发起登录，Tickloop 完成授权并把凭据保存在配置根的 `credentials/<包>/` 下；包的程序启动时经环境变量拿到凭据，OAuth 令牌由程序自己续期（见下）。

从官方 Tick 市场 [`tickloopai/ticks`](https://github.com/tickloopai/ticks) 安装预编译包，不需要 Rust 工具链：

```sh
tickloop tick list --available    # 市场中的包、最新版本、是否已安装及已装版本，以及当前平台能否安装
tickloop tick install longbridge
tickloop tick login longbridge    # 选择中国大陆或全球，打印验证链接与验证码并等待授权
tickloop tick login longbridge --issuer global
tickloop tick logout longbridge
tickloop tick uninstall longbridge
```

`login` 按清单的 `auth` 登录：`oauth2` 读授权服务器元数据（RFC 8414），每个 issuer 只做一次动态客户端注册（RFC 7591，公开客户端）并复用，再按 RFC 8628 打印验证链接（服务器提供 `verification_uri_complete` 时用它）与验证码，按服务器给的间隔轮询，处理 `authorization_pending`、`slow_down`、`expired_token` 与 `access_denied`，用户在任意设备的浏览器中确认后保存令牌；不需要全屏界面，也能在没有本机浏览器的远程主机上完成，`Ctrl+C` 取消。包有多个 issuer（如 `longbridge` 的中国大陆与全球）时用 `--issuer <名称>` 选择（不分大小写），不指定时在终端里输入名称选择，没有终端时报错并列出可选名称。授权服务器的地址只接受 https（http 只限本机地址，规则与 Tick 市场相同），元数据必须声明本 issuer、设备授权端点与设备码授权类型。`api_key` 在终端以不回显方式读入密钥（输入不是终端时读第一行）。包未安装或不需要登录（如 `futu`，登录在本机的 OpenD 中完成）时报错并说明。`logout` 删除保存的登录：OAuth 令牌的服务器提供撤销端点时先按 RFC 7009 撤销 refresh token（失败只提示，照常删除）。`tickloop tick list` 与 `list --available` 为已安装的包显示登录状态，只读凭据文件、不运行包的程序、不联网：没有凭据为 `needs login`；access token 未过期或有 refresh token 为 `logged in`；已过期且没有 refresh token 为 `login expired`；凭据文件无法解析为 `login unknown`；不需要登录为 `no login needed`。`/ticks` 显示同样的文字；给模型的 `tick_packages[].login` 仍是 `logged_in`、`logged_out`、`expired`、`not_required`、`unknown`。

凭据放在配置根的 `credentials/<包>/`（目录 0700、文件 0600），位置只由 Tickloop 按包名决定，清单不能指定：`oauth2` 为 `oauth2.json`，内容是 RFC 6749 令牌响应的字段（`access_token`、`token_type`、`refresh_token`、`scope`）加 `issuer`、`client_id`、`token_endpoint`、`revocation_endpoint` 与由 `expires_in` 换算的 `expires_at`（Unix 秒；服务器不给 `expires_in` 时按一小时）；每次写入都先取得同目录 `oauth2.lock` 的排他锁，再写同目录临时文件并 rename。各 issuer 注册得到的 `client_id` 存在 `oauth2-clients.json`，退出登录不删除；服务器不再认得它（`invalid_client`）时重新注册一次。`api_key` 为 `api_key` 文件。

Agent 中的 `/ticks` 是同一套安装、更新、卸载与登录的管理界面，在输入对话框中分已安装与市场两页，`Tab` 或左右键切页，列表用 `j`／`k` 或 `Ctrl+N`／`Ctrl+P` 选择、`Enter` 确认、`Esc` 逐层返回。已安装页每行是包名、版本与登录状态，`Enter` 打开该包的操作：登录与退出登录（`auth` 不是 `none` 的包）、更新（市场版本不同时）和卸载（先确认，说明保存的登录保留，运行中的 Tick 不受影响）。市场页每行是包名与说明、最新版本、已装版本，以及当前平台能否安装，`Enter` 安装或更新。登录在面板内完成，与 `/provider` 的设备码登录同样呈现：`oauth2` 的包显示验证链接（超链接）与验证码并等待授权，https 链接同时在浏览器中打开，多个 issuer 时操作菜单分列“Log in · <名称>”；`api_key` 的包在面板中以 `*` 遮掩输入密钥。`Esc` 取消进行中的登录。完成后该包的状态立即刷新，Agent 不需要重启，下一次请求就能看到。退出登录与 `tickloop tick logout` 相同（OAuth 令牌先撤销再删除）。安装与更新与 `tickloop tick install` 共用同一份实现，校验与失败语义相同，进行中显示状态，完成后只报告结果（如 `Installed futu 0.2.0`），包的 Skill 从当前 Chat 的下一次请求起生效；`Esc` 关闭面板不中断已开始的操作，结果改在对话中显示。市场取不到时市场页显示原因，已安装页照常使用。

`install` 按当前平台（构建 `tickloop` 时的目标三元组）选择下载，存到 `ticks/` 下的临时文件，用 `marketplace.json` 中的 SHA-256 校验后安装，登记的来源是下载地址。目前只提供 macOS Apple Silicon（`aarch64-apple-darwin`）的预编译包，其他平台安装时报错，不回退到源码编译。已装市场中的同一版本时报告已安装、不做改动，版本不同则替换。市场与下载只接受 HTTPS（http 只限本机地址），连接和下载停顿都有超时，下载不超过 256 MiB；市场取不到、内容无效、包不在市场中、平台不支持、下载失败或 SHA-256 不符时都不留临时文件，原安装与登记不变。`TICKLOOP_TICK_MARKETPLACE` 可以把市场换成别的 HTTPS 地址或本地文件，供开发与测试使用。首个版本发布到市场之前，市场中还没有包，请用下面的本机安装。

开发者在本机从源码安装：

```sh
make dev-ticks                # make package-ticks，再对每个包 tickloop tick install --path dist/ticks/<包>
tickloop tick list
tickloop tick uninstall longbridge
```

`make package-ticks` 为每个包构建 `aarch64-apple-darwin` release，在 `dist/ticks/<包>/` 组装包目录，打出 `dist/ticks/tick-<包>-<版本>-aarch64-apple-darwin.tar.gz`（解开后就是包目录），并输出它的 sha256 与对应的 `marketplace.json` 条目；它不发布任何内容。`make dev-ticks` 用这份源码构建的 `tickloop`（`cargo run`）安装，不使用 PATH 上的 `tickloop`。

`make release-ticks` 默认发布全部 Tick 包，各包从自己的版本 patch +1；`make release-ticks TICK_PACKAGES=longbridge` 只发布 Longbridge。版本来自各包的 `Cargo.toml`，同步其 `tick-package.json` 和 `Cargo.lock`，与 Agent 版本无关。要求干净的分支工作区，先在临时 checkout 中完成检查，再将整批版本文件提交为一条 `chore(release): …`，为每包创建 `tick-<包>-v<版本>` 标签，并推送当前分支与标签。`node scripts/release.mjs ticks --check` 只预检，不修改版本或 Git。全部产品的版本规则和失败恢复见[发布设计](docs/design/releases.md)。

每个包的标签触发 GitHub Actions，依次构建检查、发布到 `tickloopai/ticks`、匿名下载验证 SHA-256。各包排队发布，市场的 Release 标签仍是 `<包>-v<版本>`，已有且附件相同的 Release 可重跑继续。市场发布需配置 `TICKS_RELEASE_TOKEN`。包目录与压缩包包含第三方许可声明；`make package-ticks` 只打包，`make dev-ticks` 只在本机安装，均不递增版本或发布。

首次发布前的一次性设置：在 GitHub 为 `tickloopai` 组织创建细粒度个人访问令牌（Resource owner 选 `tickloopai`，Repository access 只选 `tickloopai/ticks`，Repository permissions 中 Contents 设为 Read and write；组织需允许细粒度令牌），把它存为 `griffinqiu/tickloop` 的 Actions 密钥 `TICKS_RELEASE_TOKEN`（Settings → Secrets and variables → Actions，或 `gh secret set TICKS_RELEASE_TOKEN --repo griffinqiu/tickloop`）。缺少这个密钥时 Publish 作业立即失败并说明。Publish 作业使用部署环境 `tick-marketplace`，首次运行时自动创建，可以在仓库设置中为它加审批人。令牌过期后更新这个密钥即可。

`tickloop tick install --path` 接受包目录或上面的压缩包，并总是替换已装的同名包：本地构建不一定改版本号，所以版本相同时也重新安装并报告“Reinstalled”。清单缺字段或无效、可执行文件不存在或不可执行、路径或符号链接越出包目录时安装失败，原安装与登记保持不变。`uninstall` 删除包目录与登记；配置根 `credentials/<包>/` 中的登录保留，输出会说明位置。

清单字段：`name`（小写字母、数字和 `-`）、`version`（与该包自己的 Cargo 版本相同，测试会核对）、`description`、`executable`（包内相对路径，文件名为 `tick-<name>`）、`protocol`，以及可选的 `auth`（缺省或 `null` 等于 `{"type": "none"}`）：

- `{"type": "none"}`：不需要登录。
- `{"type": "api_key", "env": "<变量名>", "label": "<说明>"}`：程序启动时密钥放在 `env` 中。变量名须为 `[A-Z][A-Z0-9_]*`，且不得是 `PATH`、`HOME`、`SHELL`、`USER`、`TMPDIR`，也不得以 `LD_`、`DYLD_`、`TICKLOOP_` 开头。
- `{"type": "oauth2", "issuers": [{"label": "<名称>", "issuer": "<https 地址>"}, …], "scopes"?: [...]}`：至少一个 issuer，名称与地址各不重复；issuer 是 RFC 8414 的 issuer 标识（https，http 只限本机地址，不含用户信息、查询或片段）；`scopes` 是请求的 scope（RFC 6749 的 scope-token），省略时用服务器的默认。

Agent 从同一个配置根读取已安装的包，不需要改 PATH。每个 Chat 加载各包 `skills/` 中的 Skill，名字带包前缀，如 `longbridge:usage`，正文中的 `${TICK_PACKAGE_ROOT}` 换成包的安装目录；包 Skill 与用户 Skill 一样不带额外权限，带前缀的名字也不会与内置、用户或项目 Skill 重名，格式错误或重名的只产生 warning。每次模型请求和 Skill 调用前，Chat（含子 Agent）都重新读取全部 Skill，所以 `/ticks` 或 `tickloop tick` 安装、更新、卸载包，以及修改用户或项目 Skill，都从下一次请求起生效，不需要 `/clear`。Skill 没变时 Skill 工具的定义逐字节不变，提示缓存照常命中；变了的那一次请求不命中。

打开 Tick 时，裸程序名依次在 Agent 旁的自带程序（`tick-reader`、`tickhost`）、已安装包的程序（`tick-<包>`）和 PATH 中查找；清单 `protocol` 为 true 的包程序不用传 `protocol: true` 就按扩展 Tick 打开，审批摘要写明来自哪个包。包的程序（按名称或路径打开都算）启动时由 Agent 守护进程按当时保存的登录补上凭据，只给这个实例：`oauth2` 包得到 `TICKLOOP_OAUTH_TOKEN_FILE`（`oauth2.json` 的绝对路径，未登录时也设置，由程序报告未授权），`api_key` 包在清单声明的变量中得到密钥（未登录时不设置）；Worker 环境中继承来的这两个变量一律让位，凭据只来自 Tickloop。重开丢失的实例时同样按当时的登录补上，登记中从不保存凭据。密钥只进入程序的环境，不进入审批摘要、审批依据、工具结果、Transcript 或 Monitor。新装的包马上可以打开，它的 Skill 从下一次请求起出现在当前 Chat 中。打开没装的包的程序（如 `tick-longbridge`）时报告包未安装并给出 `tickloop tick install <包>`，Agent 只提示用户运行，不自己安装。`make dev` 不构建 Tick 包，使用配置根中已安装的包；包有改动时另行执行 `make dev-ticks`（把本源码构建的包装进配置根，替换已装的同名包）。

## 授权

```sh
tickloop tick login longbridge     # 或在 Agent 的 /ticks 中登录
tick-longbridge auth               # 独立运行时自己登录，与 Tickloop 共用同一份登录
tick-longbridge auth status
tick-longbridge auth logout
```

Longbridge 的登录由 Tickloop 保存（见上节）：Agent 打开 `tick-longbridge` 时经 `TICKLOOP_OAUTH_TOKEN_FILE` 告诉它令牌文件；独立运行时没有这个变量，程序按 Tickloop 的规则自己算出同一个位置：配置根（`TICKLOOP_CONFIG_DIR` 非空时为它，否则 `~/.tickloop`）下的 `credentials/longbridge/oauth2.json`，所以在哪里登录都只需一次。`TICKLOOP_OAUTH_TOKEN_FILE` 必须是名为 `oauth2.json` 的绝对路径，否则按用法错误退出。早先的 `~/.tickloop/longbridge/auth.json` 不再读取也不迁移，升级后重新登录一次即可。

API 走登录所用授权服务器的网络：issuer 为 `https://openapi.longbridge.cn` 时走中国大陆，`https://openapi.longbridge.com` 时走全球；美国账户的凭证（`us_` 开头）始终走全球。没有单独的区域设置。

`tick-longbridge auth` 的登录在 TUI 中完成：方向键或 `j/k` 选择中国大陆或全球（即上面两个 issuer，默认是已保存登录所用的那个），Enter 确认。登录与 Tickloop 用的是同一个 OAuth 客户端（`tick/oauth`），做法相同：读授权服务器元数据（RFC 8414），按 RFC 7591 以公开设备客户端注册（与 Tickloop 共用 `oauth2-clients.json` 中的 `client_id`，服务器不认得时重新注册一次），再按 RFC 8628 申请设备码并轮询，令牌在 `oauth2.lock` 下写入 `oauth2.json`。随后在固定画面中显示验证网址、用户码和剩余有效时间，不会逐行刷倒计时，并像 Agent 自己的设备码登录一样用系统浏览器（macOS 的 `open`，其他平台的 `xdg-open`）打开带设备码的验证网页；只打开 https 网址，打不开时照常显示链接，登录继续。画面随链接加宽，尽量把链接放在一行；放不下时，支持 OSC 8 超链接的终端（判断规则与 Agent 相同）中链接按行折开，每一行都链接到完整网址，点任何一行都打开整个链接；不支持的终端改为显示不带设备码的验证网址，在网页中输入画面上的用户码，避免点击只拿到半个链接。请在自己可操作的设备打开网址确认，终端会自动完成登录。远程 agent 无法代替用户完成网页步骤；登录界面需要终端，Agent 里的 `!` 没有终端，请在 Agent 的 `/ticks` 中登录，或在自己的终端运行 `tickloop tick login longbridge`。

设备码确认期间，连接中断或授权端点的回应不是 OAuth 错误回应（带合法 `error` 的 400，或任何 401，按 RFC 6749 §5.2）时，例如 HTTP 5xx（即使带 `error`），会加倍轮询间隔重试（最长 60 秒），连续故障只提示一次；服务器要求 `slow_down` 时间隔加 5 秒并提示。重试以设备码原始有效期为限，明确拒绝或过期仍结束登录。

报价、自选、K 线、持仓和资讯的交互式命令共用这套授权界面。启动时未授权会先显示授权 TUI；运行中授权失效时先清理当前订阅，再进入授权 TUI。成功后继续或重启原来的 Tick Command。`--once` 和重定向输出属于非交互调用，不打开授权界面，提示在 Agent 的 `/ticks` 或终端里用 `tickloop tick login longbridge` 登录，并返回退出码 3。

每个 `tick-longbridge` 进程自己续期，Tickloop 不在后台刷新：access token 进入 SDK 的五分钟刷新窗口时（启动时或运行中），进程先取得同目录 `oauth2.lock` 的排他锁，重读令牌文件；别的进程已续期就直接使用，否则用 refresh token 向令牌文件中的 `token_endpoint` 刷新（带凭证所属数据中心的 `x-dc-region`），服务器轮换的 refresh token 一并保存，经同目录临时文件原子写回后才释放锁。几个 Tick 同时到期时只有一个刷新请求。刷新被拒（带合法 OAuth `error` 的 400，或任何 401）时提示在 Agent 的 `/ticks` 或终端里用 `tickloop tick login longbridge` 重新登录（退出码 3）；连不上授权服务器或其他非 2xx 回应（如 5xx，即使带 `error`）为网络错误（退出码 4），登录保留，下次续期再试。续期不会打开浏览器回调流程。`auth status` 仅读取令牌文件，不发起续期：access token 已过期但有 refresh token 时显示已登录，并说明打开 Tick 时续期，同时显示登录所在的网络；令牌文件无法解析时按未授权退出。`auth logout` 在锁下删除令牌文件，Tickloop 随即显示需要登录（撤销令牌请用 `tickloop tick logout longbridge`）。

## 配置

默认读取 `~/.tickloop/config.toml`，不存在时使用默认值。`--config PATH` 指定的文件必须存在；未知字段或无效值在进入 TUI 前报错。命令行参数仅覆盖本次运行。

```toml
color_mode = "red-up"       # red-up / green-up
language = "zh-CN"          # zh-CN / en
ambiguous_width = "narrow"  # narrow / wide
```

对应参数为 `--color-mode`、`--language`、`--ambiguous-width`（三个程序一致）。语言影响 API 名称及数字单位；不承诺将新闻原文翻译为英文。Longbridge 的接入网络由登录决定（见“授权”），早先版本写入的 `region` 字段照常读入、不再生效。

Tick Command 使用集中解析的终端渲染样式，容器外观由 Herdr 配置。

## 命令

| 命令 | 参数与行为 |
| --- | --- |
| `tick-longbridge auth [status\|logout]` | 设备码登录、查看保存的登录、删除保存的登录；与 Tickloop 共用同一份登录 |
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

扩展 Tick 使用真实 stdin/stdout 上按行 JSON-RPC 2.0 的 Tick 协议 3：`tick/describe` 返回 `kind`、`revision` 与工具目录，`tick/read` 返回当前状态 `{seq, data, at?}`，`tick/call` 带审批时的 `revision` 操作同一实例，`tick/update` 推送业务变化。可见示例在同一业务进程的控制终端上绘制，stdout 专用于协议。

Agent 通过一个 `TickCall` 调用声明的能力。`TickSubscription` 属于当前 Loop，由 `TickManage` 的 `subscribe`／`unsubscribe` 动作管理：扩展 Tick 与 Terminal Tick 选高频（每分钟，默认）、中频（每 10 分钟）或低频（每 30 分钟），到期投递最新内容；Shell 固定及时，只在退出时投递；Artifact 不接受订阅，经 Artifact 关联逐条交付 @tickloop 请求。打开、读取和发现不会自动订阅。Terminal Tick 到期读一次屏幕并与上次送出的文字比较，不保证完整输出或即时响应。

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
