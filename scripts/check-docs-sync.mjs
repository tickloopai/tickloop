import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const fenceLine = /^ {0,3}(`{3,}|~{3,})/;

function fenceMarker(line) {
  const match = fenceLine.exec(line);
  return match ? match[1][0] : null;
}

// Walk lines once. A fence toggles at a line whose marker starts the line, so a
// heading or a sample fence written inside a block is not a second heading or block.
function scan(markdown) {
  const headings = [];
  let codeBlocks = 0;
  let inFence = false;
  for (const line of markdown.split("\n")) {
    if (fenceMarker(line)) {
      if (!inFence) codeBlocks += 1;
      inFence = !inFence;
      continue;
    }
    if (inFence) continue;
    const heading = /^(#{1,6})\s+\S/.exec(line);
    if (heading) headings.push(heading[1].length);
  }
  return { headings, codeBlocks };
}

export function headingOutline(markdown) {
  return scan(markdown).headings;
}

export function codeBlockCount(markdown) {
  return scan(markdown).codeBlocks;
}

export function compareDocs(english, chinese) {
  const left = scan(english);
  const right = scan(chinese);
  const failures = [];
  if (left.headings.join(",") !== right.headings.join(",")) {
    failures.push(`headings en [${left.headings.join(",")}] zh [${right.headings.join(",")}]`);
  }
  if (left.codeBlocks !== right.codeBlocks) {
    failures.push(`code blocks en ${left.codeBlocks} zh ${right.codeBlocks}`);
  }
  return failures;
}

export function checkDocsSync(root) {
  const failures = [];
  const pairs = [["README.md", "README.zh-CN.md"]];
  const docs = join(root, "docs");
  if (!existsSync(docs)) {
    failures.push("docs/ is missing");
    return failures;
  }
  const englishPages = readdirSync(docs).filter(name => name.endsWith(".md")).sort();
  const chineseDir = join(docs, "zh-CN");
  const chinesePages = existsSync(chineseDir)
    ? readdirSync(chineseDir).filter(name => name.endsWith(".md")).sort()
    : [];
  for (const name of englishPages) {
    if (chinesePages.includes(name)) pairs.push([`docs/${name}`, `docs/zh-CN/${name}`]);
    else failures.push(`missing docs/zh-CN/${name}`);
  }
  for (const name of chinesePages) {
    if (!englishPages.includes(name)) failures.push(`docs/zh-CN/${name} has no English page`);
  }
  for (const [english, chinese] of pairs) {
    const englishPath = join(root, english);
    const chinesePath = join(root, chinese);
    if (!existsSync(englishPath) || !existsSync(chinesePath)) {
      failures.push(`missing pair ${english} / ${chinese}`);
      continue;
    }
    for (const failure of compareDocs(readFileSync(englishPath, "utf8"), readFileSync(chinesePath, "utf8"))) {
      failures.push(`${english}: ${failure}`);
    }
  }
  return failures.sort();
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = fileURLToPath(new URL("../", import.meta.url));
  const failures = checkDocsSync(root);
  if (failures.length) {
    console.error(failures.join("\n"));
    process.exitCode = 1;
  } else {
    console.log("English and Chinese docs are in sync.");
  }
}
