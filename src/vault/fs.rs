//! Direct Obsidian Markdown filesystem engine:
//! - Recursive `.md` walker (skipping `.obsidian/`, `.git/`, `.trash/`, `node_modules/`)
//! - Case-insensitive shortest-path wikilink & alias resolution (matching Obsidian behavior)
//! - Attachment & image path resolution
//! - Safe atomic writes (`write_atomic`), surgical line-preserving checkbox toggles, `.trash/` deletes
//! - Daily Notes resolution & creation (reading `.obsidian/daily-notes.json` when present).

use std::{
    collections::HashMap,
    fs,
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{bail, Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use sha2::{Digest, Sha256};

use crate::{
    config::{ensure_parent, strip_verbatim_prefix, write_atomic, VaultProfile},
    models::{CodedError, InlineSpan, NoteBlock, NoteImageRef, ParsedNote},
    vault::parser::{
        find_inline_code_end, find_matching_bracket, is_code_fence_opener, is_image_target,
        mime_type_for_path, parse_inline_markdown_link_dest, parse_single_task_line,
        upsert_frontmatter_in_markdown,
    },
};

const IGNORED_DIRS: &[&str] = &[".obsidian", ".git", ".trash", "node_modules"];

/// Returns `true` when a directory segment is excluded by Obsidian vault traversal rules.
pub fn is_ignored_dir_name(name: &str) -> bool {
    IGNORED_DIRS.iter().any(|&ig| name.eq_ignore_ascii_case(ig)) || name.starts_with('.')
}

/// Returns `true` when `path` ends with `.md` (case-insensitive).
pub fn is_markdown_file_path(path: &str) -> bool {
    let fname = path.rsplit(['/', '\\']).next().unwrap_or(path);
    fname.len() > 3 && fname.to_ascii_lowercase().ends_with(".md")
}

/// Strips Windows verbatim prefix (`\\?\`) and normalizes path separators for root prefix checks.
fn clean_fs_path_components(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let stripped = strip_verbatim_prefix(&raw);
    stripped.trim_end_matches('/').to_string()
}

/// Converts an absolute filesystem `path` inside `vault_root` (or `canonical_root`) into a
/// normalized POSIX relative path, returning `None` if outside the vault or inside an ignored/dot directory.
pub fn rel_path_in_vault(
    vault_root: &Path,
    canonical_root: Option<&Path>,
    path: &Path,
) -> Option<String> {
    let rel_raw = path
        .strip_prefix(vault_root)
        .ok()
        .or_else(|| canonical_root.and_then(|c| path.strip_prefix(c).ok()))
        .map(normalize_rel_path)
        .or_else(|| {
            let p_str = clean_fs_path_components(path);
            let roots = [Some(vault_root), canonical_root];
            for root in roots.into_iter().flatten() {
                let r_str = clean_fs_path_components(root);
                if r_str.is_empty() {
                    continue;
                }
                let matched_rest = if cfg!(windows) {
                    let p_low = p_str.to_ascii_lowercase();
                    let r_low = r_str.to_ascii_lowercase();
                    if p_low.starts_with(&r_low) && p_str.as_bytes().get(r_str.len()) == Some(&b'/')
                    {
                        Some(&p_str[r_str.len() + 1..])
                    } else {
                        None
                    }
                } else if p_str.starts_with(&r_str)
                    && p_str.as_bytes().get(r_str.len()) == Some(&b'/')
                {
                    Some(&p_str[r_str.len() + 1..])
                } else {
                    None
                };
                if let Some(rest) = matched_rest {
                    return Some(rest.trim_start_matches('/').to_string());
                }
            }
            None
        })?;

    if rel_raw.is_empty() {
        return None;
    }

    let mut segments = rel_raw.split('/').peekable();
    while let Some(seg) = segments.next() {
        if seg.is_empty() || seg == "." || seg == ".." {
            return None;
        }
        let is_last = segments.peek().is_none();
        if !is_last && is_ignored_dir_name(seg) {
            return None;
        }
    }
    Some(rel_raw)
}

/// Returns `true` if any path component of `rel_path` inside `vault_root` is a symlink.
pub fn has_symlink_component_in_vault(vault_root: &Path, rel_path: &str) -> bool {
    let mut cur = vault_root.to_path_buf();
    for seg in rel_path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        cur.push(seg);
        if let Ok(meta) = fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                return true;
            }
        }
    }
    false
}

/// Resolves the canonical on-disk relative path inside `vault_root` when the target exists.
/// Useful on case-insensitive filesystems (Windows/macOS) to detect case-only renames.
pub fn canonical_rel_path_in_vault(
    vault_root: &Path,
    canonical_root: Option<&Path>,
    abs_path: &Path,
) -> Option<String> {
    let canon = fs::canonicalize(abs_path).ok()?;
    rel_path_in_vault(vault_root, canonical_root, &canon)
}

/// Computes raw SHA-256 digest bytes for content comparison.
pub fn content_sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Computes lowercase hex SHA-256 digest for content comparison.
pub fn content_sha256_hex(bytes: &[u8]) -> String {
    hex::encode(content_sha256_bytes(bytes))
}

/// Metadata of a discovered `.md` note on disk.
#[derive(Debug, Clone)]
pub struct DiskNoteEntry {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub title: String,
    pub folder: String,
    pub mtime_ms: i64,
    pub mtime_ns: i64,
    pub size_bytes: i64,
}

/// Metadata of a discovered `.html`/`.htm`/`.xhtml` file inside a vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultHtmlFile {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub title: String,
    pub folder: String,
    pub mtime_ns: i64,
    pub size_bytes: i64,
}

/// Returns `true` when `path` has an `.html`, `.htm`, or `.xhtml` extension (ignoring `#` / `?` suffixes).
pub fn is_html_file_path(path: &str) -> bool {
    let base = path
        .split(['#', '?'])
        .next()
        .unwrap_or(path)
        .trim()
        .trim_matches(['<', '>']);
    let Some((_, ext)) = base.rsplit_once('.') else {
        return false;
    };
    matches!(ext.to_ascii_lowercase().as_str(), "html" | "htm" | "xhtml")
}

/// Walks a vault directory recursively for `.html`/`.htm`/`.xhtml` files, skipping hidden/system directories.
pub fn scan_vault_html_files(vault_root: &Path) -> Result<Vec<VaultHtmlFile>> {
    if !vault_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    walk_html_dir_recursive(vault_root, vault_root, &mut out)?;
    out.sort_by(|a, b| {
        a.rel_path
            .to_ascii_lowercase()
            .cmp(&b.rel_path.to_ascii_lowercase())
    });
    Ok(out)
}

/// Walks a single subdirectory inside `vault_root` recursively for `.html`/`.htm`/`.xhtml` files.
pub fn scan_vault_html_subdir(vault_root: &Path, subdir: &Path) -> Result<Vec<VaultHtmlFile>> {
    let is_real_dir = fs::symlink_metadata(subdir)
        .map(|m| m.file_type().is_dir() && !m.file_type().is_symlink())
        .unwrap_or(false);
    if !is_real_dir {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    walk_html_dir_recursive(vault_root, subdir, &mut out)?;
    out.sort_by(|a, b| {
        a.rel_path
            .to_ascii_lowercase()
            .cmp(&b.rel_path.to_ascii_lowercase())
    });
    Ok(out)
}

fn walk_html_dir_recursive(root: &Path, dir: &Path, out: &mut Vec<VaultHtmlFile>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if is_ignored_dir_name(&name) {
                continue;
            }
            walk_html_dir_recursive(root, &path, out)?;
        } else if ft.is_file() && is_html_file_path(&name) {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel_path = normalize_rel_path(rel);
            let title = name.to_string();
            let folder = folder_from_rel_path(&rel_path);
            let (mtime_ns, size_bytes) = file_mtime_ns_and_size(&path);
            out.push(VaultHtmlFile {
                rel_path,
                abs_path: path,
                title,
                folder,
                mtime_ns,
                size_bytes,
            });
        }
    }
    Ok(())
}

/// Result of a full vault scan: the Markdown files found, plus the directories whose listing
/// failed and whose notes may therefore still exist on disk.
#[derive(Default)]
pub struct VaultScan {
    pub entries: Vec<DiskNoteEntry>,
    /// Vault-relative directories (`""` is the vault root) that could not be read completely.
    pub unreadable_dirs: Vec<String>,
}

/// Walks a vault directory recursively for `.md` files, skipping hidden/system directories.
pub fn scan_vault_markdown_files(vault_root: &Path) -> Result<Vec<DiskNoteEntry>> {
    Ok(scan_vault_markdown_files_report(vault_root)?.entries)
}

/// Like [`scan_vault_markdown_files`], and also reports the directories that could not be read
/// so the caller does not mistake their notes for deleted ones.
pub fn scan_vault_markdown_files_report(vault_root: &Path) -> Result<VaultScan> {
    if !vault_root.is_dir() {
        bail!("Vault directory does not exist: {}", vault_root.display());
    }
    let mut scan = VaultScan::default();
    walk_dir_recursive(
        vault_root,
        vault_root,
        &mut scan.entries,
        &mut scan.unreadable_dirs,
    )?;
    scan.entries.sort_by(|a, b| {
        a.rel_path
            .to_ascii_lowercase()
            .cmp(&b.rel_path.to_ascii_lowercase())
    });
    scan.unreadable_dirs.sort();
    scan.unreadable_dirs.dedup();
    Ok(scan)
}

/// Walks a single subdirectory inside `vault_root` recursively for `.md` files.
pub fn scan_vault_markdown_subdir(vault_root: &Path, subdir: &Path) -> Result<Vec<DiskNoteEntry>> {
    let is_real_dir = fs::symlink_metadata(subdir)
        .map(|m| m.file_type().is_dir() && !m.file_type().is_symlink())
        .unwrap_or(false);
    if !is_real_dir {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    walk_dir_recursive(vault_root, subdir, &mut out, &mut Vec::new())?;
    out.sort_by(|a, b| {
        a.rel_path
            .to_ascii_lowercase()
            .cmp(&b.rel_path.to_ascii_lowercase())
    });
    Ok(out)
}

fn walk_dir_recursive(
    root: &Path,
    dir: &Path,
    out: &mut Vec<DiskNoteEntry>,
    unreadable: &mut Vec<String>,
) -> Result<()> {
    let rel_dir = || {
        dir.strip_prefix(root)
            .map(normalize_rel_path)
            .unwrap_or_default()
    };
    let entries = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) => {
            // A directory that vanished mid-scan is gone; any other failure (permissions,
            // I/O) means the notes below it may still exist.
            if err.kind() != std::io::ErrorKind::NotFound {
                unreadable.push(rel_dir());
            }
            return Ok(());
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            unreadable.push(rel_dir());
            continue;
        };
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let Ok(ft) = entry.file_type() else {
            unreadable.push(rel_dir());
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if is_ignored_dir_name(&name) {
                continue;
            }
            walk_dir_recursive(root, &path, out, unreadable)?;
        } else if ft.is_file() {
            if !is_markdown_file_path(&name) {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel_path = normalize_rel_path(rel);
            let title = title_from_rel_path(&rel_path);
            let folder = folder_from_rel_path(&rel_path);
            let (mtime_ns, size_bytes) = file_mtime_ns_and_size(&path);
            let mtime_ms = mtime_ns / 1_000_000;
            out.push(DiskNoteEntry {
                rel_path,
                abs_path: path,
                title,
                folder,
                mtime_ms,
                mtime_ns,
                size_bytes,
            });
        }
    }
    Ok(())
}

