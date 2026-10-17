import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { nextVersion, readProducts, versionChanges } from "./release-versions.mjs";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const releaseScript = join(scriptDirectory, "release.mjs");
const expectedRepository = readFileSync(releaseScript, "utf8").match(/^const expectedRepository = "([^"]+)";$/m)[1];
const realGit = spawnSync("which", ["git"], { encoding: "utf8" }).stdout.trim();
const shellQuote = text => `'${text.replaceAll("'", "'\\''")}'`;

function command(program, args, setup, extraEnv = {}) {
  const result = spawnSync(program, args, { cwd: setup.repo, env: { ...setup.env, ...extraEnv }, encoding: "utf8" });
  if (result.error) throw result.error;
  return result;
}

function git(args, setup, status = 0) {
  const result = command("git", args, setup);
  assert.equal(result.status, status, `${args.join(" ")}: ${result.stderr}`);
  return result.stdout.trim();
}

function json(path, value) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, JSON.stringify(value, null, 2) + "\n");
}

function fixture(t, packages = { longbridge: "0.2.0", futu: "0.4.0" }) {
  const base = mkdtempSync(join(tmpdir(), "release-test-"));
  t.after(() => rmSync(base, { recursive: true, force: true }));
  const repo = join(base, "repo"), remote = join(base, "remote.git"), market = join(base, "market.git"), bin = join(base, "bin");
  mkdirSync(repo); mkdirSync(bin);
  const env = { ...process.env, PATH: `${bin}:${process.env.PATH}`, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1", RELEASE_TEST_REPO: repo, RELEASE_TEST_REMOTE: remote, RELEASE_TEST_LOG: join(base, "checks.log"), RELEASE_TEST_PUSH_LOG: join(base, "push.log") };
  for (const name of ["VERSION", "TICK_PACKAGES", "CARGO_TARGET_DIR", "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"]) delete env[name];
  const setup = { repo, remote, market, base, env };
  const ssh = join(base, "ssh.sh");
  writeFileSync(ssh, '#!/bin/sh\ncase "$*" in\n*git-upload-pack*) [ "$RELEASE_TEST_REJECT_READ" != 1 ] || exit 1; exec git-upload-pack "$RELEASE_TEST_REMOTE" ;;\n*git-receive-pack*) [ "$RELEASE_TEST_REJECT_PUSH" != 1 ] || exit 1; exec git-receive-pack "$RELEASE_TEST_REMOTE" ;;\nesac\nexit 1\n');
  chmodSync(ssh, 0o700); env.GIT_SSH_COMMAND = ssh;
  writeFileSync(join(bin, "git"), `#!/bin/sh
if [ "$1" = push ]; then
  count=0
  for argument in "$@"; do case "$argument" in refs/tags/*) count=$((count + 1));; esac; done
  [ "$count" -le 3 ] || exit 77
  printf '%s\\n' "$*" >> "$RELEASE_TEST_PUSH_LOG"
fi
exec ${shellQuote(realGit)} "$@"
`);
  chmodSync(join(bin, "git"), 0o700);
  const check = `#!/bin/sh
printf '%s %s\\n' "$(basename "$0")" "$*" >> "$RELEASE_TEST_LOG"
[ "$RELEASE_TEST_CHECK_FAIL" != 1 ] || exit 1
if [ "$RELEASE_TEST_DIRTY_ROOT" = 1 ]; then printf 'user edit\\n' > "$RELEASE_TEST_REPO/snapshot.txt"; fi
if [ "$RELEASE_TEST_DIRTY_SNAPSHOT" = 1 ]; then printf 'unexpected\\n' > snapshot.txt; fi
if [ "$RELEASE_TEST_MOVE_HEAD" = 1 ]; then git -C "$RELEASE_TEST_REPO" commit --allow-empty -qm 'Concurrent work'; fi
if [ "$RELEASE_TEST_SWITCH_BRANCH" = 1 ]; then git -C "$RELEASE_TEST_REPO" checkout -q -B switched; fi
if [ -n "$RELEASE_TEST_ADD_TAG" ]; then git -C "$RELEASE_TEST_REPO" tag -f "$RELEASE_TEST_ADD_TAG"; fi
`;
  for (const name of ["npm", "make", "cargo"]) { writeFileSync(join(bin, name), check); chmodSync(join(bin, name), 0o700); }
  writeFileSync(join(bin, "curl"), `#!/bin/sh
[ "$RELEASE_TEST_MARKET_FAIL" != 1 ] || exit 1
if [ -n "$RELEASE_TEST_MARKET" ]; then printf '%s\\n' "$RELEASE_TEST_MARKET"; else printf '%s\\n' '{"packages":[]}'; fi
`);
  chmodSync(join(bin, "curl"), 0o700);
  git(["init", "--bare", "-q", remote], setup); git(["init", "--bare", "-q", market], setup); git(["init", "-q", "-b", "main"], setup);
  git(["config", "user.name", "Release Test"], setup); git(["config", "user.email", "release@example.invalid"], setup);
  git(["config", `url.${market}.insteadOf`, "https://github.com/tickloopai/ticks"], setup);
  writeFileSync(join(repo, "Cargo.toml"), '[workspace]\nmembers = ["agent/*", "ticks/*"]\n\n[workspace.package]\nversion = "0.2.0"\nedition = "2024"\n');
  const locked = [];
  for (const name of ["agent", "runtime"]) {
    const directory = join(repo, "agent", name); mkdirSync(directory, { recursive: true });
    writeFileSync(join(directory, "Cargo.toml"), `[package]\nname = "tickloop-${name}"\nversion.workspace = true\n`);
    locked.push(`[[package]]\nname = "tickloop-${name}"\nversion = "0.2.0"\n`);
  }
  for (const [name, version] of Object.entries(packages)) {
    const directory = join(repo, "ticks", name); mkdirSync(directory, { recursive: true });
    writeFileSync(join(directory, "Cargo.toml"), `[package]\nname = "tickloop-tick-${name}"\nversion = "${version}"\n`);
    json(join(directory, "tick-package.json"), { name, version, executable: `bin/tick-${name}` });
    locked.push(`[[package]]\nname = "tickloop-tick-${name}"\nversion = "${version}"\n`);
  }
  locked.push('[[package]]\nname = "external"\nversion = "0.2.0"\nsource = "registry+https://example.invalid"\n');
  writeFileSync(join(repo, "Cargo.lock"), 'version = 4\n\n' + locked.join("\n"));
  mkdirSync(join(repo, "distribution/cargo"), { recursive: true });
  writeFileSync(join(repo, "distribution/cargo/Cargo.toml"), '[package]\nname = "tickloop"\nversion = "0.2.0"\n');
  writeFileSync(join(repo, "distribution/cargo/Cargo.lock"), 'version = 4\n\n[[package]]\nname = "tickloop"\nversion = "0.2.0"\n');
  for (const [directory, version] of [["artifact", "0.1.34"], ["site", "0.1.6"]]) {
    json(join(repo, directory, "package.json"), { name: directory, version });
    json(join(repo, directory, "package-lock.json"), { name: directory, version, lockfileVersion: 3, packages: { "": { name: directory, version } } });
  }
  mkdirSync(join(repo, ".github/workflows"), { recursive: true });
  for (const workflow of ["release", "release-ticks", "artifacts-deploy", "site-deploy"]) writeFileSync(join(repo, `.github/workflows/${workflow}.yml`), "name: Release\n");
  writeFileSync(join(repo, "snapshot.txt"), "business code\n");
  git(["add", "."], setup); git(["commit", "-qm", "Initial snapshot"], setup);
  git(["remote", "add", "origin", `git@github.com:${expectedRepository}.git`], setup);
  setup.initial = git(["rev-parse", "HEAD"], setup);
  return setup;
}

function release(setup, channel, mode, env = {}) {
  return command(process.execPath, [releaseScript, channel, ...(mode ? [mode] : [])], setup, env);
}

const scenarios = [
  { channel: "binary", tags: ["v0.2.1"], paths: ["Cargo.lock", "Cargo.toml", "distribution/cargo/Cargo.lock", "distribution/cargo/Cargo.toml"] },
  { channel: "site", tags: ["site-v0.1.7"], paths: ["site/package-lock.json", "site/package.json"] },
  { channel: "artifacts", tags: ["artifacts-v0.1.35"], paths: ["artifact/package-lock.json", "artifact/package.json"] },
  { channel: "ticks", tags: ["tick-futu-v0.4.1", "tick-longbridge-v0.2.1"], paths: ["Cargo.lock", "ticks/futu/Cargo.toml", "ticks/futu/tick-package.json", "ticks/longbridge/Cargo.toml", "ticks/longbridge/tick-package.json"] },
];

for (const scenario of scenarios) {
  test(`${scenario.channel}: check is read-only; release commits only checked version files and pushes branch plus tags`, t => {
    const setup = fixture(t);
    const check = release(setup, scenario.channel, "--check"); assert.equal(check.status, 0, check.stderr);
    for (const tag of scenario.tags) assert.ok(check.stdout.includes(tag));
    assert.equal(git(["status", "--porcelain"], setup), ""); assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial);
    assert.equal(git(["tag", "--list"], setup), ""); assert.ok(!existsSync(setup.env.RELEASE_TEST_LOG));
    const result = release(setup, scenario.channel); assert.equal(result.status, 0, result.stderr);
    const commit = git(["rev-parse", "HEAD"], setup);
    assert.notEqual(commit, setup.initial); assert.equal(git(["rev-parse", "HEAD^"], setup), setup.initial);
    assert.deepEqual(git(["diff", "--name-only", "HEAD^", "HEAD"], setup).split("\n"), scenario.paths);
    assert.match(git(["log", "-1", "--format=%s"], setup), /^chore\(release\): /);
    assert.equal(git(["status", "--porcelain"], setup), "");
    assert.equal(git(["ls-remote", "--refs", "origin", "refs/heads/main"], setup), `${commit}\trefs/heads/main`);
    for (const tag of scenario.tags) {
      assert.equal(git(["cat-file", "-t", tag], setup), "tag");
      assert.equal(git(["rev-parse", `${tag}^{}`], setup), commit);
      assert.equal(git(["ls-remote", "origin", `refs/tags/${tag}^{}`], setup), `${commit}\trefs/tags/${tag}^{}`);
    }
    assert.equal(git(["worktree", "list", "--porcelain"], setup).split("\n").filter(line => line.startsWith("worktree ")).length, 1);
    if (scenario.channel === "binary") assert.equal(JSON.parse(readFileSync(join(setup.repo, "ticks/longbridge/tick-package.json"))).version, "0.2.0");
    if (scenario.channel === "ticks") assert.match(readFileSync(join(setup.repo, "Cargo.toml"), "utf8"), /version = "0.2.0"/);
  });
  test(`${scenario.channel}: failed build leaves versions, HEAD and tags untouched`, t => {
    const setup = fixture(t); const result = release(setup, scenario.channel, undefined, { RELEASE_TEST_CHECK_FAIL: "1" });
    assert.equal(result.status, 1); assert.match(result.stderr, /no version commit or release tag/);
    assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial); assert.equal(git(["status", "--porcelain"], setup), ""); assert.equal(git(["tag", "--list"], setup), "");
  });
}

