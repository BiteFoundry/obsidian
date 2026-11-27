//! Full-text FTS5 search and Korean/CJK substring LIKE fallback.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};

use super::{notes::load_tags_map, Storage};
use crate::models::{SearchHit, SearchPage};

impl Storage {
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

}

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

pub(super) fn extract_context_snippet(body: &str, needle: &str) -> Option<String> {
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

pub(super) fn first_line_preview(body: &str, max_chars: usize) -> String {
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

