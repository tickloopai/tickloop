import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
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
const releaseTag = "v0.2.1";
const versionPaths = ["Cargo.lock", "Cargo.toml", "distribution/cargo/Cargo.lock", "distribution/cargo/Cargo.toml"];

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

function fixture(t) {
  const base = mkdtempSync(join(tmpdir(), "release-test-"));
  t.after(() => rmSync(base, { recursive: true, force: true }));
  const repo = join(base, "repo"), remote = join(base, "remote.git"), bin = join(base, "bin");
  mkdirSync(repo); mkdirSync(bin);
  const env = { ...process.env, PATH: `${bin}:${process.env.PATH}`, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1", RELEASE_TEST_REPO: repo, RELEASE_TEST_REMOTE: remote, RELEASE_TEST_LOG: join(base, "checks.log"), RELEASE_TEST_PUSH_LOG: join(base, "push.log") };
  for (const name of ["VERSION", "CARGO_TARGET_DIR", "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"]) delete env[name];
  const setup = { repo, remote, base, env };
  const ssh = join(base, "ssh.sh");
  writeFileSync(ssh, '#!/bin/sh\ncase "$*" in\n*git-upload-pack*) [ "$RELEASE_TEST_REJECT_READ" != 1 ] || exit 1; exec git-upload-pack "$RELEASE_TEST_REMOTE" ;;\n*git-receive-pack*) [ "$RELEASE_TEST_REJECT_PUSH" != 1 ] || exit 1; exec git-receive-pack "$RELEASE_TEST_REMOTE" ;;\nesac\nexit 1\n');
  chmodSync(ssh, 0o700); env.GIT_SSH_COMMAND = ssh;
  writeFileSync(join(bin, "git"), `#!/bin/sh
[ "$1" != push ] || printf '%s\\n' "$*" >> "$RELEASE_TEST_PUSH_LOG"
exec ${shellQuote(realGit)} "$@"
`);
  chmodSync(join(bin, "git"), 0o700);
  const check = `#!/bin/sh
printf '%s %s\\n' "$(basename "$0")" "$*" >> "$RELEASE_TEST_LOG"
[ "$RELEASE_TEST_CHECK_FAIL" != 1 ] || exit 1
if [ "$RELEASE_TEST_DIRTY_ROOT" = 1 ]; then printf 'user edit\\n' > "$RELEASE_TEST_REPO/snapshot.txt"; fi
if [ "$RELEASE_TEST_DIRTY_SNAPSHOT" = 1 ]; then printf 'unexpected\\n' > snapshot.txt; fi
if [ "$RELEASE_TEST_MOVE_HEAD" = 1 ]; then git -C "$RELEASE_TEST_REPO" commit --allow-empty -qm 'Concurrent work'; fi
if [ "$RELEASE_TEST_MOVE_HEAD_ONCE" = 1 ] && [ ! -f "$RELEASE_TEST_REPO/.git/check-moved-head" ]; then
  touch "$RELEASE_TEST_REPO/.git/check-moved-head"
  git -C "$RELEASE_TEST_REPO" commit --allow-empty -qm 'Concurrent work'
fi
if [ -n "$RELEASE_TEST_GATE" ]; then
  printf ready > "$RELEASE_TEST_GATE.started"
  while [ ! -f "$RELEASE_TEST_GATE.continue" ]; do sleep 0.01; done
fi
if [ "$RELEASE_TEST_SWITCH_BRANCH" = 1 ]; then git -C "$RELEASE_TEST_REPO" checkout -q -B switched; fi
if [ -n "$RELEASE_TEST_ADD_TAG" ]; then git -C "$RELEASE_TEST_REPO" tag -f "$RELEASE_TEST_ADD_TAG"; fi
`;
  for (const name of ["make", "cargo"]) { writeFileSync(join(bin, name), check); chmodSync(join(bin, name), 0o700); }
  git(["init", "--bare", "-q", remote], setup); git(["init", "-q", "-b", "main"], setup);
  git(["config", "user.name", "Release Test"], setup); git(["config", "user.email", "release@example.invalid"], setup);
  writeFileSync(join(repo, "Cargo.toml"), '[workspace]\nmembers = ["harness/*", "code/*"]\n\n[workspace.package]\nversion = "0.2.0"\nedition = "2024"\n');
  const locked = [];
  for (const [name, path] of [["code", "code/cli"], ["harness-runtime", "harness/runtime"]]) {
    const directory = join(repo, path); mkdirSync(directory, { recursive: true });
    writeFileSync(join(directory, "Cargo.toml"), `[package]\nname = "tickloop-${name}"\nversion.workspace = true\n`);
    locked.push(`[[package]]\nname = "tickloop-${name}"\nversion = "0.2.0"\n`);
  }
  locked.push('[[package]]\nname = "external"\nversion = "0.2.0"\nsource = "registry+https://example.invalid"\n');
  writeFileSync(join(repo, "Cargo.lock"), 'version = 4\n\n' + locked.join("\n"));
  mkdirSync(join(repo, "distribution/cargo"), { recursive: true });
  writeFileSync(join(repo, "distribution/cargo/Cargo.toml"), '[package]\nname = "tickloop"\nversion = "0.2.0"\n');
  writeFileSync(join(repo, "distribution/cargo/Cargo.lock"), 'version = 4\n\n[[package]]\nname = "tickloop"\nversion = "0.2.0"\n');
  mkdirSync(join(repo, ".github/workflows"), { recursive: true });
  writeFileSync(join(repo, ".github/workflows/release.yml"), "name: Release\n");
  writeFileSync(join(repo, "snapshot.txt"), "business code\n");
  git(["add", "."], setup); git(["commit", "-qm", "Initial snapshot"], setup);
  git(["remote", "add", "origin", `git@github.com:${expectedRepository}.git`], setup);
  setup.initial = git(["rev-parse", "HEAD"], setup);
  return setup;
}

function release(setup, mode, env = {}, channel = "binary") {
  return command(process.execPath, [releaseScript, channel, ...(mode ? [mode] : [])], setup, env);
}

test("check is read-only; release commits only checked version files and pushes branch plus tag atomically", t => {
  const setup = fixture(t);
  const check = release(setup, "--check"); assert.equal(check.status, 0, check.stderr);
  assert.ok(check.stdout.includes(releaseTag));
  assert.equal(git(["status", "--porcelain"], setup), ""); assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial);
  assert.equal(git(["tag", "--list"], setup), ""); assert.ok(!existsSync(setup.env.RELEASE_TEST_LOG));
  const result = release(setup); assert.equal(result.status, 0, result.stderr);
  const commit = git(["rev-parse", "HEAD"], setup);
  assert.notEqual(commit, setup.initial); assert.equal(git(["rev-parse", "HEAD^"], setup), setup.initial);
  assert.deepEqual(git(["diff", "--name-only", "HEAD^", "HEAD"], setup).split("\n"), versionPaths);
  assert.match(git(["log", "-1", "--format=%s"], setup), /^chore\(release\): Tickloop 0\.2\.1$/);
  assert.equal(git(["status", "--porcelain"], setup), "");
  assert.equal(git(["ls-remote", "--refs", "origin", "refs/heads/main"], setup), `${commit}\trefs/heads/main`);
  assert.equal(git(["cat-file", "-t", releaseTag], setup), "tag");
  assert.equal(git(["rev-parse", `${releaseTag}^{}`], setup), commit);
  assert.equal(git(["ls-remote", "origin", `refs/tags/${releaseTag}^{}`], setup), `${commit}\trefs/tags/${releaseTag}^{}`);
  const pushes = readFileSync(setup.env.RELEASE_TEST_PUSH_LOG, "utf8").trim().split("\n");
  assert.equal(pushes.length, 1); assert.match(pushes[0], /--atomic/);
  assert.equal(git(["worktree", "list", "--porcelain"], setup).split("\n").filter(line => line.startsWith("worktree ")).length, 1);
});

