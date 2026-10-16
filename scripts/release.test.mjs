import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const releaseScript = join(dirname(fileURLToPath(import.meta.url)), "release.mjs");
const expectedRepository = readFileSync(releaseScript, "utf8").match(/^const expectedRepository = "([^"]+)";$/m)[1];
const workspaceManifest = `[workspace]
members = []

[workspace.package]
version = "0.2.0"
edition = "2024"
`;
const channels = [
  {
    name: "docs",
    tag: "docs-v0.1.0",
    workflow: ".github/workflows/docs-deploy.yml",
    checks: ["npm --prefix artifact ci", "npm --prefix artifact run typecheck", "npm --prefix artifact run build"],
    failedCheck: /Docs dependency install failed; no release tag was created/,
  },
  {
    name: "site",
    tag: "site-v0.1.0",
    workflow: ".github/workflows/site-deploy.yml",
    checks: ["npm --prefix site ci", "npm --prefix site run build"],
    failedCheck: /Site dependency install failed; no release tag was created/,
  },
  {
    name: "binary",
    tag: "v0.2.0",
    workflow: ".github/workflows/release.yml",
    checks: ["make build"],
    failedCheck: /Tickloop build failed; no release tag was created/,
  },
];

function command(commandName, args, { cwd, env = process.env } = {}) {
  const result = spawnSync(commandName, args, { cwd, env, encoding: "utf8" });
  if (result.error) throw result.error;
  return result;
}

function git(args, fixture, expectedStatus = 0) {
  const result = command("git", args, { cwd: fixture.repo, env: fixture.env });
  assert.equal(result.status, expectedStatus, `${args.join(" ")}: ${result.stderr}`);
  return result.stdout.trim();
}

