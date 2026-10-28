import test from 'node:test';
import assert from 'node:assert/strict';
import { chmodSync, copyFileSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const launcher = resolve('scripts/dev-agent.sh');
const makefile = resolve('Makefile');
const PROD_ORIGIN = 'https://tickloop.ai';
const PRE_ORIGIN = 'https://pre.tickloop.ai';
const DEV_ORIGIN = 'https://dev-cloud.example.test';
const STALE_ORIGIN = 'https://stale-cloud.example.test';

function makeWritable(directory) {
  if (!lstatSync(directory).isDirectory()) return;
  chmodSync(directory, 0o700);
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (entry.isDirectory()) makeWritable(join(directory, entry.name));
  }
}

function fixture(t) {
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'tickloop-dev-launch-test-')));
  t.after(() => {
    makeWritable(root);
    rmSync(root, { recursive: true, force: true });
  });
  const commands = join(root, 'commands');
  const caller = join(root, 'caller directory');
  const home = join(root, 'home');
  mkdirSync(home);
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
    fs.appendFileSync(path.join(process.env.HOME, 'program-calls'), JSON.stringify({
      program: path.basename(process.argv[1]), args, config: process.env.TICKLOOP_CONFIG_DIR,
      runtime: process.env.TICKLOOP_TICK_RUNTIME_DIR }) + '\\n');
    if (args.join(' ') === 'daemon stop' || args.join(' ') === 'stop') process.exit(0);
    if (args[0] === 'tick' && args[1] === 'list') { console.log('fixture-package'); process.exit(0); }
    if (args[0] !== '--development-root' || !args[1]) process.exit(91);
    const developmentRoot = args.splice(0, 2)[1];
    const bin = path.dirname(fs.realpathSync(process.argv[1]));
    fs.writeFileSync(path.join(bin, 'installation.json'), JSON.stringify({ developmentRoot }));
    fs.chmodSync(bin, 0o500); fs.chmodSync(path.dirname(bin), 0o500);
    console.log(JSON.stringify({ cwd: fs.realpathSync('.'), executable: fs.realpathSync(process.argv[1]), args,
      developmentRoot, compiledFor: __COMPILED_CHECKOUT__, buildTarget: __BUILD_TARGET__,
      environment: Object.fromEntries(Object.entries(process.env).filter(([name]) => name.endsWith('_RUNTIME_DIR'))),
      config: process.env.TICKLOOP_CONFIG_DIR, home: process.env.HOME,
      artifactOrigin: process.env.TICKLOOP_ARTIFACT_ORIGIN, devArtifactOrigin: process.env.TICKLOOP_DEV_ARTIFACT_ORIGIN,
      debug: process.env.TICKLOOP_DEBUG,
      shellStartupFile: process.env.ENV }));
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
  `);

  function checkout(name, { branch = 'main', revision, archive = false } = {}) {
    const directory = join(root, name);
    mkdirSync(join(directory, 'scripts'), { recursive: true });
    copyFileSync(launcher, join(directory, 'scripts/dev-agent.sh'));
    copyFileSync(makefile, join(directory, 'Makefile'));
    if (!archive) writeFileSync(join(directory, '.git'), 'fixture-only git metadata\n');
    if (branch !== null) writeFileSync(join(directory, 'branch'), branch);
    if (revision) writeFileSync(join(directory, 'revision'), revision);
    return directory;
  }

  function environment(overrides) {
    const env = { ...process.env, HOME: home, PATH: `${commands}:/usr/bin:/bin`,
      TICKLOOP_CONFIG_DIR: join(root, 'global configuration') };
    // An enclosing `make test` passes its own variables and command line down to every sub-make.
    for (const name of ['TICKLOOP_DEV_CONFIG_DIR', 'TICKLOOP_TICK_RUNTIME_DIR', 'CARGO_TARGET_DIR', 'TICKLOOP_ARTIFACT_ORIGIN', 'TICKLOOP_DEV_ARTIFACT_ORIGIN', 'TICKLOOP_DEBUG', 'ENV', 'MAKEFLAGS', 'MFLAGS', 'MAKEOVERRIDES', 'MAKELEVEL'])
      delete env[name];
    return Object.assign(env, overrides);
  }

  // make echoes the launcher command before the Agent prints its report.
  function reportedBy(result) {
    return { ...result, reported: result.status === 0 ? JSON.parse(result.stdout.trim().split('\n').at(-1)) : null };
  }

  function run(directory, overrides = {}, args = []) {
    return reportedBy(spawnSync('/bin/sh', [join(directory, 'scripts/dev-agent.sh'), ...args], {
      cwd: caller, env: environment(overrides), encoding: 'utf8', timeout: 15_000,
    }));
  }

  function runMake(directory, makeArguments = [], overrides = {}) {
    return reportedBy(spawnSync('make', ['--no-print-directory', '-C', directory, 'dev', ...makeArguments], {
      cwd: caller, env: environment(overrides), encoding: 'utf8', timeout: 15_000,
    }));
  }
  return { root, home, caller, checkout, run, runMake };
}

test('actual launcher publishes a fresh immutable program bundle for the dev daemon', t => {
  const f = fixture(t);
  const checkout = f.checkout('worktree with spaces', { branch: 'feature/升级' });
  const first = f.run(checkout, {}, ['-c', '--cwd', f.caller]);
  const second = f.run(checkout);
  assert.equal(first.status, 0, first.stderr);
  assert.equal(second.status, 0, second.stderr);
  assert.notEqual(first.reported.executable, second.reported.executable);
  assert.equal(first.reported.cwd, realpathSync(f.caller));
  assert.equal(first.reported.developmentRoot, realpathSync(checkout));
  assert.equal(first.reported.compiledFor, realpathSync(checkout));
  assert.equal(first.reported.buildTarget, join(realpathSync(checkout), 'target'));
  assert.deepEqual(JSON.parse(readFileSync(join(dirname(first.reported.executable), 'installation.json'), 'utf8')),
    { developmentRoot: realpathSync(checkout) });
  assert.deepEqual(first.reported.args, ['-c', '--cwd', f.caller]);
  assert.equal(first.reported.config, join(f.home, '.tickloop-dev'));
  assert.equal(first.reported.home, f.home);
  assert.equal(lstatSync(first.reported.executable).mode & 0o777, 0o500);
  assert.equal(lstatSync(dirname(first.reported.executable)).mode & 0o777, 0o500);
  assert.ok(existsSync(first.reported.executable));
  assert.ok(!existsSync(join(checkout, 'git-calls')));
});

test('every checkout uses the dev root and discards inherited runtime overrides', t => {
  const f = fixture(t);
  const first = f.checkout('first', { branch: 'feature/a' });
  const second = f.checkout('second', { branch: 'feature/b' });
  for (const checkout of [first, second]) {
    const result = f.run(checkout, { TICKLOOP_TICK_RUNTIME_DIR: '/must-not-use' });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(Object.keys(result.reported.environment), []);
    assert.equal(result.reported.config, join(f.home, '.tickloop-dev'));
  }
});

test('an explicit CARGO_TARGET_DIR is the build target', t => {
  const f = fixture(t);
  const checkout = f.checkout('shared target');
  const shared = join(f.root, 'shared build target');
  const result = f.run(checkout, { CARGO_TARGET_DIR: shared });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.reported.buildTarget, realpathSync(shared));
  assert.equal(result.reported.compiledFor, realpathSync(checkout));
});

test('make dev shows the debug details by default and keeps the caller\'s choice', t => {
  const f = fixture(t);
  const checkout = f.checkout('debug details');
  const byDefault = f.runMake(checkout);
  assert.equal(byDefault.status, 0, byDefault.stderr);
  assert.equal(byDefault.reported.debug, '1');
  for (const [choice, launch] of [
    ['0', () => f.runMake(checkout, [], { TICKLOOP_DEBUG: '0' })],
    ['0', () => f.runMake(checkout, ['TICKLOOP_DEBUG=0'])],
    ['0', () => f.run(checkout, { TICKLOOP_DEBUG: '0' })],
    ['1', () => f.run(checkout)],
  ]) {
    const result = launch();
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.reported.debug, choice);
  }
});

test('make dev talks to production Tickloop Cloud by default, over any configured origin', t => {
  const f = fixtureWithStaleOrigin(t);
  for (const makeArguments of [[], ['ENV=prod']]) {
    const result = f.runMake(f.directory, makeArguments, { TICKLOOP_ARTIFACT_ORIGIN: STALE_ORIGIN });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.reported.artifactOrigin, PROD_ORIGIN);
    assert.match(result.stderr, /^Tickloop Cloud: https:\/\/tickloop\.ai \(ENV=prod\)$/m);
  }
});

test('make dev ENV=pre talks to pre and keeps ARGS', t => {
  const f = fixtureWithStaleOrigin(t);
  const result = f.runMake(f.directory, ['ENV=pre', 'ARGS=-c'], { TICKLOOP_ARTIFACT_ORIGIN: STALE_ORIGIN });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.reported.artifactOrigin, PRE_ORIGIN);
  assert.deepEqual(result.reported.args, ['-c']);
  assert.match(result.stderr, /^Tickloop Cloud: https:\/\/pre\.tickloop\.ai \(ENV=pre\)$/m);
});

test('make dev ENV=dev uses the Makefile-only dev origin and stops before building without it', t => {
  const f = fixture(t);
  const configured = f.checkout('dev from dotenv');
  writeFileSync(join(configured, '.env'),
    `TICKLOOP_DEV_ARTIFACT_ORIGIN=${DEV_ORIGIN}\nTICKLOOP_ARTIFACT_ORIGIN=${STALE_ORIGIN}\n`);
  const result = f.runMake(configured, ['ENV=dev'], { TICKLOOP_ARTIFACT_ORIGIN: STALE_ORIGIN });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.reported.artifactOrigin, DEV_ORIGIN);
  assert.equal(result.reported.devArtifactOrigin, undefined);
  assert.match(result.stderr, /^Tickloop Cloud: https:\/\/dev-cloud\.example\.test \(ENV=dev\)$/m);

  // The Agent's own origin variable, from .env or the shell, never stands in for the dev origin.
  const unconfigured = f.checkout('dev without origin');
  writeFileSync(join(unconfigured, '.env'), `TICKLOOP_ARTIFACT_ORIGIN=${STALE_ORIGIN}\n`);
  const missing = f.runMake(unconfigured, ['ENV=dev'], { TICKLOOP_ARTIFACT_ORIGIN: STALE_ORIGIN });
  assert.notEqual(missing.status, 0);
  assert.match(missing.stderr, /put TICKLOOP_DEV_ARTIFACT_ORIGIN=<your dev cloud origin> in \.env/);
  assert.doesNotMatch(missing.stderr, /^Tickloop Cloud: /m);
  assert.ok(!existsSync(join(unconfigured, 'cargo-calls')));
});

test('an exported ENV or TICKLOOP_ARTIFACT_ORIGIN does not move make dev off production', t => {
  const f = fixture(t);
  const checkout = f.checkout('exported env');
  writeFileSync(join(checkout, '.env'), `TICKLOOP_DEV_ARTIFACT_ORIGIN=${DEV_ORIGIN}\n`);
  for (const exportedEnvironment of ['pre', 'dev', 'staging']) {
    const result = f.runMake(checkout, [], { ENV: exportedEnvironment, TICKLOOP_ARTIFACT_ORIGIN: STALE_ORIGIN });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.reported.artifactOrigin, PROD_ORIGIN);
    assert.equal(result.reported.shellStartupFile, undefined);
    assert.equal(result.reported.devArtifactOrigin, undefined);
  }
  const chosen = f.runMake(checkout, ['ENV=pre'], { ENV: 'dev' });
  assert.equal(chosen.reported.artifactOrigin, PRE_ORIGIN);
  assert.equal(chosen.reported.shellStartupFile, undefined);
});

test('an unknown ENV fails immediately and lists the valid values', t => {
  const f = fixture(t);
  const checkout = f.checkout('unknown env');
  for (const value of ['staging', '', 'pre dev']) {
    const result = f.runMake(checkout, [`ENV=${value}`]);
    assert.notEqual(result.status, 0);
    assert.ok(result.stderr.includes(`Unknown ENV '${value}'; use ENV=prod (the default), ENV=pre or ENV=dev`), result.stderr);
  }
  assert.ok(!existsSync(join(checkout, 'cargo-calls')));
});

function fixtureWithStaleOrigin(t) {
  const f = fixture(t);
  const directory = f.checkout('stale origin');
  writeFileSync(join(directory, '.env'), `TICKLOOP_ARTIFACT_ORIGIN=${STALE_ORIGIN}\n`);
  return { ...f, directory };
}

 test('only the dedicated override selects a different dev root', t => {
  const f = fixture(t);
  const root = join(f.root, 'dedicated dev');
  const result = f.runMake(f.checkout('override'), [], { TICKLOOP_DEV_CONFIG_DIR: root });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.reported.config, root);
  assert.ok(result.stderr.includes(`Development configuration: ${root}`));
});

const loginFiles = ['openai-codex-credentials', 'github-copilot', 'minimax-cn', 'minimax-global',
  'kimi-code-oauth', 'xai-oauth', 'google-oauth'].map(name => `auth/${name}.json`);

test('first use seeds only settings and model credentials privately, once', t => {
  const f = fixture(t);
  const checkout = f.checkout('seed');
  const source = join(f.home, '.tickloop');
  const dev = join(f.home, '.tickloop-dev');
  const allowed = ['settings.json', ...loginFiles];
  const excluded = ['plugins/installed.json', 'packages/installed.json', 'auth/google-service-token.json',
    'auth/plugin.json', 'auth/openai-codex.lock', 'chats/chat.json', 'history', 'file-history/a',
    'scheduled-tasks.json', 'cache/item'];
  for (const file of [...allowed, ...excluded]) {
    mkdirSync(dirname(join(source, file)), { recursive: true });
    writeFileSync(join(source, file), `fixture ${file}`);
  }
  writeFileSync(join(source, '.env'), 'OPENAI_API_KEY="fixture-key"\nGOOGLE_CLOUD_PROJECT="project"\nPLUGIN_SECRET="never-copy"\n');
  const result = f.run(checkout);
  assert.equal(result.status, 0, result.stderr);
  for (const file of allowed) {
    assert.equal(readFileSync(join(dev, file), 'utf8'), `fixture ${file}`);
    assert.equal(lstatSync(join(dev, file)).mode & 0o777, 0o600);
    assert.equal(readFileSync(join(source, file), 'utf8'), `fixture ${file}`);
  }
  assert.equal(readFileSync(join(dev, '.env'), 'utf8'), 'OPENAI_API_KEY="fixture-key"\nGOOGLE_CLOUD_PROJECT="project"\n');
  for (const file of excluded) assert.ok(!existsSync(join(dev, file)), file);
  for (const directory of [dev, join(dev, 'auth')]) assert.equal(lstatSync(directory).mode & 0o777, 0o700);
  assert.match(result.stderr, /Seeded development configuration: .*settings.json/);
  writeFileSync(join(dev, 'settings.json'), 'dev settings');
  const again = f.run(checkout);
  assert.equal(again.status, 0, again.stderr);
  assert.equal(readFileSync(join(dev, 'settings.json'), 'utf8'), 'dev settings');
  assert.doesNotMatch(again.stderr, /Seeded development configuration/);
});

test('built programs stop only the dev daemon and Tickhost before launch', t => {
  const f = fixture(t);
  const result = f.run(f.checkout('stop'));
  assert.equal(result.status, 0, result.stderr);
  const calls = readFileSync(join(f.home, 'program-calls'), 'utf8').trim().split('\n').map(JSON.parse);
  assert.deepEqual(calls.slice(0, 2).map(({ program, args }) => ({ program, args })), [
    { program: 'tickloop', args: ['daemon', 'stop'] }, { program: 'tickhost', args: ['stop'] },
  ]);
  for (const call of calls) {
    assert.equal(call.config, join(f.home, '.tickloop-dev'));
    assert.equal(call.runtime, undefined);
  }
});

test('released root and aliases are rejected before building or changing files', t => {
  const f = fixture(t);
  const source = join(f.home, '.tickloop');
  mkdirSync(source);
  const alias = join(f.home, 'alias');
  symlinkSync(source, alias);
  const checkout = f.checkout('unsafe override');
  for (const root of [source, alias, `${source}/.`, join(source, 'nested/dev')]) {
    const result = f.run(checkout, { TICKLOOP_DEV_CONFIG_DIR: root });
    assert.notEqual(result.status, 0, root);
    assert.ok(!existsSync(join(checkout, 'cargo-calls')));
    assert.deepEqual(readdirSync(source), []);
  }
});

test('a failed seed removes its partial root so a retry can seed', t => {
  const f = fixture(t);
  const source = join(f.home, '.tickloop');
  mkdirSync(source);
  writeFileSync(join(source, 'settings.json'), 'settings');
  writeFileSync(join(f.root, 'commands/cp'), '#!/bin/sh\nexit 1\n', { mode: 0o755 });
  const checkout = f.checkout('failed seed');
  const failed = f.run(checkout);
  assert.notEqual(failed.status, 0);
  assert.ok(!existsSync(join(f.home, '.tickloop-dev')));
  rmSync(join(f.root, 'commands/cp'));
  const retry = f.run(checkout);
  assert.equal(retry.status, 0, retry.stderr);
  assert.equal(readFileSync(join(f.home, '.tickloop-dev/settings.json'), 'utf8'), 'settings');
});
