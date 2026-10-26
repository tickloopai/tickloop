// Maintainer compatibility report for a plugin marketplace: fetches the marketplace and every
// plugin source it lists, runs `tickloop plugin validate` on each plugin and writes a Markdown and
// a JSON report. It needs the network for remote marketplaces and is not part of CI.
//
//   node scripts/plugin-compat-report.mjs MARKETPLACE [--ref REF] [--tickloop PATH] [--out DIR]
//                                         [--target TRIPLE] [--concurrency N]
//
// MARKETPLACE is a GitHub shorthand (owner/repo), a git URL, a local directory, or an https URL of
// a marketplace file.

import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { cp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs, promisify } from "node:util";

const run = promisify(execFile);

const MARKETPLACE_LOCATIONS = [".tickloop-plugin/marketplace.json", ".claude-plugin/marketplace.json", "marketplace.json"];
const PLUGIN_MANIFESTS = [".tickloop-plugin/plugin.json", ".claude-plugin/plugin.json", "tick-package.json"];
const CLAUDE_MANIFEST = ".claude-plugin/plugin.json";
const MARKETPLACE_ONLY_FIELDS = ["source", "category", "tags", "strict"];
const ENTRY_MANIFEST_FORMAT = "marketplace entry";
const GIT_SOURCE_TYPES = new Set(["github", "url", "git-subdir"]);
const GITHUB_SHORTHAND = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;
const PLUGIN_NAME = /^[a-z0-9][a-z0-9-]{0,63}$/;
const GIT_TIMEOUT_MS = 300_000;
const VALIDATE_TIMEOUT_MS = 60_000;
const DOWNLOAD_TIMEOUT_MS = 120_000;
const MAX_DOWNLOAD_BYTES = 256 * 1024 * 1024;
const MAX_OUTPUT_BYTES = 16 * 1024 * 1024;
const REVISION_LENGTH = 12;
const DEFAULT_CONCURRENCY = 8;
const HOST_TARGETS = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-arm64": "aarch64-unknown-linux-musl",
  "linux-x64": "x86_64-unknown-linux-musl",
};
const GIT_ENVIRONMENT = {
  ...process.env,
  GIT_TERMINAL_PROMPT: "0",
  GIT_ASKPASS: "",
  SSH_ASKPASS: "",
  GCM_INTERACTIVE: "never",
  GIT_SSH_COMMAND: "ssh -o BatchMode=yes",
  // Marketplaces are third-party input: no ext:: or other command-running transports.
  GIT_ALLOW_PROTOCOL: "https:ssh:file",
};
const COMMIT_SHA = /^[0-9a-f]{7,40}$/i;
const GIT_REF = /^(?!-)[A-Za-z0-9._\/-]+$/;

export const STATUS = {
  installable: "installable",
  invalid: "invalid",
  notFetchable: "not fetchable",
  unsupportedSource: "unsupported source",
};

export function cloneUrl(repositoryOrUrl) {
  return GITHUB_SHORTHAND.test(repositoryOrUrl) ? `https://github.com/${repositoryOrUrl}.git` : repositoryOrUrl;
}

/** Where a marketplace entry's plugin comes from, and whether this script can fetch it. */
export function describeSource(entry, format, pluginRoot) {
  if (format === "tick") return { type: "tick-archive" };
  const source = entry.source;
  if (typeof source === "string") {
    const path = source.startsWith("./") || !pluginRoot ? source : join(pluginRoot, source);
    return { type: "path", path };
  }
  const type = source?.source;
  const pins = { ...(source?.ref && { ref: source.ref }), ...(source?.sha && { sha: source.sha }) };
  if (type === "github" && source.repo) return { type, url: cloneUrl(source.repo), ...pins };
  if (type === "url" && source.url) return { type, url: cloneUrl(source.url), ...pins };
  if (type === "git-subdir" && source.url && source.path) return { type, url: cloneUrl(source.url), path: source.path, ...pins };
  return { type: typeof type === "string" ? type : "unknown", unsupported: true };
}

