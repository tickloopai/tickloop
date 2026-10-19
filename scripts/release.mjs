import { spawnSync } from "node:child_process";
import { closeSync, existsSync, mkdtempSync, openSync, readFileSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { compareVersions, nextVersion, readProducts, versionChanges } from "./release-versions.mjs";

const expectedRepository = "griffinqiu/tickloop";
const marketplaceRepository = "tickloopai/ticks";
const allowedRemoteUrls = new Set([
  `git@github.com:${expectedRepository}`, `git@github.com:${expectedRepository}.git`,
  `ssh://git@github.com/${expectedRepository}`, `ssh://git@github.com/${expectedRepository}.git`,
  `https://github.com/${expectedRepository}`, `https://github.com/${expectedRepository}.git`,
]);
// GitHub does not create tag-push events when a push updates more than three tags.
const maximumTagsPerPush = 3;
const maximumSnapshotAttempts = 3;
const releaseLockPollMilliseconds = 100;
const channels = new Set(["binary", "ticks"]);

function command(program, args, options = {}) {
  const result = spawnSync(program, args, { encoding: "utf8", ...options });
  if (result.error) throw new Error(`Could not start ${program}: ${result.error.message}`);
  return result;
}

function git(args, { readOnly = true, ...options } = {}) {
  return command("git", args, { env: readOnly ? { ...process.env, GIT_OPTIONAL_LOCKS: "0" } : process.env, ...options });
}

function gitOutput(args, failureMessage, options) {
  const result = git(args, options);
  if (result.status !== 0) throw new Error(`${failureMessage}${result.stderr?.trim() ? ` ${result.stderr.trim()}` : ""}`);
  return result.stdout.trim();
}

function acquireReleaseLock() {
  const directory = gitOutput(["rev-parse", "--path-format=absolute", "--git-common-dir"], "Could not locate the shared Git directory.");
  const path = join(directory, "tickloop-release.lock");
  const token = randomUUID();
  let waiting = false;
  let incompleteReads = 0;
  while (true) {
    let descriptor;
    try {
      descriptor = openSync(path, "wx", 0o600);
    } catch (error) {
      if (error.code !== "EEXIST") throw error;
      let owner;
      try { owner = JSON.parse(readFileSync(path, "utf8")); }
      catch (error) {
        if (error.code === "ENOENT") continue;
        if (++incompleteReads >= 10) throw new Error(`Release lock ${path} has no readable owner; inspect it before removing it.`);
      }
      if (owner) {
        incompleteReads = 0;
        if (!Number.isInteger(owner.pid) || owner.pid <= 0) throw new Error(`Release lock ${path} has an invalid owner.`);
        try { process.kill(owner.pid, 0); }
        catch (error) {
          if (error.code === "ESRCH") throw new Error(`Release lock ${path} belongs to exited process ${owner.pid}; inspect and remove that stale lock before retrying.`);
          if (error.code !== "EPERM") throw error;
        }
      }
      if (!waiting) { process.stderr.write("Another release is running; waiting for the shared release lock.\n"); waiting = true; }
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, releaseLockPollMilliseconds);
      continue;
    }
    try { writeFileSync(descriptor, JSON.stringify({ pid: process.pid, token })); }
    catch (error) { closeSync(descriptor); unlinkSync(path); throw error; }
    closeSync(descriptor);
    let released = false;
    const release = () => {
      if (released) return;
      released = true;
      try { if (JSON.parse(readFileSync(path, "utf8")).token === token) unlinkSync(path); }
      catch (error) { if (error.code !== "ENOENT") process.stderr.write(`Could not release ${path}: ${error.message}\n`); }
    };
    process.once("exit", release);
    return () => {
      release();
      process.off("exit", release);
    };
  }
}

function releaseSnapshotIsCurrent(state) {
  const current = preflight();
  if (current.branch !== state.branch) throw new Error("The branch changed during checks; no version commit or release tag was created.");
  return current.commit === state.commit;
}

function preflight() {
  const commit = gitOutput(["rev-parse", "--verify", "HEAD^{commit}"], "HEAD must be a committed snapshot.");
  const branch = gitOutput(["symbolic-ref", "--quiet", "HEAD"], "Release requires a branch; detached HEAD cannot receive a version commit.");
  const root = gitOutput(["rev-parse", "--show-toplevel"], "Could not locate the repository root.");
  for (const state of ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "rebase-merge", "rebase-apply"]) {
    const path = gitOutput(["rev-parse", "--git-path", state], "Could not inspect Git operation state.");
    if (existsSync(path)) throw new Error("Finish the active Git operation before releasing.");
  }
  for (const args of [["remote", "get-url", "--all", "origin"], ["remote", "get-url", "--push", "--all", "origin"]]) {
    const urls = gitOutput(args, "The origin remote must be configured.").split("\n");
    if (urls.length !== 1 || !allowedRemoteUrls.has(urls[0])) throw new Error(`origin must have exactly one fetch and one push URL for github.com/${expectedRepository}.`);
  }
  return { commit, branch, root };
}