test("failed build leaves versions, HEAD and tags untouched", t => {
  const setup = fixture(t); const result = release(setup, undefined, { RELEASE_TEST_CHECK_FAIL: "1" });
  assert.equal(result.status, 1); assert.match(result.stderr, /no version commit or release tag/);
  assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial); assert.equal(git(["status", "--porcelain"], setup), ""); assert.equal(git(["tag", "--list"], setup), "");
});

test("VERSION may explicitly advance the version but cannot reuse or downgrade it", t => {
  const setup = fixture(t);
  assert.equal(release(setup, "--check", { VERSION: "2.0.0" }).status, 0);
  for (const version of ["0.2.0", "0.0.0", "01.2.3", "1.2", "1.2.3-rc.1", "../1.2.3", "1.2.3;touch bad"]) assert.equal(release(setup, "--check", { VERSION: version }).status, 1);
  assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial);
});

test("local and remote future tags reject stale version files rather than silently skipping a version", t => {
  const setup = fixture(t); git(["tag", "v0.2.2"], setup);
  assert.match(release(setup, "--check").stderr, /must be newer/);
  git(["push", "origin", "refs/tags/v0.2.2"], setup); git(["tag", "-d", "v0.2.2"], setup);
  assert.match(release(setup, "--check").stderr, /must be newer/);
  assert.equal(release(setup, "--check", { VERSION: "0.3.0" }).status, 0);
});

