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
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::{
    config::{ensure_parent, VaultProfile},
    models::{
        BacklinkEntry, GraphData, GraphEdge, GraphNode, HeadingEntry, IndexStats, NoteDetail,
        NoteSummary, OutgoingLinkEntry, ParsedNote, SearchHit, TagCount, TaskEntry, TaskFilter,
    },
    vault::{
        fs::{
            canonical_rel_path_in_vault, content_sha256_hex, file_mtime_ns_and_size,
            folder_from_rel_path, has_symlink_component_in_vault, hydrate_parsed_note_links,
            resolve_note_identifier, resolve_wikilink_target, safe_vault_path,
            scan_vault_markdown_files, title_from_rel_path,
        },
        parser::parse_markdown,
    },
};

/// Result of an incremental path-scoped sync batch.
#[derive(Debug, Clone, Default)]
pub struct PathSyncOutcome {
    pub stats: IndexStats,
    pub locked_paths: Vec<String>,
}

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
                mtime_ns INTEGER NOT NULL DEFAULT 0,
                size_bytes INTEGER NOT NULL DEFAULT 0,
                content_hash TEXT NOT NULL DEFAULT '',
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

        // Forward-compatible migration for existing index.db files created before mtime_ns/content_hash.
        let _ = conn.execute(
            "ALTER TABLE notes ADD COLUMN mtime_ns INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE notes ADD COLUMN content_hash TEXT NOT NULL DEFAULT ''",
            [],
        );
        Ok(())
    }

    /// Returns SQLite's `PRAGMA data_version`, an O(1) counter that increments whenever
    /// any *other* database connection commits a write to `index.db` (e.g. bookmarks or CLI writes).
    pub fn data_version(&self) -> Result<i64> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let ver: i64 = conn.query_row("PRAGMA data_version", [], |r| r.get(0))?;
        Ok(ver)
    }

    /// Computes a deterministic fingerprint of the vault's indexed note state in SQLite,
    /// allowing long-lived watchers to detect cross-process updates (e.g. CLI/HTTP/MCP/Daemon).
    pub fn vault_index_fingerprint(&self, vault_id: &str) -> Result<String> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT rel_path, mtime_ms, mtime_ns, size_bytes, word_count, is_bookmarked, content_hash
             FROM notes
             WHERE vault_id = ?1
             ORDER BY rel_path ASC",
        )?;
        let rows = stmt.query_map(params![vault_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        let mut acc = String::new();
        for row in rows {
            let (rp, mt_ms, mt_ns, sz, wc, bm, hash) = row?;
            acc.push_str(&rp);
            acc.push(':');
            acc.push_str(&mt_ms.to_string());
            acc.push(':');
            acc.push_str(&mt_ns.to_string());
            acc.push(':');
            acc.push_str(&sz.to_string());
            acc.push(':');
            acc.push_str(&wc.to_string());
            acc.push(':');
            acc.push_str(&bm.to_string());
            acc.push(':');
            acc.push_str(&hash);
            acc.push('\n');
        }
        Ok(content_sha256_hex(acc.as_bytes()))
    }

    /// Marks a single note's cached `mtime_ms` stale so the next `sync_vault` unconditionally re-indexes it.
    pub fn invalidate_note_mtime(&self, vault_id: &str, rel_path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        conn.execute(
            "UPDATE notes SET mtime_ms = -1, mtime_ns = -1, content_hash = '' WHERE vault_id = ?1 AND rel_path = ?2",
            params![vault_id, rel_path],
        )?;
        Ok(())
    }

    /// Fast single-note incremental SQLite upsert (zero filesystem scanning, < 2ms).
    ///
    /// Updates only the specified note's rows in `notes`, `notes_fts`, `tags`, `headings`,
    /// `tasks`, and `links` inside a single transaction (preserving `is_bookmarked`), and
    /// resolves outgoing links for that note against existing vault paths/aliases.
    pub fn upsert_single_note(
        &self,
        vault: &VaultProfile,
        rel_path: &str,
        raw_markdown: &str,
        mtime_ms: i64,
        size_bytes: i64,
    ) -> Result<()> {
        let vault_id = vault.id.as_str();
        let title = title_from_rel_path(rel_path);
        let folder = folder_from_rel_path(rel_path);
        let parsed = parse_markdown(rel_path, &title, raw_markdown);
        let content_hash = content_sha256_hex(raw_markdown.as_bytes());
        let mtime_ns = safe_vault_path(&vault.path, rel_path)
            .ok()
            .filter(|p| p.is_file())
            .map(|p| file_mtime_ns_and_size(&p).0)
            .unwrap_or_else(|| mtime_ms.saturating_mul(1_000_000));

        let mut conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let tx = conn.transaction()?;

        let prev: Option<(bool, String)> = tx
            .query_row(
                "SELECT is_bookmarked, aliases_json FROM notes WHERE vault_id = ?1 AND rel_path = ?2",
                params![vault_id, rel_path],
                |r| Ok((r.get::<_, i64>(0)? != 0, r.get::<_, String>(1)?)),
            )
            .optional()?;
        let is_new = prev.is_none();
        let is_bookmarked = prev.as_ref().map(|(bm, _)| *bm).unwrap_or(false);
        let prev_aliases_json = prev.map(|(_, aj)| aj);
        let new_aliases_json =
            serde_json::to_string(&parsed.aliases).unwrap_or_else(|_| "[]".into());
        let aliases_changed = prev_aliases_json.as_deref() != Some(new_aliases_json.as_str());

        upsert_note_in_tx(
            &tx,
            vault_id,
            rel_path,
            &title,
            &folder,
            &parsed,
            mtime_ms,
            mtime_ns,
            size_bytes,
            &content_hash,
            is_bookmarked,
            true,
        )?;

        let mut affected_sources = HashSet::new();
        affected_sources.insert(rel_path.to_string());
        let mut affected_targets = HashSet::new();
        if aliases_changed && !is_new {
            affected_targets.insert(rel_path.to_string());
        }
        let mut stem_candidates = HashSet::new();
        if is_new {
            stem_candidates.insert(title.to_ascii_lowercase());
        }
        resolve_affected_links_in_tx(
            &tx,
            vault_id,
            &affected_sources,
            &affected_targets,
            is_new || aliases_changed,
            &stem_candidates,
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Returns lightweight aggregate `IndexStats` from SQLite without scanning the filesystem.
    pub fn index_stats(
        &self,
        vault: &VaultProfile,
        scanned: usize,
        updated: usize,
        removed: usize,
        elapsed_ms: u64,
    ) -> Result<IndexStats> {
        let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        index_stats_on_conn(&conn, vault, scanned, updated, removed, elapsed_ms)
    }

    /// Incrementally syncs only the specified Markdown relative paths and removed directory prefixes
    /// (zero full-vault directory scan and zero full-vault link sweep).
    ///
    /// - Reuses `upsert_single_note` for single live Markdown creations/modifications.
    /// - Evaluates actual on-disk path state at batch end so atomic replaces and renames converge cleanly.
    /// - Skips transiently locked/unreadable files (reporting them in `locked_paths`) without poisoning SQLite.
    /// - Deletes removed notes and re-resolves only affected wikilinks/backlinks.
    pub fn sync_paths(
        &self,
        vault: &VaultProfile,
        rel_paths: &[String],
        removed_dir_prefixes: &[String],
    ) -> Result<PathSyncOutcome> {
        let start = Instant::now();
        let vault_id = vault.id.as_str();
        let canonical_root = fs::canonicalize(&vault.path).ok();

        let mut candidate_paths: Vec<String> = Vec::with_capacity(rel_paths.len());
        let mut seen_candidates: HashSet<String> = HashSet::new();
        for rp in rel_paths {
            let clean = rp.trim().replace('\\', "/").trim_start_matches('/').to_string();
            if !clean.is_empty() && seen_candidates.insert(clean.clone()) {
                candidate_paths.push(clean);
            }
        }

        // Expand any removed/renamed-away directory prefixes against indexed notes in SQLite.
        if !removed_dir_prefixes.is_empty() {
            let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
            let mut stmt = if cfg!(windows) {
                conn.prepare(
                    "SELECT rel_path FROM notes
                     WHERE vault_id = ?1 AND (LOWER(folder) = LOWER(?2) OR LOWER(folder) LIKE LOWER(?3))",
                )?
            } else {
                conn.prepare(
                    "SELECT rel_path FROM notes
                     WHERE vault_id = ?1 AND (folder = ?2 OR folder LIKE ?3)",
                )?
            };
            for prefix in removed_dir_prefixes {
                let p = prefix
                    .trim()
                    .replace('\\', "/")
                    .trim_matches('/')
                    .to_string();
                if p.is_empty() {
                    continue;
                }
                let like_pat = format!("{p}/%");
                let rows = stmt.query_map(params![vault_id, p, like_pat], |r| r.get::<_, String>(0))?;
                for r in rows {
                    let rp = r?;
                    if seen_candidates.insert(rp.clone()) {
                        candidate_paths.push(rp);
                    }
                }
            }
        }

        if candidate_paths.is_empty() {
            let stats = self.index_stats(vault, 0, 0, 0, start.elapsed().as_millis() as u64)?;
            return Ok(PathSyncOutcome {
                stats,
                locked_paths: Vec::new(),
            });
        }

        // Inspect actual on-disk state for each candidate path outside the SQLite lock.
        struct LiveNoteDisk {
            rel_path: String,
            raw: String,
            mtime_ms: i64,
            mtime_ns: i64,
            size_bytes: i64,
            content_hash: String,
        }

        let mut live_notes: Vec<LiveNoteDisk> = Vec::new();
        let mut live_seen: HashSet<String> = HashSet::new();
        let mut removed_paths: Vec<String> = Vec::new();
        let mut removed_seen: HashSet<String> = HashSet::new();
        let mut locked_paths: Vec<String> = Vec::new();

        for rel in &candidate_paths {
            let Ok(abs) = safe_vault_path(&vault.path, rel) else {
                continue;
            };
            if has_symlink_component_in_vault(&vault.path, rel) {
                if removed_seen.insert(rel.clone()) {
                    removed_paths.push(rel.clone());
                }
                continue;
            }
            match fs::symlink_metadata(&abs) {
                Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => {
                    // Detect case-only renames on case-insensitive filesystems (Windows/macOS).
                    let disk_rel = if cfg!(windows) || cfg!(target_os = "macos") {
                        canonical_rel_path_in_vault(&vault.path, canonical_root.as_deref(), &abs)
                            .unwrap_or_else(|| rel.clone())
                    } else {
                        rel.clone()
                    };
                    if disk_rel != *rel && removed_seen.insert(rel.clone()) {
                        removed_paths.push(rel.clone());
                    }
                    if !live_seen.insert(disk_rel.clone()) {
                        continue;
                    }
                    let (mtime_ns, size_bytes) = file_mtime_ns_and_size(&abs);
                    let mtime_ms = mtime_ns / 1_000_000;
                    match fs::read(&abs) {
                        Ok(bytes) => {
                            let content_hash = content_sha256_hex(&bytes);
                            let raw = decode_utf8_lossy(bytes);
                            live_notes.push(LiveNoteDisk {
                                rel_path: disk_rel,
                                raw,
                                mtime_ms,
                                mtime_ns,
                                size_bytes,
                                content_hash,
                            });
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                            if removed_seen.insert(disk_rel.clone()) {
                                removed_paths.push(disk_rel);
                            }
                        }
                        Err(_) => {
                            locked_paths.push(disk_rel);
                        }
                    }
                }
                Ok(_) => {
                    // Symlink or directory at note path: not an indexable regular file.
                    if removed_seen.insert(rel.clone()) {
                        removed_paths.push(rel.clone());
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if removed_seen.insert(rel.clone()) {
                        removed_paths.push(rel.clone());
                    }
                }
                Err(_) => {
                    locked_paths.push(rel.clone());
                }
            }
        }

        // Fast path: single live Markdown create/modify -> directly reuse `upsert_single_note`.
        if removed_paths.is_empty() && live_notes.len() == 1 {
            let note = &live_notes[0];
            let unchanged = {
                let conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
                conn.query_row(
                    "SELECT mtime_ms, mtime_ns, size_bytes, content_hash
                     FROM notes WHERE vault_id = ?1 AND rel_path = ?2",
                    params![vault_id, note.rel_path],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, String>(3)?,
                        ))
                    },
                )
                .optional()?
                .is_some_and(|(ms, ns, sz, hash)| {
                    ms == note.mtime_ms
                        && ns == note.mtime_ns
                        && sz == note.size_bytes
                        && hash == note.content_hash
                })
            };
            let updated = if unchanged {
                0
            } else {
                self.upsert_single_note(
                    vault,
                    &note.rel_path,
                    &note.raw,
                    note.mtime_ms,
                    note.size_bytes,
                )?;
                1
            };
            let stats = self.index_stats(
                vault,
                candidate_paths.len(),
                updated,
                0,
                start.elapsed().as_millis() as u64,
            )?;
            return Ok(PathSyncOutcome {
                stats,
                locked_paths,
            });
        }

        let mut conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;
        let tx = conn.transaction()?;

        let mut updated_count = 0usize;
        let mut removed_count = 0usize;
        let mut affected_sources: HashSet<String> = HashSet::new();
        let mut affected_targets: HashSet<String> = HashSet::new();
        let mut stem_candidates: HashSet<String> = HashSet::new();
        let mut resolve_unresolved = false;

        for rel in &removed_paths {
            let existed: bool = tx
                .query_row(
                    "SELECT 1 FROM notes WHERE vault_id = ?1 AND rel_path = ?2",
                    params![vault_id, rel],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if existed {
                delete_note_in_tx(&tx, vault_id, rel)?;
                removed_count += 1;
                affected_targets.insert(rel.clone());
                stem_candidates.insert(title_from_rel_path(rel).to_ascii_lowercase());
                resolve_unresolved = true;
            }
        }

        for note in &live_notes {
            let prev: Option<(i64, i64, i64, String, bool, String)> = tx
                .query_row(
                    "SELECT mtime_ms, mtime_ns, size_bytes, content_hash, is_bookmarked, aliases_json
                     FROM notes WHERE vault_id = ?1 AND rel_path = ?2",
                    params![vault_id, note.rel_path],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)? != 0,
                            r.get::<_, String>(5)?,
                        ))
                    },
                )
                .optional()?;

            if let Some((ms, ns, sz, ref hash, _, _)) = prev {
                if ms == note.mtime_ms
                    && ns == note.mtime_ns
                    && sz == note.size_bytes
                    && *hash == note.content_hash
                {
                    continue;
                }
            }

            let is_new = prev.is_none();
            let is_bookmarked = prev.as_ref().map(|t| t.4).unwrap_or(false);
            let prev_aliases_json = prev.map(|t| t.5);
            let title = title_from_rel_path(&note.rel_path);
            let folder = folder_from_rel_path(&note.rel_path);
            let parsed = parse_markdown(&note.rel_path, &title, &note.raw);
            let new_aliases_json =
                serde_json::to_string(&parsed.aliases).unwrap_or_else(|_| "[]".into());
            let aliases_changed = prev_aliases_json.as_deref() != Some(new_aliases_json.as_str());

            upsert_note_in_tx(
                &tx,
                vault_id,
                &note.rel_path,
                &title,
                &folder,
                &parsed,
                note.mtime_ms,
                note.mtime_ns,
                note.size_bytes,
                &note.content_hash,
                is_bookmarked,
                true,
            )?;

            updated_count += 1;
            affected_sources.insert(note.rel_path.clone());
            if is_new {
                resolve_unresolved = true;
                stem_candidates.insert(title.to_ascii_lowercase());
            } else if aliases_changed {
                resolve_unresolved = true;
                affected_targets.insert(note.rel_path.clone());
            }
        }

        if updated_count > 0 || removed_count > 0 {
            resolve_affected_links_in_tx(
                &tx,
                vault_id,
                &affected_sources,
                &affected_targets,
                resolve_unresolved,
                &stem_candidates,
            )?;
        }

        let stats = index_stats_on_conn(
            &tx,
            vault,
            candidate_paths.len(),
            updated_count,
            removed_count,
            start.elapsed().as_millis() as u64,
        )?;
        tx.commit()?;

        Ok(PathSyncOutcome {
            stats,
            locked_paths,
        })
    }

    /// Incrementally scans `vault.path` comparing `(mtime_ms, mtime_ns, size_bytes)` and content hash
    /// against SQLite. Skips unreadable/locked files without overwriting their last good index row,
    /// removes deleted notes, and refreshes wikilink resolutions.
    pub fn sync_vault(&self, vault: &VaultProfile) -> Result<IndexStats> {
        self.sync_vault_with_options(vault, false)
    }

    /// Explicit full verification scan (always verifies content hashes even on large vaults).
    pub fn reindex_vault(&self, vault: &VaultProfile) -> Result<IndexStats> {
        self.sync_vault_with_options(vault, true)
    }

    pub fn sync_vault_with_options(
        &self,
        vault: &VaultProfile,
        force_hash_check: bool,
    ) -> Result<IndexStats> {
        let start = Instant::now();
        let disk_entries = scan_vault_markdown_files(&vault.path)?;
        let vault_id = vault.id.as_str();

        let mut conn = self.conn.lock().map_err(|_| anyhow!("Storage lock poisoned"))?;

        // Load existing (rel_path -> (mtime_ms, mtime_ns, size_bytes, content_hash, is_bookmarked))
        let mut existing: HashMap<String, (i64, i64, i64, String, bool)> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT rel_path, mtime_ms, mtime_ns, size_bytes, content_hash, is_bookmarked
                 FROM notes WHERE vault_id = ?1",
            )?;
            let rows = stmt.query_map(params![vault_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)? != 0,
                ))
            })?;
            for r in rows {
                let (rp, mt_ms, mt_ns, sz, hash, bm) = r?;
                existing.insert(rp, (mt_ms, mt_ns, sz, hash, bm));
            }
        }

        let disk_set: HashSet<String> = disk_entries.iter().map(|e| e.rel_path.clone()).collect();
        let removed_paths: Vec<String> = existing
            .keys()
            .filter(|p| !disk_set.contains(*p))
            .cloned()
            .collect();

        let mut to_upsert: Vec<(&crate::vault::fs::DiskNoteEntry, String, String, bool)> =
            Vec::new();
        for entry in &disk_entries {
            match existing.get(&entry.rel_path) {
                Some((mt_ms, mt_ns, sz, stored_hash, is_bm))
                    if *mt_ms == entry.mtime_ms
                        && *mt_ns == entry.mtime_ns
                        && *sz == entry.size_bytes =>
                {
                    if force_hash_check {
                        let Ok(bytes) = fs::read(&entry.abs_path) else {
                            // Unreadable / locked file: keep last successful index intact.
                            continue;
                        };
                        let disk_hash = content_sha256_hex(&bytes);
                        if stored_hash.is_empty() || *stored_hash != disk_hash {
                            let raw = decode_utf8_lossy(bytes);
                            to_upsert.push((entry, raw, disk_hash, *is_bm));
                        }
                    }
                }
                existing_row => {
                    let Ok(bytes) = fs::read(&entry.abs_path) else {
                        // Unreadable / locked file: never poison SQLite with empty content.
                        continue;
                    };
                    let disk_hash = content_sha256_hex(&bytes);
                    let is_bm = existing_row.map(|t| t.4).unwrap_or(false);
                    let raw = decode_utf8_lossy(bytes);
                    to_upsert.push((entry, raw, disk_hash, is_bm));
                }
            }
        }

        let updated_count = to_upsert.len();
        let removed_count = removed_paths.len();
        let needs_link_resolution = updated_count > 0 || removed_count > 0;

        let tx = conn.transaction_with_behavior(if existing.is_empty() {
            TransactionBehavior::Immediate
        } else {
            TransactionBehavior::Deferred
        })?;
        // Recheck under the write lock before clearing FTS rows. Another process may have
        // indexed this vault while we were reading files.
        let first_index = existing.is_empty()
            && !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM notes WHERE vault_id = ?1)",
                params![vault_id],
                |r| r.get::<_, bool>(0),
            )?;
        if first_index {
            tx.execute("DELETE FROM notes_fts WHERE vault_id = ?1", params![vault_id])?;
        }

        // Remove deleted notes
        for rel in &removed_paths {
            delete_note_in_tx(&tx, vault_id, rel)?;
        }

        // Upsert added/modified notes (only files that were successfully read from disk)
        for (entry, raw, disk_hash, is_bookmarked) in &to_upsert {
            let parsed = parse_markdown(&entry.rel_path, &entry.title, raw);
            upsert_note_in_tx(
                &tx,
                vault_id,
                &entry.rel_path,
                &entry.title,
                &entry.folder,
                &parsed,
                entry.mtime_ms,
                entry.mtime_ns,
                entry.size_bytes,
                disk_hash,
                *is_bookmarked,
                !first_index,
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

        let stats = index_stats_on_conn(
            &tx,
            vault,
            disk_entries.len(),
            updated_count,
            removed_count,
            start.elapsed().as_millis() as u64,
        )?;
        tx.commit()?;

        Ok(stats)
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
        let raw_bytes = fs::read(&abs_path)
            .with_context(|| format!("Failed to read note: {}", abs_path.display()))?;
        let raw_markdown = decode_utf8_lossy(raw_bytes);
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
            "SELECT l.source_path, n.title, l.target_raw, l.heading, l.is_embed
             FROM links l
             JOIN notes n ON n.vault_id = l.vault_id AND n.rel_path = l.source_path
             WHERE l.vault_id = ?1 AND l.target_resolved_path = ?2 AND l.source_path != ?2
             ORDER BY LOWER(n.title) ASC, l.id ASC",
        )?;
        // Keep the metadata cursor open while fetching bodies so both reads share its snapshot.
        let mut body_stmt = conn.prepare(
            "SELECT body_plain FROM notes WHERE vault_id = ?1 AND rel_path = ?2",
        )?;
        let rows = stmt.query_map(params![vault_id, target_rel_path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)? != 0,
            ))
        })?;

        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for r in rows {
            let (source_path, source_title, target_raw, heading, is_embed) = r?;
            if !seen.insert(source_path.clone()) {
                continue;
            }
            let body_plain: String =
                body_stmt.query_row(params![vault_id, source_path], |r| r.get(0))?;
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

fn decode_utf8_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn delete_note_in_tx(
    tx: &rusqlite::Transaction<'_>,
    vault_id: &str,
    rel_path: &str,
) -> Result<()> {
    tx.execute(
        "DELETE FROM notes WHERE vault_id = ?1 AND rel_path = ?2",
        params![vault_id, rel_path],
    )?;
    tx.execute(
        "DELETE FROM links WHERE vault_id = ?1 AND source_path = ?2",
        params![vault_id, rel_path],
    )?;
    tx.execute(
        "DELETE FROM tags WHERE vault_id = ?1 AND note_path = ?2",
        params![vault_id, rel_path],
    )?;
    tx.execute(
        "DELETE FROM headings WHERE vault_id = ?1 AND note_path = ?2",
        params![vault_id, rel_path],
    )?;
    tx.execute(
        "DELETE FROM tasks WHERE vault_id = ?1 AND note_path = ?2",
        params![vault_id, rel_path],
    )?;
    tx.execute(
        "DELETE FROM notes_fts WHERE vault_id = ?1 AND rel_path = ?2",
        params![vault_id, rel_path],
    )?;
    Ok(())
}

fn index_stats_on_conn(
    conn: &Connection,
    vault: &VaultProfile,
    scanned: usize,
    updated: usize,
    removed: usize,
    elapsed_ms: u64,
) -> Result<IndexStats> {
    let vault_id = vault.id.as_str();
    let total_notes: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE vault_id = ?1",
            params![vault_id],
            |r| r.get::<_, i64>(0),
        )?
        .max(0) as usize;
    let total_links: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM links WHERE vault_id = ?1",
            params![vault_id],
            |r| r.get::<_, i64>(0),
        )?
        .max(0) as usize;
    let total_tags: usize = conn
        .query_row(
            "SELECT COUNT(DISTINCT tag) FROM tags WHERE vault_id = ?1",
            params![vault_id],
            |r| r.get::<_, i64>(0),
        )?
        .max(0) as usize;
    let total_tasks: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE vault_id = ?1",
            params![vault_id],
            |r| r.get::<_, i64>(0),
        )?
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
    })
}

