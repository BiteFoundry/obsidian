//! Application configuration, multi-vault registry (`config.toml`), local paths, and atomic I/O.

use std::{
    borrow::Cow,
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const APP_DIR_NAME: &str = "obsidian-cli";
pub const DEFAULT_GITHUB_REPO: &str = "BiteFoundry/obsidian";
pub const DEFAULT_SERVE_PORT: u16 = 8792;
pub const IPC_SERVICE_NAME: &str = "obsidian-cli";

/// TUI note canvas horizontal width mode (`Centered` readable measure vs `Full` width).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageWidthMode {
    #[default]
    Centered,
    Full,
}

impl PageWidthMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::Centered => Self::Full,
            Self::Full => Self::Centered,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Centered => "Centered",
            Self::Full => "Full",
        }
    }
}

/// Terminal color theme preset (`Auto` detects `NO_COLOR`, `COLORTERM`, and `TERM`).
/// Includes 5 curated palettes: Obsidian Dark, Catppuccin Mocha, Tokyo Night, Nord, Warm Paper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreset {
    #[default]
    Auto,
    #[serde(alias = "dark", alias = "obsidian-dark")]
    ObsidianDark,
    #[serde(alias = "catppuccin-mocha")]
    Catppuccin,
    #[serde(alias = "tokyo-night")]
    TokyoNight,
    Nord,
    #[serde(alias = "light", alias = "warm-paper")]
    WarmPaper,
    Ansi256,
    Ansi16,
    NoColor,
}

impl ThemePreset {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::ObsidianDark,
            Self::ObsidianDark => Self::Catppuccin,
            Self::Catppuccin => Self::TokyoNight,
            Self::TokyoNight => Self::Nord,
            Self::Nord => Self::WarmPaper,
            Self::WarmPaper => Self::Ansi256,
            Self::Ansi256 => Self::Ansi16,
            Self::Ansi16 => Self::NoColor,
            Self::NoColor => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::ObsidianDark => "Obsidian Dark",
            Self::Catppuccin => "Catppuccin Mocha",
            Self::TokyoNight => "Tokyo Night",
            Self::Nord => "Nord",
            Self::WarmPaper => "Warm Paper",
            Self::Ansi256 => "ANSI 256",
            Self::Ansi16 => "ANSI 16",
            Self::NoColor => "No Color",
        }
    }
}

/// Terminal image rendering protocol preference (`auto` detects Kitty / iTerm2 / Sixel / Halfblock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageProtocolPreference {
    #[default]
    Auto,
    Kitty,
    #[serde(alias = "iterm")]
    Iterm2,
    Sixel,
    #[serde(alias = "halfblocks", alias = "ansi")]
    Halfblock,
}

impl ImageProtocolPreference {
    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "kitty" => Some(Self::Kitty),
            "iterm2" | "iterm" => Some(Self::Iterm2),
            "sixel" => Some(Self::Sixel),
            "halfblock" | "halfblocks" | "ansi" => Some(Self::Halfblock),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kitty => "kitty",
            Self::Iterm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::Halfblock => "halfblock",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Kitty => "Kitty",
            Self::Iterm2 => "iTerm2",
            Self::Sixel => "Sixel",
            Self::Halfblock => "Halfblock",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Kitty,
            Self::Kitty => Self::Iterm2,
            Self::Iterm2 => Self::Sixel,
            Self::Sixel => Self::Halfblock,
            Self::Halfblock => Self::Auto,
        }
    }
}

/// Registered or auto-discovered local Obsidian vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VaultProfile {
    /// Stable vault identifier (from `obsidian.json` key or derived hash/slug).
    pub id: String,
    /// Human-readable vault name (folder basename or custom `--name`).
    pub name: String,
    /// Absolute filesystem path to the vault root directory.
    pub path: PathBuf,
    /// True if discovered automatically from Obsidian's native `obsidian.json`.
    pub discovered: bool,
    /// True if marked `"open": true` in Obsidian's `obsidian.json`.
    pub open_in_obsidian: bool,
    /// Last opened timestamp (ms) from `obsidian.json`, if available.
    pub ts: Option<i64>,
    /// Optional override for daily notes folder (defaults to `.obsidian/daily-notes.json` or `"Daily Notes"`).
    pub daily_folder: Option<String>,
    /// Optional override for daily note filename date format (default `"YYYY-MM-DD"`).
    pub daily_format: Option<String>,
    /// Optional folder for pasted/attached files (default `"Attachments"`).
    pub attachments_folder: Option<String>,
}

