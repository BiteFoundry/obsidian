//! SQLite schema creation and index-version migrations.

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::Storage;

pub(super) fn configure_writer_connection(conn: &Connection) -> Result<()> {
    conn.set_prepared_statement_cache_capacity(64);
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA busy_timeout = 5000;
        PRAGMA foreign_keys = ON;
        PRAGMA cache_size = -8000;
        PRAGMA mmap_size = 67108864;
        PRAGMA temp_store = MEMORY;
        PRAGMA journal_size_limit = 67108864;
        ",
    )
    .context("Failed to configure SQLite writer PRAGMAs")?;
    Ok(())
}

pub(super) fn configure_reader_connection(conn: &Connection) -> Result<()> {
    conn.set_prepared_statement_cache_capacity(64);
    conn.execute_batch(
        "
        PRAGMA query_only = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA cache_size = -8000;
        PRAGMA mmap_size = 67108864;
        PRAGMA temp_store = MEMORY;
        ",
    )
    .context("Failed to configure SQLite reader PRAGMAs")?;
    Ok(())
}

impl Storage {
    pub(super) fn init_schema(&self) -> Result<()> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("Storage lock poisoned"))?;
        configure_writer_connection(&conn)?;
        conn.execute_batch(
            "
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
                tokenize = 'trigram'
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

        // Schema version bumps trigger safe automatic migrations when upgrading an older index.db:
        // v1: frontmatter parser
        // v2: source-relative wikilink, parser boundaries & local Markdown links (OBS-02/05/06)
        // v3: FTS5 trigram tokenizer + notes.rowid-mirrored FTS5 deletes (O(N^2) -> O(log N))
        const CURRENT_INDEX_VERSION: i64 = 3;
        let cur_ver = conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
        let fts_sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'notes_fts'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let needs_fts_rebuild = fts_sql
            .as_deref()
            .is_none_or(|sql| !sql.to_ascii_lowercase().contains("trigram"));

        if cur_ver < CURRENT_INDEX_VERSION || needs_fts_rebuild {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let tx_ver =
                tx.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
            let tx_fts_sql: Option<String> = tx
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'notes_fts'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let tx_needs_fts = tx_fts_sql
                .as_deref()
                .is_none_or(|sql| !sql.to_ascii_lowercase().contains("trigram"));

            if tx_ver < 2 {
                tx.execute(
                    "UPDATE notes SET mtime_ms = -1, mtime_ns = -1, content_hash = ''",
                    [],
                )?;
            }
            if tx_ver < 3 || tx_needs_fts {
                tx.execute_batch(
                    "
                    DROP TABLE IF EXISTS notes_fts;
                    CREATE VIRTUAL TABLE notes_fts USING fts5(
                        vault_id UNINDEXED,
                        rel_path UNINDEXED,
                        title,
                        rel_path_text,
                        aliases,
                        tags,
                        body_plain,
                        tokenize = 'trigram'
                    );
                    ",
                )?;
                let mut stmt = tx.prepare(
                    "SELECT n.rowid, n.vault_id, n.rel_path, n.title, n.aliases_json, n.body_plain,
                            COALESCE((
                                SELECT GROUP_CONCAT(t.tag, ' ')
                                FROM tags t
                                WHERE t.vault_id = n.vault_id AND t.note_path = n.rel_path
                            ), '')
                     FROM notes n",
                )?;
                let existing_rows: Vec<(i64, String, String, String, String, String, String)> = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                drop(stmt);

                if !existing_rows.is_empty() {
                    let mut ins = tx.prepare_cached(
                        "INSERT INTO notes_fts (rowid, vault_id, rel_path, title, rel_path_text, aliases, tags, body_plain)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    )?;
                    for (rowid, vid, rp, title, aliases_json, body_plain, tags_text) in existing_rows
                    {
                        let aliases_text = if aliases_json == "[]" {
                            String::new()
                        } else {
                            serde_json::from_str::<Vec<String>>(&aliases_json)
                                .unwrap_or_default()
                                .join(" ")
                        };
                        ins.execute(rusqlite::params![
                            rowid,
                            vid,
                            rp,
                            title,
                            rp,
                            aliases_text,
                            tags_text,
                            body_plain,
                        ])?;
                    }
                }
            }
            if tx_ver < CURRENT_INDEX_VERSION {
                tx.pragma_update(None, "user_version", CURRENT_INDEX_VERSION)?;
            }
            tx.commit()?;
        }
        Ok(())
    }
}