test("Tick selection increments only the named package, leaving unrelated package and Agent versions intact", t => {
  const setup = fixture(t); const result = release(setup, "ticks", undefined, { TICK_PACKAGES: "longbridge" });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(git(["tag", "--list"], setup), "tick-longbridge-v0.2.1");
  assert.equal(JSON.parse(readFileSync(join(setup.repo, "ticks/futu/tick-package.json"))).version, "0.4.0");
  assert.equal(readProducts(setup.repo, "binary")[0].version, "0.2.0");
});

test("batch release uses one commit and at most three tags per atomic push", t => {
  const setup = fixture(t, { one: "0.1.0", two: "0.2.0", three: "0.3.0", four: "0.4.0" });
  const result = release(setup, "ticks"); assert.equal(result.status, 0, result.stderr);
  assert.equal(git(["rev-list", "--count", `${setup.initial}..HEAD`], setup), "1");
  assert.equal(git(["tag", "--list"], setup).split("\n").length, 4);
  const pushes = readFileSync(setup.env.RELEASE_TEST_PUSH_LOG, "utf8").trim().split("\n"); assert.equal(pushes.length, 2);
  for (const push of pushes) assert.match(push, /--atomic/);
});

test("VERSION may explicitly advance each product but cannot reuse or downgrade its current version", t => {
  for (const { channel } of scenarios) {
    const setup = fixture(t);
    assert.equal(release(setup, channel, "--check", { VERSION: "2.0.0" }).status, 0);
    for (const version of ["0.0.0", "01.2.3", "1.2", "1.2.3-rc.1", "../1.2.3", "1.2.3;touch bad"]) assert.equal(release(setup, channel, "--check", { VERSION: version }).status, 1);
    assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial);
  }
});

