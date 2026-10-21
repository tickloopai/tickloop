import test from 'node:test';
import assert from 'node:assert/strict';
import { chmodSync, copyFileSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';

const launcher = resolve('scripts/dev-agent.sh');

function makeWritable(directory) {
  if (!lstatSync(directory).isDirectory()) return;
  chmodSync(directory, 0o700);
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (entry.isDirectory()) makeWritable(join(directory, entry.name));
  }
}

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'tickloop-dev-launch-test-'));
  t.after(() => {
    makeWritable(root);
    rmSync(root, { recursive: true, force: true });
  });
  const commands = join(root, 'commands');
  const caller = join(root, 'caller directory');
  mkdirSync(commands);
  mkdirSync(caller);
  const executable = (name, source) => writeFileSync(join(commands, name), `#!${process.execPath}\n${source}\n`, { mode: 0o755 });
  executable('git', `
    const fs = require('node:fs'); const path = require('node:path');
    const args = process.argv.slice(2);
    if (args[0] !== '-C') process.exit(90);
    const root = fs.realpathSync(args[1]);
    fs.appendFileSync(path.join(root, 'git-calls'), JSON.stringify(args.slice(2)) + '\\n');
    const command = args.slice(2).join(' ');
    if (command === 'rev-parse --show-toplevel') process.stdout.write(root + '\\n');
    else if (command === 'symbolic-ref --quiet HEAD' && fs.existsSync(path.join(root, 'branch')))
      process.stdout.write('refs/heads/' + fs.readFileSync(path.join(root, 'branch'), 'utf8') + '\\n');
    else if (command === 'rev-parse --verify HEAD' && fs.existsSync(path.join(root, 'revision')))
      process.stdout.write(fs.readFileSync(path.join(root, 'revision'), 'utf8') + '\\n');
    else process.exit(1);
  `);
  const tickloop = `#!${process.execPath}
    const fs = require('node:fs'); const path = require('node:path');
    const args = process.argv.slice(2);
    if (args[0] === 'tick' && args[1] === 'list') { console.log('fixture-package'); process.exit(0); }
    if (args[0] !== '--development-root' || !args[1]) process.exit(91);
    const developmentRoot = args.splice(0, 2)[1];
    const bin = path.dirname(fs.realpathSync(process.argv[1]));
    fs.writeFileSync(path.join(bin, 'installation.json'), JSON.stringify({ developmentRoot }));
    fs.chmodSync(bin, 0o500); fs.chmodSync(path.dirname(bin), 0o500);
    console.log(JSON.stringify({ cwd: fs.realpathSync('.'), executable: fs.realpathSync(process.argv[1]), args,
      developmentRoot, compiledFor: __COMPILED_CHECKOUT__, buildTarget: __BUILD_TARGET__,
      daemon: process.env.TICKLOOP_DAEMON_RUNTIME_DIR, tickhost: process.env.TICKLOOP_TICKHOST_RUNTIME_DIR,
      config: process.env.TICKLOOP_CONFIG_DIR, home: process.env.HOME }));
  `;
  executable('cargo', `
    const fs = require('node:fs'); const path = require('node:path');
    const args = process.argv.slice(2);
    fs.appendFileSync(path.join(process.cwd(), 'cargo-calls'), JSON.stringify(args) + '\\n');
    const targetOption = args.indexOf('--target-dir');
    const target = targetOption < 0 ? process.env.CARGO_TARGET_DIR || path.join(process.cwd(), 'target')
      : args[targetOption + 1];
    const directory = path.join(target, 'debug'); fs.mkdirSync(directory, { recursive: true });
    const program = ${JSON.stringify(tickloop)}
      .replace('__COMPILED_CHECKOUT__', JSON.stringify(fs.realpathSync('.')))
      .replace('__BUILD_TARGET__', JSON.stringify(fs.realpathSync(target)));
    for (const name of ['tickloop', 'tickhost'])
      fs.writeFileSync(path.join(directory, name), program, { mode: 0o755 });
    const switchedBranch = path.join(process.cwd(), 'switch-branch-during-build');
    if (fs.existsSync(switchedBranch)) fs.copyFileSync(switchedBranch, path.join(process.cwd(), 'branch'));
  `);

  function checkout(name, { branch = 'main', revision, archive = false } = {}) {
    const directory = join(root, name);
    mkdirSync(join(directory, 'scripts'), { recursive: true });
    copyFileSync(launcher, join(directory, 'scripts/dev-agent.sh'));
    if (!archive) writeFileSync(join(directory, '.git'), 'fixture-only git metadata\n');
    if (branch !== null) writeFileSync(join(directory, 'branch'), branch);
    if (revision) writeFileSync(join(directory, 'revision'), revision);
    return directory;
  }

  function run(directory, overrides = {}, args = []) {
    const env = { ...process.env, PATH: `${commands}:/usr/bin:/bin`,
      TICKLOOP_CONFIG_DIR: join(root, 'global configuration') };
    delete env.TICKLOOP_DAEMON_RUNTIME_DIR;
    delete env.TICKLOOP_TICKHOST_RUNTIME_DIR;
    delete env.CARGO_TARGET_DIR;
    Object.assign(env, overrides);
    const result = spawnSync('/bin/sh', [join(directory, 'scripts/dev-agent.sh'), ...args], {
      cwd: caller, env, encoding: 'utf8', timeout: 15_000,
    });
    return { ...result, reported: result.status === 0 ? JSON.parse(result.stdout.trim()) : null };
  }
  return { root, caller, checkout, run };
}