impl VaultProfile {
    pub fn display_name(&self) -> &str {
        if !self.name.trim().is_empty() {
            &self.name
        } else {
            self.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&self.id)
        }
    }

    pub fn exists(&self) -> bool {
        self.path.is_dir()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Active vault name or ID.
    pub active_vault: String,
    /// Manually registered vaults (auto-discovered vaults from `obsidian.json` are merged at runtime).
    pub vaults: Vec<VaultProfile>,
    /// GitHub `owner/repo` used by `bitefoundry-obsidian update`.
    pub github_repo: String,
    /// Show left navigation sidebar in wide/medium layout.
    pub show_sidebar: bool,
    /// Show right backlinks & outgoing links inspector pane.
    pub show_inspector: bool,
    /// Readable centered measure (`centered`) vs full width (`full`).
    pub page_width: PageWidthMode,
    /// Render inline image thumbnails directly inside note canvas.
    pub inline_images: bool,
    /// Show inline hyperlink `[href]` URLs next to link text in note canvas (toggleable with `h`).
    pub show_inline_hrefs: bool,
    /// Preferred terminal image rendering protocol (`auto`, `kitty`, `iterm2`, `sixel`, `halfblock`).
    pub image_protocol: ImageProtocolPreference,
    /// Custom download/export directory. Empty defaults to `Downloads/obsidian`.
    pub download_dir: String,
    /// Background filesystem watch/reindex poll interval (ms).
    pub watch_poll_ms: u64,
    /// TUI color theme preset.
    pub theme: ThemePreset,
    /// Set by [`AppConfig::load`] when the config file exists but could not be read or parsed.
    /// Such a config holds defaults only, and [`AppConfig::save_to`] refuses to write it, so the
    /// broken file is never replaced. Never stored in the file.
    #[serde(skip)]
    pub load_failed: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            active_vault: first_non_empty_env(&["OBSIDIAN_VAULT"]).unwrap_or_default(),
            vaults: Vec::new(),
            github_repo: first_non_empty_env(&["OBSIDIAN_GITHUB_REPO"])
                .unwrap_or_else(|| DEFAULT_GITHUB_REPO.to_string()),
            show_sidebar: true,
            show_inspector: true,
            page_width: PageWidthMode::Centered,
            inline_images: true,
            show_inline_hrefs: false,
            image_protocol: first_non_empty_env(&["OBSIDIAN_IMAGE_PROTOCOL"])
                .and_then(|s| ImageProtocolPreference::parse_str(&s))
                .unwrap_or_default(),
            download_dir: String::new(),
            watch_poll_ms: 2_500,
            theme: ThemePreset::default(),
            load_failed: false,
        }
    }
}

impl AppConfig {
    /// Loads the config for this run, with environment overrides applied.
    ///
    /// A missing file yields the defaults. A file that exists but cannot be read or parsed also
    /// yields the defaults, with a warning on stderr and `load_failed` set, so the run can go on
    /// read-only and no later save replaces the file.
    pub fn load() -> Self {
        let path = config_file_path();
        let mut cfg = match Self::read_optional(&path) {
            Ok(Some(cfg)) => cfg,
            Ok(None) => return Self::default(),
            Err(_) => {
                eprintln!(
                    "warning: {} could not be parsed; using defaults for this run, settings will not be saved",
                    path.display()
                );
                return Self {
                    load_failed: true,
                    ..Self::default()
                };
            }
        };
        if let Some(v) = first_non_empty_env(&["OBSIDIAN_VAULT"]) {
            cfg.active_vault = v;
        }
        if let Some(repo) = first_non_empty_env(&["OBSIDIAN_GITHUB_REPO"]) {
            cfg.github_repo = repo;
        }
        if let Some(proto) = first_non_empty_env(&["OBSIDIAN_IMAGE_PROTOCOL"]) {
            if let Some(parsed) = ImageProtocolPreference::parse_str(&proto) {
                cfg.image_protocol = parsed;
            }
        }
        cfg
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        #[cfg(test)]
        if std::env::var_os("OBSIDIAN_CONFIG_DIR").is_none() {
            return Ok(());
        }
        self.save_to(&config_file_path())
    }

