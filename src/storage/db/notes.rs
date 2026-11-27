//! Note listing, resolution, detail loading, text edits, moves, bookmarks, and paging helpers.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::{tx::decode_utf8_lossy, Storage, VaultPathsAndAliases};
use crate::{
    config::VaultProfile,
    models::{
        CodedError, ListPage, NoteContent, NoteDetail, NoteMove, NoteOmittedCounts, NoteSummary,
        NoteTextReplacement,
    },
    vault::{
        fs::{
            file_mtime_and_size, folder_from_rel_path, hydrate_parsed_note_links_indexed,
            move_note, replace_note_text, resolve_note_identifier,
            rewrite_wikilinks_for_move_paths, safe_vault_path, title_from_rel_path, VaultLinkIndex,
        },
        parser::parse_markdown,
    },
};

impl Storage {
    pub fn note_count(&self, vault_id: &str) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE vault_id = ?1",
            params![vault_id],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Lists all indexed notes in a vault, optionally filtered by folder prefix.
    pub fn list_notes(&self, vault_id: &str, folder: Option<&str>) -> Result<Vec<NoteSummary>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let tags_map = load_tags_map(&conn, vault_id)?;
        let (in_counts, out_counts) = load_degree_maps(&conn, vault_id)?;

        let folder_norm = folder
            .map(|f| {
                f.trim()
                    .replace('\\', "/")
                    .trim_matches('/')
                    .to_ascii_lowercase()
            })
            .filter(|f| !f.is_empty());

