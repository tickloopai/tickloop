import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, readdirSync, existsSync, rmSync, realpathSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

const directory = realpathSync(mkdtempSync('/tmp/tlw-'));
const config = join(directory, 'config');
const temporary = join(directory, 'tmp');
const home = join(directory, 'home');
for (const path of [config, temporary, home]) mkdirSync(path, { mode: 0o700 });
const registry = join(temporary, 'owned-runtimes.jsonl');
writeFileSync(registry, '', { mode: 0o600 });
const environment = { ...process.env, HOME: home, TMPDIR: temporary,
  CARGO_HOME: process.env.CARGO_HOME || join(homedir(), '.cargo'),
  RUSTUP_HOME: process.env.RUSTUP_HOME || join(homedir(), '.rustup'),
  TICKLOOP_CONFIG_DIR: config, TICKLOOP_TMPDIR: temporary,
  NO_PROXY: '127.0.0.1,localhost', no_proxy: '127.0.0.1,localhost' };
for (const key of Object.keys(environment))
  if (key.startsWith('HERDR_') || ['TICKLOOP_TICK_RUNTIME_DIR', 'TICKLOOP_DEBUG'].includes(key)) delete environment[key];

// A private inherited root or an explicitly registered fixture root proves ownership;
// executable names and build directories never grant permission to signal a process.
function registeredDirectories() {
  return [...new Set(readFileSync(registry, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse))];
}
function processes() {
  const roots = [temporary, config, ...registeredDirectories()];
  return execFileSync('ps', ['axeww', '-o', 'pid=,lstart=,command='], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 })
    .split('\n').filter(line => roots.some(root =>
      [`TICKLOOP_TMPDIR=${root}`, `TICKLOOP_CONFIG_DIR=${root}`].some(marker =>
        line.includes(` ${marker} `) || line.endsWith(` ${marker}`))))
    .map(line => {
      const match = line.trim().match(/^(\d+)\s+((?:\S+\s+){4}\S+)/);
      return match && { pid: Number(match[1]), identity: match[0] };
    }).filter(Boolean).filter(process => process.pid !== globalThis.process.pid);
}
const before = new Set(processes().map(process => process.identity));
try {
  const child = spawn(process.argv[2] || 'cargo', process.argv.length > 2 ? process.argv.slice(3)
    : ['nextest', 'run', '--locked', '--workspace', '--no-fail-fast'], { env: environment, stdio: 'inherit' });
  for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => child.kill(signal));
  let code = await new Promise(resolve => {
    child.on('error', error => { console.error(error.message); resolve(1); });
    child.on('exit', code => resolve(code ?? 1));
  });
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  let remaining = [];
  for (let attempt = 0; attempt < 50; attempt++) {
    remaining = processes().filter(process => !before.has(process.identity));
    if (!remaining.length) break;
    await sleep(100);
  }
  const runtimes = registeredDirectories();
  function runtimeDirectoriesIn(root) {
    const entries = readdirSync(root, { withFileTypes: true });
    if (entries.some(entry => ['host.sock', 'host.lock', 'daemon.sock', 'daemon.lock'].includes(entry.name))) return [root];
    return entries.filter(entry => entry.isDirectory()).flatMap(entry => runtimeDirectoriesIn(join(root, entry.name)));
  }
  const leftoverDirectories = [...new Set([...runtimes.filter(path => existsSync(path)), ...runtimeDirectoriesIn(temporary)])];
  console.error(`Test cleanup: ${remaining.length} owned processes, ${leftoverDirectories.length} owned runtime directories left by tests (report-only).`);
  if (remaining.length || leftoverDirectories.length) {
    for (const { pid, identity } of remaining) {
      if (processes().some(process => process.identity === identity)) {
        console.error(`Stopping leaked test PID ${pid}`);
        try { process.kill(pid, 'SIGTERM'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      }
    }
    await sleep(1000);
    for (const { pid, identity } of remaining) {
      if (processes().some(process => process.identity === identity)) {
        try { process.kill(pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      }
    }
    for (const path of leftoverDirectories) {
      console.error(`Removing leaked owned runtime ${path}`);
      rmSync(path, { recursive: true, force: true });
    }
  }
  process.exitCode = code;
} finally {
  rmSync(directory, { recursive: true, force: true });
}