    /// Reads the config file as stored, for commands that change it and write it back.
    ///
    /// A missing file yields an empty config (first run). A file that cannot be read or parsed
    /// is an error, so the caller stops before anything is written.
    pub fn load_persisted() -> Result<Self> {
        Self::load_persisted_from(&config_file_path())
    }

    /// [`AppConfig::load_persisted`] for an explicit path.
    pub fn load_persisted_from(path: &Path) -> Result<Self> {
        Ok(Self::read_optional(path)?.unwrap_or_else(|| Self {
            active_vault: String::new(),
            ..Self::default()
        }))
    }

    /// `Ok(None)` when `path` does not exist; an unreadable or unparsable file is an error.
    fn read_optional(path: &Path) -> Result<Option<Self>> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => bail!(
                "Config file {} could not be read: {err}. Nothing was changed. Fix the file or move it aside, then retry.",
                path.display()
            ),
        };
        match toml::from_str(&raw) {
            Ok(cfg) => Ok(Some(cfg)),
            Err(err) => bail!(
                "Config file {} could not be parsed: {}\nNothing was changed. Fix the file or move it aside, then retry.",
                path.display(),
                err.to_string().trim_end()
            ),
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if self.load_failed {
            bail!(
                "Config file {} could not be parsed when it was loaded, so it is not overwritten. Fix the file or move it aside, then retry.",
                config_file_path().display()
            );
        }
        ensure_parent(path)?;
        let raw = toml::to_string_pretty(self).context("Failed to serialize config")?;
        write_atomic(path, raw.as_bytes())
    }

    pub fn resolved_download_dir(&self) -> PathBuf {
        if self.download_dir.trim().is_empty() {
            default_download_dir()
        } else {
            expand_tilde(self.download_dir.trim())
        }
    }

    pub fn register_vault(&mut self, profile: VaultProfile, make_active: bool) -> Result<()> {
        let norm_new = normalize_path_key(&profile.path);
        if let Some(conflict) = self.vaults.iter().find(|v| {
            v.name.eq_ignore_ascii_case(&profile.name) && normalize_path_key(&v.path) != norm_new
        }) {
            bail!(
                "Vault name '{}' is already registered for '{}'. Use a different `--name` or remove the existing vault first.",
                conflict.name,
                conflict.path.display()
            );
        }
        if let Some(existing) = self.vaults.iter_mut().find(|v| {
            v.id.eq_ignore_ascii_case(&profile.id) || normalize_path_key(&v.path) == norm_new
        }) {
            *existing = profile.clone();
        } else {
            self.vaults.push(profile.clone());
        }
        if make_active || self.active_vault.trim().is_empty() {
            self.active_vault = profile.name;
        }
        Ok(())
    }

    pub fn upsert_vault(&mut self, profile: VaultProfile, make_active: bool) {
        let norm_new = normalize_path_key(&profile.path);
        if let Some(existing) = self.vaults.iter_mut().find(|v| {
            v.id.eq_ignore_ascii_case(&profile.id)
                || v.name.eq_ignore_ascii_case(&profile.name)
                || normalize_path_key(&v.path) == norm_new
        }) {
            *existing = profile.clone();
        } else {
            self.vaults.push(profile.clone());
        }
        if make_active || self.active_vault.trim().is_empty() {
            self.active_vault = profile.name;
        }
    }

    pub fn remove_vault(&mut self, name_or_id: &str) -> bool {
        let needle = name_or_id.trim();
        if needle.is_empty() {
            return false;
        }
        let needle_path = normalize_path_key(Path::new(needle));
        let active = self.active_vault.trim().to_string();
        let active_path = (!active.is_empty()).then(|| normalize_path_key(Path::new(&active)));

        let mut removed_any = false;
        let mut removed_active = false;
        self.vaults.retain(|v| {
            let v_path = normalize_path_key(&v.path);
            let matched = v.name.eq_ignore_ascii_case(needle)
                || v.id.eq_ignore_ascii_case(needle)
                || v_path == needle_path;
            if matched {
                removed_any = true;
                if !active.is_empty()
                    && (v.name.eq_ignore_ascii_case(&active)
                        || v.id.eq_ignore_ascii_case(&active)
                        || active_path.as_deref() == Some(v_path.as_str()))
                {
                    removed_active = true;
                }
            }
            !matched
        });

        if removed_active || (!active.is_empty() && active.eq_ignore_ascii_case(needle)) {
            self.active_vault.clear();
        }
        removed_any
    }

    pub fn find_vault(&self, query: &str) -> Option<&VaultProfile> {
        let needle = query.trim();
        if needle.is_empty() {
            return None;
        }
        let needle_path = normalize_path_key(Path::new(needle));
        self.vaults.iter().find(|v| {
            v.name.eq_ignore_ascii_case(needle)
                || v.id.eq_ignore_ascii_case(needle)
                || normalize_path_key(&v.path) == needle_path
        })
    }
}

