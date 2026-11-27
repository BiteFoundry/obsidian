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
    fmt::Write as _,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use sha2::{Digest, Sha256};

use crate::{config::ensure_parent, models::IndexStats};

const READER_POOL_SIZE: usize = 4;

/// Indexed note paths paired with frontmatter aliases keyed by `rel_path`.
pub type VaultPathsAndAliases = (Vec<String>, HashMap<String, Vec<String>>);

/// Result of an incremental path-scoped sync batch.
#[derive(Debug, Clone, Default)]
pub struct PathSyncOutcome {
    pub stats: IndexStats,
    pub locked_paths: Vec<String>,
}

pub(super) struct ReaderPool {
    conns: Vec<Mutex<Connection>>,
    next: AtomicUsize,
}

impl ReaderPool {
    fn open(path: &Path) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut conns = Vec::with_capacity(READER_POOL_SIZE);
        for _ in 0..READER_POOL_SIZE {
            let conn = Connection::open_with_flags(path, flags)
                .with_context(|| format!("Failed to open SQLite reader: {}", path.display()))?;
            schema::configure_reader_connection(&conn)?;
            conns.push(Mutex::new(conn));
        }
        Ok(Self {
            conns,
            next: AtomicUsize::new(0),
        })
    }

    fn acquire(&self) -> Result<MutexGuard<'_, Connection>> {
        let len = self.conns.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed) % len;
        for offset in 0..len {
            let idx = (start + offset) % len;
            match self.conns[idx].try_lock() {
                Ok(guard) => return Ok(guard),
                Err(std::sync::TryLockError::WouldBlock) => continue,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(anyhow!("Storage reader lock poisoned"))
                }
            }
        }
        self.conns[start]
            .lock()
            .map_err(|_| anyhow!("Storage reader lock poisoned"))
    }
}

#[derive(Clone)]
pub struct Storage {
    pub(super) conn: Arc<Mutex<Connection>>,
    pub(super) readers: Option<Arc<ReaderPool>>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        ensure_parent(path)?;
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite index: {}", path.display()))?;
        let mut storage = Self {
            conn: Arc::new(Mutex::new(conn)),
            readers: None,
        };
        storage.init_schema()?;
        if let Ok(pool) = ReaderPool::open(path) {
            storage.readers = Some(Arc::new(pool));
        }
        Ok(storage)
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&crate::config::db_path())
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Failed to open in-memory SQLite")?;
        let storage = Self {
            conn: Arc::new(Mutex::new(conn)),
            readers: None,
        };
        storage.init_schema()?;
        Ok(storage)
    }

    pub(super) fn read_conn(&self) -> Result<MutexGuard<'_, Connection>> {
        if let Some(ref pool) = self.readers {
            pool.acquire()
        } else {
            self.conn
                .lock()
                .map_err(|_| anyhow!("Storage lock poisoned"))
        }
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
        let conn = self.read_conn()?;
        let mut stmt = conn.prepare_cached(
            "SELECT rel_path, mtime_ms, mtime_ns, size_bytes, word_count, is_bookmarked, content_hash
             FROM notes
             WHERE vault_id = ?1
             ORDER BY rel_path ASC",
        )?;
        let mut hasher = Sha256::new();
        let mut row_buf = String::with_capacity(256);
        let mut rows = stmt.query(params![vault_id])?;
        while let Some(r) = rows.next()? {
            let rp = r.get_ref(0)?.as_str()?;
            let mt_ms: i64 = r.get(1)?;
            let mt_ns: i64 = r.get(2)?;
            let sz: i64 = r.get(3)?;
            let wc: i64 = r.get(4)?;
            let bm: i64 = r.get(5)?;
            let hash = r.get_ref(6)?.as_str()?;
            row_buf.clear();
            let _ = writeln!(row_buf, "{rp}:{mt_ms}:{mt_ns}:{sz}:{wc}:{bm}:{hash}");
            hasher.update(row_buf.as_bytes());
        }
        Ok(crate::hex_encode(hasher.finalize()))
    }

    /// Marks a single note's cached `mtime_ms` stale so the next `sync_vault` unconditionally re-indexes it.
    pub fn invalidate_note_mtime(&self, vault_id: &str, rel_path: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        conn.prepare_cached(
            "UPDATE notes SET mtime_ms = -1, mtime_ns = -1, content_hash = '' WHERE vault_id = ?1 AND rel_path = ?2",
        )?
        .execute(params![vault_id, rel_path])?;
        Ok(())
    }
}
