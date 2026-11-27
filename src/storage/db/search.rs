//! Full-text FTS5 trigram search and `< 3`-char substring LIKE fallback.

use anyhow::Result;
use rusqlite::{params, Connection};

use super::{notes::load_tags_for_note, Storage};
use crate::models::{SearchHit, SearchPage};

impl Storage {
    /// Full-text search via SQLite FTS5 (`trigram` tokenizer), falling back to a `LIKE`
    /// scan only when a query contains `< 3`-char tokens (or when FTS5 yields no hits).
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
        let conn = self.read_conn()?;
        let q = query.trim();
        if q.is_empty() {
            let mut stmt = conn.prepare_cached(
                "SELECT rel_path, title, folder, word_count, mtime_ms
                 FROM notes
                 WHERE vault_id = ?1
                 ORDER BY LOWER(folder) ASC, LOWER(title) ASC, rel_path ASC
                 LIMIT ?2",
            )?;
            let rows: Vec<(String, String, String, usize, i64)> = stmt
                .query_map(params![vault_id, fetch as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?.max(0) as usize,
                        r.get::<_, i64>(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            let mut out = Vec::with_capacity(rows.len());
            for (rel_path, title, folder, word_count, mtime_ms) in rows {
                let tags = load_tags_for_note(&conn, vault_id, &rel_path)?;
                out.push(SearchHit {
                    rel_path,
                    title,
                    folder,
                    snippet: String::new(),
                    tags,
                    word_count,
                    mtime_ms,
                });
            }
            return Ok(out);
        }

        let tokens = extract_query_tokens(q);
        // Trigram FTS5 requires >= 3 characters per token; only fall back to LIKE directly
        // when the query has `< 3`-char tokens (or contains literal LIKE-wildcard chars with CJK
        // that trigram still matches verbatim).
        let has_short_token =
            tokens.is_empty() || tokens.iter().any(|tok| tok.chars().count() < 3);

        if !has_short_token {
            let fts_query = build_trigram_fts5_query(&tokens);
            if !fts_query.is_empty() {
                if let Ok(hits) =
                    run_fts5_search(&conn, vault_id, &fts_query, &tokens, contains_cjk(q), fetch)
                {
                    if !hits.is_empty() {
                        return Ok(hits);
                    }
                }
            }
        }

        // Fallback: case-insensitive LIKE over title, rel_path, aliases, tags, body_plain
        run_like_search(&conn, vault_id, q, fetch)
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

fn extract_query_tokens(raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .filter_map(|tok| {
            let clean = tok
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '/');
            (!clean.is_empty()).then(|| clean.to_string())
        })
        .collect()
}

fn build_trigram_fts5_query(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|tok| {
            let escaped = tok.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Returns `true` if `haystack` contains `needle_lower` starting at a word boundary
/// (start of string or preceded by a non-alphanumeric character).
fn contains_at_word_boundary(haystack_lower: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    let mut start = 0;
    while let Some(rel) = haystack_lower[start..].find(needle_lower) {
        let abs = start + rel;
        let prev_ok = haystack_lower[..abs]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        if prev_ok {
            return true;
        }
        let step = haystack_lower[abs..]
            .chars()
            .next()
            .map_or(1, char::len_utf8);
        start = abs + step;
    }
    false
}

fn row_matches_word_boundaries(
    tokens_lower: &[String],
    title: &str,
    rel_path: &str,
    aliases_json: &str,
    tags: &[String],
    body_plain: &str,
) -> bool {
    let title_low = title.to_lowercase();
    let rp_low = rel_path.to_lowercase();
    let aliases_low = aliases_json.to_lowercase();
    let body_low = body_plain.to_lowercase();
    tokens_lower.iter().all(|tok| {
        contains_at_word_boundary(&title_low, tok)
            || contains_at_word_boundary(&rp_low, tok)
            || contains_at_word_boundary(&aliases_low, tok)
            || contains_at_word_boundary(&body_low, tok)
            || tags
                .iter()
                .any(|t| contains_at_word_boundary(&t.to_lowercase(), tok))
    })
}

fn run_fts5_search(
    conn: &Connection,
    vault_id: &str,
    fts_query: &str,
    tokens: &[String],
    is_cjk: bool,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    // Over-fetch slightly so word-boundary ranking/filtering never truncates exact hits prematurely.
    let candidate_limit = limit.saturating_mul(4).clamp(limit, 400);
    let mut stmt = conn.prepare_cached(
        "SELECT f.rel_path, n.title, n.folder,
                snippet(notes_fts, 6, '[', ']', '...', 14) AS snip,
                n.word_count, n.mtime_ms, n.aliases_json, n.body_plain
         FROM notes_fts f
         JOIN notes n ON n.rowid = f.rowid
         WHERE f.vault_id = ?1 AND notes_fts MATCH ?2
         ORDER BY bm25(notes_fts, 0.0, 0.0, 6.0, 2.0, 4.0, 3.0, 1.0) ASC, f.rel_path ASC
         LIMIT ?3",
    )?;
    struct FtsCandidateRow {
        rel_path: String,
        title: String,
        folder: String,
        snippet: String,
        word_count: usize,
        mtime_ms: i64,
        aliases_json: String,
        body_plain: String,
    }

    let raw_rows: Vec<FtsCandidateRow> = stmt
        .query_map(params![vault_id, fts_query, candidate_limit as i64], |r| {
            Ok(FtsCandidateRow {
                rel_path: r.get(0)?,
                title: r.get(1)?,
                folder: r.get(2)?,
                snippet: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                word_count: r.get::<_, i64>(4)?.max(0) as usize,
                mtime_ms: r.get(5)?,
                aliases_json: r.get(6)?,
                body_plain: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if raw_rows.is_empty() {
        return Ok(Vec::new());
    }

    let tokens_lower: Vec<String> = tokens.iter().map(|t| t.to_lowercase()).collect();
    let mut boundary_hits = Vec::new();
    let mut substring_hits = Vec::new();

    for FtsCandidateRow {
        rel_path,
        title,
        folder,
        snippet,
        word_count,
        mtime_ms,
        aliases_json,
        body_plain,
    } in raw_rows
    {
        let tags = load_tags_for_note(conn, vault_id, &rel_path)?;
        let is_boundary = row_matches_word_boundaries(
            &tokens_lower,
            &title,
            &rel_path,
            &aliases_json,
            &tags,
            &body_plain,
        );
        let hit = SearchHit {
            rel_path,
            title,
            folder,
            snippet: snippet.replace('\n', " "),
            tags,
            word_count,
            mtime_ms,
        };
        if is_boundary {
            boundary_hits.push(hit);
        } else {
            substring_hits.push(hit);
        }
    }

    if is_cjk {
        // CJK keeps word-boundary matches ranked first, followed by mid-word substring matches
        // (e.g. `회의록 작성` before `주간회의록`, both via O(log N) trigram index).
        boundary_hits.extend(substring_hits);
        boundary_hits.truncate(limit);
        Ok(boundary_hits)
    } else if !boundary_hits.is_empty() {
        // Non-CJK preserves word-prefix FTS semantics when boundary matches exist
        // (e.g. `meeting` matches `meeting notes` without pulling in `premeeting`).
        boundary_hits.truncate(limit);
        Ok(boundary_hits)
    } else {
        substring_hits.truncate(limit);
        Ok(substring_hits)
    }
}

pub(super) fn escape_like_pattern(raw: &str) -> String {
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
) -> Result<Vec<SearchHit>> {
    let pattern = format!("%{}%", escape_like_pattern(&query.to_lowercase()));
    let mut stmt = conn.prepare_cached(
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
    let rows: Vec<(String, String, String, String, usize, i64)> = stmt
        .query_map(params![vault_id, pattern, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?.max(0) as usize,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut out = Vec::with_capacity(rows.len());
    for (rel_path, title, folder, body_plain, word_count, mtime_ms) in rows {
        let snippet = extract_context_snippet(&body_plain, query)
            .unwrap_or_else(|| first_line_preview(&body_plain, 96));
        let tags = load_tags_for_note(conn, vault_id, &rel_path)?;
        out.push(SearchHit {
            tags,
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