        let mut stmt = conn.prepare(
            "SELECT rel_path, title, folder, frontmatter_json, aliases_json, word_count,
                    mtime_ms, size_bytes, is_bookmarked
             FROM notes
             WHERE vault_id = ?1
             ORDER BY LOWER(folder) ASC, LOWER(title) ASC, rel_path ASC",
        )?;
        let rows = stmt.query_map(params![vault_id], |r| {
            let rel_path: String = r.get(0)?;
            let title: String = r.get(1)?;
            let folder: String = r.get(2)?;
            let fm_json: String = r.get(3)?;
            let aliases_json: String = r.get(4)?;
            let word_count: i64 = r.get(5)?;
            let mtime_ms: i64 = r.get(6)?;
            let size_bytes: i64 = r.get(7)?;
            let is_bookmarked: bool = r.get::<_, i64>(8)? != 0;
            Ok((
                rel_path,
                title,
                folder,
                fm_json,
                aliases_json,
                word_count,
                mtime_ms,
                size_bytes,
                is_bookmarked,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (
                rel_path,
                title,
                folder_val,
                fm_json,
                aliases_json,
                word_count,
                mtime_ms,
                size_bytes,
                is_bookmarked,
            ) = row?;

            if let Some(ref flt) = folder_norm {
                let f_low = folder_val.to_ascii_lowercase();
                if f_low != *flt && !f_low.starts_with(&format!("{flt}/")) {
                    continue;
                }
            }

            let frontmatter: BTreeMap<String, serde_json::Value> =
                serde_json::from_str(&fm_json).unwrap_or_default();
            let aliases: Vec<String> = serde_json::from_str(&aliases_json).unwrap_or_default();
            let tags = tags_map.get(&rel_path).cloned().unwrap_or_default();
            let backlink_count = *in_counts.get(&rel_path).unwrap_or(&0);
            let outgoing_count = *out_counts.get(&rel_path).unwrap_or(&0);

            out.push(NoteSummary {
                vault_id: vault_id.to_string(),
                rel_path,
                title,
                folder: folder_val,
                frontmatter,
                aliases,
                tags,
                word_count: word_count.max(0) as usize,
                mtime_ms,
                size_bytes,
                is_bookmarked,
                backlink_count,
                outgoing_count,
            });
        }
        Ok(out)
    }

    /// Lists indexed notes using deterministic keyset pagination.
    pub fn list_notes_page(
        &self,
        vault_id: &str,
        folder: Option<&str>,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<ListPage<NoteSummary>> {
        let after = cursor.map(parse_note_cursor).transpose()?;
        let all = self.list_notes(vault_id, folder)?;
        let keyed: Vec<((String, String, String), NoteSummary)> = all
            .into_iter()
            .map(|n| {
                let key = (
                    n.folder.to_ascii_lowercase(),
                    n.title.to_ascii_lowercase(),
                    n.rel_path.clone(),
                );
                (key, n)
            })
            .collect();
        Ok(page_after(keyed, after, limit, |n| n.rel_path.clone()))
    }

    /// Resolves any user-provided note identifier (relative path, title, stem, or alias) in `vault_id`.
    pub fn resolve_note_path(&self, vault_id: &str, note_ref: &str) -> Result<String> {
        let (all_paths, aliases_map) = self.all_paths_and_aliases(vault_id)?;
        resolve_note_identifier(note_ref, &all_paths, &aliases_map)
    }

    pub fn replace_note_text(
        &self,
        vault: &VaultProfile,
        note: &str,
        find: &str,
        replace: &str,
    ) -> Result<NoteTextReplacement> {
        let rel_path = self.resolve_note_path(&vault.id, note)?;
        let abs_path = safe_vault_path(&vault.path, &rel_path)?;
        let matches = replace_note_text(&abs_path, find, replace)?;
        let replaced = matches == 1;
        if replaced {
            self.sync_paths(vault, std::slice::from_ref(&rel_path), &[])?;
        }
        Ok(NoteTextReplacement {
            replaced,
            matches,
            rel_path,
        })
    }

    /// Move a note in an indexed vault and refresh its backlinks and bookmark.
    pub fn move_note_with_links(
        &self,
        vault: &VaultProfile,
        src: &str,
        dst: &str,
    ) -> Result<NoteMove> {
        let (paths, aliases) = self.all_paths_and_aliases(&vault.id)?;
        let from = resolve_note_identifier(src, &paths, &aliases)?;
        let was_bookmarked = self
            .get_note_summary(&vault.id, &from)?
            .is_some_and(|summary| summary.is_bookmarked);
        let to = move_note(&vault.path, &from, dst)?;
        let rewritten_paths =
            rewrite_wikilinks_for_move_paths(&vault.path, &from, &to, &paths, &aliases)?;
        let updated_links_in_notes = rewritten_paths.len();
        let mut changed_paths = Vec::with_capacity(rewritten_paths.len() + 2);
        changed_paths.push(from.clone());
        changed_paths.push(to.clone());
        changed_paths.extend(rewritten_paths);
        self.sync_paths(vault, &changed_paths, &[])?;
        if was_bookmarked {
            self.set_bookmark(&vault.id, &to, true)?;
        }
        Ok(NoteMove {
            moved: true,
            from,
            to,
            updated_links_in_notes,
            bookmark_preserved: was_bookmarked,
        })
    }

    pub fn all_paths_and_aliases(&self, vault_id: &str) -> Result<VaultPathsAndAliases> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let mut stmt =
            conn.prepare("SELECT rel_path, aliases_json FROM notes WHERE vault_id = ?1")?;
        let rows = stmt.query_map(params![vault_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut paths = Vec::new();
        let mut aliases_map = HashMap::new();
        for r in rows {
            let (rp, aj) = r?;
            let aliases: Vec<String> = serde_json::from_str(&aj).unwrap_or_default();
            if !aliases.is_empty() {
                aliases_map.insert(rp.clone(), aliases);
            }
            paths.push(rp);
        }
        Ok((paths, aliases_map))
    }

    /// Resolves the note and reads its Markdown without constructing the detail view.
    pub fn load_note_content(&self, vault: &VaultProfile, note_ref: &str) -> Result<NoteContent> {
        let rel_path = self.resolve_note_path(&vault.id, note_ref)?;
        let abs_path = safe_vault_path(&vault.path, &rel_path)?;
        let (mtime_ms, _) = file_mtime_and_size(&abs_path);
        let raw_bytes = fs::read(&abs_path)
            .with_context(|| format!("Failed to read note: {}", abs_path.display()))?;
        Ok(NoteContent {
            title: title_from_rel_path(&rel_path),
            rel_path,
            raw_markdown: decode_utf8_lossy(raw_bytes),
            mtime_ms,
        })
    }

    /// Counts for the omitted object of a compact read. One query, no file access.
    pub fn note_omitted_counts(&self, vault_id: &str, rel_path: &str) -> Result<NoteOmittedCounts> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let (backlinks, outgoing_links, tags, headings, tasks, aliases_json): (
            i64,
            i64,
            i64,
            i64,
            i64,
            Option<String>,
        ) = conn.query_row(
            "SELECT
              (SELECT COUNT(DISTINCT source_path) FROM links
                WHERE vault_id = ?1 AND target_resolved_path = ?2 AND source_path != ?2),
              (SELECT COUNT(*) FROM links    WHERE vault_id = ?1 AND source_path = ?2),
              (SELECT COUNT(*) FROM tags     WHERE vault_id = ?1 AND note_path = ?2),
              (SELECT COUNT(*) FROM headings WHERE vault_id = ?1 AND note_path = ?2),
              (SELECT COUNT(*) FROM tasks    WHERE vault_id = ?1 AND note_path = ?2),
              (SELECT aliases_json FROM notes WHERE vault_id = ?1 AND rel_path = ?2)",
            params![vault_id, rel_path],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?;
        let aliases = aliases_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .map_or(0, |v| v.len());
        Ok(NoteOmittedCounts {
            backlinks: backlinks.max(0) as usize,
            outgoing_links: outgoing_links.max(0) as usize,
            tags: tags.max(0) as usize,
            aliases,
            headings: headings.max(0) as usize,
            tasks: tasks.max(0) as usize,
        })
    }

    /// Loads complete `NoteDetail` from disk + SQLite index (with resolved wikilinks, images, backlinks, outgoing links).
    pub fn load_note_detail(&self, vault: &VaultProfile, note_ref: &str) -> Result<NoteDetail> {
        let (all_paths, aliases_map) = self.all_paths_and_aliases(&vault.id)?;
        let rel_path = resolve_note_identifier(note_ref, &all_paths, &aliases_map)?;
        let abs_path = safe_vault_path(&vault.path, &rel_path)?;
        let raw_bytes = fs::read(&abs_path)
            .with_context(|| format!("Failed to read note: {}", abs_path.display()))?;
        let raw_markdown = decode_utf8_lossy(raw_bytes);
        let title = title_from_rel_path(&rel_path);
        let mut parsed = parse_markdown(&rel_path, &title, &raw_markdown);

        let link_index = VaultLinkIndex::build(&all_paths, &aliases_map);
        let images =
            hydrate_parsed_note_links_indexed(&vault.path, &rel_path, &mut parsed, &link_index);

        let summary = self
            .get_note_summary(&vault.id, &rel_path)?
            .ok_or_else(|| anyhow!("Note missing from index: {rel_path}"))?;
        let backlinks = self.get_backlinks(&vault.id, &rel_path)?;
        let outgoing_links = self.get_outgoing_links_inner(vault, &rel_path, Some(&link_index))?;

        Ok(NoteDetail {
            summary,
            raw_markdown,
            parsed,
            backlinks,
            outgoing_links,
            images,
        })
    }

    pub fn get_note_summary(&self, vault_id: &str, rel_path: &str) -> Result<Option<NoteSummary>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let row = conn
            .query_row(
                "SELECT rel_path, title, folder, frontmatter_json, aliases_json, word_count,
                        mtime_ms, size_bytes, is_bookmarked
                 FROM notes
                 WHERE vault_id = ?1 AND rel_path = ?2",
                params![vault_id, rel_path],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                        r.get::<_, i64>(8)? != 0,
                    ))
                },
            )
            .optional()?;

        let Some((
            rp,
            title,
            folder,
            fm_json,
            aliases_json,
            word_count,
            mtime_ms,
            size_bytes,
            is_bookmarked,
        )) = row
        else {
            return Ok(None);
        };

        let mut tags = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT tag FROM tags WHERE vault_id = ?1 AND note_path = ?2 ORDER BY tag ASC",
            )?;
            let rows = stmt.query_map(params![vault_id, rp], |r| r.get::<_, String>(0))?;
            for t in rows {
                tags.push(t?);
            }
        }
        let backlink_count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT source_path) FROM links
             WHERE vault_id = ?1 AND target_resolved_path = ?2 AND source_path != ?2",
            params![vault_id, rp],
            |r| r.get(0),
        )?;
        let outgoing_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM links WHERE vault_id = ?1 AND source_path = ?2",
            params![vault_id, rp],
            |r| r.get(0),
        )?;

        Ok(Some(NoteSummary {
            vault_id: vault_id.to_string(),
            rel_path: rp,
            title,
            folder,
            frontmatter: serde_json::from_str(&fm_json).unwrap_or_default(),
            aliases: serde_json::from_str(&aliases_json).unwrap_or_default(),
            tags,
            word_count: word_count.max(0) as usize,
            mtime_ms,
            size_bytes,
            is_bookmarked,
            backlink_count: backlink_count.max(0) as usize,
            outgoing_count: outgoing_count.max(0) as usize,
        }))
    }

    /// Toggles bookmark flag on a note, returning the new bookmarked state.
    pub fn toggle_bookmark(&self, vault_id: &str, rel_path: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let current: i64 = conn
            .query_row(
                "SELECT is_bookmarked FROM notes WHERE vault_id = ?1 AND rel_path = ?2",
                params![vault_id, rel_path],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let next = if current != 0 { 0 } else { 1 };
        conn.execute(
            "UPDATE notes SET is_bookmarked = ?1 WHERE vault_id = ?2 AND rel_path = ?3",
            params![next, vault_id, rel_path],
        )?;
        Ok(next != 0)
    }

    pub fn set_bookmark(&self, vault_id: &str, rel_path: &str, bookmarked: bool) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        conn.execute(
            "UPDATE notes SET is_bookmarked = ?1 WHERE vault_id = ?2 AND rel_path = ?3",
            params![i64::from(bookmarked), vault_id, rel_path],
        )?;
        Ok(())
    }
}