function assertVersionFilesUnchanged(paths) {
  const changes = gitOutput(["status", "--porcelain=v1", "--untracked-files=all", "--", ...paths], "Could not inspect the release version files.");
  if (changes) throw new Error(`Release version files have local edits that would be overwritten; other working-tree changes are allowed:\n${changes}`);
}

function assertUnusedTag(tag, remote = "origin") {
  const ref = `refs/tags/${tag}`;
  if (remote === "origin") {
    const result = git(["show-ref", "--verify", "--quiet", ref]);
    if (result.status === 0) throw new Error(`${tag} already exists locally.`);
    if (result.status !== 1) throw new Error("Could not inspect local tags.");
  }
  const result = git(["ls-remote", "--exit-code", "--refs", remote, ref]);
  if (result.status === 0) throw new Error(`${tag} already exists on ${remote}.`);
  if (result.status !== 2) throw new Error(`Could not check tags on ${remote}; no release was started.`);
}

function checkReleaseTargets(products) {
  for (const product of products) {
    const type = gitOutput(["cat-file", "-t", `HEAD:${product.workflow}`], `The committed snapshot must contain ${product.workflow}.`);
    if (type !== "blob") throw new Error(`The committed snapshot must contain ${product.workflow} as a file.`);
    const tags = gitOutput(["tag", "--list", `${product.tagPrefix}*`], "Could not inspect local release tags.").split("\n");
    const remoteTags = gitOutput(["ls-remote", "--refs", "--tags", "origin", `refs/tags/${product.tagPrefix}*`], `Could not check ${product.label} tags on origin.`);
    tags.push(...remoteTags.split("\n").map(line => line.split("\t")[1]?.replace(/^refs\/tags\//, "") ?? ""));
    for (const tag of tags) {
      if (!tag.startsWith(product.tagPrefix)) continue;
      const version = tag.slice(product.tagPrefix.length);
      if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version)) continue;
      if (compareVersions(product.nextVersion, version) <= 0) throw new Error(`${product.tag} must be newer than existing tag ${tag}; synchronize the version files or set VERSION explicitly.`);
    }
    assertUnusedTag(product.tag);
  }
  const ticks = products.filter(product => product.kind === "tick");
  if (ticks.length) {
    const response = command("curl", ["--fail", "--silent", "--show-error", "--location", "--max-time", "30", `https://raw.githubusercontent.com/${marketplaceRepository}/HEAD/marketplace.json`]);
    if (response.status !== 0) throw new Error("Could not read the Tick marketplace; no release was started.");
    const marketplace = JSON.parse(response.stdout);
    if (!Array.isArray(marketplace.packages)) throw new Error("The Tick marketplace has no package list.");
    for (const product of ticks) {
      const listed = marketplace.packages.find(entry => entry.name === product.id);
      if (listed && compareVersions(product.nextVersion, listed.version) <= 0) throw new Error(`${product.label} ${product.nextVersion} is not newer than marketplace version ${listed.version}.`);
      assertUnusedTag(`${product.id}-v${product.nextVersion}`, `https://github.com/${marketplaceRepository}`);
    }
  }
}

function checksFor(channel, products) {
  if (channel === "binary") return [
    ["make", ["build"]],
    ["cargo", ["package", "--locked", "--allow-dirty", "--no-verify", "--manifest-path", "distribution/cargo/Cargo.toml", "--target-dir", "target/cargo-launcher"]],
  ];
  return [["cargo", ["check", "--locked", ...products.flatMap(product => ["-p", product.cargoName]), "--bins"]]];
}

function prepareAndCheck(channel, state, mode) {
  const parent = mkdtempSync(join(tmpdir(), "tickloop-release-"));
  const snapshot = join(parent, "snapshot");
  const added = git(["worktree", "add", "--detach", snapshot, state.commit], { readOnly: false });
  try {
    if (added.status !== 0) throw new Error(`Could not create the release checkout: ${added.stderr.trim()}`);
    const products = readProducts(snapshot, channel, process.env.TICK_PACKAGES).map(product => {
      const version = nextVersion(product, process.env.VERSION);
      return { ...product, nextVersion: version, tag: `${product.tagPrefix}${version}` };
    });
    checkReleaseTargets(products);
    const changes = versionChanges(snapshot, products);
    assertVersionFilesUnchanged([...changes.keys()]);
    if (mode) return { products, changes };
    for (const [path, content] of changes) writeFileSync(join(snapshot, path), content);
    const env = { ...process.env, CARGO_TARGET_DIR: process.env.CARGO_TARGET_DIR || join(state.root, "target") };
    for (const [program, args] of checksFor(channel, products)) {
      const result = command(program, args, { cwd: snapshot, env, stdio: "inherit" });
      if (result.status !== 0) throw new Error(`${channel} ${program} check failed; no version commit or release tag was created.`);
    }
    const changed = gitOutput(["diff", "--name-only", "HEAD"], "Could not inspect the checked release files.", { cwd: snapshot }).split("\n");
    if (changed.length !== changes.size || changed.some(path => !changes.has(path))) throw new Error("Release checks modified files outside the version plan.");
    for (const [path, content] of changes) if (readFileSync(join(snapshot, path), "utf8") !== content) throw new Error(`Release checks changed ${path} after its version was prepared.`);
    return { products, changes };
  } finally {
    if (added.status !== 0 || git(["worktree", "remove", "--force", snapshot], { readOnly: false }).status === 0) rmSync(parent, { recursive: true, force: true });
    else process.stderr.write(`Could not remove the temporary worktree at ${snapshot}.\n`);
  }
}

function finalizeRelease(products, state, changes) {
  if (!releaseSnapshotIsCurrent(state)) return false;
  checkReleaseTargets(products);
  const paths = [...changes.keys()];
  assertVersionFilesUnchanged(paths);
  if (!releaseSnapshotIsCurrent(state)) return false;
  const description = products.map(product => `${product.label} ${product.nextVersion}`).join(", ");
  for (const [path, content] of changes) writeFileSync(join(state.root, path), content);
  const created = git(["commit", "--only", "-m", `chore(release): ${description}`, "--", ...paths], { readOnly: false });
  if (created.status !== 0) throw new Error(`Version commit failed; version files are retained for inspection, and no tag was created. ${created.stderr.trim()}`);
  const commit = gitOutput(["rev-parse", "HEAD"], "Could not read the release commit.");
  const changed = gitOutput(["diff", "--name-only", state.commit, commit], "Could not verify the release commit.").split("\n");
  if (gitOutput(["rev-parse", "HEAD^"], "Could not verify the release parent.") !== state.commit || changed.length !== paths.length || changed.some(path => !changes.has(path))) throw new Error(`Release commit ${commit} contains unexpected changes; no tags were created. Inspect it before continuing.`);
  for (const [path, content] of changes) {
    const committed = git(["show", `${commit}:${path}`]);
    if (committed.status !== 0 || committed.stdout !== content) throw new Error(`Release commit ${commit} does not contain the checked ${path}; no tags were created.`);
  }
  const afterCommit = preflight();
  if (afterCommit.commit !== commit || afterCommit.branch !== state.branch) throw new Error("The release branch changed during the version commit; no tags were created.");
  for (const product of products) {
    const result = git(["tag", "-a", product.tag, "-m", `${product.label} ${product.nextVersion}`, commit], { readOnly: false });
    if (result.status !== 0) throw new Error(`Release commit ${commit} remains, but ${product.tag} could not be created. Inspect local tags before continuing. ${result.stderr.trim()}`);
  }
  for (let index = 0; index < products.length; index += maximumTagsPerPush) {
    const group = products.slice(index, index + maximumTagsPerPush);
    const refspecs = [`${commit}:${state.branch}`, ...group.map(product => `refs/tags/${product.tag}:refs/tags/${product.tag}`)];
    const pushed = git(["push", "--porcelain", "--atomic", "origin", ...refspecs], { readOnly: false });
    if (pushed.status !== 0) throw new Error(`Push failed; release commit ${commit} and local tags remain. Earlier tag batches may already be published. Inspect remote refs with git ls-remote origin ${state.branch} ${products.map(product => `refs/tags/${product.tag}`).join(" ")}, then retry only the missing refs with git push --atomic origin ${refspecs.join(" ")}. Do not rerun release to bump versions again. ${pushed.stderr.trim()}`);
  }
  process.stdout.write(`Committed ${description} as ${commit}. Published ${products.map(product => product.tag).join(", ")} with ${state.branch.slice("refs/heads/".length)}. GitHub Actions will build and deploy the releases.\n`);
  return true;
}

try {
  const [channel, mode, ...extra] = process.argv.slice(2);
  if (!channels.has(channel) || extra.length || mode && !["--check", "--dry-run"].includes(mode)) throw new Error("Usage: [VERSION=X.Y.Z] [TICK_PACKAGES='longbridge futu'] node scripts/release.mjs binary|ticks [--check|--dry-run]");
  const releaseLock = acquireReleaseLock();
  try {
    const initial = preflight();
    for (let attempt = 0; attempt < maximumSnapshotAttempts; attempt++) {
      const state = preflight();
      if (state.branch !== initial.branch) throw new Error("The release branch changed while preparing the snapshot.");
      const { products, changes } = prepareAndCheck(channel, state, mode);
      if (mode) {
        process.stdout.write(`Ready to bump ${products.map(product => `${product.label} ${product.version} → ${product.nextVersion}`).join(", ")}, create one version commit and tags ${products.map(product => product.tag).join(", ")}, then push ${state.branch.slice("refs/heads/".length)} and tags. No changes were made.\n`);
        break;
      }
      if (finalizeRelease(products, state, changes)) break;
      if (attempt + 1 === maximumSnapshotAttempts) throw new Error("HEAD kept advancing during release checks; no version commit or release tag was created. Retry when commits have settled.");
      process.stderr.write("HEAD advanced during checks; preparing and checking the latest committed snapshot again.\n");
    }
  } finally {
    releaseLock();
  }
} catch (error) {
  process.stderr.write(`${error.message}\n`);
  process.exitCode = 1;
}