/// Reads `(mtime_ns, size_bytes)` from a file's filesystem metadata with nanosecond precision.
pub fn file_mtime_ns_and_size(path: &Path) -> (i64, i64) {
    let meta = fs::metadata(path).ok();
    let size_bytes = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);
    let mtime_ns = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    (mtime_ns, size_bytes)
}

/// Reads `(mtime_ms, size_bytes)` from a file's filesystem metadata using the exact
/// same clock conversion as `scan_vault_markdown_files`.
pub fn file_mtime_and_size(path: &Path) -> (i64, i64) {
    let (mtime_ns, size_bytes) = file_mtime_ns_and_size(path);
    (mtime_ns / 1_000_000, size_bytes)
}

/// Builds a disk-versioned cache key for inline halfblock image previews.
pub fn inline_image_cache_key(abs_path: &Path) -> String {
    let (mtime_ns, size_bytes) = file_mtime_ns_and_size(abs_path);
    format!(
        "inline:{}:{}:{}",
        abs_path.to_string_lossy(),
        mtime_ns,
        size_bytes
    )
}

pub fn normalize_rel_path(rel: &Path) -> String {
    rel.to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

pub fn title_from_rel_path(rel_path: &str) -> String {
    let fname = rel_path.rsplit('/').next().unwrap_or(rel_path);
    fname
        .strip_suffix(".md")
        .or_else(|| fname.strip_suffix(".MD"))
        .unwrap_or(fname)
        .to_string()
}

pub fn folder_from_rel_path(rel_path: &str) -> String {
    match rel_path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

/// Validates and joins a relative note path safely inside `vault_root` (preventing path traversal).
pub fn safe_vault_path(vault_root: &Path, rel_path: &str) -> Result<PathBuf> {
    let cleaned = rel_path.trim().replace('\\', "/");
    let cleaned = cleaned.trim_start_matches('/');
    if cleaned.is_empty() {
        bail!("Note path cannot be empty");
    }
    let mut joined = vault_root.to_path_buf();
    for comp in cleaned.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            bail!("Path traversal ('..') is not allowed: {rel_path}");
        }
        // On Windows `PathBuf::push("C:foo")` replaces the whole base, and `name:stream` addresses
        // an NTFS alternate data stream. On Unix `:` is an ordinary filename character.
        if cfg!(windows) && comp.contains(':') {
            return Err(CodedError::new(
                "invalid_argument",
                format!(
                    "Path component '{comp}' is not allowed (drive prefix or stream separator): {rel_path}"
                ),
            )
            .into());
        }
        // Structural check: the component must be exactly one plain name.
        let mut parts = Path::new(comp).components();
        if !matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(_)), None)
        ) {
            return Err(CodedError::new(
                "invalid_argument",
                format!("Path component '{comp}' is not allowed: {rel_path}"),
            )
            .into());
        }
        joined.push(comp);
    }
    if !joined.starts_with(vault_root) {
        return Err(CodedError::new(
            "invalid_argument",
            format!("Path escapes the vault root: {rel_path}"),
        )
        .into());
    }
    Ok(joined)
}

/// Obsidian-style case-insensitive shortest-path wikilink resolver (also checks frontmatter aliases).
pub fn resolve_wikilink_target(
    target_raw: &str,
    all_rel_paths: &[String],
    aliases_by_path: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let raw = wikilink_note_target(target_raw);
    let raw = raw.trim_start_matches('/');
    if raw.is_empty() || is_image_target(raw) {
        return None;
    }

    let raw_lower = raw.to_ascii_lowercase();
    let raw_with_md = if raw_lower.ends_with(".md") {
        raw_lower.clone()
    } else {
        format!("{raw_lower}.md")
    };

    // 1. Exact relative path match (case-insensitive)
    if let Some(exact) = all_rel_paths
        .iter()
        .find(|p| p.to_ascii_lowercase() == raw_with_md || p.to_ascii_lowercase() == raw_lower)
    {
        return Some(exact.clone());
    }

    // 2. Suffix path match (e.g. `Projects/RFC` -> `Work/Projects/RFC.md`), shortest path wins
    let suffix_needle = format!("/{raw_with_md}");
    let mut suffix_matches: Vec<&String> = all_rel_paths
        .iter()
        .filter(|p| p.to_ascii_lowercase().ends_with(&suffix_needle))
        .collect();
    if !suffix_matches.is_empty() {
        suffix_matches.sort_by_key(|p| (p.matches('/').count(), p.len()));
        return Some(suffix_matches[0].clone());
    }

    // 3. Stem / title match (shortest path wins)
    let stem_needle = raw_lower.strip_suffix(".md").unwrap_or(&raw_lower);
    let mut stem_matches: Vec<&String> = all_rel_paths
        .iter()
        .filter(|p| title_from_rel_path(p).to_ascii_lowercase() == stem_needle)
        .collect();
    if !stem_matches.is_empty() {
        stem_matches.sort_by_key(|p| (p.matches('/').count(), p.len()));
        return Some(stem_matches[0].clone());
    }

    // 4. Frontmatter alias match
    for (rel_path, aliases) in aliases_by_path {
        if aliases
            .iter()
            .any(|a| a.trim().eq_ignore_ascii_case(stem_needle))
        {
            return Some(rel_path.clone());
        }
    }

    None
}

fn wikilink_note_target(raw: &str) -> String {
    raw.split('#')
        .next()
        .unwrap_or(raw)
        .trim()
        .replace('\\', "/")
}

/// Matches of the stage that decides a wikilink target, sorted. Exactly one entry means resolved.
pub(crate) fn wikilink_candidates_from_source(
    target_raw: &str,
    source_rel: &str,
    paths: &[String],
    aliases: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let raw = wikilink_note_target(target_raw);
    if raw.is_empty() || is_image_target(&raw) {
        return Vec::new();
    }
    let target = raw.to_ascii_lowercase();
    let target = if target.ends_with(".md") {
        target
    } else {
        format!("{target}.md")
    };
    let collect_sorted = |iter: &mut dyn Iterator<Item = &String>| -> Vec<String> {
        let mut found: Vec<String> = iter.cloned().collect();
        found.sort();
        found.dedup();
        found
    };
    let exact_all = |path: &str| -> Vec<String> {
        let mut found: Vec<String> = paths
            .iter()
            .filter(|p| p.eq_ignore_ascii_case(path))
            .cloned()
            .collect();
        found.sort();
        found.dedup();
        found
    };
    let source_folder = source_rel.rsplit_once('/').map_or("", |(folder, _)| folder);

    if let Some(rooted) = target.strip_prefix('/') {
        let Some(normalized) = normalize_vault_rel_path("", rooted) else {
            return Vec::new();
        };
        return exact_all(&normalized);
    }
    if target.starts_with("./") || target.starts_with("../") {
        let mut components: Vec<_> = source_folder.split('/').filter(|s| !s.is_empty()).collect();
        for component in target.split('/') {
            match component {
                "." | "" => {}
                ".." => {
                    if components.pop().is_none() {
                        return Vec::new();
                    }
                }
                _ => components.push(component),
            }
        }
        return exact_all(&components.join("/"));
    }
    if target.contains('/') {
        let exact = exact_all(&target);
        if exact.len() == 1 {
            return exact;
        }
        let suffix = format!("/{target}");
        let suffix_matches = collect_sorted(
            &mut paths
                .iter()
                .filter(|p| p.to_ascii_lowercase().ends_with(&suffix)),
        );
        if !suffix_matches.is_empty() {
            return suffix_matches;
        }
        return exact;
    }

    let local = if source_folder.is_empty() {
        target.clone()
    } else {
        format!("{source_folder}/{target}")
    };
    let local_exact = exact_all(&local);
    if local_exact.len() == 1 {
        return local_exact;
    }

    let by_filename = collect_sorted(&mut paths.iter().filter(|p| {
        p.rsplit('/')
            .next()
            .is_some_and(|filename| filename.eq_ignore_ascii_case(&target))
    }));
    if !by_filename.is_empty() {
        return by_filename;
    }

    let stem = target.strip_suffix(".md").unwrap_or(&target);
    collect_sorted(&mut aliases.iter().filter_map(|(path, names)| {
        names
            .iter()
            .any(|name| name.trim().eq_ignore_ascii_case(stem))
            .then_some(path)
    }))
}

/// Resolves a wikilink target relative to `source_rel`.
///
/// Rules:
/// - `/...` resolves from the vault root
/// - `./...` and `../...` resolve relative to `source_rel`'s folder
/// - paths with `/` match exact vault relative path, then a unique suffix match
/// - bare names check same-folder exact filename -> unique filename across vault -> unique alias
///
/// Ambiguous multi-candidate matches return `None` (unresolved).
pub fn resolve_wikilink_from_source(
    target_raw: &str,
    source_rel: &str,
    paths: &[String],
    aliases: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let mut found = wikilink_candidates_from_source(target_raw, source_rel, paths, aliases);
    (found.len() == 1).then(|| found.remove(0))
}

fn resolve_move_wikilink_target(
    target_raw: &str,
    source_rel: &str,
    paths: &[String],
    aliases: &HashMap<String, Vec<String>>,
) -> Option<String> {
    resolve_wikilink_from_source(target_raw, source_rel, paths, aliases)
}

fn normalize_vault_rel_path(base_folder: &str, raw_path: &str) -> Option<String> {
    let mut components: Vec<&str> = base_folder.split('/').filter(|s| !s.is_empty()).collect();
    for comp in raw_path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            _ => components.push(comp),
        }
    }
    (!components.is_empty()).then(|| components.join("/"))
}

fn has_uri_scheme(raw: &str) -> bool {
    let Some((scheme, _)) = raw.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn decode_markdown_link_path(path_raw: &str) -> Option<String> {
    let mut unescaped = String::with_capacity(path_raw.len());
    let mut chars = path_raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(&next) = chars.peek() {
                if next.is_ascii_punctuation() {
                    unescaped.push(chars.next().unwrap());
                    continue;
                }
            }
        }
        unescaped.push(ch);
    }
    urlencoding::decode(&unescaped)
        .ok()
        .map(std::borrow::Cow::into_owned)
}

/// Splits a Markdown link destination into `(raw_path, optional_heading)` when it targets a
/// local Markdown note rather than an external URL, same-note anchor, image, HTML, or other binary file.
pub fn extract_local_markdown_note_link(url: &str) -> Option<(String, Option<String>)> {
    let trimmed = url.trim().trim_matches(['<', '>']);
    let (raw_path, heading) = match trimmed.split_once('#') {
        Some((p, h)) => {
            let h_clean = h.trim();
            (p.trim(), (!h_clean.is_empty()).then(|| h_clean.to_string()))
        }
        None => (trimmed, None),
    };
    if raw_path.is_empty() || raw_path.starts_with("//") || has_uri_scheme(raw_path) {
        return None;
    }
    let decoded = decode_markdown_link_path(raw_path)?;
    let decoded_norm = decoded.replace('\\', "/");
    if decoded_norm.is_empty()
        || decoded_norm.starts_with("//")
        || has_uri_scheme(&decoded_norm)
        || is_image_target(&decoded_norm)
        || is_html_file_path(&decoded_norm)
    {
        return None;
    }
    let fname = decoded_norm.rsplit('/').next().unwrap_or(&decoded_norm);
    if fname.is_empty() || fname == "." || fname == ".." {
        return None;
    }
    let lower = fname.to_ascii_lowercase();
    if !lower.ends_with(".md") && fname.contains('.') {
        return None;
    }
    Some((raw_path.to_string(), heading))
}