test("local and remote future tags reject stale version files rather than silently skipping a version", t => {
  const setup = fixture(t); git(["tag", "site-v0.1.8"], setup);
  assert.match(release(setup, "site", "--check").stderr, /must be newer/);
  git(["push", "origin", "refs/tags/site-v0.1.8"], setup); git(["tag", "-d", "site-v0.1.8"], setup);
  assert.match(release(setup, "site", "--check").stderr, /must be newer/);
  assert.equal(release(setup, "site", "--check", { VERSION: "0.2.0" }).status, 0);
});

test("refuses dirty, staged and untracked work without creating a release commit", t => {
  for (const state of ["dirty", "staged", "untracked"]) {
    const setup = fixture(t); writeFileSync(join(setup.repo, state === "untracked" ? "extra.txt" : "snapshot.txt"), "user work\n");
    if (state === "staged") git(["add", "snapshot.txt"], setup);
    const before = git(["status", "--porcelain"], setup);
    const result = release(setup, "site"); assert.equal(result.status, 1); assert.match(result.stderr, /clean working tree/);
    assert.equal(git(["status", "--porcelain"], setup), before); assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial);
  }
});

test("rejects detached HEAD, unknown channels, bad selections and unexpected destinations", t => {
  const setup = fixture(t);
  for (const channel of ["docs", "artifact", "constructor"]) assert.match(release(setup, channel).stderr, /Usage:/);
  for (const selection of ["../futu", "missing", "futu futu"]) assert.equal(release(setup, "ticks", "--check", { TICK_PACKAGES: selection }).status, 1);
  git(["checkout", "--detach", "-q"], setup); assert.match(release(setup, "site").stderr, /detached HEAD/);
  git(["checkout", "-q", "main"], setup); git(["remote", "set-url", "--push", "origin", "git@github.com:someone/other.git"], setup);
  assert.match(release(setup, "site").stderr, /exactly one fetch and one push/);
});