/// Strips the Windows verbatim prefix (`\\?\` or `//?/`) from a path string, in either
/// separator form. The UNC verbatim form (`\\?\UNC\server\share`) maps back to the plain UNC
/// path (`\\server\share`) instead of the relative-looking `UNC\server\share`.
pub fn strip_verbatim_prefix(s: &str) -> Cow<'_, str> {
    let (rest, sep) = if let Some(rest) = s.strip_prefix(r"\\?\") {
        (rest, '\\')
    } else if let Some(rest) = s.strip_prefix("//?/") {
        (rest, '/')
    } else {
        return Cow::Borrowed(s);
    };
    let is_unc = rest
        .get(..3)
        .is_some_and(|head| head.eq_ignore_ascii_case("UNC"))
        && matches!(rest.as_bytes().get(3), Some(b'\\') | Some(b'/'));
    if is_unc {
        return Cow::Owned(format!("{sep}{sep}{}", &rest[4..]));
    }
    Cow::Borrowed(rest)
}

pub fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return canon;
    }
    let mut current = path.to_path_buf();
    let mut tail = Vec::new();
    while let Some(name) = current.file_name() {
        tail.push(name.to_os_string());
        if !current.pop() || current.as_os_str().is_empty() {
            break;
        }
        if let Ok(mut canon) = fs::canonicalize(&current) {
            for comp in tail.into_iter().rev() {
                canon.push(comp);
            }
            return canon;
        }
    }
    path.to_path_buf()
}