/// Resolves an inline Markdown link destination path from `source_rel` against known vault note paths.
pub fn resolve_markdown_link_from_source(
    path_raw: &str,
    source_rel: &str,
    paths: &[String],
) -> Option<String> {
    resolve_move_markdown_target(path_raw, source_rel, paths)
}

/// Resolves an inline Markdown link destination path from `source_rel` against known vault note paths.
fn resolve_move_markdown_target(
    path_raw: &str,
    source_rel: &str,
    paths: &[String],
) -> Option<String> {
    resolve_move_markdown_target_staged(path_raw, source_rel, paths).map(|(path, _)| path)
}

fn resolve_move_markdown_target_staged(
    path_raw: &str,
    source_rel: &str,
    paths: &[String],
) -> Option<(String, PathBase)> {
    fn unique<'a>(mut matches: impl Iterator<Item = &'a String>) -> Option<String> {
        let first = matches.next()?;
        matches.next().is_none().then(|| first.clone())
    }

    if path_raw.is_empty() || path_raw.starts_with("//") || has_uri_scheme(path_raw) {
        return None;
    }

    let decoded = decode_markdown_link_path(path_raw)?;
    if decoded.is_empty()
        || decoded.starts_with("//")
        || has_uri_scheme(&decoded)
        || is_image_target(&decoded)
    {
        return None;
    }

    let match_path = |candidate: &str| -> Option<String> {
        let with_md = if candidate.to_ascii_lowercase().ends_with(".md") {
            candidate.to_owned()
        } else {
            format!("{candidate}.md")
        };
        unique(paths.iter().filter(|p| p.eq_ignore_ascii_case(&with_md)))
    };

    let source_folder = folder_from_rel_path(source_rel);

    if let Some(rooted) = decoded.strip_prefix('/') {
        let normalized = normalize_vault_rel_path("", rooted)?;
        return match_path(&normalized).map(|p| (p, PathBase::Rooted));
    }
    if decoded.starts_with("./") || decoded.starts_with("../") {
        let normalized = normalize_vault_rel_path(&source_folder, &decoded)?;
        return match_path(&normalized).map(|p| (p, PathBase::Explicit));
    }

    if let Some(normalized) = normalize_vault_rel_path(&source_folder, &decoded) {
        if let Some(found) = match_path(&normalized) {
            return Some((found, PathBase::SourceFolder));
        }
    }
    if !source_folder.is_empty() {
        if let Some(normalized) = normalize_vault_rel_path("", &decoded) {
            return match_path(&normalized).map(|p| (p, PathBase::VaultRoot));
        }
    }
    None
}

/// True for a local destination whose last segment has an extension other than .md.
pub(crate) fn is_local_attachment_dest(decoded: &str) -> bool {
    let trimmed = decoded.trim().trim_matches(['<', '>']);
    let path_part = trimmed
        .split('#')
        .next()
        .unwrap_or(trimmed)
        .replace('\\', "/");
    if path_part.is_empty() || path_part.starts_with("//") || has_uri_scheme(&path_part) {
        return false;
    }
    let last = path_part.rsplit('/').next().unwrap_or(&path_part);
    if last.is_empty() || last == "." || last == ".." {
        return false;
    }
    let Some((_, ext)) = last.rsplit_once('.') else {
        return false;
    };
    !ext.is_empty() && !ext.eq_ignore_ascii_case("md")
}

fn same_file(a: &Path, b: &Path) -> bool {
    fs::canonicalize(a)
        .ok()
        .zip(fs::canonicalize(b).ok())
        .is_some_and(|(ca, cb)| ca == cb)
}

fn relative_markdown_path(source_rel: &str, target_rel: &str) -> String {
    let source_folder = folder_from_rel_path(source_rel);
    let source_parts: Vec<&str> = source_folder.split('/').filter(|s| !s.is_empty()).collect();
    let target_parts: Vec<&str> = target_rel.split('/').filter(|s| !s.is_empty()).collect();

    let common_len = source_parts
        .iter()
        .zip(
            target_parts
                .iter()
                .take(target_parts.len().saturating_sub(1)),
        )
        .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
        .count();

    let mut rel_parts: Vec<&str> = Vec::with_capacity(
        source_parts.len().saturating_sub(common_len)
            + target_parts.len().saturating_sub(common_len),
    );
    rel_parts.extend(std::iter::repeat_n(
        "..",
        source_parts.len().saturating_sub(common_len),
    ));
    for part in &target_parts[common_len..] {
        rel_parts.push(part);
    }
    rel_parts.join("/")
}