function fixture(t) {
  const base = mkdtempSync(join(tmpdir(), "release-test-"));
  t.after(() => rmSync(base, { recursive: true, force: true }));
  const repo = join(base, "repo");
  const remote = join(base, "remote.git");
  const ssh = join(base, "ssh.sh");
  const testBin = join(base, "bin");
  const checkLog = join(base, "checks.log");
  const checkContext = join(base, "check-context.log");
  assert.equal(command("git", ["init", "--bare", "-q", remote]).status, 0);
  assert.equal(command("git", ["init", "-q", repo]).status, 0);
  writeFileSync(ssh, `#!/bin/sh
case "$*" in
  *git-upload-pack*)
    if [ "$RELEASE_TEST_REJECT_READ" = 1 ]; then exit 1; fi
    exec git-upload-pack "$RELEASE_TEST_REMOTE_PATH" ;;
  *git-receive-pack*)
    if [ "$RELEASE_TEST_REJECT_PUSH" = 1 ]; then exit 1; fi
    exec git-receive-pack "$RELEASE_TEST_REMOTE_PATH" ;;
esac
exit 1
`);
  chmodSync(ssh, 0o700);
  mkdirSync(testBin);
  const checkCommand = join(testBin, "npm");
  writeFileSync(checkCommand, `#!/bin/sh
printf '%s %s\\n' "$(basename "$0")" "$*" >> "$RELEASE_TEST_CHECK_LOG"
if [ -e unknown.txt ]; then untracked=present; else untracked=absent; fi
printf '%s\\t%s\\t%s\\t%s\\n' "$(pwd -P)" "$(cat snapshot.txt)" "$untracked" "$CARGO_TARGET_DIR" >> "$RELEASE_TEST_CHECK_CONTEXT"
if [ "$RELEASE_TEST_CHECK_FAIL" = 1 ]; then exit 1; fi
if [ "$RELEASE_TEST_CHECK_MOVE_HEAD" = 1 ]; then
  git -C "$RELEASE_TEST_REPO_PATH" commit --allow-empty -qm "Changed HEAD during release checks" || exit 1
fi
if [ "$RELEASE_TEST_CHECK_DIRTY_WORKTREE" = 1 ]; then
  printf 'changed during build\\n' > "$RELEASE_TEST_REPO_PATH/snapshot.txt"
fi
if [ "$RELEASE_TEST_CHECK_SWITCH_BRANCH" = 1 ]; then
  git -C "$RELEASE_TEST_REPO_PATH" show-ref --verify --quiet refs/heads/switched ||
    git -C "$RELEASE_TEST_REPO_PATH" branch switched || exit 1
  git -C "$RELEASE_TEST_REPO_PATH" checkout -q switched || exit 1
fi
if [ -n "$RELEASE_TEST_CHECK_ADD_TAG" ]; then
  git -C "$RELEASE_TEST_REPO_PATH" show-ref --verify --quiet "refs/tags/$RELEASE_TEST_CHECK_ADD_TAG" ||
    git -C "$RELEASE_TEST_REPO_PATH" tag "$RELEASE_TEST_CHECK_ADD_TAG" || exit 1
fi
exit 0
`);
  chmodSync(checkCommand, 0o700);
  copyFileSync(checkCommand, join(testBin, "make"));
  chmodSync(join(testBin, "make"), 0o700);
  const env = {
    ...process.env,
    VERSION: "0.1.0",
    PATH: `${testBin}:${process.env.PATH}`,
    GIT_SSH_COMMAND: ssh,
    RELEASE_TEST_REMOTE_PATH: remote,
    RELEASE_TEST_REPO_PATH: repo,
    RELEASE_TEST_CHECK_LOG: checkLog,
    RELEASE_TEST_CHECK_CONTEXT: checkContext,
  };
  delete env.CARGO_TARGET_DIR;
  const created = { repo, remote, env, checkLog, checkContext };
  git(["config", "user.name", "Release Test"], created);
  git(["config", "user.email", "release@example.invalid"], created);
  writeFileSync(join(repo, "snapshot.txt"), "release snapshot\n");
  writeFileSync(join(repo, "Cargo.toml"), workspaceManifest);
  mkdirSync(join(repo, ".github", "workflows"), { recursive: true });
  for (const { workflow } of channels) writeFileSync(join(repo, workflow), "name: Release workflow\n");
  git(["add", "."], created);
  git(["commit", "-qm", "Initial snapshot"], created);
  git(["remote", "add", "origin", `git@github.com:${expectedRepository}.git`], created);
  return created;
}

function release(fixture, channel, args = [], env = fixture.env) {
  return command(process.execPath, [releaseScript, channel, ...args], { cwd: fixture.repo, env });
}

function ranChecks(fixture) {
  return existsSync(fixture.checkLog) ? readFileSync(fixture.checkLog, "utf8").trim().split("\n") : [];
}

function checkContexts(fixture) {
  if (!existsSync(fixture.checkContext)) return [];
  return readFileSync(fixture.checkContext, "utf8").trim().split("\n").map(line => {
    const [directory, snapshot, untracked, cargoTargetDirectory] = line.split("\t");
    return { directory, snapshot, untracked, cargoTargetDirectory };
  });
}

function worktreeCount(fixture) {
  return git(["worktree", "list", "--porcelain"], fixture).split("\n").filter(line => line.startsWith("worktree ")).length;
}

function commitManifest(fixture, manifest) {
  writeFileSync(join(fixture.repo, "Cargo.toml"), manifest);
  git(["commit", "-qam", "Change the workspace manifest"], fixture);
}

