# 指令

[Tickloop](../../README.zh-CN.md) · [English](../instructions.md) | 简体中文

Agent 按以下顺序读取 Markdown 中的常驻指令（后读到的文件权重更高）：

1. 你自己的、对每个项目都生效的指令：`~/.tickloop/AGENTS.md`。没有这个文件时，改用 `~/.claude/CLAUDE.md` 和 `~/.claude/rules/*.md`。
2. 从文件系统根目录一直到工作目录的每一层：该目录的 `AGENTS.md`；没有 `AGENTS.md` 时，读该目录的 `CLAUDE.md`、`.claude/CLAUDE.md` 和 `.claude/rules/*.md`。因此父目录中的 `AGENTS.md` 对它下面的每个项目都生效。
3. 目录中的 `CLAUDE.local.md`（你自己的、不进 Git 的笔记）总会被读取。

要让某个目录的 `CLAUDE.md` 不被读取，在该目录放一个 `AGENTS.md` 即可，空文件也够。同样，只要存在 `~/.tickloop/AGENTS.md`，哪怕是空的，就不会再读 `~/.claude`。frontmatter 里带 `paths` 的规则文件会被略过。写 `@path` 可以引入另一个文件（`@docs/style.md`、`@~/notes.md`）；项目里的文件只能引入工作目录之内的文件，你自己的指令可以引入任意文件。这些文件在对话开始时读取，并在 `/compact` 之后再读一次。

要复用为其他工具写的指令，从 `~/.tickloop/AGENTS.md` 引入（例如 `@~/.codex/AGENTS.md`），或在项目中加一个 `AGENTS.md` 来引入已有文件（例如 `@GEMINI.md`）。