pub fn normalize_path_key(path: &Path) -> String {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = resolved.to_string_lossy().replace('\\', "/");
    let stripped = strip_verbatim_prefix(&s);
    if cfg!(windows) {
        stripped.trim_end_matches('/').to_ascii_lowercase()
    } else {
        stripped.trim_end_matches('/').to_string()
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("OBSIDIAN_CONFIG_DIR") {
        if !override_dir.trim().is_empty() {
            return expand_tilde(override_dir.trim());
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

pub fn data_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("OBSIDIAN_DATA_DIR") {
        if !override_dir.trim().is_empty() {
            return expand_tilde(override_dir.trim());
        }
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

pub fn config_file_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn drafts_path() -> PathBuf {
    config_dir().join("drafts.json")
}

pub fn db_path() -> PathBuf {
    data_dir().join("index.db")
}

pub fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("obsidian")
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    Ok(())
}

static ATOMIC_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically writes `data` to `path` using a unique temp file (`create_new(true)`),
/// flushing before rename.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let is_secret = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase().contains("drafts"))
        .unwrap_or(false);
    write_atomic_with_mode(path, data, is_secret)
}

/// Atomically writes `data` to `path` with `0o600` permissions at creation time on Unix.
pub fn write_atomic_0600(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_with_mode(path, data, true)
}

fn write_atomic_with_mode(path: &Path, data: &[u8], mode_0600: bool) -> Result<()> {
    let target_path: Cow<'_, Path> = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if mode_0600 {
                bail!(
                    "Refusing to write sensitive file through symlink: {}",
                    path.display()
                );
            }
            let resolved = fs::canonicalize(path).with_context(|| {
                format!(
                    "Failed to resolve symlink target for atomic write: {}",
                    path.display()
                )
            })?;
            if resolved.is_dir() {
                bail!(
                    "Refusing to overwrite directory through symlink: {} -> {}",
                    path.display(),
                    resolved.display()
                );
            }
            Cow::Owned(resolved)
        }
        Ok(meta) if meta.file_type().is_dir() => {
            bail!("Refusing to overwrite directory: {}", path.display());
        }
        _ => Cow::Borrowed(path),
    };
    let target = target_path.as_ref();
    ensure_parent(target)?;
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let pid = std::process::id();
    let seq = ATOMIC_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.{pid}.{seq}.tmp"));

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if mode_0600 {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = mode_0600;

    let write_res = (|| -> Result<()> {
        let mut file = opts
            .open(&tmp)
            .with_context(|| format!("Failed to create temp file: {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write temp file: {}", tmp.display()))?;
        file.flush()
            .with_context(|| format!("Failed to flush temp file: {}", tmp.display()))?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }

    if let Err(rename_err) = fs::rename(&tmp, target) {
        if let Err(replace_err) =
            replace_existing_with_backup(&tmp, target, parent, name, pid, seq, rename_err)
        {
            let _ = fs::remove_file(&tmp);
            return Err(replace_err);
        }
    }
    Ok(())
}

fn replace_existing_with_backup(
    tmp: &Path,
    path: &Path,
    parent: &Path,
    name: &str,
    pid: u32,
    seq: u64,
    original_err: std::io::Error,
) -> Result<()> {
    if path.symlink_metadata().is_err() {
        return Err(original_err)
            .with_context(|| format!("Failed to save file: {}", path.display()));
    }
    let backup = parent.join(format!(".{name}.{pid}.{seq}.bak"));
    fs::rename(path, &backup)
        .with_context(|| format!("Failed to back up existing file: {}", path.display()))?;
    match fs::rename(tmp, path) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(err) => {
            let _ = fs::rename(&backup, path);
            Err(err).with_context(|| format!("Atomic file save failed: {}", path.display()))
        }
    }
}

/// Persistent draft store backed by `drafts.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DraftStore {
    pub text: HashMap<String, String>,
}

impl DraftStore {
    pub fn load() -> Self {
        Self::load_from(&drafts_path()).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str::<Self>(&raw).unwrap_or_default())
    }

    pub fn save(&self) -> Result<()> {
        #[cfg(test)]
        if std::env::var_os("OBSIDIAN_CONFIG_DIR").is_none() {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        write_atomic_0600(&drafts_path(), &bytes)
    }
}

