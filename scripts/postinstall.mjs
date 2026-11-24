#!/usr/bin/env node
// Zero-Node-Process npm native binary linker.
//
// On Unix, npm's symlink follows the native binary linked over the JS launcher.
// On Windows, npm's shims invoke a hardlink or copy of the native executable.

import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const BIN_NAME = 'bitefoundry-obsidian';
const LEGACY_BIN_NAMES = ['obsidian', 'obs'];
const PKG_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

function linkOrCopyFile(source, destination) {
  const tmp = `${destination}.tmp-${process.pid}`;
  try {
    fs.rmSync(tmp, { force: true });
  } catch {
    // Ignore cleanup errors on nonexistent temp paths.
  }
  try {
    fs.linkSync(source, tmp);
  } catch {
    fs.copyFileSync(source, tmp);
  }
  try {
    fs.renameSync(tmp, destination);
  } catch {
    try {
      fs.rmSync(destination, { force: true });
    } catch {
      // Ignore; renameSync below will surface or fail safely.
    }
    fs.renameSync(tmp, destination);
  }
}

function replaceJsWithNativeLinkUnix(binaryPath, destinationJs) {
  const tmp = `${destinationJs}.tmp-${process.pid}`;
  try {
    fs.rmSync(tmp, { force: true });
  } catch {
    // Ignore cleanup errors.
  }
  try {
    fs.linkSync(binaryPath, tmp);
  } catch {
    try {
      fs.rmSync(tmp, { force: true });
    } catch {
      // Ignore cleanup errors.
    }
    fs.symlinkSync(binaryPath, tmp);
  }
  try {
    fs.renameSync(tmp, destinationJs);
  } catch (err) {
    try {
      fs.rmSync(tmp, { force: true });
    } catch {
      // Ignore cleanup errors.
    }
    throw err;
  }
}

function collectShimDirs(pkgRoot, platform, env) {
  const dirs = [];
  const parentScope = path.dirname(pkgRoot);
  const nodeModulesDir = path.dirname(parentScope);
  if (
    path.basename(parentScope) === '@bitefoundry' &&
    path.basename(nodeModulesDir) === 'node_modules'
  ) {
    dirs.push(path.join(nodeModulesDir, '.bin'));
    const parent = path.dirname(nodeModulesDir);
    if (platform === 'win32') {
      dirs.push(parent);
    } else if (path.basename(parent) === 'lib') {
      dirs.push(path.join(path.dirname(parent), 'bin'));
    }
  }

  if (env.npm_config_global === 'true' && env.npm_config_prefix) {
    const prefix = path.resolve(env.npm_config_prefix);
    dirs.push(prefix);
    dirs.push(path.join(prefix, 'bin'));
  }

  return [...new Set(dirs)];
}

function shimReferencesLauncher(content, name) {
  const normalized = content.replace(/\\/g, '/').toLowerCase();
  return normalized.includes(`@bitefoundry/obsidian/bin/${name}.js`);
}

