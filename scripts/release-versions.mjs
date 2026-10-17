import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const versionPattern = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const maximumTagLength = 32;

export function parseVersion(version) {
  if (!versionPattern.test(version)) throw new Error(`Invalid release version ${JSON.stringify(version)}; expected X.Y.Z without leading zeroes or prerelease suffixes.`);
  return version.split(".").map(BigInt);
}

export function compareVersions(left, right) {
  const a = parseVersion(left), b = parseVersion(right);
  const index = a.findIndex((part, index) => part !== b[index]);
  return index < 0 ? 0 : a[index] > b[index] ? 1 : -1;
}

export function nextVersion(product, requested) {
  const [major, minor, patch] = parseVersion(product.version);
  const version = requested || `${major}.${minor}.${patch + 1n}`;
  if (compareVersions(version, product.version) <= 0) throw new Error(`${product.label} VERSION must be newer than ${product.version}.`);
  if (`${product.tagPrefix}${version}`.length > maximumTagLength) throw new Error(`${product.label} release tag exceeds ${maximumTagLength} characters.`);
  return version;
}

function section(text, name) {
  const heading = `[${name}]`;
  const start = text.split("\n").findIndex(line => line.trim() === heading);
  if (start < 0) throw new Error(`Missing ${heading}`);
  const lines = text.split("\n");
  const following = lines.slice(start + 1).findIndex(line => /^\s*\[/.test(line));
  return { lines, start: start + 1, end: following < 0 ? lines.length : start + 1 + following };
}

function tomlValue(text, name, key) {
  const { lines, start, end } = section(text, name);
  const matches = lines.slice(start, end).map(line => line.match(new RegExp(`^\\s*${key}\\s*=\\s*"([^"]+)"\\s*(?:#.*)?$`))).filter(Boolean);
  if (matches.length !== 1) throw new Error(`Expected one ${key} in [${name}]`);
  return matches[0][1];
}

function withTomlVersion(text, name, version) {
  const { lines, start, end } = section(text, name);
  tomlValue(text, name, "version");
  for (let index = start; index < end; index++) {
    lines[index] = lines[index].replace(/^(\s*version\s*=\s*")[^"]+("\s*(?:#.*)?)$/, (_, before, after) => `${before}${version}${after}`);
  }
  return lines.join("\n");
}

export function readProducts(root, channel, selection = "") {
  const read = path => readFileSync(join(root, path), "utf8");
  if (channel === "binary") {
    const version = tomlValue(read("Cargo.toml"), "workspace.package", "version");
    const launcherManifest = "distribution/cargo/Cargo.toml";
    if (tomlValue(read(launcherManifest), "package", "name") !== "tickloop" || tomlValue(read(launcherManifest), "package", "version") !== version) throw new Error("The Cargo launcher must be named tickloop and match the workspace version.");
    return [{ id: "binary", label: "Tickloop", scope: "tickloop", kind: "workspace", manifest: "Cargo.toml", launcherManifest, version, tagPrefix: "v", workflow: ".github/workflows/release.yml" }];
  }
  if (channel === "artifacts" || channel === "site") {
    const directory = channel === "artifacts" ? "artifact" : "site";
    const manifest = `${directory}/package.json`;
    return [{ id: channel, label: channel === "artifacts" ? "Artifacts" : "Site", scope: channel, kind: "node", directory, manifest, version: JSON.parse(read(manifest)).version, tagPrefix: `${channel}-v`, workflow: `.github/workflows/${channel}-deploy.yml` }];
  }
  if (channel !== "ticks") throw new Error("Unknown release channel");
  const available = readdirSync(join(root, "ticks"), { withFileTypes: true }).filter(entry => entry.isDirectory()).map(entry => entry.name).sort();
  const names = selection.trim() ? selection.trim().split(/\s+/) : available;
  if (!names.length || new Set(names).size !== names.length || names.some(name => !/^[a-z][a-z0-9-]*$/.test(name) || !available.includes(name))) throw new Error(`TICK_PACKAGES must select distinct installed source packages: ${available.join(" ")}`);
  return names.sort().map(name => {
    const manifest = `ticks/${name}/Cargo.toml`;
    const version = tomlValue(read(manifest), "package", "version");
    const packageManifest = `ticks/${name}/tick-package.json`;
    const metadata = JSON.parse(read(packageManifest));
    if (metadata.name !== name || metadata.version !== version) throw new Error(`${packageManifest} must match its Cargo package name and version.`);
    return { id: name, label: name, scope: `tick-${name}`, kind: "tick", manifest, packageManifest, cargoName: tomlValue(read(manifest), "package", "name"), version, tagPrefix: `tick-${name}-v`, workflow: ".github/workflows/release-ticks.yml" };
  });
}

export function versionChanges(root, products) {
  const changes = new Map();
  const read = path => changes.get(path) ?? readFileSync(join(root, path), "utf8");
  const cargoVersions = new Map();
  for (const product of products) {
    parseVersion(product.nextVersion);
    if (product.kind === "node") {
      const manifest = JSON.parse(read(product.manifest));
      const lockPath = `${product.directory}/package-lock.json`;
      const lock = JSON.parse(read(lockPath));
      if (manifest.version !== product.version || lock.version !== product.version || lock.packages[""].version !== product.version) throw new Error(`${product.label} package and lock versions are inconsistent.`);
      manifest.version = lock.version = lock.packages[""].version = product.nextVersion;
      changes.set(product.manifest, JSON.stringify(manifest, null, 2) + "\n");
      changes.set(lockPath, JSON.stringify(lock, null, 2) + "\n");
    } else if (product.kind === "tick") {
      changes.set(product.manifest, withTomlVersion(read(product.manifest), "package", product.nextVersion));
      const manifest = JSON.parse(read(product.packageManifest));
      manifest.version = product.nextVersion;
      changes.set(product.packageManifest, JSON.stringify(manifest, null, 2) + "\n");
      cargoVersions.set(product.cargoName, { before: product.version, after: product.nextVersion });
    } else {
      const workspace = read(product.manifest);
      changes.set(product.manifest, withTomlVersion(workspace, "workspace.package", product.nextVersion));
      changes.set(product.launcherManifest, withTomlVersion(read(product.launcherManifest), "package", product.nextVersion));
      const launcherLock = "distribution/cargo/Cargo.lock";
      const lockText = read(launcherLock);
      if (tomlValue(lockText, "[package]", "name") !== "tickloop" || tomlValue(lockText, "[package]", "version") !== product.version) throw new Error("The Cargo launcher lock version must match the workspace version.");
      changes.set(launcherLock, withTomlVersion(lockText, "[package]", product.nextVersion));
      const { lines, start, end } = section(workspace, "workspace");
      const members = lines.slice(start, end).join("\n").match(/\bmembers\s*=\s*\[([\s\S]*?)\]/)?.[1];
      if (!members) throw new Error("Workspace members are missing");
      for (const [, member] of members.matchAll(/"([^"]+)"/g)) {
        const directories = member.endsWith("/*")
          ? readdirSync(join(root, member.slice(0, -2)), { withFileTypes: true }).filter(entry => entry.isDirectory()).map(entry => `${member.slice(0, -2)}/${entry.name}`)
          : [member];
        for (const directory of directories) {
          const text = read(`${directory}/Cargo.toml`);
          const { lines, start, end } = section(text, "package");
          if (lines.slice(start, end).some(line => /^\s*version\.workspace\s*=\s*true\s*(?:#.*)?$/.test(line))) cargoVersions.set(tomlValue(text, "package", "name"), { before: product.version, after: product.nextVersion });
        }
      }
    }
  }
  if (cargoVersions.size) {
    const seen = new Set();
    let lock = read("Cargo.lock").split(/(?=^\[\[package\]\])/m).map(block => {
      const name = block.match(/^name = "([^"]+)"$/m)?.[1];
      const version = cargoVersions.get(name);
      if (!version) return block;
      if (/^source =/m.test(block) || block.match(/^version = "([^"]+)"$/m)?.[1] !== version.before) throw new Error(`Unexpected locked package ${name}`);
      seen.add(name);
      return block.replace(/^version = "[^"]+"$/m, `version = "${version.after}"`);
    }).join("");
    for (const [name, version] of cargoVersions) {
      if (!seen.has(name)) throw new Error(`Cargo.lock has no workspace package ${name}`);
      lock = lock.replaceAll(`"${name} ${version.before}"`, `"${name} ${version.after}"`);
    }
    changes.set("Cargo.lock", lock);
  }
  return changes;
}
