import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export function compareApi(baseline, current) {
  const before = new Set(baseline.trim().split("\n").filter(Boolean));
  const after = new Set(current.trim().split("\n").filter(Boolean));
  return {
    removed: [...before].filter(line => !after.has(line)).sort(),
    added: [...after].filter(line => !before.has(line)).sort(),
  };
}

function typeIdentity(documents, name) {
  const [crate, ...segments] = name.split("::");
  const document = Object.values(documents).find(doc => doc.index[doc.root]?.name === crate);
  if (!document) return undefined;
  let item = document.index[document.root];
  for (const [position, segment] of segments.entries()) {
    item = item?.inner.module?.items.map(id => document.index[id]).find(child =>
      child?.visibility === "public" && (child.inner.use?.name ?? child.name) === segment);
    if (item?.inner.use) {
      const target = item.inner.use.id;
      if (position === segments.length - 1) {
        const path = document.paths[target];
        return path && `${path.kind} ${path.path.join("::")}`;
      }
      item = document.index[target];
    }
  }
  const path = item ? document.paths[item.id] : Object.values(document.paths).find(path =>
    path.crate_id === 0 && path.path.join("::") === name);
  return path && `${path.kind} ${path.path.join("::")}`;
}

export function compareApiGroup(baseline, current, group, documents = {}) {
  const aliases = Object.entries(group.aliases).sort(([left], [right]) => right.length - left.length);
  for (const snapshots of [baseline, current]) {
    const definitions = new Map();
    for (const snapshot of Object.values(snapshots)) {
      for (const line of snapshot.split("\n")) {
        const name = /^pub (?:struct|enum|union|type|trait) ([\w:]+)(?:\b|$)/.exec(line)?.[1];
        if (!name) continue;
        const canonical = group.aliases[name] ?? name;
        if (!definitions.has(canonical)) definitions.set(canonical, new Set());
        definitions.get(canonical).add(name);
      }
    }
    for (const [canonical, names] of definitions) {
      if (names.size > 1) throw new Error(`Cannot normalize independent definitions of ${canonical}: ${[...names].join(", ")}`);
    }
  }
  const normalize = (snapshots, label) => {
    const snapshotText = Object.values(snapshots).join("\n");
    const verifiedAliases = aliases.flatMap(([alias, canonical]) => {
      const escaped = alias.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
      const pattern = new RegExp(`(?<![\\w:])${escaped}\\b`, "g");
      if (!pattern.test(snapshotText)) return [];
      const identity = typeIdentity(documents[label] ?? {}, alias);
      if (!identity || identity !== typeIdentity(documents[label] ?? {}, canonical)) {
        throw new Error(`${label}: unverified equivalent paths ${alias} and ${canonical}`);
      }
      return [[pattern, canonical]];
    });
    return snapshotText.trim().split("\n").map(line => {
      for (const [pattern, canonical] of verifiedAliases) line = line.replace(pattern, canonical);
      const exported = Object.values(group.exports).flat().map(name => group.aliases[name] ?? name);
      return exported.some(name => line === `pub use ${name}`) ? line.replace("pub use ", "pub struct ") : line;
    }).join("\n");
  };
  const diff = compareApi(normalize(baseline, "baseline"), normalize(current, "current"));
  for (const [pkg, exports] of Object.entries(group.exports)) {
    for (const name of exports) {
      const exposes = snapshot => snapshot.split("\n").some(line => line === `pub use ${name}` || line === `pub struct ${name}`);
      if (!exposes(baseline[pkg] ?? "")) throw new Error(`Baseline does not export ${name} from ${pkg}`);
      if (!exposes(current[pkg] ?? "")) diff.removed.push(`export ${name} from ${pkg}`);
    }
  }
  diff.removed.sort();
  return diff;
}

