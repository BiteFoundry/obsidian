//! Zero-auth local Obsidian vault discovery, multi-vault registry, and starter vault scaffolding.

mod frontmatter;
pub mod fs;
pub mod parser;

use std::{
    collections::{HashMap, HashSet},
    fs as stdfs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::{normalize_path_key, write_atomic, AppConfig, VaultProfile};

#[derive(Debug, Deserialize)]
struct NativeObsidianConfig {
    #[serde(default)]
    vaults: HashMap<String, NativeObsidianVault>,
}

#[derive(Debug, Deserialize)]
struct NativeObsidianVault {
    path: String,
    #[serde(default)]
    ts: Option<i64>,
    #[serde(default)]
    open: Option<bool>,
}

/// Returns the platform-specific path to Obsidian Desktop's native `obsidian.json` config file.
pub fn native_obsidian_config_path() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("OBSIDIAN_APP_CONFIG_PATH") {
        let trimmed = override_path.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(dirs::config_dir)
            .map(|d| d.join("obsidian").join("obsidian.json"))
    }
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| {
            h.join("Library")
                .join("Application Support")
                .join("obsidian")
                .join("obsidian.json")
        })
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        dirs::config_dir().map(|d| d.join("obsidian").join("obsidian.json"))
    }
}

/// Reads Obsidian's native `obsidian.json` and returns all existing local vaults.
pub fn discover_native_vaults() -> Vec<VaultProfile> {
    let Some(path) = native_obsidian_config_path() else {
        return Vec::new();
    };
    discover_native_vaults_from(&path)
}

/// Parses an explicit `obsidian.json` path (filtering to existing directories).
pub fn discover_native_vaults_from(config_path: &Path) -> Vec<VaultProfile> {
    let Ok(raw) = stdfs::read_to_string(config_path) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<NativeObsidianConfig>(&raw) else {
        return Vec::new();
    };

    let mut vaults: Vec<VaultProfile> = parsed
        .vaults
        .into_iter()
        .filter_map(|(id, entry)| {
            let dir = PathBuf::from(entry.path.trim());
            if !dir.is_dir() {
                return None;
            }
            let name = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&id)
                .to_string();
            Some(VaultProfile {
                id,
                name,
                path: dir,
                discovered: true,
                open_in_obsidian: entry.open.unwrap_or(false),
                ts: entry.ts,
                daily_folder: None,
                daily_format: None,
                attachments_folder: None,
            })
        })
        .collect();

    vaults.sort_by(|a, b| {
        b.open_in_obsidian
            .cmp(&a.open_in_obsidian)
            .then_with(|| b.ts.cmp(&a.ts))
            .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
    });
    vaults
}

/// Generates a deterministic short vault ID from its canonical filesystem path.
pub fn derive_vault_id(path: &Path) -> String {
    let key = normalize_path_key(path);
    let digest = Sha256::digest(key.as_bytes());
    hex::encode(&digest[..8])
}

/// Merges manually registered vaults in `cfg` with auto-discovered vaults from `obsidian.json`.
pub fn merged_vaults(cfg: &AppConfig) -> Vec<VaultProfile> {
    let mut out: Vec<VaultProfile> = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();

    for v in &cfg.vaults {
        let key = normalize_path_key(&v.path);
        if seen_paths.insert(key) {
            out.push(v.clone());
        }
    }

    for v in discover_native_vaults() {
        let key = normalize_path_key(&v.path);
        if seen_paths.insert(key) {
            out.push(v);
        }
    }

    out
}