function windowsNativeShims(shimDir, pkgRoot, name) {
  const exePath = path.join(pkgRoot, 'bin', `${name}.exe`);
  const relWin = path.relative(shimDir, exePath).replace(/\//g, '\\');
  const relPosix = path.relative(shimDir, exePath).replace(/\\/g, '/');
  return [
    [path.join(shimDir, `${name}.cmd`), `@ECHO off\r\n"%~dp0${relWin}" %*\r\n`],
    [path.join(shimDir, `${name}.ps1`), [
      '#!/usr/bin/env pwsh',
      '$basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent',
      `$exePath = Join-Path $basedir '${relWin.replace(/'/g, "''")}'`,
      'if ($MyInvocation.ExpectingInput) {',
      '  $input | & $exePath @args',
      '} else {',
      '  & $exePath @args',
      '}',
      'exit $LASTEXITCODE',
      '',
    ].join('\n')],
    [path.join(shimDir, name), [
      '#!/bin/sh',
      'basedir=$(dirname "$(echo "$0" | sed -e \'s,\\\\,/,g\')")',
      `exec "$basedir/${relPosix}" "$@"`,
      '',
    ].join('\n')],
  ];
}

function rewriteWindowsShims(shimDir, pkgRoot) {
  let rewritten = 0;
  for (const [shim, body] of windowsNativeShims(shimDir, pkgRoot, BIN_NAME)) {
    try {
      if (fs.lstatSync(shim).isFile() && shimReferencesLauncher(fs.readFileSync(shim, 'utf8'), BIN_NAME)) {
        fs.writeFileSync(shim, body, 'utf8');
        rewritten += 1;
      }
    } catch {
      // Keep npm's launcher if it cannot be rewritten.
    }
  }
  return rewritten;
}

function removeLegacyShims(shimDir, pkgRoot, platform) {
  for (const name of LEGACY_BIN_NAMES) {
    if (platform === 'win32') {
      for (const [shim, body] of windowsNativeShims(shimDir, pkgRoot, name)) {
        try {
          if (fs.lstatSync(shim).isFile() && fs.readFileSync(shim, 'utf8') === body) {
            fs.unlinkSync(shim);
          }
        } catch {
          // An absent or read-only shim needs no change.
        }
      }
    } else {
      const shim = path.join(shimDir, name);
      const oldLauncher = path.join(pkgRoot, 'bin', `${name}.js`);
      try {
        if (path.resolve(shimDir, fs.readlinkSync(shim)) === oldLauncher) {
          fs.unlinkSync(shim);
        }
      } catch {
        // readlinkSync only succeeds for symlinks.
      }
    }
  }
}

export function runPostinstall(options = {}) {
  const pkgRoot = path.resolve(options.pkgRoot || PKG_ROOT);
  const platform = options.platform || process.platform;
  const arch = options.arch || process.arch;
  const env = options.env || process.env;

  if (
    fs.existsSync(path.join(pkgRoot, 'Cargo.toml')) ||
    fs.existsSync(path.join(pkgRoot, '.git'))
  ) {
    return { skipped: true, reason: 'dev_repo' };
  }

  const platformKey = `${platform}-${arch}`;
  const packageName = `@bitefoundry/obsidian-${platformKey}`;
  const binaryName = platform === 'win32' ? `${BIN_NAME}.exe` : BIN_NAME;
  const shimDirs = collectShimDirs(pkgRoot, platform, env);
  for (const shimDir of shimDirs) {
    removeLegacyShims(shimDir, pkgRoot, platform);
  }

  let packageDir = null;
  try {
    const requireFromPkg = createRequire(path.join(pkgRoot, 'package.json'));
    packageDir = path.dirname(requireFromPkg.resolve(`${packageName}/package.json`));
  } catch {
    packageDir = null;
  }

  const binaryPath = packageDir ? path.join(packageDir, 'bin', binaryName) : null;
  if (!binaryPath || !fs.existsSync(binaryPath)) {
    return { skipped: true, reason: 'missing_platform_binary' };
  }

  const binDir = path.join(pkgRoot, 'bin');
  if (!fs.existsSync(binDir)) {
    return { skipped: true, reason: 'missing_bin_dir' };
  }

  if (platform === 'win32') {
    try {
      linkOrCopyFile(binaryPath, path.join(binDir, binaryName));
    } catch {
      return { skipped: false, platform, linked: 0, shimsRewritten: 0 };
    }
    const shimsRewritten = shimDirs.reduce((count, dir) => count + rewriteWindowsShims(dir, pkgRoot), 0);
    return { skipped: false, platform, linked: 1, shimsRewritten };
  }

  try {
    fs.chmodSync(binaryPath, 0o755);
  } catch {
    // Ignore on read-only stores.
  }

  try {
    replaceJsWithNativeLinkUnix(binaryPath, path.join(binDir, `${BIN_NAME}.js`));
  } catch {
    return { skipped: false, platform, linked: 0 };
  }
  return { skipped: false, platform, linked: 1 };
}

function isMainModule() {
  if (!process.argv[1]) {
    return false;
  }
  const modulePath = fileURLToPath(import.meta.url);
  try {
    return fs.realpathSync(process.argv[1]) === fs.realpathSync(modulePath);
  } catch {
    return path.resolve(process.argv[1]) === path.resolve(modulePath);
  }
}

if (isMainModule()) {
  try {
    runPostinstall();
  } catch {
    // Postinstall optimization must never fail npm install.
  }
  process.exit(0);
}
