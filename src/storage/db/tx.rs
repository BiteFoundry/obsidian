//! SQLite transaction helpers for note upsert, pruning, stats, and wikilink resolution.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    config::VaultProfile,
    models::{IndexStats, ParsedNote},
    vault::fs::{
        extract_local_markdown_note_link, title_from_rel_path, VaultLinkIndex,
    },
};

pub(super) fn decode_utf8_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

pub(super) fn delete_note_in_tx(tx: &rusqlite::Transaction<'_>, vault_id: &str, rel_path: &str) -> Result<()> {
    let deleted_rowid: Option<i64> = tx
        .prepare_cached("DELETE FROM notes WHERE vault_id = ?1 AND rel_path = ?2 RETURNING rowid")?
        .query_row(params![vault_id, rel_path], |r| r.get(0))
        .optional()?;
    tx.prepare_cached("DELETE FROM links WHERE vault_id = ?1 AND source_path = ?2")?
        .execute(params![vault_id, rel_path])?;
    tx.prepare_cached("DELETE FROM tags WHERE vault_id = ?1 AND note_path = ?2")?
        .execute(params![vault_id, rel_path])?;
    tx.prepare_cached("DELETE FROM headings WHERE vault_id = ?1 AND note_path = ?2")?
        .execute(params![vault_id, rel_path])?;
    tx.prepare_cached("DELETE FROM tasks WHERE vault_id = ?1 AND note_path = ?2")?
        .execute(params![vault_id, rel_path])?;
    if let Some(rowid) = deleted_rowid {
        tx.prepare_cached("DELETE FROM notes_fts WHERE rowid = ?1")?
            .execute(params![rowid])?;
    }
    Ok(())
}

pub(super) fn index_stats_on_conn(
    conn: &Connection,
    vault: &VaultProfile,
    scanned: usize,
    updated: usize,
    removed: usize,
    elapsed_ms: u64,
) -> Result<IndexStats> {
    let vault_id = vault.id.as_str();
    let total_notes: usize = conn
        .prepare_cached("SELECT COUNT(*) FROM notes WHERE vault_id = ?1")?
        .query_row(params![vault_id], |r| r.get::<_, i64>(0))?
        .max(0) as usize;
    let total_links: usize = conn
        .prepare_cached("SELECT COUNT(*) FROM links WHERE vault_id = ?1")?
        .query_row(params![vault_id], |r| r.get::<_, i64>(0))?
        .max(0) as usize;
    let total_tags: usize = conn
        .prepare_cached("SELECT COUNT(DISTINCT tag) FROM tags WHERE vault_id = ?1")?
        .query_row(params![vault_id], |r| r.get::<_, i64>(0))?
        .max(0) as usize;
    let total_tasks: usize = conn
        .prepare_cached("SELECT COUNT(*) FROM tasks WHERE vault_id = ?1")?
        .query_row(params![vault_id], |r| r.get::<_, i64>(0))?
        .max(0) as usize;
    Ok(IndexStats {
        vault_id: vault.id.clone(),
        vault_name: vault.display_name().to_string(),
        scanned,
        updated,
        removed,
        total_notes,
        total_links,
        total_tags,
        total_tasks,
        elapsed_ms,
        unreadable_dirs: 0,
    })
}

/// Adds each alias, trimmed and ASCII-lower-cased as the resolver compares them, to `names`.
pub(super) fn insert_alias_names(names: &mut HashSet<String>, aliases: &[String]) {
    for alias in aliases {
        let name = alias.trim().to_ascii_lowercase();
        if !name.is_empty() {
            names.insert(name);
        }
    }
}