test('actual launcher reuses branch services but publishes a fresh immutable program bundle', t => {
  const f = fixture(t);
  const checkout = f.checkout('worktree with spaces', { branch: 'feature/升级' });
  const first = f.run(checkout, {}, ['-c', '--cwd', f.caller]);
  const second = f.run(checkout);
  assert.equal(first.status, 0, first.stderr);
  assert.equal(second.status, 0, second.stderr);
  assert.equal(first.reported.daemon, second.reported.daemon);
  assert.equal(first.reported.tickhost, second.reported.tickhost);
  assert.notEqual(first.reported.executable, second.reported.executable);
  assert.match(first.reported.daemon, new RegExp(`^/tmp/tickloop-dev-${process.getuid()}-[a-f0-9]{16}$`));
  assert.match(first.reported.tickhost, /^\/tmp\/tlh-[a-f0-9]{16}$/);
  assert.equal(first.reported.tickhost, `/tmp/tlh-${first.reported.daemon.slice(-16)}`);
  const expectedNamespace = createHash('sha256')
    .update([String(process.getuid()), realpathSync(checkout), 'branch:refs/heads/feature/升级', ''].join('\0'))
    .digest('hex').slice(0, 16);
  assert.equal(first.reported.tickhost, `/tmp/tlh-${expectedNamespace}`);
  assert.equal(first.reported.cwd, realpathSync(f.caller));
  assert.equal(first.reported.developmentRoot, realpathSync(checkout));
  assert.equal(first.reported.compiledFor, realpathSync(checkout));
  assert.equal(first.reported.buildTarget, join(realpathSync(checkout), 'target'));
  assert.deepEqual(JSON.parse(readFileSync(join(dirname(first.reported.executable), 'installation.json'), 'utf8')),
    { developmentRoot: realpathSync(checkout) });
  assert.deepEqual(first.reported.args, ['-c', '--cwd', f.caller]);
  assert.equal(first.reported.config, join(f.root, 'global configuration'));
  assert.equal(first.reported.home, process.env.HOME);
  assert.equal(lstatSync(first.reported.executable).mode & 0o777, 0o500);
  assert.equal(lstatSync(dirname(first.reported.executable)).mode & 0o777, 0o500);
  assert.ok(existsSync(first.reported.executable));
  const workerSocket = 'w-00000000-0000-0000-0000-000000000000.upgrade';
  const maxSessionDirectoryBytes = 55;
  const longestTickSession = join('s', 'ffffffffffffffff');
  for (const prefix of ['', '/private']) {
    assert.ok(Buffer.byteLength(join(prefix, first.reported.daemon, workerSocket)) < 104);
    assert.ok(Buffer.byteLength(join(prefix, first.reported.tickhost, 'host.sock')) < 104);
    assert.ok(Buffer.byteLength(join(prefix, first.reported.tickhost, longestTickSession)) <= maxSessionDirectoryBytes);
  }
});

test('different branches and worktrees cannot select each other by default', t => {
  const f = fixture(t);
  const first = f.checkout('first', { branch: 'feature/a' });
  const second = f.checkout('second', { branch: 'feature/a' });
  const a = f.run(first).reported;
  const otherWorktree = f.run(second).reported;
  writeFileSync(join(first, 'branch'), 'feature/b');
  const b = f.run(first).reported;
  writeFileSync(join(first, 'branch'), 'feature/a');
  const returned = f.run(first).reported;
  assert.notEqual(a.daemon, otherWorktree.daemon);
  assert.notEqual(a.tickhost, otherWorktree.tickhost);
  assert.notEqual(a.daemon, b.daemon);
  assert.notEqual(a.tickhost, b.tickhost);
  assert.equal(a.daemon, returned.daemon);
  assert.equal(a.tickhost, returned.tickhost);
});

