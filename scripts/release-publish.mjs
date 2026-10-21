// Publishes an existing vX.Y.Z tag from this Mac: checks the tagged source on macOS and Linux,
// builds the four archives, publishes them on the tap, updates the Homebrew formula and publishes
// the Cargo launcher. --remote starts .github/workflows/release.yml on GitHub Actions instead.
// Every step can run again: built archives are reused, and published assets, the formula and the
// crate are only ever added when missing and refused when they differ.
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repository = "tickloopai/tickloop";
const tapRepository = "tickloopai/homebrew-tap";
const programs = ["tickhost", "tickloop"];
const linuxImage = "tickloop-release-linux";
export const steps = ["prepare", "check", "build", "publish", "tap", "cargo"];
export const targets = [
  { triple: "aarch64-apple-darwin", arch: "arm64" },
  { triple: "x86_64-apple-darwin", arch: "x86_64" },
  { triple: "aarch64-unknown-linux-musl", platform: "linux/arm64" },
  { triple: "x86_64-unknown-linux-musl", platform: "linux/amd64" },
];
const usage = "Usage: node scripts/release-publish.mjs [--remote] [--from STEP] vX.Y.Z";

export function parseArguments(argv) {
  const options = { remote: false, from: steps[0], tag: undefined };
  for (let index = 0; index < argv.length; index++) {
    const argument = argv[index];
    if (argument === "--remote") options.remote = true;
    else if (argument === "--from") options.from = argv[++index];
    else if (options.tag === undefined) options.tag = argument;
    else throw new Error(usage);
  }
  if (!/^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(options.tag ?? "")) throw new Error(usage);
  if (!steps.includes(options.from)) throw new Error(`--from must be one of ${steps.join(", ")}.`);
  if (options.remote && options.from !== steps[0]) throw new Error("--from applies to local publishing only; rerun failed jobs on GitHub instead.");
  return options;
}

function run(program, args, options = {}) {
  const result = spawnSync(program, args, { stdio: "inherit", ...options });
  if (result.error) throw new Error(`Could not start ${program}: ${result.error.message}`);
  if (result.status !== 0) throw new Error(`${program} ${args.join(" ")} failed with status ${result.status}.`);
}

function output(program, args, options = {}) {
  const result = spawnSync(program, args, { encoding: "utf8", maxBuffer: 64 * 1024 * 1024, ...options });
  if (result.error) throw new Error(`Could not start ${program}: ${result.error.message}`);
  if (result.status !== 0) throw new Error(`${program} ${args.join(" ")} failed: ${result.stderr.trim()}`);
  return result.stdout.trim();
}

function succeeds(program, args, options = {}) {
  return spawnSync(program, args, { stdio: "ignore", ...options }).status === 0;
}

function section(title) {
  process.stdout.write(`\n==> ${title}\n`);
}

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

export function checksumLine(path, name) {
  return `${sha256(path)}  ${name}\n`;
}

// The commit a tag names here, which must be the one it names on origin.
function taggedCommit(tag) {
  const local = output("git", ["-C", root, "rev-parse", "--verify", `refs/tags/${tag}^{commit}`]);
  const listing = output("git", ["-C", root, "ls-remote", "--tags", "origin", `refs/tags/${tag}*`]);
  const refs = new Map(listing.split("\n").filter(Boolean).map(line => line.split("\t").reverse()));
  // An annotated tag lists its own object and, as ^{}, the commit it points at.
  const remote = refs.get(`refs/tags/${tag}^{}`) ?? refs.get(`refs/tags/${tag}`);
  if (!remote) throw new Error(`${tag} is not on origin; push it before publishing.`);
  if (remote !== local) throw new Error(`${tag} names ${local} here but ${remote} on origin.`);
  return local;
}

// A reusable worktree of the tag, so the Rust builds stay incremental across releases.
function checkOut(source, tag, commit) {
  if (existsSync(join(source, ".git"))) {
    run("git", ["-C", source, "checkout", "--quiet", "--detach", "--force", commit]);
    run("git", ["-C", source, "clean", "-ffdxq", "-e", "/target"]);
  } else {
    run("git", ["-C", root, "worktree", "add", "--detach", source, tag]);
  }
  if (output("git", ["-C", source, "rev-parse", "HEAD"]) !== commit) throw new Error(`The release worktree is not at ${tag}.`);
}

