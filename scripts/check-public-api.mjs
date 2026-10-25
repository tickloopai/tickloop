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
  try {
    for (const pkg of baseline.packages) {
      const snapshots = [];
      for (const [label, checkout] of [["baseline", source], ["current", root]]) {
        const output = execFileSync("cargo", [`+${baseline.toolchain}`, "public-api", "--manifest-path", join(checkout, "Cargo.toml"), "--target-dir", join(directory, "build"), "-p", pkg, "--all-features", "--color", "never"], {
          cwd: checkout, encoding: "utf8", maxBuffer: 32 * 1024 * 1024, stdio: ["ignore", "pipe", "inherit"],
        });
        writeFileSync(join(run, `${pkg}.${label}.txt`), output);
        snapshots.push(output);
      }
      const diff = compareApi(...snapshots);
      if (diff.removed.length || diff.added.length) {
        changed = true;
        console.error(`${pkg}: public API differs from ${baseline.release}\n${diff.removed.map(line => `- ${line}`).concat(diff.added.map(line => `+ ${line}`)).join("\n")}`);
      } else {
        console.log(`${pkg}: public API matches ${baseline.release}`);
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
