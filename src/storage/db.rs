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
        BacklinkEntry, CodedError, GraphData, GraphEdge, GraphNode, HeadingEntry, IndexStats,
        LinkStatus, ListPage, NoteContent, NoteDetail, NoteMove, NoteOmittedCounts, NoteSummary,
        NoteTextReplacement, OutgoingLinkEntry, ParsedNote, SearchHit, SearchPage, TagCount,
        TaskEntry, TaskFilter,
    },
    vault::{
        fs::{
            canonical_rel_path_in_vault, content_sha256_hex, extract_local_markdown_note_link,
            file_mtime_and_size, file_mtime_ns_and_size, folder_from_rel_path,
            has_symlink_component_in_vault, hydrate_parsed_note_links, is_local_attachment_dest,
            move_note, normalize_rel_path, replace_note_text, resolve_attachment_staged,
            resolve_markdown_link_from_source, resolve_note_identifier,
            resolve_wikilink_from_source, rewrite_wikilinks_for_move, safe_vault_path,
            scan_vault_markdown_files_report, title_from_rel_path, wikilink_candidates_from_source,
        },
        parser::parse_markdown,
    },
};

/// Indexed note paths paired with frontmatter aliases keyed by `rel_path`.
pub type VaultPathsAndAliases = (Vec<String>, HashMap<String, Vec<String>>);

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
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
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
                is_embed INTEGER NOT NULL DEFAULT 0,
                link_kind TEXT NOT NULL DEFAULT 'wikilink'
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
        let _ = conn.execute(
            "ALTER TABLE links ADD COLUMN link_kind TEXT NOT NULL DEFAULT 'wikilink'",
            [],
        );
        // Schema version bumps trigger one clean rescan when upgrading an older index.db:
        // v1: frontmatter parser, v2: source-relative wikilink, parser boundaries & local Markdown links (OBS-02/05/06).
        const CURRENT_INDEX_VERSION: i64 = 2;
        if conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?
            < CURRENT_INDEX_VERSION
        {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if tx.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?
                < CURRENT_INDEX_VERSION
            {
                tx.execute(
                    "UPDATE notes SET mtime_ms = -1, mtime_ns = -1, content_hash = ''",
                    [],
                )?;
                tx.pragma_update(None, "user_version", CURRENT_INDEX_VERSION)?;
            }
            tx.commit()?;
        }
        Ok(())
    }

    /// Returns SQLite's `PRAGMA data_version`, an O(1) counter that increments whenever
    /// any *other* database connection commits a write to `index.db` (e.g. bookmarks or CLI writes).
    pub fn data_version(&self) -> Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let ver: i64 = conn.query_row("PRAGMA data_version", [], |r| r.get(0))?;
        Ok(ver)
    }

    /// Computes a deterministic fingerprint of the vault's indexed note state in SQLite,
    /// allowing long-lived watchers to detect cross-process updates (e.g. CLI/HTTP/MCP/Daemon).
    pub fn vault_index_fingerprint(&self, vault_id: &str) -> Result<String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
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
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
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

        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
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
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
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
            let clean = rp
                .trim()
                .replace('\\', "/")
                .trim_start_matches('/')
                .to_string();
            if !clean.is_empty() && seen_candidates.insert(clean.clone()) {
                candidate_paths.push(clean);
            }
        }

        // Expand any removed/renamed-away directory prefixes against indexed notes in SQLite.
        if !removed_dir_prefixes.is_empty() {
            let conn = self
                .conn
                .lock()
                .map_err(|_| anyhow!("Storage lock poisoned"))?;
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
                let rows =
                    stmt.query_map(params![vault_id, p, like_pat], |r| r.get::<_, String>(0))?;
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
                let conn = self
                    .conn
                    .lock()
                    .map_err(|_| anyhow!("Storage lock poisoned"))?;
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

        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
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
                insert_alias_names(&mut stem_candidates, &parsed.aliases);
            } else if aliases_changed {
                resolve_unresolved = true;
                affected_targets.insert(note.rel_path.clone());
                let prev: Vec<String> = prev_aliases_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str(json).ok())
                    .unwrap_or_default();
                insert_alias_names(&mut stem_candidates, &prev);
                insert_alias_names(&mut stem_candidates, &parsed.aliases);
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
        let scan = scan_vault_markdown_files_report(&vault.path)?;
        let disk_entries = scan.entries;
        let vault_id = vault.id.as_str();

        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;

        // Load existing (rel_path -> (mtime_ms, mtime_ns, size_bytes, content_hash, is_bookmarked, aliases_json))
        let mut existing: HashMap<String, (i64, i64, i64, String, bool, String)> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT rel_path, mtime_ms, mtime_ns, size_bytes, content_hash, is_bookmarked, aliases_json
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
                    row.get::<_, String>(6)?,
                ))
            })?;
            for r in rows {
                let (rp, mt_ms, mt_ns, sz, hash, bm, aliases) = r?;
                existing.insert(rp, (mt_ms, mt_ns, sz, hash, bm, aliases));
            }
        }

        let disk_set: HashSet<String> = disk_entries.iter().map(|e| e.rel_path.clone()).collect();
        // A directory the scan could not read must not look like a deleted one: rows under it
        // stay as they are (bookmark included) until a later scan can read it again.
        let removed_paths =
            compute_removed_paths(existing.keys(), &disk_set, &scan.unreadable_dirs);

        // (entry, raw markdown, content hash, is_bookmarked, previous aliases_json or None when new)
        let mut to_upsert: Vec<(
            &crate::vault::fs::DiskNoteEntry,
            String,
            String,
            bool,
            Option<&str>,
        )> = Vec::new();
        for entry in &disk_entries {
            match existing.get(&entry.rel_path) {
                Some((mt_ms, mt_ns, sz, stored_hash, is_bm, aliases))
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
                            to_upsert.push((entry, raw, disk_hash, *is_bm, Some(aliases.as_str())));
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
                    let prev_aliases = existing_row.map(|t| t.5.as_str());
                    let raw = decode_utf8_lossy(bytes);
                    to_upsert.push((entry, raw, disk_hash, is_bm, prev_aliases));
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
            tx.execute(
                "DELETE FROM notes_fts WHERE vault_id = ?1",
                params![vault_id],
            )?;
        }

        // Inputs for incremental link resolution (same sets `sync_paths` builds).
        let mut affected_sources: HashSet<String> = HashSet::new();
        let mut affected_targets: HashSet<String> = HashSet::new();
        let mut name_candidates: HashSet<String> = HashSet::new();
        let mut resolve_unresolved = false;

        // Remove deleted notes
        for rel in &removed_paths {
            delete_note_in_tx(&tx, vault_id, rel)?;
            affected_targets.insert(rel.clone());
            name_candidates.insert(title_from_rel_path(rel).to_ascii_lowercase());
            resolve_unresolved = true;
        }

        // Upsert added/modified notes (only files that were successfully read from disk)
        for (entry, raw, disk_hash, is_bookmarked, prev_aliases) in &to_upsert {
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

            affected_sources.insert(entry.rel_path.clone());
            match prev_aliases {
                None => {
                    resolve_unresolved = true;
                    name_candidates.insert(entry.title.to_ascii_lowercase());
                    insert_alias_names(&mut name_candidates, &parsed.aliases);
                }
                Some(prev_json) => {
                    let new_json =
                        serde_json::to_string(&parsed.aliases).unwrap_or_else(|_| "[]".into());
                    if *prev_json != new_json {
                        resolve_unresolved = true;
                        affected_targets.insert(entry.rel_path.clone());
                        let prev: Vec<String> = serde_json::from_str(prev_json).unwrap_or_default();
                        insert_alias_names(&mut name_candidates, &prev);
                        insert_alias_names(&mut name_candidates, &parsed.aliases);
                    }
                }
            }
        }

        if force_hash_check {
            // Explicit reindex is the full repair path: every link is re-resolved even when no
            // file changed.
            resolve_all_links_in_tx(&tx, vault_id)?;
        } else if needs_link_resolution {
            // The first index re-resolves every link, and so does a sync that touches more than
            // a quarter of the vault, where the targeted route has no advantage. Otherwise only
            // the links that can have changed are redone.
            let vault_size = disk_entries.len().max(existing.len());
            let bulk_change = (updated_count + removed_count) * 4 > vault_size;
            if first_index || bulk_change {
                resolve_all_links_in_tx(&tx, vault_id)?;
            } else {
                resolve_affected_links_in_tx(
                    &tx,
                    vault_id,
                    &affected_sources,
                    &affected_targets,
                    resolve_unresolved,
                    &name_candidates,
                )?;
            }
        }

        let mut stats = index_stats_on_conn(
            &tx,
            vault,
            disk_entries.len(),
            updated_count,
            removed_count,
            start.elapsed().as_millis() as u64,
        )?;
        stats.unreadable_dirs = scan.unreadable_dirs.len();
        tx.commit()?;

        Ok(stats)
    }

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
            self.sync_vault(vault)?;
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
        let updated_links_in_notes =
            rewrite_wikilinks_for_move(&vault.path, &from, &to, &paths, &aliases)?;
        self.sync_vault(vault)?;
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
        let NoteContent {
            rel_path,
            title,
            raw_markdown,
            ..
        } = self.load_note_content(vault, note_ref)?;
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
        let outgoing_links = self.get_outgoing_links(vault, &rel_path)?;

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

    /// Full-text search via SQLite FTS5. Korean/CJK queries also run a substring `LIKE` scan
    /// (alone below 3 characters, merged after the FTS hits otherwise), so they match inside
    /// longer words such as `주간회의록` for `회의록`.
    pub fn search_notes(
        &self,
        vault_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let limit = limit.clamp(1, 200);
        self.search_notes_fetch(vault_id, query, limit)
    }

    /// Bounded full-text search reporting the applied limit (1..=200) and whether more matches exist.
    pub fn search_notes_page(
        &self,
        vault_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<SearchPage> {
        let applied = limit.clamp(1, 200);
        let mut hits = self.search_notes_fetch(vault_id, query, applied + 1)?;
        let has_more = hits.len() > applied;
        if has_more {
            hits.truncate(applied);
        }
        Ok(SearchPage {
            limit: applied,
            hits,
            has_more,
        })
    }

    fn search_notes_fetch(
        &self,
        vault_id: &str,
        query: &str,
        fetch: usize,
    ) -> Result<Vec<SearchHit>> {
        let q = query.trim();
        if q.is_empty() {
            let notes = self.list_notes(vault_id, None)?;
            return Ok(notes
                .into_iter()
                .take(fetch)
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

        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let tags_map = load_tags_map(&conn, vault_id)?;

        // Use LIKE fallback when query has <3 chars with Hangul/CJK or very short token
        let use_like_first = should_use_short_cjk_like(q);
        // `unicode61` indexes a run of Hangul/CJK as one token and FTS5 only matches token
        // prefixes, so a CJK query misses words that merely contain it. Longer CJK queries
        // therefore merge the substring scan in after the ranked FTS hits.
        let merge_like = !use_like_first && contains_cjk(q);
        if !use_like_first {
            let fts_query = build_fts5_query(q);
            if !fts_query.is_empty() {
                let fts_res = run_fts5_search(&conn, vault_id, &fts_query, fetch, &tags_map);
                if let Ok(hits) = fts_res {
                    if merge_like {
                        let mut merged = hits;
                        let mut seen: HashSet<String> =
                            merged.iter().map(|h| h.rel_path.clone()).collect();
                        for hit in run_like_search(&conn, vault_id, q, fetch, &tags_map)? {
                            if merged.len() >= fetch {
                                break;
                            }
                            if seen.insert(hit.rel_path.clone()) {
                                merged.push(hit);
                            }
                        }
                        return Ok(merged);
                    }
                    if !hits.is_empty() {
                        return Ok(hits);
                    }
                }
            }
        }

        // Fallback: case-insensitive LIKE over title, rel_path, aliases, tags, body_plain
        run_like_search(&conn, vault_id, q, fetch, &tags_map)
    }

    /// Returns all incoming Linked Mentions (backlinks) to `target_rel_path` with context snippets.
    pub fn get_backlinks(
        &self,
        vault_id: &str,
        target_rel_path: &str,
    ) -> Result<Vec<BacklinkEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let target_title = title_from_rel_path(target_rel_path);
        let mut stmt = conn.prepare(
            "SELECT l.source_path, n.title, l.target_raw, l.heading, l.is_embed
             FROM links l
             JOIN notes n ON n.vault_id = l.vault_id AND n.rel_path = l.source_path
             WHERE l.vault_id = ?1 AND l.target_resolved_path = ?2 AND l.source_path != ?2
             ORDER BY LOWER(n.title) ASC, l.id ASC",
        )?;
        // Keep the metadata cursor open while fetching bodies so both reads share its snapshot.
        let mut body_stmt =
            conn.prepare("SELECT body_plain FROM notes WHERE vault_id = ?1 AND rel_path = ?2")?;
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

    /// Returns all outgoing wikilinks from `source_rel_path` (including attachments, ambiguous, and missing links).
    pub fn get_outgoing_links(
        &self,
        vault: &VaultProfile,
        source_rel_path: &str,
    ) -> Result<Vec<OutgoingLinkEntry>> {
        struct OutgoingLinkRow {
            target_raw: String,
            resolved_path: Option<String>,
            resolved_title: Option<String>,
            alias: Option<String>,
            heading: Option<String>,
            is_embed: bool,
            link_kind: String,
        }

        let rows: Vec<OutgoingLinkRow> = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| anyhow!("Storage lock poisoned"))?;
            let mut stmt = conn.prepare(
                "SELECT l.target_raw, l.target_resolved_path, n.title, l.alias, l.heading, l.is_embed, l.link_kind
                 FROM links l
                 LEFT JOIN notes n ON n.vault_id = l.vault_id AND n.rel_path = l.target_resolved_path
                 WHERE l.vault_id = ?1 AND l.source_path = ?2
                 ORDER BY l.id ASC",
            )?;
            let mapped = stmt.query_map(params![vault.id, source_rel_path], |r| {
                Ok(OutgoingLinkRow {
                    target_raw: r.get(0)?,
                    resolved_path: r.get(1)?,
                    resolved_title: r.get(2)?,
                    alias: r.get(3)?,
                    heading: r.get(4)?,
                    is_embed: r.get::<_, i64>(5)? != 0,
                    link_kind: r.get(6)?,
                })
            })?;
            let mut collected = Vec::new();
            for r in mapped {
                collected.push(r?);
            }
            collected
        };

        let has_unresolved = rows.iter().any(|row| row.resolved_path.is_none());
        let all_notes = if has_unresolved {
            Some(self.all_paths_and_aliases(&vault.id)?)
        } else {
            None
        };

        let mut out = Vec::with_capacity(rows.len());
        for OutgoingLinkRow {
            target_raw,
            resolved_path,
            resolved_title,
            alias,
            heading,
            is_embed,
            link_kind,
        } in rows
        {
            let target_title = resolved_title.unwrap_or_else(|| target_raw.clone());
            if let Some(rp) = resolved_path {
                out.push(OutgoingLinkEntry {
                    target_raw,
                    target_resolved_path: Some(rp),
                    target_title,
                    alias,
                    heading,
                    is_embed,
                    resolved: true,
                    status: LinkStatus::Resolved,
                    candidates: Vec::new(),
                });
                continue;
            }

            if link_kind == "wikilink" && is_local_attachment_dest(&target_raw) {
                if let Some((abs, _)) =
                    resolve_attachment_staged(&vault.path, source_rel_path, &target_raw)
                {
                    if let Ok(rel) = abs.strip_prefix(&vault.path) {
                        out.push(OutgoingLinkEntry {
                            target_raw,
                            target_resolved_path: Some(normalize_rel_path(rel)),
                            target_title,
                            alias,
                            heading,
                            is_embed,
                            resolved: true,
                            status: LinkStatus::Attachment,
                            candidates: Vec::new(),
                        });
                        continue;
                    }
                }
            }

            if link_kind == "wikilink" {
                if let Some((ref all_paths, ref aliases_map)) = all_notes {
                    let mut cands = wikilink_candidates_from_source(
                        &target_raw,
                        source_rel_path,
                        all_paths,
                        aliases_map,
                    );
                    if cands.len() >= 2 {
                        cands.truncate(10);
                        out.push(OutgoingLinkEntry {
                            target_raw,
                            target_resolved_path: None,
                            target_title,
                            alias,
                            heading,
                            is_embed,
                            resolved: false,
                            status: LinkStatus::Ambiguous,
                            candidates: cands,
                        });
                        continue;
                    }
                }
            }

            out.push(OutgoingLinkEntry {
                target_raw,
                target_resolved_path: None,
                target_title,
                alias,
                heading,
                is_embed,
                resolved: false,
                status: LinkStatus::Missing,
                candidates: Vec::new(),
            });
        }
        Ok(out)
    }

    /// Builds a Local (BFS around `center_rel_path` up to `depth`) or Global (`center_rel_path == None`) Knowledge Graph.
    pub fn get_graph(
        &self,
        vault: &VaultProfile,
        center_rel_path: Option<&str>,
        depth: usize,
    ) -> Result<GraphData> {
        let depth = depth.clamp(1, 5);
        let (all_paths, aliases_map) = self.all_paths_and_aliases(&vault.id)?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let vault_id = vault.id.as_str();
        let tags_map = load_tags_map(&conn, vault_id)?;

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

        // Load all edges (excluding image and existing local attachment embeds so graph shows note-to-note connections)
        let mut all_edges: Vec<(String, String, bool, bool)> = Vec::new();
        let mut unresolved_status: HashMap<String, LinkStatus> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT source_path, target_raw, target_resolved_path, is_embed, link_kind
                 FROM links WHERE vault_id = ?1
                 ORDER BY id ASC",
            )?;
            let rows = stmt.query_map(params![vault_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)? != 0,
                    r.get::<_, String>(4)?,
                ))
            })?;
            for r in rows {
                let (src, raw, resolved_opt, is_embed, link_kind) = r?;
                if crate::vault::parser::is_image_target(&raw) {
                    continue;
                }
                let (target_id, is_resolved) = match resolved_opt {
                    Some(rp) => (rp, true),
                    None => {
                        if is_local_attachment_dest(&raw)
                            && resolve_attachment_staged(&vault.path, &src, &raw).is_some()
                        {
                            continue;
                        }
                        let target_id = format!("unresolved:{raw}");
                        let is_ambiguous = link_kind == "wikilink"
                            && wikilink_candidates_from_source(
                                &raw,
                                &src,
                                &all_paths,
                                &aliases_map,
                            )
                            .len()
                                >= 2;
                        let entry = unresolved_status
                            .entry(target_id.clone())
                            .or_insert(LinkStatus::Missing);
                        if is_ambiguous {
                            *entry = LinkStatus::Ambiguous;
                        }
                        (target_id, false)
                    }
                };
                if src == target_id {
                    continue;
                }
                all_edges.push((src, target_id, is_embed, is_resolved));
            }
        }

        let mut in_deg: HashMap<String, usize> = HashMap::new();
        let mut out_deg: HashMap<String, usize> = HashMap::new();
        let mut counted: HashSet<(&str, &str)> = HashSet::new();
        for (src, dst, _, _) in &all_edges {
            if counted.insert((src.as_str(), dst.as_str())) {
                *out_deg.entry(src.clone()).or_default() += 1;
                *in_deg.entry(dst.clone()).or_default() += 1;
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
            let max_depth = depth;
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
                    status: LinkStatus::Resolved,
                    depth: *d,
                    in_degree: *in_deg.get(id).unwrap_or(&0),
                    out_degree: *out_deg.get(id).unwrap_or(&0),
                    tags: tags_map.get(id).cloned().unwrap_or_default(),
                });
            } else {
                let raw_name = id.strip_prefix("unresolved:").unwrap_or(id).to_string();
                let status = unresolved_status
                    .get(id)
                    .copied()
                    .unwrap_or(LinkStatus::Missing);
                nodes.push(GraphNode {
                    id: id.clone(),
                    title: raw_name,
                    folder: String::new(),
                    resolved: false,
                    status,
                    depth: *d,
                    in_degree: *in_deg.get(id).unwrap_or(&0),
                    out_degree: 0,
                    tags: Vec::new(),
                });
            }
        }

        nodes.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then_with(|| (b.in_degree + b.out_degree).cmp(&(a.in_degree + a.out_degree)))
                .then_with(|| {
                    a.title
                        .to_ascii_lowercase()
                        .cmp(&b.title.to_ascii_lowercase())
                })
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut edge_set = HashSet::new();
        let mut edges = Vec::new();
        for (src, dst, is_embed, _) in all_edges {
            if node_depths.contains_key(&src)
                && node_depths.contains_key(&dst)
                && edge_set.insert((src.clone(), dst.clone()))
            {
                edges.push(GraphEdge {
                    source: src,
                    target: dst,
                    is_embed,
                });
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
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
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
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT t.note_path, n.title, t.line_no, t.text, t.checked
             FROM tasks t
             JOIN notes n ON n.vault_id = t.vault_id AND n.rel_path = t.note_path
             WHERE t.vault_id = ?1
             ORDER BY t.checked ASC, LOWER(n.title) ASC, t.note_path ASC, t.line_no ASC",
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

    /// Lists tasks using deterministic keyset pagination.
    pub fn list_tasks_page(
        &self,
        vault_id: &str,
        filter: TaskFilter,
        note_rel_path: Option<&str>,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<ListPage<TaskEntry>> {
        let after = cursor.map(parse_task_cursor).transpose()?;
        let all = self.list_tasks(vault_id, filter, note_rel_path)?;
        let keyed: Vec<((bool, String, String, usize), TaskEntry)> = all
            .into_iter()
            .map(|t| {
                let key = (
                    t.checked,
                    t.note_title.to_ascii_lowercase(),
                    t.note_path.clone(),
                    t.line_no,
                );
                (key, t)
            })
            .collect();
        Ok(page_after(keyed, after, limit, |t| {
            format!("{}:{}:{}", usize::from(t.checked), t.line_no, t.note_path)
        }))
    }

    /// Returns headings outline for a specific note.
    pub fn get_headings(&self, vault_id: &str, rel_path: &str) -> Result<Vec<HeadingEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
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

fn page_after<T, K: Ord>(
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

fn parse_note_cursor(raw: &str) -> Result<(String, String, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CodedError::new("invalid_argument", format!("Invalid cursor: '{raw}'")).into());
    }
    let folder = folder_from_rel_path(trimmed).to_ascii_lowercase();
    let title = title_from_rel_path(trimmed).to_ascii_lowercase();
    Ok((folder, title, trimmed.to_string()))
}

fn parse_task_cursor(raw: &str) -> Result<(bool, String, String, usize)> {
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

fn load_tags_map(conn: &Connection, vault_id: &str) -> Result<HashMap<String, Vec<String>>> {
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
    contains_cjk(trimmed) && char_count < 3
}

/// True when `s` contains Hangul, Han, Hiragana or Katakana characters.
fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        ('\u{AC00}'..='\u{D7A3}').contains(&c)
            || ('\u{1100}'..='\u{11FF}').contains(&c)
            || ('\u{3130}'..='\u{318F}').contains(&c)
            || ('\u{4E00}'..='\u{9FFF}').contains(&c)
            || ('\u{3040}'..='\u{30FF}').contains(&c)
    })
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
         ORDER BY bm25(notes_fts, 0.0, 0.0, 6.0, 2.0, 4.0, 3.0, 1.0) ASC, f.rel_path ASC
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

fn escape_like_pattern(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn run_like_search(
    conn: &Connection,
    vault_id: &str,
    query: &str,
    limit: usize,
    tags_map: &HashMap<String, Vec<String>>,
) -> Result<Vec<SearchHit>> {
    let pattern = format!("%{}%", escape_like_pattern(&query.to_lowercase()));
    let mut stmt = conn.prepare(
        "SELECT n.rel_path, n.title, n.folder, n.body_plain, n.word_count, n.mtime_ms
         FROM notes n
         WHERE n.vault_id = ?1
           AND (
             LOWER(n.title) LIKE ?2 ESCAPE '\\'
             OR LOWER(n.rel_path) LIKE ?2 ESCAPE '\\'
             OR LOWER(n.aliases_json) LIKE ?2 ESCAPE '\\'
             OR LOWER(n.body_plain) LIKE ?2 ESCAPE '\\'
             OR EXISTS (
                SELECT 1 FROM tags t
                WHERE t.vault_id = n.vault_id AND t.note_path = n.rel_path AND LOWER(t.tag) LIKE ?2 ESCAPE '\\'
             )
           )
         ORDER BY
           CASE WHEN LOWER(n.title) LIKE ?2 ESCAPE '\\' THEN 0 ELSE 1 END,
           n.mtime_ms DESC,
           n.rel_path ASC
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
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn delete_note_in_tx(tx: &rusqlite::Transaction<'_>, vault_id: &str, rel_path: &str) -> Result<()> {
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
        unreadable_dirs: 0,
    })
}

/// Adds each alias, trimmed and ASCII-lower-cased as the resolver compares them, to `names`.
fn insert_alias_names(names: &mut HashSet<String>, aliases: &[String]) {
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
fn compute_removed_paths<'a>(
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

/// Re-resolves every link of the vault against every note path: O(links x notes).
/// Used for the first index, explicit reindex and bulk changes.
fn resolve_all_links_in_tx(tx: &rusqlite::Transaction<'_>, vault_id: &str) -> Result<()> {
    let mut all_paths: Vec<String> = Vec::new();
    let mut aliases_by_path: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut stmt =
            tx.prepare("SELECT rel_path, aliases_json FROM notes WHERE vault_id = ?1")?;
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

    let mut link_rows: Vec<(i64, String, String, String)> = Vec::new();
    {
        let mut stmt = tx.prepare(
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

    let mut upd = tx.prepare("UPDATE links SET target_resolved_path = ?1 WHERE id = ?2")?;
    for (id, source_path, target_raw, link_kind) in link_rows {
        let resolved = if link_kind == "markdown" {
            resolve_markdown_link_from_source(&target_raw, &source_path, &all_paths)
        } else {
            resolve_wikilink_from_source(&target_raw, &source_path, &all_paths, &aliases_by_path)
        };
        upd.execute(params![resolved, id])?;
    }
    Ok(())
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
        let mut stmt =
            tx.prepare("SELECT rel_path, aliases_json FROM notes WHERE vault_id = ?1")?;
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
    let mut link_rows: Vec<(i64, String, String, String)> = Vec::new();

    if !affected_sources.is_empty() {
        let mut stmt = tx.prepare(
            "SELECT id, source_path, target_raw, link_kind FROM links WHERE vault_id = ?1 AND source_path = ?2",
        )?;
        for src in affected_sources {
            let rows = stmt.query_map(params![vault_id, src], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for r in rows {
                let (id, source_path, target_raw, link_kind) = r?;
                if seen_link_ids.insert(id) {
                    link_rows.push((id, source_path, target_raw, link_kind));
                }
            }
        }
    }

    if !target_set.is_empty() {
        let mut stmt = tx.prepare(
            "SELECT id, source_path, target_raw, link_kind FROM links WHERE vault_id = ?1 AND target_resolved_path = ?2",
        )?;
        for tgt in &target_set {
            let rows = stmt.query_map(params![vault_id, tgt], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for r in rows {
                let (id, source_path, target_raw, link_kind) = r?;
                if seen_link_ids.insert(id) {
                    link_rows.push((id, source_path, target_raw, link_kind));
                }
            }
        }
    }

    if resolve_unresolved {
        let mut stmt = tx.prepare(
            "SELECT id, source_path, target_raw, link_kind FROM links WHERE vault_id = ?1 AND target_resolved_path IS NULL",
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
            let (id, source_path, target_raw, link_kind) = r?;
            if seen_link_ids.insert(id) {
                link_rows.push((id, source_path, target_raw, link_kind));
            }
        }
    }

    if !link_rows.is_empty() {
        let mut upd = tx.prepare("UPDATE links SET target_resolved_path = ?1 WHERE id = ?2")?;
        for (id, source_path, target_raw, link_kind) in link_rows {
            let resolved = if link_kind == "markdown" {
                resolve_markdown_link_from_source(&target_raw, &source_path, &all_paths)
            } else {
                resolve_wikilink_from_source(
                    &target_raw,
                    &source_path,
                    &all_paths,
                    &aliases_by_path,
                )
            };
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
                vault_id, source_path, target_raw, target_resolved_path, alias, heading, is_embed, link_kind
             ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, 'wikilink')",
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
    for ml in &parsed.md_links {
        if ml.is_image {
            continue;
        }
        if let Some((target_raw, heading)) = extract_local_markdown_note_link(&ml.url) {
            let alias = (!ml.label.trim().is_empty()).then(|| ml.label.trim().to_string());
            tx.execute(
                "INSERT INTO links (
                    vault_id, source_path, target_raw, target_resolved_path, alias, heading, is_embed, link_kind
                 ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, 0, 'markdown')",
                params![vault_id, rel_path, target_raw, alias, heading],
            )?;
        }
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
    use crate::vault::vault_from_dir;

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
        fs::write(
            root.join("Orphan.md"),
            "# Lone Note\n\nNothing links here.\n",
        )
        .unwrap();

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
            .get_graph(&vault, Some("Projects/RFC.md"), 2)
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
        assert!(storage
            .toggle_bookmark(&vault.id, "Projects/RFC.md")
            .unwrap());

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
        assert!(
            summary.is_bookmarked,
            "upsert_single_note must preserve is_bookmarked"
        );
        assert!(summary.tags.iter().any(|t| t == "instanttag"));
        assert_eq!(summary.mtime_ms, mtime_ms);
        assert_eq!(summary.size_bytes, size_bytes);

        // Verify FTS5, headings, tags, tasks, and resolved links all updated
        let hits = storage
            .search_notes(&vault.id, "UniqueSearchToken", 10)
            .unwrap();
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

        let outgoing = storage
            .get_outgoing_links(&vault, "Projects/RFC.md")
            .unwrap();
        assert_eq!(outgoing.len(), 1);
        assert!(outgoing[0].resolved);
        assert_eq!(
            outgoing[0].target_resolved_path.as_deref(),
            Some("Target.md")
        );

        // Verify subsequent sync_vault sees 0 changed files (suppressing redundant watcher scans!)
        let stats = storage.sync_vault(&vault).unwrap();
        assert_eq!(stats.updated, 0);
        assert_eq!(stats.removed, 0);
    }

    fn obs10_vault(
        notes: &[(&str, &str)],
    ) -> (tempfile::TempDir, crate::config::VaultProfile, Storage) {
        let tmp = tempfile::tempdir().unwrap();
        for (path, body) in notes {
            fs::write(tmp.path().join(path), body).unwrap();
        }
        let vault = vault_from_dir(tmp.path(), Some("obs10"));
        let storage = Storage::open_in_memory().unwrap();
        storage.sync_vault(&vault).unwrap();
        (tmp, vault, storage)
    }

    const OBS10_KOREAN: &[(&str, &str)] = &[
        ("a-minutes.md", "회의록 작성\n"),
        ("b-weekly.md", "주간회의록\n"),
        ("c-team.md", "팀회의록\n"),
        ("d-other.md", "관계없는 메모\n"),
    ];

    fn obs10_paths(hits: &[crate::models::SearchHit]) -> Vec<&str> {
        hits.iter().map(|h| h.rel_path.as_str()).collect()
    }

    #[test]
    fn obs10_korean_three_char_query_recalls_3_of_3() {
        let (_tmp, vault, storage) = obs10_vault(OBS10_KOREAN);
        for query in ["회의록", "회의"] {
            let hits = storage.search_notes(&vault.id, query, 25).unwrap();
            let mut paths = obs10_paths(&hits);
            paths.sort_unstable();
            assert_eq!(
                paths,
                ["a-minutes.md", "b-weekly.md", "c-team.md"],
                "{query}"
            );
        }
    }

    #[test]
    fn obs10_cjk_merge_reports_has_more() {
        let (_tmp, vault, storage) = obs10_vault(OBS10_KOREAN);
        let page = storage.search_notes_page(&vault.id, "회의록", 2).unwrap();
        assert_eq!(page.hits.len(), 2);
        assert!(page.has_more);
        let page = storage.search_notes_page(&vault.id, "회의록", 3).unwrap();
        assert_eq!(page.hits.len(), 3);
        assert!(!page.has_more);
        // The limit bounds the merged list, never more than asked for.
        assert_eq!(
            storage.search_notes(&vault.id, "회의록", 1).unwrap().len(),
            1
        );
    }

    #[test]
    fn obs10_cjk_merge_keeps_fts_hits_first() {
        let (_tmp, vault, storage) = obs10_vault(OBS10_KOREAN);
        let hits = storage.search_notes(&vault.id, "회의록", 25).unwrap();
        assert_eq!(hits[0].rel_path, "a-minutes.md");
        let limited = storage.search_notes(&vault.id, "회의록", 1).unwrap();
        assert_eq!(obs10_paths(&limited), ["a-minutes.md"]);
    }

    #[test]
    fn obs10_japanese_and_han_substring() {
        let (_tmp, vault, storage) = obs10_vault(&[
            ("weekly.md", "週次議事録\n"),
            ("kana.md", "まいにちのメモ帳です\n"),
            ("other.md", "unrelated\n"),
        ]);
        let hits = storage.search_notes(&vault.id, "議事録", 25).unwrap();
        assert_eq!(obs10_paths(&hits), ["weekly.md"]);
        let hits = storage.search_notes(&vault.id, "メモ帳", 25).unwrap();
        assert_eq!(obs10_paths(&hits), ["kana.md"]);
        // LIKE wildcards in a CJK query stay literal.
        assert!(storage
            .search_notes(&vault.id, "議%録", 25)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn obs10_ascii_query_path_unchanged() {
        let (_tmp, vault, storage) = obs10_vault(&[
            ("first.md", "meeting notes\n"),
            ("second.md", "premeeting\n"),
            ("third.md", "ÜBER uns\n"),
        ]);
        // The FTS hit suppresses the substring scan for non-CJK queries, as before.
        let hits = storage.search_notes(&vault.id, "meeting", 25).unwrap();
        assert_eq!(obs10_paths(&hits), ["first.md"]);
        let hits = storage.search_notes(&vault.id, "über", 25).unwrap();
        assert_eq!(obs10_paths(&hits), ["third.md"]);
        assert!(!should_use_short_cjk_like("meeting"));
        assert!(should_use_short_cjk_like("회의"));
        assert!(!should_use_short_cjk_like("회의록"));
    }

    #[test]
    fn obs05_compute_removed_paths_protects_unreadable_prefixes() {
        let existing: Vec<String> = ["A/x.md", "A/B/y.md", "AB/x.md", "C/z.md", "r.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let disk: HashSet<String> = ["r.md".to_string()].into_iter().collect();
        let removed = |unreadable: &[&str]| {
            let dirs: Vec<String> = unreadable.iter().map(|s| s.to_string()).collect();
            compute_removed_paths(existing.iter(), &disk, &dirs)
        };

        // Nothing unreadable: every indexed path missing from disk is removed.
        assert_eq!(removed(&[]), ["A/B/y.md", "A/x.md", "AB/x.md", "C/z.md"]);
        // `A` protects `A/...` at any depth, but not the sibling `AB`.
        assert_eq!(removed(&["A"]), ["AB/x.md", "C/z.md"]);
        assert_eq!(removed(&["A/B"]), ["A/x.md", "AB/x.md", "C/z.md"]);
        // An unreadable vault root protects everything.
        assert!(removed(&[""]).is_empty());
        if cfg!(any(windows, target_os = "macos")) {
            assert_eq!(removed(&["a"]), ["AB/x.md", "C/z.md"]);
        } else {
            assert_eq!(removed(&["a"]), ["A/B/y.md", "A/x.md", "AB/x.md", "C/z.md"]);
        }
    }

    fn obs05_locked_vault() -> (tempfile::TempDir, crate::config::VaultProfile, Storage) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("Locked/Deep")).unwrap();
        fs::write(root.join("Locked/n.md"), "# n\n\n[[Open]]\n").unwrap();
        fs::write(root.join("Locked/Deep/d.md"), "# d\n").unwrap();
        fs::write(root.join("Open.md"), "# Open\n").unwrap();
        let vault = vault_from_dir(root, Some("obs05"));
        let storage = Storage::open_in_memory().unwrap();
        let first = storage.sync_vault(&vault).unwrap();
        assert_eq!(first.total_notes, 3);
        assert_eq!(first.unreadable_dirs, 0);
        assert!(!serde_json::to_string(&first)
            .unwrap()
            .contains("unreadable_dirs"));
        assert!(storage.toggle_bookmark(&vault.id, "Locked/n.md").unwrap());
        (tmp, vault, storage)
    }

    fn obs05_assert_rows_survive_unreadable_dir(
        vault: &crate::config::VaultProfile,
        storage: &Storage,
    ) {
        let stats = storage.sync_vault(vault).unwrap();
        assert_eq!(stats.unreadable_dirs, 1);
        assert_eq!(stats.removed, 0);
        assert_eq!(stats.total_notes, 3);
        assert!(serde_json::to_string(&stats)
            .unwrap()
            .contains("\"unreadable_dirs\":1"));
        let notes = storage.list_notes(&vault.id, None).unwrap();
        let locked = notes
            .iter()
            .find(|n| n.rel_path == "Locked/n.md")
            .expect("row under the unreadable directory is kept");
        assert!(locked.is_bookmarked);
        assert!(notes.iter().any(|n| n.rel_path == "Locked/Deep/d.md"));
        // The explicit reindex honours the same protection.
        let reindexed = storage.reindex_vault(vault).unwrap();
        assert_eq!(reindexed.removed, 0);
        assert_eq!(reindexed.total_notes, 3);
    }

    #[cfg(unix)]
    #[test]
    fn obs05_permission_denied_dir_keeps_rows_and_bookmark() {
        use std::os::unix::fs::PermissionsExt;

        struct Restore(std::path::PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }

        let (tmp, vault, storage) = obs05_locked_vault();
        let locked = tmp.path().join("Locked");
        let _restore = Restore(locked.clone());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&locked).is_ok() {
            eprintln!(
                "obs05_permission_denied_dir_keeps_rows_and_bookmark: skipped (running as root)"
            );
            return;
        }

        obs05_assert_rows_survive_unreadable_dir(&vault, &storage);

        // Once the directory is readable again, a real deletion is reconciled.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(locked.join("n.md")).unwrap();
        let stats = storage.sync_vault(&vault).unwrap();
        assert_eq!(stats.unreadable_dirs, 0);
        assert_eq!(stats.removed, 1);
        assert_eq!(stats.total_notes, 2);
    }

    #[cfg(windows)]
    #[test]
    fn obs05_access_denied_dir_keeps_rows_and_bookmark() {
        // Deny "list folder" to Everyone (S-1-1-0) on the directory itself.
        struct Restore(std::path::PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::process::Command::new("icacls")
                    .arg(&self.0)
                    .args(["/remove:d", "*S-1-1-0"])
                    .output();
            }
        }

        let (tmp, vault, storage) = obs05_locked_vault();
        let locked = tmp.path().join("Locked");
        let _restore = Restore(locked.clone());
        let denied = std::process::Command::new("icacls")
            .arg(&locked)
            .args(["/deny", "*S-1-1-0:(RD)"])
            .output();
        if !denied.is_ok_and(|out| out.status.success()) || fs::read_dir(&locked).is_ok() {
            eprintln!("obs05_access_denied_dir_keeps_rows_and_bookmark: skipped (cannot deny directory listing here)");
            return;
        }

        obs05_assert_rows_survive_unreadable_dir(&vault, &storage);

        // Once the directory is readable again, a real deletion is reconciled.
        drop(_restore);
        fs::remove_file(locked.join("n.md")).unwrap();
        let stats = storage.sync_vault(&vault).unwrap();
        assert_eq!(stats.unreadable_dirs, 0);
        assert_eq!(stats.removed, 1);
        assert_eq!(stats.total_notes, 2);
    }

    #[test]
    fn obs05_deleted_dir_is_still_pruned() {
        let (tmp, vault, storage) = obs05_locked_vault();
        fs::remove_dir_all(tmp.path().join("Locked")).unwrap();
        let stats = storage.sync_vault(&vault).unwrap();
        assert_eq!(stats.unreadable_dirs, 0);
        assert_eq!(stats.removed, 2);
        assert_eq!(stats.total_notes, 1);
        let notes = storage.list_notes(&vault.id, None).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].rel_path, "Open.md");
    }

    fn obs12_link_snapshot(
        storage: &Storage,
        vault_id: &str,
    ) -> Vec<(String, String, Option<String>)> {
        let conn = storage.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT source_path, target_raw, target_resolved_path FROM links
                 WHERE vault_id = ?1 ORDER BY source_path, target_raw, id",
            )
            .unwrap();
        let rows = stmt
            .query_map(params![vault_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn obs12_incremental_matches_full_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for dir in ["A", "A/Deep", "B", "C"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let write = |rel: &str, body: &str| {
            fs::write(root.join(rel), body).unwrap();
        };
        // 48 filler notes spread over four locations, each linking to neighbours in every style.
        let folders = ["", "A/", "B/", "C/"];
        for i in 0..48usize {
            let folder = folders[i % 4];
            let next = (i + 1) % 48;
            let far = (i + 7) % 48;
            let next_folder = folders[next % 4];
            let mut body = String::new();
            if i % 6 == 0 {
                body.push_str(&format!("---\naliases: [alias-{i:02}]\n---\n"));
            }
            body.push_str(&format!("# N{i:02}\n\n"));
            body.push_str(&format!(
                "Wikilink [[N{next:02}]] and [[{next_folder}N{next:02}|shown]].\n"
            ));
            body.push_str(&format!(
                "Alias [[alias-{:02}]] and far [[N{far:02}#Heading]].\n",
                (i / 6) * 6
            ));
            body.push_str(&format!(
                "Markdown [rooted](/{next_folder}N{next:02}.md) and [bare](N{far:02}.md).\n"
            ));
            body.push_str(
                "Shared [[Dup]], [[Solo]], [[Foo]], [[Ghost]], [[Renamed]] and [[Deep/Solo]].\n",
            );
            write(&format!("{folder}N{i:02}.md"), &body);
        }
        write("A/Dup.md", "# Dup A\n");
        write("B/Dup.md", "# Dup B\n\n[[Dup]] and [[A/Dup]].\n");
        write("A/Deep/Solo.md", "# Solo\n\n[up](../../N00.md) [[N01]]\n");
        write(
            "C/Holder.md",
            "---\naliases: [Foo, Bar]\n---\n# Holder\n\n[[Bar]]\n",
        );
        write("Renamed.md", "# Renamed\n\n[[Holder]]\n");

        let vault = vault_from_dir(root, Some("obs12"));
        let storage = Storage::open_in_memory().unwrap();
        storage.sync_vault(&vault).unwrap();

        // Each step changes one or two files (well under a quarter of the vault), syncs
        // incrementally, then compares against the full re-resolution of a reindex.
        type Step<'a> = (&'a str, Box<dyn Fn() + 'a>);
        let steps: Vec<Step> = vec![
            (
                "edit a body",
                Box::new(|| {
                    write(
                        "N05.md",
                        "# N05\n\n[[N06]] [[Ghost]] [[Dup]] [new](B/N02.md)\n",
                    )
                }),
            ),
            (
                "add a note that resolves a dangling link",
                Box::new(|| write("B/Ghost.md", "# Ghost\n")),
            ),
            (
                "add a shorter path for an existing stem",
                Box::new(|| write("Solo.md", "# Solo root\n")),
            ),
            (
                "add Foo.md while [[Foo]] resolves through an alias",
                Box::new(|| write("Foo.md", "# Foo\n")),
            ),
            (
                "add a second holder of an alias",
                Box::new(|| {
                    write(
                        "B/Second.md",
                        "---\naliases: [alias-06, Bar]\n---\n# Second\n",
                    )
                }),
            ),
            (
                "remove an alias",
                Box::new(|| {
                    write(
                        "C/Holder.md",
                        "---\naliases: [Bar]\n---\n# Holder\n\n[[Bar]]\n",
                    )
                }),
            ),
            (
                "give an existing note a colliding alias",
                Box::new(|| {
                    write(
                        "N10.md",
                        "---\naliases: [alias-12, N11]\n---\n# N10\n\n[[N11]] [[alias-12]]\n",
                    )
                }),
            ),
            (
                "delete a note",
                Box::new(|| fs::remove_file(root.join("Foo.md")).unwrap()),
            ),
            (
                "delete one of two same-stem notes",
                Box::new(|| fs::remove_file(root.join("A/Dup.md")).unwrap()),
            ),
            (
                "rename via delete plus create",
                Box::new(|| {
                    fs::remove_file(root.join("Renamed.md")).unwrap();
                    write("C/Renamed.md", "# Renamed\n\n[[Holder]] [[Solo]]\n");
                }),
            ),
            (
                "add a same-folder note for a bare link",
                Box::new(|| write("A/Dup.md", "# Dup A again\n")),
            ),
            (
                "restore the removed alias",
                Box::new(|| {
                    write(
                        "C/Holder.md",
                        "---\naliases: [Foo, Bar]\n---\n# Holder\n\n[[Bar]] [[Second]]\n",
                    )
                }),
            ),
            (
                "delete the second alias holder",
                Box::new(|| fs::remove_file(root.join("B/Second.md")).unwrap()),
            ),
        ];
        for (label, apply) in &steps {
            apply();
            let stats = storage.sync_vault(&vault).unwrap();
            assert!(
                stats.updated + stats.removed >= 1,
                "{label}: change not detected"
            );
            assert!(
                (stats.updated + stats.removed) * 4 <= stats.total_notes,
                "{label}: step must stay on the incremental route"
            );
            let incremental = obs12_link_snapshot(&storage, &vault.id);
            storage.reindex_vault(&vault).unwrap();
            let full = obs12_link_snapshot(&storage, &vault.id);
            assert!(!full.is_empty());
            let diff: Vec<_> = incremental
                .iter()
                .zip(full.iter())
                .filter(|(a, b)| a != b)
                .take(5)
                .collect();
            assert!(
                incremental.len() == full.len() && diff.is_empty(),
                "{label}: incremental resolution differs from full: {diff:?}"
            );
        }
    }

    fn obs12_seed_ring(root: &Path, notes: usize, links: usize) {
        for i in 0..notes {
            let mut body = format!("# Ring{i:05}\n\n");
            for k in 1..=links {
                body.push_str(&format!("[[Ring{:05}]]\n", (i + k) % notes));
            }
            fs::write(root.join(format!("Ring{i:05}.md")), body).unwrap();
        }
    }

    #[test]
    fn obs12_single_edit_resolves_only_its_links() {
        use rusqlite::ffi;
        use std::{
            cell::Cell,
            ffi::{c_int, c_uint, c_void, CStr},
            ptr,
        };

        unsafe extern "C" fn count_link_updates(
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
                        .starts_with(b"UPDATE links SET target_resolved_path")
                {
                    let updates = &*context.cast::<Cell<usize>>();
                    updates.set(updates.get() + 1);
                }
            }
            0
        }

        const NOTES: usize = 2000;
        const LINKS: usize = 10;
        let tmp = tempfile::tempdir().unwrap();
        obs12_seed_ring(tmp.path(), NOTES, LINKS);
        let vault = vault_from_dir(tmp.path(), Some("obs12-ring"));
        let updates = Cell::new(0usize);
        let storage = Storage::open_in_memory().unwrap();
        let first = storage.sync_vault(&vault).unwrap();
        assert_eq!(first.total_notes, NOTES);
        assert_eq!(first.total_links, NOTES * LINKS);
        {
            let conn = storage.conn.lock().unwrap();
            // The callback is used synchronously by this connection, which drops before `updates`.
            assert_eq!(
                unsafe {
                    ffi::sqlite3_trace_v2(
                        conn.handle(),
                        ffi::SQLITE_TRACE_STMT as c_uint,
                        Some(count_link_updates),
                        ptr::from_ref(&updates).cast_mut().cast(),
                    )
                },
                ffi::SQLITE_OK
            );
        }

        // Edit one note without touching its aliases or its links.
        let edited = tmp.path().join("Ring00000.md");
        let mut body = fs::read_to_string(&edited).unwrap();
        body.push_str("\nOne more paragraph.\n");
        fs::write(&edited, body).unwrap();
        let stats = storage.sync_vault(&vault).unwrap();
        assert_eq!(stats.updated, 1);
        assert!(
            updates.get() >= 1 && updates.get() <= LINKS,
            "one edit re-resolved {} links (vault has {})",
            updates.get(),
            NOTES * LINKS
        );
        let unresolved: usize = storage
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM links WHERE vault_id = ?1 AND target_resolved_path IS NULL",
                params![vault.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unresolved, 0);

        // The explicit reindex stays the full repair path.
        updates.set(0);
        storage.reindex_vault(&vault).unwrap();
        assert_eq!(updates.get(), NOTES * LINKS);
    }

    /// Run by hand: `cargo test --release obs12_large -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn obs12_large_vault_edit_sync_wall_clock() {
        let tmp = tempfile::tempdir().unwrap();
        obs12_seed_ring(tmp.path(), 4000, 10);
        let vault = vault_from_dir(tmp.path(), Some("obs12-large"));
        let storage = Storage::open_in_memory().unwrap();

        let started = Instant::now();
        storage.sync_vault(&vault).unwrap();
        let first_index = started.elapsed();

        let started = Instant::now();
        storage.sync_vault(&vault).unwrap();
        let no_change = started.elapsed();

        let edited = tmp.path().join("Ring00000.md");
        let mut body = fs::read_to_string(&edited).unwrap();
        body.push_str("\nOne more paragraph.\n");
        fs::write(&edited, body).unwrap();
        let started = Instant::now();
        let stats = storage.sync_vault(&vault).unwrap();
        let one_edit = started.elapsed();
        assert_eq!(stats.updated, 1);

        eprintln!(
            "obs12 4000x10: first index {first_index:?}, no-change sync {no_change:?}, one-edit sync {one_edit:?}"
        );
        assert!(
            one_edit < no_change * 5 + std::time::Duration::from_millis(250),
            "one-edit sync {one_edit:?} vs no-change sync {no_change:?}"
        );
    }
}
