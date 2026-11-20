//! SQLite WAL + FTS5 Knowledge Graph Index (`notes`, `links`, `tags`, `headings`, `tasks`, `notes_fts`).
//!
//! Features:
//! - WAL mode + `busy_timeout = 5000`, scoped by `vault_id`
//! - Fast incremental filesystem scanner (`mtime_ms` + `size_bytes` diff)
//! - Instant Backlinks (with snippet context), Outgoing Links, Unresolved Links, Orphans, and Local/Global BFS Graph
//! - FTS5 full-text search with automatic Korean/CJK `<3` char `LIKE` fallback.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    config::{ensure_parent, VaultProfile},
    models::{
        BacklinkEntry, GraphData, GraphEdge, GraphNode, HeadingEntry, IndexStats, NoteDetail,
        NoteSummary, OutgoingLinkEntry, SearchHit, TagCount, TaskEntry, TaskFilter,
    },
    vault::{
        fs::{
            hydrate_parsed_note_links, resolve_note_identifier, resolve_wikilink_target,
            safe_vault_path, scan_vault_markdown_files, title_from_rel_path,
        },
        parser::parse_markdown,
    },
};

#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        ensure_parent(path)?;
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite index: {}", path.display()))?;
        let storage = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        storage.init_schema()?;
        Ok(storage)
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&crate::config::db_path())
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Failed to open in-memory SQLite")?;
        let storage = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        storage.init_schema()?;
        Ok(storage)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 5000;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS notes (
                vault_id TEXT NOT NULL,
                rel_path TEXT NOT NULL,
                title TEXT NOT NULL,
                folder TEXT NOT NULL,
                frontmatter_json TEXT NOT NULL DEFAULT '{}',
                aliases_json TEXT NOT NULL DEFAULT '[]',
                body_plain TEXT NOT NULL DEFAULT '',
                word_count INTEGER NOT NULL DEFAULT 0,
                mtime_ms INTEGER NOT NULL DEFAULT 0,
                size_bytes INTEGER NOT NULL DEFAULT 0,
                is_bookmarked INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (vault_id, rel_path)
            );

            CREATE TABLE IF NOT EXISTS links (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                vault_id TEXT NOT NULL,
                source_path TEXT NOT NULL,
                target_raw TEXT NOT NULL,
                target_resolved_path TEXT,
                alias TEXT,
                heading TEXT,
                is_embed INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_links_source ON links(vault_id, source_path);
            CREATE INDEX IF NOT EXISTS idx_links_target ON links(vault_id, target_resolved_path);

            CREATE TABLE IF NOT EXISTS tags (
                vault_id TEXT NOT NULL,
                note_path TEXT NOT NULL,
                tag TEXT NOT NULL,
                PRIMARY KEY (vault_id, note_path, tag)
            );
            CREATE INDEX IF NOT EXISTS idx_tags_tag ON tags(vault_id, tag);

            CREATE TABLE IF NOT EXISTS headings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                vault_id TEXT NOT NULL,
                note_path TEXT NOT NULL,
                level INTEGER NOT NULL,
                text TEXT NOT NULL,
                line_no INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_headings_note ON headings(vault_id, note_path);

            CREATE TABLE IF NOT EXISTS tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                vault_id TEXT NOT NULL,
                note_path TEXT NOT NULL,
                line_no INTEGER NOT NULL,
                text TEXT NOT NULL,
                checked INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_note ON tasks(vault_id, note_path);

            CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
                vault_id UNINDEXED,
                rel_path UNINDEXED,
                title,
                rel_path_text,
                aliases,
                tags,
                body_plain,
                tokenize = 'unicode61 remove_diacritics 2'
            );
            ",
        )
        .context("Failed to initialize SQLite schema")?;
        Ok(())
    }

    /// Marks a single note's cached `mtime_ms` stale so the next `sync_vault` unconditionally re-indexes it.
    pub fn invalidate_note_mtime(&self, vault_id: &str, rel_path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        conn.execute(
            "UPDATE notes SET mtime_ms = -1 WHERE vault_id = ?1 AND rel_path = ?2",
            params![vault_id, rel_path],
        )?;
        Ok(())
    }

    /// Incrementally scans `vault.path` comparing `(mtime_ms, size_bytes)` against SQLite.
    /// Parses only added/modified `.md` files, removes deleted notes, and refreshes wikilink resolutions.
    pub fn sync_vault(&self, vault: &VaultProfile) -> Result<IndexStats> {
        let start = Instant::now();
        let disk_entries = scan_vault_markdown_files(&vault.path)?;
        let vault_id = vault.id.as_str();

        let mut conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;

        // Load existing (rel_path -> (mtime_ms, size_bytes, is_bookmarked))
        let mut existing: HashMap<String, (i64, i64, bool)> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT rel_path, mtime_ms, size_bytes, is_bookmarked FROM notes WHERE vault_id = ?1",
            )?;
            let rows = stmt.query_map(params![vault_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)? != 0,
                ))
            })?;
            for r in rows {
                let (rp, mt, sz, bm) = r?;
                existing.insert(rp, (mt, sz, bm));
            }
        }

        let disk_set: HashSet<String> = disk_entries.iter().map(|e| e.rel_path.clone()).collect();
        let removed_paths: Vec<String> = existing
            .keys()
            .filter(|p| !disk_set.contains(*p))
            .cloned()
            .collect();

        let mut to_update = Vec::new();
        for entry in &disk_entries {
            match existing.get(&entry.rel_path) {
                Some(&(mt, sz, _)) if mt == entry.mtime_ms && sz == entry.size_bytes => {}
                _ => to_update.push(entry),
            }
        }

        let needs_link_resolution = !to_update.is_empty() || !removed_paths.is_empty();
        let updated_count = to_update.len();
        let removed_count = removed_paths.len();

        let tx = conn.transaction()?;

        // Remove deleted notes
        for rel in &removed_paths {
            tx.execute(
                "DELETE FROM notes WHERE vault_id = ?1 AND rel_path = ?2",
                params![vault_id, rel],
            )?;
            tx.execute(
                "DELETE FROM links WHERE vault_id = ?1 AND source_path = ?2",
                params![vault_id, rel],
            )?;
            tx.execute(
                "DELETE FROM tags WHERE vault_id = ?1 AND note_path = ?2",
                params![vault_id, rel],
            )?;
            tx.execute(
                "DELETE FROM headings WHERE vault_id = ?1 AND note_path = ?2",
                params![vault_id, rel],
            )?;
            tx.execute(
                "DELETE FROM tasks WHERE vault_id = ?1 AND note_path = ?2",
                params![vault_id, rel],
            )?;
            tx.execute(
                "DELETE FROM notes_fts WHERE vault_id = ?1 AND rel_path = ?2",
                params![vault_id, rel],
            )?;
        }

        // Upsert added/modified notes
        for entry in &to_update {
            let raw = fs::read_to_string(&entry.abs_path).unwrap_or_default();
            let parsed = parse_markdown(&entry.rel_path, &entry.title, &raw);
            let is_bookmarked = existing
                .get(&entry.rel_path)
                .map(|t| t.2)
                .unwrap_or(false);
            let fm_json = serde_json::to_string(&parsed.frontmatter).unwrap_or_else(|_| "{}".into());
            let aliases_json = serde_json::to_string(&parsed.aliases).unwrap_or_else(|_| "[]".into());
            let aliases_text = parsed.aliases.join(" ");
            let tags_text = parsed.tags.join(" ");

            tx.execute(
                "INSERT INTO notes (
                    vault_id, rel_path, title, folder, frontmatter_json, aliases_json,
                    body_plain, word_count, mtime_ms, size_bytes, is_bookmarked
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(vault_id, rel_path) DO UPDATE SET
                    title = excluded.title,
                    folder = excluded.folder,
                    frontmatter_json = excluded.frontmatter_json,
                    aliases_json = excluded.aliases_json,
                    body_plain = excluded.body_plain,
                    word_count = excluded.word_count,
                    mtime_ms = excluded.mtime_ms,
                    size_bytes = excluded.size_bytes",
                params![
                    vault_id,
                    entry.rel_path,
                    entry.title,
                    entry.folder,
                    fm_json,
                    aliases_json,
                    parsed.body_plain,
                    parsed.word_count as i64,
                    entry.mtime_ms,
                    entry.size_bytes,
                    i64::from(is_bookmarked),
                ],
            )?;

            tx.execute(
                "DELETE FROM links WHERE vault_id = ?1 AND source_path = ?2",
                params![vault_id, entry.rel_path],
            )?;
            for wl in &parsed.wikilinks {
                tx.execute(
                    "INSERT INTO links (
                        vault_id, source_path, target_raw, target_resolved_path, alias, heading, is_embed
                     ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6)",
                    params![
                        vault_id,
                        entry.rel_path,
                        wl.target_raw,
                        wl.alias,
                        wl.heading,
                        i64::from(wl.is_embed),
                    ],
                )?;
            }

            tx.execute(
                "DELETE FROM tags WHERE vault_id = ?1 AND note_path = ?2",
                params![vault_id, entry.rel_path],
            )?;
            for tag in &parsed.tags {
                tx.execute(
                    "INSERT OR IGNORE INTO tags (vault_id, note_path, tag) VALUES (?1, ?2, ?3)",
                    params![vault_id, entry.rel_path, tag],
                )?;
            }

            tx.execute(
                "DELETE FROM headings WHERE vault_id = ?1 AND note_path = ?2",
                params![vault_id, entry.rel_path],
            )?;
            for h in &parsed.headings {
                tx.execute(
                    "INSERT INTO headings (vault_id, note_path, level, text, line_no)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![vault_id, entry.rel_path, h.level as i64, h.text, h.line_no as i64],
                )?;
            }

            tx.execute(
                "DELETE FROM tasks WHERE vault_id = ?1 AND note_path = ?2",
                params![vault_id, entry.rel_path],
            )?;
            for t in &parsed.tasks {
                tx.execute(
                    "INSERT INTO tasks (vault_id, note_path, line_no, text, checked)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        vault_id,
                        entry.rel_path,
                        t.line_no as i64,
                        t.text,
                        i64::from(t.checked),
                    ],
                )?;
            }

            tx.execute(
                "DELETE FROM notes_fts WHERE vault_id = ?1 AND rel_path = ?2",
                params![vault_id, entry.rel_path],
            )?;
            tx.execute(
                "INSERT INTO notes_fts (vault_id, rel_path, title, rel_path_text, aliases, tags, body_plain)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    vault_id,
                    entry.rel_path,
                    entry.title,
                    entry.rel_path,
                    aliases_text,
                    tags_text,
                    parsed.body_plain,
                ],
            )?;
        }

        // Re-resolve all wikilinks in the vault whenever notes were added/updated/removed
        if needs_link_resolution {
            let mut all_paths: Vec<String> = Vec::new();
            let mut aliases_by_path: HashMap<String, Vec<String>> = HashMap::new();
            {
                let mut stmt = tx.prepare(
                    "SELECT rel_path, aliases_json FROM notes WHERE vault_id = ?1",
                )?;
                let rows = stmt.query_map(params![vault_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                for r in rows {
                    let (rp, aj) = r?;
                    let aliases: Vec<String> = serde_json::from_str(&aj).unwrap_or_default();
                    if !aliases.is_empty() {
                        aliases_by_path.insert(rp.clone(), aliases);
                    }
                    all_paths.push(rp);
                }
            }

            let mut link_rows: Vec<(i64, String)> = Vec::new();
            {
                let mut stmt = tx.prepare(
                    "SELECT id, target_raw FROM links WHERE vault_id = ?1",
                )?;
                let rows = stmt.query_map(params![vault_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?;
                for r in rows {
                    link_rows.push(r?);
                }
            }

            let mut upd = tx.prepare("UPDATE links SET target_resolved_path = ?1 WHERE id = ?2")?;
            for (id, target_raw) in link_rows {
                let resolved = resolve_wikilink_target(&target_raw, &all_paths, &aliases_by_path);
                upd.execute(params![resolved, id])?;
            }
        }

        let total_notes: usize = tx
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE vault_id = ?1",
                params![vault_id],
                |r| r.get::<_, i64>(0),
            )?
            .max(0) as usize;
        let total_links: usize = tx
            .query_row(
                "SELECT COUNT(*) FROM links WHERE vault_id = ?1",
                params![vault_id],
                |r| r.get::<_, i64>(0),
            )?
            .max(0) as usize;
        let total_tags: usize = tx
            .query_row(
                "SELECT COUNT(DISTINCT tag) FROM tags WHERE vault_id = ?1",
                params![vault_id],
                |r| r.get::<_, i64>(0),
            )?
            .max(0) as usize;
        let total_tasks: usize = tx
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE vault_id = ?1",
                params![vault_id],
                |r| r.get::<_, i64>(0),
            )?
            .max(0) as usize;

        tx.commit()?;

        Ok(IndexStats {
            vault_id: vault.id.clone(),
            vault_name: vault.display_name().to_string(),
            scanned: disk_entries.len(),
            updated: updated_count,
            removed: removed_count,
            total_notes,
            total_links,
            total_tags,
            total_tasks,
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
    }

    pub fn note_count(&self, vault_id: &str) -> Result<usize> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE vault_id = ?1",
            params![vault_id],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Lists all indexed notes in a vault, optionally filtered by folder prefix.
    pub fn list_notes(&self, vault_id: &str, folder: Option<&str>) -> Result<Vec<NoteSummary>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let tags_map = load_tags_map(&conn, vault_id)?;
        let (in_counts, out_counts) = load_degree_maps(&conn, vault_id)?;

        let folder_norm = folder
            .map(|f| f.trim().replace('\\', "/").trim_matches('/').to_ascii_lowercase())
            .filter(|f| !f.is_empty());

        let mut stmt = conn.prepare(
            "SELECT rel_path, title, folder, frontmatter_json, aliases_json, word_count,
                    mtime_ms, size_bytes, is_bookmarked
             FROM notes
             WHERE vault_id = ?1
             ORDER BY LOWER(folder) ASC, LOWER(title) ASC",
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

    /// Resolves any user-provided note identifier (relative path, title, stem, or alias) in `vault_id`.
    pub fn resolve_note_path(&self, vault_id: &str, note_ref: &str) -> Result<String> {
        let (all_paths, aliases_map) = self.all_paths_and_aliases(vault_id)?;
        resolve_note_identifier(note_ref, &all_paths, &aliases_map)
    }

    pub fn all_paths_and_aliases(
        &self,
        vault_id: &str,
    ) -> Result<(Vec<String>, HashMap<String, Vec<String>>)> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
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

    /// Loads complete `NoteDetail` from disk + SQLite index (with resolved wikilinks, images, backlinks, outgoing links).
    pub fn load_note_detail(&self, vault: &VaultProfile, note_ref: &str) -> Result<NoteDetail> {
        let rel_path = self.resolve_note_path(&vault.id, note_ref)?;
        let abs_path = safe_vault_path(&vault.path, &rel_path)?;
        let raw_markdown = fs::read_to_string(&abs_path)
            .with_context(|| format!("Failed to read note: {}", abs_path.display()))?;
        let title = title_from_rel_path(&rel_path);
        let mut parsed = parse_markdown(&rel_path, &title, &raw_markdown);

        let (all_paths, aliases_map) = self.all_paths_and_aliases(&vault.id)?;
        let images = hydrate_parsed_note_links(
            &vault.path,
            &rel_path,
            &mut parsed,
            &all_paths,
            &aliases_map,
        );

        let summary = self
            .get_note_summary(&vault.id, &rel_path)?
            .ok_or_else(|| anyhow!("Note missing from index: {rel_path}"))?;
        let backlinks = self.get_backlinks(&vault.id, &rel_path)?;
        let outgoing_links = self.get_outgoing_links(&vault.id, &rel_path)?;

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
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
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

    /// Full-text search via SQLite FTS5 with automatic `<3`-char Korean/CJK `LIKE` fallback.
    pub fn search_notes(
        &self,
        vault_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let q = query.trim();
        let limit = limit.clamp(1, 200);
        if q.is_empty() {
            let notes = self.list_notes(vault_id, None)?;
            return Ok(notes
                .into_iter()
                .take(limit)
                .map(|n| SearchHit {
                    rel_path: n.rel_path,
                    title: n.title,
                    folder: n.folder,
                    snippet: String::new(),
                    tags: n.tags,
                    word_count: n.word_count,
                    mtime_ms: n.mtime_ms,
                })
                .collect());
        }

        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let tags_map = load_tags_map(&conn, vault_id)?;

        // Use LIKE fallback when query has <3 chars with Hangul/CJK or very short token
        let use_like_first = should_use_short_cjk_like(q);
        if !use_like_first {
            let fts_query = build_fts5_query(q);
            if !fts_query.is_empty() {
                let fts_res = run_fts5_search(&conn, vault_id, &fts_query, limit, &tags_map);
                if let Ok(hits) = fts_res {
                    if !hits.is_empty() {
                        return Ok(hits);
                    }
                }
            }
        }

        // Fallback: case-insensitive LIKE over title, rel_path, aliases, tags, body_plain
        run_like_search(&conn, vault_id, q, limit, &tags_map)
    }

    /// Returns all incoming Linked Mentions (backlinks) to `target_rel_path` with context snippets.
    pub fn get_backlinks(&self, vault_id: &str, target_rel_path: &str) -> Result<Vec<BacklinkEntry>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let target_title = title_from_rel_path(target_rel_path);
        let mut stmt = conn.prepare(
            "SELECT DISTINCT l.source_path, n.title, n.body_plain, l.target_raw, l.heading, l.is_embed
             FROM links l
             JOIN notes n ON n.vault_id = l.vault_id AND n.rel_path = l.source_path
             WHERE l.vault_id = ?1 AND l.target_resolved_path = ?2 AND l.source_path != ?2
             ORDER BY LOWER(n.title) ASC",
        )?;
        let rows = stmt.query_map(params![vault_id, target_rel_path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)? != 0,
            ))
        })?;

        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for r in rows {
            let (source_path, source_title, body_plain, target_raw, heading, is_embed) = r?;
            if !seen.insert(source_path.clone()) {
                continue;
            }
            let snippet = extract_context_snippet(&body_plain, &target_raw)
                .or_else(|| extract_context_snippet(&body_plain, &target_title))
                .unwrap_or_else(|| first_line_preview(&body_plain, 96));
            out.push(BacklinkEntry {
                source_path,
                source_title,
                context_snippet: snippet,
                heading,
                is_embed,
            });
        }
        Ok(out)
    }

    /// Returns all outgoing wikilinks from `source_rel_path` (including unresolved/broken links).
    pub fn get_outgoing_links(
        &self,
        vault_id: &str,
        source_rel_path: &str,
    ) -> Result<Vec<OutgoingLinkEntry>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT l.target_raw, l.target_resolved_path, n.title, l.alias, l.heading, l.is_embed
             FROM links l
             LEFT JOIN notes n ON n.vault_id = l.vault_id AND n.rel_path = l.target_resolved_path
             WHERE l.vault_id = ?1 AND l.source_path = ?2
             ORDER BY l.id ASC",
        )?;
        let rows = stmt.query_map(params![vault_id, source_rel_path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)? != 0,
            ))
        })?;

        let mut out = Vec::new();
        for r in rows {
            let (target_raw, resolved_path, resolved_title, alias, heading, is_embed) = r?;
            let resolved = resolved_path.is_some();
            let target_title = resolved_title.unwrap_or_else(|| target_raw.clone());
            out.push(OutgoingLinkEntry {
                target_raw,
                target_resolved_path: resolved_path,
                target_title,
                alias,
                heading,
                is_embed,
                resolved,
            });
        }
        Ok(out)
    }

    /// Builds a Local (BFS around `center_rel_path` up to `depth`) or Global (`center_rel_path == None`) Knowledge Graph.
    pub fn get_graph(
        &self,
        vault_id: &str,
        center_rel_path: Option<&str>,
        depth: usize,
    ) -> Result<GraphData> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let tags_map = load_tags_map(&conn, vault_id)?;
        let (in_deg, out_deg) = load_degree_maps(&conn, vault_id)?;

        // Load all notes metadata
        let mut note_meta: HashMap<String, (String, String)> = HashMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT rel_path, title, folder FROM notes WHERE vault_id = ?1")?;
            let rows = stmt.query_map(params![vault_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for r in rows {
                let (rp, title, folder) = r?;
                note_meta.insert(rp, (title, folder));
            }
        }

        // Load all edges (excluding image embeds so graph shows note-to-note connections)
        let mut all_edges: Vec<(String, String, bool, bool)> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT source_path, target_raw, target_resolved_path, is_embed
                 FROM links WHERE vault_id = ?1",
            )?;
            let rows = stmt.query_map(params![vault_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)? != 0,
                ))
            })?;
            for r in rows {
                let (src, raw, resolved_opt, is_embed) = r?;
                if crate::vault::parser::is_image_target(&raw) {
                    continue;
                }
                let (target_id, is_resolved) = match resolved_opt {
                    Some(rp) => (rp, true),
                    None => (format!("unresolved:{raw}"), false),
                };
                if src == target_id {
                    continue;
                }
                all_edges.push((src, target_id, is_embed, is_resolved));
            }
        }

        // Build undirected adjacency for BFS neighborhood discovery
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        for (src, dst, _, _) in &all_edges {
            adj.entry(src.clone()).or_default().push(dst.clone());
            adj.entry(dst.clone()).or_default().push(src.clone());
        }

        let mut node_depths: HashMap<String, usize> = HashMap::new();
        if let Some(center) = center_rel_path {
            let max_depth = depth.clamp(1, 5);
            let mut q = VecDeque::new();
            node_depths.insert(center.to_string(), 0);
            q.push_back((center.to_string(), 0usize));
            while let Some((cur, d)) = q.pop_front() {
                if d >= max_depth {
                    continue;
                }
                if let Some(neighbors) = adj.get(&cur) {
                    for nxt in neighbors {
                        if !node_depths.contains_key(nxt) {
                            node_depths.insert(nxt.clone(), d + 1);
                            q.push_back((nxt.clone(), d + 1));
                        }
                    }
                }
            }
        } else {
            // Global graph: all notes + referenced unresolved nodes
            for rp in note_meta.keys() {
                node_depths.insert(rp.clone(), 0);
            }
            for (_, dst, _, is_res) in &all_edges {
                if !is_res {
                    node_depths.entry(dst.clone()).or_insert(1);
                }
            }
        }

        let mut nodes: Vec<GraphNode> = Vec::new();
        for (id, d) in &node_depths {
            if let Some((title, folder)) = note_meta.get(id) {
                nodes.push(GraphNode {
                    id: id.clone(),
                    title: title.clone(),
                    folder: folder.clone(),
                    resolved: true,
                    depth: *d,
                    in_degree: *in_deg.get(id).unwrap_or(&0),
                    out_degree: *out_deg.get(id).unwrap_or(&0),
                    tags: tags_map.get(id).cloned().unwrap_or_default(),
                });
            } else {
                let raw_name = id.strip_prefix("unresolved:").unwrap_or(id).to_string();
                nodes.push(GraphNode {
                    id: id.clone(),
                    title: raw_name,
                    folder: String::new(),
                    resolved: false,
                    depth: *d,
                    in_degree: 1,
                    out_degree: 0,
                    tags: Vec::new(),
                });
            }
        }

        nodes.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then_with(|| {
                    (b.in_degree + b.out_degree).cmp(&(a.in_degree + a.out_degree))
                })
                .then_with(|| a.title.to_ascii_lowercase().cmp(&b.title.to_ascii_lowercase()))
        });

        let mut edge_set = HashSet::new();
        let mut edges = Vec::new();
        for (src, dst, is_embed, _) in all_edges {
            if node_depths.contains_key(&src) && node_depths.contains_key(&dst) {
                if edge_set.insert((src.clone(), dst.clone())) {
                    edges.push(GraphEdge {
                        source: src,
                        target: dst,
                        is_embed,
                    });
                }
            }
        }

        Ok(GraphData {
            center: center_rel_path.map(String::from),
            depth,
            nodes,
            edges,
        })
    }

    /// Returns orphan notes (0 incoming resolved links and 0 outgoing links).
    pub fn find_orphans(&self, vault_id: &str) -> Result<Vec<NoteSummary>> {
        let all = self.list_notes(vault_id, None)?;
        Ok(all
            .into_iter()
            .filter(|n| n.backlink_count == 0 && n.outgoing_count == 0)
            .collect())
    }

    /// Lists all tags across the vault sorted by descending note count.
    pub fn list_tags(&self, vault_id: &str) -> Result<Vec<TagCount>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT tag, COUNT(DISTINCT note_path) AS cnt
             FROM tags
             WHERE vault_id = ?1
             GROUP BY tag
             ORDER BY cnt DESC, tag ASC",
        )?;
        let rows = stmt.query_map(params![vault_id], |r| {
            Ok(TagCount {
                tag: r.get(0)?,
                count: r.get::<_, i64>(1)?.max(0) as usize,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Finds notes matching `#tag` (or nested `#tag/...`).
    pub fn find_by_tag(&self, vault_id: &str, tag: &str) -> Result<Vec<NoteSummary>> {
        let clean = tag.trim().trim_start_matches('#').to_ascii_lowercase();
        let all = self.list_notes(vault_id, None)?;
        Ok(all
            .into_iter()
            .filter(|n| {
                n.tags
                    .iter()
                    .any(|t| t == &clean || t.starts_with(&format!("{clean}/")))
            })
            .collect())
    }

    /// Lists tasks across the vault (optionally filtered by status or note).
    pub fn list_tasks(
        &self,
        vault_id: &str,
        filter: TaskFilter,
        note_rel_path: Option<&str>,
    ) -> Result<Vec<TaskEntry>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT t.note_path, n.title, t.line_no, t.text, t.checked
             FROM tasks t
             JOIN notes n ON n.vault_id = t.vault_id AND n.rel_path = t.note_path
             WHERE t.vault_id = ?1
             ORDER BY t.checked ASC, LOWER(n.title) ASC, t.line_no ASC",
        )?;
        let rows = stmt.query_map(params![vault_id], |r| {
            Ok(TaskEntry {
                note_path: r.get(0)?,
                note_title: r.get(1)?,
                line_no: r.get::<_, i64>(2)?.max(0) as usize,
                text: r.get(3)?,
                checked: r.get::<_, i64>(4)? != 0,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            let item = r?;
            if let Some(np) = note_rel_path {
                if !item.note_path.eq_ignore_ascii_case(np) {
                    continue;
                }
            }
            match filter {
                TaskFilter::All => out.push(item),
                TaskFilter::Open if !item.checked => out.push(item),
                TaskFilter::Done if item.checked => out.push(item),
                _ => {}
            }
        }
        Ok(out)
    }

    /// Returns headings outline for a specific note.
    pub fn get_headings(&self, vault_id: &str, rel_path: &str) -> Result<Vec<HeadingEntry>> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT note_path, level, text, line_no
             FROM headings
             WHERE vault_id = ?1 AND note_path = ?2
             ORDER BY line_no ASC",
        )?;
        let rows = stmt.query_map(params![vault_id, rel_path], |r| {
            Ok(HeadingEntry {
                note_path: r.get(0)?,
                level: r.get::<_, i64>(1)?.clamp(1, 6) as u8,
                text: r.get(2)?,
                line_no: r.get::<_, i64>(3)?.max(0) as usize,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Toggles bookmark flag on a note, returning the new bookmarked state.
    pub fn toggle_bookmark(&self, vault_id: &str, rel_path: &str) -> Result<bool> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
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
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        conn.execute(
            "UPDATE notes SET is_bookmarked = ?1 WHERE vault_id = ?2 AND rel_path = ?3",
            params![i64::from(bookmarked), vault_id, rel_path],
        )?;
        Ok(())
    }
}

fn load_tags_map(conn: &Connection, vault_id: &str) -> Result<HashMap<String, Vec<String>>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT note_path, tag FROM tags WHERE vault_id = ?1 ORDER BY tag ASC",
    )?;
    let rows = stmt.query_map(params![vault_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for r in rows {
        let (path, tag) = r?;
        map.entry(path).or_default().push(tag);
    }
    Ok(map)
}

fn load_degree_maps(
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

/// True when the query contains Korean/CJK characters and has fewer than 3 chars (requiring `LIKE` fallback).
pub fn should_use_short_cjk_like(query: &str) -> bool {
    let trimmed = query.trim();
    let char_count = trimmed.chars().count();
    if char_count == 0 {
        return false;
    }
    let has_cjk = trimmed.chars().any(|c| {
        ('\u{AC00}'..='\u{D7A3}').contains(&c)
            || ('\u{1100}'..='\u{11FF}').contains(&c)
            || ('\u{3130}'..='\u{318F}').contains(&c)
            || ('\u{4E00}'..='\u{9FFF}').contains(&c)
            || ('\u{3040}'..='\u{30FF}').contains(&c)
    });
    has_cjk && char_count < 3
}

fn build_fts5_query(raw: &str) -> String {
    let tokens: Vec<String> = raw
        .split_whitespace()
        .filter_map(|tok| {
            let clean = tok
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '/');
            if clean.is_empty() {
                None
            } else {
                let escaped = clean.replace('"', "\"\"");
                Some(format!("\"{escaped}\"*"))
            }
        })
        .collect();
    tokens.join(" AND ")
}

fn run_fts5_search(
    conn: &Connection,
    vault_id: &str,
    fts_query: &str,
    limit: usize,
    tags_map: &HashMap<String, Vec<String>>,
) -> Result<Vec<SearchHit>> {
    let mut stmt = conn.prepare(
        "SELECT f.rel_path, n.title, n.folder,
                snippet(notes_fts, 6, '[', ']', '...', 14) AS snip,
                n.word_count, n.mtime_ms
         FROM notes_fts f
         JOIN notes n ON n.vault_id = f.vault_id AND n.rel_path = f.rel_path
         WHERE f.vault_id = ?1 AND notes_fts MATCH ?2
         ORDER BY bm25(notes_fts, 0.0, 0.0, 6.0, 2.0, 4.0, 3.0, 1.0) ASC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![vault_id, fts_query, limit as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            r.get::<_, i64>(4)?.max(0) as usize,
            r.get::<_, i64>(5)?,
        ))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (rel_path, title, folder, snippet, word_count, mtime_ms) = r?;
        out.push(SearchHit {
            tags: tags_map.get(&rel_path).cloned().unwrap_or_default(),
            rel_path,
            title,
            folder,
            snippet: snippet.replace('\n', " "),
            word_count,
            mtime_ms,
        });
    }
    Ok(out)
}

fn run_like_search(
    conn: &Connection,
    vault_id: &str,
    query: &str,
    limit: usize,
    tags_map: &HashMap<String, Vec<String>>,
) -> Result<Vec<SearchHit>> {
    let pattern = format!("%{}%", query.to_lowercase());
    let mut stmt = conn.prepare(
        "SELECT n.rel_path, n.title, n.folder, n.body_plain, n.word_count, n.mtime_ms
         FROM notes n
         WHERE n.vault_id = ?1
           AND (
             LOWER(n.title) LIKE ?2
             OR LOWER(n.rel_path) LIKE ?2
             OR LOWER(n.aliases_json) LIKE ?2
             OR LOWER(n.body_plain) LIKE ?2
             OR EXISTS (
                SELECT 1 FROM tags t
                WHERE t.vault_id = n.vault_id AND t.note_path = n.rel_path AND LOWER(t.tag) LIKE ?2
             )
           )
         ORDER BY
           CASE WHEN LOWER(n.title) LIKE ?2 THEN 0 ELSE 1 END,
           n.mtime_ms DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![vault_id, pattern, limit as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?.max(0) as usize,
            r.get::<_, i64>(5)?,
        ))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (rel_path, title, folder, body_plain, word_count, mtime_ms) = r?;
        let snippet = extract_context_snippet(&body_plain, query)
            .unwrap_or_else(|| first_line_preview(&body_plain, 96));
        out.push(SearchHit {
            tags: tags_map.get(&rel_path).cloned().unwrap_or_default(),
            rel_path,
            title,
            folder,
            snippet,
            word_count,
            mtime_ms,
        });
    }
    Ok(out)
}

fn extract_context_snippet(body: &str, needle: &str) -> Option<String> {
    let needle_low = needle.trim().to_lowercase();
    if needle_low.is_empty() {
        return None;
    }
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.to_lowercase().contains(&needle_low) {
            return Some(truncate_chars(trimmed, 110));
        }
    }
    None
}

fn first_line_preview(body: &str, max_chars: usize) -> String {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| truncate_chars(l, max_chars))
        .unwrap_or_default()
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::vault_from_dir;

    #[test]
    fn test_incremental_sync_backlinks_graph_and_korean_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("Projects")).unwrap();
        fs::write(
            root.join("Projects/RFC.md"),
            "---\ntags: [architecture, rust]\naliases: [RFC-1]\n---\n\n# Architecture RFC\n\nLinks to [[Storage Engine]] and [[Missing Note]].\n\n- [ ] Ship graph\n",
        )
        .unwrap();
        fs::write(
            root.join("Storage Engine.md"),
            "---\ntags: [storage]\n---\n\n# Storage Engine\n\nBack-referenced by [[RFC-1]]. Contains 회의 notes.\n",
        )
        .unwrap();
        fs::write(root.join("Orphan.md"), "# Lone Note\n\nNothing links here.\n").unwrap();

        let vault = vault_from_dir(root, Some("TestVault"));
        let storage = Storage::open_in_memory().unwrap();

        let s1 = storage.sync_vault(&vault).unwrap();
        assert_eq!(s1.total_notes, 3);
        assert_eq!(s1.updated, 3);

        // Second sync without disk edits should update 0 files
        let s2 = storage.sync_vault(&vault).unwrap();
        assert_eq!(s2.updated, 0);
        assert_eq!(s2.total_notes, 3);

        // Backlinks via alias (`[[RFC-1]]`)
        let rfc_backlinks = storage.get_backlinks(&vault.id, "Projects/RFC.md").unwrap();
        assert_eq!(rfc_backlinks.len(), 1);
        assert_eq!(rfc_backlinks[0].source_path, "Storage Engine.md");

        // Local graph
        let graph = storage
            .get_graph(&vault.id, Some("Projects/RFC.md"), 2)
            .unwrap();
        assert!(graph.nodes.len() >= 3); // RFC, Storage Engine, Missing Note (unresolved)

        // Orphans
        let orphans = storage.find_orphans(&vault.id).unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].rel_path, "Orphan.md");

        // Short 2-char Korean LIKE fallback
        let kr_hits = storage.search_notes(&vault.id, "회의", 10).unwrap();
        assert_eq!(kr_hits.len(), 1);
        assert_eq!(kr_hits[0].rel_path, "Storage Engine.md");
    }
}
