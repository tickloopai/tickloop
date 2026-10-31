# Sandbox (macOS)

[Tickloop](../README.md) · English | [简体中文](zh-CN/sandbox.md)

On macOS, Bash, background Shells and `!` Shells can run in an OS sandbox. It is off by default and is read only from your own `settings.json`; repository content cannot turn it on, off or widen it. For example:

```json
{
  "sandbox": {
    "enabled": true,
    "network": { "allowedDomains": ["registry.npmjs.org", "*.github.com"] }
  }
}
```

Use `/sandbox` to choose **Mode** (sandbox with auto-allow, sandbox with regular permissions, or off), **Overrides** (allow escape requests or strict sandbox), and the read-only **Config** page. Left/right switches pages; `Ctrl+N`/`Ctrl+P` or up/down moves or scrolls, Enter saves, and Esc closes. `/sandbox exclude "docker:*"` adds an exclusion for this Folder; excluded commands still use normal approval. Non-macOS hosts can inspect settings but cannot enable the sandbox through this command.

Changes are saved only in your own `settings.json`, under `projects["<canonical Folder>"].sandbox`, and apply to the next command without restarting. Folder scalars override global `sandbox` settings; `excludedCommands`, `filesystem.allowWrite`/`denyWrite`, and `network.allowedDomains` merge and deduplicate across both scopes. Config labels each setting's source and shows writable scope, protected paths, network rules and host support. Concurrent sandbox edits refresh the page instead of overwriting the other edit. `/permissions` links back to `/sandbox`.

Sandboxed commands may write the Folder, additional directories, verified linked worktrees, the system temporary directory and the family's temp space (shared by an Agent and its Subagents). They cannot write your configuration directory, your settings file or sensitive paths such as Git hooks, and networking is blocked unless the allowlist names the destination. `/permissions` shows whether the sandbox is on. Ticks, MCP servers and the file tools (such as `Read`, `Write` and `Edit`) do not run in the sandbox; your permission rules govern them as before.

## Fields

| Field | Default | Effect |
| --- | --- | --- |
| `enabled` | `false` | Run Bash, background Shells and `!` Shells in the sandbox |
| `failIfUnavailable` | `false` | When the sandbox is unavailable (another platform, or already inside a sandbox): `false` warns and runs without isolation, `true` refuses to run |
| `autoAllowBashIfSandboxed` | `true` | In Ask and Auto, sandboxed Bash and background Shell commands run without asking or Auto review |
| `allowUnsandboxedCommands` | `true` | Allow model requests through `dangerouslyDisableSandbox`; `false` ignores those requests, while `excludedCommands` still applies |
| `excludedCommands` | `[]` | Commands that always run outside the sandbox, with ordinary approval |
| `filesystem.allowWrite` | `[]` | More writable paths |
| `filesystem.allowGitConfig` | `false` | Permit `.git/config` writes; Git hooks and other protections remain in force |
| `filesystem.denyWrite` | `[]` | Paths kept read-only; wins over `allowWrite` |
| `network.allowedDomains` | `[]` | Destinations reachable through the local proxy |

Other `sandbox` fields produce an unsupported-setting warning. If the user settings file cannot be read or parsed, or its `sandbox` value is invalid, Shell preparation fails until you fix it. A valid file without `sandbox` keeps it off.

While the sandbox is on, Ask and Auto run sandboxed Bash and background Shell commands, a Subagent's included, without asking or Auto review: the sandbox, not the reviewer, bounds what they can write and reach. Explicit Ask and Deny rules still apply first, Plan and Full Access are unchanged, and `autoAllowBashIfSandboxed: false` keeps the isolation but restores ordinary approval.

## Leaving the sandbox

For a single call, Bash can request `dangerouslyDisableSandbox: true`. With the sandbox enabled, that call runs without isolation and goes through ordinary approval; Auto reviews it instead of using the read-only or local-edit fast paths. `sandbox.excludedCommands` selects calls that should use the same route: exact commands, prefixes such as `"docker:*"`, and `*` patterns are supported. A matching part of a compound command selects the whole invocation. Leading literal environment assignments and safe wrapper commands are recognized. Setting `allowUnsandboxedCommands` to false ignores model escape requests; explicitly excluded commands still run outside the sandbox with ordinary approval. With the sandbox off, the Bash parameter has no effect.

## Write protection

Default write protection applies at every depth under writable roots, regardless of filename case: `.gitconfig`, `.gitmodules`, `.bashrc`, `.bash_profile`, `.zshrc`, `.zprofile`, `.profile`, `.ripgreprc`, `.mcp.json`, `.vscode/`, `.idea/`, `.git/hooks/` and `.git/config`. It also covers `.tickloop/`, `AGENTS.md`, `CLAUDE.md`, `CLAUDE.local.md`, and `.claude/` settings (`settings.json`, `settings.local.json`), skills, hooks, commands, agents and rules. Credential paths, the user's settings file and configuration root remain protected. Home files outside writable roots cannot be written either. Ordinary project files and other Git data remain writable; `filesystem.allowGitConfig: true` permits `.git/config` only, leaving hooks and explicit `denyWrite` restrictions protected.

`filesystem.allowWrite` and `denyWrite` take literal paths: relative paths resolve from the Chat Folder, independently of Bash `cwd`, and `~/` from your home directory. A `cwd` outside the Folder and additional directories goes through ordinary approval while the sandbox keeps the same writable roots. Deny rules and sensitive paths take precedence. Reads are not restricted, except that sandboxed commands cannot traverse sensitive symlinks, even to read; this prevents aliases from bypassing write protection. Configured deny paths and the user settings file also protect their resolved targets.

## Network

HTTP and HTTPS (CONNECT) go through a local proxy that sandboxed commands receive in the standard proxy environment variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`); commands that ignore them cannot connect directly. Wildcards such as `*.github.com` match subdomains, not the domain itself. A denied destination is named in the command output, so you know which domain to add. One authenticated loopback proxy stays bound for the lifetime of each Tickhost; each Shell keeps the allowlist it started with.

A failed sandboxed command whose output indicates a permission denial gets a note that the sandbox probably blocked it, which suggests `dangerouslyDisableSandbox` only when escapes are allowed. Its exit status is kept, and no command is retried automatically.

## Known limitation

A detached descendant can outlive a Tickhost restart. Its OS sandbox still permits the former proxy's loopback port, so it may reach a different service that later binds that port.
