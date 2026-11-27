//! SQLite WAL + FTS5 Knowledge Graph Index (`notes`, `links`, `tags`, `headings`, `tasks`, `notes_fts`).
//!
//! Features:
//! - WAL mode + `busy_timeout = 5000`, scoped by `vault_id`
//! - Fast incremental filesystem scanner (`mtime_ms` + `size_bytes` diff)
//! - Instant Backlinks (with snippet context), Outgoing Links, Unresolved Links, Orphans, and Local/Global BFS Graph
//! - FTS5 full-text search with automatic Korean/CJK `<3` char `LIKE` fallback.

mod graph;
mod notes;
mod schema;
mod search;
mod sync;
mod tags_tasks;
mod tx;

#[cfg(test)]
mod tests;

pub use search::should_use_short_cjk_like;

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection};

use crate::{config::ensure_parent, models::IndexStats, vault::fs::content_sha256_hex};

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
    pub(super) conn: Arc<Mutex<Connection>>,
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
}
