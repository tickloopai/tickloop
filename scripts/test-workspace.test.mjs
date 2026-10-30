import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, copyFileSync, writeFileSync, readFileSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

test('workspace runner removes its temporary roots on success and failure', t => {
  const directory = mkdtempSync(join(tmpdir(), 'workspace-runner-test-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  mkdirSync(join(directory, 'scripts'));
  copyFileSync(resolve('Makefile'), join(directory, 'Makefile'));
  if (existsSync('scripts/test-workspace.mjs'))
    copyFileSync(resolve('scripts/test-workspace.mjs'), join(directory, 'scripts/test-workspace.mjs'));
  const bin = join(directory, 'bin');
  mkdirSync(bin);
  const report = join(directory, 'report');
  writeFileSync(join(bin, 'node'), `#!/bin/sh\ncase "$1" in *test-workspace.mjs) exec '${process.execPath}' "$@";; esac\n`, { mode: 0o755 });
  writeFileSync(join(bin, 'cargo'), `#!${process.execPath}
    const fs = require('node:fs');
    if (process.argv.includes('--version')) process.exit(0);
    fs.writeFileSync(${JSON.stringify(report)}, JSON.stringify([process.env.TICKLOOP_CONFIG_DIR, process.env.TICKLOOP_TMPDIR, process.env.TICKLOOP_DEBUG ?? null]));
    process.exit(fs.existsSync(${JSON.stringify(join(directory, 'fail'))}) ? 1 : 0);
  `, { mode: 0o755 });
  for (const fails of [false, true]) {
    if (fails) writeFileSync(join(directory, 'fail'), '');
    const result = spawnSync('make', ['test-workspace'], { cwd: directory,
      env: { ...process.env, PATH: `${bin}:/usr/bin:/bin`, HOME: directory, MAKEFLAGS: '', MAKELEVEL: '', TICKLOOP_DEBUG: '1' },
      encoding: 'utf8' });
    const [config, temporary, debug] = JSON.parse(readFileSync(report, 'utf8'));
    // A developer's debug details must not reach the tests' terminal output.
    assert.equal(debug, null);
    const roots = [config, temporary];
    const leaked = roots.filter(root => existsSync(root));
    for (const root of leaked) rmSync(root, { recursive: true, force: true });
    assert.deepEqual(leaked, [], result.stderr);
    assert.equal(result.status === 0, !fails, result.stderr);
  }
});

test('workspace runner reports and cleans only its leaked process and runtime without failing', t => {
  const directory = mkdtempSync(join(tmpdir(), 'workspace-leak-test-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const childFile = join(directory, 'child');
  const runtimeFile = join(directory, 'runtime');
  const script = join(directory, 'leak.mjs');
  writeFileSync(script, `
    import { spawn } from 'node:child_process';
    import { mkdirSync, writeFileSync } from 'node:fs';
    import { join } from 'node:path';
    const root = join(process.env.TICKLOOP_TMPDIR, 'forgotten-host');
    mkdirSync(root); writeFileSync(join(root, 'host.lock'), 'fixture');
    writeFileSync(${JSON.stringify(runtimeFile)}, root);
    const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { detached: true, stdio: 'ignore' });
    writeFileSync(${JSON.stringify(childFile)}, String(child.pid)); child.unref();
  `);
  const result = spawnSync(process.execPath, [resolve('scripts/test-workspace.mjs'), process.execPath, script], {
    encoding: 'utf8', timeout: 20_000,
  });
  const pid = Number(readFileSync(childFile, 'utf8'));
  t.after(() => { try { process.kill(pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; } });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, /1 owned processes, 1 owned runtime directories/);
  assert.ok(!existsSync(readFileSync(runtimeFile, 'utf8')));
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
});

test('workspace runner reruns failed tests one at a time and keeps the rerun result', t => {
  const directory = mkdtempSync(join(tmpdir(), 'workspace-rerun-test-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const bin = join(directory, 'bin');
  mkdirSync(bin);
  const calls = join(directory, 'calls');
  writeFileSync(join(bin, 'cargo'), `#!${process.execPath}
    const fs = require('node:fs');
    const args = process.argv.slice(2);
    fs.appendFileSync(${JSON.stringify(calls)}, JSON.stringify([args, process.env.NEXTEST_EXPERIMENTAL_RECORD ?? null]) + '\\n');
    if (!args.includes('--rerun')) process.exit(Number(process.env.FIRST_EXIT));
    process.exit(Number(process.env.RERUN_EXIT));
  `, { mode: 0o755 });
  for (const [firstExit, rerunExit, reruns, status] of [[0, 1, false, 0], [101, 0, false, 101], [100, 0, true, 0], [100, 100, true, 100]]) {
    rmSync(calls, { force: true });
    const result = spawnSync(process.execPath, [resolve('scripts/test-workspace.mjs')], { encoding: 'utf8',
      env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, FIRST_EXIT: String(firstExit), RERUN_EXIT: String(rerunExit) } });
    const runs = readFileSync(calls, 'utf8').trim().split('\n').map(line => JSON.parse(line));
    assert.equal(result.status, status, result.stderr);
    assert.equal(runs.length, reruns ? 2 : 1, result.stderr);
    assert.ok(runs.every(([args, record]) => record === '1' && args.includes('--user-config-file')));
    if (reruns) {
      const rerun = runs[1][0].join(' ');
      assert.match(rerun, /--no-fail-fast .*--rerun latest --test-threads 1$/);
      assert.match(result.stderr, /Rerunning the failed tests one at a time/);
    }
  }
});
