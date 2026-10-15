import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Release tags go to this repository; it is the one place to change when the source moves.
const expectedRepository = "griffinqiu/tickloop";
const allowedRemoteUrls = new Set([
  `git@github.com:${expectedRepository}`,
  `git@github.com:${expectedRepository}.git`,
  `ssh://git@github.com/${expectedRepository}`,
  `ssh://git@github.com/${expectedRepository}.git`,
  `https://github.com/${expectedRepository}`,
  `https://github.com/${expectedRepository}.git`,
]);
const versionPattern = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const maximumVersionLength = 26;
const firstVersion = "0.1.0";
const workspaceManifest = "Cargo.toml";
const workspacePackageSection = "[workspace.package]";
const workspaceVersionLine = /^version\s*=\s*"([^"]*)"\s*(#.*)?$/;

// Docs and the site take the next patch after their highest tag; binaries carry the Cargo
// workspace version, so their tag follows the committed Cargo.toml. Checks run in a fresh checkout
// of the tagged commit, so each channel installs its own dependencies there.
const channels = {
  docs: {
    label: "Docs",
    tagPrefix: "docs-v",
    workflow: ".github/workflows/docs-deploy.yml",
    versionFromWorkspace: false,
    checks: [
      { name: "dependency install", command: "npm", args: ["--prefix", "artifact", "ci"] },
      { name: "typecheck", command: "npm", args: ["--prefix", "artifact", "run", "typecheck"] },
      { name: "build", command: "npm", args: ["--prefix", "artifact", "run", "build"] },
    ],
    outcome: "GitHub Actions will deploy Docs.",
  },
  site: {
    label: "Site",
    tagPrefix: "site-v",
    workflow: ".github/workflows/site-deploy.yml",
    versionFromWorkspace: false,
    checks: [
      { name: "dependency install", command: "npm", args: ["--prefix", "site", "ci"] },
      { name: "build", command: "npm", args: ["--prefix", "site", "run", "build"] },
    ],
    outcome: "GitHub Actions will deploy the site.",
  },
  binary: {
    label: "Tickloop",
    tagPrefix: "v",
    workflow: ".github/workflows/release.yml",
    versionFromWorkspace: true,
    checks: [{ name: "build", command: "make", args: ["build"] }],
    outcome: "GitHub Actions will build the macOS archives and publish them with the formula on the Homebrew tap.",
  },
};

function git(args, { readOnly = true } = {}) {
  const result = spawnSync("git", args, {
    encoding: "utf8",
    env: readOnly ? { ...process.env, GIT_OPTIONAL_LOCKS: "0" } : process.env,
  });
  if (result.error) throw new Error(`Could not start git: ${result.error.message}`);
  return result;
}

function gitOutput(args, failureMessage) {
  const result = git(args);
  if (result.status !== 0) throw new Error(failureMessage);
  return result.stdout.trim();
}

function preflight(channel) {
  const commit = gitOutput(["rev-parse", "--verify", "HEAD^{commit}"], "HEAD must be a committed snapshot.");
  const workflowType = gitOutput(["cat-file", "-t", `HEAD:${channel.workflow}`], `The committed snapshot must contain ${channel.workflow}.`);
  if (workflowType !== "blob") throw new Error(`The committed snapshot must contain ${channel.workflow} as a file.`);

  for (const args of [["remote", "get-url", "--all", "origin"], ["remote", "get-url", "--push", "--all", "origin"]]) {
    const urls = gitOutput(args, "The origin remote must be configured.").split("\n");
    if (urls.length !== 1 || !allowedRemoteUrls.has(urls[0])) {
      throw new Error(`origin must have exactly one fetch and one push URL for github.com/${expectedRepository}.`);
    }
  }
  return commit;
}