fn resolve_affected_links_in_tx(
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
        let mut stmt = tx.prepare("SELECT rel_path, aliases_json FROM notes WHERE vault_id = ?1")?;
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

    let mut target_set = affected_targets.clone();
    if !stem_candidates.is_empty() {
        for rp in &all_paths {
            if stem_candidates.contains(&title_from_rel_path(rp).to_ascii_lowercase()) {
                target_set.insert(rp.clone());
            }
        }
    }

    let mut seen_link_ids: HashSet<i64> = HashSet::new();
    let mut link_rows: Vec<(i64, String)> = Vec::new();

    if !affected_sources.is_empty() {
        let mut stmt = tx.prepare(
            "SELECT id, target_raw FROM links WHERE vault_id = ?1 AND source_path = ?2",
        )?;
        for src in affected_sources {
            let rows = stmt.query_map(params![vault_id, src], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            for r in rows {
                let (id, target_raw) = r?;
                if seen_link_ids.insert(id) {
                    link_rows.push((id, target_raw));
                }
            }
        }
    }

    if !target_set.is_empty() {
        let mut stmt = tx.prepare(
            "SELECT id, target_raw FROM links WHERE vault_id = ?1 AND target_resolved_path = ?2",
        )?;
        for tgt in &target_set {
            let rows = stmt.query_map(params![vault_id, tgt], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            for r in rows {
                let (id, target_raw) = r?;
                if seen_link_ids.insert(id) {
                    link_rows.push((id, target_raw));
                }
            }
        }
    }

    if resolve_unresolved {
        let mut stmt = tx.prepare(
            "SELECT id, target_raw FROM links WHERE vault_id = ?1 AND target_resolved_path IS NULL",
        )?;
        let rows = stmt.query_map(params![vault_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        for r in rows {
            let (id, target_raw) = r?;
            if seen_link_ids.insert(id) {
                link_rows.push((id, target_raw));
            }
        }
    }

    if !link_rows.is_empty() {
        let mut upd = tx.prepare("UPDATE links SET target_resolved_path = ?1 WHERE id = ?2")?;
        for (id, target_raw) in link_rows {
            let resolved = resolve_wikilink_target(&target_raw, &all_paths, &aliases_by_path);
            upd.execute(params![resolved, id])?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn upsert_note_in_tx(
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

    tx.execute(
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
            content_hash = excluded.content_hash",
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
    )?;

    tx.execute(
        "DELETE FROM links WHERE vault_id = ?1 AND source_path = ?2",
        params![vault_id, rel_path],
    )?;
    for wl in &parsed.wikilinks {
        tx.execute(
            "INSERT INTO links (
                vault_id, source_path, target_raw, target_resolved_path, alias, heading, is_embed
             ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6)",
            params![
                vault_id,
                rel_path,
                wl.target_raw,
                wl.alias,
                wl.heading,
                i64::from(wl.is_embed),
            ],
        )?;
    }

    tx.execute(
        "DELETE FROM tags WHERE vault_id = ?1 AND note_path = ?2",
        params![vault_id, rel_path],
    )?;
    for tag in &parsed.tags {
        tx.execute(
            "INSERT OR IGNORE INTO tags (vault_id, note_path, tag) VALUES (?1, ?2, ?3)",
            params![vault_id, rel_path, tag],
        )?;
    }

    tx.execute(
        "DELETE FROM headings WHERE vault_id = ?1 AND note_path = ?2",
        params![vault_id, rel_path],
    )?;
    for h in &parsed.headings {
        tx.execute(
            "INSERT INTO headings (vault_id, note_path, level, text, line_no)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![vault_id, rel_path, h.level as i64, h.text, h.line_no as i64],
        )?;
    }

    tx.execute(
        "DELETE FROM tasks WHERE vault_id = ?1 AND note_path = ?2",
        params![vault_id, rel_path],
    )?;
    for t in &parsed.tasks {
        tx.execute(
            "INSERT INTO tasks (vault_id, note_path, line_no, text, checked)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                vault_id,
                rel_path,
                t.line_no as i64,
                t.text,
                i64::from(t.checked),
            ],
        )?;
    }

    if replace_fts {
        tx.execute(
            "DELETE FROM notes_fts WHERE vault_id = ?1 AND rel_path = ?2",
            params![vault_id, rel_path],
        )?;
    }
    tx.execute(
        "INSERT INTO notes_fts (vault_id, rel_path, title, rel_path_text, aliases, tags, body_plain)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            vault_id,
            rel_path,
            title,
            rel_path,
            aliases_text,
            tags_text,
            parsed.body_plain,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{fs::file_mtime_and_size, vault_from_dir};

    #[test]
    fn backlinks_keep_first_link_order_and_context_for_matching_titles() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("A")).unwrap();
        fs::create_dir(root.join("B")).unwrap();
        for (path, body) in [
            ("Target.md", "---\naliases: [Destination]\n---\n# Target\n\n[[Target]]\n"),
            ("A/Same.md", "See ![[Destination#First|Destination]].\n\n[[Target#Second]]\n\n[[Target#Second]]\n"),
            ("B/Same.md", "See [[Target#Third|Target]].\n\n![[Destination#Fourth]]\n"),
            ("later.md", "See [[Target]].\n"),
        ] {
            fs::write(root.join(path), body).unwrap();
        }
        let vault = vault_from_dir(root, None);
        let storage = Storage::open_in_memory().unwrap();
        storage.sync_vault(&vault).unwrap();

        let links = storage.get_backlinks(&vault.id, "Target.md").unwrap();
        let fields: Vec<_> = links
            .iter()
            .map(|link| {
                (
                    link.source_path.as_str(),
                    link.source_title.as_str(),
                    link.context_snippet.as_str(),
                    link.heading.as_deref(),
                    link.is_embed,
                )
            })
            .collect();
        assert_eq!(
            fields,
            [
                ("later.md", "later", "See Target.", None, false),
                ("A/Same.md", "Same", "See Destination.", Some("First"), true),
                ("B/Same.md", "Same", "See Target.", Some("Third"), false),
            ]
        );
    }

    #[test]
    fn first_index_clears_fts_once_and_keeps_other_vault_searchable() {
        use rusqlite::ffi;
        use std::{
            cell::Cell,
            ffi::{c_int, c_uint, c_void, CStr},
            ptr,
        };

        unsafe extern "C" fn count_deletes(
            _: c_uint,
            context: *mut c_void,
            statement: *mut c_void,
            _: *mut c_void,
        ) -> c_int {
            // SQLITE_TRACE_STMT supplies a live statement and the registered context.
            unsafe {
                let sql = ffi::sqlite3_sql(statement.cast());
                if !sql.is_null()
                    && CStr::from_ptr(sql)
                        .to_bytes()
                        .starts_with(b"DELETE FROM notes_fts WHERE ")
                {
                    let deletes = &*context.cast::<Cell<usize>>();
                    deletes.set(deletes.get() + 1);
                }
            }
            0
        }

        let first = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let vault = vault_from_dir(first.path(), None);
        let other_vault = vault_from_dir(other.path(), None);
        for i in 0..2048 {
            let body = if i == 0 {
                "Anchor firstword"
            } else {
                "Index filler"
            };
            fs::write(first.path().join(format!("Note{i:04}.md")), body).unwrap();
        }
        fs::write(other.path().join("Note0000.md"), "secondvault").unwrap();

        let deletes = Cell::new(0usize);
        let storage = Storage::open_in_memory().unwrap();
        storage.sync_vault(&other_vault).unwrap();
        {
            let conn = storage.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO notes_fts (vault_id, rel_path, body_plain) VALUES (?1, 'Orphan.md', 'stalefts')",
                params![vault.id],
            ).unwrap();
            // The callback is used synchronously by this connection, which drops before `deletes`.
            assert_eq!(
                unsafe {
                    ffi::sqlite3_trace_v2(
                        conn.handle(),
                        ffi::SQLITE_TRACE_STMT as c_uint,
                        Some(count_deletes),
                        ptr::from_ref(&deletes).cast_mut().cast(),
                    )
                },
                ffi::SQLITE_OK
            );
        }
        storage.sync_vault(&vault).unwrap();
        assert_eq!(deletes.get(), 1);

        let fts_count = |vault_id: &str, query: &str| -> usize {
            storage
                .conn
                .lock()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM notes_fts WHERE vault_id = ?1 AND notes_fts MATCH ?2",
                    params![vault_id, query],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let total: usize = storage
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM notes_fts WHERE vault_id = ?1",
                params![vault.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 2048);
        assert_eq!(fts_count(&vault.id, "stalefts"), 0);
        assert_eq!(fts_count(&vault.id, "Anchor"), 1);
        assert_eq!(fts_count(&other_vault.id, "secondvault"), 1);

        let anchor = first.path().join("Note0000.md");
        fs::write(&anchor, "New indexed replacement").unwrap();
        storage.sync_vault(&vault).unwrap();
        assert_eq!(fts_count(&vault.id, "firstword"), 0);
        assert_eq!(fts_count(&vault.id, "replacement"), 1);
        fs::remove_file(anchor).unwrap();
        storage.sync_vault(&vault).unwrap();
        assert_eq!(fts_count(&vault.id, "replacement"), 0);
        assert_eq!(fts_count(&other_vault.id, "secondvault"), 1);
    }

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

    #[test]
    fn test_upsert_single_note_updates_all_tables_preserves_bookmark_and_prevents_redundant_sync() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("Projects")).unwrap();
        let rfc_abs = root.join("Projects/RFC.md");
        fs::write(&rfc_abs, "# Initial Title\n\nOld body.\n").unwrap();
        fs::write(root.join("Target.md"), "# Target\n\nTarget body.\n").unwrap();

        let vault = vault_from_dir(root, Some("FastVault"));
        let storage = Storage::open_in_memory().unwrap();
        storage.sync_vault(&vault).unwrap();

        // Bookmark the note before editing
        assert!(storage.toggle_bookmark(&vault.id, "Projects/RFC.md").unwrap());

        // Write updated content to disk and call upsert_single_note (NO sync_vault!)
        let new_md = "# Fresh Heading\n\nUniqueSearchToken links to [[Target]] with #instanttag\n\n- [ ] Instant task\n";
        fs::write(&rfc_abs, new_md).unwrap();
        let (mtime_ms, size_bytes) = file_mtime_and_size(&rfc_abs);

        storage
            .upsert_single_note(&vault, "Projects/RFC.md", new_md, mtime_ms, size_bytes)
            .unwrap();

        // Verify bookmark preserved + summary updated
        let summary = storage
            .get_note_summary(&vault.id, "Projects/RFC.md")
            .unwrap()
            .unwrap();
        assert!(summary.is_bookmarked, "upsert_single_note must preserve is_bookmarked");
        assert!(summary.tags.iter().any(|t| t == "instanttag"));
        assert_eq!(summary.mtime_ms, mtime_ms);
        assert_eq!(summary.size_bytes, size_bytes);

        // Verify FTS5, headings, tags, tasks, and resolved links all updated
        let hits = storage.search_notes(&vault.id, "UniqueSearchToken", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rel_path, "Projects/RFC.md");

        let headings = storage.get_headings(&vault.id, "Projects/RFC.md").unwrap();
        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].text, "Fresh Heading");

        let tasks = storage
            .list_tasks(&vault.id, TaskFilter::Open, Some("Projects/RFC.md"))
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "Instant task");

        let outgoing = storage.get_outgoing_links(&vault.id, "Projects/RFC.md").unwrap();
        assert_eq!(outgoing.len(), 1);
        assert!(outgoing[0].resolved);
        assert_eq!(outgoing[0].target_resolved_path.as_deref(), Some("Target.md"));

        // Verify subsequent sync_vault sees 0 changed files (suppressing redundant watcher scans!)
        let stats = storage.sync_vault(&vault).unwrap();
        assert_eq!(stats.updated, 0);
        assert_eq!(stats.removed, 0);
    }
}
