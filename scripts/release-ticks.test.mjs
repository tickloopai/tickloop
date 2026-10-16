import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const scripts = dirname(fileURLToPath(import.meta.url));
function run(program, args, setup) {
  const result = spawnSync(program, args, { cwd: setup.root, env: setup.env, encoding: "utf8" });
  if (result.error) throw result.error;
  return result;
}
function git(args, setup) {
  const result = run("git", ["-C", setup.market, ...args], setup);
  assert.equal(result.status, 0, result.stderr); return result.stdout.trim();
}
function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), "release-ticks-test-")); t.after(() => rmSync(root, { recursive: true, force: true }));
  const market = join(root, "market"), archives = join(root, "archives"), bin = join(root, "bin"), releases = join(root, "releases"), remote = join(root, "remote.git");
  for (const directory of [market, archives, bin, releases, join(root, "scripts")]) mkdirSync(directory, { recursive: true });
  copyFileSync(join(scripts, "release-ticks.sh"), join(root, "scripts/release-ticks.sh"));
  writeFileSync(join(root, "Cargo.toml"), '[workspace.package]\nversion = "9.9.9"\n');
  for (const [name, version] of [["longbridge", "0.2.1"], ["futu", "0.4.9"]]) {
    mkdirSync(join(root, "ticks", name), { recursive: true });
    writeFileSync(join(root, "ticks", name, "tick-package.json"), JSON.stringify({ name, version, description: `${name} data` }));
  }
  const originalFutu = { name: "futu", version: "0.3.0", description: "Futu", downloads: { "aarch64-apple-darwin": { url: "https://example.test/futu.tar.gz", sha256: "original" } } };
  writeFileSync(join(market, "marketplace.json"), JSON.stringify({ name: "Tickloop", packages: [originalFutu] }) + "\n");
  writeFileSync(join(market, "README.md"), "# Tick packages\n\n## Packages\n\nOld list\n");
  const archiveName = "tick-longbridge-0.2.1-aarch64-apple-darwin.tar.gz";
  writeFileSync(join(archives, archiveName), "the selected package archive\n");
  const mock = `#!/usr/bin/env node
const fs = require('node:fs'), path = require('node:path');
const args = process.argv.slice(2), store = process.env.RELEASE_TEST_STORE;
fs.appendFileSync(process.env.RELEASE_TEST_CALLS, path.basename(process.argv[1])+' '+args.join(' ')+'\\n');
if (path.basename(process.argv[1]) === 'curl') {
  const url = new URL(args.at(-1));
  const parts = url.pathname.split('/');
  fs.copyFileSync(path.join(store, parts.at(-2), parts.at(-1)), args[args.indexOf('-o')+1]);
} else {
  const [kind, action, tag, archive] = args;
  if (kind !== 'release') process.exit(2);
  const target = path.join(store, tag);
  if (action === 'view') { if (!fs.existsSync(target)) { console.error('release not found'); process.exit(1); } }
  else if (action === 'create') { fs.mkdirSync(target); fs.copyFileSync(archive, path.join(target,path.basename(archive))); }
  else if (action === 'download') { const directory = args[args.indexOf('--dir')+1]; fs.mkdirSync(directory,{recursive:true}); fs.copyFileSync(path.join(target,args[args.indexOf('--pattern')+1]),path.join(directory,args[args.indexOf('--pattern')+1])); }
  else process.exit(2);
}
`;
  for (const program of ["gh", "curl"]) { writeFileSync(join(bin, program), mock); chmodSync(join(bin, program), 0o700); }
  const env = { ...process.env, PATH: `${bin}:${process.env.PATH}`, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1", RELEASE_TAG: "tick-longbridge-v0.2.1", RELEASE_COMMIT: "a".repeat(40), GITHUB_REPOSITORY: "griffinqiu/tickloop", RELEASE_TEST_STORE: releases, RELEASE_TEST_CALLS: join(root, "calls.log") };
  delete env.GITHUB_STEP_SUMMARY; delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const setup = { root, market, archives, archiveName, originalFutu, env };
  git(["init", "--bare", "-q", remote], setup); git(["init", "-q", "-b", "main"], setup);
  git(["config", "user.name", "Release Test"], setup); git(["config", "user.email", "release@example.invalid"], setup);
  git(["add", "."], setup); git(["commit", "-qm", "Initial marketplace"], setup); git(["remote", "add", "origin", remote], setup);
  return setup;
}
function release(setup, ...args) { return run("sh", [join(setup.root, "scripts/release-ticks.sh"), ...args], setup); }