function checkPublicApi() {
  const root = fileURLToPath(new URL("../", import.meta.url));
  const baseline = JSON.parse(readFileSync(new URL("./public-api-baseline.json", import.meta.url), "utf8"));
  const version = execFileSync("cargo", [`+${baseline.toolchain}`, "public-api", "--version"], { encoding: "utf8" }).trim();
  if (version !== `cargo-public-api ${baseline.generatorVersion}`) {
    throw new Error(`Expected cargo-public-api ${baseline.generatorVersion}; install with cargo install --locked cargo-public-api --version ${baseline.generatorVersion}`);
  }
  const commit = execFileSync("git", ["rev-parse", `${baseline.release}^{commit}`], { cwd: root, encoding: "utf8" }).trim();
  if (commit !== baseline.commit) throw new Error(`Release ${baseline.release} no longer matches the recorded API baseline commit`);
  const directory = join(root, "target", "public-api");
  mkdirSync(directory, { recursive: true });
  const run = mkdtempSync(join(directory, "check-"));
  const source = join(run, "release");
  mkdirSync(source);
  const archive = join(run, "release.tar");
  execFileSync("git", ["archive", "--format=tar", "--output", archive, commit], { cwd: root });
  execFileSync("tar", ["-xf", archive, "-C", source]);
  rmSync(archive);
  let changed = false;
  const collected = { baseline: {}, current: {} };
  const documents = { baseline: {}, current: {} };
  try {
    for (const pkg of baseline.packages) {
      const snapshots = [];
      for (const [label, checkout] of [["baseline", source], ["current", root]]) {
        // The immutable release source repeats diagnostics fixed since then; show its build output only when it fails.
        const isBaseline = label === "baseline";
        let output;
        try {
          output = execFileSync("cargo", [`+${baseline.toolchain}`, "public-api", "--manifest-path", join(checkout, "Cargo.toml"), "--target-dir", join(directory, "build"), "-p", pkg, "--all-features", "--omit", "blanket-impls", "--color", "never"], {
            cwd: checkout, encoding: "utf8", maxBuffer: 32 * 1024 * 1024, stdio: ["ignore", "pipe", isBaseline ? "pipe" : "inherit"],
          });
        } catch (error) {
          if (isBaseline && error.stderr) process.stderr.write(error.stderr);
          throw error;
        }
        writeFileSync(join(run, `${pkg}.${label}.txt`), output);
        collected[label][pkg] = output;
        if (baseline.groups?.some(group => group.packages.includes(pkg))) {
          const crate = /^pub mod (\w+)$/m.exec(output)?.[1];
          if (!crate) throw new Error(`Cannot find the crate root in ${pkg} ${label} API`);
          const rustdoc = readFileSync(join(directory, "build", "doc", `${crate}.json`), "utf8");
          writeFileSync(join(run, `${pkg}.${label}.json`), rustdoc);
          documents[label][pkg] = JSON.parse(rustdoc);
        }
        snapshots.push(output);
      }
      if (baseline.groups?.some(group => group.packages.includes(pkg))) continue;
      const diff = compareApi(...snapshots);
      if (diff.removed.length || diff.added.length) {
        changed = true;
        console.error(`${pkg}: public API differs from ${baseline.release}\n${diff.removed.map(line => `- ${line}`).concat(diff.added.map(line => `+ ${line}`)).join("\n")}`);
      } else {
        console.log(`${pkg}: public API matches ${baseline.release}`);
      }
    }
    for (const group of baseline.groups ?? []) {
      const snapshots = label => Object.fromEntries(group.packages.map(pkg => [pkg, collected[label][pkg]]));
      const diff = compareApiGroup(snapshots("baseline"), snapshots("current"), group, documents);
      if (diff.removed.length || diff.added.length) {
        changed = true;
        console.error(`${group.packages.join(" + ")}: public API differs from ${baseline.release}\n${diff.removed.map(line => `- ${line}`).concat(diff.added.map(line => `+ ${line}`)).join("\n")}`);
      } else {
        console.log(`${group.packages.join(" + ")}: public API matches ${baseline.release} with preserved exports`);
      }
    }
  } finally {
    rmSync(source, { recursive: true, force: true });
    console.log(`Public API snapshots: ${run}`);
  }
  if (changed) process.exitCode = 1;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    checkPublicApi();
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
