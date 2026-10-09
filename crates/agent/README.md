# Tick Loop Agent

Agent 的权限、问题、确认、工作目录信任、设置、帮助、`/resume` 与 `/artifacts` 选择器、`/tasks` 任务中心、`/permissions` 管理、订阅管理，以及 `/model`、`/provider` 的内置选择器、连接详情、凭据编辑和登录使用底部固定 12 行的输入对话框（包含顶端全宽规则线、标题和末行提示），替换输入框、上下分隔线和 Subagents 面板。终端过矮时容器缩小，上方内容仍保留；关闭后恢复原草稿与光标。Agent 请求优先于用户打开的菜单，完成后恢复被打断的界面；界面状态随 Chat 保存。最近一条用户消息显示在左栏首行，Monitor 从同一行开始；整页 Agents、Transcript、Diff 与右侧 Shell 详情保留各自的查看方式。


主命令为 `tickloop`。Herdr 内自动接入 Tick，宿主外可独立使用；双向操作见[原生 Herdr 接入](../../docs/design/native-herdr.md)。

运行环境和接入状态会提供给模型；Herdr 接入可用时，首个对话请求前自动加载内置 `tickloop` Skill。Tick 的内容和工具由实例动态声明，查看股价或 K 线默认优先通过 Tick 展示。tmux、Zellij 当前提示尚未接入，独立运行不提供跨 Tick 能力。

通用 Agent 的 Rust library 与完整交互式终端。默认 CLI 是本机 daemon 的 View；每次独立根任务由受监督 worker 持有 runtime。多个 View 可连接同一执行，各自草稿独立。Herdr 持有布局与终端生命周期；HostToolProvider 与 Tick Subscription 使用 Agent 工作上下文，界面离开后执行继续。

从仓库根目录启动：

```sh
cargo run -p tickloop-agent
```

To replace an incompatible local Agent service, run this build's stop command, then launch again:

```sh
./target/debug/tickloop service stop
./target/debug/tickloop
```

This stops the service and its running Agents. Saved history is kept. `TICKLOOP_AGENT_RUNTIME_DIR` selects the service; keep it unchanged between these commands. Closing a View still leaves its work running. The next launch starts this build's service without automatically continuing saved work.

从仓库根目录执行 `make package`，将 Agent 与 example Tick 编译为 Release 并打包到 `dist/tickloop-<target>.tar.gz`，默认使用当前主机的 Rust target。压缩包包含 `tickloop` 和 `tick`，不包含 `.env`、配置或会话数据；解压后在匹配的系统与架构上运行，无需安装 Rust。模型凭据通过下述配置入口提供，默认状态目录为 `~/.tickloop`，可用 `TICKLOOP_CONFIG_DIR` 指定。

省略 `--cwd` 时使用当前目录。需要指定工作目录时，传入一个已经存在的实际目录，例如 `--cwd "$PWD"`；程序不会创建工作目录。

不带参数启动进入空白新 Agent 输入，首次发送才创建执行；空输入按 Left 进入本机全部 Agent 总览。总览输入创建新 Agent，提交后留在总览，默认目录取本次 CLI 启动 cwd；项目／Workspace 筛选不改变派发目录，`/cwd PATH` 可显式替代。总览按状态区分待审批与待回答，根行下展开真实 child；`r` 打开历史选择器。`-c`／`--continue` 继续本目录最近活动的 Chat；`-r`／`--resume CHAT` 按 id 恢复，省略 id 时打开 Chat 选择器；`--fork-session` 只能与 `-c` 或 `-r` 一起使用，把恢复的对话复制成新 Chat 并写入新的 Transcript，原记录不变。自动化可用 `--chat-id CHAT` 打开或创建一个固定身份，不与上述恢复参数或 `--no-session-persistence` 组合。`--no-session-persistence` 让本次运行不保存 Chat，也不在工作目录留下文件；`settings.json` 中 `cleanupPeriodDays` 为 0 时同样不保存 Chat（见下文“过期文件清理”）。一个 View 同一时间显示一个 Agent；已接受任务不因切换或关闭该 View 停止。


总览只列顶层家族，以后台生成的短 Chat 标题和最近消息预览辨认；查看子 Agent 不改变家族标题。输入下方 Subagents 面板来自同一执行目录与运行层不可变代号注册表。进入当前 Agent 的后代始终原位；其他 Agent 若在同一 Herdr 实例有活 View，优先经核实的 Pane 导航，多位置无法确定首选时选择。否则缓存当前界面后原位连接。进入 child 后，面板的 main／子代号标明当前查看对象，@handle 标签与 Message @handle… 提示同步显示，后续文字仍路由到其稳定身份；审批／问题附近面板标实际 requester。`Subagent` 创建真实独立 Chat／Loop，后续输入保持冻结授权上限；根可用 StopSubagent 停止直接子 Agent；嵌套委派、团队调度、child 历史改写和整家族 stop 不提供，拒绝不会转发给 parent。

`Subagent {task,name?,description?,run_in_background?}` 注册不可复用的家族代号：前台等待实际报告并返回工具结果；后台立即返回，完成通知自动进入父 Loop 并携带状态、结果与用量。`SendMessage {to,message,relay_user_message_id?}` 自动送达同一家族的根、直接子和同级子；忙时在下个工具回合接入，空闲时开始新回合。正常完成、失败或停止的子任务可按原冻结权限继续，停止的根和过期任务仍需要真人 Continue。`ListSubagents` 用于发现不可变代号与实际状态；`StopSubagent {to}` 只供根停止直接 child 的本次执行。模型无需查询收件箱或等待结果的另一个工具。Agent 消息与核验后的转述始终不是人类审批或新增授权。真实 @handle 输入保留来源、附件与原始字节，不直接启动来源模型。独立 root、跨家族、嵌套和广播不开放。详见[家族消息合同](../../.scratch/agent-layout-redesign/parent-child-messaging.md)。

owner 自动启动使用当前登录用户私有 Unix socket，启动锁协调并发 CLI，不安装 launchd 服务。关闭终端／Pane、`/exit` 或 View EOF 只撤界面；owner／worker 退出后旧 execution 失效。`/resume` 打开历史不执行，明确向原 Loop Continue 才恢复其原有效 Trigger；没有自动崩溃续跑或跨崩溃 exactly-once 承诺。确认丢失先查询原 CommandId，创建仍未知时保留输入并阻止盲目重复创建；检查总览与历史后，可按提示 `/acknowledge-create COMMAND_ID` 显式承认未知，以允许另一条新提交，此动作不重放旧命令。New Agent 的暂存选择逐条以回执证明后才提交首个输入，未知或被拒的选择不会重发，也不会让输入落到配置不全的 Agent；同一命令可取消其余选择。普通输入结果未知时原样恢复到输入区，在不改动的情况下再次 Enter 只读取原收据：已被接受就采纳原结果、不再发送；改动后的输入等原结果确定后作为新输入发送；`/acknowledge-input COMMAND_ID` 只停止跟踪、不重发。

输入区自然增长并独立滚动，正文使用底部真实占位后的空间。下方展开 Agents 会缩小正文；Todo 始终在消息流左栏的 Working 下方，有条目时空闲也保留，全部完成后保留 5 秒再持久化清空；Shell Tick 在内容区右侧的 Monitor 中显示，输入和底栏保持全宽；帮助在输入对话框内显示，替换整个输入区及 Subagents 面板。条件顶栏只摘要当前正在阅读且 user 消息已滚出视口的回合。`settings.json` 的可选 `statusLines` 是字符串数组，未配置使用默认状态行，空数组隐藏普通状态行，最多显示三条物理行；必要连接／未知提示仍占一行，帮助或请求界面临时隐藏普通 status。预算、焦点与可交互原型见[布局契约](../../.scratch/agent-layout-redesign/layout-contract.md)，真实运行与未测边界见[实施记录](../../.scratch/agent-layout-redesign/implementation.md)。

使用默认 API Key 模式时，可以先启动 Agent，在 `/provider` 中选择连接；没有凭据时会自动要求输入并保存 Key。输入内容以星号遮蔽，不进入对话或输入历史。Key 保存到 Agent 状态目录的 `.env`（文件权限 `0600`），保存后立即生效，重启继续可用。也支持启动前设置 `DEEPSEEK_API_KEY` 等环境变量；同名变量以已保存的 `.env` 值优先。程序内置模型定义只保存认证变量名称，不保存凭据值。未保存模型选择时，默认模型为 DeepSeek 的 `deepseek-flash`，使用 OpenAI Chat Completions。已有内置模型选择继续保留，不自动升级到新模型。

### Provider 请求失败排查

错误显示实际连接名、HTTP 状态、服务原文及分类提示；安全的 HTTP(S) 链接可在支持 OSC 8 的终端中点击，普通终端保留完整地址。错误说明会保存到 Transcript，重新打开仍可查看。失败记录不充当模型回复，不加入下一次模型请求；Auto 审批服务失败时，模型收到的工具失败说明同样经过脱敏。

- **使用限制／余额不足**：检查服务返回的额度、订阅或充值入口，或用 `/provider` 切换连接；不会自动重试或删除登录凭据。
- **认证失效**：在该连接 Details 中重新登录或替换 Key。HTTP 403 本身不足以判断未登录。
- **限流／服务故障**：限流显示 `Retry-After`；可取消等待，稍后重试或切换连接。服务明确禁止重试时不重试，已有输出时不重放请求。
- **模型／套餐权限**：用 `/model` 选择其他模型，或检查套餐、账号与组织权限。

Kimi Code 的 `kimi-for-coding` 是各套餐的默认入口。目录列出模型只表示服务认识这个模型，不证明账号拥有它。`access_terminated_error` 的 monthly usage limit 可能是使用限制，也可能是当前套餐不含所选模型；先尝试 `kimi-for-coding`，再检查服务提供的订阅链接。不会根据该含糊错误断定额度耗尽或隐藏模型，也不按套餐价格推断权益。连接 Details 和模型页在见到此错误后提示其他模型可能需要更高套餐。

本进程的状态行及 Details 保留每条连接最近一次 HTTP 失败，直到该连接后续模型请求成功；刷新模型目录、重新打开菜单、切换模型或登录不会清除诊断记录。

也可使用自己的 Codex 套餐：在 Agent 输入 `/provider`，选择 OpenAI 分组下的 `Codex subscription (chatgpt.com)` 连接；未登录时显示登录方式菜单。选择 Browser sign-in 打开官方授权页面，选择 Device code sign-in 显示地址和设备码，适合远程终端。无需额外脚本或 Codex CLI。

自动打开的是程序所在机器的浏览器；若没有出现可访问的页面，在自己的设备上打开显示的地址。浏览器无法返回终端时，可在登录界面的 callback 字段粘贴包含 state 的完整重定向 URL；按 Tab 编辑，Enter 提交，Esc 先退出编辑、再取消登录。密码与账号验证只在 OpenAI 官方页面完成，Tick Loop 输入框不收集密码。