function acquireLock(path) {
  mkdirSync(dirname(path), { recursive: true });
  if (existsSync(path)) {
    const owner = Number(readFileSync(path, "utf8"));
    let alive = false;
    try { process.kill(owner, 0); alive = true; } catch {}
    if (alive) throw new Error(`Another local release (process ${owner}) is running; wait for it to finish.`);
  }
  writeFileSync(path, String(process.pid));
  return () => rmSync(path, { force: true });
}

function linux(source, platform, script, { environment = {} } = {}) {
  const arch = platform.split("/")[1];
  const image = `${linuxImage}:${arch}`;
  run("docker", ["build", "--quiet", "--platform", platform, "--tag", image,
    "--file", join(source, "scripts/release/linux.Dockerfile"), join(source, "scripts/release")], { stdio: ["ignore", "ignore", "inherit"] });
  const workspace = "/home/builder/tickloop";
  // Named volumes keep Linux build products apart from the macOS ones in the same worktree.
  const volumes = ["--volume", `tickloop-release-registry-${arch}:/usr/local/cargo/registry`,
    "--volume", `tickloop-release-target-${arch}:${workspace}/target`];
  run("docker", ["run", "--rm", "--platform", platform, ...volumes, image,
    "chown", "builder:builder", "/usr/local/cargo/registry", `${workspace}/target`]);
  const variables = Object.entries({ HOME: "/home/builder", ...environment }).flatMap(([name, value]) => ["--env", `${name}=${value}`]);
  run("docker", ["run", "--rm", "--platform", platform, "--user", "builder", ...variables, ...volumes,
    "--volume", `${source}:${workspace}`, "--workdir", workspace, image, "bash", "-euo", "pipefail", "-c", script]);
}

function prepare(source, version) {
  if (process.platform !== "darwin" || process.arch !== "arm64") throw new Error("Local releases build every archive on an Apple silicon Mac.");
  const workspace = readFileSync(join(source, "Cargo.toml"), "utf8");
  const workspaceVersion = workspace.match(/\[workspace\.package\][\s\S]*?^version = "([^"]+)"/m)?.[1];
  if (workspaceVersion !== version) throw new Error(`The tagged workspace version is ${workspaceVersion}, not ${version}.`);
  if (!succeeds("arch", ["-x86_64", "/usr/bin/true"])) throw new Error("Rosetta is required to test the Intel archive: softwareupdate --install-rosetta --agree-to-license");
  if (!succeeds("docker", ["info"])) throw new Error("Docker (OrbStack) must be running to check and build the Linux archives.");
  if (output("gh", ["api", `repos/${tapRepository}`, "--jq", ".permissions.push"]) !== "true") throw new Error(`gh must be signed in with push access to ${tapRepository}.`);
  if (output("gh", ["api", `repos/${tapRepository}/branches?per_page=1`, "--jq", "length"]) === "0") throw new Error(`${tapRepository} has no commits, so a release tag cannot be created there.`);
  const cargoHome = process.env.CARGO_HOME ?? join(homedir(), ".cargo");
  if (!process.env.CARGO_REGISTRY_TOKEN && !existsSync(join(cargoHome, "credentials.toml"))) throw new Error("Sign in to crates.io with cargo login, or set CARGO_REGISTRY_TOKEN.");
  run("node", ["--test", "scripts/release.test.mjs", "scripts/install.test.mjs", "scripts/release-cargo.test.mjs", "scripts/release-publish.test.mjs"], { cwd: source });
  run("node", ["scripts/release-cargo.mjs", "--check"], { cwd: source });
}

function check(source) {
  // One after the other: concurrent full test runs time out each other's PTY tests.
  run("make", ["check-workspace"], { cwd: source, env: { ...process.env, NEXTEST_PROFILE: "ci" } });
  linux(source, "linux/arm64",
    "make check-workspace && cargo clippy --locked --workspace --all-targets --target aarch64-unknown-linux-musl -- -D warnings",
    { environment: { NEXTEST_PROFILE: "ci" } });
}

const archiveCheck = `
  expected=$(printf '%s\\n' ${programs.join(" ")})
  [ "$(tar -tzf "$archive" | LC_ALL=C sort)" = "$expected" ] || { echo "$archive must hold exactly: ${programs.join(" ")}" >&2; exit 1; }
`;

