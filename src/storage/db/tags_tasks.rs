//! Tag counts, tag filtering, checklist task listing, and headings queries.

use anyhow::{anyhow, Result};
use rusqlite::params;

use super::{
    notes::{page_after, parse_task_cursor},
    Storage,
};
use crate::models::{HeadingEntry, ListPage, NoteSummary, TagCount, TaskEntry, TaskFilter};

impl Storage {
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

}