test("unrelated staged, unstaged and untracked changes survive release", t => {
  const setup = fixture(t);
  writeFileSync(join(setup.repo, "snapshot.txt"), "staged work\n");
  git(["add", "snapshot.txt"], setup);
  writeFileSync(join(setup.repo, "snapshot.txt"), "unstaged work on top\n");
  writeFileSync(join(setup.repo, "untracked.txt"), "untracked work\n");
  const status = git(["status", "--porcelain"], setup);
  const staged = git(["diff", "--cached", "--binary"], setup);
  const unstaged = git(["diff", "--binary"], setup);
  const checked = release(setup, "--check");
  assert.equal(checked.status, 0, checked.stderr);
  assert.equal(git(["status", "--porcelain"], setup), status);
  assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial);
  const result = release(setup);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(git(["diff", "--name-only", "HEAD^", "HEAD"], setup).split("\n"), versionPaths);
  assert.equal(git(["status", "--porcelain"], setup), status);
  assert.equal(git(["diff", "--cached", "--binary"], setup), staged);
  assert.equal(git(["diff", "--binary"], setup), unstaged);
  assert.equal(readFileSync(join(setup.repo, "untracked.txt"), "utf8"), "untracked work\n");
  assert.equal(git(["show", "HEAD:snapshot.txt"], setup), "business code");
});

test("only release version files must be unchanged", t => {
  for (const path of versionPaths) {
    const setup = fixture(t);
    const value = readFileSync(join(setup.repo, path), "utf8") + "\nuser version edits\n";
    writeFileSync(join(setup.repo, path), value);
    const result = release(setup);
    assert.equal(result.status, 1, path);
    assert.match(result.stderr, /Release version files have local edits/);
    assert.ok(result.stderr.includes(path));
    assert.equal(readFileSync(join(setup.repo, path), "utf8"), value);
    assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial);
  }
});

test("unrelated edits made while checks run are retained", t => {
  const setup = fixture(t);
  const result = release(setup, undefined, { RELEASE_TEST_DIRTY_ROOT: "1" });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(readFileSync(join(setup.repo, "snapshot.txt"), "utf8"), "user edit\n");
  assert.equal(git(["show", "HEAD:snapshot.txt"], setup), "business code");
});

test("rejects detached HEAD, unknown channels, bad arguments and unexpected destinations", t => {
  const setup = fixture(t);
  for (const channel of ["ticks", "docs", "artifacts", "site", "constructor"]) assert.match(release(setup, undefined, {}, channel).stderr, /Usage:/);
  for (const mode of ["--force", "--check extra"]) assert.match(command(process.execPath, [releaseScript, "binary", ...mode.split(" ")], setup).stderr, /Usage:/);
  git(["checkout", "--detach", "-q"], setup); assert.match(release(setup).stderr, /detached HEAD/);
  git(["checkout", "-q", "main"], setup); git(["remote", "set-url", "--push", "origin", "git@github.com:someone/other.git"], setup);
  assert.match(release(setup).stderr, /exactly one fetch and one push/);
  assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial); assert.equal(git(["tag", "--list"], setup), "");
});