pub(super) fn page_after<T, K: Ord>(
    mut sorted: Vec<(K, T)>,
    after: Option<K>,
    limit: Option<usize>,
    cursor_of: impl Fn(&T) -> String,
) -> ListPage<T> {
    sorted.sort_by(|(ka, _), (kb, _)| ka.cmp(kb));
    let total = sorted.len();
    let filtered = sorted.into_iter().filter_map(|(k, item)| match &after {
        Some(bound) if k <= *bound => None,
        _ => Some(item),
    });
    let (items, has_more) = match limit {
        Some(lim) => {
            let mut taken: Vec<T> = filtered.take(lim.saturating_add(1)).collect();
            let has_more = taken.len() > lim;
            if has_more {
                taken.truncate(lim);
            }
            (taken, has_more)
        }
        None => (filtered.collect(), false),
    };
    let next_cursor = if has_more {
        items.last().map(cursor_of)
    } else {
        None
    };
    ListPage {
        items,
        total,
        has_more,
        next_cursor,
    }
}

pub(super) fn parse_note_cursor(raw: &str) -> Result<(String, String, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CodedError::new("invalid_argument", format!("Invalid cursor: '{raw}'")).into());
    }
    let folder = folder_from_rel_path(trimmed).to_ascii_lowercase();
    let title = title_from_rel_path(trimmed).to_ascii_lowercase();
    Ok((folder, title, trimmed.to_string()))
}

