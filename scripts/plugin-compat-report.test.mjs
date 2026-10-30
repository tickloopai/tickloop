import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { after, test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { checkGitArguments, checkout, cloneUrl, createReport, describeSource, parseValidateOutput, renderMarkdown, summarize, STATUS } from "./plugin-compat-report.mjs";

const script = fileURLToPath(new URL("./plugin-compat-report.mjs", import.meta.url));
const builtTickloop = fileURLToPath(new URL("../target/debug/tickloop", import.meta.url));
const scratch = mkdtempSync(join(tmpdir(), "plugin-compat-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

function git(cwd, ...args) {
  return execFileSync("git", ["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false", ...args], { cwd, encoding: "utf8" }).trim();
}

function writeFiles(root, files) {
  for (const [path, content] of Object.entries(files)) {
    mkdirSync(dirname(join(root, path)), { recursive: true });
    writeFileSync(join(root, path), typeof content === "string" ? content : `${JSON.stringify(content)}\n`);
  }
}

function commit(root, files) {
  writeFiles(root, files);
  git(root, "add", "--all");
  git(root, "commit", "--quiet", "-m", "change");
  return git(root, "rev-parse", "HEAD");
}

let repositoryCount = 0;
function repository(name) {
  const root = join(scratch, `${name}-${++repositoryCount}`);
  mkdirSync(root, { recursive: true });
  git(root, "init", "--quiet", "--initial-branch=main");
  return root;
}

const skill = name => `---\nname: ${name}\ndescription: Does ${name}\n---\nBody of ${name}\n`;

// Prints what `tickloop plugin validate` prints for the few manifest shapes the fixtures use.
function fakeTickloop() {
  const path = join(scratch, "fake-tickloop.mjs");
  writeFileSync(path, `#!/usr/bin/env node
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { basename, join } from "node:path";
if (process.argv[2] === "--version") { console.log("tickloop 0.0.0-fake"); process.exit(0); }
const directory = process.argv[4];
const found = [".tickloop-plugin/plugin.json", ".claude-plugin/plugin.json", "tick-package.json"].find(file => existsSync(join(directory, file)));
const manifest = found ? JSON.parse(readFileSync(join(directory, found), "utf8")) : { name: basename(directory) };
if (typeof manifest.name !== "string") {
  console.error(\`Error: \${directory} is not a valid plugin: name in \${found} must be a string\`);
  process.exit(1);
}
const identity = process.argv[5] === "--name" ? process.argv[6] : manifest.name;
if (!/^[a-z0-9][a-z0-9-]*$/.test(identity)) {
  console.error(\`Error: \${directory} is not a valid plugin: "\${manifest.name}" is not a plugin name\`);
  process.exit(1);
}
console.log(\`Plugin \${identity}\${manifest.version ? " " + manifest.version : ""}, read from \${found ?? "its directory"}.\`);
if (identity !== manifest.name) console.log(\`Its manifest calls it \${manifest.name}.\`);
const skills = existsSync(join(directory, "skills")) ? readdirSync(join(directory, "skills")).sort().map(name => "skills/" + name) : [];
console.log(\`Skills: \${skills.length ? skills.join(", ") : "none"}\`);
if (manifest.executable) console.log(\`Tick program: \${manifest.executable} (speaks the Tick protocol)\`);
const commands = existsSync(join(directory, "commands")) ? readdirSync(join(directory, "commands")).sort().map(name => name.replace(/\\.md$/, "")) : [];
console.log(\`Commands: \${commands.length ? commands.join(", ") : "none"}\`);
const unsupported = [[".mcp.json", "MCP servers"]].filter(([file]) => existsSync(join(directory, file))).map(([, label]) => label);
if (unsupported.length) console.log(\`Not supported yet, so not used: \${unsupported.join(", ")}.\`);
`);
  chmodSync(path, 0o755);
  return path;
}

function claudeMarketplace() {
  const external = repository("external");
  const externalCommit = commit(external, {
    ".claude-plugin/plugin.json": { name: "remote", version: "2.0.0" },
    "skills/lookup/SKILL.md": skill("lookup"),
    ".mcp.json": { mcpServers: {} },
  });
  const monorepo = repository("monorepo");
  const firstMonorepoCommit = commit(monorepo, { "plugins/pinned/skills/old/SKILL.md": skill("old"), "README.md": "monorepo\n" });
  const monorepoCommit = commit(monorepo, { "plugins/sub/skills/draft/SKILL.md": skill("draft"), "other/large.txt": "not needed\n" });
  const marketplace = repository("marketplace");
  const marketplaceCommit = commit(marketplace, {
    ".claude-plugin/marketplace.json": {
      name: "fixture-market",
      owner: { name: "Test" },
      plugins: [
        { name: "notes", source: "./plugins/notes" },
        { name: "remote", source: { source: "url", url: pathToFileURL(external).href } },
        { name: "sub", description: "From the entry", version: "0.3.0", source: { source: "git-subdir", url: pathToFileURL(monorepo).href, path: "plugins/sub", ref: "main" } },
        { name: "pinned", source: { source: "git-subdir", url: pathToFileURL(monorepo).href, path: "./plugins/pinned/", sha: firstMonorepoCommit } },
        { name: "wrong-pin", source: { source: "url", url: pathToFileURL(external).href, sha: "0".repeat(40) } },
        { name: "gone", source: { source: "url", url: pathToFileURL(join(scratch, "missing.git")).href } },
        { name: "from-npm", source: { source: "npm", package: "@example/plugin" } },
        { name: "broken", source: "./plugins/broken" },
        { name: "escape", source: "../outside" },
      ],
    },
    "plugins/notes/.claude-plugin/plugin.json": { name: "notes", version: "1.0.0" },
    "plugins/notes/skills/read/SKILL.md": skill("read"),
    "plugins/notes/commands/summarize.md": "Summarize\n",
    "plugins/broken/.claude-plugin/plugin.json": { name: 7 },
  });
  return { marketplace, marketplaceCommit, external, externalCommit, monorepo, monorepoCommit, firstMonorepoCommit };
}

function byName(report) {
  return Object.fromEntries(report.plugins.map(plugin => [plugin.name, plugin]));
}

test("GitHub shorthands become clone URLs and unsupported sources are named", () => {
  assert.equal(cloneUrl("anthropics/claude-plugins-official"), "https://github.com/anthropics/claude-plugins-official.git");
  assert.equal(cloneUrl("git@example.com:team/plugins.git"), "git@example.com:team/plugins.git");
  assert.deepEqual(describeSource({ source: { source: "github", repo: "o/r", ref: "v1" } }, "claude"), { type: "github", url: "https://github.com/o/r.git", ref: "v1" });
  assert.deepEqual(describeSource({ source: { source: "git-subdir", url: "o/r", path: "p", sha: "abc" } }, "claude"), { type: "git-subdir", url: "https://github.com/o/r.git", path: "p", sha: "abc" });
  assert.deepEqual(describeSource({ source: "formatter" }, "claude", "./plugins"), { type: "path", path: "plugins/formatter" });
  assert.deepEqual(describeSource({ source: { source: "pip", package: "x" } }, "claude"), { type: "pip", unsupported: true });
  assert.deepEqual(describeSource({ name: "demo" }, "tick"), { type: "tick-archive" });
});

test("a marketplace cannot pass git an option through a URL, ref, commit or path", async () => {
  const url = "https://example.test/plugin.git";
  assert.doesNotThrow(() => checkGitArguments({ url, ref: "release/1.2", sha: "0123abcd", paths: ["plugins/one"] }));
  assert.throws(() => checkGitArguments({ url, ref: "--upload-pack=touch /tmp/pwned" }), /invalid ref/);
  assert.throws(() => checkGitArguments({ url, sha: "--output=/tmp/x" }), /invalid commit/);
  assert.throws(() => checkGitArguments({ url: "--upload-pack=touch /tmp/pwned" }), /invalid repository URL/);
  assert.throws(() => checkGitArguments({ url, paths: ["--no-cone"] }), /invalid path/);
  const directory = mkdtempSync(join(tmpdir(), "compat-injection-"));
  try {
    await assert.rejects(checkout({ url, ref: "--upload-pack=touch /tmp/pwned" }, directory), /invalid ref/);
    assert.equal(existsSync(join(directory, ".git")), false, "nothing ran before the check");
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("the output of tickloop plugin validate is read line by line", () => {
  assert.deepEqual(parseValidateOutput([
    "Plugin notes 1.0.0, read from .claude-plugin/plugin.json.",
    "Skills: skills/read, skills/write",
    "Commands: summarize",
    "Tick program: bin/tick-notes (speaks the Tick protocol)",
    "Not supported yet, so not used: MCP servers.",
    "Warning: skills path \"../outside\" is ignored: it must start with ./",
    "",
  ].join("\n")), {
    name: "notes",
    displayName: null,
    version: "1.0.0",
    format: ".claude-plugin/plugin.json",
    skills: ["skills/read", "skills/write"],
    commands: ["summarize"],
    tick: { executable: "bin/tick-notes", kind: "speaks the Tick protocol" },
    mcp: [],
    hooks: null,
    unsupported: ["MCP servers"],
    warnings: ["skills path \"../outside\" is ignored: it must start with ./"],
  });
  assert.deepEqual(parseValidateOutput("Plugin bare-plugin, read from its directory.\nSkills: none\n"), {
    name: "bare-plugin", displayName: null, version: null, format: "its directory", skills: [], commands: [], tick: null, mcp: [], hooks: null, unsupported: [], warnings: [],
  });
});

test("a Claude marketplace in a git repository reports every plugin's source, status, components and problems", async () => {
  const fixture = claudeMarketplace();
  const report = await createReport({
    marketplace: pathToFileURL(fixture.marketplace).href,
    tickloop: fakeTickloop(),
    workDirectory: join(scratch, "claude-work"),
    target: "aarch64-apple-darwin",
  });
  const plugins = byName(report);

  assert.equal(report.marketplace.name, "fixture-market");
  assert.equal(report.marketplace.file, ".claude-plugin/marketplace.json");
  assert.equal(report.marketplace.commit, fixture.marketplaceCommit);
  assert.deepEqual(Object.fromEntries(report.plugins.map(plugin => [plugin.name, plugin.status])), {
    notes: STATUS.installable,
    remote: STATUS.installable,
    sub: STATUS.installable,
    pinned: STATUS.installable,
    "wrong-pin": STATUS.notFetchable,
    gone: STATUS.notFetchable,
    "from-npm": STATUS.unsupportedSource,
    broken: STATUS.invalid,
    escape: STATUS.notFetchable,
  });
  assert.deepEqual(
    { revision: plugins.notes.revision, format: plugins.notes.manifestFormat, skills: plugins.notes.skills, unsupported: plugins.notes.unsupportedComponents },
    { revision: fixture.marketplaceCommit.slice(0, 12), format: ".claude-plugin/plugin.json", skills: ["skills/read"], unsupported: [] },
  );
  assert.deepEqual(
    { revision: plugins.remote.revision, version: plugins.remote.version, used: plugins.remote.supportedComponents, unsupported: plugins.remote.unsupportedComponents },
    { revision: fixture.externalCommit.slice(0, 12), version: "2.0.0", used: ["skills"], unsupported: ["MCP servers"] },
  );
  assert.deepEqual(
    { revision: plugins.sub.revision, format: plugins.sub.manifestFormat, version: plugins.sub.version, skills: plugins.sub.skills },
    { revision: fixture.monorepoCommit.slice(0, 12), format: "marketplace entry", version: "0.3.0", skills: ["skills/draft"] },
  );
  assert.deepEqual({ revision: plugins.pinned.revision, skills: plugins.pinned.skills }, { revision: fixture.firstMonorepoCommit.slice(0, 12), skills: ["skills/old"] });
  assert.match(plugins["wrong-pin"].errors[0], /0000000000000000000000000000000000000000/);
  assert.equal(plugins.gone.errors.length, 1);
  assert.deepEqual(plugins["from-npm"].errors, ["npm sources cannot be installed"]);
  assert.deepEqual(plugins.broken.errors, ["name in .claude-plugin/plugin.json must be a string"]);
  assert.deepEqual(plugins.escape.errors, ["../outside leaves the marketplace"]);

  assert.deepEqual(report.totals, {
    plugins: 9,
    installable: 4,
    installableWithSkillsOrTick: 4,
    mcp: { servers: 0, byTransport: {}, oauthConfigured: 0, oauthDiscovery: 0, needsUserConfig: 0, unsupported: 0 },
    invalid: 1,
    notFetchable: 3,
    unsupportedSource: 1,
    withWarnings: 0,
    bySourceType: {
      "git-subdir": { plugins: 2, installable: 2, invalid: 0, "not fetchable": 0, "unsupported source": 0 },
      npm: { plugins: 1, installable: 0, invalid: 0, "not fetchable": 0, "unsupported source": 1 },
      path: { plugins: 3, installable: 1, invalid: 1, "not fetchable": 1, "unsupported source": 0 },
      url: { plugins: 3, installable: 1, invalid: 0, "not fetchable": 2, "unsupported source": 0 },
    },
    manifestFormats: { ".claude-plugin/plugin.json": 2, "marketplace entry": 2 },
    supportedComponents: { skills: 4, commands: 1 },
    unsupportedComponents: { "MCP servers": 1 },
    hooks: { plugins: 0, fully: 0, partly: 0, none: 0, supportedHooks: 0, unsupportedEvents: {}, unsupportedTypes: {}, unsupportedFields: {} },
  });
  assert.equal(git(fixture.monorepo, "status", "--porcelain"), "", "the plugin sources stay untouched");
  assert.ok(!existsSync(join(scratch, "claude-work", "git", "1", "other")), "a git subdirectory is checked out sparsely");

  const markdown = renderMarkdown(report);
  assert.match(markdown, /^# Plugin compatibility: fixture-market$/m);
  assert.match(markdown, /^\| Installable \| 4 \|$/m);
  assert.match(markdown, /^\| commands \| 1 \| yes \|$/m);
  assert.match(markdown, new RegExp(`^\\| sub \\| git-subdir \\S+ plugins/sub #main \\| installable \\| ${fixture.monorepoCommit.slice(0, 12)} \\| marketplace entry \\| 0.3.0 \\| skills \\|`, "m"));
  assert.match(markdown, /^\| from-npm \| npm @example\/plugin \| unsupported source \|/m);
});

test("a Tick marketplace downloads the archive for the target and checks its SHA-256", async () => {
  const packageRoot = join(scratch, "tick-package");
  writeFiles(packageRoot, {
    "tick-package.json": { name: "demo", version: "1.2.0", description: "Demo", executable: "tick-demo", protocol: true },
    "tick-demo": "#!/bin/sh\n",
    "skills/usage/SKILL.md": skill("usage"),
  });
  const archive = join(scratch, "demo.tar.gz");
  execFileSync("tar", ["-czf", archive, "-C", packageRoot, "."]);
  const sha256 = createHash("sha256").update(readFileSync(archive)).digest("hex");
  const download = (checksum, target = "aarch64-apple-darwin") => ({ [target]: { url: pathToFileURL(archive).href, sha256: checksum } });
  const marketplace = join(scratch, "tick-market");
  writeFiles(marketplace, {
    "marketplace.json": {
      name: "ticks",
      owner: { name: "Test" },
      packages: [
        { name: "demo", version: "1.2.0", description: "Demo", downloads: download(sha256) },
        { name: "tampered", version: "1.0.0", description: "Tampered", downloads: download("f".repeat(64)) },
        { name: "elsewhere", version: "1.0.0", description: "Linux only", downloads: download(sha256, "x86_64-unknown-linux-musl") },
      ],
    },
  });

  const report = await createReport({ marketplace, tickloop: fakeTickloop(), workDirectory: join(scratch, "tick-work"), target: "aarch64-apple-darwin" });
  const plugins = byName(report);

  assert.equal(report.marketplace.format, "tick");
  assert.equal(report.marketplace.file, "marketplace.json");
  assert.deepEqual(
    { status: plugins.demo.status, revision: plugins.demo.revision, format: plugins.demo.manifestFormat, used: plugins.demo.supportedComponents },
    { status: STATUS.installable, revision: "1.2.0", format: "tick-package.json", used: ["tick", "skills"] },
  );
  assert.equal(plugins.tampered.status, STATUS.notFetchable);
  assert.match(plugins.tampered.errors[0], new RegExp(`SHA-256 is ${sha256}`));
  assert.deepEqual({ status: plugins.elsewhere.status, errors: plugins.elsewhere.errors }, { status: STATUS.unsupportedSource, errors: ["no download for aarch64-apple-darwin"] });
});

test("the command writes the Markdown and JSON reports under the output directory", () => {
  const marketplace = join(scratch, "local-market");
  writeFiles(marketplace, {
    ".claude-plugin/marketplace.json": { name: "local market", owner: { name: "Test" }, plugins: [{ name: "solo", source: "./solo" }] },
    "solo/skills/one/SKILL.md": skill("one"),
  });
  const out = join(scratch, "out");
  const output = execFileSync(process.execPath, [script, marketplace, "--tickloop", fakeTickloop(), "--out", out], { encoding: "utf8" });

  assert.match(output, /^1 plugins: 1 installable \(1 with Skills or a Tick program\), 0 invalid, 0 not fetchable, 0 with an unsupported source\.$/m);
  assert.equal(JSON.parse(readFileSync(join(out, "local-market.json"), "utf8")).plugins[0].manifestFormat, "marketplace entry");
  assert.match(readFileSync(join(out, "local-market.md"), "utf8"), /^\| solo \| path \.\/solo \| installable \|/m);
  assert.ok(existsSync(join(out, "work", "plugins", "solo", ".claude-plugin", "plugin.json")));
  assert.ok(!existsSync(join(marketplace, "solo", ".claude-plugin")), "the marketplace stays untouched");
});

test("the built tickloop reads the fixture marketplace the same way", { skip: !existsSync(builtTickloop) && "build tickloop with cargo build -p tickloop-code first" }, async () => {
  const fixture = claudeMarketplace();
  const report = await createReport({
    marketplace: pathToFileURL(fixture.marketplace).href,
    tickloop: builtTickloop,
    workDirectory: join(scratch, "built-work"),
    target: "aarch64-apple-darwin",
  });
  const plugins = byName(report);

  assert.deepEqual(
    { installable: report.totals.installable, invalid: report.totals.invalid, notFetchable: report.totals.notFetchable, unsupportedSource: report.totals.unsupportedSource },
    { installable: 4, invalid: 1, notFetchable: 3, unsupportedSource: 1 },
  );
  assert.deepEqual(plugins.notes.unsupportedComponents, []);
  assert.deepEqual(plugins.remote.unsupportedComponents, []);
  assert.deepEqual({ format: plugins.sub.manifestFormat, skills: plugins.sub.skills }, { format: "marketplace entry", skills: ["skills/draft"] });
  assert.match(plugins.broken.errors[0], /name .* must be a string/);
});


test("validation staging rejects escaping and dangling links before writing a fallback manifest", async () => {
  for (const kind of ["external", "dangling"]) {
    const marketplace = join(scratch, `linked-market-${kind}`);
    const outside = join(scratch, `outside-${kind}`);
    mkdirSync(outside);
    writeFiles(marketplace, {
      "marketplace.json": { name: "linked", plugins: [{ name: "linked", source: "./linked" }] },
      "linked/skills/read/SKILL.md": skill("read"),
    });
    symlinkSync(kind === "external" ? outside : join(outside, "missing"), join(marketplace, "linked/.claude-plugin"));
    const report = await createReport({ marketplace, tickloop: fakeTickloop(), workDirectory: join(scratch, `linked-work-${kind}`), target: "aarch64-apple-darwin" });
    assert.deepEqual(readdirSync(outside), [], "staging must not write through the directory link");
    assert.equal(report.plugins[0].status, STATUS.notFetchable);
    assert.match(report.plugins[0].errors[0], /symbolic link/);
  }
});


test("marketplace compatibility uses entry identity and retains the manifest display name", async () => {
  const marketplace = join(scratch, "identity-market");
  const manifest = { name: "Notion", version: "1.0.0" };
  writeFiles(marketplace, {
    "marketplace.json": { name: "identity", plugins: [{ name: "notion", source: "./notion" }] },
    "notion/.claude-plugin/plugin.json": manifest,
    "notion/skills/read/SKILL.md": skill("read"),
  });
  const report = await createReport({ marketplace, tickloop: fakeTickloop(), workDirectory: join(scratch, "identity-work"), target: "aarch64-apple-darwin" });
  assert.equal(report.plugins[0].status, STATUS.installable);
  assert.equal(report.plugins[0].name, "notion");
  assert.equal(report.plugins[0].displayName, "Notion");
  assert.deepEqual(report.plugins[0].skills, ["skills/read"]);
  assert.equal(report.totals.installableWithSkillsOrTick, 1);
  assert.deepEqual(JSON.parse(readFileSync(join(marketplace, "notion/.claude-plugin/plugin.json"))), manifest);
});


test("validation staging rejects a linked plugin root without modifying its target", async () => {
  const marketplace = join(scratch, "linked-root-market");
  const outside = join(scratch, "linked-root-target");
  writeFiles(outside, { "skills/read/SKILL.md": skill("read") });
  writeFiles(marketplace, { "marketplace.json": { name: "linked-root", plugins: [{ name: "linked", source: "./linked" }] } });
  symlinkSync(outside, join(marketplace, "linked"));
  const report = await createReport({ marketplace, tickloop: fakeTickloop(), workDirectory: join(scratch, "linked-root-work"), target: "aarch64-apple-darwin" });
  assert.deepEqual(readdirSync(outside), ["skills"]);
  assert.equal(report.plugins[0].status, STATUS.notFetchable);
  assert.match(report.plugins[0].errors[0], /real directory/);
});

test("MCP validation contributes transports, OAuth discovery and configuration to the report", () => {
  const parsed = parseValidateOutput([
    'MCP server: plugin_demo_local | transport: stdio | command: ["npx","server"] | OAuth: none | userConfig: none | status: ready',
    'MCP server: plugin_demo_remote | transport: http | address: https://example.com/mcp | OAuth: discover on connect | userConfig: required | status: needs configuration',
    'MCP server: plugin_demo_legacy | transport: sse | address: https://example.com/sse | OAuth: none | userConfig: none | status: unsupported',
  ].join("\n"));
  assert.equal(parsed.mcp.length, 3);
  assert.deepEqual(parsed.mcp[0], { name: "plugin_demo_local", transport: "stdio", targetKind: "command", target: '["npx","server"]', oauth: "none", userConfig: "none", status: "ready" });
  const totals = summarize([{ status: STATUS.installable, sourceType: "path", supportedComponents: ["MCP servers"], unsupportedComponents: [], warnings: [], manifestFormat: "directory", mcp: parsed.mcp }]);
  assert.deepEqual(totals.mcp, { servers: 3, byTransport: { http: 1, sse: 1, stdio: 1 }, oauthConfigured: 0, oauthDiscovery: 1, needsUserConfig: 1, unsupported: 1 });
  assert.equal(totals.installableWithSkillsOrTick, 0);
});

test("hook support is read from validate and reported per plugin as fully, partly or not supported", () => {
  const partly = parseValidateOutput([
    "Plugin guard 1.0.0, read from .claude-plugin/plugin.json.",
    "Skills: none",
    "Hooks: 2 supported | events: PreToolUse, Stop | types: command | fields: command, timeout",
    "Hooks not supported yet: events: Notification | types: prompt | fields: shell",
    "Warning: hook events not supported yet, so their hooks are not used: Notification",
  ].join("\n"));
  assert.deepEqual(partly.hooks, {
    support: "partly",
    supported: 2,
    events: ["PreToolUse", "Stop"],
    types: ["command"],
    fields: ["command", "timeout"],
    unsupportedEvents: ["Notification"],
    unsupportedTypes: ["prompt"],
    unsupportedFields: ["shell"],
  });
  const fully = parseValidateOutput("Hooks: 1 supported | events: Stop | types: command | fields: command\n").hooks;
  assert.deepEqual(
    { support: fully.support, unsupportedEvents: fully.unsupportedEvents, unsupportedTypes: fully.unsupportedTypes, unsupportedFields: fully.unsupportedFields },
    { support: "fully", unsupportedEvents: [], unsupportedTypes: [], unsupportedFields: [] },
  );
  const none = parseValidateOutput([
    "Not supported yet, so not used: hooks.",
    "Hooks: 0 supported | events: none | types: none | fields: none",
    "Hooks not supported yet: events: Notification | types: none | fields: none",
  ].join("\n"));
  assert.deepEqual({ support: none.hooks.support, events: none.hooks.events, unsupported: none.unsupported }, { support: "none", events: [], unsupported: ["hooks"] });

  const plugin = (name, hooks, unsupportedComponents = []) => ({
    name, status: STATUS.installable, sourceType: "path", source: `./${name}`, revision: "abc", manifestFormat: ".claude-plugin/plugin.json", version: "1.0.0",
    supportedComponents: hooks?.supported ? ["hooks"] : [], unsupportedComponents, warnings: [], errors: [], mcp: [], hooks,
  });
  const report = {
    generatedAt: "now", tickloop: { version: "tickloop test" }, target: "aarch64-apple-darwin",
    marketplace: { name: "hooked", source: "hooked", format: "claude", file: "marketplace.json" },
    plugins: [plugin("guard", partly.hooks), plugin("done", fully), plugin("notify", none.hooks, ["hooks"]), plugin("plain", null)],
  };
  report.totals = summarize(report.plugins);
  assert.deepEqual(report.totals.hooks, {
    plugins: 3, fully: 1, partly: 1, none: 1, supportedHooks: 3,
    unsupportedEvents: { Notification: 2 }, unsupportedTypes: { prompt: 1 }, unsupportedFields: { shell: 1 },
  });
  assert.deepEqual(report.totals.supportedComponents, { hooks: 2 });

  const markdown = renderMarkdown(report);
  assert.match(markdown, /^## Hooks$/m);
  assert.match(markdown, /^\| Partly supported \| 1 \|$/m);
  assert.match(markdown, /^\| guard \| partly \| 2 \| PreToolUse, Stop \| command \| command, timeout \| Notification \| prompt \| shell \|$/m);
  assert.match(markdown, /^\| Plugin \| Source \| Status \| Revision \| Manifest \| Version \| Used \| Not supported yet \| Hooks \| Warnings and errors \|$/m);
  assert.match(markdown, /^\| guard \| path \.\/guard \| installable \| abc \| \S+ \| 1\.0\.0 \| hooks \|  \| partly: 2 supported \(PreToolUse, Stop\); not supported: events Notification; types prompt; fields shell \|  \|$/m);
  assert.match(markdown, /^\| plain \| path \.\/plain \| installable \| abc \| \S+ \| 1\.0\.0 \|  \|  \|  \|  \|$/m);
});