test("a package tag publishes and verifies only that package at its own version, independently of the workspace", t => {
  const setup = fixture(t);
  let result = release(setup, "publish", setup.market, setup.archives); assert.equal(result.status, 0, result.stderr);
  const catalog = JSON.parse(readFileSync(join(setup.market, "marketplace.json")));
  assert.deepEqual(catalog.packages.find(p => p.name === "futu"), setup.originalFutu);
  assert.equal(catalog.packages.find(p => p.name === "longbridge").version, "0.2.1");
  assert.match(catalog.packages.find(p => p.name === "longbridge").downloads["aarch64-apple-darwin"].url, /longbridge-v0\.2\.1/);
  const commit = git(["rev-parse", "HEAD"], setup);
  result = release(setup, "publish", setup.market, setup.archives); assert.equal(result.status, 0, result.stderr);
  assert.equal(git(["rev-parse", "HEAD"], setup), commit);
  result = release(setup, "verify", setup.archives); assert.equal(result.status, 0, result.stderr);
  assert.ok(!readFileSync(setup.env.RELEASE_TEST_CALLS, "utf8").includes("futu-v"));
});

test("an existing release with different bytes is rejected without changing the marketplace", t => {
  const setup = fixture(t); assert.equal(release(setup, "publish", setup.market, setup.archives).status, 0);
  const commit = git(["rev-parse", "HEAD"], setup);
  writeFileSync(join(setup.archives, setup.archiveName), "different bytes\n");
  const result = release(setup, "publish", setup.market, setup.archives);
  assert.equal(result.status, 1); assert.match(result.stderr, /already exists with another/);
  assert.equal(git(["rev-parse", "HEAD"], setup), commit);
});

test("unknown, malformed and mismatched package tags fail before any publication", t => {
  const setup = fixture(t); const commit = git(["rev-parse", "HEAD"], setup);
  for (const tag of ["ticks-v0.2.1", "tick-../longbridge-v0.2.1", "tick-missing-v0.2.1", "tick-longbridge-v0.9.0", "tick-longbridge-v01.2.1"]) {
    setup.env.RELEASE_TAG = tag;
    assert.equal(release(setup, "publish", setup.market, setup.archives).status, 1, tag);
  }
  assert.equal(git(["rev-parse", "HEAD"], setup), commit);
});

test("Tick workflows queue package releases and keep their assets separate at a shared commit", () => {
  const workflow = readFileSync(join(scripts, "../.github/workflows/release-ticks.yml"), "utf8");
  assert.ok(workflow.includes("- 'tick-*-v*'")); assert.ok(workflow.includes("queue: max"));
  assert.ok(workflow.includes("tick-packages-${{ github.ref_name }}"));
  assert.ok(!workflow.includes("tick-packages-${{ github.sha }}"));
});

test("rerunning an older package release cannot downgrade a newer marketplace entry", t => {
  const setup = fixture(t);
  const path = join(setup.market, "marketplace.json");
  const catalog = JSON.parse(readFileSync(path));
  catalog.packages.push({ name: "longbridge", version: "0.3.0", description: "Newer", downloads: {} });
  writeFileSync(path, JSON.stringify(catalog) + "\n");
  git(["add", "."], setup); git(["commit", "-qm", "Newer package"], setup);
  const commit = git(["rev-parse", "HEAD"], setup);
  const result = release(setup, "publish", setup.market, setup.archives);
  assert.equal(result.status, 1); assert.match(result.stderr, /refusing to downgrade/);
  assert.equal(git(["rev-parse", "HEAD"], setup), commit);
  assert.equal(JSON.parse(readFileSync(path)).packages.find(p => p.name === "longbridge").version, "0.3.0");
});
