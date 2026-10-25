import { execFileSync } from "node:child_process";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HARNESS_PACKAGES = new Set([
  "tickloop-harness", "tickloop-harness-runtime", "tickloop-harness-protocol",
  "tickloop-harness-transcript", "tickloop-harness-model", "tickloop-harness-tools",
  "tickloop-harness-policy", "tickloop-harness-cron", "tickloop-harness-artifacts",
  "tickloop-harness-base", "tickloop-harness-test-support",
]);
const ACCESS_PACKAGES = new Set(["tickloop-code-access"]);

export function checkAgentAccessBoundary(metadata) {
  const members = new Set(metadata.workspace_members);
  const packages = new Map(metadata.packages.filter(pkg => members.has(pkg.id)).map(pkg => [pkg.name, pkg]));
  const failures = [];
  for (const name of packages.keys()) {
    if (!HARNESS_PACKAGES.has(name) && !name.startsWith("tickloop-harness-")) continue;
    const pending = [[name]];
    const visited = new Set();
    while (pending.length) {
      const path = pending.shift();
      const current = path.at(-1);
      if (visited.has(current)) continue;
      visited.add(current);
      if (ACCESS_PACKAGES.has(current)) {
        failures.push(`${path.join(" -> ")}: Harness must not depend on AgentAccess`);
        continue;
      }
      for (const dependency of packages.get(current)?.dependencies ?? []) {
        if (packages.has(dependency.name)) pending.push([...path, dependency.name]);
      }
    }
  }
  return failures.sort();
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = fileURLToPath(new URL("../", import.meta.url));
  const metadata = JSON.parse(execFileSync("cargo", ["metadata", "--locked", "--no-deps", "--format-version", "1"], { cwd: root, encoding: "utf8" }));
  const failures = checkAgentAccessBoundary(metadata);
  if (failures.length) {
    console.error(failures.join("\n"));
    process.exitCode = 1;
  } else {
    console.log("Harness has no AgentAccess dependency.");
  }
}