fn encode_markdown_link_path(rel_path: &str, angle_bracketed: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(rel_path.len());
    for ch in rel_path.chars() {
        if ch.is_ascii() {
            let byte = ch as u8;
            let needs_encode = if angle_bracketed {
                byte < 0x20
                    || byte == 0x7F
                    || matches!(byte, b'%' | b'#' | b'?' | b'<' | b'>' | b'\\')
            } else {
                byte <= 0x20
                    || byte == 0x7F
                    || matches!(
                        byte,
                        b'%' | b'#' | b'?' | b'(' | b')' | b'<' | b'>' | b'"' | b'\\' | b'[' | b']'
                    )
            };
            if needs_encode {
                let _ = write!(out, "%{byte:02X}");
            } else {
                out.push(ch);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// How a local link destination was matched. SourceFolder and Explicit depend on the
/// referring note's folder; the others do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathBase {
    Rooted,
    Explicit,
    SourceFolder,
    VaultRoot,
    AttachmentDir,
    NameSearch,
}

impl PathBase {
    fn is_file_relative(self) -> bool {
        matches!(self, Self::Explicit | Self::SourceFolder)
    }
}

/// Resolves a local attachment destination and reports the stage that matched.
pub(crate) fn resolve_attachment_staged(
    vault_root: &Path,
    note_rel_path: &str,
    raw_target: &str,
) -> Option<(PathBuf, PathBase)> {
    let trimmed = raw_target.trim().trim_matches(['<', '>']);
    if let Some(found) = resolve_attachment_staged_once(vault_root, note_rel_path, trimmed) {
        return Some(found);
    }
    let decoded = decode_markdown_link_path(trimmed)?;
    if decoded != trimmed {
        return resolve_attachment_staged_once(vault_root, note_rel_path, &decoded);
    }
    None
}

fn resolve_attachment_staged_once(
    vault_root: &Path,
    note_rel_path: &str,
    candidate: &str,
) -> Option<(PathBuf, PathBase)> {
    let norm = candidate.replace('\\', "/");
    if norm.is_empty() || norm.starts_with("//") || has_uri_scheme(&norm) {
        return None;
    }

    let rooted = norm.starts_with('/');
    let t = norm.trim_start_matches('/');
    if t.is_empty() {
        return None;
    }

    let has_parent = t.split('/').any(|seg| seg == "..");
    let explicit = t.starts_with("./") || t.starts_with("../") || has_parent;

    if !rooted {
        let note_folder = folder_from_rel_path(note_rel_path);
        if let Some(rel) = normalize_vault_rel_path(&note_folder, t) {
            let cand = vault_root.join(&rel);
            if cand.is_file() {
                let stage = if explicit {
                    PathBase::Explicit
                } else {
                    PathBase::SourceFolder
                };
                return Some((cand, stage));
            }
        }
    }

    if has_parent {
        if rooted {
            if let Some(rel) = normalize_vault_rel_path("", t) {
                let cand = vault_root.join(&rel);
                if cand.is_file() {
                    return Some((cand, PathBase::Rooted));
                }
            }
        }
        return None;
    }

    let c = t.trim_start_matches("./");
    if c.is_empty() {
        return None;
    }
    let root_cand = vault_root.join(c);
    if root_cand.is_file() {
        let stage = if rooted {
            PathBase::Rooted
        } else {
            PathBase::VaultRoot
        };
        return Some((root_cand, stage));
    }

    for sub in [
        "Attachments",
        "attachments",
        "assets",
        "Assets",
        "media",
        "images",
    ] {
        let cand = vault_root.join(sub).join(c);
        if cand.is_file() {
            return Some((cand, PathBase::AttachmentDir));
        }
    }

    let target_fname = c.rsplit('/').next().unwrap_or(c);
    find_attachment_by_name(vault_root, target_fname).map(|p| (p, PathBase::NameSearch))
}

/// Resolves an embedded image/attachment target relative to the note folder, vault root, or common attachment dirs.
pub fn resolve_attachment_path(
    vault_root: &Path,
    note_rel_path: &str,
    raw_target: &str,
) -> Option<PathBuf> {
    resolve_attachment_staged(vault_root, note_rel_path, raw_target).map(|(path, _)| path)
}

fn find_attachment_by_name(dir: &Path, filename: &str) -> Option<PathBuf> {
    let rd = fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if ft.is_dir() {
            if IGNORED_DIRS
                .iter()
                .any(|&ig| name_str.eq_ignore_ascii_case(ig))
                || name_str.starts_with('.')
            {
                continue;
            }
            if let Some(found) = find_attachment_by_name(&entry.path(), filename) {
                return Some(found);
            }
        } else if ft.is_file() && name_str.eq_ignore_ascii_case(filename) {
            return Some(entry.path());
        }
    }
    None
}

/// Populates `resolved_path` on inline wikilinks & image embeds inside a `ParsedNote`.
pub fn hydrate_parsed_note_links(
    vault_root: &Path,
    note_rel_path: &str,
    parsed: &mut ParsedNote,
    all_rel_paths: &[String],
    aliases_by_path: &HashMap<String, Vec<String>>,
) -> Vec<NoteImageRef> {
    let mut images = Vec::new();
    hydrate_blocks(
        vault_root,
        note_rel_path,
        &mut parsed.blocks,
        all_rel_paths,
        aliases_by_path,
        &mut images,
    );
    images
}

fn hydrate_blocks(
    vault_root: &Path,
    note_rel_path: &str,
    blocks: &mut [NoteBlock],
    all_rel_paths: &[String],
    aliases_by_path: &HashMap<String, Vec<String>>,
    images: &mut Vec<NoteImageRef>,
) {
    for block in blocks {
        match block {
            NoteBlock::Heading { spans, .. }
            | NoteBlock::Paragraph { spans, .. }
            | NoteBlock::Task { spans, .. }
            | NoteBlock::BulletItem { spans, .. }
            | NoteBlock::NumberedItem { spans, .. }
            | NoteBlock::FootnoteDef { spans, .. } => {
                resolve_spans(spans, note_rel_path, all_rel_paths, aliases_by_path);
            }
            NoteBlock::Callout { body_blocks, .. } => {
                hydrate_blocks(
                    vault_root,
                    note_rel_path,
                    body_blocks,
                    all_rel_paths,
                    aliases_by_path,
                    images,
                );
            }
            NoteBlock::Blockquote { blocks, .. } => {
                hydrate_blocks(
                    vault_root,
                    note_rel_path,
                    blocks,
                    all_rel_paths,
                    aliases_by_path,
                    images,
                );
            }
            NoteBlock::Table { headers, rows, .. } => {
                for cell in headers {
                    resolve_spans(cell, note_rel_path, all_rel_paths, aliases_by_path);
                }
                for row in rows {
                    for cell in row {
                        resolve_spans(cell, note_rel_path, all_rel_paths, aliases_by_path);
                    }
                }
            }
            NoteBlock::NoteEmbed {
                target,
                resolved_path,
                ..
            } => {
                *resolved_path = resolve_wikilink_from_source(
                    target,
                    note_rel_path,
                    all_rel_paths,
                    aliases_by_path,
                );
            }
            NoteBlock::ImageEmbed {
                raw_target,
                alt,
                resolved_path,
                line_no,
                ..
            } if is_image_target(raw_target) => {
                if let Some(abs) = resolve_attachment_path(vault_root, note_rel_path, raw_target) {
                    *resolved_path = Some(abs.clone());
                    images.push(NoteImageRef {
                        raw_target: raw_target.clone(),
                        display_name: alt.clone(),
                        mime_type: mime_type_for_path(raw_target).to_string(),
                        abs_path: abs,
                        line_no: *line_no,
                    });
                }
            }
            _ => {}
        }
    }
}

fn resolve_spans(
    spans: &mut [InlineSpan],
    source_rel: &str,
    all_rel_paths: &[String],
    aliases_by_path: &HashMap<String, Vec<String>>,
) {
    for span in spans {
        if let InlineSpan::Wikilink {
            target,
            resolved_path,
            ..
        } = span
        {
            *resolved_path =
                resolve_wikilink_from_source(target, source_rel, all_rel_paths, aliases_by_path);
        }
    }
}

/// Surgically toggles a task checkbox (`- [ ]` <-> `- [x]`) at the exact 1-based `line_no`.
/// Preserves existing line endings and writes atomically. Returns the new `checked` boolean.
pub fn toggle_task_at_line(abs_path: &Path, line_no: usize) -> Result<bool> {
    toggle_task_at_line_verified(abs_path, line_no, None, None)
}

/// Surgically toggles a task checkbox at `line_no` after verifying the current line on disk
/// matches optional `expected_text` and/or `expected_checked`. Refuses to mutate the file
/// when external edits shifted or altered the target line.
pub fn toggle_task_at_line_verified(
    abs_path: &Path,
    line_no: usize,
    expected_text: Option<&str>,
    expected_checked: Option<bool>,
) -> Result<bool> {
    if line_no == 0 {
        bail!("line_no must be >= 1");
    }
    let raw_bytes = fs::read(abs_path)
        .with_context(|| format!("Failed to read note: {}", abs_path.display()))?;
    let raw = String::from_utf8(raw_bytes)
        .with_context(|| format!("Failed to decode note as UTF-8: {}", abs_path.display()))?;

    let mut line_range: Option<(usize, usize)> = None;
    let mut total_lines = 0usize;
    let mut offset = 0usize;
    for seg in raw.split_inclusive('\n') {
        total_lines += 1;
        let content_len = seg
            .strip_suffix("\r\n")
            .or_else(|| seg.strip_suffix('\n'))
            .map_or(seg.len(), str::len);
        if total_lines == line_no {
            line_range = Some((offset, offset + content_len));
        }
        offset += seg.len();
    }
    let Some((line_start, line_end)) = line_range else {
        bail!(
            "Task conflict: line {} out of range (file has {} lines)",
            line_no,
            total_lines
        );
    };

    // Verify with the exact task-indexing parser so code fences/frontmatter are never mutated.
    let parsed = crate::vault::parser::parse_markdown("", "", &raw);
    let Some(indexed_task) = parsed.tasks.iter().find(|t| t.line_no == line_no) else {
        let preview = &raw[line_start..line_end];
        return Err(CodedError::new(
            "invalid_argument",
            format!("Line {line_no} does not contain a Markdown task checkbox: {preview}"),
        )
        .into());
    };

    let bom_offset = if line_no == 1 && raw[line_start..line_end].starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let line_for_parse = &raw[line_start + bom_offset..line_end];
    let Some((was_checked, clean_text, raw_text)) = parse_single_task_line(line_for_parse) else {
        return Err(CodedError::new(
            "invalid_argument",
            format!("Line {line_no} does not contain a Markdown task checkbox: {line_for_parse}"),
        )
        .into());
    };
    if let Some(exp_text) = expected_text.map(str::trim) {
        if clean_text.trim() != exp_text
            && raw_text.trim() != exp_text
            && indexed_task.text.trim() != exp_text
        {
            return Err(CodedError::new(
                "conflict",
                format!(
                    "Task conflict at line {line_no}: expected '{exp_text}', found '{clean_text}' (file modified externally)"
                ),
            )
            .into());
        }
    }
    if let Some(exp_checked) = expected_checked {
        if was_checked != exp_checked {
            return Err(CodedError::new(
                "conflict",
                format!(
                    "Task conflict at line {line_no}: checkbox state changed externally (expected {}, found {})",
                    if exp_checked { "[x]" } else { "[ ]" },
                    if was_checked { "[x]" } else { "[ ]" }
                ),
            )
            .into());
        }
    }

    let (marker_rel_pos, new_byte, new_checked) = if let Some(pos) =
        find_checkbox_marker(line_for_parse, "[ ]")
    {
        (pos + 1, b'x', true)
    } else if let Some(pos) = find_checkbox_marker(line_for_parse, "[x]")
        .or_else(|| find_checkbox_marker(line_for_parse, "[X]"))
    {
        (pos + 1, b' ', false)
    } else {
        return Err(CodedError::new(
            "invalid_argument",
            format!("Line {line_no} does not contain a Markdown task checkbox: {line_for_parse}"),
        )
        .into());
    };

    let mut out_bytes = raw.into_bytes();
    out_bytes[line_start + bom_offset + marker_rel_pos] = new_byte;
    write_atomic(abs_path, &out_bytes)?;
    Ok(new_checked)
}

fn find_checkbox_marker(line: &str, marker: &str) -> Option<usize> {
    let indent = line.chars().take_while(|&c| c == ' ' || c == '\t').count();
    let rest = &line[indent..];
    for prefix in ["- ", "* ", "+ "] {
        if let Some(after) = rest.strip_prefix(prefix) {
            if after.starts_with(marker) {
                return Some(indent + prefix.len());
            }
        }
    }
    None
}

/// Creates a new note safely (with optional YAML frontmatter and tags), returning its relative POSIX path.
pub fn create_note(
    vault_root: &Path,
    title: &str,
    folder: Option<&str>,
    content: Option<&str>,
    tags: &[String],
) -> Result<(String, PathBuf)> {
    let clean_title = sanitize_note_filename(title);
    if clean_title.is_empty() {
        return Err(CodedError::new("invalid_argument", "Note title cannot be empty").into());
    }
    let rel_path = match folder.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => {
            let f_norm = f.replace('\\', "/").trim_matches('/').to_string();
            if clean_title.to_ascii_lowercase().ends_with(".md") {
                format!("{f_norm}/{clean_title}")
            } else {
                format!("{f_norm}/{clean_title}.md")
            }
        }
        None => {
            if clean_title.to_ascii_lowercase().ends_with(".md") {
                clean_title.clone()
            } else {
                format!("{clean_title}.md")
            }
        }
    };
    let Some(rel_path) = normalize_vault_rel_path("", &rel_path) else {
        return Err(CodedError::new("invalid_argument", "Note title cannot be empty").into());
    };

    let abs_path = safe_vault_path(vault_root, &rel_path)?;
    if abs_path.exists() {
        return Err(
            CodedError::new("already_exists", format!("Note already exists: {rel_path}")).into(),
        );
    }

    let today = Local::now().format("%Y-%m-%d").to_string();
    let display_title = title_from_rel_path(&rel_path);
    let clean_tags: Vec<String> = tags
        .iter()
        .map(|t| t.trim().trim_start_matches('#').to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let rendered_tags = (!clean_tags.is_empty()).then(|| {
        serde_json::Value::Array(
            clean_tags
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        )
    });
    let mut body = String::new();
    if let Some(c) = content {
        let bom_len = c.len() - c.trim_start_matches('\u{feff}').len();
        if super::frontmatter::markdown_body_offset(c) > bom_len {
            if let Some(tags_val) = rendered_tags {
                let existing = super::frontmatter::read_frontmatter_properties(c)?;
                if existing.contains_key("tags") {
                    return Err(CodedError::new(
                        "invalid_argument",
                        "Content frontmatter already defines tags; remove --tags or the frontmatter tags",
                    )
                    .into());
                }
                let mut updates = std::collections::BTreeMap::new();
                updates.insert("tags".to_string(), tags_val);
                body = upsert_frontmatter_in_markdown(c, &updates)?;
            } else {
                body.push_str(c);
            }
        } else {
            let trimmed = c.trim_start();
            body.push_str("---\n");
            body.push_str(&format!("created: {today}\n"));
            if let Some(tags_val) = rendered_tags {
                body.push_str(&format!(
                    "tags: {}\n",
                    super::frontmatter::render(&tags_val)
                ));
            }
            body.push_str("---\n\n");
            if !trimmed.starts_with("# ") {
                body.push_str(&format!("# {display_title}\n\n"));
            }
            body.push_str(c);
        }
    } else {
        body.push_str("---\n");
        body.push_str(&format!("created: {today}\n"));
        if let Some(tags_val) = rendered_tags {
            body.push_str(&format!(
                "tags: {}\n",
                super::frontmatter::render(&tags_val)
            ));
        }
        body.push_str("---\n\n");
        body.push_str(&format!("# {display_title}\n\n"));
    }

    if !body.ends_with('\n') {
        body.push('\n');
    }
    write_atomic(&abs_path, body.as_bytes())?;
    Ok((rel_path, abs_path))
}

/// Appends text/line/task/callout cleanly to an existing note atomically.
pub fn append_to_note(abs_path: &Path, text: &str) -> Result<()> {
    let existing = fs::read_to_string(abs_path)
        .with_context(|| format!("Failed to read note: {}", abs_path.display()))?;
    let line_ending = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push_str(line_ending);
    }
    let normalized_append = text.replace("\r\n", "\n").replace('\n', line_ending);
    updated.push_str(&normalized_append);
    if !updated.ends_with('\n') {
        updated.push_str(line_ending);
    }
    write_atomic(abs_path, updated.as_bytes())
}

/// Replace a literal string when it occurs exactly once in the Markdown body.
pub(crate) fn replace_note_text(abs_path: &Path, find: &str, replace: &str) -> Result<usize> {
    let first_char_len = find
        .chars()
        .next()
        .ok_or_else(|| {
            anyhow::Error::from(CodedError::new(
                "invalid_argument",
                "Search text must contain at least one character",
            ))
        })?
        .len_utf8();
    let raw = fs::read_to_string(abs_path)
        .with_context(|| format!("Failed to read note: {}", abs_path.display()))?;
    let body_start = super::frontmatter::markdown_body_offset(&raw);
    let body = &raw[body_start..];
    let Some(first_match) = body.find(find) else {
        return Ok(0);
    };
    let matches = std::iter::successors(Some(first_match), |&index| {
        let next = index + first_char_len;
        body[next..].find(find).map(|offset| next + offset)
    })
    .count();
    if matches == 1 && find != replace {
        let start = body_start + first_match;
        let mut updated = raw.clone();
        updated.replace_range(start..start + find.len(), replace);
        if fs::read(abs_path)? != raw.as_bytes() {
            bail!("Note changed while replacing text: {}", abs_path.display());
        }
        write_atomic(abs_path, updated.as_bytes())?;
    }
    Ok(matches)
}

/// Updates YAML frontmatter keys in a note file atomically.
pub fn update_note_frontmatter(
    abs_path: &Path,
    updates: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    let raw = fs::read_to_string(abs_path)
        .with_context(|| format!("Failed to read note: {}", abs_path.display()))?;
    let updated = upsert_frontmatter_in_markdown(&raw, updates)?;
    if updated != raw {
        if fs::read(abs_path)? != raw.as_bytes() {
            bail!(
                "Note changed while updating frontmatter: {}",
                abs_path.display()
            );
        }
        write_atomic(abs_path, updated.as_bytes())?;
    }
    Ok(())
}

/// Moves a note to `<vault>/.trash/` (never permanently unlinking user notes).
pub fn move_note_to_trash(vault_root: &Path, rel_path: &str) -> Result<PathBuf> {
    let src = safe_vault_path(vault_root, rel_path)?;
    if !src.is_file() {
        bail!("Note not found: {rel_path}");
    }
    let trash_root = vault_root.join(".trash");
    let mut dst = trash_root.join(rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    if dst.exists() {
        let stem = dst
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("note")
            .to_string();
        let ext = dst
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("md")
            .to_string();
        let stamp = Local::now().format("%Y%m%d-%H%M%S");
        dst.set_file_name(format!("{stem}-{stamp}.{ext}"));
    }
    ensure_parent(&dst)?;
    fs::rename(&src, &dst)
        .with_context(|| format!("Failed to move {} to .trash/", src.display()))?;
    Ok(dst)
}

/// Renames/moves a note within the vault.
pub fn move_note(vault_root: &Path, src_rel: &str, dst_rel: &str) -> Result<String> {
    let src = safe_vault_path(vault_root, src_rel)?;
    if !src.is_file() {
        bail!("Source note not found: {src_rel}");
    }
    let raw_dst = dst_rel.replace('\\', "/");
    let with_md = if raw_dst.to_ascii_lowercase().ends_with(".md") {
        raw_dst
    } else {
        format!("{raw_dst}.md")
    };
    let Some(normalized_dst) = normalize_vault_rel_path("", &with_md) else {
        return Err(CodedError::new(
            "invalid_destination",
            format!("Destination must be a path within the vault: {dst_rel}"),
        )
        .into());
    };
    if cfg!(windows) && normalized_dst.contains(':') {
        return Err(CodedError::new(
            "invalid_destination",
            format!("Destination must be a path within the vault: {dst_rel}"),
        )
        .into());
    }
    let has_ignored_dir = normalized_dst
        .rsplit_once('/')
        .is_some_and(|(dir, _)| dir.split('/').any(is_ignored_dir_name));
    if has_ignored_dir || !is_markdown_file_path(&normalized_dst) {
        return Err(CodedError::new(
            "invalid_destination",
            format!(
                "Destination is outside the vault index: {normalized_dst} (dot-folders and a bare '.md' name are not indexed)"
            ),
        )
        .into());
    }
    let dst = safe_vault_path(vault_root, &normalized_dst)?;
    let mut case_only_same_dir_rename = false;
    match fs::symlink_metadata(&dst) {
        Ok(_) => {
            let same_parent = src.parent() == dst.parent();
            let diff_spelling = src.file_name() != dst.file_name()
                && src.file_name().zip(dst.file_name()).is_some_and(|(a, b)| {
                    a.to_string_lossy()
                        .eq_ignore_ascii_case(&b.to_string_lossy())
                });
            let same_canonical_entry = same_parent
                && diff_spelling
                && fs::canonicalize(&src)
                    .ok()
                    .zip(fs::canonicalize(&dst).ok())
                    .is_some_and(|(cs, cd)| cs == cd);
            if same_canonical_entry {
                case_only_same_dir_rename = true;
            } else {
                return Err(CodedError::new(
                    "already_exists",
                    format!("Destination note already exists: {normalized_dst}"),
                )
                .into());
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("Cannot inspect {}", dst.display())),
    }
    let existing_parent = dst
        .ancestors()
        .skip(1)
        .find(|parent| parent.is_dir())
        .context("Cannot find the destination's parent directory")?;
    if !existing_parent
        .canonicalize()?
        .starts_with(vault_root.canonicalize()?)
        || has_symlink_component_in_vault(vault_root, &normalized_dst)
    {
        return Err(CodedError::new(
            "invalid_destination",
            format!("Destination must be a path within the vault: {dst_rel}"),
        )
        .into());
    }
    ensure_parent(&dst)?;
    if case_only_same_dir_rename {
        let parent = src
            .parent()
            .context("Cannot determine parent directory for case-only rename")?;
        let pid = std::process::id();
        let tmp = (0u64..1000)
            .map(|attempt| parent.join(format!(".obsidian-case-rename-{pid}-{attempt}.tmp")))
            .find(|candidate| fs::symlink_metadata(candidate).is_err())
            .context("Failed to allocate temporary path for case-only rename")?;
        fs::rename(&src, &tmp).with_context(|| {
            format!(
                "Failed to move {} -> temporary {}",
                src.display(),
                tmp.display()
            )
        })?;
        if let Err(err) = fs::rename(&tmp, &dst) {
            let _ = fs::rename(&tmp, &src);
            return Err(err).with_context(|| {
                format!(
                    "Failed to move {} -> {} (rolled back to {})",
                    tmp.display(),
                    dst.display(),
                    src.display()
                )
            });
        }
    } else {
        fs::rename(&src, &dst)
            .with_context(|| format!("Failed to move {} -> {}", src.display(), dst.display()))?;
    }
    Ok(normalized_dst)
}

/// Rewrites incoming `[[wikilinks]]` and inline Markdown links across the vault after moving
/// `old_rel` -> `new_rel`, preserving headings, aliases, embeds, titles, fragments, and skipping
/// fenced/inline code. Returns the number of note files updated on disk.
pub fn rewrite_wikilinks_for_move(
    vault_root: &Path,
    old_rel: &str,
    new_rel: &str,
    all_paths_before_move: &[String],
    aliases_by_path: &HashMap<String, Vec<String>>,
) -> Result<usize> {
    let mut all_paths_after: Vec<String> = all_paths_before_move
        .iter()
        .map(|p| {
            if p == old_rel {
                new_rel.to_string()
            } else {
                p.clone()
            }
        })
        .collect();
    if !all_paths_after.iter().any(|p| p == new_rel) {
        all_paths_after.push(new_rel.to_string());
    }

    let mut aliases_after = HashMap::new();
    for (p, al) in aliases_by_path {
        if p == old_rel {
            aliases_after.insert(new_rel.to_string(), al.clone());
        } else {
            aliases_after.insert(p.clone(), al.clone());
        }
    }

    let entries = scan_vault_markdown_files(vault_root)?;
    let mut updated_notes = 0usize;
    for entry in entries {
        let Ok(raw) = fs::read_to_string(&entry.abs_path) else {
            continue;
        };
        if !raw.contains("[[") && !raw.contains("](") {
            continue;
        }
        let source_after = entry.rel_path.as_str();
        let source_before = if source_after == new_rel {
            old_rel
        } else {
            source_after
        };
        let is_case_only_rename = old_rel != new_rel && old_rel.eq_ignore_ascii_case(new_rel);
        let wikilink_replacement = |target_raw: &str| {
            if source_after == new_rel
                && (is_image_target(target_raw)
                    || (is_local_attachment_dest(target_raw)
                        && resolve_move_wikilink_target(
                            target_raw,
                            source_before,
                            all_paths_before_move,
                            aliases_by_path,
                        )
                        .is_none()))
            {
                if let Some((before_abs, base)) =
                    resolve_attachment_staged(vault_root, old_rel, target_raw)
                {
                    if let Some((after_abs, after_base)) =
                        resolve_attachment_staged(vault_root, new_rel, target_raw)
                    {
                        if same_file(&before_abs, &after_abs)
                            && (base != PathBase::Explicit || after_base == PathBase::Explicit)
                        {
                            return None;
                        }
                    }
                    let cand1 = normalize_rel_path(before_abs.strip_prefix(vault_root).ok()?);
                    if resolve_attachment_staged(vault_root, new_rel, &cand1)
                        .is_some_and(|(abs, _)| same_file(&before_abs, &abs))
                    {
                        return Some(cand1);
                    }
                    return Some(format!("/{cand1}"));
                }
            }
            let before = resolve_move_wikilink_target(
                target_raw,
                source_before,
                all_paths_before_move,
                aliases_by_path,
            )?;
            let expected = if before == old_rel { new_rel } else { &before };
            let resolve_after = |target: &str| {
                resolve_move_wikilink_target(target, source_after, &all_paths_after, &aliases_after)
            };
            if before == old_rel && is_case_only_rename {
                let raw_norm = target_raw.trim().replace('\\', "/");
                let has_md = raw_norm.to_ascii_lowercase().ends_with(".md");
                let new_no_ext = expected
                    .strip_suffix(".md")
                    .or_else(|| expected.strip_suffix(".MD"))
                    .unwrap_or(expected);
                let new_stem = title_from_rel_path(expected);
                let old_stem = title_from_rel_path(old_rel);
                if raw_norm.eq_ignore_ascii_case(old_rel)
                    || raw_norm.eq_ignore_ascii_case(new_no_ext)
                {
                    let updated = if has_md {
                        expected.to_owned()
                    } else {
                        new_no_ext.to_owned()
                    };
                    return (raw_norm != updated
                        && resolve_after(&updated).as_deref() == Some(expected))
                    .then_some(updated);
                }
                if raw_norm.eq_ignore_ascii_case(&old_stem)
                    || raw_norm.eq_ignore_ascii_case(&format!("{old_stem}.md"))
                {
                    let updated = if has_md {
                        format!("{new_stem}.md")
                    } else {
                        new_stem
                    };
                    return (raw_norm != updated
                        && resolve_after(&updated).as_deref() == Some(expected))
                    .then_some(updated);
                }
            }
            if resolve_after(target_raw).as_deref() == Some(expected) {
                return None;
            }

            let stem = title_from_rel_path(expected);
            if resolve_after(&stem).as_deref() == Some(expected) {
                return Some(stem);
            }
            let path = expected
                .strip_suffix(".md")
                .or_else(|| expected.strip_suffix(".MD"))
                .unwrap_or(expected);
            let explicit = if path.contains('/') {
                path.to_owned()
            } else {
                format!("/{path}")
            };
            (resolve_after(&explicit).as_deref() == Some(expected)).then_some(explicit)
        };
        let markdown_replacement = |path_raw: &str, angle_bracketed: bool| {
            let decoded = decode_markdown_link_path(path_raw)?;
            if is_local_attachment_dest(&decoded) {
                if source_after != new_rel {
                    return None;
                }
                let (before_abs, base) = resolve_attachment_staged(vault_root, old_rel, path_raw)?;
                if let Some((after_abs, after_base)) =
                    resolve_attachment_staged(vault_root, new_rel, path_raw)
                {
                    if same_file(&before_abs, &after_abs)
                        && (!base.is_file_relative() || after_base.is_file_relative())
                    {
                        return None;
                    }
                }
                let target_rel = normalize_rel_path(before_abs.strip_prefix(vault_root).ok()?);
                let relative = relative_markdown_path(new_rel, &target_rel);
                let encoded = encode_markdown_link_path(&relative, angle_bracketed);
                return (encoded != path_raw).then_some(encoded);
            }

            let (before, base) = resolve_move_markdown_target_staged(
                path_raw,
                source_before,
                all_paths_before_move,
            )?;
            let expected = if before == old_rel { new_rel } else { &before };
            let resolve_after = |path: &str| {
                resolve_move_markdown_target_staged(path, source_after, &all_paths_after)
            };
            let relative = relative_markdown_path(source_after, expected);
            let encoded = encode_markdown_link_path(&relative, angle_bracketed);
            if before == old_rel && is_case_only_rename {
                if path_raw != encoded
                    && resolve_after(&encoded).as_ref().map(|(p, _)| p.as_str()) == Some(expected)
                {
                    return Some(encoded);
                }
                return None;
            }
            if let Some((after_path, after_base)) = resolve_after(path_raw) {
                if after_path == expected
                    && !(base == PathBase::SourceFolder && after_base == PathBase::VaultRoot)
                {
                    return None;
                }
            }

            (resolve_after(&encoded).as_ref().map(|(p, _)| p.as_str()) == Some(expected))
                .then_some(encoded)
        };
        let Some(rewritten) =
            rewrite_markdown_links(&raw, &wikilink_replacement, &markdown_replacement)
        else {
            continue;
        };
        write_atomic(&entry.abs_path, rewritten.as_bytes())?;
        updated_notes += 1;
    }
    Ok(updated_notes)
}

fn rewrite_markdown_links(
    raw: &str,
    wikilink_replacement: &impl Fn(&str) -> Option<String>,
    markdown_replacement: &impl Fn(&str, bool) -> Option<String>,
) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut changed = false;
    let mut in_fence: Option<(char, usize)> = None;

    for raw_line in raw.split_inclusive('\n') {
        let (line, ending) = if let Some(line) = raw_line.strip_suffix("\r\n") {
            (line, "\r\n")
        } else if let Some(line) = raw_line.strip_suffix('\n') {
            (line, "\n")
        } else {
            (raw_line, "")
        };
        let trimmed = line.trim();
        if let Some((fc, flen)) = in_fence {
            let close_len = trimmed.chars().take_while(|&c| c == fc).count();
            if close_len >= flen && trimmed[close_len..].trim().is_empty() {
                in_fence = None;
            }
            out.push_str(raw_line);
            continue;
        }
        if let Some((fc, flen)) = is_code_fence_opener(trimmed) {
            in_fence = Some((fc, flen));
            out.push_str(raw_line);
            continue;
        }

        let (new_line, line_changed) =
            rewrite_line_links(line, wikilink_replacement, markdown_replacement);
        changed |= line_changed;
        out.push_str(&new_line);
        out.push_str(ending);
    }

    changed.then_some(out)
}

fn rewrite_line_links(
    line: &str,
    wikilink_replacement: &impl Fn(&str) -> Option<String>,
    markdown_replacement: &impl Fn(&str, bool) -> Option<String>,
) -> (String, bool) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut out = String::with_capacity(len);
    let mut changed = false;

    while i < len {
        // Preserve backslash-escaped delimiters so `\[...](...)` is not treated as a link
        if bytes[i] == b'\\' && i + 1 < len && matches!(bytes[i + 1], b'\\' | b'[' | b'`') {
            out.push_str(&line[i..i + 2]);
            i += 2;
            continue;
        }

        // Preserve inline code spans verbatim
        if bytes[i] == b'`' {
            if let Some(end) = find_inline_code_end(line, i) {
                out.push_str(&line[i..end]);
                i = end;
            } else {
                let ticks = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                out.push_str(&line[i..i + ticks]);
                i += ticks;
            }
            continue;
        }

        if i + 1 < len && bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if let Some(rel_close) = line[i + 2..].find("]]") {
                let inner = &line[i + 2..i + 2 + rel_close];
                let (target_and_heading, pipe_sep, alias_opt) = match inner.split_once('|') {
                    Some((lhs, rhs)) => match lhs.strip_suffix('\\') {
                        Some(stripped) => (stripped, "\\|", Some(rhs)),
                        None => (lhs, "|", Some(rhs)),
                    },
                    None => (inner, "|", None),
                };
                let (target_raw, heading_opt) = match target_and_heading.split_once('#') {
                    Some((lhs, rhs)) => (lhs, Some(rhs)),
                    None => (target_and_heading, None),
                };

                if let Some(target) = wikilink_replacement(target_raw) {
                    out.push_str("[[");
                    out.push_str(&target);
                    if let Some(h) = heading_opt {
                        out.push('#');
                        out.push_str(h);
                    }
                    if let Some(a) = alias_opt {
                        out.push_str(pipe_sep);
                        out.push_str(a);
                    }
                    out.push_str("]]");
                    changed = true;
                    i += 2 + rel_close + 2;
                    continue;
                }

                out.push_str(&line[i..i + 2 + rel_close + 2]);
                i += 2 + rel_close + 2;
                continue;
            }
        }

        if bytes[i] == b'[' {
            if let Some(close_bracket) = find_matching_bracket(line, i) {
                if bytes.get(close_bracket + 1) == Some(&b'(') {
                    if let Some(dest) = parse_inline_markdown_link_dest(line, close_bracket + 1) {
                        let raw_dest = &line[dest.dest_start..dest.dest_end];
                        let path_len = raw_dest.find('#').unwrap_or(raw_dest.len());
                        let path_raw = &raw_dest[..path_len];
                        if let Some(new_path) = markdown_replacement(path_raw, dest.angle_bracketed)
                        {
                            out.push_str(&line[i..dest.dest_start]);
                            out.push_str(&new_path);
                            out.push_str(&line[dest.dest_start + path_len..dest.close_end]);
                            changed = true;
                        } else {
                            out.push_str(&line[i..dest.close_end]);
                        }
                        i = dest.close_end;
                        continue;
                    }
                }
            }
        }

        let ch = line[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    (out, changed)
}

/// Reads Obsidian's `.obsidian/daily-notes.json` (when present) to determine daily notes folder & format.
pub fn daily_note_settings(vault: &VaultProfile) -> (String, String) {
    let mut folder = vault
        .daily_folder
        .clone()
        .unwrap_or_else(|| "Daily Notes".to_string());
    let mut format = vault
        .daily_format
        .clone()
        .unwrap_or_else(|| "YYYY-MM-DD".to_string());

    let cfg_path = vault.path.join(".obsidian").join("daily-notes.json");
    if let Ok(raw) = fs::read_to_string(cfg_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
            if vault.daily_folder.is_none() {
                // A present `folder` is used even when empty: in Obsidian `""` (or `/`) means
                // the vault root. Only a missing key keeps the `Daily Notes` default.
                if let Some(f) = val.get("folder").and_then(|v| v.as_str()) {
                    folder = f.trim().trim_matches('/').trim().to_string();
                }
            }
            if vault.daily_format.is_none() {
                if let Some(fmt) = val.get("format").and_then(|v| v.as_str()) {
                    if !fmt.trim().is_empty() {
                        format = fmt.trim().to_string();
                    }
                }
            }
        }
    }
    (folder, format)
}

/// Converts Obsidian Moment.js date format (e.g. `YYYY-MM-DD`) to `chrono` strftime format.
///
/// Handles only `YYYY`, `YY`, `MM` and `DD`, and mangles formats such as `MMM`.
#[deprecated(note = "use format_moment_date, which understands the Moment tokens Obsidian uses")]
pub fn obsidian_date_format_to_chrono(fmt: &str) -> String {
    fmt.replace("YYYY", "%Y")
        .replace("YY", "%y")
        .replace("MM", "%m")
        .replace("DD", "%d")
}

const MOMENT_MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MOMENT_WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
/// Moment date tokens this formatter understands, longest first within each letter.
const MOMENT_TOKENS: [&str; 19] = [
    "YYYY", "YY", "MMMM", "MMM", "MM", "M", "DDDD", "DDD", "DD", "Do", "D", "dddd", "ddd", "dd",
    "d", "Q", "WW", "W", "GGGG",
];

fn ordinal_suffix(n: u32) -> &'static str {
    match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    }
}