function build(source, out) {
  mkdirSync(out, { recursive: true });
  for (const target of targets) {
    const name = `tickloop-${target.triple}.tar.gz`;
    const archive = join(out, name);
    const checksum = `${archive}.sha256`;
    if (existsSync(archive) && existsSync(checksum) && readFileSync(checksum, "utf8") === checksumLine(archive, name)) {
      process.stdout.write(`Reusing ${name}.\n`);
      continue;
    }
    section(`Build ${target.triple}`);
    rmSync(join(source, "dist", name), { force: true });
    if (target.platform) {
      linux(source, target.platform, `
        make package PACKAGE_TARGET=${target.triple}
        archive=dist/${name}
        ${archiveCheck}
        unpacked=$(mktemp -d) && tar -xzf "$archive" -C "$unpacked"
        for program in ${programs.join(" ")}; do
          "$unpacked/$program" --help > /dev/null
          if file "$unpacked/$program" | grep -q 'dynamically linked'; then echo "$program is not a static program." >&2; exit 1; fi
        done`);
    } else {
      run("rustup", ["target", "add", target.triple]);
      run("make", ["package", `PACKAGE_TARGET=${target.triple}`], { cwd: source });
      const unpacked = mkdtempSync(join(tmpdir(), "tickloop-release-smoke-"));
      try {
        run("sh", ["-euc", `archive=$1\n${archiveCheck}\ntar -xzf "$archive" -C "$2"`, "sh", join(source, "dist", name), unpacked]);
        // `arch` runs each program as the target's architecture, so a wrong-architecture archive fails.
        for (const program of programs) run("arch", [`-${target.arch}`, join(unpacked, program), "--help"], { stdio: ["ignore", "ignore", "inherit"] });
      } finally {
        rmSync(unpacked, { recursive: true, force: true });
      }
    }
    copyFileSync(join(source, "dist", name), archive);
    writeFileSync(checksum, checksumLine(archive, name));
  }
}

function assets(out) {
  return targets.flatMap(target => {
    const archive = join(out, `tickloop-${target.triple}.tar.gz`);
    if (!existsSync(archive) || readFileSync(`${archive}.sha256`, "utf8") !== checksumLine(archive, `tickloop-${target.triple}.tar.gz`)) {
      throw new Error(`${archive} is missing or does not match its checksum; run the build step again.`);
    }
    return [archive, `${archive}.sha256`];
  });
}

function publish(out, tag) {
  const files = assets(out);
  if (succeeds("gh", ["release", "view", tag, "--repo", tapRepository])) {
    // A published version is immutable; a rerun may only add missing identical assets.
    const published = new Set(output("gh", ["release", "view", tag, "--repo", tapRepository, "--json", "assets", "--jq", ".assets[].name"]).split("\n"));
    const scratch = mkdtempSync(join(tmpdir(), "tickloop-release-assets-"));
    try {
      for (const file of files) {
        const name = file.split("/").pop();
        if (!published.has(name)) {
          run("gh", ["release", "upload", tag, file, "--repo", tapRepository]);
          continue;
        }
        run("gh", ["release", "download", tag, "--repo", tapRepository, "--pattern", name, "--dir", scratch]);
        if (sha256(join(scratch, name)) !== sha256(file)) throw new Error(`Published asset ${name} differs; release a new version instead.`);
      }
    } finally {
      rmSync(scratch, { recursive: true, force: true });
    }
  } else {
    run("gh", ["release", "create", tag, ...files, "--repo", tapRepository, "--draft", "--title", `Tickloop ${tag.slice(1)}`,
      "--notes", `Prebuilt macOS and Linux archives of Tickloop ${tag.slice(1)}. Install with: brew install tickloopai/tap/tickloop`]);
  }
  run("gh", ["release", "edit", tag, "--repo", tapRepository, "--draft=false"]);
}

export function renderFormula(template, version, checksums) {
  const formula = template
    .replaceAll("@VERSION@", version)
    .replaceAll("@AARCH64_SHA256@", checksums["aarch64-apple-darwin"])
    .replaceAll("@X86_64_SHA256@", checksums["x86_64-apple-darwin"])
    .replaceAll("@LINUX_AARCH64_SHA256@", checksums["aarch64-unknown-linux-musl"])
    .replaceAll("@LINUX_X86_64_SHA256@", checksums["x86_64-unknown-linux-musl"]);
  const placeholder = formula.match(/@[A-Z0-9_]+@/);
  if (placeholder) throw new Error(`The rendered formula still has the placeholder ${placeholder[0]}.`);
  return formula;
}

