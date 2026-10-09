---
name: artifact-draft
description: Draft, write, build or edit one Tickloop Artifacts artifact the user names by link (/artifact/<id>) or artifact_id, e.g. a pasted "用 Tickloop Artifacts 打开 … 起草这篇 Doc / 做这个 Design 页面" or "Open … with Tickloop Artifacts and draft …" prompt.
---

# Draft into a Tickloop Artifacts artifact

The user created a doc or a design on the web page and wants you to write it. A doc is writing: reports, notes, plans. A design is a visual page: visual design, UI, prototypes, interactive pages. Work only on that artifact, with `ArtifactRead`, `ArtifactEdit` and `ArtifactComment`; this Skill grants no extra permissions, and each edit still goes through its normal approval.

## 1. Find the artifact

Use the `artifact_id` from the prompt. With only a link, take the id from its `/artifact/<artifact_id>` path. Never guess an id; if neither is present, `ArtifactRead` `list` with a title `query` and ask the user when more than one matches.

Call `ArtifactRead` `{"action":"read","artifact_id":"…"}`. The result gives `title`, `format` (`doc` or `design`), `url`, `access_role` and `revision`. With `access_role` `view` you cannot edit: say so and stop.

## 2. Keep the title and the first heading identical

The artifact's name is `title`, and a doc's first `# ` heading shows that same name. Keep the first heading exactly `# <title>` as read returned it, and never write a different H1 to rename it; only the user renames, on the web page. For a design, keep `<title>` equal to the title.

## 3. Write a doc (`format` `doc`)

`read` returns `blocks`, each with `id`, `hash` and `text`, paged by `next_cursor`; read every page before an edit that depends on the whole doc.

Edit with `ArtifactEdit` `{"action":"patch","artifact_id":"…","patches":[…]}`. Each patch copies `target` (a block `id`) and `expected_hash` (that block's `hash`) from the latest read:

- `{"type":"replace","target":…,"expected_hash":…,"text":"…"}`
- `{"type":"insert","target":…,"expected_hash":…,"side":"before"|"after","text":"…"}`
- `{"type":"delete","target":…,"expected_hash":…}`

A blank doc has a single empty block: replace it with the whole draft, starting with `# <title>`. If the doc already has the title heading, insert after it or replace later blocks. Write plain text formatting with `#` headings, paragraphs, lists, tables, quotes, code blocks, links and basic inline marks. On `unsupported_doc_syntax`, rewrite that part more plainly.

## 4. Write a design (`format` `design`)

`read` returns the whole page `source`. Save the whole page with `ArtifactEdit` `{"action":"replace","artifact_id":"…","expected_revision":<revision from read>,"content":"<whole page source>"}`. Use one self-contained page with inline `<style>` and `<script>`; only the library CDN named in the `ArtifactEdit` description may be loaded. Make it fit phone widths (fluid layout, `overflow-x:auto` around wide tables or charts).

## 5. Conflicts and finishing

Saves are live for everyone reading the artifact. On `content_conflict`, someone else changed it: read again and merge your change into the new content; never overwrite. Do not create a new artifact for a draft the user asked you to write into an existing one.

When done, reply with a short summary of what you wrote and the artifact `url`. Use `ArtifactComment` only when you are handling an @tickloop request.
