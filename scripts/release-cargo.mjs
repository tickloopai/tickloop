import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { lstatSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const manifest = "distribution/cargo/Cargo.toml";
const target = "target/cargo-launcher";
const sourceFiles = ["Cargo.toml", "Cargo.lock", "LICENSE", "README.md", "src/lib.rs", "src/main.rs", "src/bin/tl.rs", "src/bin/tickhost.rs", "src/bin/tick-reader.rs"];
export const packageFiles = [...sourceFiles, "Cargo.toml.orig", ".cargo_vcs_info.json"];

export function auditFiles(files) {
  if (new Set(files).size !== files.length || files.some(file => !packageFiles.includes(file))) throw new Error("Cargo package contains unexpected files; private application source must never be published.");
  for (const file of [...sourceFiles, "Cargo.toml.orig"]) if (!files.includes(file)) throw new Error(`Cargo package is missing ${file}.`);
}

function cargo(args) {
  const result = spawnSync("cargo", args, { cwd: root, encoding: "utf8", maxBuffer: 8 * 1024 * 1024, timeout: 10 * 60 * 1000 });
  if (result.error || result.status !== 0) throw new Error(`Cargo ${args[0]} failed: ${result.error?.message || result.stderr}`);
  if (result.stderr) process.stderr.write(result.stderr);
  return result.stdout.trim();
}

export function validateMetadata(value, workspaceVersion) {
  if (value.packages.length !== 1 || value.packages[0].name !== "tickloop" || value.packages[0].version !== workspaceVersion) throw new Error("The standalone tickloop package must match the product release version.");
  const metadata = value.packages[0];
  if (metadata.dependencies.length !== 0 || value.workspace_members.length !== 1) throw new Error("The public launcher must not depend on the private workspace or other source packages.");
  const bins = metadata.targets.filter(target => target.kind.includes("bin")).map(target => target.name).sort();
  if (JSON.stringify(bins) !== JSON.stringify(["tick-reader", "tickhost", "tickloop", "tl"])) throw new Error("The Cargo launcher must install all four public entries.");
  return metadata.version;
}

async function publishedVersion(version, fetcher = fetch) {
  const response = await fetcher(`https://crates.io/api/v1/crates/tickloop/${version}`, {
    headers: { "User-Agent": "Tickloop release automation (https://tickloop.ai)" },
    signal: AbortSignal.timeout(30_000),
  });
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`Cannot inspect the published crate: HTTP ${response.status}`);
  return (await response.json()).version;
}

export async function publishImmutable(version, checksum, upload, fetcher = fetch) {
  const existing = await publishedVersion(version, fetcher);
  if (existing) {
    if (existing.checksum !== checksum || existing.yanked) throw new Error(`tickloop ${version} already exists with different content or is yanked; publish a new version.`);
    return "already-published";
  }
  await upload();
  for (let attempt = 0; attempt < 15; attempt++) {
    const published = await publishedVersion(version, fetcher);
    if (published) {
      if (published.checksum !== checksum || published.yanked) throw new Error("Published Cargo checksum does not match the checked package.");
      return "published";
    }
    await new Promise(resolve => setTimeout(resolve, 2000));
  }
  throw new Error("Cargo upload completed but registry verification timed out; inspect the version before rerunning.");
}

async function main() {
  const [mode, ...extra] = process.argv.slice(2);
  if (extra.length || !["--check", "--publish"].includes(mode)) throw new Error("Usage: node scripts/release-cargo.mjs --check|--publish");
  const workspace = readFileSync(join(root, "Cargo.toml"), "utf8");
  const workspaceVersion = workspace.match(/\[workspace\.package\][\s\S]*?^version = "([^"]+)"/m)?.[1];
  if (!workspaceVersion) throw new Error("Workspace product version is missing.");
  const metadata = JSON.parse(cargo(["metadata", "--locked", "--no-deps", "--format-version", "1", "--manifest-path", manifest]));
  const version = validateMetadata(metadata, workspaceVersion);
  if (mode === "--publish" && (process.env.GITHUB_ACTIONS !== "true" || process.env.GITHUB_REF_NAME !== `v${version}` || !process.env.CARGO_REGISTRY_TOKEN)) throw new Error("Publishing requires the matching vX.Y.Z Actions run and CARGO_REGISTRY_TOKEN.");
  for (const file of sourceFiles) if (!lstatSync(join(root, "distribution/cargo", file)).isFile()) throw new Error(`Refusing a non-regular public source file: ${file}`);
  const base = ["--locked", "--manifest-path", manifest, "--target-dir", target];
  const dirty = mode === "--check" ? ["--allow-dirty"] : [];
  const files = cargo(["package", ...base, ...dirty, "--list"]).split("\n");
  auditFiles(files);
  cargo(["package", ...base, ...dirty]);
  const archive = join(root, target, "package", `tickloop-${version}.crate`);
  const listing = spawnSync("tar", ["-tzf", archive], { encoding: "utf8", maxBuffer: 1024 * 1024 });
  if (listing.status !== 0) throw new Error("Cannot inspect the packaged crate archive.");
  const prefix = `tickloop-${version}/`;
  const archived = listing.stdout.trim().split("\n");
  if (archived.some(file => !file.startsWith(prefix))) throw new Error("Unexpected crate archive root.");
  auditFiles(archived.map(file => file.slice(prefix.length)));
  const checksum = createHash("sha256").update(readFileSync(archive)).digest("hex");
  if (mode === "--check") {
    cargo(["publish", ...base, ...dirty, "--registry", "crates-io", "--dry-run"]);
    process.stdout.write(`Checked tickloop ${version}: ${files.length} installer-only files, SHA-256 ${checksum}.\n`);
    return;
  }
  const result = await publishImmutable(version, checksum, () => cargo(["publish", ...base, "--registry", "crates-io"]));
  process.stdout.write(`tickloop ${version}: ${result}, verified SHA-256 ${checksum}.\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch(error => { process.stderr.write(`${error.message}\n`); process.exitCode = 1; });
}
