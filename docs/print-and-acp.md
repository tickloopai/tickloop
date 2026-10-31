# Print and ACP

[Tickloop](../README.md) · English | [简体中文](zh-CN/print-and-acp.md)

```sh
tickloop -p "Summarize this project"                 # --print is an alias; --p is not supported
printf '%s' 'Explain this text' | tickloop -p -
tickloop -p "Review this change" --output-format json --timeout 300
tickloop -p "Run the checks" --output-format stream-json
tickloop acp                                        # ACP JSON-RPC over stdin/stdout
tickloop acp --mode fullAccess                      # sessions may select Full Access (also the starting mode)
```

Configure the provider with `/provider` first. These clients use the same saved model, permission rules and Agent runtime as the Console, without opening a terminal UI. An unknown working directory is refused: trust it interactively first, or explicitly assert trust for this invocation with `--working-directory-is-trusted` (does not save trust).

Print mode defaults to Auto with human prompts denied, not Full Access. `text` writes only the final answer; `json` writes a versioned result; `stream-json` writes versioned JSONL events and a final result. Diagnostics go to stderr. Exit codes are 0 (success), 1 (execution/unknown outcome/cleanup failure), 2 (arguments), 3 (human input required), 124 (timeout), 130 (SIGINT), and 143 (SIGTERM). `--no-session-persistence` disables saving the new Chat; Print does not resume an existing Chat.

The initial managed profiles do not offer structured questions, Plan approvals, Subagents, independent/scheduled goals, Tick subscriptions or persistent Artifact feedback. Ordinary tool denials need not fail the whole task. A completed turn is settled and its remaining Shells and hosted Ticks are cleaned before success is reported; failures and unknown outcomes are not silently retried. Client crashes are covered by an expiring daemon control lease. Ordinary Console detach behavior is unchanged.

## ACP

Configure an ACP-capable editor to launch `tickloop acp`. Stable ACP v1 supports successive prompts, text/tool updates, live thinking (up to 1 MiB per turn, then cut off), the session title and context usage (sent when changed at the start and end of each turn and on load), one-time permission answers, cancellation, resource references/embedded text, and inline images when the selected model supports them. Resource URIs are provenance, not permission to read or fetch them. Session-scoped configuration offers the real models of every signed-in provider (grouped by provider when there are several, as in `/model`; each is named `provider/model`, and a bare model id names the current provider's), Auto/Ask within the startup ceiling (plus Full Access when launched with `--mode fullAccess`; `--mode` is both the starting mode and the ceiling, default auto), and the selected model's effort levels (auto or the levels it offers; session-only, never written to settings, and back to the settings default on load); advertised slash commands are actual user-invocable Skills. Persistent ACP sessions support listing and exclusive loading with public history replay; loading never runs a model, tool or MCP server and cannot take over a live Chat or ordinary Console history. Print still has no resume mode.

Client-provided stdio MCP servers are validated at session creation and start only on the first prompt, after mandatory one-time human approval (explicit Deny still wins, including in Full Access). Servers use a clean minimal environment plus explicitly supplied variables, and remain available across prompts until release/EOF/lease expiry. argv/env are not recorded in public history; argv is shown transiently to the supplying client for approval, so put secrets in env, not argv. MCP processes are not sandboxed; a supervisor cleans their process groups, but cannot guarantee cleanup of malicious `setsid` escapes or simultaneous supervisor termination. MCP tools require flat object input schemas; top-level composition and references are rejected. HTTP/SSE MCP and client filesystem/terminal execution are not supported.

A client's `cwd` is not itself a trust assertion. Prompts are bounded to 64 blocks, 256 KiB of text, 1 MiB of embedded resource text and four images (16 MiB aggregate base64); protocol frames and queues are also bounded. stdout is exclusively ACP protocol traffic. ACP is an AgentAccess client, not a Tick type.