test("requires its workflow and consistent version mirrors in the committed snapshot", t => {
  const setup = fixture(t);
  git(["rm", "-q", ".github/workflows/release.yml"], setup); git(["commit", "-qm", "Remove workflow"], setup);
  assert.match(release(setup).stderr, /committed snapshot must contain/);
  git(["checkout", "-q", setup.initial, "--", ".github/workflows/release.yml"], setup);
  writeFileSync(join(setup.repo, "distribution/cargo/Cargo.toml"), '[package]\nname = "tickloop"\nversion = "9.0.0"\n');
  git(["add", "."], setup); git(["commit", "-qm", "Mismatch"], setup);
  assert.match(release(setup).stderr, /Cargo launcher must be named tickloop and match the workspace version/);
  writeFileSync(join(setup.repo, "distribution/cargo/Cargo.toml"), '[package]\nname = "tickloop"\nversion = "0.2.0"\n');
  writeFileSync(join(setup.repo, "distribution/cargo/Cargo.lock"), 'version = 4\n\n[[package]]\nname = "tickloop"\nversion = "9.0.0"\n');
  git(["add", "."], setup); git(["commit", "-qm", "Lock mismatch"], setup);
  assert.match(release(setup).stderr, /launcher lock version must match/);
  assert.equal(git(["tag", "--list"], setup), "");
});

test("branch moves, unexpected build edits and racing tags stop finalization", t => {
  for (const variable of ["RELEASE_TEST_MOVE_HEAD", "RELEASE_TEST_SWITCH_BRANCH", "RELEASE_TEST_DIRTY_SNAPSHOT", "RELEASE_TEST_ADD_TAG"]) {
    const setup = fixture(t);
    const result = release(setup, undefined, { [variable]: variable === "RELEASE_TEST_ADD_TAG" ? releaseTag : "1" });
    assert.equal(result.status, 1, variable);
    assert.equal(readProducts(setup.repo, "binary")[0].version, "0.2.0");
    assert.equal(git(["ls-remote", "--refs", "origin"], setup), "");
  }
});

test("an unreadable remote stops before version changes", t => {
  const setup = fixture(t); const result = release(setup, undefined, { RELEASE_TEST_REJECT_READ: "1" });
  assert.equal(result.status, 1); assert.match(result.stderr, /Could not check Tickloop tags on origin/);
  assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial); assert.equal(git(["status", "--porcelain"], setup), "");
  assert.ok(!existsSync(setup.env.RELEASE_TEST_LOG));
});

test("failed push keeps the release commit and tag with recovery instructions, without changing remote refs", t => {
  const setup = fixture(t); const result = release(setup, undefined, { RELEASE_TEST_REJECT_PUSH: "1" });
  assert.equal(result.status, 1); assert.match(result.stderr, /Do not rerun release to bump versions again/);
  assert.equal(git(["rev-parse", `${releaseTag}^{}`], setup), git(["rev-parse", "HEAD"], setup));
  assert.equal(git(["ls-remote", "--refs", "origin"], setup), ""); assert.equal(git(["status", "--porcelain"], setup), "");
});

test("a rejected branch does not publish the tag in the same atomic push", t => {
  const setup = fixture(t); const hook = join(setup.remote, "hooks/update");
  writeFileSync(hook, '#!/bin/sh\ncase "$1" in refs/heads/*) exit 1;; esac\n'); chmodSync(hook, 0o700);
  const result = release(setup);
  assert.equal(result.status, 1); assert.match(result.stderr, /Push failed/);
  assert.equal(git(["ls-remote", "--refs", "origin"], setup), "");
});

test("commit hook failure keeps version edits inspectable and never tags or pushes", t => {
  const setup = fixture(t); const hook = join(setup.repo, ".git/hooks/pre-commit");
  writeFileSync(hook, '#!/bin/sh\nexit 1\n'); chmodSync(hook, 0o700);
  const result = release(setup); assert.equal(result.status, 1); assert.match(result.stderr, /Version commit failed/);
  assert.equal(git(["rev-parse", "HEAD"], setup), setup.initial); assert.equal(git(["tag", "--list"], setup), "");
  assert.equal(readProducts(setup.repo, "binary")[0].version, "0.2.1");
  assert.ok(!existsSync(setup.env.RELEASE_TEST_PUSH_LOG));
});