test('canonical checkout paths and detached commits produce stable identities', t => {
  const f = fixture(t);
  const checkout = f.checkout('detached', { branch: null, revision: 'a'.repeat(40) });
  const alias = join(f.root, 'checkout alias');
  symlinkSync(checkout, alias);
  const first = f.run(checkout).reported;
  assert.equal(first.daemon, f.run(alias).reported.daemon);
  assert.equal(first.tickhost, f.run(checkout).reported.tickhost);
  writeFileSync(join(checkout, 'revision'), 'b'.repeat(40));
  assert.notEqual(first.daemon, f.run(checkout).reported.daemon);
});

test('source archives remain isolated without consulting an ancestor Git repository', t => {
  const f = fixture(t);
  const first = f.checkout('archive one', { archive: true });
  const second = f.checkout('archive two', { archive: true });
  const a = f.run(first).reported;
  assert.equal(a.daemon, f.run(first).reported.daemon);
  assert.notEqual(a.daemon, f.run(second).reported.daemon);
  assert.ok(!existsSync(join(first, 'git-calls')));
});

test('explicit existing service overrides are honored independently without changing global config', t => {
  const f = fixture(t);
  const checkout = f.checkout('overrides');
  const baseline = f.run(checkout).reported;
  const daemon = '/tmp/chosen-daemon';
  const tickhost = '/tmp/chosen-tickhost';
  const daemonOnly = f.run(checkout, { TICKLOOP_DAEMON_RUNTIME_DIR: daemon }).reported;
  assert.equal(daemonOnly.daemon, daemon);
  assert.equal(daemonOnly.tickhost, join(daemon, 'tickhost'));
  const hostOnly = f.run(checkout, { TICKLOOP_TICKHOST_RUNTIME_DIR: tickhost }).reported;
  assert.equal(hostOnly.daemon, baseline.daemon);
  assert.equal(hostOnly.tickhost, tickhost);
  const both = f.run(checkout, { TICKLOOP_DAEMON_RUNTIME_DIR: daemon,
    TICKLOOP_TICKHOST_RUNTIME_DIR: tickhost, CARGO_TARGET_DIR: join(f.root, 'custom target') }).reported;
  assert.equal(both.daemon, daemon);
  assert.equal(both.tickhost, tickhost);
  assert.equal(both.config, baseline.config);
  assert.equal(both.home, baseline.home);
});

test('unreadable checkout identity fails before building or selecting services', t => {
  const f = fixture(t);
  const checkout = f.checkout('broken metadata', { branch: null });
  const result = f.run(checkout);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /Cannot identify the development checkout/);
  assert.ok(!existsSync(join(checkout, 'cargo-calls')));
});

test('shared Cargo overrides isolate build artifacts by worktree and branch', t => {
  const f = fixture(t);
  const first = f.checkout('shared target first');
  const second = f.checkout('shared target second');
  const shared = join(f.root, 'shared build target');
  const overrides = { CARGO_TARGET_DIR: shared };
  const a = f.run(first, overrides);
  const b = f.run(second, overrides);
  assert.equal(a.status, 0, a.stderr);
  assert.equal(b.status, 0, b.stderr);
  assert.equal(a.reported.compiledFor, realpathSync(first));
  assert.equal(b.reported.compiledFor, realpathSync(second));
  assert.notEqual(a.reported.buildTarget, b.reported.buildTarget);
  for (const result of [a, b]) {
    assert.equal(dirname(result.reported.buildTarget), realpathSync(shared));
    assert.ok(existsSync(join(result.reported.buildTarget, 'debug/tickloop')));
    assert.match(result.reported.buildTarget, /\/tickloop-dev-[a-f0-9]{16}$/);
  }
  assert.equal(f.run(first, overrides).reported.buildTarget, a.reported.buildTarget);
  writeFileSync(join(first, 'branch'), 'another-branch');
  const switched = f.run(first, overrides).reported;
  assert.notEqual(switched.buildTarget, a.reported.buildTarget);
  assert.notEqual(switched.buildTarget, b.reported.buildTarget);
  assert.equal(switched.compiledFor, realpathSync(first));
  assert.ok(!existsSync(join(shared, 'debug')));
});

test('switching branches during a build cannot publish into the previous branch namespace', t => {
  const f = fixture(t);
  const checkout = f.checkout('changing branch', { branch: 'before' });
  writeFileSync(join(checkout, 'switch-branch-during-build'), 'after');
  const result = f.run(checkout);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /changed branches during the build/);
  assert.equal(result.stdout, '');
  assert.deepEqual(readdirSync(join(checkout, 'target/dev-runs')), []);
});
