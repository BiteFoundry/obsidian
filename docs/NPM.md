# npm Cross-Platform Packaging (`@bitefoundry/obsidian`)

`@bitefoundry/obsidian` distributes prebuilt native Rust binaries across 6 OS/CPU targets via npm `optionalDependencies` and a zero-Node-process `postinstall` linker.

## Published Packages

| Package | OS | Arch | Rust Target |
| :--- | :--- | :--- | :--- |
| `@bitefoundry/obsidian` | All | All | Root CLI wrapper (`bitefoundry-obsidian`) |
| `@bitefoundry/obsidian-win32-x64` | Windows | x64 | `x86_64-pc-windows-msvc` |
| `@bitefoundry/obsidian-win32-arm64` | Windows | arm64 | `aarch64-pc-windows-msvc` |
| `@bitefoundry/obsidian-darwin-x64` | macOS (`>= 11.0`) | x64 | `x86_64-apple-darwin` |
| `@bitefoundry/obsidian-darwin-arm64` | macOS (`>= 11.0`) | arm64 | `aarch64-apple-darwin` |
| `@bitefoundry/obsidian-linux-x64` | Linux (`GLIBC >= 2.28`) | x64 | `x86_64-unknown-linux-gnu` |
| `@bitefoundry/obsidian-linux-arm64` | Linux (`GLIBC >= 2.28`) | arm64 | `aarch64-unknown-linux-gnu` |

## Compatibility Guarantees

- **Linux**: Built with `cargo-zigbuild` targeting `GLIBC 2.28` and verified in CI via `node scripts/verify-glibc.mjs --max 2.28` + AlmaLinux 8 container smoke test.
- **macOS**: Built with `MACOSX_DEPLOYMENT_TARGET=11.0` (Big Sur and newer).
- **Local vault access**: SQLite and the YAML parser are bundled with the binary. Local vaults require no cloud sign-in, `libsecret`, or system DBus.

## Maintenance Scripts

```bash
node scripts/sync-version.mjs --check     # Verify Cargo.toml + all 7 package.json versions match
node scripts/verify-glibc.mjs --self-test # Run ELF GLIBC parser self-tests
node scripts/build-npm.mjs --all          # Stage release binaries into npm/obsidian-*/bin
```

## Checklist smoke test

Run the unpacked package's launcher against a temporary vault. Both commands should exit successfully and write the checklist text to the corresponding Markdown file.

```bash
scratch="$(mktemp -d)"
export OBSIDIAN_CONFIG_DIR="$scratch/config"
export OBSIDIAN_DATA_DIR="$scratch/data"
export OBSIDIAN_APP_CONFIG_PATH="$scratch/obsidian.json"
printf '%s\n' '{"vaults":{}}' > "$OBSIDIAN_APP_CONFIG_PATH"
vault="$scratch/vault"
bitefoundry-obsidian init "$vault"
bitefoundry-obsidian --vault "$vault" notes create "Release Review"
bitefoundry-obsidian --vault "$vault" notes append "Release Review" "- [ ] Verify the release"
bitefoundry-obsidian --vault "$vault" daily "--append=- [ ] Review PRs"
```

## Standalone updater transition

Versions 0.2.0 and 0.2.1 include the original asset names for the updater in 0.1.x. While either version is the latest release, run `obsidian update` with the old standalone binary to upgrade at the existing path. Rename the updated executable to `bitefoundry-obsidian` on macOS or Linux, or `bitefoundry-obsidian.exe` on Windows. Use `bitefoundry-obsidian update` for later releases. For npm installs, upgrade with npm.

Change `obsidian notes list` or `obs notes list` in scripts to `bitefoundry-obsidian notes list`. Change `{"command":"obsidian","args":["mcp"]}` or `{"command":"obs","args":["mcp"]}` to `{"command":"bitefoundry-obsidian","args":["mcp"]}` in MCP settings.

## Keep shared commands in an existing npm prefix

If another program supplies `obsidian` or `obs` inside the same global npm prefix used by 0.1.x, install the new version in a separate prefix. npm can remove the old filenames during an in-place upgrade before the new install script runs.

```powershell
$obsidianPrefix = Join-Path $env:LOCALAPPDATA 'BiteFoundry\Obsidian'
npm install --global --prefix "$obsidianPrefix" @bitefoundry/obsidian
& (Join-Path $obsidianPrefix 'bitefoundry-obsidian.cmd') --version
```

```bash
obsidian_prefix="$HOME/.local/bitefoundry-obsidian"
npm install --global --prefix "$obsidian_prefix" @bitefoundry/obsidian
"$obsidian_prefix/bin/bitefoundry-obsidian" --version
```
