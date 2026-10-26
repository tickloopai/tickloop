import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";

export function checkBoundaries(metadata, policy) {
  const members = new Set(metadata.workspace_members);
  const packages = metadata.packages.filter(pkg => members.has(pkg.id));
  const names = new Set(packages.map(pkg => pkg.name));
  const failures = [];
  const usedExceptions = new Set();
  for (const pkg of packages) {
    const layer = policy.layers[pkg.name];
    if (!layer) {
      failures.push(`Unclassified workspace package: ${pkg.name}`);
      continue;
    }
    for (const dependency of pkg.dependencies) {
      if (!names.has(dependency.name)) continue;
      const destination = policy.layers[dependency.name];
      if (policy.allowed[layer]?.includes(destination)) continue;
      const kind = dependency.kind ?? "normal";
      if (kind === "dev" && policy.allowedDev?.[layer]?.includes(destination)) continue;
      const target = dependency.target ?? null;
      const exception = policy.exceptions.find(edge => edge.from === pkg.name
        && edge.to === dependency.name && edge.kind === kind && edge.target === target);
      if (exception) usedExceptions.add(exception);
      if (!exception) failures.push(`${pkg.name} -> ${dependency.name} (${kind}${target ? `, ${target}` : ""}): ${layer} may not depend on ${destination ?? "unclassified"}`);
    }
  }
  for (const edge of policy.exceptions) {
    if (!usedExceptions.has(edge)) failures.push(`Stale exception: ${edge.from} -> ${edge.to} (${edge.kind})`);
  }
  return failures.sort();
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const root = fileURLToPath(new URL("../", import.meta.url));
  const policy = JSON.parse(readFileSync(new URL("./dependency-boundaries.json", import.meta.url), "utf8"));
  const metadata = JSON.parse(execFileSync("cargo", ["metadata", "--locked", "--no-deps", "--format-version", "1"], { cwd: root, encoding: "utf8" }));
  const failures = checkBoundaries(metadata, policy);
  const lightweightTree = execFileSync("cargo", ["tree", "--locked", "-p", "tickloop-harness", "--no-default-features", "-F", "cron,transcript", "-e", "normal", "--prefix", "none", "--format", "{p}"], { cwd: root, encoding: "utf8" });
  const lightweightPackages = new Set(lightweightTree.trim().split("\n").map(line => line.split(" ")[0]));
  for (const name of ["tickloop-harness-base", "image", "system-configuration"]) {
    if (lightweightPackages.has(name)) failures.push(`Lightweight Harness (cron,transcript) must not depend on ${name}`);
  }
  if (failures.length) {
    console.error(failures.join("\n"));
    process.exitCode = 1;
  } else {
    console.log("Workspace dependency boundaries passed.");
  }
}