/// Resolves the target vault from CLI `--vault`, `OBSIDIAN_VAULT`, active config, or auto-discovery.
pub fn resolve_active_vault(
    cfg: &AppConfig,
    cli_override: Option<&str>,
) -> Result<VaultProfile> {
    let all = merged_vaults(cfg);

    if let Some(spec) = cli_override.map(str::trim).filter(|s| !s.is_empty()) {
        return resolve_explicit_vault(&all, spec, "--vault");
    }

    if let Some(env_spec) = std::env::var("OBSIDIAN_VAULT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return resolve_explicit_vault(&all, &env_spec, "OBSIDIAN_VAULT");
    }

    if !cfg.active_vault.trim().is_empty() {
        if let Some(found) = find_vault_in_slice(&all, &cfg.active_vault) {
            if found.exists() {
                return Ok(found.clone());
            }
        }
        let direct = PathBuf::from(cfg.active_vault.trim());
        if direct.is_dir() {
            return Ok(vault_from_dir(&direct, None));
        }
    }

    if let Some(open) = all.iter().find(|v| v.open_in_obsidian && v.exists()) {
        return Ok(open.clone());
    }
    if let Some(first) = all.iter().find(|v| v.exists()) {
        return Ok(first.clone());
    }

    bail!(
        "No Obsidian vaults found. Run `bitefoundry-obsidian init <dir>` to create a starter vault or `bitefoundry-obsidian vaults add <path>` to register an existing vault."
    )
}

fn resolve_explicit_vault(
    all: &[VaultProfile],
    spec: &str,
    _source: &str,
) -> Result<VaultProfile> {
    if let Some(found) = find_vault_in_slice(all, spec) {
        if found.exists() {
            return Ok(found.clone());
        }
        bail!(
            "Vault '{}' directory does not exist: {}",
            found.name,
            found.path.display()
        );
    }
    let direct = PathBuf::from(spec);
    if direct.is_dir() {
        return Ok(vault_from_dir(&direct, None));
    }
    bail!(
        "Vault '{spec}' not found. Run `bitefoundry-obsidian vaults list`, `bitefoundry-obsidian vaults add <path>`, or `bitefoundry-obsidian init <dir>`."
    );
}

/// Resolves a vault strictly from registered or auto-discovered vaults (by name, ID, or path).
/// Unregistered directories are rejected with guidance to run `vaults add`.
pub fn resolve_registered_vault(cfg: &AppConfig, spec: &str) -> Result<VaultProfile> {
    let needle = spec.trim();
    if needle.is_empty() {
        bail!("Vault selector cannot be empty.");
    }
    let all = merged_vaults(cfg);
    if let Some(found) = find_vault_in_slice(&all, needle) {
        if found.exists() {
            return Ok(found.clone());
        }
        bail!(
            "Registered vault '{}' directory does not exist: {}",
            found.name,
            found.path.display()
        );
    }
    if Path::new(needle).is_dir() {
        bail!(
            "Vault path '{needle}' is not registered. Register it first with `bitefoundry-obsidian vaults add \"{needle}\"`."
        );
    }
    bail!(
        "Vault '{needle}' is not registered. Run `bitefoundry-obsidian vaults list` or register a directory with `bitefoundry-obsidian vaults add <path>`."
    );
}

pub fn vault_from_dir(dir: &Path, custom_name: Option<&str>) -> VaultProfile {
    let canonical = stdfs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    // Strip Windows extended-length prefix (`\\?\`) for clean display paths.
    let clean_path = strip_windows_verbatim_prefix(&canonical);
    let name = custom_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| {
            clean_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Vault")
                .to_string()
        });
    VaultProfile {
        id: derive_vault_id(&clean_path),
        name,
        path: clean_path,
        discovered: false,
        open_in_obsidian: false,
        ts: None,
        daily_folder: None,
        daily_format: None,
        attachments_folder: None,
    }
}

fn strip_windows_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

fn find_vault_in_slice<'a>(vaults: &'a [VaultProfile], query: &str) -> Option<&'a VaultProfile> {
    let needle = query.trim();
    if needle.is_empty() {
        return None;
    }
    let needle_path = normalize_path_key(Path::new(needle));
    vaults.iter().find(|v| {
        v.name.eq_ignore_ascii_case(needle)
            || v.id.eq_ignore_ascii_case(needle)
            || normalize_path_key(&v.path) == needle_path
    })
}