test("Cargo version edits leave external dependency versions untouched and enforce the tag size limit", t => {
  const setup = fixture(t); const product = readProducts(setup.repo, "binary")[0];
  const changes = versionChanges(setup.repo, [{ ...product, nextVersion: "0.3.0" }]);
  assert.deepEqual([...changes.keys()].sort(), versionPaths);
  assert.match(changes.get("Cargo.lock"), /name = "external"\nversion = "0.2.0"/);
  assert.match(changes.get("Cargo.lock"), /name = "tickloop-code"\nversion = "0.3.0"/);
  assert.match(changes.get("Cargo.lock"), /name = "tickloop-harness-runtime"\nversion = "0.3.0"/);
  assert.equal(nextVersion({ ...product, version: `1.0.${"9".repeat(26)}` }), `1.0.1${"0".repeat(26)}`);
  assert.throws(() => nextVersion({ ...product, version: `1.0.${"9".repeat(27)}` }), /exceeds/);
});

test("an advancing HEAD on the same branch is checked again before creating a version commit", t => {
  const setup = fixture(t);
  const result = release(setup, undefined, { RELEASE_TEST_MOVE_HEAD_ONCE: "1" });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, /HEAD advanced during checks/);
  assert.equal(git(["rev-list", "--count", `${setup.initial}..HEAD`], setup), "2");
  assert.equal(git(["log", "-1", "--format=%s", "HEAD^"], setup), "Concurrent work");
  assert.equal(git(["rev-parse", `${releaseTag}^{}`], setup), git(["rev-parse", "HEAD"], setup));
});

function startRelease(setup, gate) {
  const child = spawn(process.execPath, [releaseScript, "binary"], {
    cwd: setup.repo, env: { ...setup.env, RELEASE_TEST_GATE: gate }, stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "", stderr = "";
  child.stdout.on("data", value => stdout += value);
  child.stderr.on("data", value => stderr += value);
  const done = new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("close", status => resolve({ status, stdout, stderr }));
  });
  return { done, stderr: () => stderr };
}

async function waitUntil(predicate) {
  const deadline = Date.now() + 10_000;
  while (!predicate()) {
    assert.ok(Date.now() < deadline, "timed out waiting for release synchronization");
    await new Promise(resolve => setTimeout(resolve, 20));
  }
}

test("concurrent releases wait for one shared lock and the queued one releases the next version", async t => {
  const setup = fixture(t);
  const firstGate = join(setup.base, "first-gate"), secondGate = join(setup.base, "second-gate");
  writeFileSync(`${secondGate}.continue`, "ready");
  const first = startRelease(setup, firstGate);
  let second;
  try {
    await waitUntil(() => existsSync(`${firstGate}.started`));
    second = startRelease(setup, secondGate);
    await waitUntil(() => second.stderr().includes("waiting for the shared release lock"));
    assert.ok(!existsSync(`${secondGate}.started`), "the queued release must not check an obsolete HEAD");
  } finally {
    writeFileSync(`${firstGate}.continue`, "ready");
  }
  const firstResult = await first.done;
  const secondResult = await second.done;
  assert.equal(firstResult.status, 0, firstResult.stderr);
  assert.equal(secondResult.status, 0, secondResult.stderr);
  assert.equal(git(["rev-list", "--count", `${setup.initial}..HEAD`], setup), "2");
  assert.equal(git(["rev-parse", "v0.2.1^{}"], setup), git(["rev-parse", "HEAD^"], setup));
  assert.equal(git(["rev-parse", "v0.2.2^{}"], setup), git(["rev-parse", "HEAD"], setup));
  assert.equal(readProducts(setup.repo, "binary")[0].version, "0.2.2");
  assert.ok(!existsSync(join(setup.repo, ".git/tickloop-release.lock")));
});
