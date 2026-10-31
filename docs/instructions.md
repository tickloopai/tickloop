# Instructions

[Tickloop](../README.md) · English | [简体中文](zh-CN/instructions.md)

The agent follows standing instructions from Markdown files, read in this order (later files weigh more):

1. Your own instructions for every project: `~/.tickloop/AGENTS.md`. Without it, `~/.claude/CLAUDE.md` and `~/.claude/rules/*.md` are used instead.
2. Each directory from the top of the filesystem down to the working directory: its `AGENTS.md`, or, in a directory without one, its `CLAUDE.md`, `.claude/CLAUDE.md` and `.claude/rules/*.md`. An `AGENTS.md` in a parent folder therefore applies to every project below it.
3. A directory's `CLAUDE.local.md`, your private notes that stay out of Git, is always read.

To keep a directory's `CLAUDE.md` from being read, put an `AGENTS.md` there; an empty one is enough. Likewise, any `~/.tickloop/AGENTS.md`, even an empty one, keeps `~/.claude` from being read. A rule file with `paths` in its frontmatter is left out. Write `@path` to bring in another file (`@docs/style.md`, `@~/notes.md`); a project's files can only bring in files inside the working directory, while your own instructions can bring in any file. Files are read when a conversation starts and again after `/compact`.

To reuse instructions written for another tool, bring them in from `~/.tickloop/AGENTS.md` (for example `@~/.codex/AGENTS.md`), or add an `AGENTS.md` to the project that brings in its existing file (for example `@GEMINI.md`).
