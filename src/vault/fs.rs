//! Direct Obsidian Markdown filesystem engine:
//! - Recursive `.md` walker (skipping `.obsidian/`, `.git/`, `.trash/`, `node_modules/`)
//! - Case-insensitive shortest-path wikilink & alias resolution (matching Obsidian behavior)
//! - Attachment & image path resolution
//! - Safe atomic writes (`write_atomic`), surgical line-preserving checkbox toggles, `.trash/` deletes
//! - Daily Notes resolution & creation (reading `.obsidian/daily-notes.json` when present).

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;

use crate::{
    config::{ensure_parent, write_atomic, VaultProfile},
    models::{InlineSpan, NoteBlock, NoteImageRef, ParsedNote},
    vault::parser::{is_image_target, mime_type_for_path, upsert_frontmatter_in_markdown},
};

const IGNORED_DIRS: &[&str] = &[".obsidian", ".git", ".trash", "node_modules"];

/// Metadata of a discovered `.md` note on disk.
#[derive(Debug, Clone)]
pub struct DiskNoteEntry {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub title: String,
    pub folder: String,
    pub mtime_ms: i64,
    pub size_bytes: i64,
}

/// Walks a vault directory recursively for `.md` files, skipping hidden/system directories.
pub fn scan_vault_markdown_files(vault_root: &Path) -> Result<Vec<DiskNoteEntry>> {
    if !vault_root.is_dir() {
        bail!("Vault directory does not exist: {}", vault_root.display());
    }
    let mut out = Vec::new();
    walk_dir_recursive(vault_root, vault_root, &mut out)?;
    out.sort_by(|a, b| a.rel_path.to_ascii_lowercase().cmp(&b.rel_path.to_ascii_lowercase()));
    Ok(out)
}

fn walk_dir_recursive(root: &Path, dir: &Path, out: &mut Vec<DiskNoteEntry>) -> Result<()> {
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
            if IGNORED_DIRS.iter().any(|&ig| name.eq_ignore_ascii_case(ig))
                || name.starts_with('.')
            {
                continue;
            }
            walk_dir_recursive(root, &path, out)?;
        } else if ft.is_file() {
            if !name.to_ascii_lowercase().ends_with(".md") {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel_path = normalize_rel_path(rel);
            let title = title_from_rel_path(&rel_path);
            let folder = folder_from_rel_path(&rel_path);
            let meta = entry.metadata().ok();
            let size_bytes = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);
            let mtime_ms = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.push(DiskNoteEntry {
                rel_path,
                abs_path: path,
                title,
                folder,
                mtime_ms,
                size_bytes,
            });
        }
    }
    Ok(())
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
        joined.push(comp);
    }
    Ok(joined)
}

