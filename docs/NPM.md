# npm Cross-Platform Packaging (`@bitefoundry/obsidian`)

`@bitefoundry/obsidian` distributes prebuilt native Rust binaries across 6 OS/CPU targets via npm `optionalDependencies` and a zero-Node-process `postinstall` linker.

## Published Packages

| Package | OS | Arch | Rust Target |
| :--- | :--- | :--- | :--- |
| `@bitefoundry/obsidian` | All | All | Root CLI wrapper (`obsidian`, `obs`) |
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
export OBSIDIAN_AUTO_DAEMON=0
printf '%s\n' '{"vaults":{}}' > "$OBSIDIAN_APP_CONFIG_PATH"
vault="$scratch/vault"
obsidian init "$vault"
obsidian --vault "$vault" notes create "Release Review"
obsidian --vault "$vault" notes append "Release Review" -- "- [ ] Ship v0.1.0"
obsidian --vault "$vault" daily "--append=- [ ] Review PRs"
```