如果设备码页面提示启用设备代码授权，需要本人在 ChatGPT 的 Settings → Security 中开启后，在连接登录方式菜单中重试 Device code sign-in，或选择 Browser sign-in。这个账号设置由 OpenAI 管理。官方说明见 [Codex 身份验证](https://developers.openai.com/codex/auth#login-on-headless-devices)。

`/provider` 选择连接：已有可用凭据时回车进入模型页，缺少凭据时先认证，成功后继续模型选择；保存凭据不自动应用模型，明确选择才生效。按 `p` 打开当前连接的 Details，通过明确的 Replace API key／Sign in again 操作更换凭据；取消或失败保留旧值。Remove saved key／Sign out 需要确认，保留外部 shell 环境，界面说明环境变量回退。`f` 切换 All／Configured；模型页仅在支持账号发现时提供 `r` Refresh。`/model` 在已配置 Provider → Model 两级中选择模型，已有选择时直接进入当前 Provider，并查询或复用同连接账号模型目录。明确无权限的模型隐藏，查询失败或无发现接口时保留未验证候选。Herdr 内外使用同一套 Agent 命令。

同一 `TICKLOOP_CONFIG_DIR` 的 Chat 共用登录，后续模型请求读取最新账户并在到期前刷新。Codex 连接 Details 中的 `Sign out…`（确认后）仅删除本地登录，不撤销已发出的请求或远端授权。凭证单独保存在状态根目录的 `auth/openai-codex-credentials.json`，使用私有文件权限；`auth/openai-codex-generation.json` 是私有的登录代次标记，阻止晚到登录覆盖后续登录／退出，不含 token。不读取 Codex CLI 或 pi 的登录文件。不要分享这份文件；同一系统用户下已经获准执行的任意 Shell 仍可能访问本地文件。

Kimi 中国区使用 `MOONSHOT_CN_API_KEY`，国际区使用 `MOONSHOT_API_KEY`；两个服务分别验证 key，不自动切换地区。百炼使用 `DASHSCOPE_API_KEY` 和 `https://dashscope.aliyuncs.com/compatible-mode/v1/`；Anthropic 使用 `ANTHROPIC_API_KEY`，Gemini Developer API 使用 `GEMINI_API_KEY`，xAI API 使用 `XAI_API_KEY`。Kimi Code 和 Grok 的 OAuth 连接在 `/provider` 中使用设备授权，并在 consent 说明实际注册客户端名称。Grok OAuth 使用官方订阅代理 `cli-chat-proxy.grok.com`，独立于按量 API。

Vertex service account 在 `/provider` → Google → Vertex service account 按 Enter，输入本机私有 `service_account` JSON 文件的绝对路径。已有路径会预填，支持普通文本编辑与 `Ctrl+G` 外部编辑器；Esc 先退出输入、再取消。程序验证文件权限（`0600`，不含符号链接）、格式和签名密钥后保存路径，失败则留在输入框且不修改已有配置。保存后立即进入该连接的模型页，明确选择后才应用；JSON 文件留在原处，不复制其内容。

脚本和非交互启动仍可通过 `GOOGLE_APPLICATION_CREDENTIALS` 设置文件路径，或写入 Agent 状态根的私有 `.env`。`GOOGLE_CLOUD_PROJECT` 可覆盖文件中的资源项目，`GOOGLE_CLOUD_LOCATION` 默认 `global`，选择其他位置前核对模型在该位置的支持。程序自行签名 JWT、交换及刷新短期 token。该文件路径及 key 不写入模型 YAML。

Google 用户 OAuth 使用自己的 installed-app client：在环境或私有 `.env` 设置 `GOOGLE_OAUTH_CLIENT_ID`、`GOOGLE_OAUTH_CLIENT_SECRET` 和 `GOOGLE_CLOUD_PROJECT`，再进入 Google 的 OAuth 连接授权。它使用 PKCE/state 和官方 Google token 服务；远程终端可粘贴完整 callback URL。Gemini Developer API 和 Vertex 使用独立 Provider 身份及资源路径，共享本程序的 Google 用户登录。也提供 `GoogleAuth::import_authorized_user` 显式导入私有 ADC 用户文件的 library 接口；不会自动读取其他程序的凭据。退出登录仅清除本地 token；服务账号文件由用户管理。

百炼当前 Qwen OAuth 免费登录已退役；Claude 订阅 OAuth 和 Gemini CLI Code Assist OAuth 没有开放给第三方客户端的使用授权，因此不提供这些登录入口。实际支持的认证路线、官方来源与未测账户见 [接入验证记录](../../.scratch/provider-expansion/verification.md)。

`/settings` → Usage 显示服务端实际返回的 token 用量，包括审查和压缩请求；这些数字不是套餐剩余额度或实际账单。账号失效时在 `/provider` 选中连接并按 `r` 重新授权；模型没有权限时在 `/model` 中自行选择可用模型；套餐额度耗尽会结束本次任务，并在服务端提供时显示重置时间。不会自动换号、切换到付费 API 或等额度重置后自行继续。

本地协议、终端和恢复验证与真实套餐验收分别记录在[验证记录](../../.scratch/codex-subscription/verification.md)。真实服务未验证的能力仍以该记录中的未验收项为准。


### OpenAI API 与账号模型目录

`/provider` 的 OpenAI 厂商入口有两条独立连接：`Pay-as-you-go API (api.openai.com)` 与 `Codex subscription (chatgpt.com)`。按量 API 使用 `OPENAI_API_KEY`，可在菜单遮蔽输入并保存到 Agent 私有 `.env`；订阅使用独立 OAuth 凭据及套餐额度，两者不会自动回退或共用凭据。API 的主协议为 OpenAI Responses，推荐 Astra (6)、Sol (6.1)、Luna (6)，具体卡和请求分别是 `gpt-6-astra`、`gpt-6.1-sol`、`gpt-6-luna`；API 卡预算为 1,050,000 上下文与 128,000 单次输出上限。Astra/Sol 不能关闭 Thinking，Luna 可以。

API 查询 `/v1/models`，Codex 订阅查询其自己的 `/backend-api/codex/models`，不交叉使用目录。“Account directory checked.” 只代表同连接目录检查成功，不代表工具、图片、额度或所有权限实测通过。缺席/失败/未提供接口均标未验证，不静默降级或换计费；明确拒绝的模型不会出现在选择列表。启动时异步检查当前已配置 Provider；其他连接在首次进入时检查，返回 Provider 层不查询其他账号。Esc 取消且保留登录，模型页可强制刷新当前 Provider。认证失效、模型权限拒绝或额度耗尽的请求报错会使对应缓存失效，下次进入再查；目录在当前进程会话内缓存并按连接、实际 origin/路由、账号隔离，登录/新 Key/注销失效，正常 token 刷新保留身份。DeepSeek/MiniMax/GLM 现有路线未提供发现，使用静态推荐及统一账号未检查说明；Copilot 独立接入通过通用发现 Seam 实施，其账户证据由对应工单记录。

本次假 HTTP、配置/菜单与运行检查见[验证记录](../../.scratch/provider-hierarchy/verification.md)。无 OpenAI API Key，真实 API/Codex 发现、额度与服务端缓存未测。本人有 Key 时可选运行 `cargo run --locked -p tickloop-agent --example openai_probe -- STATE_DIRECTORY [MODEL_CARD_ID] [image]`；探针只调用无副作用 Echo 与结构化 fixture，Key 从该私有目录 `.env` 或 `OPENAI_API_KEY` 读取，输出不含 Key。

### MiniMax

`/provider` 一级列表显示单个 `MiniMax`，进入连接页后先选两区 `API` 或 `Portal` 产品连接，连接名显示 China / International 和实际服务 host，四个 Provider 分别认证。API Key 使用 `MINIMAX_CN_API_KEY` 或 `MINIMAX_GLOBAL_API_KEY`，也可以在菜单遮蔽输入并保存；账户和 Key 必须对应站点。

OAuth 使用 Portal 设备授权，终端显示地址和用户码；在自己的浏览器完成授权，无需远程机器的回调。采用官方 OpenClaw 指南的 Portal 路线，同意页客户端显示为 OpenClaw，不是独立登记的 Tick Loop 客户端。登录文件分别为状态根 `auth/minimax-cn.json` 和 `auth/minimax-global.json`；访问 token 到期前刷新，刷新保持历史来源身份，重新登录产生新身份。连接 Details 中可明确选择 Sign out 清除对应地区登录，或 Sign in again 重新授权；重新授权取消／失败保留旧登录；不宣称撤销远端授权。

内置范围为 M3 及以上。M3 支持文字和图片，1M 上下文，默认单次输出上限 131,072 tokens；默认关闭 Thinking，开启使用 adaptive，菜单提供 on/off。OAuth 目录另提供 M3.1 Flash Preview，强制 Thinking，支持 low/medium/high/xhigh/max、默认 max。预览模型暂限有相应权益的 M Plan / MiniMax Code；OAuth 登录成功不代表该账户具有模型权限。视频不在 Tick Loop 当前输入范围。

使用 MiniMax 专用 Messages Adapter；API 中国站地址为 `https://api.minimax.cn/anthropic`，中国 OAuth 为 `https://api.minimaxi.com/anthropic`，国际站为 `https://api.minimax.io/anthropic`。认证、模型能力和端点依据见[接入规格](../../.scratch/minimax-integration/spec.md)，本地与真实账户证据见[验证记录](../../.scratch/minimax-integration/verification.md)。

### 智谱 GLM

`/provider` 一级列表显示单个 `Zhipu GLM`。进入连接页后，按站点和计费方式选择四个独立 Provider，Key 分开保存，可在菜单中遮蔽输入：

| 连接 | 地址 | Key 变量 |
| --- | --- | --- |
| `Pay-as-you-go API · China (open.bigmodel.cn)` | `https://open.bigmodel.cn/api/paas/v4` | `ZHIPU_API_KEY` |
| `Pay-as-you-go API · International (api.z.ai)` | `https://api.z.ai/api/paas/v4` | `ZAI_API_KEY` |
| `Coding Plan · China (open.bigmodel.cn)` | `https://open.bigmodel.cn/api/coding/paas/v4` | `ZHIPU_CODING_API_KEY` |
| `Coding Plan · International (api.z.ai)` | `https://api.z.ai/api/coding/paas/v4` | `ZAI_CODING_API_KEY` |

按量连接在账户没有余额或资源包时返回业务码 1113；只订阅了 GLM Coding Plan 的账户在按量连接上也会报这个码，需要改用 Coding Plan 连接。两种计费方式之间不自动探测、不回退。官方条款把 Coding Plan 额度限定在指定工具内，Tick Loop 不在名单，可能被限流或封禁；Coding Plan 模型页会显示这条提示，是否使用由用户自行决定。Coding Plan 只提供 GLM 与 GLM Flash 两个系列，FlashX 不在套餐内。智谱不提供第三方 OAuth，个人版和团队版套餐都用 API Key 认证，团队套餐 Key 与平台其他 Key 不通用。

模型定义与当前接入分开维护。Provider YAML 中的 Model Card 描述具体版本 GLM-5.3、GLM-5.3-Flash、GLM-5.3-FlashX 与 GLM-5.2。当前接入目录 [src/model/access/](src/model/access/) 为每个 Provider 声明系列及推荐卡：`GLM → glm-5.3`、`GLM Flash → glm-5.3-flash`、`GLM FlashX → glm-5.3-flashx`。模型页由这份绑定生成 `GLM-5.3`、`GLM-5.3-Flash`、`GLM-5.3-FlashX` 三个入口；选中后保存、请求和历史记录都使用具体卡，状态栏显示实际版本。三个推荐版本均为 1M 上下文、默认单次输出上限 131,072 tokens、强制 Thinking，档位为 low/high/max（`auto` 即服务端默认 max）；Flash 与 FlashX 接收文字和图片，基础 GLM 只收文字，视频与文件不在当前输入范围。GLM-5.2 卡继续保留供已保存选择和历史使用，可开关 Thinking，档位 high/max，但不再出现在普通菜单。旧保存选择中的 `glm-5.2` 和兼容别名 `glm-5.2-chat-completions` 仍按内置目录解析；不再通过用户 Provider YAML 固定或覆盖卡。新版本发布后，先新增具体卡，再更新系列绑定；已有已知内置选择不会被目录更新静默切换。

四个连接都使用 GLM 专用 OpenAI Chat Completions Adapter。请求开启 Preserved Thinking（`clear_thinking: false`），同一来源的 `reasoning_content` 原样回放以保持缓存前缀；有工具时开启流式工具参数。结构化审查使用官方 JSON mode 并由宿主严格校验。余额不足、额度耗尽和套餐/模型权限错误不重试；限流与过载按现有预算重试。官方依据与参考实现见[接入规格](../../.scratch/glm-integration/spec.md)，证据与未验收项见[验证记录](../../.scratch/glm-integration/verification.md)；真实账户可用 `cargo run -p tickloop-agent --example glm_probe -- cn|global|cn-coding|global-coding STATE_DIRECTORY` 自行验证，只调用无副作用的 Echo 与结构化 fixture。

同一 Chat 可在 Messages、Chat Completions、Responses 模型间切换。原始历史逐条保留实际生成来源，每次按目标模型生成请求副本：文字、工具调用和结果继续有效；异来源可读 Thinking 转普通历史文本，签名/加密等私有状态只重放给兼容来源。切回原模型可以恢复仍保留的原始状态；重启与 rewind 保持来源。运行中或等待工具时切换延后生效，pending/OutcomeUnknown 不因切换重发工具。相同来源保留签名、块顺序、合法调用 ID 与稳定缓存前缀；实际命中以服务端 usage 为准。

仓库内模型接入 SOP 位于 [.tickloop/skills/model-integration](../../.tickloop/skills/model-integration/SKILL.md)，在本项目通过 `/model-integration` 使用。

### 内置 Provider 与模型

目前只支持随程序编译的 Provider/model 目录，动态 Provider 接入延期。支持的厂商、连接、地区、模型和所有定义参数均随程序版本变化，目录源文件位于 [src/model/providers/](src/model/providers/)，通过 `include_str!` 编译到程序中。

使用 `tickloop --cwd "$PWD"` 启动；`/provider` 和 `/model` 选择内置连接与模型。模型与 Provider 定义由程序版本提供，不接受外部配置文件，也不读取状态目录 `providers/*.yaml`、旧 `settings.json.providers` 或其他外部 Provider/model 定义。单次输出上限、上下文窗口、端点、认证变量和 Model Card 能力参数均采用内置版本值；外部文件不能覆盖它们。

模型选择和 Thinking 保存在状态目录 `settings.json` 的 `defaultModel`、`effortLevel` 和 `alwaysThinkingEnabled`。旧的内置选择与兼容别名继续使用当前版本内置定义；引用未知外部身份的旧选择给出一条明确提示并回退到内置默认。旧定义/调优文件被忽略，用户文件不会被删除。

API Key 仍读取状态目录 `.env`，OAuth 登录使用私有认证存储，Google service account 等凭据文件继续支持。凭据不参与 Provider/model 定义，不在厂商、地区、产品或计费连接之间自动复制或别名匹配。开发入口 `make dev` 在共享 target 中增量构建，再用每次保留的唯一固定副本启动 CLI／owner／worker，沿用调用方 cwd 与全局配置（`TICKLOOP_CONFIG_DIR` 或默认状态目录），不另建开发配置。

内置目录与 AccessCatalog 控制普通菜单的模型系列和推荐卡；既有已退休内置选择保持兼容，不因目录更新静默升级。新增支持通过源码/SOP、验证和程序发布提供，外部 Provider 示例不构成产品加载入口。

排查网络失败时，可为启动命令设置 `TICKLOOP_NETWORK_DIAGNOSTICS=1`，将标准错误重定向到本地日志。该模式记录失败阶段、目标的协议/主机/端口、代理来源、HTTP 状态和耗时，不记录 URL 路径、查询参数、认证头或请求正文；默认不输出这些诊断行。

输入自然语言开始工作，再输入补充要求继续同一目标。输入区固定在终端底部，上方显示流式回复和工具摘要。单行状态栏依次显示当前 Execution Mode、模型及 `Context x% used`，重要提示优先显示；窗口或用量未知，以及显示百分比舍入为 0% 时，省略整个 Context 片段与相邻分隔符；有确认的非零百分比后再显示，请求进行中沿用最近一次已确认的非零用量。当前 Loop 的 Todo 始终显示在消息流左栏的 Working 下方；空闲时仍显示非空列表，全部完成后保留 5 秒再持久化清空。同根 Agents 使用默认展开的底栏面板，入口聚焦后 Enter／Space 切换。当前 Chat 打开或明确加入的 Tick、未结束 Loop 订阅的 Tick、产物与后台 Shell 在内容区右侧的 Monitor 中按实例去重呈现，每页至多 128 行，`Monitor N` 计全集；列表内 `j`／`k` 导航（越过页尾或页首翻页），Enter 打开目标，Shell 在详情中停止；`x` 只从本 Chat 的 Monitor 移除该行并持久记住（运行中的 Shell 需先停止），`a` 从当前 Herdr Workspace 或已移除列表明确加入。PgUp／PgDn 回看半页，Ctrl+Home／End 到会话首尾；上滚后新输出不会抢走阅读位置，底栏显示新消息数。`Shift+Tab` 在普通输入界面按 Auto → Ask → Full Access → Auto 循环；权限 Diff、菜单和候选打开时由当前界面优先处理。`/settings` 中的 Auto-scroll 开关只作用于当前终端客户端，关闭后手动到底也不会被之后的输出重新带走。

模式、模型与 Context 固定在输入框下分隔线右侧；窄屏、短屏与自定义 `statusLines` 保持独立状态行回退。旧 `statusLinePosition` 静默忽略并作为未知键保留。左侧 `Subagents N` 仅在有子 Agent 时出现，N 只统计子 Agent，不计 Main（当前查看的子 Agent 仍计入）；默认展开，点击或 ↓ 聚焦入口后的 Enter／Space 切换，隐藏状态仅属于本 View 会话。展开后 ↓ 进入列表，Enter 打开 Agent，Space 折叠后代，Esc 返回。Todo 始终显示在当前 Agent 消息流左栏的 Working 下方，空闲时也保留非空列表；没有展开或快捷键开关，全部完成后保留 5 秒再持久化清空列表；Shell 保持在右侧 Monitor。子 Agent 不再产生重复的 `reply completed` 状态行，完成状态与回复仍可在列表和该 Agent 对话查看。

输入 `/model` 在同一列表中选择全部已登录 Provider 的模型，Provider 是不可选分组标题；Settings → Model 打开同一选择器。未登录 Provider 经 `/provider` 或 Manage Providers 配置；`/provider` 的 All/Configured 目录用 `f` 切换过滤，并保留厂商 configured/total 状态。普通 Enter 复用该实际连接的凭据。单连接模型页标题显示厂商及实际产品、地区、服务域名；模型行使用完整 Model Card 名称；按固定厂商顺序、产品/地区连接顺序，再按模型系列（OpenAI 在 Claude 前）和接入目录维护的能力顺序排列。能力顺序不改变默认推荐。光标所在行用整行背景高亮，长列表跟随光标并提示上下方还有内容；当前模型用 `●` 标记，打开时定位到它，但不移到第一项。交互式 Settings 及其子选择器、Provider、Model、Details 和凭据操作页不显示序号，也不接受数字直接确认；管道模式保留编号文本选择作为非交互回退。上下键、`j`／`k`、`Ctrl+N`／`Ctrl+P` 导航，左右键或 `h`／`l` 调整 Thinking，回车原子应用 Provider、模型与 Thinking 并保留模型列表。直接 `/model` 按一次 `Esc` 关闭；Settings → Model 的 `Esc` 返回 Settings；`/provider` → 模型的 `Esc` 返回目录，再按 `Esc` 关闭。目录或模型页按 `p` 打开该实际连接的 Details；Replace API key、Edit service account、Sign in again 才开启编辑，取消／失败保留旧凭据。替换 key 保存成功前旧 key 仍可用；Remove saved key 与 Sign out 单独确认。模型页 `r` 仅刷新支持的账号目录。返回栈恢复发起页、过滤、模型／操作行焦点与 Thinking 草稿；空模型页通过 Manage Providers 进入 All。发现结果保留页面与焦点；登录完成留在原授权页并保留 callback 字段，用户按 Enter／Esc 返回一层。连接来源与模型参数采用编译内置值，旧外部覆盖被忽略；不跨地区／产品共享凭据。新配置连接立即出现在 Provider 层，但不自动应用。`/thinking` 可单独调整当前模型的 Thinking 档位。每个模型绑定首选协议，普通菜单不提供独立 API 风格切换；DeepSeek 只列 OpenAI Chat Completions 的首选模型。两个模型共用 `DEEPSEEK_API_KEY`；当前内置 Model Card 的上下文窗口为 1M，输出限制为 64,000 tokens，随程序版本固定。`/settings` 集中显示模型、Execution Mode、当前会话状态、本进程用量和 Auto-scroll。Enter 打开子页或立即修改，Settings 保持打开并保留选中项；普通子页确认回 Settings，保存失败留在发起页。Esc 每次只退出字段、确认页或子页一层，最后才关闭 Settings，已保存变化保留。照 Claude Code，`/model` 的选择、持久权限规则和 `defaultMode` 位于状态根目录的 `settings.json`；启动时 `--mode` 优先于保存的默认模式。内置默认是 Auto。Chat 内切换、会话级规则和 `/add-dir` 附加目录只在本进程内有效；新建、恢复与 Branch 使用当时的默认模式，不恢复来源 Chat 的临时模式。Auto-scroll 作用于当前终端会话；在途工作结束后模型切换才生效，历史按目标模型投影，协议变化不要求先压缩。已知模型的上下文窗口自动取该模型的最大值。

内置 DeepSeek 配置的单次输出上限为 64,000 tokens；保存的旧内置 4096 配置在读取时升级，外部输出参数覆盖不再生效。模型报告输出截断时，Agent 只用已收到的文字续接，最多再请求三次；没有完整文字可续接或次数耗尽时报告失败。上下文自动压缩独立按输入历史与上下文窗口判断。

`/settings` 的 Thinking 项独立开启或关闭思考，保存为 `settings.json` 的 `alwaysThinkingEnabled`；仅当前模型的 model card 允许关闭时可切换。`/thinking` 打开按 model card 过滤的档位选择器，也可输入 `/thinking LEVEL`。DeepSeek 的力度档位是 `low`、`high`、`max`；`auto` 恢复模型默认值。Claude 档位按具体模型变化：Opus 4.6 为四档，Opus 5 为五档。选择结果保存为 `effortLevel`，应用于当前 Chat 的后续请求和新 Chat。当前模型不支持的档位不会出现，直接输入也会被拒绝。交互对话在思考期间逐字显示一行实时预览；思考文本换行时预览切到最新一行。运行中的 `Working`、Thinking、工具名与展开任务列表的 `running` 状态统一使用从左向右的文字流光，减少动效时静止显示。普通回复按完整行显示，最后一行没有换行时在回复结束时显示。思考结束后正文仍折叠为 `∴ Thinking (ctrl+o to expand)`；Ctrl+O 在运行中显示已收到的完整思考正文，完成后显示模型提供的可读思考文本。Responses 同时返回摘要和原文时优先显示摘要，并明确标为 `Thinking summary`；加密状态不显示。管道输出直接输出模型返回的思考文本。模型未返回思考内容时不会伪造思考。

模型调用遇到连接中断、超时、HTTP 408／409／429 或 5xx 时，按 Claude Code 的 500ms 起步、指数退避、32 秒基础间隔上限与随机抖动重试，普通对话与摘要最多再试十次；HTTP 529 最多总计三次。Auto 审批使用与普通对话分开的请求和结果校验；Jev 的总预算、重试与 LLM 回退见下文。服务返回 `Retry-After` 时优先按指定秒数等待，`x-should-retry: false` 或明确的额度耗尽错误停止重试。每次调用独立记录用量和结果，等待期间可以打断；Chat 显示错误类别、重试次数和等待时间。已显示文字的失败响应不直接重发，以免重复输出。认证、余额不足、普通请求格式错误、上下文超限和无效响应直接报告失败。

`/settings` 的 Output style 选择回复的表达方式：标准（不附加提示词）、简洁或详尽。照 Claude Code `outputStyle`，选择写入 `settings.json`，之后打开的 Chat（新建、恢复或分支）使用它，已打开的 Chat 保持原风格。助手回复按 Markdown 呈现：标题、强调、行内代码、列表、引用与表格去掉标记后带样式显示，续行悬挂在 `⏺` 之后；非交互输出保留 Markdown 原文。

Settings → Usage 统计本进程启动以来当前 Chat 的模型调用，按调用时的模型配置和协议分组，分别显示对话（含工具后续轮）、压缩和权限检查的调用数量、结果及服务报告的输入、输出、缓存用量。失败或取消前收到的用量也会保留，压缩不会清空记录；照 Claude Code，用量只随 assistant 消息写入 Transcript，重启后不重新汇总，恢复的 Chat 从零开始统计。未报告的字段显示 `not reported`，只有部分调用报告时显示 `reported subtotal` 和覆盖数量。这些计数不代表费用、账单或账户额度。页面支持 PgUp／PgDn 阅读，返回再打开可查看最新记录。

内置工具的名称、参数与返回给模型的文本照 Claude Code：`Read`、`Write`、`Edit`、`Glob`、`Grep`、`Bash`、`TaskStop`、`WebFetch`、`WebSearch`、`AskUserQuestion`、`EnterPlanMode`、`ExitPlanMode`、`TodoWrite` 与 `Skill`；`Bash` 另接受可选的 `cwd`，`WebSearch` 另接受可选的 `limit`。后台命令的输出文件用 `Read` 读取，`TaskStop` 只停止本目标的后台命令。`StartGoal` 登记独立的新目标；在工作台 Chat 中另有 `ReadWorkspace`、`ReadTick`、`CallTickTool`、`ModifyTick`、`SubscribeTick` 与 `UnsubscribeTick`。产物业务操作使用安装的 Artifact MCP 工具；ReadTick／CallTickTool 仅服务 Herdr 实例，Shell 使用原生 Bash／Read 输出文件／TaskStop。各类型通过 SubscribeTick 共用更新→Loop 触发规则。权限规则按这些工具名书写，例如 `Bash(npm test)`、`WebFetch(domain:example.com)`；旧 Transcript、规则与设置中的旧工具名不做迁移。

一般权限请求在下方显示完整操作与四项选择：`Yes`、`Yes, for this session`、`Yes, and don't ask again for: Bash(<完整命令>)`、`No, and tell Claude what to do differently (esc)`。命中显式 Ask 规则时只提供单次允许和拒绝，不能保存会话或持久放行。方向键选择，回车确认，Esc 拒绝当前操作；一般请求的第三项直接写明 Enter 会保存的精确 Shell 规则，按 Tab 会打开完整规则选择器：第一项仍是精确命令，随后按 shell token 边界列出由窄到宽的命令前缀规则。带引号的参数视为一个 token；管道、`;`、`&&` 等复合命令不生成宽泛前缀。长规则按终端宽度换行，菜单高度保持受限。单次与会话授权保持精确操作及目标范围；持久授权写入 `settings.json` 中当前工作目录的分区，并立即同步到当前 Session，对该目录的所有 Chat 和目标生效，不影响其他目录。权限卡片显示人工审批的原因与来源；宿主强制限制不能由保存规则或 Auto 绕过，Auto reviewer 请求人工判断时显示具体原因。多个目标的请求显示所属目标。拒绝后本轮可以补充输入继续。

工具默认显示动作与少量真实结果，`Ctrl+O` 或 `/details` 打开独立的完整会话记录，包含公开对话、工具参数、真实结果及完整输出引用。文件修改在批准前展示与实际待执行操作绑定的 diff；`Shift+Tab` 聚焦 diff 后可用 `j/k`、PgUp／PgDn、Home／End 浏览，Tab 返回决策区。创建、覆盖、编辑和全量替换分别标记；每条改变的文本行都可到达，文件在批准后发生变化时执行会因 identity 冲突而失败。

在会话记录中，`/` 搜索，Enter 接受，`n`／`N` 跳到下一个／上一个匹配；Esc 取消搜索并恢复原位置。上下或 `j/k` 逐行，PgUp／PgDn 半页，Space／`b` 整页，Home／End 或 `g/G` 到首尾，`{`／`}` 跳到用户消息；End 恢复跟随新输出。`?` 查看完整键位。Ctrl+O、Esc、q 或 Ctrl+C 返回对话，保留草稿和待答菜单，不将记录重复追加到聊天，也不批准工具。

照 Claude Code，按 Esc 或 `/interrupt` 中断时，已经流出的文字保留为一条 assistant 消息，其后记一条 `[Request interrupted by user]`；中断发生在工具执行或等待授权时，未完成的工具调用补上错误结果，并改记 `[Request interrupted by user for tool use]`。这些消息与普通对话一起发给下一次模型请求并随 Chat 保存，对话中显示为 `⎿ Interrupted · What should the Agent do instead?`。断流或请求失败时已显示的文字不写入对话。

输入 `/rewind`，或在空输入时连按两次 `Esc`，可以从当前对话中选择一条用户提示；排队但未执行的输入、定时与事件触发的输入、工具结果、权限附言和内部总结不列为可选位置。选择器与动作菜单支持 `j/k` 与 `Ctrl+N/P` 导航，返回时保留草稿。

照 Claude Code，Rewind 提供 Restore code and conversation、Restore conversation 和 Restore code；只有恢复代码确实会改变文件时才提供含代码的两项。恢复对话只截断内存中的对话：选中的提示及之后的消息离开对话，提示当时的 Execution Mode 被读回，提示原文与附件回填到底部输入，并绑定到新建的无 trigger Idle Loop。Loop 随对话链生效：在选中提示或之后创建的 Loop 离开当前对话，它的 Trigger 与后台 Shell 随之停止；更早创建的 Loop 保持原生命周期。下一条消息接在最后保留的消息之后，在同一 Transcript 中形成分叉，被放弃的消息仍留在文件中；不继续对话就重启时，恢复仍进入最近活动的一支。

恢复代码使用 Claude Code 的文件回滚备份：Agent 的文件工具修改文件之前，为尚未跟踪的文件在 `<root>/file-history/<chatId>/` 建首版备份（文件名为路径 SHA-256 的前 16 位加版本号，保留权限位；文件不存在时记为空）；每条用户提示为跟踪中的文件建快照，未变化复用上一版、变化则版本加一、已删除记为空，最多保留最近 100 个快照，快照作为 `file-history-snapshot` 条目写入 Transcript，恢复 Chat 时沿对话链重建。Restore code 取选中提示的快照：该版本不存在的文件被删除，快照中没有的文件使用首版备份，内容不同才复制回去；单个文件失败时跳过并在结果中列出，快照已被淘汰时不改动任何文件。恢复不跟随符号链接：被跟踪的文件或其父目录已被替换为符号链接、或不再是普通文件时记为失败并跳过，快照也不备份链接指向的内容；恢复时重建的缺失目录按 umask 创建（只有备份目录为 0700）。Shell 命令和其他外部副作用不在覆盖范围内，也不协调多个进程同时修改同一文件。`--fork-session` 与 `/branch` 得到的新 Chat 把备份硬链接（失败时复制）到自己的目录，仍能恢复分出之前的文件状态。

Rewind 同时提供 `Summarize from here` 和 `Summarize up to here`，照 Claude Code 的 partial compact 实现。前者把选中 prompt 及之后的消息替换为摘要，对话变为"压缩边界、选点之前的消息、摘要"，并把选中 prompt 与附件恢复到输入框；后者把选点之前的消息替换为摘要，对话变为"压缩边界、摘要、选中 prompt 及之后的消息"（其中旧的边界与摘要被去掉），并保持空输入。保留的消息内容不变，但在 Transcript 中作为新条目按对话顺序直线重写在新边界之后（换用新的消息 id，文件回滚快照随提示以新 id 重新记录，保留消息中工具结果的内容替换决定也在新边界之后重新记录，其中 assistant 消息的用量清零）；边界不带保留区段，摘要消息记录被摘要的消息数、补充说明和方向。被摘要的旧条目留在文件中，成为被放弃的分支。选点必须在最后一个压缩边界之后。两者都使用当前 Chat 的模型和 adapter，摘要请求不带工具，以 Chat 级用量记录，不创建或重绑 Loop。高亮 summary action 后按 Tab 可输入 `add context (optional)`；按当前显示的数字会立即执行且不带 guidance。运行中 Ctrl+C 取消；取消、失败、超时或被拒绝的摘要不改变对话，也不写入 Transcript，进程在摘要完成前退出时重启后如同没有发起过。中断标记、`!` 命令和任务通知都是普通消息，按所在位置参与摘要。

输入 `/branch` 或 `/branch NAME` 从当前公开会话末尾创建独立 child Chat，并立即切换。child 继承当前模型、adapter 和目录配置，并复制分出点之前创建的 Loop（照重启恢复规则：运行或等待中的转为已中断，Trigger 全部解除武装，向该 Loop 提交输入时重新武装），但不复制 Run、usage、operation、pending interaction 或 live Shell owner；复制对话中的用户提示仍可作为 Rewind 位置，文件回滚备份随之复制；之后不指定目标的普通输入创建新 Loop。请求带有 `source_loop_id` 且存在精确 process-session owner 映射时，已有 session allowance 才会继续有效，重启后消失。后台任务仍只属于 source：分支之后的输出与终态只出现在 source，child 只保留分支时刻的对话；source 保留任务 owner、输出日志、usage、Task 视图与 Ctrl+O detail。其他 busy 状态会无副作用拒绝。`/fork` 不是 `/branch` 的别名；`--fork-session` 是恢复参数，见上文。

`/diff` 打开本地变更总览。`Current` 实时显示 Git working tree，逐轮 `Agent` source 只显示本进程运行期间已经成功执行并保存的文件操作，重启后不再列出；Left／Right 切换 source，`j/k` 或 Up／Down 选择文件，Enter 打开详情，Esc 或 Left 返回。该视图不调用模型、不修改文件，也不执行 Git 写操作；review artifact 缺失或损坏只影响对应条目。

文本输入只有一种普通编辑模式（照 Claude Code 普通模式，不提供 Vim 模式，见 [ADR 0010](../../docs/adr/0010-no-vim-mode-in-text-input.md)），底栏不显示编辑模式标签。普通编辑采用 Readline emacs 风格：`Ctrl+A/E` 行首尾，`Ctrl+B/F` 按字符移动，`Alt+B/F`、`Ctrl／Alt+←/→` 按词移动，`Ctrl+P/N` 视觉行上下，`Ctrl+H`／Backspace 与 `Ctrl+D`／Delete 删除字符，`Ctrl+K/U/W`、`Alt+D`、`Alt／Ctrl+Backspace`、`Alt+Delete` 删除并进入 kill ring，`Ctrl+Y` yank、`Alt+Y` 轮换，`Ctrl+_`（含终端把 0x1F 报成的 `Ctrl+7`）或 `Ctrl+Shift+-` 撤销，`Ctrl+S` 暂存完整未发送输入，空输入时再次按下恢复文字、光标、图片、折叠粘贴、文件提及与 Shell 输入模式；暂存属于当前 View 的逐 Agent 缓存，退出界面后不保留；照 Claude Code `useInputBuffer`，1 秒内连续输入合并为一次撤销，粘贴与其他编辑各自成组，撤销记录最多保留 50 条；`Ctrl+J`、`Ctrl+S`、`Ctrl+O` 沿用 Claude Code 含义而非 Readline；到达输入边界后才浏览历史，Down 恢复未提交草稿。

输入区显示真实多行并向上扩展，保留中文、制表符和换行；长行按终端列宽折行，resize 后围绕同一光标位置重排。Ctrl+J 或终端能识别的 Alt／Shift+Enter 插入换行。输入 `/` 或命令前缀会在输入区上方显示实际命令候选；输入 `@` 与部分路径会异步显示当前工作目录及附加目录中的文件／目录。Tab 填入首项，方向键或 `Ctrl+N/P` 选择后 Enter 只填入，未选择时 Enter 仍提交草稿。只有经候选接受并在提交时再次验证的 `@` 路径会作为独立、不受信任的模型上下文读取；普通 `@text` 仍是普通文字。聚焦的候选和菜单支持 `j/k` 与 `Ctrl+N/P`。异步菜单保留草稿并阻止补全按键泄漏；Tab 在输入区与菜单间切换。Ctrl+C 优先清空草稿或中断当前运行；空输入时 Ctrl+C／Ctrl+D 需要在 800ms 内再次按下才退出，非空输入的 Ctrl+D 只向前删除字符。输入框中的 Esc 先关闭候选或弹层，其后按中断、清空草稿或 Rewind 逐层处理；菜单中的 Esc 只取消或拒绝当前菜单。

`Ctrl+R` 打开独立历史搜索。`Ctrl+S` 在当前 session、当前项目和全部项目间切换范围，方向键或 `Ctrl+N/P` 选择，Enter／Tab 只把结果放回输入框，Esc 恢复原草稿和光标。历史保存在状态根目录的 `history.jsonl`（默认 `~/.tickloop`，可用 `TICKLOOP_CONFIG_DIR` 指定）；若持久目录不可写则退回当前进程内存，不影响 Agent 启动。

空输入开头键入或粘贴 `!` 会进入 Shell mode，根 Agent 的命令由创建它的用户环境执行，不经过模型工具审批；child 的直接 Shell 仍受冻结的委派范围限制，无法证明范围的操作明确拒绝。Tab 补全当前项目的 Shell history 和路径；空 Shell 输入用 Esc／Backspace／Ctrl+U 返回普通 Prompt。前台命令运行时 Ctrl+B 或 Ctrl+X Ctrl+B 转后台、Ctrl+C 停止，`/tasks` 可继续查看输出或控制同一进程。普通 prompt、pipe 和菜单回填中的 `!` 不会进入直接执行入口。照 Claude Code，命令执行期间不向 Chat 写消息；前台结束、转入后台或启动失败时，`<bash-input>` user 消息与结果一起加入 Chat：结束时以 `<bash-stdout>`／`<bash-stderr>` user 消息记录有界输出（失败时输出与退出码记入 stderr，被 Ctrl+C 停止时改记中断标记，启动失败记 `Command failed`），转入后台时记录后台提示，后台结束后改由任务通知说明结果。命令运行期间到达的任务通知排在这组消息之后。这些消息随 Chat 保存，对话中按 `! 命令` 与 `⎿` 输出块显示；照 Claude Code，后台命令结束时任务通知会继续启动它的 Loop，模型据此决定是否读取输出；Chat 正忙时排在当前一轮之后，多个任务同时结束只继续一轮。新 Chat 不会为 Shell 结果隐式创建 Loop，没有归属 Loop 的 `!` 命令只写通知消息，继续输入问题后模型才能解释结果。

`/tasks` 在底部固定 12 行的输入对话框中打开 Shell 任务中心，列表与详情都保留上方的 Chat 内容，模型响应期间也可立即进入。Shift+↑／Shift+↓ 聚焦右侧 Monitor 的 Shells 组，Enter 在右侧列表内打开所选 Shell 的详情，与任务中心分别显示。单任务直接打开详情，多任务先显示列表；`j/k`、方向键或 Ctrl+N/P 选择，Enter 查看，`b` 转后台，`x` 停止可控任务。详情支持逐行、翻页与 Home/End；向上浏览时保持位置并累计未读输出，End 恢复尾随。Left 返回列表，Esc／Enter／Space 关闭详情，Ctrl+O 查看完整记录。后台输出留在任务详情中；后台 Shell（User 或 Agent）结束后，所属 Chat 收到一条 `<task-notification>` user 消息，包含任务 id、Agent 任务的 tool-use id、输出文件、状态（completed／failed／killed）和摘要，对话中显示为 `● 摘要`，下一次模型请求会带上它。Shell 任务状态与输出读取依赖所属执行 worker，View 退出不丢失；worker 退出后历史不重建活任务。

底部只提供有子 Agent 时的 Subagents N 入口，N 只统计子 Agent，不计 Main（当前查看的子 Agent 仍计入），列表默认展开。输入光标在最后视觉行且历史已到底时，Down 聚焦入口；点击或在入口按 Enter／Space 隐藏或显示列表，带修饰键的 Enter 忽略。展开入口再 Down 进入列表，j/k 或 Ctrl+N/P 选择，Enter 打开 Agent，Space 折叠后代，Esc 先回入口，再回输入。隐藏列表不获得焦点或消费导航。Todo 不属于 footer，始终在 Working 下方按当前 Agent 的消息流宽度显示；空闲也保留，全部完成保留 5 秒后清空，没有展开、详情或开关。首项带树形连接，后续图标对齐；Unicode 与 Nerd Font 均提供单列的待办／进行中／完成图标。待办普通、进行中 accent 加粗、完成变暗并划除，颜色跟随当前 Theme；Working 使用当前 Loop 第一项进行中的 activeForm，保留计时与中断提示。差异视图通过独立 /diff 打开；候选、弹层和阅读界面独占各自按键。

超过 800 字符或超过 3 行的 bracketed paste 在输入区折叠为一个块，左右移动和删除把它视为一个单元，提交仍使用完整原文。输入框可编辑时 Ctrl+G 随时使用 VISUAL／EDITOR 打开外部编辑器（可以是 Vim）；返回后用编辑结果更新草稿，并恢复底部输入、raw mode 和 bracketed paste。从输入历史取回（包括重启后）的粘贴恢复为完整原文，超过 1024 字符的原文保存在状态根目录的 `paste-cache/`；取回时不恢复折叠块。Ctrl+V 把剪贴板图片附加为 `[Image #N]`，与折叠粘贴共用编号并同样整块移动和删除，删除占位符即移除图片；发送时图片按占位符顺序随消息提交，输入历史只保留占位符文字。当前模型未在 `input_modalities` 中声明 `image` 时不附加并提示；通过 SSH 运行时读取的是远程主机的剪贴板，可改用 `@路径`。把 png、jpg、jpeg、gif 或 webp 文件拖入终端或粘贴其路径（支持引号、shell 转义、`file://`、`~/`，相对路径按 Chat 工作目录解析）时同样附加为 `[Image #N]`，混合粘贴中的其余文字保持原样；路径不存在或不是图片时按文字粘贴。macOS 上剪贴板是图片时 Cmd+V 产生的空粘贴会读取剪贴板图片。

`/help` 或 `/` 打开按功能分组的命令菜单。普通补全显示 23 个公开命令和系统 Skill；`/help advanced` 查看 20 个高级命令，高级命令仍可直接执行。`/resume`、`/loops`、`/tasks` 分别选择 Chat、目标和 Shell 任务；`/plan` 在尚无计划时进入只读规划；有待批计划时重新打开批准界面，否则查看已保存计划。选择历史只打开内容，不自动恢复执行。模型调用 `ExitPlanMode` 后自动打开计划批准输入对话框，工具等待用户选择；批准并开始使用审阅的精确版本。

模型的协议、服务地址和参数由编译进程序的 Model Card 持有。DeepSeek 的主协议为 OpenAI Chat Completions；已有 Messages/Responses 内置卡继续保留独立身份和端点。用户通过 `/model` 选择版本支持的内置卡，不加载外部模型配置文件。

内置 Model Card 的 `input_modalities` 声明模型接受的输入（`"text"`、`"image"`），能力随程序版本持有。内置 Anthropic 与 Codex 模型、`deepseek-flash` 的 Messages 与 Chat Completions 条目声明 `["text", "image"]`；`deepseek-v4-pro`、`deepseek-v4-flash` 与 DeepSeek Responses 只收文字。当前模型不支持图片时，带 `@图片` 的消息不会发送，草稿保留，并提示去掉 `@图片` 或用 `/model` 切换模型；历史中的图片在请求时替换为说明文字，模型会告知用户它看不到图片，Transcript 中的原图不变，切回支持图片的模型后照常发送。Agent 用 `Read` 读取 PNG、JPEG、GIF 或 WebP 图片时，模型收到图片本身，界面只显示路径、格式与尺寸；当前模型不支持图片时 `Read` 不加载图片，返回说明文字让 Agent 告知用户。

| 分类 | 命令 | 行为 |
| --- | --- | --- |
| 公开 | `/loops new` | 下一条输入建立新的目标，保留当前 Chat 历史 |
| 公开 | `/clear` | 开始新 Chat；照 Claude Code 停止原 Chat 中未转入后台的 Shell 任务，原 Chat 的目标继续在 daemon 运行；新输入首次发送才创建独立根 Agent，child 暂不支持此命令 |
| 公开 | `/branch [NAME]` | 从当前 Chat 末尾创建并切换到独立根 Chat（不是委派 child）；标题照 Claude Code 为 "NAME (Branch)"（未指定名称时用首条提示），重名时为 "(Branch 2)" 等；`/fork` 不是别名 |
| 公开 | `/resume` | 打开 Chat 选择器：按最近活动列出本项目已保存的 Chat，显示标题或首条提示、相对时间、git 分支与 Branch 来源；打开历史不会自动执行 |
| 公开 | `/resume CHAT` | 按 id 切换前台 Chat；再次输入消息才开始运行 |
| 公开 | `/loops [GOAL_ID]` | 无参数打开 Loops；详情显示 Trigger 与定时设置，选择 Use this goal 后用于后续输入；给 ID 直接选择 |
| 高级 | `/answer QUESTION ANSWER` | 回答澄清问题；只有一个可见问题时也可直接输入答案 |
| 高级 | `/allow REQUEST [once\|session\|persistent]`、`/deny REQUEST` | 回答绑定具体操作的权限请求；默认为单次 |
| 高级 | `/mode [auto\|ask\|fullAccess\|plan]` | 查看或修改当前 Chat 的模式；旧 `acceptEdits` 兼容 Ask，`manual` 与 `default` 是废弃别名；Plan 推荐使用 `/plan` |
| 公开 | `/provider` | 选择连接，按需认证后选择模型；另可重认证、注销与配置独立 Jev Key，保存凭据不自动切换模型 |
| 公开 | `/model` | 全部已登录 Provider 的分组模型列表，当前模型原位标记；p 打开 Details，r 刷新连接；原子选择保存到 `settings.json` 并保持列表 |
| 公开 | `/thinking [LEVEL]` | 选择当前模型支持的力度；无参数打开选择器，`auto` 使用模型默认值 |
| 公开 | `/permissions` | 先选择 Auto／Ask／Full Access，或直接查看近期记录；Auto 下可配置 LLM／Jev 与 Key；Settings 中的 Permissions 打开同一入口 |
| 公开 | `/settings` | 持续打开的设置列表：Enter 修改／进入，Esc 逐层返回；模型、Thinking、Execution Mode、Permissions、状态、用量、显示与 Output style |
| 公开 | `/details`、Ctrl+O | 打开完整会话记录并浏览、搜索；Ctrl+O 返回。管道模式 `/details` 保留文本详情开关 |
| 公开 | `/rewind` | 打开用户提示选择器，恢复代码、对话或两者，或执行两向定向总结；summary 行可用 Tab 添加可选 guidance |
| 快捷键 | Esc Esc（空输入） | 打开同一 Rewind 选择器；非空草稿连按两次 Esc 清空草稿并存入历史 |
| 快捷键 | Tab／→、Enter、Esc（有提示建议时） | Tab／→ 接受为可编辑草稿；空输入 Enter 接受并提交；Esc 在关闭候选／对话框后取消建议；输入替换建议。`/settings` → Prompt suggestions 可关闭后台生成 |
| 公开 | `/diff` | 审阅当前 Git working tree 与本进程内已成功执行的逐轮 Agent 文件变更；支持 source、文件和完整 diff 导航 |
| 高级 | `/rules`、`/rule allow\|ask\|deny TOOL [session\|persistent]`、`/rule remove UUID` | 查看、增加与删除权限规则；session 规则覆盖本 Chat 所有目标中该工具的输入，本进程内有效；persistent 规则写入 `settings.json` 中当前工作目录的分区，对该目录的新 Chat 与重启后生效 |
| 公开 | `/cwd [PATH]` | 查看或修改主工作目录 |
| 高级 | `/add-dir PATH` | 增加允许访问的目录；附加目录只在本进程内有效 |
| 高级 | `/interrupt` | 打断当前运行，之后可以补充输入继续目标 |
| 公开 | `/stop [GOAL]` | 停止指定目标及其后续输入；列出仍未确认完成的操作 |
| 公开 | `/plan` | 无计划时进入只读规划；重新打开待批计划或查看保存计划；`/plan GOAL` 选择指定计划 |
| 公开 | `/tasks` | 打开本机执行目录中活 Agent 的 User／Agent Shell 任务列表与实时输出详情，其他 Chat 的任务标注所属 Chat，按来源转后台或停止；无法确认结束时显示 Unknown，重启后不保留 |
| 高级 | `/command-output OPERATION [OFFSET] [LIMIT]` | 有限读取本进程内的命令输出，并显示完整输出引用；重启后不可用 |
| 高级 | `/search-config FILE`、`/search-config off` | 配置或关闭搜索服务；文件包含 `endpoint` 和可选 `api_key_env` |
| 高级 | `/events PROMPT`、`/trigger SOURCE [JSON]` | 为所选目标开启显式事件，或递交带来源与数据的事件 |
| 公开 | `/loops schedule GOAL_ID SECONDS [--for DURATION_SECONDS] PROMPT` | 设置本机定时执行，可选到期时长 |
| 公开 | `/loops trigger [GOAL_ID]` | 查看指定或所选目标的 Trigger，不改变选择 |
| 高级 | `/clear-trigger` | 关闭所选目标的整个 Trigger，包含定时、事件与订阅 |
| 高级 | `/acknowledge-input COMMAND_ID` | 停止等待结果未知的输入，不重发；之后可以发送新输入 |
| 高级 | `/acknowledge-create COMMAND_ID` | 停止跟踪未知或失败的创建（交回草稿），或取消首个输入前未确认的其余选择；不重放任何命令 |
| 公开 | `/compact [INSTRUCTIONS]` | 主动压缩；补充说明只用于本次摘要请求。自动压缩使用当前模型的已知上下文窗口 |
| 高级 | `/operations` | 查看本进程内记录的工具操作及结果；重启后不保留 |
| 公开 | `/help` | 显示入口与命令说明 |
| 公开 | `/exit` | 关闭当前 View；Agent 与已接受任务继续 |
| 公开 | `/subscriptions` | 查看当前 Chat 的 Tick Subscription，调整 cadence 或取消 |
| 高级 | `/tui [inline\|fullscreen]` | 查看或切换终端渲染器 |
| 高级 | `/status` | 查看当前 Chat、模型、执行模式与队列 |
| 高级 | `/background OPERATION`、`/stop-command OPERATION` | 后台执行或停止一个 Shell 任务 |
| Skill | `/tickloop [REQUEST]` | 加载系统 Tick Loop Skill，不额外授权 |

默认不保留 alias；本次移除的名称不再被解析，也不会自动转发。旧 `/new`、`/goal`、`/triggers`、`/schedule` 的能力归入 `/loops`；Theme 位于 `/settings → Theme`，只影响当前终端；退出 Codex 登录位于 `/provider → Codex subscription → p Connection → Sign out`，不会自动重新登录。

Loops 的 Configure timer 把当前配置填入主输入框，使用同一套普通编辑键与 Ctrl+G 外部编辑器。修改 `/loops schedule GOAL_ID SECONDS [--for DURATION_SECONDS] PROMPT` 后 Enter 保存，Ctrl+C 取消草稿；已有草稿不会被覆盖。无 `--for` 时移除期限。当前任务忙时排队；显式参数形式可用于管道。新目标保留当前 Chat 历史，选择目标与查看详情不自动执行任务。

交互菜单仅在 stdin 和 stdout 都是终端时启用。管道输入保持按行命令和详细事件输出，不输出终端控制码；模型／设置选择可以输入菜单编号或 `cancel`。原有 `/allow REQUEST`、`/loops UUID`、`/background OPERATION`、`/stop-command OPERATION` 等显式参数形式继续可用，适合脚本及高级操作。计划批准在管道输入中也自动显示同一组选项，输入菜单编号或 `cancel` 处理。

管道输入的 EOF 会等待已提交的输入按序完成；模型失败时以非零状态退出。显式 `/exit` 立即离开 View，已接受工作继续。

运行中的补充输入立即由 Agent 受理，并在可以构造完整模型请求时按序执行。等待权限时不会自动批准；若输入流已关闭，则提示当前 View 无法回答并离开；daemon 保留真实等待请求，其他 View 可以处理。

交互权限菜单的 `Allow once` 和 `Deny` 支持按 `Tab` 编辑附言，再按 `Tab` 收起并保留文字，或按 `Enter` 连同选择提交。批准附言在工具结果之后交给模型；带附言拒绝把理由交给模型并继续当前轮；无附言拒绝或停止当前轮正在等待的前台命令等同中断，结束当前轮。照 Claude Code，工具执行失败以及被规则、Plan 模式或自动审查拒绝时，错误作为工具结果交给模型，当前轮继续。会话和持久授权选项不接受附言，附言不会扩大授权范围。

`/permissions` 首页按 Auto、Ask、Full Access 排列，另有直接查看近期记录的入口。Auto 是默认模式；Ask 保留工作区编辑便利，其他受保护操作由人确认；Full Access 跳过普通人工与模型审批，但显式 Ask／Deny、Host 强制确认、目录信任和工具执行边界仍有效。模式切换只影响当前 Chat，不改用户默认值；恢复 Chat 仍按配置默认值或启动覆盖选择。Auto 的二级页先显示 Auto mode、后显示 Recent activity；Ask／Full Access 只显示 Recent activity。Esc 从二级页回模式首页，再返回原 Chat／Settings。

Auto 默认由当前 Chat 的模型发起独立、无工具的 LLM 审批，不是新的对话轮次。`/permissions` 的 Auto mode 页将审批方式分为 LLM 与 Judgment：LLM 使用当前 Chat 模型，Judgment 当前使用 Jev（TypeSafe），可复用或更新 Jev Key，并编辑 Soft allow、Soft deny、Hard deny 与 Environment；选择审批方式不会改变 Execution Mode。`/jev` 是迁移入口，在 Auto 下打开该页，其他模式先回到权限首页。`/model` 保持聊天模型选择流程，并额外提供 `Judgment settings · Jev (TypeSafe)`；该入口只配置共享 Key，不切换聊天模型、执行模式或审批方式。Auto 页面需要配置 Key 时打开同一个配置页，并返回原入口；只有从 Auto 发起的明确改选流程，才会在 Key 保存成功后尝试应用审批方式。Key 在隐藏输入框填写，保存在用户状态目录的 `.env`（目录 `0700`、文件 `0600`），不进入对话与普通设置；也可使用 `TYPESAFE_API_KEY` 环境变量。已保存的 Key 优先于环境变量；显示“有 Key”不代表认证成功。Key 保存成功后下一次 Jev 审批使用新值。

默认的八类风险——外部代码、数据破坏、持久后台活动、安全改动、数据外发、凭据访问、任务外改动与金融执行——以带稳定 ID 和版本的内置 Soft deny 展示；Soft allow 与 Hard deny 内置组初始为空。新增用户规则保留内置组；明确替换时先预览失效内容并核对设置版本，恢复默认只重置所选组。Environment 区分内置内容与用户替换，它描述可信运行事实，不授予权限。规则和 Environment 只约束实际进入 Auto 审批的操作，不能覆盖确定性 Deny、显式 Ask 或宿主强制限制；需要按工具禁止时使用 `/rule deny`。两种审查器共享同一份当前 Loop 的真实用户要求、拟执行操作、完整有效规则与 Environment。压缩摘要、工具结果、Agent 叙述和其他 Loop 的输入不能提供授权；原始依据缺失或设置损坏时转人工，无人值守拒绝。

Jev 对任务范围、每条 Hard deny 的适用性及每条 Soft deny 的适用性和解除依据分别提出独立 Choice 问题；LLM 返回同样的逐规则语义发现，宿主统一归约。明确授权或匹配的 Soft allow 只能解除对应 Soft deny；Hard deny 不能由普通授权或连续拒绝后的单次确认绕过。Jev 的置信度低于 0.8 或所选选项概率低于 0.9、条件未证实时转人工；无效结构属于调用失败，不能伪装为“不确定”。Jev 不计算金融风险限额，也不建立交易授权。

Jev 非敏感服务配置仍可写在用户 `settings.json` 的 `jev`，例如 `{"model":"jev-1.13.0","endpoint":"https://api.typesafe.ai/v1/systemone"}`；新审批重新读取配置，不要在这里存 Key。旧 `--auto-reviewer-config` 与 `--permission-checker-config` 独立模型参数会明确报迁移错误，改在 `/permissions` 选择。选用 Jev 时每次审批总预算 10 秒、最多两次尝试；配置、认证、网络、超时、容量或无效响应等技术失败用同一份完整依据回退到当前 Chat 的 LLM，有效允许、拒绝和语义不确定均不回退。失败后 Jev 冷却 60 秒，下一次审批探测恢复；回退和恢复各提示一次，保存的首选仍是 Jev。两者都不能完成审批则转人工，无人值守拒绝。任何容量失败都不截断授权依据或规则。

当前 Chat 的 Recent activity 页保留最多 100 条最终权限结果，包括只读工具的直接放行、LLM／Jev 审批、人工确认和拒绝。每项占一行，显示结果、判定方式和操作；`No review` 表示直接放行且未调用审批模型，`LLM`／`Jev` 表示实际完成审批的模型来源，`Manual` 表示人工决定。Enter 查看请求参数、时间、原因、规则和回退经过。请求详情保留有界的命令、路径和普通输入内容，敏感值脱敏；旧记录未保存请求时明确提示。权限放行不代表工具执行成功。启用 Chat 持久保存且记录成功时，重启恢复同一 Chat 后可继续查看；关闭持久保存时只在本次运行中保留。已有拒绝记录可继续读取，旧对话中未记录的放行不会补造。只有拒绝详情可按 `r` 请求 Agent 重新计划和审批，不能直接重放历史工具命令或建立 Allow。Allow／Ask／Deny 和 Directories 管理页本期暂不展示，既有规则与 `/rule`、`/add-dir` 等命令继续生效。明确拒绝作为可恢复工具结果；软拒绝连续达到阈值时交互 Chat 可转人工，Hard deny 不能这样放行。`--deny-on-prompt` 和 `--dangerously-skip-permissions` 是不进入普通模式选择的高级启动策略；后者只应用于外部隔离环境。

首次进入未知的规范化工作目录时，先启动终端界面，再显示目录信任选择；默认选中 `No, exit`，用方向键、j/k 或 Ctrl+N/P 选择后 Enter 确认。No、Esc、Ctrl+C 或 EOF 安静退出（exit 0），不保存信任或发起模型、工具请求。明确选择 Yes 后才加载项目内容、连接执行服务并进入普通输入。信任通过用户本地 `settings.json` 保存精确目录（包括 home），等价符号链接共享同一身份，子目录不自动继承；保存失败留在选择页，可重试或取消。非交互启动默认拒绝未知目录；外部宿主已经独立建立信任时才使用 `--working-directory-is-trusted` 明确声明该前提，且该参数不保存信任。

选择时光标停在聚焦选项的指示符。窗口必须完整显示标题、目录路径、两个选项与确认提示；放不下时提示调整窗口并禁止 Yes，仍可取消。真实输入 PTY 关闭也按 EOF 安静退出。

搜索配置示例为 `examples/mwmbl-search-config.json`，指向公开的 Mwmbl 搜索接口。设置服务地址不等于授予联网权限；工具执行前仍按当前规则检查。定时任务、后台进程与切走的 Chat 依赖本机 daemon／worker 存活。退出 View 不停止执行；所有者或机器退出后的恢复需要显式装载与 Continue。

每个 Chat 的对话照 Claude Code 写成只追加的 Transcript：`<root>/projects/<项目目录名>/<chatId>.jsonl`，状态根目录默认为 `~/.tickloop`，可用 `TICKLOOP_CONFIG_DIR` 指定，`--no-session-persistence` 关闭持久保存。项目目录名由工作目录的 canonical 路径生成，文件在第一条消息写入时创建，之后 `/cwd` 改变工作目录也不移动文件。条目每 100ms 批量追加，不加锁；进程被强制结束时最多丢失最后一批。同一目录可以同时运行多个 `tickloop` View；同一 owner 仲裁同 store＋Chat 的装载并返回同一个活执行。独立 Embedded 容器继续保留原文件分叉约定，最近活动链用于恢复；这不是全局 Transcript 写锁。

压缩照 Claude Code 写成压缩边界：`/compact` 与自动压缩在对话末尾追加一个 `compact_boundary` system 条目（父指针为空，逻辑父指针指向压缩前最后一条）和一条压缩摘要 user 消息，之后的消息接在摘要后面；模型只看到最后一个边界之后的消息。本进程内边界之前的消息仍在 Ctrl+O 的完整记录中，rewind 到边界之前的提示时边界随之丢弃，模型重新看到原始对话，下一条消息在 Transcript 中从边界之前分叉（重新出现的私有推理按目标来源投影，原始块仍保留）；恢复 Chat 时模型对话链在边界处断开，边界之前的提示不再作为模型历史加载。审批依据另行保留真实用户输入及来源：新边界记录压缩前的原始输入，旧边界从当前对话链的 Transcript 记录恢复；摘要、工具结果、Agent 叙述或其他 Loop 的消息不因此获得授权效力，来源缺失则转人工。定向摘要不照 Claude Code 写保留区段（叠加摘要或 rewind 到保留消息的第一条提示后，区段重接会丢失或复活消息），保留的消息直线重写在新边界之后，恢复时沿最新叶子自然只得到新链；读取 Claude Code 格式、带保留区段的文件时仍按区段重接并清零保留 assistant 消息的用量，只有最新叶子的对话链经过文件中最后一个边界时才删除该边界之前的其他内容，区段校验失败时加载完整历史并在打开 Chat 时显示警告。超过 5MB 的 Transcript 只加载最后一个不带保留区段、父指针为空的边界之后的模型历史，边界之前只读取标题和 Branch 来源；审批所需原始输入仍从边界元数据或旧文件的当前链定向恢复。最新叶子的对话链走到被跳过的内容（rewind 越过该边界后继续的分支）时改为加载完整文件。自动压缩的摘要消息照 Claude Code 带继续指令，要求模型不再提问、直接接着做；手动 `/compact` 不带。压缩后元数据重新追加到文件末尾。

Chat 的附属文件照 Claude Code 放在 Transcript 同级的 `<chatId>/` 目录：超过阈值的工具结果（`Grep` 20K、`Bash` 30K、其余工具 50K 字符；`Read` 与含图片的结果不外存）写入 `tool-results/<调用 id>.txt`，模型收到带文件路径和 2KB 预览的 `<persisted-output>` 文本；同一轮工具结果合计超过 200K 字符时，最大的新结果也照此外存，替换决定记入 Transcript，恢复、`--fork-session` 与 `/branch` 后发给模型的内容保持不变；压缩与定向摘要写出边界时，把仍在边界之后对话中的结果的替换决定重新记录在边界之后，`--fork-session` 把替换决定写在重写的对话之后，超过 5MB 的 Transcript 跳过边界之前的内容时替换不会丢失。文件改动 review 写在 `file-changes/`。后台与前台 Shell 输出写入系统临时目录 `/tmp/tickloop-<uid>/<项目目录名>/<chatId>/tasks/<任务 id>.output`（可用 `TICKLOOP_TMPDIR` 改变 `/tmp`），每个文件最多 64 MiB，不做清理。模型读取项目目录与项目临时目录中的文件不需要授权。`--no-session-persistence` 时 Chat 目录也在上述临时目录中，工作目录下不写任何文件。

启动时不列出、也不加载已保存的 Chat；`/resume` 列出当前项目的 Chat，`/resume CHAT` 或 `-r CHAT` 按 id 恢复。恢复过滤没有工具结果的 tool_use 与空白回复，保留仅含 thinking/私有协议块的原始回复并按来源投影，末尾是工具结果时追加 "Continue from where you left off."，末尾是 user 消息时追加不请求回复的哨兵；Loop 的创建、状态变化、Trigger 配置与结束作为 system 条目挂在对话链上（不发给模型），恢复时沿当前链重放：运行中或等待中的 Loop 转为已中断，过期的 Trigger 标为已过期，武装中的 Trigger 解除武装，向该 Loop 提交输入时重新武装，不会自动运行；压缩后为未结束的 Loop 重记完整状态。Chat 快照不整体写入磁盘，恢复时全部由对话链与元数据重建：工作目录取链上最后一个条目记录的 `cwd`（`/cwd` 之后没有新条目时，重启回到之前记录的目录），Branch 来源取 `branch-origin` 与标题元数据（Branch 的请求标识与复制时的公开对话来源只在创建它的进程内有效）。同一个 Loop 被多个进程武装时，触发前获取 `<chatId>/loops/<loopId>.lock` 的租约，只有持有者触发：锁文件独占创建，内容（chatId、pid、nonce、启动时间、主机）只作诊断，持有权由对该文件的 flock 决定，持有进程退出（包括被杀死）时由内核释放，其他进程每 5 秒重试接手；锁内容记录的主机与本机不同时不接管，并在状态栏提示。结束、解除武装与重新武装写入 `<chatId>/loops/<loopId>.state`（递增 generation，结束不可撤销，rewind 移除 Loop 视为结束），持有者每次触发前读取，Loop 已在其他进程结束或解除武装时本地同步停止或解除武装且不触发。租约被其他进程持有、租约或状态文件读写失败时，状态栏显示 `Trigger:` 提示，显式事件触发失败时返回存储错误。照 Claude Code，挂起的权限请求与提问、排队输入与命令、操作记录、事件回放和用量不随 Chat 保存，恢复后不再出现；Execution Mode 使用当前 `defaultMode` 或启动覆盖，模型使用本进程当前的选择。打开历史无需已有模型凭据，开始运行时才检查当前凭据与目录。状态根目录需要可写，否则可以用 `TICKLOOP_CONFIG_DIR` 指向另一个可写目录，或用 `--no-session-persistence` 运行而不保存 Chat。

计划属于 Loop，照 Claude Code 写入 `<root>/plans/<slug>-loop-<loopId>.md`：slug 在 Chat 第一次进入 Plan 模式时生成（形容词-动词-名词），记录在之后的每个 Transcript 条目中，恢复时按第一个有效的 slug 找回计划文件（slug 只接受以连字符连接的小写字母单词、最长 64 字节；链上没有有效 slug 时警告并换用新的 slug）。模型可以调用 `EnterPlanMode` 请求进入 Plan 模式，经用户确认后生效。Plan 模式下每次请求的系统提示写明当前 Loop 计划文件的绝对路径，这是唯一允许 `Write`／`Edit` 的文件；模型写好计划后调用 `ExitPlanMode` 提交审批，文件不存在或为空时报错。提交时宿主在计划文件首行写入 `<!-- tickloop plan version: N sha256: <正文摘要> -->` 注释（Markdown 渲染时不可见），版本号与内容一起先写入同目录临时文件（0600）并同步到磁盘，再改名替换，写入中途失败不会留下半截文件；读取不跟随符号链接，只接受普通文件、不超过 64KB 的正文，`plans/` 须是当前用户私有（0700、非符号链接）的目录。`ExitPlanMode` 自动发起属于来源 Agent 的批准请求，进入固定 12 行的输入对话框并等待决定。正文可用 PgUp/PgDn 滚动，`v` 查看全文，全文中的 Esc 先返回批准界面。选择“批准并开始 · Auto / Ask / Full Access”后，宿主核对计划版本、正文与文件摘要，再将 Claude Code 风格的 `User has approved your plan. You can now start coding. …`、路径与审阅正文作为当前工具结果返回，原 Run 继续实施。选择“No, keep planning”后输入反馈，Enter 将反馈作为拒绝工具结果交给模型，保留 Plan 模式继续修订；批准界面的 Esc 取消当前轮并保留待批计划，`/plan` 可重新打开。已处理的计划由 `/plan` 查看，不改变执行模式。计划版本与待批状态随 Loop 状态记录在 Transcript 中，待办仍从最后一次成功的 `TodoWrite` 输入恢复；压缩、rewind 截断、`/branch` 与 `--fork-session` 使之前的调用不在链上时，Loop 状态条目会带上当时的计划版本与待办。恢复时核对计划文件：版本行中的版本等于重建的版本、且摘要与正文相符才沿用该版本；否则（例如提交后文件又被编辑、结果写入 Transcript 之前进程崩溃，或文件在对话之外被修改）计划以大于文件与链上版本的新版本恢复为未批准，按旧版本号的批准一律被拒，并显示加载警告。链上没有记录、也没有版本行的文件是从未提交的草稿，不恢复为计划。计划文件缺失或不可读时该 Loop 暂无计划并显示警告，但保留已知的最高版本，下一次提交从更大的版本继续，版本号不会被复用。重启恢复待批计划时重新进入 Plan 并自动展示同一个批准界面，执行等待明确决定。批准选择的模式与 Execution Mode 一样只在本进程内有效，已处理计划的恢复不会自动开始实施。rewind 不回退计划文件与版本（照 Claude Code 计划文件不随 rewind 恢复），版本号不会被复用。`/branch` 与 `--fork-session` 生成新的 slug 并复制已提交的计划，之后双方修改互不影响。不保存 Chat 时（`--no-session-persistence` 或 `cleanupPeriodDays` 为 0）计划文件照 File History 放在系统临时目录的 `tickloop-<uid>/<项目目录名>/plans/`。

过期文件清理照 Claude Code：保留天数取状态根目录 `settings.json` 的 `cleanupPeriodDays`，默认 30。daemon worker 在首个实际 run／Shell 开始后计时，10 分钟后在后台线程清理一次，不依赖前台 View；仅装载历史不启动清理。活记录由 retention 锁保护；worker 不接收本地草稿、按键或鼠标活动。Embedded 调用者仍可沿原 HousekeepingSchedule 自行记录交互并推迟清理。清理按修改时间删除保留期之前的 `projects/*/<chatId>.jsonl`、`<chatId>` Chat 目录中 `tool-results/` 下的文件、`plans/*.md`、整个 `file-history/<chatId>/` 目录与 `paste-cache/*.txt`（只认 UUID 形式的 chatId，其他同名文件与目录不动），之后删除清空的工具结果目录、Chat 目录、项目目录、`plans/` 与 `file-history/`；`file-changes/`、Loop 租约、`history.jsonl`、进程登记、设置文件与系统临时目录中的 Shell 输出不清理。活 runtime 为每个已加载 Chat 持有私有 retention flock；清理只有在取得独占锁后才按原 TTL 删除记录与附件。共享存活期间保留，即使没有近期输出；最后 shutdown 或进程退出后由内核释放，后续清理沿原期限。锁不是 Agent 身份、数据库或持久 journal。手动删除记录仍会提示后续无法保存。文件回滚备份目录与计划文件不随对话更新修改时间，因此 Transcript 仍未过期的 Chat 保留其 `file-history/<chatId>/`，未过期 Transcript 中记录的计划前缀对应的 `plans/<slug>-loop-<loopId>.md` 也保留（超大 Transcript 头尾各 64KB 没有前缀且超过 64MB 时，本次不删除任何这类计划文件）。符号链接一律跳过，不删除链接，也不进入或删除它指向的内容。`settings.json` 有任何无效内容（清理天数本身、模型、上下文窗口或权限规则）且显式写了 `cleanupPeriodDays` 时，本次不清理，并与其他设置问题一样显示警告；文件不是合法 JSON 时只要其中出现 `"cleanupPeriodDays"` 就不清理，文件不可读时也不清理。`cleanupPeriodDays` 为 0 时 Agent 启动后不写 Transcript（大工具结果、文件回滚备份与 Shell 输出放在系统临时目录，计划只在内存中，与 `--no-session-persistence` 相同），`-c`／`-r` 会提示原因；用户设置与输入历史照常读写；后台清理把修改时间早于清理时刻的上述文件全部删除。是否写 Transcript 在启动时按当时的设置决定，运行中改为 0 或从 0 改回在下次启动生效；清理天数在清理时读取。

用 `/resume` 或 `/clear` 切走的 Chat 留在本机 daemon 内，其中的目标、定时与事件触发继续运行，结果只写入它自己的 Transcript；前台只显示当前 Chat 的事件，后台 Chat 的回复结束或 Shell 任务完成时只在状态栏提示，不插入前台对话。退出 View 只撤显示登记；明确的 run interrupt／Loop stop 才停止相应工作，不向 child／家族传播。

View 的兼容诊断记录仍在 `<root>/sessions/<pid>.json` 登记 pid、当前前台 chatId、工作目录、启动时间、类型与入口；切换前台 Chat 时改写，正常退出时删除。启动登记后会检查其他登记文件，删除已退出进程留下的文件。

```sh
printf '记住验收词紫罗兰。\n刚才的验收词是什么？\n' | \
  cargo run -p tickloop-agent --bin tickloop
```

功能进度与真实接入记录见仓库 `.scratch/agent/issues/`；界面与 Claude Code 的逐项对照在 `.scratch/agent/validation/claude-code-ui-checklist.md`。运行终端测试不需要真实模型凭据，测试通过本地 HTTP 服务验证实际编解码和可执行入口：

```sh
cargo test -p tickloop-agent --test terminal
```

Agent 的呈现与宿主布局分开，Herdr 负责容器外观；颜色、装饰与 Theme 不写入业务上下文和 Agent 存档。

内置主协议卡 ID 采用官方原始服务 ID，保留大小写；`adapter` 表达协议，官方模型名称不添加自造协议/套餐后缀。Provider 的 `model_aliases` 保留旧 ID：例如 `gpt-6.1-sol-responses` → `gpt-6.1-sol`、`minimax-m3-messages` → `MiniMax-M3`。保存的已知内置身份与别名继续可用，下一次明确保存使用 canonical ID；所有路由、能力及参数均从当前编译目录解析，既有外部定义和覆盖被忽略并显示一条提示。未知外部身份回退内置默认，用户文件与凭据保留；已知内置身份不会自动升级到其他模型版本。DeepSeek 已有额外 Messages/Responses 卡继续保留独立旧 ID 与原 Adapter/endpoint，避免与同模型主协议卡冲突。

The current Chat’s Monitor appears as a 42-column list beside the conversation on wide terminals. Monitor holds the Ticks this Chat opened or explicitly added, those its unfinished Loops subscribe to, its artifacts and its background Shells, deduplicated by instance; `Monitor N` counts that full set. Opening, adding, reading and viewing never subscribe or call the model. The input, both dividers, hints and footer remain full width; narrow-terminal overlays cover only conversation content. An empty Monitor stays hidden. `/monitor` or the input’s `Monitor N` toggle saves the `monitor: auto|hidden` setting; `→` from an empty input focuses a nonempty Monitor when no prompt suggestion is visible. Clicking either row line selects it and runs Enter. `x` removes a row from this Chat’s Monitor only and is remembered until the instance is explicitly added again; its process, subscriptions and feedback stay as they were. In a Herdr View, `a` in Monitor or `/monitor add` lists the current Workspace’s programs one 128-row page at a time for explicit adding, which transfers no ownership or permission. Other Ticks open a details view with their owner, state, latest business update and process resources, shown as unavailable when unknown. Shells open a detail inside the Tick panel with live output, scrolling and a confirmed `s` stop; Esc returns to the selected row. `/tasks` opens the Task Center list and detail in the fixed 12-row Input Dialog. Subscribed Ticks open their subscription, and subscribed artifacts retain browser/copy/feedback actions. `/artifacts` signs in when needed and opens the All/Mine/Shared/Pinned picker. Enter adds an artifact without granting access, connecting feedback or subscribing. Artifact `f` resumes feedback or asks the Agent to connect it. Artifact Service sign-out is in `/settings`.

模型回复写入完整消息后，主界面立即显示剩余正文（含未换行尾行），不等待后续工具审批。提议工具先显示 Queued，准入／完成时更新原调用行；排队的问题先预览，到实际提问时才打开回答菜单。工具权限与执行顺序保持原契约。

分隔线两侧的入口使用短横线边段与相邻空格：`─ ▾ Subagents N …`、`… ◉ Monitor N ─`，核心状态为 `… Auto · Model [· Context N% used] ─`；窄屏先省略边段，鼠标只命中 glyph 与标签。Subagents 展开后直接显示 main 与子 Agent 列表，不再插入 items／打开提示摘要行。

Subagents 的功能键提示保留原文本与按键行为，位于底部区域最后一行：展开时在列表后，收起时紧接分隔线；不在分隔线与成员之间插入提示。

Subagents 行不填充面板背景，只对键盘选中行使用 Selection。完成／失败／停止行在离开当前查看后保留30秒；查看中的完成子 Agent 不自动移除。隐藏只改变本 View 的面板和返回记忆，不结束执行、不删除上下文、不解除代号；后续 SendMessage 使同身份子 Agent 工作时重新出现。总览返回家族时恢复该 View 最后查看且仍可见的成员，否则 main。命令拒绝反馈位于输入框上方独立 Warning 行，保留草稿；子 Agent 的 /clear 不改写历史，反馈为命令在前的短说明。