for (const channel of channels) {
  test(`${channel.name}: check, then publish an annotated tag for the committed snapshot`, t => {
    const setup = fixture(t);
    delete setup.env.VERSION;
    const commit = git(["rev-parse", "HEAD"], setup);
    assert.match(release(setup, channel.name, ["--check"]).stdout, /No Git changes were made/);
    assert.match(release(setup, channel.name, ["--dry-run"]).stdout, new RegExp(`Ready to release ${channel.tag}\\b`));
    assert.deepEqual(ranChecks(setup), []);
    git(["show-ref", "--verify", "--quiet", `refs/tags/${channel.tag}`], setup, 1);

    const result = release(setup, channel.name);
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, new RegExp(`Published ${channel.tag} from ${commit}`));
    assert.deepEqual(ranChecks(setup), channel.checks);
    assert.equal(git(["cat-file", "-t", `refs/tags/${channel.tag}`], setup), "tag");
    assert.equal(git(["rev-parse", `refs/tags/${channel.tag}^{}`], setup), commit);
    const localTag = git(["rev-parse", `refs/tags/${channel.tag}`], setup);
    assert.equal(git(["ls-remote", "--refs", "origin", `refs/tags/${channel.tag}`], setup).split("\t")[0], localTag);
    const branch = git(["symbolic-ref", "HEAD"], setup);
    const remoteBranch = git(["ls-remote", "--refs", "origin", branch], setup);
    assert.equal(remoteBranch, channel.name === "site" ? `${commit}\t${branch}` : "");
  });

  test(`${channel.name}: reject existing local and remote tags without retrying`, t => {
    const setup = fixture(t);
    const env = { ...setup.env, VERSION: channel.tag.replace(/^[a-z-]*v/, "") };
    assert.equal(release(setup, channel.name, [], env).status, 0);
    assert.match(release(setup, channel.name, [], env).stderr, /already exists locally/);
    git(["tag", "-d", channel.tag], setup);
    assert.match(release(setup, channel.name, [], env).stderr, /already exists on origin/);
  });

  test(`${channel.name}: reject an unexpected push destination`, t => {
    const setup = fixture(t);
    delete setup.env.VERSION;
    git(["remote", "set-url", "--push", "origin", "git@github.com:someone/other.git"], setup);
    const result = release(setup, channel.name, ["--check"]);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /exactly one fetch and one push URL/);
  });

  test(`${channel.name}: require its deployment workflow in the committed snapshot`, t => {
    const setup = fixture(t);
    delete setup.env.VERSION;
    git(["rm", "-q", channel.workflow], setup);
    git(["commit", "-qm", "Remove workflow"], setup);
    const result = release(setup, channel.name, ["--check"]);
    assert.equal(result.status, 1);
    assert.match(result.stderr, new RegExp(`committed snapshot must contain ${channel.workflow.replaceAll(".", "\\.")}`));
  });

  test(`${channel.name}: a failed local check creates no tag`, t => {
    const setup = fixture(t);
    delete setup.env.VERSION;
    const result = release(setup, channel.name, [], { ...setup.env, RELEASE_TEST_CHECK_FAIL: "1" });
    assert.equal(result.status, 1);
    assert.match(result.stderr, channel.failedCheck);
    assert.deepEqual(ranChecks(setup), channel.checks.slice(0, 1));
    assert.equal(worktreeCount(setup), 1);
    assert.equal(existsSync(checkContexts(setup)[0].directory), false);
    git(["show-ref", "--verify", "--quiet", `refs/tags/${channel.tag}`], setup, 1);
  });
}

test("site: atomically publish the current branch and tag at the same checked commit", t => {
  const setup = fixture(t);
  git(["checkout", "-qb", "website/release"], setup);
  const commit = git(["rev-parse", "HEAD"], setup);
  const checked = release(setup, "site", ["--check"]);
  assert.equal(checked.status, 0, checked.stderr);
  assert.match(checked.stdout, /website\/release/);
  assert.equal(git(["ls-remote", "--refs", "origin"], setup), "");
  const result = release(setup, "site");
  assert.equal(result.status, 0, result.stderr);
  assert.equal(git(["ls-remote", "--refs", "origin", "refs/heads/website/release"], setup), `${commit}\trefs/heads/website/release`);
  assert.equal(git(["ls-remote", "origin", "refs/tags/site-v0.1.0^{}"], setup), `${commit}\trefs/tags/site-v0.1.0^{}`);
});

