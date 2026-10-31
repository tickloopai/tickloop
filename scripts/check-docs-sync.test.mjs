import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { checkDocsSync, codeBlockCount, compareDocs, headingOutline } from "./check-docs-sync.mjs";

const root = fileURLToPath(new URL("../", import.meta.url));

test("the repository's English and Chinese manuals match", () => {
  assert.deepEqual(checkDocsSync(root), []);
});

test("heading levels must match in order, and fences hide sample headings", () => {
  const english = "# Title\n\n```md\n## Hidden\n```\n\n## Section\n";
  const chinese = "# 标题\n\n```md\n## 隐藏\n```\n\n## 章节\n";
  assert.deepEqual(headingOutline(english), [1, 2]);
  assert.equal(codeBlockCount(english), 1);
  assert.deepEqual(compareDocs(english, chinese), []);
  assert.deepEqual(compareDocs("# Title\n\n## Section\n", "# 标题\n\n## 章节\n### 多出来\n"), [
    "headings en [1,2] zh [1,2,3]",
  ]);
  assert.deepEqual(compareDocs("# Title\n\n```sh\necho\n```\n", "# 标题\n"), [
    "code blocks en 1 zh 0",
  ]);
});

test("each English page needs a Chinese page with the same file name", () => {
  const directory = mkdtempSync(join(tmpdir(), "docs-sync-"));
  writeFileSync(join(directory, "README.md"), "# Tickloop\n");
  writeFileSync(join(directory, "README.zh-CN.md"), "# Tickloop\n");
  mkdirSync(join(directory, "docs"));
  mkdirSync(join(directory, "docs/zh-CN"));
  writeFileSync(join(directory, "docs/only.md"), "# Only\n");
  writeFileSync(join(directory, "docs/zh-CN/extra.md"), "# 多余\n");
  assert.deepEqual(checkDocsSync(directory), [
    "docs/zh-CN/extra.md has no English page",
    "missing docs/zh-CN/only.md",
  ]);
});
