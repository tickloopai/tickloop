import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const installer = resolve('scripts/install.sh');
function fixture(t, { corrupt = false, linked = false, conflicting = false } = {}) {
  const root = mkdtempSync(join(tmpdir(), 'tickloop-installer-test-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const commands = join(root, 'commands');
  const bundle = join(root, 'bundle');
  mkdirSync(commands);
  mkdirSync(bundle);
  writeFileSync(join(commands, 'uname'), '#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n', { mode: 0o755 });
  for (const program of ['tickloop', 'tickhost', 'tick-reader']) {
    writeFileSync(join(bundle, program), `#!/bin/sh\nif [ "$1" = --version ]; then echo '${program} 1.2.3'; elif [ "$1" = update ] && [ "$2" = --install ]; then echo INSTALLER_EXECUTED; else exit 2; fi\n`, { mode: 0o755 });
  }
  if (linked) {
    rmSync(join(bundle, 'tickhost'));
    symlinkSync('tickloop', join(bundle, 'tickhost'));
  }
  const archive = join(root, 'archive.tar.gz');
  assert.equal(spawnSync('/usr/bin/tar', ['--format=ustar', '-czf', archive, '-C', bundle, 'tickloop', 'tickhost', 'tick-reader'], { env: { ...process.env, COPYFILE_DISABLE: '1' } }).status, 0);
  const hash = createHash('sha256').update(readFileSync(archive)).digest('hex');
  writeFileSync(join(root, 'checksum'), `${hash}  tickloop-aarch64-apple-darwin.tar.gz\n`);
  if (corrupt) writeFileSync(archive, 'corrupted archive');
  writeFileSync(join(commands, 'curl'), `#!${process.execPath}\nconst fs=require('node:fs'); const args=process.argv.slice(2); if(args.includes('%{url_effective}')) { process.stdout.write('https://github.com/tickloopai/homebrew-tap/releases/tag/v1.2.3'); } else { const url=args.find(a=>a.startsWith('https://')); if(!url.includes('/download/v1.2.3/')) process.exit(2); fs.copyFileSync(url.endsWith('.sha256')?${JSON.stringify(join(root, 'checksum'))}:${JSON.stringify(archive)}, args[args.indexOf('-o')+1]); }\n`, { mode: 0o755 });
  if (conflicting) writeFileSync(join(commands, 'tickloop'), '#!/bin/sh\nexit 0\n', { mode: 0o755 });
  return spawnSync('/bin/sh', [installer], { encoding: 'utf8', timeout: 15_000,
    env: { ...process.env, PATH: `${commands}:/usr/bin:/bin`, TMPDIR: root } });
}

test('native bootstrap pins the release, verifies its checksum and invokes the installer', t => {
  const result = fixture(t);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /INSTALLER_EXECUTED/);
});

test('native bootstrap never executes a corrupted download', t => {
  const result = fixture(t, { corrupt: true });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /SHA-256 mismatch/);
  assert.doesNotMatch(result.stdout, /INSTALLER_EXECUTED/);
});

test('native bootstrap rejects archive symlinks before extraction', t => {
  const result = fixture(t, { linked: true });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /regular files/);
  assert.doesNotMatch(result.stdout, /INSTALLER_EXECUTED/);
});

test('native bootstrap does not take over another installation', t => {
  const result = fixture(t, { conflicting: true });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /already available/);
  assert.doesNotMatch(result.stdout, /INSTALLER_EXECUTED/);
});