/// Indexed paths that are no longer on disk, minus everything at or below a directory the scan
/// could not read (`""` protects the whole vault). Prefixes compare case-insensitively on
/// Windows and macOS.
pub(super) fn compute_removed_paths<'a>(
    existing: impl IntoIterator<Item = &'a String>,
    disk_set: &HashSet<String>,
    unreadable_dirs: &[String],
) -> Vec<String> {
    let fold = |s: &str| {
        if cfg!(any(windows, target_os = "macos")) {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    let protected: Vec<String> = unreadable_dirs
        .iter()
        .map(|d| fold(d.trim_matches('/')))
        .collect();
    let is_protected = |path: &str| {
        if protected.is_empty() {
            return false;
        }
        let key = fold(path);
        protected.iter().any(|dir| {
            dir.is_empty()
                || key == *dir
                || key
                    .strip_prefix(dir.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let mut removed: Vec<String> = existing
        .into_iter()
        .filter(|p| !disk_set.contains(*p) && !is_protected(p))
        .cloned()
        .collect();
    removed.sort();
    removed
}

/// Re-resolves every link of the vault using a precomputed O(1) lookup index: O(notes + links).
/// Used for the first index, explicit reindex and bulk changes.
pub(super) fn resolve_all_links_in_tx(tx: &rusqlite::Transaction<'_>, vault_id: &str) -> Result<()> {
    let mut all_paths: Vec<String> = Vec::new();
    let mut aliases_by_path: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut stmt =
            tx.prepare_cached("SELECT rel_path, aliases_json FROM notes WHERE vault_id = ?1")?;
        let rows = stmt.query_map(params![vault_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for r in rows {
            let (rp, aj) = r?;
            if aj != "[]" {
                let aliases: Vec<String> = serde_json::from_str(&aj).unwrap_or_default();
                if !aliases.is_empty() {
                    aliases_by_path.insert(rp.clone(), aliases);
                }
            }
            all_paths.push(rp);
        }
    }

    let mut link_rows: Vec<(i64, String, String, String)> = Vec::new();
    {
        let mut stmt = tx.prepare_cached(
            "SELECT id, source_path, target_raw, link_kind FROM links WHERE vault_id = ?1",
        )?;
        let rows = stmt.query_map(params![vault_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for r in rows {
            link_rows.push(r?);
        }
    }

    if link_rows.is_empty() {
        return Ok(());
    }

    let index = VaultLinkIndex::build(&all_paths, &aliases_by_path);
    let mut upd =
        tx.prepare_cached("UPDATE links SET target_resolved_path = ?1 WHERE id = ?2")?;
    for (id, source_path, target_raw, link_kind) in link_rows {
        let resolved = if link_kind == "markdown" {
            index.resolve_markdown_link(&target_raw, &source_path)
        } else {
            index.resolve_wikilink(&target_raw, &source_path)
        };
        upd.execute(params![resolved, id])?;
    }
    Ok(())
}

pub(super) fn resolve_affected_links_in_tx(
    tx: &rusqlite::Transaction<'_>,
    vault_id: &str,
    affected_sources: &HashSet<String>,
    affected_targets: &HashSet<String>,
    resolve_unresolved: bool,
    stem_candidates: &HashSet<String>,
) -> Result<()> {
    if affected_sources.is_empty()
        && affected_targets.is_empty()
        && !resolve_unresolved
        && stem_candidates.is_empty()
    {
        return Ok(());
    }

    let mut all_paths: Vec<String> = Vec::new();
    let mut aliases_by_path: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut stmt =
            tx.prepare_cached("SELECT rel_path, aliases_json FROM notes WHERE vault_id = ?1")?;
        let rows = stmt.query_map(params![vault_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for r in rows {
            let (rp, aj) = r?;
            if aj != "[]" {
                let aliases: Vec<String> = serde_json::from_str(&aj).unwrap_or_default();
                if !aliases.is_empty() {
                    aliases_by_path.insert(rp.clone(), aliases);
                }
            }
            all_paths.push(rp);
        }
    }

    // A name in `stem_candidates` may now resolve differently, so revisit every link that points
    // at a note carrying that name, either as its file stem or as one of its aliases. Without
    // the alias half, `[[Foo]]` resolved through an alias would keep its old target after
    // `Foo.md` (or a second note with the alias `Foo`) appears.
    let mut target_set = affected_targets.clone();
    if !stem_candidates.is_empty() {
        for rp in &all_paths {
            let by_stem = stem_candidates.contains(&title_from_rel_path(rp).to_ascii_lowercase());
            let by_alias = || {
                aliases_by_path.get(rp).is_some_and(|aliases| {
                    aliases
                        .iter()
                        .any(|a| stem_candidates.contains(&a.trim().to_ascii_lowercase()))
                })
            };
            if by_stem || by_alias() {
                target_set.insert(rp.clone());
            }
        }
    }

    let mut seen_link_ids: HashSet<i64> = HashSet::new();
    let mut link_rows: Vec<(i64, String, String, String, Option<String>)> = Vec::new();

    if !affected_sources.is_empty() {
        let mut stmt = tx.prepare_cached(
            "SELECT id, source_path, target_raw, link_kind, target_resolved_path
             FROM links WHERE vault_id = ?1 AND source_path = ?2",
        )?;
        for src in affected_sources {
            let rows = stmt.query_map(params![vault_id, src], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?;
            for r in rows {
                let (id, source_path, target_raw, link_kind, prev_resolved) = r?;
                if seen_link_ids.insert(id) {
                    link_rows.push((id, source_path, target_raw, link_kind, prev_resolved));
                }
            }
        }
    }

    if !target_set.is_empty() {
        let mut stmt = tx.prepare_cached(
            "SELECT id, source_path, target_raw, link_kind, target_resolved_path
             FROM links WHERE vault_id = ?1 AND target_resolved_path = ?2",
        )?;
        for tgt in &target_set {
            let rows = stmt.query_map(params![vault_id, tgt], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?;
            for r in rows {
                let (id, source_path, target_raw, link_kind, prev_resolved) = r?;
                if seen_link_ids.insert(id) {
                    link_rows.push((id, source_path, target_raw, link_kind, prev_resolved));
                }
            }
        }
    }

    if resolve_unresolved {
        let mut stmt = tx.prepare_cached(
            "SELECT id, source_path, target_raw, link_kind, target_resolved_path
             FROM links WHERE vault_id = ?1 AND target_resolved_path IS NULL",
        )?;
        let rows = stmt.query_map(params![vault_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        for r in rows {
            let (id, source_path, target_raw, link_kind, prev_resolved) = r?;
            if seen_link_ids.insert(id) {
                link_rows.push((id, source_path, target_raw, link_kind, prev_resolved));
            }
        }
    }

    if !link_rows.is_empty() {
        let index = VaultLinkIndex::build(&all_paths, &aliases_by_path);
        let mut upd =
            tx.prepare_cached("UPDATE links SET target_resolved_path = ?1 WHERE id = ?2")?;
        for (id, source_path, target_raw, link_kind, prev_resolved) in link_rows {
            let resolved = if link_kind == "markdown" {
                index.resolve_markdown_link(&target_raw, &source_path)
            } else {
                index.resolve_wikilink(&target_raw, &source_path)
            };
            if resolved != prev_resolved {
                upd.execute(params![resolved, id])?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn upsert_note_in_tx(
    tx: &rusqlite::Transaction<'_>,
    vault_id: &str,
    rel_path: &str,
    title: &str,
    folder: &str,
    parsed: &ParsedNote,
    mtime_ms: i64,
    mtime_ns: i64,
    size_bytes: i64,
    content_hash: &str,
    is_bookmarked: bool,
    replace_fts: bool,
) -> Result<()> {
    let fm_json = serde_json::to_string(&parsed.frontmatter).unwrap_or_else(|_| "{}".into());
    let aliases_json = serde_json::to_string(&parsed.aliases).unwrap_or_else(|_| "[]".into());
    let aliases_text = parsed.aliases.join(" ");
    let tags_text = parsed.tags.join(" ");

    let note_rowid: i64 = tx
        .prepare_cached(
            "INSERT INTO notes (
                vault_id, rel_path, title, folder, frontmatter_json, aliases_json,
                body_plain, word_count, mtime_ms, mtime_ns, size_bytes, content_hash, is_bookmarked
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(vault_id, rel_path) DO UPDATE SET
                title = excluded.title,
                folder = excluded.folder,
                frontmatter_json = excluded.frontmatter_json,
                aliases_json = excluded.aliases_json,
                body_plain = excluded.body_plain,
                word_count = excluded.word_count,
                mtime_ms = excluded.mtime_ms,
                mtime_ns = excluded.mtime_ns,
                size_bytes = excluded.size_bytes,
                content_hash = excluded.content_hash
             RETURNING rowid",
        )?
        .query_row(
            params![
                vault_id,
                rel_path,
                title,
                folder,
                fm_json,
                aliases_json,
                parsed.body_plain,
                parsed.word_count as i64,
                mtime_ms,
                mtime_ns,
                size_bytes,
                content_hash,
                i64::from(is_bookmarked),
            ],
            |r| r.get(0),
        )?;

    tx.prepare_cached("DELETE FROM links WHERE vault_id = ?1 AND source_path = ?2")?
        .execute(params![vault_id, rel_path])?;
    if !parsed.wikilinks.is_empty() {
        let mut ins_wl = tx.prepare_cached(
            "INSERT INTO links (
                vault_id, source_path, target_raw, target_resolved_path, alias, heading, is_embed, link_kind
             ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, 'wikilink')",
        )?;
        for wl in &parsed.wikilinks {
            ins_wl.execute(params![
                vault_id,
                rel_path,
                wl.target_raw,
                wl.alias,
                wl.heading,
                i64::from(wl.is_embed),
            ])?;
        }
    }
    if !parsed.md_links.is_empty() {
        let mut ins_ml = tx.prepare_cached(
            "INSERT INTO links (
                vault_id, source_path, target_raw, target_resolved_path, alias, heading, is_embed, link_kind
             ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, 0, 'markdown')",
        )?;
        for ml in &parsed.md_links {
            if ml.is_image {
                continue;
            }
            if let Some((target_raw, heading)) = extract_local_markdown_note_link(&ml.url) {
                let alias = (!ml.label.trim().is_empty()).then(|| ml.label.trim().to_string());
                ins_ml.execute(params![vault_id, rel_path, target_raw, alias, heading])?;
            }
        }
    }

    tx.prepare_cached("DELETE FROM tags WHERE vault_id = ?1 AND note_path = ?2")?
        .execute(params![vault_id, rel_path])?;
    if !parsed.tags.is_empty() {
        let mut ins_tag = tx.prepare_cached(
            "INSERT OR IGNORE INTO tags (vault_id, note_path, tag) VALUES (?1, ?2, ?3)",
        )?;
        for tag in &parsed.tags {
            ins_tag.execute(params![vault_id, rel_path, tag])?;
        }
    }

    tx.prepare_cached("DELETE FROM headings WHERE vault_id = ?1 AND note_path = ?2")?
        .execute(params![vault_id, rel_path])?;
    if !parsed.headings.is_empty() {
        let mut ins_h = tx.prepare_cached(
            "INSERT INTO headings (vault_id, note_path, level, text, line_no)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for h in &parsed.headings {
            ins_h.execute(params![
                vault_id,
                rel_path,
                h.level as i64,
                h.text,
                h.line_no as i64,
            ])?;
        }
    }

    tx.prepare_cached("DELETE FROM tasks WHERE vault_id = ?1 AND note_path = ?2")?
        .execute(params![vault_id, rel_path])?;
    if !parsed.tasks.is_empty() {
        let mut ins_t = tx.prepare_cached(
            "INSERT INTO tasks (vault_id, note_path, line_no, text, checked)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for t in &parsed.tasks {
            ins_t.execute(params![
                vault_id,
                rel_path,
                t.line_no as i64,
                t.text,
                i64::from(t.checked),
            ])?;
        }
    }

    if replace_fts {
        tx.prepare_cached("DELETE FROM notes_fts WHERE rowid = ?1")?
            .execute(params![note_rowid])?;
    }
    tx.prepare_cached(
        "INSERT INTO notes_fts (rowid, vault_id, rel_path, title, rel_path_text, aliases, tags, body_plain)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?
    .execute(params![
        note_rowid,
        vault_id,
        rel_path,
        title,
        rel_path,
        aliases_text,
        tags_text,
        parsed.body_plain,
    ])?;
    Ok(())
}