test("site: reject a detached HEAD before checks or tag creation", t => {
  const setup = fixture(t);
  git(["checkout", "--detach", "-q"], setup);
  const result = release(setup, "site");
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Site release requires a branch/);
  assert.deepEqual(ranChecks(setup), []);
  assert.equal(git(["tag", "--list"], setup), "");
});

test("site: reject a branch switch during checks even when HEAD is unchanged", t => {
  const setup = fixture(t);
  const commit = git(["rev-parse", "HEAD"], setup);
  const result = release(setup, "site", [], { ...setup.env, RELEASE_TEST_CHECK_SWITCH_BRANCH: "1" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Branch changed during Site checks/);
  assert.equal(git(["rev-parse", "HEAD"], setup), commit);
  assert.equal(git(["tag", "--list"], setup), "");
  assert.equal(git(["ls-remote", "--refs", "origin"], setup), "");
});

test("site: a rejected branch leaves neither remote branch nor release tag", t => {
  const setup = fixture(t);
  const hook = join(setup.remote, "hooks", "update");
  writeFileSync(hook, '#!/bin/sh\ncase "$1" in refs/heads/*) exit 1 ;; esac\n');
  chmodSync(hook, 0o700);
  const result = release(setup, "site");
  assert.equal(result.status, 1);
  assert.match(result.stderr, /atomic branch-and-tag push/);
  assert.equal(git(["ls-remote", "--refs", "origin"], setup), "");
  git(["show-ref", "--verify", "--quiet", "refs/tags/site-v0.1.0"], setup);
});

test("site: a remote branch ahead of HEAD is not overwritten or partially released", t => {
  const setup = fixture(t);
  const branch = git(["symbolic-ref", "HEAD"], setup);
  git(["checkout", "-qb", "remote-work"], setup);
  git(["commit", "--allow-empty", "-qm", "Remote work"], setup);
  const remoteCommit = git(["rev-parse", "HEAD"], setup);
  git(["push", "origin", `HEAD:${branch}`], setup);
  git(["checkout", "-q", branch.slice("refs/heads/".length)], setup);
  const result = release(setup, "site");
  assert.equal(result.status, 1);
  assert.equal(git(["ls-remote", "--refs", "origin", branch], setup), `${remoteCommit}\t${branch}`);
  assert.equal(git(["ls-remote", "--refs", "origin", "refs/tags/site-v0.1.0"], setup), "");
});

test("start at 0.1.0 when VERSION is omitted or empty and no Docs tags exist", t => {
  const setup = fixture(t);
  delete setup.env.VERSION;
  for (const env of [setup.env, { ...setup.env, VERSION: "" }]) {
    const result = release(setup, "docs", ["--check"], { ...env, RELEASE_TEST_CHECK_FAIL: "1" });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /Ready to release docs-v0\.1\.0/);
    assert.equal(git(["tag", "--list"], setup), "");
    assert.equal(git(["ls-remote", "--refs", "origin"], setup), "");
  }
  assert.equal(release(setup, "docs").status, 0);
  assert.equal(git(["cat-file", "-t", "refs/tags/docs-v0.1.0"], setup), "tag");
});

test("increment the numerically highest stable Docs tag from origin even without a local copy", t => {
  const setup = fixture(t);
  delete setup.env.VERSION;
  git(["tag", "-a", "docs-v1.10.9", "-m", "Prior release"], setup);
  git(["push", "origin", "refs/tags/docs-v1.10.9"], setup);
  git(["tag", "-d", "docs-v1.10.9"], setup);
  for (const tag of ["docs-v1.9.99", "docs-v0.99.99", "docs-v1.10.8", "docs-v9.0.0-rc.1", "docs-v01.0.0", "v99.0.0", "site-v99.0.0"]) {
    git(["tag", tag], setup);
  }
  const result = release(setup, "docs");
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Published docs-v1\.10\.10/);
  assert.notEqual(git(["ls-remote", "--refs", "origin", "refs/tags/docs-v1.10.10"], setup), "");
});

