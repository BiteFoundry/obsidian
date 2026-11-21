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