test("requires its workflow and consistent version mirrors in the committed snapshot", t => {
  const setup = fixture(t);
  git(["rm", ".github/workflows/artifacts-deploy.yml"], setup); git(["commit", "-qm", "Remove workflow"], setup);
  assert.match(release(setup, "artifacts").stderr, /committed snapshot must contain/);
  json(join(setup.repo, "ticks/futu/tick-package.json"), { name: "futu", version: "9.0.0" });
  git(["add", "."], setup); git(["commit", "-qm", "Mismatch"], setup);
  assert.match(release(setup, "ticks").stderr, /must match its Cargo/);
});

test("root edits, branch moves, unexpected build edits and racing tags stop finalization", t => {
  for (const variable of ["RELEASE_TEST_DIRTY_ROOT", "RELEASE_TEST_MOVE_HEAD", "RELEASE_TEST_SWITCH_BRANCH", "RELEASE_TEST_DIRTY_SNAPSHOT", "RELEASE_TEST_ADD_TAG"]) {
    const setup = fixture(t);
    const result = release(setup, "site", undefined, { [variable]: variable === "RELEASE_TEST_ADD_TAG" ? "site-v0.1.7" : "1" });
    assert.equal(result.status, 1, variable);
    assert.equal(readProducts(setup.repo, "site")[0].version, "0.1.6");
    assert.equal(git(["ls-remote", "--refs", "origin"], setup), "");
    if (variable === "RELEASE_TEST_DIRTY_ROOT") assert.equal(readFileSync(join(setup.repo, "snapshot.txt"), "utf8"), "user edit\n");
  }
});

