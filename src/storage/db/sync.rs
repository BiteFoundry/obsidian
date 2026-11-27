//! Incremental and full vault filesystem indexing into SQLite.

use std::{
    collections::{HashMap, HashSet},
    fs,
    time::Instant,
};

use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension, TransactionBehavior};

use super::{
    tx::{
        compute_removed_paths, decode_utf8_lossy, delete_note_in_tx, index_stats_on_conn,
        insert_alias_names, resolve_affected_links_in_tx, resolve_all_links_in_tx,
        upsert_note_in_tx,
    },
    PathSyncOutcome, Storage,
};
use crate::{
    config::VaultProfile,
    models::{IndexStats, ParsedNote},
    vault::{
        fs::{
            canonical_rel_path_in_vault, content_sha256_hex, file_mtime_ns_and_size,
            folder_from_rel_path, has_symlink_component_in_vault, safe_vault_path,
            scan_vault_markdown_files_report, title_from_rel_path,
        },
        parser::parse_markdown,
    },
};

impl Storage {
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
            insert_alias_names(&mut stem_candidates, &parsed.aliases);
        } else if aliases_changed {
            let prev_aliases: Vec<String> = prev_aliases_json
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or_default();
            insert_alias_names(&mut stem_candidates, &prev_aliases);
            insert_alias_names(&mut stem_candidates, &parsed.aliases);
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

        // Snapshot existing metadata briefly, then release the SQLite lock before doing
        // filesystem reads, SHA-256 hashing, and Markdown parsing so concurrent readers stay fast.
        let mut existing: HashMap<String, (i64, i64, i64, String, bool, String)> = HashMap::new();
        {
            let conn = self
                .conn
                .lock()
                .map_err(|_| anyhow!("Storage lock poisoned"))?;
            let mut stmt = conn.prepare_cached(
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

        // Read, hash, and parse changed files completely outside the SQLite lock/transaction.
        struct PreparedNote<'a> {
            entry: &'a crate::vault::fs::DiskNoteEntry,
            parsed: ParsedNote,
            disk_hash: String,
            is_bookmarked: bool,
            prev_aliases: Option<&'a str>,
        }
        let mut to_upsert: Vec<PreparedNote<'_>> = Vec::new();
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
                            let parsed = parse_markdown(&entry.rel_path, &entry.title, &raw);
                            to_upsert.push(PreparedNote {
                                entry,
                                parsed,
                                disk_hash,
                                is_bookmarked: *is_bm,
                                prev_aliases: Some(aliases.as_str()),
                            });
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
                    let parsed = parse_markdown(&entry.rel_path, &entry.title, &raw);
                    to_upsert.push(PreparedNote {
                        entry,
                        parsed,
                        disk_hash,
                        is_bookmarked: is_bm,
                        prev_aliases,
                    });
                }
            }
        }

        let updated_count = to_upsert.len();
        let removed_count = removed_paths.len();
        let needs_link_resolution = updated_count > 0 || removed_count > 0;

        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;

        // Fast no-op path: nothing changed and not a first index or forced repair.
        if !needs_link_resolution && !force_hash_check && !existing.is_empty() {
            let mut stats = index_stats_on_conn(
                &conn,
                vault,
                disk_entries.len(),
                0,
                0,
                start.elapsed().as_millis() as u64,
            )?;
            stats.unreadable_dirs = scan.unreadable_dirs.len();
            return Ok(stats);
        }

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
        for item in &to_upsert {
            let entry = item.entry;
            upsert_note_in_tx(
                &tx,
                vault_id,
                &entry.rel_path,
                &entry.title,
                &entry.folder,
                &item.parsed,
                entry.mtime_ms,
                entry.mtime_ns,
                entry.size_bytes,
                &item.disk_hash,
                item.is_bookmarked,
                !first_index,
            )?;

            affected_sources.insert(entry.rel_path.clone());
            match item.prev_aliases {
                None => {
                    resolve_unresolved = true;
                    name_candidates.insert(entry.title.to_ascii_lowercase());
                    insert_alias_names(&mut name_candidates, &item.parsed.aliases);
                }
                Some(prev_json) => {
                    let new_json =
                        serde_json::to_string(&item.parsed.aliases).unwrap_or_else(|_| "[]".into());
                    if prev_json != new_json {
                        resolve_unresolved = true;
                        affected_targets.insert(entry.rel_path.clone());
                        let prev: Vec<String> = serde_json::from_str(prev_json).unwrap_or_default();
                        insert_alias_names(&mut name_candidates, &prev);
                        insert_alias_names(&mut name_candidates, &item.parsed.aliases);
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

}