function tap(source, out, version) {
  assets(out);
  const checksums = Object.fromEntries(targets.map(target =>
    [target.triple, readFileSync(join(out, `tickloop-${target.triple}.tar.gz.sha256`), "utf8").split(" ")[0]]));
  const formula = renderFormula(readFileSync(join(source, ".github/homebrew/tickloop.rb.in"), "utf8"), version, checksums);
  const clone = mkdtempSync(join(tmpdir(), "tickloop-homebrew-tap-"));
  try {
    run("gh", ["repo", "clone", tapRepository, clone, "--", "--depth", "1", "--quiet"]);
    mkdirSync(join(clone, "Formula"), { recursive: true });
    writeFileSync(join(clone, "Formula/tickloop.rb"), formula);
    run("ruby", ["-c", join(clone, "Formula/tickloop.rb")], { stdio: ["ignore", "ignore", "inherit"] });
    run("git", ["-C", clone, "add", "Formula/tickloop.rb"]);
    if (succeeds("git", ["-C", clone, "diff", "--cached", "--quiet"])) {
      process.stdout.write(`Formula/tickloop.rb already describes ${version}.\n`);
      return;
    }
    run("git", ["-C", clone, "commit", "--quiet", "-m", `tickloop ${version}`]);
    run("git", ["-C", clone, "push", "--quiet", "origin", "HEAD"]);
  } finally {
    rmSync(clone, { recursive: true, force: true });
  }
}

function cargo(source, version, tag) {
  const launcher = ["--locked", "--manifest-path", "distribution/cargo/Cargo.toml", "--target-dir", "target/cargo-launcher"];
  run("cargo", ["test", ...launcher], { cwd: source });
  run("cargo", ["clippy", ...launcher, "--all-targets", "--", "-D", "warnings"], { cwd: source });
  const verify = installRoot => {
    run("/usr/bin/sandbox-exec", ["-p", "(version 1) (allow default) (deny network*)", join(installRoot, "bin/tickloop"), "--help"], { stdio: ["ignore", "ignore", "inherit"] });
    run(join(installRoot, "bin/tickhost"), ["--help"], { stdio: ["ignore", "ignore", "inherit"] });
    run(join(installRoot, "bin/tl"), ["--version"]);
  };
  const scratch = mkdtempSync(join(tmpdir(), "tickloop-cargo-smoke-"));
  try {
    run("cargo", ["install", "--locked", "--path", "distribution/cargo", "--root", join(scratch, "local"), "--target-dir", "target/cargo-launcher"], { cwd: source });
    verify(join(scratch, "local"));
    run("node", ["scripts/release-cargo.mjs", "--publish"], { cwd: source, env: { ...process.env, TICKLOOP_RELEASE_TAG: tag } });
    run("cargo", ["install", "--locked", "--registry", "crates-io", "--version", version, "--root", join(scratch, "registry"), "--target-dir", "target/cargo-launcher", "tickloop"], { cwd: source });
    verify(join(scratch, "registry"));
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
  linux(source, "linux/arm64", `
    cargo install --locked --registry crates-io --version ${version} --root /tmp/cargo-smoke tickloop
    for program in ${programs.join(" ")}; do /tmp/cargo-smoke/bin/$program --help > /dev/null; done
    /tmp/cargo-smoke/bin/tl --version`);
}

function publishRemotely(tag) {
  taggedCommit(tag);
  run("gh", ["workflow", "run", "release.yml", "--repo", repository, "--ref", tag]);
  process.stdout.write(`Started the release workflow for ${tag} on GitHub Actions: https://github.com/${repository}/actions/workflows/release.yml\n`);
}

function publishLocally(tag, from) {
  const version = tag.slice(1);
  const work = join(root, "target/release-local");
  const release = acquireLock(join(work, "publish.lock"));
  try {
    const commit = taggedCommit(tag);
    const source = join(work, "source");
    const out = join(work, tag);
    checkOut(source, tag, commit);
    const actions = {
      prepare: () => prepare(source, version),
      check: () => check(source),
      build: () => build(source, out),
      publish: () => publish(out, tag),
      tap: () => tap(source, out, version),
      cargo: () => cargo(source, version, tag),
    };
    for (const step of steps.slice(steps.indexOf(from))) {
      section(`${step} (${tag})`);
      try {
        actions[step]();
      } catch (error) {
        throw new Error(`${error.message}\nResume with: make release-publish VERSION=${version} FROM=${step}`);
      }
    }
    process.stdout.write(`\nPublished Tickloop ${version}: https://github.com/${tapRepository}/releases/tag/${tag}, Homebrew and crates.io.\n`);
  } finally {
    release();
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const options = parseArguments(process.argv.slice(2));
    if (options.remote) publishRemotely(options.tag);
    else publishLocally(options.tag, options.from);
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  }
}