function nextVersion(channel) {
  const { label, tagPrefix } = channel;
  const localTags = gitOutput(["tag", "--list", `${tagPrefix}*`], `Could not inspect local ${label} tags.`).split("\n");
  const remoteTags = gitOutput(["ls-remote", "--refs", "--tags", "origin", `refs/tags/${tagPrefix}*`], `Could not check ${label} tags on origin. Check remote access before releasing.`)
    .split("\n").map(line => line.split("\t")[1]?.replace(/^refs\/tags\//, "") ?? "");
  let latest;
  for (const tag of [...localTags, ...remoteTags]) {
    if (!tag.startsWith(tagPrefix)) continue;
    const version = tag.slice(tagPrefix.length);
    if (!versionPattern.test(version)) continue;
    const parts = version.split(".").map(part => BigInt(part));
    const difference = latest ? parts.findIndex((part, index) => part !== latest[index]) : -1;
    if (!latest || difference !== -1 && parts[difference] > latest[difference]) latest = parts;
  }
  const version = latest ? `${latest[0]}.${latest[1]}.${latest[2] + 1n}` : firstVersion;
  if (version.length > maximumVersionLength) throw new Error(`The next ${label} version exceeds the supported length; specify VERSION explicitly.`);
  return version;
}

function workspaceVersion(commit) {
  const manifest = gitOutput(["show", `${commit}:${workspaceManifest}`], `The committed snapshot must contain ${workspaceManifest}.`);
  let inWorkspacePackage = false;
  let version;
  for (const line of manifest.split("\n").map(text => text.trim())) {
    if (line.startsWith("[")) {
      inWorkspacePackage = line === workspacePackageSection;
      continue;
    }
    const match = inWorkspacePackage && line.match(workspaceVersionLine);
    if (match) {
      version = match[1];
      break;
    }
  }
  if (!version || !versionPattern.test(version) || version.length > maximumVersionLength) {
    throw new Error(`The committed ${workspaceManifest} must set ${workspacePackageSection} version to a release number such as 0.2.0.`);
  }
  return version;
}

function releaseVersion(channel, commit, requestedVersion) {
  if (!channel.versionFromWorkspace) return requestedVersion || nextVersion(channel);
  const version = workspaceVersion(commit);
  if (requestedVersion && requestedVersion !== version) {
    throw new Error(`VERSION ${requestedVersion} does not match the workspace version ${version} in the committed ${workspaceManifest}; commit the version change before releasing.`);
  }
  return version;
}

function assertUnusedTag(tag) {
  const ref = `refs/tags/${tag}`;
  const localTag = git(["show-ref", "--verify", "--quiet", ref]);
  if (localTag.status === 0) throw new Error(`${tag} already exists locally; inspect it before trying another release.`);
  if (localTag.status !== 1) throw new Error("Could not inspect local tags.");

  const remoteTag = git(["ls-remote", "--exit-code", "--refs", "origin", ref]);
  if (remoteTag.status === 0) throw new Error(`${tag} already exists on origin; inspect it before trying another release.`);
  if (remoteTag.status !== 2) throw new Error("Could not check tags on origin. Check remote access before releasing.");
  return ref;
}

// Cargo reuses this checkout's target directory, so the snapshot recompiles only workspace crates.
function runChecks(channel, commit) {
  const repositoryRoot = gitOutput(["rev-parse", "--show-toplevel"], "Could not locate the repository root.");
  const snapshotParent = mkdtempSync(join(tmpdir(), "tickloop-release-"));
  const snapshot = join(snapshotParent, "snapshot");
  const added = git(["worktree", "add", "--detach", snapshot, commit], { readOnly: false });
  try {
    if (added.status !== 0) throw new Error(`Could not check out ${commit} for ${channel.label} checks: ${added.stderr.trim() || "git worktree add failed"}`);
    const env = { ...process.env, CARGO_TARGET_DIR: process.env.CARGO_TARGET_DIR || join(repositoryRoot, "target") };
    for (const { name, command, args } of channel.checks) {
      const result = spawnSync(command, args, { cwd: snapshot, env, stdio: "inherit" });
      if (result.error) throw new Error(`Could not start ${channel.label} ${name}: ${result.error.message}`);
      if (result.status !== 0) throw new Error(`${channel.label} ${name} failed; no release tag was created.`);
    }
  } finally {
    if (added.status !== 0 || git(["worktree", "remove", "--force", snapshot], { readOnly: false }).status === 0) {
      rmSync(snapshotParent, { recursive: true, force: true });
    } else {
      process.stderr.write(`Could not remove the temporary worktree; remove it with git worktree remove --force ${snapshot}\n`);
    }
  }
}

function main() {
  const [channelName, mode, ...extraArguments] = process.argv.slice(2);
  const channel = Object.hasOwn(channels, channelName ?? "") ? channels[channelName] : undefined;
  if (!channel || extraArguments.length > 0 || (mode && mode !== "--check" && mode !== "--dry-run")) {
    throw new Error(`Usage: [VERSION=0.1.0] node scripts/release.mjs ${Object.keys(channels).join("|")} [--check|--dry-run]`);
  }

  const requestedVersion = process.env.VERSION ?? "";
  if (requestedVersion && (!versionPattern.test(requestedVersion) || requestedVersion.length > maximumVersionLength)) {
    throw new Error("VERSION must be a release number such as 0.1.0 (three numeric parts, no leading zeroes).");
  }

  const commit = preflight(channel);
  const version = releaseVersion(channel, commit, requestedVersion);
  const tag = `${channel.tagPrefix}${version}`;
  const ref = assertUnusedTag(tag);
  if (mode) {
    process.stdout.write(`Ready to release ${tag} from ${commit}. No Git changes were made.\n`);
    return;
  }

  runChecks(channel, commit);
  const checkedCommit = preflight(channel);
  if (checkedCommit !== commit) throw new Error(`HEAD changed during ${channel.label} checks (${commit} to ${checkedCommit}); no release tag was created.`);
  if (!channel.versionFromWorkspace && !requestedVersion && nextVersion(channel) !== version) {
    throw new Error(`${channel.label} release tags changed during checks; rerun the release to select the next version. No release tag was created.`);
  }
  assertUnusedTag(tag);

  const created = git(["tag", "-a", tag, "-m", `${channel.label} release ${version}`, commit], { readOnly: false });
  if (created.status !== 0) throw new Error(`Could not create ${tag}: ${created.stderr.trim() || "git tag failed"}`);

  const pushed = git(["push", "--porcelain", "origin", `${ref}:${ref}`], { readOnly: false });
  if (pushed.status !== 0) {
    throw new Error(`Push of ${tag} failed. The local tag remains at ${commit}. Check the remote tag with git ls-remote --tags origin ${ref}; if absent, push only ${ref} after resolving the error. Git reported: ${pushed.stderr.trim() || pushed.stdout.trim() || "git push failed"}`);
  }
  process.stdout.write(`Published ${tag} from ${commit} to origin. ${channel.outcome}\n`);
}

try {
  main();
} catch (error) {
  process.stderr.write(`${error.message}\n`);
  process.exitCode = 1;
}