fn first_non_empty_env(keys: &[&str]) -> Option<String> {
    for &key in keys {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub(crate) fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = input
        .strip_prefix("~/")
        .or_else(|| input.strip_prefix("~\\"))
    {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) struct EnvVarGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
        let mut saved = Vec::with_capacity(vars.len());
        for &(key, val) in vars {
            saved.push((key, std::env::var(key).ok()));
            match val {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        Self { saved }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (key, orig) in self.saved.drain(..) {
            match orig {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_config_vault_registry_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut cfg = AppConfig {
            page_width: PageWidthMode::Full,
            theme: ThemePreset::Catppuccin,
            ..AppConfig::default()
        };
        cfg.upsert_vault(
            VaultProfile {
                id: "v-acme".into(),
                name: "Acme Second Brain".into(),
                path: dir.path().join("vault-a"),
                discovered: false,
                open_in_obsidian: false,
                ts: None,
                daily_folder: Some("Daily Notes".into()),
                daily_format: Some("YYYY-MM-DD".into()),
                attachments_folder: Some("Attachments".into()),
            },
            true,
        );
        cfg.save_to(&path).unwrap();

        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.active_vault, "Acme Second Brain");
        assert_eq!(loaded.vaults.len(), 1);
        assert_eq!(loaded.page_width, PageWidthMode::Full);
        assert_eq!(loaded.theme, ThemePreset::Catppuccin);
        assert_eq!(loaded.find_vault("Acme Second Brain").unwrap().id, "v-acme");
    }

    #[test]
    fn test_theme_preset_and_width_cycles() {
        let mut p = ThemePreset::Auto;
        for _ in 0..9 {
            p = p.next();
        }
        assert_eq!(p, ThemePreset::Auto);
        assert_eq!(PageWidthMode::Centered.toggle(), PageWidthMode::Full);
        assert_eq!(PageWidthMode::Full.toggle(), PageWidthMode::Centered);
    }

    #[tokio::test]
    async fn test_env_overrides() {
        let _lock = TEST_ENV_LOCK.lock().await;
        let _guard = EnvVarGuard::set(&[("OBSIDIAN_VAULT", Some("EnvVault"))]);
        let cfg = AppConfig::default();
        assert_eq!(cfg.active_vault, "EnvVault");
    }

    #[test]
    fn strip_verbatim_prefix_handles_unc_and_drive() {
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\server\share\v"),
            r"\\server\share\v"
        );
        assert_eq!(
            strip_verbatim_prefix(r"\\?\unc\server\share\v"),
            r"\\server\share\v"
        );
        assert_eq!(
            strip_verbatim_prefix("//?/UNC/server/share/v"),
            "//server/share/v"
        );
        assert_eq!(strip_verbatim_prefix(r"\\?\C:\v"), r"C:\v");
        assert_eq!(strip_verbatim_prefix("//?/C:/v"), "C:/v");
        assert_eq!(
            strip_verbatim_prefix(r"\\server\share\v"),
            r"\\server\share\v"
        );
        assert_eq!(
            strip_verbatim_prefix("/home/user/vault"),
            "/home/user/vault"
        );
        assert_eq!(strip_verbatim_prefix(r"\\?\UNCLE\v"), r"UNCLE\v");
        assert_eq!(strip_verbatim_prefix(r"\\?\한글"), "한글");
    }

    const OBS08_BROKEN: &str = "active_vault = \"Kept\"\n\n[[vaults]]\nid = \"v-kept\"\nname = \"Kept\"\npath = '/tmp/kept'\n\nthis line is not toml ===\n";

    #[test]
    fn obs08_load_marks_failed_and_save_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // A missing file is a first run, not a failure.
        let fresh = AppConfig::load_persisted_from(&path).unwrap();
        assert!(fresh.vaults.is_empty());
        assert!(fresh.active_vault.is_empty());
        assert!(!fresh.load_failed);

        fs::write(&path, OBS08_BROKEN).unwrap();
        let err = AppConfig::load_persisted_from(&path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("config.toml"), "{err}");
        assert!(err.contains("could not be parsed"), "{err}");
        assert!(err.contains("Nothing was changed"), "{err}");

        // A config that failed to load refuses to save, to this path or any other.
        let failed = AppConfig {
            load_failed: true,
            ..AppConfig::default()
        };
        assert!(failed.save_to(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), OBS08_BROKEN);
        let other = dir.path().join("other.toml");
        assert!(failed.save_to(&other).is_err());
        assert!(!other.exists());

        // The flag never reaches the file.
        let saved = dir.path().join("saved.toml");
        AppConfig::default().save_to(&saved).unwrap();
        assert!(!fs::read_to_string(&saved).unwrap().contains("load_failed"));
    }

    #[test]
    fn obs08_legacy_keys_still_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "active_vault = \"Old\"\nnotify = false\nauto_daemon = true\ndensity = \"compact\"\ntheme = \"nord\"\nwatch_poll_ms = 900\n",
        )
        .unwrap();

        let loaded = AppConfig::load_persisted_from(&path).unwrap();
        assert_eq!(loaded.active_vault, "Old");
        assert_eq!(loaded.theme, ThemePreset::Nord);
        assert_eq!(loaded.watch_poll_ms, 900);
        assert!(!loaded.load_failed);

        // The removed keys disappear the next time the file is saved.
        loaded.save_to(&path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        for key in ["notify", "auto_daemon", "density"] {
            assert!(!saved.contains(key), "{key} survived in:\n{saved}");
        }
        assert!(saved.contains("theme = \"nord\""));
    }

    #[test]
    fn canonicalize_existing_ancestor_resolves_missing_tail_and_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let existing_sub = dir.path().join("real_sub");
        fs::create_dir_all(&existing_sub).unwrap();

        let missing_leaf = existing_sub.join("future").join("nested").join("config.toml");
        let resolved = canonicalize_existing_ancestor(&missing_leaf);
        let canon_sub = fs::canonicalize(&existing_sub).unwrap();
        assert_eq!(resolved, canon_sub.join("future").join("nested").join("config.toml"));

        let existing_file = existing_sub.join("here.txt");
        fs::write(&existing_file, "ok").unwrap();
        assert_eq!(
            canonicalize_existing_ancestor(&existing_file),
            fs::canonicalize(&existing_file).unwrap()
        );
    }

    #[tokio::test]
    async fn env_config_and_data_dirs_expand_tilde() {
        let _lock = TEST_ENV_LOCK.lock().await;
        let home = dirs::home_dir().expect("home dir available");
        let _guard = EnvVarGuard::set(&[
            ("OBSIDIAN_CONFIG_DIR", Some("~/.custom-ob-cfg")),
            ("OBSIDIAN_DATA_DIR", Some("~/.custom-ob-data")),
        ]);
        assert_eq!(config_dir(), home.join(".custom-ob-cfg"));
        assert_eq!(data_dir(), home.join(".custom-ob-data"));
    }

    #[test]
    fn write_atomic_symlink_hardening_preserves_normal_and_refuses_0600_and_dangling() {
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("dotfiles");
        let link_dir = dir.path().join("config");
        fs::create_dir_all(&target_dir).unwrap();
        fs::create_dir_all(&link_dir).unwrap();

        let real_file = target_dir.join("config.toml");
        fs::write(&real_file, "old = 1\n").unwrap();
        let link_file = link_dir.join("config.toml");

        // Refuses overwriting a directory directly.
        assert!(write_atomic(&target_dir, b"nope").is_err());

        let symlink_ok = {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&real_file, &link_file).is_ok()
            }
            #[cfg(windows)]
            {
                match std::os::windows::fs::symlink_file(&real_file, &link_file) {
                    Ok(()) => true,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::PermissionDenied
                            || e.raw_os_error() == Some(1314) =>
                    {
                        false
                    }
                    Err(e) => panic!("unexpected symlink_file error: {e}"),
                }
            }
        };

        if !symlink_ok {
            return;
        }

        // 1. mode_0600 == false resolves target and preserves the symlink.
        write_atomic(&link_file, b"updated = 2\n").unwrap();
        assert!(link_file.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real_file).unwrap(), "updated = 2\n");

        // 2. mode_0600 == true refuses symlinks and leaves target untouched.
        let err = write_atomic_0600(&link_file, b"secret").unwrap_err();
        assert!(err.to_string().contains("symlink"), "got: {err}");
        assert_eq!(fs::read_to_string(&real_file).unwrap(), "updated = 2\n");

        // 3. Dangling symlink fails cleanly.
        let dangling = link_dir.join("dangling.toml");
        #[cfg(unix)]
        std::os::unix::fs::symlink(target_dir.join("does-not-exist.toml"), &dangling).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(target_dir.join("does-not-exist.toml"), &dangling)
            .unwrap();
        assert!(write_atomic(&dangling, b"nope").is_err());

        // 4. Symlink to a directory fails cleanly.
        let dir_link = link_dir.join("dir_link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target_dir, &dir_link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target_dir, &dir_link).unwrap();
        assert!(write_atomic(&dir_link, b"nope").is_err());
    }
}
