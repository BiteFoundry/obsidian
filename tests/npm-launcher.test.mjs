import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

const launcher = 'bitefoundry-obsidian';
const platformPackage = `obsidian-${process.platform}-${process.arch}`;

test('npm symlink runs the launcher with install scripts disabled', { skip: process.platform === 'win32' }, (t) => {
  const prefix = fs.mkdtempSync(path.join(os.tmpdir(), 'obsidian-npm-'));
  t.after(() => fs.rmSync(prefix, { recursive: true, force: true }));

  const scope = path.join(prefix, 'lib', 'node_modules', '@bitefoundry');
  const packageRoot = path.join(scope, 'obsidian');
  const jsBin = path.join(packageRoot, 'bin', `${launcher}.js`);
  const globalBin = path.join(prefix, 'bin', launcher);
  fs.mkdirSync(path.dirname(jsBin), { recursive: true });
  fs.copyFileSync(new URL(`../bin/${launcher}.js`, import.meta.url), jsBin);
  fs.writeFileSync(path.join(packageRoot, 'package.json'), '{"type":"module"}');
  fs.mkdirSync(path.dirname(globalBin));
  fs.symlinkSync(path.relative(path.dirname(globalBin), jsBin), globalBin);

  const env = { ...process.env };
  delete env.OBSIDIAN_BIN_PATH;
  const run = (...args) => spawnSync(process.execPath, [globalBin, ...args], { encoding: 'utf8', env });
  const missing = run('--version');
  assert.equal(missing.status, 1);
  assert.match(missing.stderr, /Unable to locate native binary/);

  const platformRoot = path.join(scope, platformPackage);
  const nativeBin = path.join(platformRoot, 'bin', launcher);
  fs.mkdirSync(path.dirname(nativeBin), { recursive: true });
  fs.writeFileSync(path.join(platformRoot, 'package.json'), '{}');
  fs.writeFileSync(nativeBin, '#!/bin/sh\nprintf "%s\\n" "$@"\nexit 7\n', { mode: 0o755 });

  const launched = run('notes', 'note with spaces');
  assert.equal(launched.status, 7);
  assert.equal(launched.stdout, 'notes\nnote with spaces\n');
  assert.equal(launched.stderr, '');
});

test('postinstall runs when invoked via a symlinked prefix or home path', (t) => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'obsidian-postinstall-symlink-'));
  t.after(() => fs.rmSync(base, { recursive: true, force: true }));

  const realPrefix = path.join(base, 'real-prefix');
  const linkPrefix = path.join(base, 'linked-prefix');
  fs.mkdirSync(realPrefix, { recursive: true });
  try {
    fs.symlinkSync(realPrefix, linkPrefix, process.platform === 'win32' ? 'dir' : undefined);
  } catch (err) {
    if (process.platform === 'win32' && err && err.code === 'EPERM') {
      t.skip('Windows unprivileged directory symlinks unavailable');
      return;
    }
    throw err;
  }

  const scopeReal = path.join(realPrefix, 'lib', 'node_modules', '@bitefoundry');
  const pkgRootReal = path.join(scopeReal, 'obsidian');
  const postinstallReal = path.join(pkgRootReal, 'scripts', 'postinstall.mjs');
  const jsBinReal = path.join(pkgRootReal, 'bin', `${launcher}.js`);
  fs.mkdirSync(path.dirname(postinstallReal), { recursive: true });
  fs.mkdirSync(path.dirname(jsBinReal), { recursive: true });
  fs.copyFileSync(new URL('../scripts/postinstall.mjs', import.meta.url), postinstallReal);
  fs.writeFileSync(path.join(pkgRootReal, 'package.json'), '{"name":"@bitefoundry/obsidian","type":"module"}');
  fs.writeFileSync(jsBinReal, '// placeholder js launcher\n');

  const binName = process.platform === 'win32' ? `${launcher}.exe` : launcher;
  const platformRootReal = path.join(scopeReal, platformPackage);
  const nativeBinReal = path.join(platformRootReal, 'bin', binName);
  const marker = '#!/bin/sh\necho linked-via-symlinked-postinstall\n';
  fs.mkdirSync(path.dirname(nativeBinReal), { recursive: true });
  fs.writeFileSync(
    path.join(platformRootReal, 'package.json'),
    JSON.stringify({ name: `@bitefoundry/${platformPackage}` }),
  );
  fs.writeFileSync(nativeBinReal, marker, { mode: 0o755 });

  const postinstallViaLink = path.join(
    linkPrefix,
    'lib',
    'node_modules',
    '@bitefoundry',
    'obsidian',
    'scripts',
    'postinstall.mjs',
  );
  const res = spawnSync(process.execPath, [postinstallViaLink], { encoding: 'utf8' });
  assert.equal(res.status, 0, res.stderr);

  if (process.platform === 'win32') {
    const linkedExe = path.join(pkgRootReal, 'bin', binName);
    assert.equal(fs.existsSync(linkedExe), true, 'expected native exe to be linked/copied');
    assert.equal(fs.readFileSync(linkedExe, 'utf8'), marker);
  } else {
    assert.equal(fs.readFileSync(jsBinReal, 'utf8'), marker);
  }
});