test("select the next site version from site tags only", t => {
  const setup = fixture(t);
  delete setup.env.VERSION;
  for (const tag of ["docs-v3.0.0", "v9.0.0", "site-v0.4.1-rc.1"]) git(["tag", tag], setup);
  assert.match(release(setup, "site", ["--check"]).stdout, /Ready to release site-v0\.1\.0/);
  git(["tag", "site-v0.4.1"], setup);
  git(["push", "origin", "refs/tags/site-v0.4.1"], setup);
  git(["tag", "-d", "site-v0.4.1"], setup);
  const result = release(setup, "site");
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Published site-v0\.4\.2/);
});

test("include local tags when selecting the next version", t => {
  const setup = fixture(t);
  delete setup.env.VERSION;
  git(["tag", "docs-v1.99.99"], setup);
  git(["push", "origin", "refs/tags/docs-v1.99.99"], setup);
  git(["tag", "docs-v2.0.0"], setup);
  const result = release(setup, "docs", ["--dry-run"]);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Ready to release docs-v2\.0\.1/);
  git(["show-ref", "--verify", "--quiet", "refs/tags/docs-v2.0.1"], setup, 1);
});

test("honor an explicit version instead of incrementing existing tags", t => {
  const setup = fixture(t);
  git(["tag", "docs-v1.0.9"], setup);
  const result = release(setup, "docs", [], { ...setup.env, VERSION: "2.0.0" });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Published docs-v2\.0\.0/);
  git(["show-ref", "--verify", "--quiet", "refs/tags/docs-v1.0.10"], setup, 1);
});

test("increment large patch numbers exactly and reject a version that grows beyond the supported length", t => {
  const setup = fixture(t);
  delete setup.env.VERSION;
  git(["tag", "docs-v0.1.9007199254740993"], setup);
  const result = release(setup, "docs", ["--check"]);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Ready to release docs-v0\.1\.9007199254740994/);
  git(["tag", `docs-v1.0.${"9".repeat(22)}`], setup);
  const overflow = release(setup, "docs");
  assert.equal(overflow.status, 1);
  assert.match(overflow.stderr, /next Docs version exceeds the supported length/);
  assert.equal(git(["ls-remote", "--refs", "origin"], setup), "");
});

test("do not guess an automatic version when origin cannot be read", t => {
  const setup = fixture(t);
  delete setup.env.VERSION;
  for (const [channel, label] of [["docs", "Docs"], ["site", "Site"]]) {
    const result = release(setup, channel, [], { ...setup.env, RELEASE_TEST_REJECT_READ: "1" });
    assert.equal(result.status, 1);
    assert.match(result.stderr, new RegExp(`Could not check ${label} tags on origin`));
  }
  const binary = release(setup, "binary", [], { ...setup.env, RELEASE_TEST_REJECT_READ: "1" });
  assert.equal(binary.status, 1);
  assert.match(binary.stderr, /Could not check tags on origin/);
  assert.equal(git(["tag", "--list"], setup), "");
});

test("stop if a newer tag appears during automatic release checks", t => {
  const setup = fixture(t);
  delete setup.env.VERSION;
  const result = release(setup, "docs", [], { ...setup.env, RELEASE_TEST_CHECK_ADD_TAG: "docs-v1.0.0" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Docs release tags changed during checks/);
  git(["show-ref", "--verify", "--quiet", "refs/tags/docs-v0.1.0"], setup, 1);
  assert.equal(git(["ls-remote", "--refs", "origin"], setup), "");
});

test("binary: take the version from the committed Cargo.toml, not the worktree", t => {
  const setup = fixture(t);
  delete setup.env.VERSION;
  git(["tag", "v0.9.0"], setup);
  writeFileSync(join(setup.repo, "Cargo.toml"), workspaceManifest.replace('"0.2.0"', '"9.9.9"'));
  assert.match(release(setup, "binary", ["--check"]).stdout, /Ready to release v0\.2\.0/);
  commitManifest(setup, workspaceManifest.replace('version = "0.2.0"', 'version = "0.3.0" # next release'));
  const result = release(setup, "binary");
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Published v0\.3\.0/);
  assert.notEqual(git(["ls-remote", "--refs", "origin", "refs/tags/v0.3.0"], setup), "");
});