/** What `tickloop plugin validate` printed for a plugin it could read. */
export function parseValidateOutput(stdout) {
  const result = { name: null, version: null, format: null, skills: [], tick: null, unsupported: [], warnings: [] };
  for (const line of stdout.split("\n")) {
    let match;
    if ((match = /^Plugin (\S+)(?: (\S+))?, read from (.+)\.$/.exec(line))) {
      [, result.name, result.version = null, result.format] = match;
    } else if ((match = /^Skills: (.+)$/.exec(line))) {
      result.skills = match[1] === "none" ? [] : match[1].split(", ");
    } else if ((match = /^Tick program: (.+) \((.+)\)$/.exec(line))) {
      result.tick = { executable: match[1], kind: match[2] };
    } else if ((match = /^Not supported yet, so not used: (.+)\.$/.exec(line))) {
      result.unsupported = match[1].split(", ");
    } else if ((match = /^Warning: (.+)$/.exec(line))) {
      result.warnings.push(match[1]);
    }
  }
  return result;
}

function lastLine(text) {
  return text?.trim().split("\n").filter(Boolean).at(-1) ?? "";
}

async function git(args, options = {}) {
  try {
    const { stdout } = await run("git", args, { env: GIT_ENVIRONMENT, timeout: GIT_TIMEOUT_MS, maxBuffer: MAX_OUTPUT_BYTES, ...options });
    return stdout.trim();
  } catch (error) {
    if (error.code === "ENOENT") throw new Error("git is not installed or not on PATH");
    if (error.killed) throw new Error(`git ${args.join(" ")} timed out after ${GIT_TIMEOUT_MS / 1000} s`);
    throw new Error(lastLine(error.stderr) || error.message);
  }
}

/**
 * Refuses a URL, ref, commit or path from a marketplace that git could read as an option, such
 * as a ref of `--upload-pack=<command>`, which would run that command.
 */
export function checkGitArguments({ url, ref, sha, paths = [] }) {
  if (typeof url !== "string" || url.startsWith("-")) throw new Error(`invalid repository URL ${JSON.stringify(url)}`);
  if (sha !== undefined && !COMMIT_SHA.test(sha)) throw new Error(`invalid commit ${JSON.stringify(sha)}`);
  if (ref !== undefined && !GIT_REF.test(ref)) throw new Error(`invalid ref ${JSON.stringify(ref)}`);
  const badPath = paths.find(path => typeof path !== "string" || path.startsWith("-"));
  if (badPath !== undefined) throw new Error(`invalid path ${JSON.stringify(badPath)}`);
}

/** Fetches one commit of `url` into `directory`, only `paths` of it when given. */
export async function checkout({ url, ref, sha, paths }, directory) {
  checkGitArguments({ url, ref, sha, paths });
  await mkdir(directory, { recursive: true });
  const inRepository = args => git(["-C", directory, ...args]);
  await inRepository(["init", "--quiet"]);
  await inRepository(["remote", "add", "--end-of-options", "origin", url]);
  const isSparse = paths?.length > 0 && !paths.includes("");
  if (isSparse) await inRepository(["sparse-checkout", "set", "--cone", "--end-of-options", ...paths]);
  const filter = isSparse ? ["--filter=blob:none"] : [];
  try {
    await inRepository(["fetch", "--quiet", "--depth", "1", "--no-tags", ...filter, "--end-of-options", "origin", sha ?? ref ?? "HEAD"]);
  } catch (error) {
    if (!sha) throw error;
    // Servers that refuse a commit by SHA still send the history of the ref that contains it.
    await inRepository(["fetch", "--quiet", "--no-tags", ...filter, "--end-of-options", "origin", ref ?? "HEAD"]);
  }
  try {
    await inRepository(["checkout", "--quiet", "--end-of-options", sha ?? "FETCH_HEAD"]);
  } catch (error) {
    throw sha ? new Error(`commit ${sha} is not in the repository`) : error;
  }
  const commit = await inRepository(["rev-parse", "HEAD"]);
  if (sha && !commit.startsWith(sha.toLowerCase())) throw new Error(`checked out ${commit}, not the pinned commit ${sha}`);
  return commit;
}

function isInside(root, path) {
  const fromRoot = relative(root, path);
  return fromRoot === "" || (!fromRoot.startsWith("..") && !isAbsolute(fromRoot));
}

async function isDirectory(path) {
  return (await stat(path).catch(() => null))?.isDirectory() ?? false;
}

async function obtainMarketplace(marketplace, ref, workDirectory) {
  if (await isDirectory(marketplace)) {
    const root = resolve(marketplace);
    const commit = await git(["-C", root, "rev-parse", "HEAD"]).catch(() => null);
    return { root, commit, ...(await readMarketplaceFile(root)) };
  }
  if (/^https:\/\/.+\.json(\?.*)?$/.test(marketplace)) {
    const document = JSON.parse((await download(marketplace)).toString("utf8"));
    return { root: null, commit: null, file: marketplace, document };
  }
  const root = join(workDirectory, "marketplace");
  const commit = await checkout({ url: cloneUrl(marketplace), ref }, root);
  return { root, commit, ...(await readMarketplaceFile(root)) };
}