/// Obsidian-style case-insensitive shortest-path wikilink resolver (also checks frontmatter aliases).
pub fn resolve_wikilink_target(
    target_raw: &str,
    all_rel_paths: &[String],
    aliases_by_path: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let raw = target_raw
        .split('#')
        .next()
        .unwrap_or(target_raw)
        .trim()
        .replace('\\', "/");
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

/// Resolves an embedded image/attachment target relative to the note folder, vault root, or common attachment dirs.
pub fn resolve_attachment_path(
    vault_root: &Path,
    note_rel_path: &str,
    raw_target: &str,
) -> Option<PathBuf> {
    let cleaned = raw_target
        .trim()
        .trim_start_matches("./")
        .replace('\\', "/");
    if cleaned.is_empty()
        || cleaned.starts_with("http://")
        || cleaned.starts_with("https://")
        || cleaned.contains("..")
    {
        return None;
    }

    let note_folder = folder_from_rel_path(note_rel_path);
    let mut candidates = Vec::new();
    if !note_folder.is_empty() {
        candidates.push(vault_root.join(&note_folder).join(&cleaned));
    }
    candidates.push(vault_root.join(&cleaned));
    for sub in ["Attachments", "attachments", "assets", "Assets", "media", "images"] {
        candidates.push(vault_root.join(sub).join(&cleaned));
    }
    for cand in candidates {
        if cand.is_file() {
            return Some(cand);
        }
    }

    // Fallback: recursive filename lookup across vault
    let target_fname = cleaned.rsplit('/').next().unwrap_or(&cleaned);
    find_attachment_by_name(vault_root, target_fname)
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
            if IGNORED_DIRS.iter().any(|&ig| name_str.eq_ignore_ascii_case(ig))
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

    for block in &mut parsed.blocks {
        match block {
            NoteBlock::Heading { spans, .. }
            | NoteBlock::Paragraph { spans, .. }
            | NoteBlock::Task { spans, .. }
            | NoteBlock::BulletItem { spans, .. }
            | NoteBlock::NumberedItem { spans, .. } => {
                resolve_spans(spans, all_rel_paths, aliases_by_path);
            }
            NoteBlock::Callout { body_lines, .. } => {
                for line in body_lines {
                    resolve_spans(line, all_rel_paths, aliases_by_path);
                }
            }
            NoteBlock::Blockquote { lines, .. } => {
                for line in lines {
                    resolve_spans(line, all_rel_paths, aliases_by_path);
                }
            }
            NoteBlock::NoteEmbed {
                target,
                resolved_path,
                ..
            } => {
                *resolved_path = resolve_wikilink_target(target, all_rel_paths, aliases_by_path);
            }
            NoteBlock::ImageEmbed {
                raw_target,
                alt,
                resolved_path,
                line_no,
                ..
            } => {
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
    images
}

fn resolve_spans(
    spans: &mut [InlineSpan],
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
            *resolved_path = resolve_wikilink_target(target, all_rel_paths, aliases_by_path);
        }
    }
}

/// Surgically toggles a task checkbox (`- [ ]` <-> `- [x]`) at the exact 1-based `line_no`.
/// Preserves existing line endings and writes atomically. Returns the new `checked` boolean.
pub fn toggle_task_at_line(abs_path: &Path, line_no: usize) -> Result<bool> {
    if line_no == 0 {
        bail!("line_no must be >= 1");
    }
    let raw = fs::read_to_string(abs_path)
        .with_context(|| format!("Failed to read note: {}", abs_path.display()))?;
    let line_ending = if raw.contains("\r\n") { "\r\n" } else { "\n" };
    let had_trailing_newline = raw.ends_with('\n');
    let mut lines: Vec<String> = raw.lines().map(String::from).collect();
    if line_no > lines.len() {
        bail!(
            "Line {} out of range (file has {} lines)",
            line_no,
            lines.len()
        );
    }

    let line = &mut lines[line_no - 1];
    let new_checked = if let Some(pos) = find_checkbox_marker(line, "[ ]") {
        line.replace_range(pos..pos + 3, "[x]");
        true
    } else if let Some(pos) = find_checkbox_marker(line, "[x]")
        .or_else(|| find_checkbox_marker(line, "[X]"))
    {
        line.replace_range(pos..pos + 3, "[ ]");
        false
    } else {
        bail!("Line {line_no} does not contain a Markdown task checkbox: {line}");
    };

    let mut out = lines.join(line_ending);
    if had_trailing_newline {
        out.push_str(line_ending);
    }
    write_atomic(abs_path, out.as_bytes())?;
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
        bail!("Note title cannot be empty");
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

    let abs_path = safe_vault_path(vault_root, &rel_path)?;
    if abs_path.exists() {
        bail!("Note already exists: {rel_path}");
    }

    let today = Local::now().format("%Y-%m-%d").to_string();
    let display_title = title_from_rel_path(&rel_path);
    let mut body = String::new();
    if let Some(c) = content {
        let trimmed = c.trim_start();
        if trimmed.starts_with("---") {
            body.push_str(c);
        } else {
            body.push_str("---\n");
            body.push_str(&format!("created: {today}\n"));
            if !tags.is_empty() {
                let clean_tags: Vec<String> = tags
                    .iter()
                    .map(|t| t.trim().trim_start_matches('#').to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
                if !clean_tags.is_empty() {
                    body.push_str(&format!("tags: [{}]\n", clean_tags.join(", ")));
                }
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
        if !tags.is_empty() {
            let clean_tags: Vec<String> = tags
                .iter()
                .map(|t| t.trim().trim_start_matches('#').to_string())
                .filter(|t| !t.is_empty())
                .collect();
            if !clean_tags.is_empty() {
                body.push_str(&format!("tags: [{}]\n", clean_tags.join(", ")));
            }
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

/// Updates YAML frontmatter keys in a note file atomically.
pub fn update_note_frontmatter(
    abs_path: &Path,
    updates: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    let raw = fs::read_to_string(abs_path)
        .with_context(|| format!("Failed to read note: {}", abs_path.display()))?;
    let updated = upsert_frontmatter_in_markdown(&raw, updates);
    write_atomic(abs_path, updated.as_bytes())
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
    let normalized_dst = if dst_rel.to_ascii_lowercase().ends_with(".md") {
        dst_rel.replace('\\', "/").trim_start_matches('/').to_string()
    } else {
        format!(
            "{}.md",
            dst_rel.replace('\\', "/").trim_start_matches('/')
        )
    };
    let dst = safe_vault_path(vault_root, &normalized_dst)?;
    if dst.exists() {
        bail!("Destination note already exists: {normalized_dst}");
    }
    ensure_parent(&dst)?;
    fs::rename(&src, &dst)
        .with_context(|| format!("Failed to move {} -> {}", src.display(), dst.display()))?;
    Ok(normalized_dst)
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
                if let Some(f) = val.get("folder").and_then(|v| v.as_str()) {
                    if !f.trim().is_empty() {
                        folder = f.trim().trim_matches('/').to_string();
                    }
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
pub fn obsidian_date_format_to_chrono(fmt: &str) -> String {
    fmt.replace("YYYY", "%Y")
        .replace("YY", "%y")
        .replace("MM", "%m")
        .replace("DD", "%d")
}

/// Opens or creates today's Daily Note, returning `(rel_path, abs_path, created)`.
pub fn ensure_daily_note(vault: &VaultProfile, date_override: Option<&str>) -> Result<(String, PathBuf, bool)> {
    let (folder, fmt) = daily_note_settings(vault);
    let date_str = match date_override.map(str::trim).filter(|s| !s.is_empty()) {
        Some(d) => d.to_string(),
        None => {
            let chrono_fmt = obsidian_date_format_to_chrono(&fmt);
            Local::now().format(&chrono_fmt).to_string()
        }
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

/// Resolves a user-supplied note identifier (exact rel_path, stem/title, or alias) against a list of paths.
pub fn resolve_note_identifier(
    input: &str,
    all_rel_paths: &[String],
    aliases_by_path: &HashMap<String, Vec<String>>,
) -> Result<String> {
    resolve_wikilink_target(input, all_rel_paths, aliases_by_path)
        .ok_or_else(|| anyhow!("Note not found in vault: '{input}'"))
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
}