test("binary: accept VERSION only when it matches the workspace version", t => {
  const setup = fixture(t);
  const mismatch = release(setup, "binary", [], { ...setup.env, VERSION: "0.3.0" });
  assert.equal(mismatch.status, 1);
  assert.match(mismatch.stderr, /VERSION 0\.3\.0 does not match the workspace version 0\.2\.0/);
  assert.deepEqual(ranChecks(setup), []);
  assert.equal(git(["tag", "--list"], setup), "");

  const matching = release(setup, "binary", [], { ...setup.env, VERSION: "0.2.0" });
  assert.equal(matching.status, 0, matching.stderr);
  assert.match(matching.stdout, /Published v0\.2\.0/);
});

test("binary: require a release version in the committed workspace manifest", t => {
  const manifests = [
    "[workspace]\nmembers = []\n\n[package]\nversion = \"0.2.0\"\n",
    workspaceManifest.replace('"0.2.0"', '"0.2"'),
    workspaceManifest.replace('"0.2.0"', '"01.2.0"'),
    workspaceManifest.replace('"0.2.0"', '"0.2.0-rc.1"'),
  ];
  for (const manifest of manifests) {
    const setup = fixture(t);
    delete setup.env.VERSION;
    commitManifest(setup, manifest);
    const result = release(setup, "binary", ["--check"]);
    assert.equal(result.status, 1, manifest);
    assert.match(result.stderr, /committed Cargo\.toml must set \[workspace\.package\] version/);
  }

  const setup = fixture(t);
  delete setup.env.VERSION;
  git(["rm", "-q", "Cargo.toml"], setup);
  git(["commit", "-qm", "Remove the workspace manifest"], setup);
  const missing = release(setup, "binary", ["--check"]);
  assert.equal(missing.status, 1);
  assert.match(missing.stderr, /committed snapshot must contain Cargo\.toml/);
});

test("check and publish the captured HEAD, not staged, unstaged or untracked changes", t => {
  for (const change of ["staged", "unstaged", "untracked"]) {
    const setup = fixture(t);
    const commit = git(["rev-parse", "HEAD"], setup);
    if (change === "untracked") {
      writeFileSync(join(setup.repo, "unknown.txt"), "untracked\n");
    } else {
      writeFileSync(join(setup.repo, "snapshot.txt"), "changed\n");
      if (change === "staged") git(["add", "snapshot.txt"], setup);
    }
    const checked = release(setup, "docs", ["--check"]);
    assert.equal(checked.status, 0, `${change}: ${checked.stderr}`);
    git(["show-ref", "--verify", "--quiet", "refs/tags/docs-v0.1.0"], setup, 1);
    const published = release(setup, "docs");
    assert.equal(published.status, 0, `${change}: ${published.stderr}`);
    const contexts = checkContexts(setup);
    assert.equal(contexts.length, 3, change);
    for (const context of contexts) {
      assert.notEqual(context.directory, realpathSync(setup.repo), change);
      assert.equal(context.snapshot, "release snapshot", change);
      assert.equal(context.untracked, "absent", change);
      assert.equal(existsSync(context.directory), false, change);
    }
    assert.equal(worktreeCount(setup), 1, change);
    assert.equal(git(["rev-parse", "refs/tags/docs-v0.1.0^{}"], setup), commit);
    assert.notEqual(git(["ls-remote", "--refs", "origin", "refs/tags/docs-v0.1.0"], setup), "");
  }
});