async function readMarketplaceFile(root) {
  for (const file of MARKETPLACE_LOCATIONS) {
    const path = join(root, file);
    if (existsSync(path)) return { file, document: JSON.parse(await readFile(path, "utf8")) };
  }
  throw new Error(`no marketplace file in ${root}; looked for ${MARKETPLACE_LOCATIONS.join(", ")}`);
}

async function download(url) {
  if (url.startsWith("file:")) return readFile(fileURLToPath(url));
  if (!/^https:\/\//.test(url)) throw new Error(`downloads must use https: ${url}`);
  const response = await fetch(url, { signal: AbortSignal.timeout(DOWNLOAD_TIMEOUT_MS) });
  if (!response.ok) throw new Error(`${url} answered ${response.status}`);
  if (Number(response.headers.get("content-length")) > MAX_DOWNLOAD_BYTES) throw new Error(`${url} is larger than 256 MiB`);
  const body = Buffer.from(await response.arrayBuffer());
  if (body.length > MAX_DOWNLOAD_BYTES) throw new Error(`${url} is larger than 256 MiB`);
  return body;
}

async function mapWithLimit(items, limit, work) {
  const results = new Array(items.length);
  let next = 0;
  const worker = async () => {
    while (next < items.length) {
      const index = next++;
      results[index] = await work(items[index], index);
    }
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  return results;
}

function fetchKey({ url, ref, sha }) {
  return JSON.stringify([url, ref ?? null, sha ?? null]);
}

/** Checks out every git source once per repository and commit, sparsely when only subdirectories are needed. */
async function checkoutGitSources(plugins, workDirectory, concurrency) {
  const units = new Map();
  for (const plugin of plugins) {
    if (!GIT_SOURCE_TYPES.has(plugin.source.type) || plugin.source.unsupported) continue;
    const path = plugin.source.type === "git-subdir" ? normalizeSubdirectory(plugin.source.path) : "";
    if (path === null) continue;
    const key = fetchKey(plugin.source);
    if (!units.has(key)) units.set(key, { ...plugin.source, paths: new Set() });
    units.get(key).paths.add(path);
  }
  const checkouts = new Map();
  await mapWithLimit([...units.entries()], concurrency, async ([key, unit], index) => {
    const directory = join(workDirectory, "git", String(index));
    try {
      const commit = await checkout({ ...unit, paths: [...unit.paths] }, directory);
      checkouts.set(key, { directory, commit });
    } catch (error) {
      checkouts.set(key, { error: error.message });
    }
  });
  return checkouts;
}

function normalizeSubdirectory(path) {
  const trimmed = path.replace(/^\.\//, "").replace(/\/+$/, "");
  return trimmed.split("/").includes("..") || isAbsolute(path) ? null : trimmed;
}

/** The plugin's directory as fetched, with the commit it came from. */
async function fetchPlugin(plugin, context) {
  const { source } = plugin;
  if (source.type === "path") {
    if (!context.marketplace.root) throw new Error("a relative path cannot be fetched from a marketplace file served over https");
    const directory = resolve(context.marketplace.root, source.path);
    if (!isInside(context.marketplace.root, directory)) throw new Error(`${source.path} leaves the marketplace`);
    if (!(await isDirectory(directory))) throw new Error(`${source.path} is not a directory in the marketplace`);
    return { directory, revision: context.marketplace.commit?.slice(0, REVISION_LENGTH) ?? "local" };
  }
  const subdirectory = source.type === "git-subdir" ? normalizeSubdirectory(source.path) : "";
  if (subdirectory === null) throw new Error(`${source.path} leaves the repository`);
  const fetched = context.checkouts.get(fetchKey(source));
  if (fetched.error) throw new Error(fetched.error);
  const directory = join(fetched.directory, subdirectory);
  if (!(await isDirectory(directory))) throw new Error(`${source.path} is not a directory in the repository`);
  return { directory, revision: fetched.commit.slice(0, REVISION_LENGTH) };
}

async function fetchTickArchive(plugin, context, destination) {
  const offered = plugin.entry.downloads?.[context.target];
  const archive = await download(offered.url);
  const sha256 = createHash("sha256").update(archive).digest("hex");
  if (sha256 !== offered.sha256.toLowerCase()) throw new Error(`the archive's SHA-256 is ${sha256}, not ${offered.sha256}`);
  const archivePath = join(context.workDirectory, "archives", `${plugin.directoryName}.tar.gz`);
  await mkdir(join(context.workDirectory, "archives"), { recursive: true });
  await writeFile(archivePath, archive);
  await mkdir(destination, { recursive: true });
  await run("tar", ["-xzf", archivePath, "-C", destination]);
  return { revision: plugin.entry.version };
}

/** A copy of the plugin to validate, with its marketplace entry as its manifest when it has none, as installing writes it. */
async function stageForValidation(plugin, fetched, destination) {
  await cp(fetched.directory, destination, {
    recursive: true,
    verbatimSymlinks: true,
    filter: path => !path.endsWith("/.git"),
  });
  const hasManifest = PLUGIN_MANIFESTS.some(file => existsSync(join(destination, file)));
  if (hasManifest) return false;
  const manifest = Object.fromEntries(Object.entries(plugin.entry).filter(([field]) => !MARKETPLACE_ONLY_FIELDS.includes(field)));
  await mkdir(join(destination, ".claude-plugin"), { recursive: true });
  await writeFile(join(destination, CLAUDE_MANIFEST), `${JSON.stringify(manifest, null, 2)}\n`);
  return true;
}

async function validate(tickloop, directory) {
  try {
    const { stdout } = await run(tickloop, ["plugin", "validate", directory], { timeout: VALIDATE_TIMEOUT_MS, maxBuffer: MAX_OUTPUT_BYTES });
    return { ok: true, ...parseValidateOutput(stdout) };
  } catch (error) {
    if (error.code === "ENOENT") throw new Error(`cannot run ${tickloop}; build it with cargo build -p tickloop-code`);
    const message = lastLine(error.stderr).replace(/^Error: /, "").replace(`${directory} is not a valid plugin: `, "");
    return { ok: false, error: message || error.message };
  }
}

async function examinePlugin(plugin, context) {
  const result = {
    name: plugin.name,
    sourceType: plugin.source.type,
    source: plugin.entry.source ?? null,
    status: null,
    revision: null,
    manifestFormat: null,
    version: null,
    skills: [],
    tick: null,
    supportedComponents: [],
    unsupportedComponents: [],
    warnings: [],
    errors: [],
  };
  const isTickArchive = plugin.source.type === "tick-archive";
  if (plugin.source.unsupported || (isTickArchive && !plugin.entry.downloads?.[context.target])) {
    result.status = STATUS.unsupportedSource;
    result.errors.push(isTickArchive ? `no download for ${context.target}` : `${plugin.source.type} sources cannot be installed`);
    return result;
  }
  const destination = join(context.workDirectory, "plugins", plugin.directoryName);
  let wroteEntryManifest = false;
  try {
    if (isTickArchive) {
      result.revision = (await fetchTickArchive(plugin, context, destination)).revision;
    } else {
      const fetched = await fetchPlugin(plugin, context);
      result.revision = fetched.revision;
      wroteEntryManifest = await stageForValidation(plugin, fetched, destination);
    }
  } catch (error) {
    result.status = STATUS.notFetchable;
    result.errors.push(error.message);
    return result;
  }
  const validated = await validate(context.tickloop, destination);
  if (!validated.ok) {
    result.status = STATUS.invalid;
    result.manifestFormat = wroteEntryManifest ? ENTRY_MANIFEST_FORMAT : null;
    result.errors.push(validated.error);
    return result;
  }
  result.status = STATUS.installable;
  result.manifestFormat = wroteEntryManifest ? ENTRY_MANIFEST_FORMAT : validated.format;
  result.version = validated.version;
  result.skills = validated.skills;
  result.tick = validated.tick;
  result.supportedComponents = [...(validated.tick ? ["tick"] : []), ...(validated.skills.length ? ["skills"] : [])];
  result.unsupportedComponents = validated.unsupported;
  result.warnings = validated.warnings;
  if (validated.name !== plugin.name) result.warnings.push(`the manifest names it ${validated.name}, the marketplace ${plugin.name}`);
  return result;
}

function countBy(items, key) {
  const counts = {};
  for (const item of items) for (const value of [key(item)].flat()) if (value !== null) counts[value] = (counts[value] ?? 0) + 1;
  return Object.fromEntries(Object.entries(counts).sort(([left], [right]) => left.localeCompare(right)));
}

export function summarize(plugins) {
  const installable = plugins.filter(plugin => plugin.status === STATUS.installable);
  const bySourceType = {};
  for (const plugin of plugins) {
    const row = bySourceType[plugin.sourceType] ??= { plugins: 0, ...Object.fromEntries(Object.values(STATUS).map(status => [status, 0])) };
    row.plugins += 1;
    row[plugin.status] += 1;
  }
  return {
    plugins: plugins.length,
    installable: installable.length,
    installableWithSkillsOrTick: installable.filter(plugin => plugin.supportedComponents.length > 0).length,
    invalid: plugins.filter(plugin => plugin.status === STATUS.invalid).length,
    notFetchable: plugins.filter(plugin => plugin.status === STATUS.notFetchable).length,
    unsupportedSource: plugins.filter(plugin => plugin.status === STATUS.unsupportedSource).length,
    withWarnings: installable.filter(plugin => plugin.warnings.length > 0).length,
    bySourceType: Object.fromEntries(Object.entries(bySourceType).sort(([left], [right]) => left.localeCompare(right))),
    manifestFormats: countBy(installable, plugin => plugin.manifestFormat),
    supportedComponents: countBy(installable, plugin => plugin.supportedComponents),
    unsupportedComponents: countBy(installable, plugin => plugin.unsupportedComponents),
  };
}

/** Fetches `marketplace` and every plugin it lists, and validates each one with `tickloop`. */
export async function createReport({ marketplace, ref, tickloop, workDirectory, target, concurrency = DEFAULT_CONCURRENCY }) {
  await rm(workDirectory, { recursive: true, force: true });
  await mkdir(workDirectory, { recursive: true });
  const obtained = await obtainMarketplace(marketplace, ref, workDirectory);
  const { document } = obtained;
  const format = Array.isArray(document.plugins) ? "claude" : Array.isArray(document.packages) ? "tick" : null;
  if (!format) throw new Error(`${obtained.file} lists neither plugins nor packages`);
  const usedDirectoryNames = new Set();
  const plugins = (format === "claude" ? document.plugins : document.packages).map((entry, index) => {
    const name = typeof entry.name === "string" ? entry.name : `entry-${index}`;
    const directoryName = PLUGIN_NAME.test(name) && !usedDirectoryNames.has(name) ? name : `entry-${index}`;
    usedDirectoryNames.add(directoryName);
    return { name, entry, directoryName, source: describeSource(entry, format, document.metadata?.pluginRoot) };
  });
  const checkouts = await checkoutGitSources(plugins, workDirectory, concurrency);
  const context = { marketplace: obtained, checkouts, tickloop, workDirectory, target };
  const results = await mapWithLimit(plugins, concurrency, plugin => examinePlugin(plugin, context));
  const { stdout: tickloopVersion } = await run(tickloop, ["--version"]).catch(() => ({ stdout: "unknown" }));
  return {
    generatedAt: new Date().toISOString(),
    tickloop: { path: tickloop, version: tickloopVersion.trim() },
    target,
    marketplace: { name: document.name ?? null, source: marketplace, ref: ref ?? null, file: obtained.file, format, commit: obtained.commit },
    totals: summarize(results),
    plugins: results,
  };
}

function cell(text) {
  return String(text ?? "").replace(/\|/g, "\\|").replace(/\s*\n\s*/g, " ");
}

function table(header, rows) {
  return [`| ${header.join(" | ")} |`, `| ${header.map(() => "---").join(" | ")} |`, ...rows.map(row => `| ${row.map(cell).join(" | ")} |`)].join("\n");
}

function describeSourceCell(plugin) {
  const source = plugin.source;
  if (typeof source === "string") return `path ${source}`;
  const where = source?.repo ?? source?.url ?? source?.package ?? "";
  const path = source?.path ? ` ${source.path}` : "";
  const pin = source?.sha ? ` @${source.sha.slice(0, REVISION_LENGTH)}` : source?.ref ? ` #${source.ref}` : "";
  return `${plugin.sourceType} ${where}${path}${pin}`.trim();
}

export function renderMarkdown(report) {
  const { marketplace, totals } = report;
  const statuses = Object.values(STATUS);
  const lines = [
    `# Plugin compatibility: ${marketplace.name ?? marketplace.source}`,
    "",
    `Generated ${report.generatedAt} with ${report.tickloop.version} (\`tickloop plugin validate\`) for ${report.target}.`,
    `Marketplace \`${marketplace.source}\`${marketplace.ref ? ` at ${marketplace.ref}` : ""}, ${marketplace.format} format, read from \`${marketplace.file}\`${marketplace.commit ? ` at commit ${marketplace.commit}` : ""}.`,
    "",
    "## Totals",
    "",
    table(["", "Plugins"], [
      ["Listed", totals.plugins],
      ["Installable", totals.installable],
      ["Installable with Skills or a Tick program", totals.installableWithSkillsOrTick],
      ["Installable with warnings", totals.withWarnings],
      ["Invalid", totals.invalid],
      ["Not fetchable", totals.notFetchable],
      ["Unsupported source", totals.unsupportedSource],
    ]),
    "",
    "### By source type",
    "",
    table(["Source type", "Plugins", ...statuses.map(status => status[0].toUpperCase() + status.slice(1))],
      Object.entries(totals.bySourceType).map(([type, row]) => [type, row.plugins, ...statuses.map(status => row[status])])),
    "",
    "### Components of installable plugins",
    "",
    table(["Component", "Plugins", "Used by Tickloop"], [
      ...Object.entries(totals.supportedComponents).map(([component, count]) => [component, count, "yes"]),
      ...Object.entries(totals.unsupportedComponents).map(([component, count]) => [component, count, "not yet"]),
    ]),
    "",
    "### Manifests of installable plugins",
    "",
    table(["Read from", "Plugins"], Object.entries(totals.manifestFormats)),
    "",
    "## Plugins",
    "",
    table(["Plugin", "Source", "Status", "Revision", "Manifest", "Version", "Used", "Not supported yet", "Warnings and errors"],
      report.plugins.map(plugin => [
        plugin.name,
        describeSourceCell(plugin),
        plugin.status,
        plugin.revision,
        plugin.manifestFormat,
        plugin.version,
        plugin.supportedComponents.join(", "),
        plugin.unsupportedComponents.join(", "),
        [...plugin.errors, ...plugin.warnings].join("; "),
      ])),
    "",
  ];
  return lines.join("\n");
}

export async function writeReport(report, outDirectory) {
  const baseName = (report.marketplace.name ?? "marketplace").replace(/[^A-Za-z0-9._-]+/g, "-");
  await mkdir(outDirectory, { recursive: true });
  const markdownPath = join(outDirectory, `${baseName}.md`);
  const jsonPath = join(outDirectory, `${baseName}.json`);
  await writeFile(markdownPath, renderMarkdown(report));
  await writeFile(jsonPath, `${JSON.stringify(report, null, 2)}\n`);
  return { markdownPath, jsonPath };
}

const USAGE = "Usage: node scripts/plugin-compat-report.mjs MARKETPLACE [--ref REF] [--tickloop PATH] [--out DIR] [--target TRIPLE] [--concurrency N]";

async function main() {
  const repositoryRoot = fileURLToPath(new URL("../", import.meta.url));
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      ref: { type: "string" },
      tickloop: { type: "string", default: join(repositoryRoot, "target/debug/tickloop") },
      out: { type: "string", default: join(repositoryRoot, "target/plugin-compat") },
      target: { type: "string", default: HOST_TARGETS[`${process.platform}-${process.arch}`] },
      concurrency: { type: "string", default: String(DEFAULT_CONCURRENCY) },
      help: { type: "boolean", short: "h" },
    },
  });
  if (values.help || positionals.length !== 1) {
    console.log(USAGE);
    if (!values.help) process.exitCode = 2;
    return;
  }
  const outDirectory = resolve(values.out);
  const report = await createReport({
    marketplace: positionals[0],
    ref: values.ref,
    tickloop: resolve(values.tickloop),
    workDirectory: join(outDirectory, "work"),
    target: values.target,
    concurrency: Math.max(1, Number.parseInt(values.concurrency, 10) || DEFAULT_CONCURRENCY),
  });
  const { markdownPath, jsonPath } = await writeReport(report, outDirectory);
  const { totals } = report;
  console.log(`${totals.plugins} plugins: ${totals.installable} installable (${totals.installableWithSkillsOrTick} with Skills or a Tick program), ${totals.invalid} invalid, ${totals.notFetchable} not fetchable, ${totals.unsupportedSource} with an unsupported source.`);
  console.log(`Wrote ${markdownPath} and ${jsonPath}.`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch(error => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