test("unreadable remotes and marketplace failures stop before version changes", t => {
  for (const [channel, env] of [["site", { RELEASE_TEST_REJECT_READ: "1" }], ["ticks", { RELEASE_TEST_MARKET_FAIL: "1" }], ["ticks", { RELEASE_TEST_MARKET: '{"packages":[{"name":"longbridge","version":"9.0.0"}]}' }]]) {
    const setup = fixture(t); const result = release(setup, channel, undefined, env);
    assert.equal(result.status, 1);
    if (env.RELEASE_TEST_MARKET) assert.match(result.stderr, /not newer than marketplace version 9.0.0/);
    assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial); assert.equal(git(["status", "--porcelain"], setup), "");
  }
});

test("failed push keeps the release commit and tags with recovery instructions, without changing remote refs", t => {
  const setup = fixture(t); const result = release(setup, "site", undefined, { RELEASE_TEST_REJECT_PUSH: "1" });
  assert.equal(result.status, 1); assert.match(result.stderr, /Do not rerun release to bump versions again/);
  assert.equal(git(["rev-parse", "site-v0.1.7^{}"], setup), git(["rev-parse", "HEAD"], setup));
  assert.equal(git(["ls-remote", "--refs", "origin"], setup), ""); assert.equal(git(["status", "--porcelain"], setup), "");
});

test("a rejected branch does not publish any tag in the same atomic push", t => {
  const setup = fixture(t); const hook = join(setup.remote, "hooks/update");
  writeFileSync(hook, '#!/bin/sh\ncase "$1" in refs/heads/*) exit 1;; esac\n'); chmodSync(hook, 0o700);
  assert.equal(release(setup, "ticks").status, 1); assert.equal(git(["ls-remote", "--refs", "origin"], setup), "");
});

test("commit hook failure keeps version edits inspectable and never tags or pushes", t => {
  const setup = fixture(t); const hook = join(setup.repo, ".git/hooks/pre-commit");
  writeFileSync(hook, '#!/bin/sh\nexit 1\n'); chmodSync(hook, 0o700);
  const result = release(setup, "site"); assert.equal(result.status, 1); assert.match(result.stderr, /Version commit failed/);
  assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial); assert.equal(git(["tag", "--list"], setup), "");
  assert.equal(readProducts(setup.repo, "site")[0].version, "0.1.7");
});

test("Cargo version edits leave external dependency versions untouched and enforce the tag size limit", t => {
  const setup = fixture(t); const product = readProducts(setup.repo, "binary")[0];
  const changes = versionChanges(setup.repo, [{ ...product, nextVersion: "0.3.0" }]);
  assert.match(changes.get("Cargo.lock"), /name = "external"\nversion = "0.2.0"/);
  assert.match(changes.get("Cargo.lock"), /name = "tickloop-agent"\nversion = "0.3.0"/);
  assert.equal(nextVersion({ ...product, tagPrefix: "artifacts-v", version: `1.0.${"9".repeat(16)}` }), `1.0.1${"0".repeat(16)}`);
  assert.throws(() => nextVersion({ ...product, tagPrefix: "artifacts-v", version: `1.0.${"9".repeat(17)}` }), /exceeds/);
});