test("binary: build the snapshot with this checkout's Cargo target directory unless one is set", t => {
  const setup = fixture(t);
  delete setup.env.VERSION;
  assert.equal(release(setup, "binary").status, 0);
  assert.equal(checkContexts(setup)[0].cargoTargetDirectory, join(realpathSync(setup.repo), "target"));

  const custom = fixture(t);
  delete custom.env.VERSION;
  assert.equal(release(custom, "binary", [], { ...custom.env, CARGO_TARGET_DIR: "/custom/target" }).status, 0);
  assert.equal(checkContexts(custom)[0].cargoTargetDirectory, "/custom/target");
});

test("reject multiple push destinations even when the first is correct", t => {
  const setup = fixture(t);
  git(["remote", "set-url", "--add", "--push", "origin", `git@github.com:${expectedRepository}.git`], setup);
  git(["remote", "set-url", "--add", "--push", "origin", "git@github.com:someone/other.git"], setup);
  const result = release(setup, "docs", ["--check"]);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /exactly one fetch and one push URL/);
  git(["show-ref", "--verify", "--quiet", "refs/tags/docs-v0.1.0"], setup, 1);
});

test("a HEAD change during local checks creates no tag", t => {
  const setup = fixture(t);
  const initialCommit = git(["rev-parse", "HEAD"], setup);
  const result = release(setup, "docs", [], { ...setup.env, RELEASE_TEST_CHECK_MOVE_HEAD: "1" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /HEAD changed during Docs checks/);
  assert.notEqual(git(["rev-parse", "HEAD"], setup), initialCommit);
  git(["show-ref", "--verify", "--quiet", "refs/tags/docs-v0.1.0"], setup, 1);
});

test("worktree changes during local checks still publish the captured HEAD", t => {
  const setup = fixture(t);
  const commit = git(["rev-parse", "HEAD"], setup);
  const result = release(setup, "docs", [], { ...setup.env, RELEASE_TEST_CHECK_DIRTY_WORKTREE: "1" });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(git(["rev-parse", "refs/tags/docs-v0.1.0^{}"], setup), commit);
  assert.notEqual(git(["ls-remote", "--refs", "origin", "refs/tags/docs-v0.1.0"], setup), "");
});

test("preserve the local tag and report recovery after a failed push", t => {
  const setup = fixture(t);
  const result = release(setup, "docs", [], { ...setup.env, RELEASE_TEST_REJECT_PUSH: "1" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /local tag remains/);
  assert.match(result.stderr, /git ls-remote --tags origin/);
  git(["show-ref", "--verify", "--quiet", "refs/tags/docs-v0.1.0"], setup);
  assert.equal(git(["ls-remote", "--refs", "origin", "refs/tags/docs-v0.1.0"], setup), "");
});

test("reject unsafe versions, unknown channels and unsupported arguments before Git writes", t => {
  const setup = fixture(t);
  for (const channel of ["docs", "binary"]) {
    for (const version of [" ", "01.2.3", "1.2", "1.2.3;touch injected", "../1.2.3", "1.2.3-rc.1", "12345678901234567890123.2.3"]) {
      const result = release(setup, channel, [], { ...setup.env, VERSION: version });
      assert.equal(result.status, 1, `${channel} ${version}`);
      assert.match(result.stderr, /VERSION must be a release number/);
    }
  }
  for (const args of [["docs", "--unknown"], ["docs", "--check", "extra"], ["artifact"], ["constructor"], []]) {
    const result = command(process.execPath, [releaseScript, ...args], { cwd: setup.repo, env: setup.env });
    assert.equal(result.status, 1, args.join(" "));
    assert.match(result.stderr, /Usage: .*docs\|site\|binary/);
  }
  assert.deepEqual(ranChecks(setup), []);
  assert.equal(git(["tag", "--list"], setup), "");
});
