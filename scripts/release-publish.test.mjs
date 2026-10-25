import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { checksumLine, parseArguments, renderFormula, steps, targets } from "./release-publish.mjs";

const read = path => readFileSync(new URL(`../${path}`, import.meta.url), "utf8");

test("publishing takes one release tag, an optional first step and the remote switch", () => {
  assert.deepEqual(parseArguments(["v1.2.3"]), { remote: false, from: "prepare", tag: "v1.2.3" });
  assert.deepEqual(parseArguments(["--from", "tap", "v1.2.3"]), { remote: false, from: "tap", tag: "v1.2.3" });
  assert.deepEqual(parseArguments(["--remote", "v1.2.3"]), { remote: true, from: "prepare", tag: "v1.2.3" });
  for (const argv of [[], ["1.2.3"], ["v1.2"], ["v1.2.3-rc.1"], ["v01.2.3"], ["v1.2.3", "v1.2.4"], ["--from", "deploy", "v1.2.3"], ["--remote", "--from", "tap", "v1.2.3"]]) {
    assert.throws(() => parseArguments(argv), undefined, argv.join(" "));
  }
});

test("local publishing builds every target the workflow, installer and updater know", () => {
  const triples = targets.map(target => target.triple).sort();
  const workflow = read(".github/workflows/release.yml");
  assert.deepEqual([...workflow.matchAll(/- \{ target: ([a-z0-9_-]+), runner:/g)].map(match => match[1]).sort(), triples);
  for (const path of ["scripts/install.sh", "code/updater/src/release.rs", "distribution/cargo/build.rs"]) {
    const source = read(path);
    for (const triple of triples) assert.ok(source.includes(triple), `${path} lacks ${triple}`);
  }
  assert.deepEqual(steps, ["prepare", "build", "publish", "tap", "cargo"]);
});

test("the formula renders every placeholder of the template with its target's checksum", () => {
  const checksums = Object.fromEntries(targets.map((target, index) => [target.triple, String(index).repeat(64)]));
  const formula = renderFormula(read(".github/homebrew/tickloop.rb.in"), "1.2.3", checksums);
  assert.match(formula, /version "1\.2\.3"/);
  for (const target of targets) {
    const url = `releases/download/v1.2.3/tickloop-${target.triple}.tar.gz"\n      sha256 "${checksums[target.triple]}"`;
    assert.ok(formula.includes(url), target.triple);
  }
  assert.throws(() => renderFormula("@UNKNOWN@", "1.2.3", checksums), /placeholder @UNKNOWN@/);
});

test("checksum files match what shasum writes", t => {
  const directory = mkdtempSync(join(tmpdir(), "tickloop-release-checksum-"));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const archive = join(directory, "tickloop-aarch64-apple-darwin.tar.gz");
  writeFileSync(archive, "archive");
  const shasum = spawnSync("shasum", ["-a", "256", "tickloop-aarch64-apple-darwin.tar.gz"], { cwd: directory, encoding: "utf8" });
  assert.equal(checksumLine(archive, "tickloop-aarch64-apple-darwin.tar.gz"), shasum.stdout);
});

test("make release publishes locally unless REMOTE is set, and tags no longer start the workflow", () => {
  const dryRun = (...args) => spawnSync("make", ["--no-print-directory", "-n", ...args], { cwd: new URL("..", import.meta.url), encoding: "utf8" }).stdout;
  assert.match(dryRun("release"), /TICKLOOP_RELEASE_PUBLISH=local node scripts\/release\.mjs binary/);
  assert.match(dryRun("release", "REMOTE=1"), /TICKLOOP_RELEASE_PUBLISH=remote node scripts\/release\.mjs binary/);
  assert.match(dryRun("release-publish", "VERSION=1.2.3", "FROM=tap"), /node scripts\/release-publish\.mjs\s+--from 'tap' 'v1\.2\.3'/);
  assert.match(dryRun("release-publish", "VERSION=1.2.3", "REMOTE=1"), /node scripts\/release-publish\.mjs --remote\s+'v1\.2\.3'/);
  const workflow = read(".github/workflows/release.yml");
  const triggers = workflow.slice(workflow.indexOf("\non:\n"), workflow.indexOf("\npermissions:"));
  assert.match(triggers, /workflow_dispatch:/);
  assert.doesNotMatch(triggers, /push:|tags:/);
});