/// Formats `date` with an Obsidian (Moment.js) date format such as `YYYY-MM-DD dddd`.
///
/// Supported tokens: `YYYY` `YY` `MMMM` `MMM` `MM` `M` `DDDD` `DDD` `DD` `Do` `D` `dddd` `ddd`
/// `dd` `d` `Q` `WW` `W` `GGGG`, plus `[literal text]`. Everything else is copied as is, so no
/// input can fail. Month and weekday names are English (Obsidian uses the app locale).
pub fn format_moment_date(fmt: &str, date: NaiveDate) -> String {
    let month = MOMENT_MONTHS[date.month0() as usize];
    let weekday_index = date.weekday().num_days_from_sunday();
    let weekday = MOMENT_WEEKDAYS[weekday_index as usize];
    let iso = date.iso_week();

    let mut out = String::with_capacity(fmt.len() + 8);
    let mut rest = fmt;
    while let Some(ch) = rest.chars().next() {
        if ch == '[' {
            if let Some(end) = rest.find(']') {
                out.push_str(&rest[1..end]);
                rest = &rest[end + 1..];
                continue;
            }
        }
        let Some(token) = MOMENT_TOKENS.iter().find(|t| rest.starts_with(**t)) else {
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
            continue;
        };
        match *token {
            "YYYY" => out.push_str(&format!("{:04}", date.year())),
            "YY" => out.push_str(&format!("{:02}", date.year().rem_euclid(100))),
            "MMMM" => out.push_str(month),
            "MMM" => out.push_str(&month[..3]),
            "MM" => out.push_str(&format!("{:02}", date.month())),
            "M" => out.push_str(&date.month().to_string()),
            "DDDD" => out.push_str(&format!("{:03}", date.ordinal())),
            "DDD" => out.push_str(&date.ordinal().to_string()),
            "DD" => out.push_str(&format!("{:02}", date.day())),
            "Do" => out.push_str(&format!("{}{}", date.day(), ordinal_suffix(date.day()))),
            "D" => out.push_str(&date.day().to_string()),
            "dddd" => out.push_str(weekday),
            "ddd" => out.push_str(&weekday[..3]),
            "dd" => out.push_str(&weekday[..2]),
            "d" => out.push_str(&weekday_index.to_string()),
            "Q" => out.push_str(&(date.month0() / 3 + 1).to_string()),
            "WW" => out.push_str(&format!("{:02}", iso.week())),
            "W" => out.push_str(&iso.week().to_string()),
            _ => out.push_str(&format!("{:04}", iso.year())),
        }
        rest = &rest[token.len()..];
    }
    out
}

