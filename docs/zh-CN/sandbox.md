# 沙箱 (macOS)

[Tickloop](../../README.zh-CN.md) · [English](../sandbox.md) | 简体中文

在 macOS 上，Bash、后台 Shell 和 `!` Shell 可以在操作系统沙箱中运行。沙箱默认关闭，只读取你自己的 `settings.json`；仓库内容不能打开、关闭或放宽它。例如：

```json
{
  "sandbox": {
    "enabled": true,
    "network": { "allowedDomains": ["registry.npmjs.org", "*.github.com"] }
  }
}
```

用 `/sandbox` 打开 **Mode**（沙箱且免审、沙箱但正常审批、关闭）、**Overrides**（允许申请越出或严格模式）与只读的 **Config** 页。左右键切页，`Ctrl+N`/`Ctrl+P` 或上下键移动或滚动，Enter 保存，Esc 关闭。`/sandbox exclude "docker:*"` 为本 Folder 添加排除命令；排除命令仍走正常审批。非 macOS 平台可以查看设置，但不能通过此命令开启沙箱。

改动只保存到用户自己的 `settings.json` 的 `projects["<规范化 Folder 路径>"].sandbox`，下一条命令起生效，无需重启。目录标量覆盖全局 `sandbox`；`excludedCommands`、`filesystem.allowWrite`/`denyWrite`、`network.allowedDomains` 合并两层并去重。Config 标出每项来源，并展示可写范围、受保护路径、网络规则与本机支持情况。其他窗口同时修改沙箱设置时，页面刷新并要求重新确认，不覆盖别人的改动。`/permissions` 提示可用 `/sandbox` 修改。

沙箱中的命令可以写 Folder、附加目录、已验证的 linked worktree、系统临时目录和家族临时空间（Agent 与其 Subagent 共用）；不能写配置目录、用户设置文件和 Git hooks 等敏感路径；网络默认断开，只能访问白名单中的目标。`/permissions` 显示沙箱是否开启。Tick、MCP 服务以及文件工具（如 `Read`、`Write`、`Edit`）不在沙箱中运行，仍由权限规则管理。

## 字段

| 字段 | 默认 | 作用 |
| --- | --- | --- |
| `enabled` | `false` | 让 Bash、后台 Shell 和 `!` Shell 在沙箱中运行 |
| `failIfUnavailable` | `false` | 沙箱不可用时（其他平台，或已在另一个沙箱中）：`false` 警告后按无隔离运行，`true` 拒绝执行 |
| `autoAllowBashIfSandboxed` | `true` | Ask 与 Auto 下，在沙箱里运行的 Bash 和后台 Shell 直接执行，不问人也不送 Auto 审查 |
| `allowUnsandboxedCommands` | `true` | 允许模型通过 `dangerouslyDisableSandbox` 申请越出；`false` 忽略该请求，但 `excludedCommands` 仍生效 |
| `excludedCommands` | `[]` | 总在沙箱外运行并走正常审批的命令 |
| `filesystem.allowWrite` | `[]` | 追加可写路径 |
| `filesystem.allowGitConfig` | `false` | 放开 `.git/config` 写入；Git hooks 与其他保护仍生效 |
| `filesystem.denyWrite` | `[]` | 保持只读的路径，优先于 `allowWrite` |
| `network.allowedDomains` | `[]` | 经本机代理可以访问的目标 |

其余 `sandbox` 字段会提示“暂不支持”。用户设置文件无法读取或解析，或 `sandbox` 的值无效时，Shell 准备会报错并拒绝执行，直到你改正。合法文件未配置 `sandbox` 时仍为关闭。

沙箱开启后，Ask 与 Auto 下在沙箱里运行的 Bash 和后台 Shell（包括 Subagent 的）直接执行，不问人也不送 Auto 审查：能写什么、能连什么由沙箱限定，而不是审查器。显式 Ask/Deny 规则仍先生效，Plan 与 Full Access 不变；设置 `autoAllowBashIfSandboxed: false` 保留隔离但恢复正常审批。

## 越出沙箱

Bash 可为单次调用传入 `dangerouslyDisableSandbox: true` 申请越出沙箱。沙箱开启时，这次调用按无隔离运行并走正常审批；Auto 会送审查器，不走只读或目录内编辑的快速通过。`sandbox.excludedCommands` 可让匹配的调用走相同路径，支持完整命令、`"docker:*"` 这样的前缀以及 `*` 通配；复合命令中的一段命中时，整次调用在沙箱外运行；也识别前置的字面环境变量赋值和安全包装命令。`allowUnsandboxedCommands` 设为假只忽略模型的越出请求；明确排除的命令仍在沙箱外运行并走正常审批。沙箱关闭时 Bash 的这一参数不生效。

## 写保护

默认禁写在可写根下的任意深度生效，不区分文件名大小写：`.gitconfig`、`.gitmodules`、`.bashrc`、`.bash_profile`、`.zshrc`、`.zprofile`、`.profile`、`.ripgreprc`、`.mcp.json`、`.vscode/`、`.idea/`、`.git/hooks/` 与 `.git/config`。还保护 `.tickloop/`、`AGENTS.md`、`CLAUDE.md`、`CLAUDE.local.md`，以及 `.claude/` 下的 `settings.json`、`settings.local.json`、skills、hooks、commands、agents、rules。凭据路径、用户设置文件和配置根仍受保护；家目录文件位于可写根外时同样不可写。普通项目文件与其他 Git 数据仍可写；`filesystem.allowGitConfig: true` 仅放开 `.git/config`，hooks 和显式 `denyWrite` 仍生效。

`filesystem.allowWrite` 与 `denyWrite` 写字面路径：相对路径以 Chat 的 Folder 为基准，不随 Bash 的 `cwd` 改变，`~/` 指你的主目录。`cwd` 位于 Folder 和附加目录之外时走正常审批，沙箱仍保持原有可写范围。禁写规则和敏感路径优先。读取不受限制，但沙箱中的命令不能穿过敏感符号链接（读取也不行），以防别名绕过禁写规则；配置的禁写路径与用户设置文件也会保护解析后的目标。

## 网络

HTTP 与 HTTPS（CONNECT）经过本机代理，沙箱中的命令从标准代理环境变量（`HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`）得到它；不遵循这些变量的命令也不能直连。`*.github.com` 这样的通配只匹配子域名，不包含域名本身。被拒的目标会写入命令输出，方便你知道该加哪条白名单。每个 Tickhost 的认证回环代理在其整个生命周期内保持端口绑定，每个 Shell 使用启动时冻结的白名单。

沙箱内失败且输出包含权限拒绝特征的命令会附加一行“被沙箱拦截”的推测说明；只有允许越出时才提示 `dangerouslyDisableSandbox`。退出状态保持不变，不会自动重试命令。

## 已知限制

脱离原进程组的后代可能在 Tickhost 重启后仍存活；其 OS 沙箱仍允许连接原代理的回环端口，因此可能访问之后占用该端口的其他服务。
