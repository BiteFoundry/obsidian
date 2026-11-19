#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const PLATFORM_KEY = `${process.platform}-${process.arch}`;
const PACKAGE_NAME = `@bitefoundry/obsidian-${PLATFORM_KEY}`;
const BINARY_NAME = process.platform === 'win32' ? 'obsidian.exe' : 'obsidian';

function resolveBinary() {
  if (process.env.OBSIDIAN_BIN_PATH && fs.existsSync(process.env.OBSIDIAN_BIN_PATH)) {
    return process.env.OBSIDIAN_BIN_PATH;
  }

  const requireFromHere = createRequire(import.meta.url);
  try {
    const pkgJsonPath = requireFromHere.resolve(`${PACKAGE_NAME}/package.json`);
    const candidate = path.join(path.dirname(pkgJsonPath), 'bin', BINARY_NAME);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  } catch {
    // Fall through to local repository fallback.
  }

  const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  const localCandidates = [
    path.join(repoRoot, 'npm', `obsidian-${PLATFORM_KEY}`, 'bin', BINARY_NAME),
    path.join(repoRoot, 'target', 'release', BINARY_NAME),
    path.join(repoRoot, 'target', 'debug', BINARY_NAME),
  ];
  for (const candidate of localCandidates) {
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }

  console.error(
    `[@bitefoundry/obsidian] Unable to locate native binary for ${PLATFORM_KEY} (${PACKAGE_NAME}).\n` +
      `Try reinstalling with optional dependencies enabled:\n` +
      `  npm install -g @bitefoundry/obsidian`,
  );
  process.exit(1);
}

export function runObsidianCli(argv = process.argv.slice(2)) {
  const binary = resolveBinary();
  if (process.platform !== 'win32') {
    try {
      fs.chmodSync(binary, 0o755);
    } catch {
      // Ignore chmod failures on read-only package stores.
    }
  }

  const result = spawnSync(binary, argv, {
    stdio: 'inherit',
    windowsHide: false,
  });

  if (result.error) {
    console.error(`[@bitefoundry/obsidian] Failed to launch ${binary}: ${result.error.message}`);
    process.exit(1);
  }
  if (result.signal) {
    process.kill(process.pid, result.signal);
  }
  process.exit(result.status ?? 0);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  runObsidianCli();
}