/// `Some` for a strict `YYYY-MM-DD` calendar date, `None` for any other string.
fn parse_iso_date(s: &str) -> Option<NaiveDate> {
    let bytes = s.as_bytes();
    let shape_ok = bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                *b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    if !shape_ok {
        return None;
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Opens or creates a Daily Note, returning `(rel_path, abs_path, created)`.
///
/// The file name follows the vault's Moment date format. `date_override` picks the day when it
/// is a `YYYY-MM-DD` date; any other non-empty override is used as the file name as given.
pub fn ensure_daily_note(
    vault: &VaultProfile,
    date_override: Option<&str>,
) -> Result<(String, PathBuf, bool)> {
    let (folder, fmt) = daily_note_settings(vault);
    let format_date = |date: NaiveDate| {
        let name = format_moment_date(&fmt, date);
        if name.trim().is_empty() {
            format_moment_date("YYYY-MM-DD", date)
        } else {
            name
        }
    };
    let date_str = match date_override.map(str::trim).filter(|s| !s.is_empty()) {
        Some(d) => match parse_iso_date(d) {
            Some(date) => format_date(date),
            None => d.to_string(),
        },
        None => format_date(Local::now().date_naive()),
    };
    let rel_path = if folder.is_empty() {
        format!("{date_str}.md")
    } else {
        format!("{}/{date_str}.md", folder.trim_matches('/'))
    };
    let abs_path = safe_vault_path(&vault.path, &rel_path)?;
    if abs_path.is_file() {
        return Ok((rel_path, abs_path, false));
    }

    // A format such as `YYYY/MM/YYYY-MM-DD` puts the note in nested folders; the note itself is
    // named by the last segment.
    let date_str = date_str
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let body = format!(
        "---\ncreated: {date_str}\ntags: [daily]\n---\n\n# {date_str}\n\n## Focus\n\n- [ ] Review priorities and notes\n\n## Notes\n\n"
    );
    write_atomic(&abs_path, body.as_bytes())?;
    Ok((rel_path, abs_path, true))
}

/// Saves a pasted PNG or attached local file into the vault's attachments folder and returns the filename.
pub fn save_vault_attachment(
    vault: &VaultProfile,
    filename: &str,
    bytes: &[u8],
) -> Result<(String, PathBuf)> {
    let folder = vault
        .attachments_folder
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Attachments");
    let clean_name = sanitize_attachment_filename(filename);
    let rel = format!("{}/{clean_name}", folder.trim_matches('/'));
    let abs = safe_vault_path(&vault.path, &rel)?;
    write_atomic(&abs, bytes)?;
    Ok((clean_name, abs))
}

pub fn sanitize_note_filename(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\' => '-',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

fn sanitize_attachment_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment.bin");
    let cleaned = sanitize_note_filename(base);
    if cleaned.is_empty() {
        "attachment.bin".to_string()
    } else {
        cleaned
    }
}

/// Resolves a user-supplied note identifier (exact rel_path, suffix path, stem/title, or alias)
/// against a list of paths without stripping `#` as a wikilink heading separator.
pub fn resolve_note_identifier(
    input: &str,
    all_rel_paths: &[String],
    aliases_by_path: &HashMap<String, Vec<String>>,
) -> Result<String> {
    let mut candidates = note_identifier_candidates(input, all_rel_paths, aliases_by_path);
    match candidates.len() {
        0 => {
            Err(CodedError::new("not_found", format!("Note not found in vault: '{input}'")).into())
        }
        1 => Ok(candidates.remove(0)),
        n => {
            let preview_candidates: Vec<String> = candidates.iter().take(10).cloned().collect();
            let preview = if n > 10 {
                format!("{}, +{} more", preview_candidates.join(", "), n - 10)
            } else {
                preview_candidates.join(", ")
            };
            Err(CodedError::with_candidates(
                "ambiguous",
                format!(
                    "Ambiguous note reference '{input}': {n} notes match ({preview}). Use a vault-relative path."
                ),
                preview_candidates,
            )
            .into())
        }
    }
}

/// Candidates from the first stage that matches anything, sorted. One entry means resolved.
fn note_identifier_candidates(
    input: &str,
    all_rel_paths: &[String],
    aliases_by_path: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let normalized = input.trim().replace('\\', "/");
    let mut raw = normalized.as_str();
    while let Some(rest) = raw.strip_prefix("./") {
        raw = rest;
    }
    let raw = raw.trim_start_matches('/');
    if raw.is_empty() {
        return Vec::new();
    }

    let raw_lower = raw.to_ascii_lowercase();
    let has_md_ext = raw_lower.ends_with(".md");
    let raw_with_md = if has_md_ext {
        raw_lower.clone()
    } else {
        format!("{raw_lower}.md")
    };
    let raw_exact_with_md = format!("{raw}.md");

    // 1a. Exact relative path match (byte-for-byte; exact input wins over input + ".md")
    if let Some(exact) = all_rel_paths.iter().find(|p| p.as_str() == raw) {
        return vec![exact.clone()];
    }
    if let Some(exact) = all_rel_paths
        .iter()
        .find(|p| p.as_str() == raw_exact_with_md)
    {
        return vec![exact.clone()];
    }

    // 1b. Exact relative path match (case-insensitive)
    let mut exact_ci: Vec<String> = all_rel_paths
        .iter()
        .filter(|p| {
            let low = p.to_ascii_lowercase();
            low == raw_lower || low == raw_with_md
        })
        .cloned()
        .collect();
    if !exact_ci.is_empty() {
        exact_ci.sort();
        exact_ci.dedup();
        return exact_ci;
    }

    // 2. Suffix path match (e.g. `Projects/RFC` -> `Work/Projects/RFC.md`)
    let suffix_needle = format!("/{raw_with_md}");
    let mut suffix_matches: Vec<String> = all_rel_paths
        .iter()
        .filter(|p| p.to_ascii_lowercase().ends_with(&suffix_needle))
        .cloned()
        .collect();
    if !suffix_matches.is_empty() {
        suffix_matches.sort();
        suffix_matches.dedup();
        return suffix_matches;
    }

    // 3. Stem / title match
    let stem_needle = raw_lower.strip_suffix(".md").unwrap_or(&raw_lower);
    let mut stem_matches: Vec<String> = all_rel_paths
        .iter()
        .filter(|p| title_from_rel_path(p).to_ascii_lowercase() == stem_needle)
        .cloned()
        .collect();
    if !stem_matches.is_empty() {
        stem_matches.sort();
        stem_matches.dedup();
        return stem_matches;
    }

    // 4. Frontmatter alias match (skip when caller passed an explicit `.md` path literal)
    if !has_md_ext {
        let mut alias_matches: Vec<String> = aliases_by_path
            .iter()
            .filter(|(_, aliases)| {
                aliases
                    .iter()
                    .any(|a| a.trim().eq_ignore_ascii_case(stem_needle))
            })
            .map(|(rel_path, _)| rel_path.clone())
            .collect();
        if !alias_matches.is_empty() {
            alias_matches.sort();
            alias_matches.dedup();
            return alias_matches;
        }
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shortest_path_wikilink_resolution() {
        let paths = vec![
            "Architecture.md".to_string(),
            "Projects/Q4 Architecture RFC.md".to_string(),
            "Archive/Old/Q4 Architecture RFC.md".to_string(),
            "Daily Notes/2026-11-20.md".to_string(),
        ];
        let mut aliases = HashMap::new();
        aliases.insert(
            "Projects/Q4 Architecture RFC.md".to_string(),
            vec!["RFC-042".to_string()],
        );

        assert_eq!(
            resolve_wikilink_target("Q4 Architecture RFC", &paths, &aliases).as_deref(),
            Some("Projects/Q4 Architecture RFC.md")
        );
        assert_eq!(
            resolve_wikilink_target("rfc-042", &paths, &aliases).as_deref(),
            Some("Projects/Q4 Architecture RFC.md")
        );
        assert_eq!(
            resolve_wikilink_target("Archive/Old/Q4 Architecture RFC", &paths, &aliases).as_deref(),
            Some("Archive/Old/Q4 Architecture RFC.md")
        );
        assert_eq!(
            resolve_wikilink_target("Nonexistent Note", &paths, &aliases),
            None
        );
    }

    #[test]
    fn test_surgical_task_toggle_and_trash() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("Tasks.md");
        fs::write(&note, "# Tasks\n\n- [ ] First task\n- [x] Second task\n").unwrap();

        assert!(toggle_task_at_line(&note, 3).unwrap());
        let content = fs::read_to_string(&note).unwrap();
        assert!(content.contains("- [x] First task"));
        assert!(content.contains("- [x] Second task"));

        assert!(!toggle_task_at_line(&note, 4).unwrap());
        let content2 = fs::read_to_string(&note).unwrap();
        assert!(content2.contains("- [ ] Second task"));

        let trashed = move_note_to_trash(dir.path(), "Tasks.md").unwrap();
        assert!(!note.exists());
        assert!(trashed.is_file());
    }

    #[test]
    fn test_relative_markdown_path_and_encoding() {
        assert_eq!(
            relative_markdown_path("Incoming.md", "Archive/Review.md"),
            "Archive/Review.md"
        );
        assert_eq!(
            relative_markdown_path("Projects/Index.md", "Projects/Renamed Plan.md"),
            "Renamed Plan.md"
        );
        assert_eq!(
            relative_markdown_path("Projects/Sub/Index.md", "Projects/Other/Target.md"),
            "../Other/Target.md"
        );
        assert_eq!(
            relative_markdown_path("Projects/Sub/Index.md", "Root.md"),
            "../../Root.md"
        );
        assert_eq!(
            relative_markdown_path("Archive/Review.md", "Archive/Review.md"),
            "Review.md"
        );
        assert_eq!(
            encode_markdown_link_path("Archive/Falsify Plan (v2).md", false),
            "Archive/Falsify%20Plan%20%28v2%29.md"
        );
        assert_eq!(
            encode_markdown_link_path("Archive/Falsify Plan (v2).md", true),
            "Archive/Falsify Plan (v2).md"
        );
        assert_eq!(
            encode_markdown_link_path("../Projects/계획 초안.md", false),
            "../Projects/계획%20초안.md"
        );
    }

    #[test]
    fn staged_attachment_resolution_reports_stage_and_rejects_escapes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        fs::create_dir_all(root.join("n")).unwrap();
        fs::create_dir_all(root.join("Attachments")).unwrap();
        fs::create_dir_all(root.join("deep/x")).unwrap();
        fs::create_dir_all(root.join("my files")).unwrap();
        fs::write(tmp.path().join("outside.png"), b"escape").unwrap();

        fs::write(root.join("n/Note.md"), "# Note\n").unwrap();
        fs::write(root.join("n/local.png"), b"local").unwrap();
        fs::write(root.join("root.png"), b"root").unwrap();
        fs::write(root.join("Attachments/att.png"), b"att").unwrap();
        fs::write(root.join("deep/x/found.png"), b"found").unwrap();
        fs::write(root.join("my files/a b.png"), b"spaced").unwrap();

        let check = |target: &str, expected_rel: &str, expected_stage: PathBase| {
            let (path, stage) = resolve_attachment_staged(&root, "n/Note.md", target)
                .unwrap_or_else(|| panic!("expected resolution for {target:?}"));
            assert_eq!(stage, expected_stage, "wrong stage for {target:?}");
            assert!(
                same_file(&path, &root.join(expected_rel)),
                "wrong file for {target:?}: got {}",
                path.display()
            );
        };

        check("local.png", "n/local.png", PathBase::SourceFolder);
        check("./local.png", "n/local.png", PathBase::Explicit);
        check("../root.png", "root.png", PathBase::Explicit);
        check("root.png", "root.png", PathBase::VaultRoot);
        check("/root.png", "root.png", PathBase::Rooted);
        check("att.png", "Attachments/att.png", PathBase::AttachmentDir);
        check("found.png", "deep/x/found.png", PathBase::NameSearch);
        check(
            "../my%20files/a%20b.png",
            "my files/a b.png",
            PathBase::Explicit,
        );
        check(
            "<../my files/a b.png>",
            "my files/a b.png",
            PathBase::Explicit,
        );

        for rejected in ["../../outside.png", "https://x/y.png", "//h/y.png", ""] {
            assert_eq!(
                resolve_attachment_staged(&root, "n/Note.md", rejected),
                None,
                "expected None for {rejected:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn safe_vault_path_rejects_windows_drive_prefix() {
        let root = Path::new(r"C:\vaults\main");
        for bad in [
            "C:evil.md",
            "C:obs-esc-test/esc.md",
            "D:/outside/note.md",
            r"D:outside\note.md",
            "sub/C:x.md",
            "note.md:stream",
        ] {
            let err = safe_vault_path(root, bad).expect_err(bad);
            let coded = err.downcast_ref::<CodedError>().expect("coded error");
            assert_eq!(coded.code, "invalid_argument", "{bad}");
        }
    }

    #[test]
    fn safe_vault_path_keeps_valid_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for ok in ["a/b.md", "/a/b.md", r"a\b.md", "./a/./b.md", "한글/노트.md"] {
            let joined = safe_vault_path(root, ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
            assert!(joined.starts_with(root), "{ok} -> {}", joined.display());
        }
        assert_eq!(
            safe_vault_path(root, "./a/./b.md").unwrap(),
            root.join("a").join("b.md")
        );
        assert!(safe_vault_path(root, "../x.md").is_err());
        assert!(safe_vault_path(root, "a/../../x.md").is_err());
        assert!(safe_vault_path(root, "").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn safe_vault_path_allows_colon_on_unix() {
        let root = Path::new("/vaults/main");
        let joined = safe_vault_path(root, "Standup 10:30.md").unwrap();
        assert_eq!(joined, root.join("Standup 10:30.md"));
    }

    fn obs09_vault(daily_json: Option<&str>) -> (tempfile::TempDir, VaultProfile) {
        let tmp = tempfile::tempdir().unwrap();
        if let Some(json) = daily_json {
            fs::create_dir_all(tmp.path().join(".obsidian")).unwrap();
            fs::write(tmp.path().join(".obsidian/daily-notes.json"), json).unwrap();
        }
        let vault = crate::vault::vault_from_dir(tmp.path(), Some("obs09"));
        (tmp, vault)
    }

    #[test]
    fn obs09_moment_tokens() {
        let monday = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let cases = [
            ("YYYY-MM-DD", "2026-11-23"),
            ("YYYY-MM-DD dddd", "2026-11-23 Monday"),
            ("YYYY-MMM-DD", "2026-Nov-23"),
            ("YYYY/MMMM/YYYY-MM-DD", "2026/November/2026-11-23"),
            ("YY.M.D", "26.11.23"),
            ("ddd Do MMM", "Mon 23rd Nov"),
            ("[Daily] YYYY-MM-DD", "Daily 2026-11-23"),
            ("[YYYY]", "YYYY"),
            ("dd d", "Mo 1"),
            ("DDDD DDD", "327 327"),
            ("[Q]Q GGGG-[W]WW W", "Q4 2026-W48 48"),
            ("100% YYYY", "100% 2026"),
            ("[open YYYY", "[open 2026"),
            ("YYYY년 M월 D일", "2026년 11월 23일"),
            ("", ""),
        ];
        for (fmt, expected) in cases {
            assert_eq!(format_moment_date(fmt, monday), expected, "{fmt}");
        }

        let march = NaiveDate::from_ymd_opt(2026, 3, 5).unwrap();
        assert_eq!(format_moment_date("M/D", march), "3/5");
        assert_eq!(format_moment_date("MM/DD", march), "03/05");
        assert_eq!(format_moment_date("DDDD", march), "064");
        assert_eq!(format_moment_date("Q", march), "1");
        // ISO week-year differs from the calendar year around New Year.
        let new_year = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        assert_eq!(format_moment_date("GGGG-[W]WW", new_year), "2026-W53");
        assert_eq!(
            format_moment_date("d dddd", NaiveDate::from_ymd_opt(2026, 11, 22).unwrap()),
            "0 Sunday"
        );

        for (day, expected) in [
            (1, "1st"),
            (2, "2nd"),
            (3, "3rd"),
            (4, "4th"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (21, "21st"),
            (22, "22nd"),
            (30, "30th"),
            (31, "31st"),
        ] {
            let date = NaiveDate::from_ymd_opt(2026, 10, day).unwrap();
            assert_eq!(format_moment_date("Do", date), expected);
        }

        // No input panics, whatever it contains.
        for junk in [
            "%",
            "%Y-%m-%d %!",
            "[[]]",
            "]",
            "[",
            "\\\\YYYY",
            "{}{:?}",
            "dddddddddd",
            "MMMMMMMMM",
            "한글 [메모] ✅",
            "\u{0}\t\n",
        ] {
            let _ = format_moment_date(junk, monday);
        }
        // A stray `%` is copied, never read as a strftime specifier.
        assert_eq!(format_moment_date("%Y-%m", monday), "%Y-%m");
    }

    #[test]
    fn obs09_daily_folder_empty_means_root() {
        for folder in ["", "/", "  "] {
            let json = format!("{{\"folder\":\"{folder}\",\"format\":\"YYYY-MM-DD\"}}");
            let (tmp, vault) = obs09_vault(Some(&json));
            let (rel_path, abs_path, created) =
                ensure_daily_note(&vault, Some("2026-11-23")).unwrap();
            assert_eq!(rel_path, "2026-11-23.md", "folder {folder:?}");
            assert!(created);
            assert_eq!(abs_path, tmp.path().join("2026-11-23.md"));
            assert!(tmp.path().join("2026-11-23.md").is_file());
            assert!(!tmp.path().join("Daily Notes").exists());
        }
    }

    #[test]
    fn obs09_daily_folder_absent_keeps_default() {
        for json in [
            None,
            Some("{\"format\":\"YYYY-MM-DD\"}"),
            Some("{}"),
            Some("not json"),
        ] {
            let (tmp, vault) = obs09_vault(json);
            let (rel_path, _, created) = ensure_daily_note(&vault, Some("2026-11-23")).unwrap();
            assert_eq!(rel_path, "Daily Notes/2026-11-23.md", "{json:?}");
            assert!(created);
            assert_eq!(
                fs::read_to_string(tmp.path().join("Daily Notes/2026-11-23.md")).unwrap(),
                "---\ncreated: 2026-11-23\ntags: [daily]\n---\n\n# 2026-11-23\n\n## Focus\n\n- [ ] Review priorities and notes\n\n## Notes\n\n"
            );
        }

        // A folder set on the vault profile still wins over daily-notes.json.
        let (_tmp, mut vault) = obs09_vault(Some("{\"folder\":\"\"}"));
        vault.daily_folder = Some("Journal".into());
        let (rel_path, _, _) = ensure_daily_note(&vault, Some("2026-11-23")).unwrap();
        assert_eq!(rel_path, "Journal/2026-11-23.md");
    }

    #[test]
    fn obs09_daily_override_uses_vault_format() {
        let (_tmp, vault) = obs09_vault(Some("{\"format\":\"YYYY-MM-DD dddd\"}"));
        let (rel_path, _, _) = ensure_daily_note(&vault, Some("2026-11-23")).unwrap();
        assert_eq!(rel_path, "Daily Notes/2026-11-23 Monday.md");
        // The same day resolves to the same note again.
        let (again, _, created) = ensure_daily_note(&vault, Some(" 2026-11-23 ")).unwrap();
        assert_eq!(again, rel_path);
        assert!(!created);

        // Anything that is not a strict YYYY-MM-DD date is used as the file name as given.
        for verbatim in ["sprint-12", "2026-1-5", "2026-02-30", "20261123"] {
            let (rel_path, _, _) = ensure_daily_note(&vault, Some(verbatim)).unwrap();
            assert_eq!(rel_path, format!("Daily Notes/{verbatim}.md"));
        }

        // With no override the name is today's date in the vault format.
        let today = format_moment_date("YYYY-MM-DD dddd", Local::now().date_naive());
        let (rel_path, _, _) = ensure_daily_note(&vault, None).unwrap();
        // Tolerate a midnight rollover between the two clock reads.
        let after = format_moment_date("YYYY-MM-DD dddd", Local::now().date_naive());
        assert!(
            rel_path == format!("Daily Notes/{today}.md")
                || rel_path == format!("Daily Notes/{after}.md"),
            "{rel_path}"
        );
    }

    #[test]
    fn obs09_nested_format_creates_folders_and_names_by_last_segment() {
        let (tmp, vault) = obs09_vault(Some(
            "{\"folder\":\"Journal\",\"format\":\"YYYY/MMMM/YYYY-MM-DD\"}",
        ));
        let (rel_path, abs_path, created) = ensure_daily_note(&vault, Some("2026-11-23")).unwrap();
        assert_eq!(rel_path, "Journal/2026/November/2026-11-23.md");
        assert!(created);
        assert!(abs_path.starts_with(tmp.path()));
        let body = fs::read_to_string(&abs_path).unwrap();
        assert!(body.starts_with("---\ncreated: 2026-11-23\n"), "{body}");
        assert!(body.contains("\n# 2026-11-23\n"), "{body}");

        // A format that yields nothing falls back to the ISO date instead of ".md".
        let (_tmp, vault) = obs09_vault(Some("{\"folder\":\"\",\"format\":\"[]\"}"));
        let (rel_path, _, _) = ensure_daily_note(&vault, Some("2026-11-23")).unwrap();
        assert_eq!(rel_path, "2026-11-23.md");
    }

    #[test]
    fn obs09_format_output_still_goes_through_path_confinement() {
        let (tmp, vault) = obs09_vault(Some("{\"folder\":\"\",\"format\":\"[../]YYYY-MM-DD\"}"));
        assert!(ensure_daily_note(&vault, Some("2026-11-23")).is_err());
        assert!(!tmp.path().parent().unwrap().join("2026-11-23.md").exists());

        let (tmp, vault) = obs09_vault(Some(
            "{\"folder\":\"\",\"format\":\"YYYY-MM-DD [at] HH:mm\"}",
        ));
        let result = ensure_daily_note(&vault, Some("2026-11-23"));
        if cfg!(windows) {
            let err = result.unwrap_err();
            let coded = err.downcast_ref::<crate::models::CodedError>().unwrap();
            assert_eq!(coded.code, "invalid_argument");
            assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1); // only .obsidian
        } else {
            assert_eq!(result.unwrap().0, "2026-11-23 at HH:mm.md");
        }
    }
}
