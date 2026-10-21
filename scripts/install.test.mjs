import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const installer = resolve('scripts/install.sh');
function fixture(t, { corrupt = false, linked = false, conflicting = false, system = 'Darwin', machine = 'arm64', target = 'aarch64-apple-darwin', shell = '/bin/zsh', home, onPath = false } = {}) {
  const root = mkdtempSync(join(tmpdir(), 'tickloop-installer-test-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  home ??= join(root, 'home');
  mkdirSync(home, { recursive: true });
  const commands = join(root, 'commands');
  const bundle = join(root, 'bundle');
  mkdirSync(commands);
  mkdirSync(bundle);
  writeFileSync(join(commands, 'uname'), `#!/bin/sh\ncase "$1" in -s) echo ${system};; -m) echo ${machine};; esac\n`, { mode: 0o755 });
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
  writeFileSync(join(root, 'checksum'), `${hash}  tickloop-${target}.tar.gz\n`);
  if (corrupt) writeFileSync(archive, 'corrupted archive');
  writeFileSync(join(commands, 'curl'), `#!${process.execPath}\nconst fs=require('node:fs'); const args=process.argv.slice(2); if(args.includes('%{url_effective}')) { process.stdout.write('https://github.com/tickloopai/homebrew-tap/releases/tag/v1.2.3'); } else { const url=args.find(a=>a.startsWith('https://')); if(!url.includes('/download/v1.2.3/tickloop-${target}.tar.gz')) process.exit(2); fs.copyFileSync(url.endsWith('.sha256')?${JSON.stringify(join(root, 'checksum'))}:${JSON.stringify(archive)}, args[args.indexOf('-o')+1]); }\n`, { mode: 0o755 });
  if (conflicting) writeFileSync(join(commands, 'tickloop'), '#!/bin/sh\nexit 0\n', { mode: 0o755 });
  return spawnSync('/bin/sh', [installer], { encoding: 'utf8', timeout: 15_000,
    env: { ...process.env, HOME: home, SHELL: shell, ZDOTDIR: '', PATH: `${onPath ? `${join(home, '.local/bin')}:` : ''}${commands}:/usr/bin:/bin`, TMPDIR: root } });
}

test('native bootstrap pins the release, verifies its checksum and invokes the installer', t => {
  const result = fixture(t);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /INSTALLER_EXECUTED/);
});

test('native bootstrap installs the static Linux release on ARM64 and x86_64', t => {
  for (const [machine, target] of [['x86_64', 'x86_64-unknown-linux-musl'], ['aarch64', 'aarch64-unknown-linux-musl']]) {
    const result = fixture(t, { system: 'Linux', machine, target });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /INSTALLER_EXECUTED/);
  }
});

test('native bootstrap rejects unsupported platforms before downloading', t => {
  const result = fixture(t, { system: 'FreeBSD', machine: 'amd64' });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /support macOS and Linux/);
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

function profilePath(home, file) {
  const shell = spawnSync('/bin/sh', ['-c', `. "${file}"; printf %s "$PATH"`], { encoding: 'utf8', env: { HOME: home, PATH: '/usr/bin:/bin' } });
  return shell.stdout.split(':');
}

test('native bootstrap adds ~/.local/bin to the PATH of new zsh shells once and says so', t => {
  const home = mkdtempSync(join(tmpdir(), 'tickloop-installer-home-'));
  t.after(() => rmSync(home, { recursive: true, force: true }));
  const first = fixture(t, { home });
  assert.equal(first.status, 0, first.stderr);
  const zshrc = join(home, '.zshrc');
  assert.match(first.stdout, new RegExp(`Added ${home}/\\.local/bin to your PATH in: ${zshrc}\n`));
  assert.match(first.stdout, /Open a new terminal, or run this in the current one:\n  export PATH="\$HOME\/\.local\/bin:\$PATH"\n$/);
  const configured = readFileSync(zshrc, 'utf8');
  assert.deepEqual(profilePath(home, zshrc).filter(entry => entry === join(home, '.local/bin')), [join(home, '.local/bin')]);
  const second = fixture(t, { home });
  assert.equal(second.status, 0, second.stderr);
  assert.match(second.stdout, /is already added to your PATH in:/);
  assert.equal(readFileSync(zshrc, 'utf8'), configured);
});

test('native bootstrap configures bash login and interactive shells and fish', t => {
  const bash = fixture(t, { shell: '/bin/bash' });
  assert.equal(bash.status, 0, bash.stderr);
  assert.match(bash.stdout, /to your PATH in: \S+\/\.bashrc \S+\/\.profile\n/);
  const fish = fixture(t, { shell: '/usr/bin/fish' });
  assert.equal(fish.status, 0, fish.stderr);
  assert.match(fish.stdout, /to your PATH in: \S+\/\.config\/fish\/conf\.d\/tickloop\.fish\n/);
});

test('native bootstrap leaves shell configuration alone when ~/.local/bin is on PATH', t => {
  const home = mkdtempSync(join(tmpdir(), 'tickloop-installer-home-'));
  t.after(() => rmSync(home, { recursive: true, force: true }));
  const result = fixture(t, { home, onPath: true });
  assert.equal(result.status, 0, result.stderr);
  assert.doesNotMatch(result.stdout, /PATH/);
  assert.throws(() => readFileSync(join(home, '.zshrc')));
});

test('native bootstrap explains the PATH change when the shell configuration cannot be written', t => {
  const home = mkdtempSync(join(tmpdir(), 'tickloop-installer-home-'));
  t.after(() => rmSync(home, { recursive: true, force: true }));
  mkdirSync(join(home, '.zshrc'));
  const result = fixture(t, { home });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Could not update your shell configuration; add \S+ to PATH yourself/);
  assert.doesNotMatch(result.stdout, /Added/);
});