pub(super) fn parse_task_cursor(raw: &str) -> Result<(bool, String, String, usize)> {
    let mut parts = raw.splitn(3, ':');
    let checked_str = parts.next().unwrap_or("");
    let line_str = parts.next().unwrap_or("");
    let note_path = parts.next().unwrap_or("");
    let checked = match checked_str {
        "0" => false,
        "1" => true,
        _ => {
            return Err(
                CodedError::new("invalid_argument", format!("Invalid cursor: '{raw}'")).into(),
            );
        }
    };
    let line_no: usize = line_str.parse().map_err(|_| {
        anyhow::Error::from(CodedError::new(
            "invalid_argument",
            format!("Invalid cursor: '{raw}'"),
        ))
    })?;
    if note_path.trim().is_empty() {
        return Err(CodedError::new("invalid_argument", format!("Invalid cursor: '{raw}'")).into());
    }
    let note_title = title_from_rel_path(note_path).to_ascii_lowercase();
    Ok((checked, note_title, note_path.to_string(), line_no))
}

pub(super) fn load_tags_map(conn: &Connection, vault_id: &str) -> Result<HashMap<String, Vec<String>>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let mut stmt =
        conn.prepare("SELECT note_path, tag FROM tags WHERE vault_id = ?1 ORDER BY tag ASC")?;
    let rows = stmt.query_map(params![vault_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for r in rows {
        let (path, tag) = r?;
        map.entry(path).or_default().push(tag);
    }
    Ok(map)
}

pub(super) fn load_degree_maps(
    conn: &Connection,
    vault_id: &str,
) -> Result<(HashMap<String, usize>, HashMap<String, usize>)> {
    let mut in_deg: HashMap<String, usize> = HashMap::new();
    let mut out_deg: HashMap<String, usize> = HashMap::new();

    {
        let mut stmt = conn.prepare(
            "SELECT target_resolved_path, COUNT(DISTINCT source_path)
             FROM links
             WHERE vault_id = ?1 AND target_resolved_path IS NOT NULL AND source_path != target_resolved_path
             GROUP BY target_resolved_path",
        )?;
        let rows = stmt.query_map(params![vault_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as usize))
        })?;
        for r in rows {
            let (p, c) = r?;
            in_deg.insert(p, c);
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT source_path, COUNT(*)
             FROM links
             WHERE vault_id = ?1
             GROUP BY source_path",
        )?;
        let rows = stmt.query_map(params![vault_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as usize))
        })?;
        for r in rows {
            let (p, c) = r?;
            out_deg.insert(p, c);
        }
    }
    Ok((in_deg, out_deg))
}