/// Scaffolds a clean starter Obsidian vault with a Welcome note, Daily Notes folder, and `.obsidian/daily-notes.json`.
pub fn init_starter_vault(dir: &Path) -> Result<VaultProfile> {
    stdfs::create_dir_all(dir)
        .with_context(|| format!("Failed to create vault directory: {}", dir.display()))?;
    stdfs::create_dir_all(dir.join("Daily Notes"))?;
    stdfs::create_dir_all(dir.join("Attachments"))?;
    stdfs::create_dir_all(dir.join(".obsidian"))?;

    let today = Local::now().format("%Y-%m-%d").to_string();

    let daily_cfg = dir.join(".obsidian").join("daily-notes.json");
    if !daily_cfg.exists() {
        write_atomic(
            &daily_cfg,
            b"{\n  \"folder\": \"Daily Notes\",\n  \"format\": \"YYYY-MM-DD\"\n}\n",
        )?;
    }

    let welcome_path = dir.join("Welcome.md");
    if !welcome_path.exists() {
        let welcome = format!(
            "---\ncreated: {today}\ntags: [welcome, second-brain]\naliases: [Home, Index]\n---\n\n\
# Welcome to Your Vault\n\n\
> [!tip] Keyboard-First Knowledge Graph\n\
> Press `G` for interactive Knowledge Graph View, `Ctrl+O` for Quick Switcher, `D` for Today's Daily Note, and `B` for Backlinks.\n\n\
## Quick Links\n\n\
- [[Daily Notes/{today}|Today's Daily Note]]\n\
- [[Getting Started]]\n\n\
## Starter Checklist\n\n\
- [x] Initialize local Obsidian vault\n\
- [ ] Explore [[Getting Started]] and link your first notes\n\
- [ ] Press `G` to inspect the live knowledge graph\n"
        );
        write_atomic(&welcome_path, welcome.as_bytes())?;
    }

    let guide_path = dir.join("Getting Started.md");
    if !guide_path.exists() {
        let guide = format!(
            "---\ncreated: {today}\ntags: [guide, workflow]\n---\n\n\
# Getting Started\n\n\
Return to [[Welcome]] anytime or jump to [[Daily Notes/{today}]].\n\n\
> [!note] 100% Local-First\n\
> Every note is a plain Markdown file on disk. Toggling checkboxes updates the `.md` file atomically.\n"
        );
        write_atomic(&guide_path, guide.as_bytes())?;
    }

    let daily_path = dir.join("Daily Notes").join(format!("{today}.md"));
    if !daily_path.exists() {
        let daily = format!(
            "---\ncreated: {today}\ntags: [daily]\n---\n\n\
# {today}\n\n\
Linked from [[Welcome]].\n\n\
## Today's Tasks\n\n\
- [ ] Capture key notes & links\n"
        );
        write_atomic(&daily_path, daily.as_bytes())?;
    }

    Ok(vault_from_dir(dir, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_native_obsidian_json_filters_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let existing_vault = tmp.path().join("My Brain");
        stdfs::create_dir_all(&existing_vault).unwrap();
        let missing_vault = tmp.path().join("Deleted Vault");

        let cfg_file = tmp.path().join("obsidian.json");
        let json = serde_json::json!({
            "vaults": {
                "abc123": {
                    "path": existing_vault.to_string_lossy(),
                    "ts": 1732000000000i64,
                    "open": true
                },
                "dead99": {
                    "path": missing_vault.to_string_lossy(),
                    "ts": 1700000000000i64
                }
            }
        });
        stdfs::write(&cfg_file, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let found = discover_native_vaults_from(&cfg_file);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "abc123");
        assert_eq!(found[0].name, "My Brain");
        assert!(found[0].open_in_obsidian);
        assert!(found[0].discovered);
    }

    #[test]
    fn test_init_starter_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_dir = tmp.path().join("StarterVault");
        let prof = init_starter_vault(&vault_dir).unwrap();
        assert_eq!(prof.name, "StarterVault");
        assert!(vault_dir.join("Welcome.md").is_file());
        assert!(vault_dir.join("Getting Started.md").is_file());
        assert!(vault_dir.join(".obsidian/daily-notes.json").is_file());
    }
}
