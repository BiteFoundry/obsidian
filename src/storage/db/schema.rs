//! SQLite schema creation and index-version migrations.

use anyhow::{anyhow, Context, Result};
use rusqlite::TransactionBehavior;

use super::Storage;

impl Storage {
    pub(super) fn init_schema(&self) -> Result<()> {
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

}
