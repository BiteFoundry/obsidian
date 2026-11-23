//! Stdio JSON-RPC 2.0 Model Context Protocol (MCP) server (`bitefoundry-obsidian mcp`, 20 typed tools).
//!
//! Tools:
//! 1.  `list_vaults`
//! 2.  `search_notes`
//! 3.  `read_note`
//! 4.  `list_notes`
//! 5.  `create_note`
//! 6.  `append_note`
//! 7.  `update_frontmatter`
//! 8.  `delete_note` (`confirm: true` guard -> moves note to `.trash/`)
//! 9.  `daily_note`
//! 10. `get_backlinks`
//! 11. `get_outgoing_links`
//! 12. `get_local_graph`
//! 13. `list_tags`
//! 14. `find_by_tag`
//! 15. `list_tasks`
//! 16. `toggle_task`
//! 17. `find_orphans`
//! 18. `reindex_vault`
//! 19. `move_note`
//! 20. `replace_note_text`

use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, Write},
};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::{
    config::{AppConfig, VaultProfile},
    models::{NoteOmittedCounts, TaskFilter, VaultSummary},
    storage::Storage,
    vault::{
        fs::{
            append_to_note, create_note, ensure_daily_note, file_mtime_and_size,
            move_note_to_trash, safe_vault_path, toggle_task_at_line_verified,
            update_note_frontmatter,
        },
        merged_vaults, resolve_request_vault,
    },
};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

pub struct McpServer {
    pub cfg: AppConfig,
    pub default_vault: VaultProfile,
    pub storage: Storage,
}

impl McpServer {
    pub fn new(cfg: AppConfig, default_vault: VaultProfile, storage: Storage) -> Self {
        let _ = storage.sync_vault(&default_vault);
        Self {
            cfg,
            default_vault,
            storage,
        }
    }

    pub fn run_stdio(&self) -> Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());

        for line_res in stdin.lock().lines() {
            let line = line_res?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let req: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(err) => {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": -32700, "message": format!("Parse error: {err}") }
                    });
                    writeln!(out, "{}", serde_json::to_string(&resp)?)?;
                    out.flush()?;
                    continue;
                }
            };
            if let Some(resp) = self.handle_rpc(&req) {
                writeln!(out, "{}", serde_json::to_string(&resp)?)?;
                out.flush()?;
            }
        }
        Ok(())
    }

    pub fn handle_rpc(&self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        // JSON-RPC notifications have no id
        if id.is_none() && method.starts_with("notifications/") {
            return None;
        }
        let resp_id = id.unwrap_or(Value::Null);

        match method {
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "id": resp_id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "@bitefoundry/obsidian",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            })),
            "ping" => Some(json!({ "jsonrpc": "2.0", "id": resp_id, "result": {} })),
            "tools/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": resp_id,
                "result": { "tools": tool_definitions() }
            })),
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.dispatch_tool(name, &args) {
                    Ok(payload) => {
                        let text =
                            serde_json::to_string(&payload).unwrap_or_else(|_| payload.to_string());
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": resp_id,
                            "result": {
                                "content": [{ "type": "text", "text": text }],
                                "isError": false
                            }
                        }))
                    }
                    Err(err) => Some(json!({
                        "jsonrpc": "2.0",
                        "id": resp_id,
                        "result": {
                            "content": [{ "type": "text", "text": format!("Error: {err}") }],
                            "isError": true
                        }
                    })),
                }
            }
            _ => Some(json!({
                "jsonrpc": "2.0",
                "id": resp_id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") }
            })),
        }
    }

    fn resolve_vault_unsynced(&self, args: &Value) -> Result<VaultProfile> {
        match args
            .get("vault")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => Ok(self.default_vault.clone()),
            Some(spec) => resolve_request_vault(&self.cfg, &self.default_vault, spec),
        }
    }

    fn resolve_vault(&self, args: &Value) -> Result<VaultProfile> {
        let vault = self.resolve_vault_unsynced(args)?;
        let _ = self.storage.sync_vault(&vault);
        Ok(vault)
    }

    pub fn dispatch_tool(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "list_vaults" => {
                let vaults: Vec<VaultSummary> = merged_vaults(&self.cfg)
                    .into_iter()
                    .map(|v| {
                        let exists = v.path.is_dir();
                        if exists {
                            let _ = self.storage.sync_vault(&v);
                        }
                        let active = v.id == self.default_vault.id;
                        let count = self.storage.note_count(&v.id).unwrap_or(0);
                        VaultSummary {
                            id: v.id,
                            name: v.name,
                            exists,
                            path: v.path,
                            active,
                            discovered: v.discovered,
                            open_in_obsidian: v.open_in_obsidian,
                            note_count: count,
                        }
                    })
                    .collect();
                Ok(json!({ "vaults": vaults }))
            }
            "search_notes" => {
                let vault = self.resolve_vault(args)?;
                let query = req_str(args, "query")?;
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let page = self.storage.search_notes_page(&vault.id, query, limit)?;
                let returned = page.hits.len();
                Ok(json!({
                    "query": query,
                    "limit": page.limit,
                    "count": returned,
                    "returned": returned,
                    "has_more": page.has_more,
                    "next_cursor": Value::Null,
                    "results": page.hits,
                }))
            }
            "read_note" => {
                let vault = self.resolve_vault(args)?;
                let note = req_str(args, "note")?;
                let compact = match args.get("compact") {
                    None => true,
                    Some(v) => v
                        .as_bool()
                        .ok_or_else(|| anyhow!("Invalid 'compact': expected a boolean"))?,
                };
                if compact {
                    let content = self.storage.load_note_content(&vault, note)?;
                    let counts = self
                        .storage
                        .note_omitted_counts(&vault.id, &content.rel_path)?;
                    return Ok(json!({
                        "rel_path": content.rel_path,
                        "title": content.title,
                        "raw_markdown": content.raw_markdown,
                        "mtime_ms": content.mtime_ms,
                        "omitted": mcp_omitted(
                            Some(counts),
                            &["folder", "frontmatter", "word_count"],
                        ),
                        "hint": mcp_read_note_hint(&content.rel_path),
                    }));
                }
                let detail = self.storage.load_note_detail(&vault, note)?;
                Ok(json!({
                    "rel_path": detail.summary.rel_path,
                    "title": detail.summary.title,
                    "folder": detail.summary.folder,
                    "frontmatter": detail.summary.frontmatter,
                    "tags": detail.summary.tags,
                    "aliases": detail.summary.aliases,
                    "word_count": detail.summary.word_count,
                    "mtime_ms": detail.summary.mtime_ms,
                    "raw_markdown": detail.raw_markdown,
                    "headings": detail.parsed.headings,
                    "tasks": detail.parsed.tasks,
                    "backlinks": detail.backlinks,
                    "outgoing_links": detail.outgoing_links,
                }))
            }
            "list_notes" => {
                let vault = self.resolve_vault(args)?;
                let folder = args.get("folder").and_then(|v| v.as_str());
                let limit = parse_page_limit(args)?;
                let cursor = parse_page_cursor(args)?;
                let page = self
                    .storage
                    .list_notes_page(&vault.id, folder, cursor, Some(limit))?;
                let returned = page.items.len();
                Ok(json!({
                    "count": returned,
                    "returned": returned,
                    "total": page.total,
                    "has_more": page.has_more,
                    "next_cursor": page.next_cursor,
                    "notes": page.items,
                }))
            }
            "create_note" => {
                let vault = self.resolve_vault(args)?;
                let title = req_str(args, "title")?;
                let folder = args.get("folder").and_then(|v| v.as_str());
                let content = args.get("content").and_then(|v| v.as_str());
                let tags: Vec<String> = match args.get("tags") {
                    None => Vec::new(),
                    Some(v) => {
                        let arr = v.as_array().ok_or_else(|| {
                            anyhow!("Invalid 'tags': expected an array of strings")
                        })?;
                        let mut out = Vec::with_capacity(arr.len());
                        for item in arr {
                            let s = item.as_str().ok_or_else(|| {
                                anyhow!("Invalid 'tags': expected an array of strings")
                            })?;
                            out.push(s.to_string());
                        }
                        out
                    }
                };
                let (rel_path, _) = create_note(&vault.path, title, folder, content, &tags)?;
                let _ = self.storage.sync_vault(&vault);
                Ok(json!({ "created": true, "rel_path": rel_path }))
            }
            "append_note" => {
                let vault = self.resolve_vault(args)?;
                let note = req_str(args, "note")?;
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| anyhow!("Missing required string argument 'text'"))?;
                let rel_path = self.storage.resolve_note_path(&vault.id, note)?;
                let abs = safe_vault_path(&vault.path, &rel_path)?;
                append_to_note(&abs, text)?;
                let _ = self.storage.sync_vault(&vault);
                Ok(json!({ "appended": true, "rel_path": rel_path }))
            }
            "replace_note_text" => {
                let vault = self.resolve_vault(args)?;
                let note = req_str(args, "note")?;
                let find = args
                    .get("find")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Missing required string argument 'find'"))?;
                let replace = args
                    .get("replace")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Missing required string argument 'replace'"))?;
                Ok(serde_json::to_value(
                    self.storage
                        .replace_note_text(&vault, note, find, replace)?,
                )?)
            }
            "update_frontmatter" => {
                let vault = self.resolve_vault(args)?;
                let note = req_str(args, "note")?;
                let properties = args
                    .get("properties")
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| anyhow!("Missing object argument 'properties'"))?;
                let mut map = BTreeMap::new();
                for (k, v) in properties {
                    map.insert(k.clone(), v.clone());
                }
                let rel_path = self.storage.resolve_note_path(&vault.id, note)?;
                let abs = safe_vault_path(&vault.path, &rel_path)?;
                update_note_frontmatter(&abs, &map)?;
                let _ = self.storage.sync_vault(&vault);
                Ok(
                    json!({ "updated": true, "rel_path": rel_path, "keys": map.keys().collect::<Vec<_>>() }),
                )
            }
            "delete_note" => {
                let confirm = args
                    .get("confirm")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !confirm {
                    bail!("Deletion requires confirm: true (moves the note safely to .trash/)");
                }
                let vault = self.resolve_vault(args)?;
                let note = req_str(args, "note")?;
                let rel_path = self.storage.resolve_note_path(&vault.id, note)?;
                let trashed = move_note_to_trash(&vault.path, &rel_path)?;
                let _ = self.storage.sync_vault(&vault);
                Ok(json!({
                    "deleted": true,
                    "rel_path": rel_path,
                    "trash_path": trashed.to_string_lossy(),
                }))
            }
            "move_note" => {
                let src = req_str(args, "src")?;
                let dst = req_nonblank_string(args, "dst")?;
                let vault = self.resolve_vault_unsynced(args)?;
                self.storage.sync_vault(&vault)?;
                let moved = self.storage.move_note_with_links(&vault, src, dst)?;
                Ok(serde_json::to_value(moved)?)
            }
            "daily_note" => {
                let vault = self.resolve_vault(args)?;
                let date = args.get("date").and_then(|v| v.as_str());
                let append = args.get("append").and_then(|v| v.as_str());
                let (rel_path, abs, created) = ensure_daily_note(&vault, date)?;
                let mut appended = false;
                if let Some(txt) = append.filter(|s| !s.trim().is_empty()) {
                    append_to_note(&abs, txt)?;
                    appended = true;
                }
                let synced = self.storage.sync_vault(&vault).is_ok();
                if appended {
                    return Ok(json!({
                        "rel_path": rel_path,
                        "created": created,
                        "appended": true,
                        "hint": mcp_read_note_hint(&rel_path),
                    }));
                }
                let counts = if synced {
                    self.storage.note_omitted_counts(&vault.id, &rel_path).ok()
                } else {
                    None
                };
                let raw_bytes = fs::read(&abs)?;
                let raw_markdown = String::from_utf8(raw_bytes)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                Ok(json!({
                    "rel_path": rel_path,
                    "created": created,
                    "raw_markdown": raw_markdown,
                    "omitted": mcp_omitted(
                        counts,
                        &["title", "mtime_ms", "folder", "frontmatter", "word_count"],
                    ),
                    "hint": mcp_read_note_hint(&rel_path),
                }))
            }
            "get_backlinks" => {
                let vault = self.resolve_vault(args)?;
                let note = req_str(args, "note")?;
                let rel_path = self.storage.resolve_note_path(&vault.id, note)?;
                let backlinks = self.storage.get_backlinks(&vault.id, &rel_path)?;
                Ok(json!({ "note": rel_path, "count": backlinks.len(), "backlinks": backlinks }))
            }
            "get_outgoing_links" => {
                let vault = self.resolve_vault(args)?;
                let note = req_str(args, "note")?;
                let rel_path = self.storage.resolve_note_path(&vault.id, note)?;
                let links = self.storage.get_outgoing_links(&vault, &rel_path)?;
                Ok(json!({ "note": rel_path, "count": links.len(), "outgoing_links": links }))
            }
            "get_local_graph" => {
                let vault = self.resolve_vault(args)?;
                let center = match args
                    .get("note")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(n) => Some(self.storage.resolve_note_path(&vault.id, n)?),
                    None => None,
                };
                let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
                let graph = self.storage.get_graph(&vault, center.as_deref(), depth)?;
                Ok(serde_json::to_value(graph)?)
            }
            "list_tags" => {
                let vault = self.resolve_vault(args)?;
                let tags = self.storage.list_tags(&vault.id)?;
                Ok(json!({ "count": tags.len(), "tags": tags }))
            }
            "find_by_tag" => {
                let vault = self.resolve_vault(args)?;
                let tag = req_str(args, "tag")?;
                let notes = self.storage.find_by_tag(&vault.id, tag)?;
                Ok(
                    json!({ "tag": tag.trim_start_matches('#'), "count": notes.len(), "notes": notes }),
                )
            }
            "list_tasks" => {
                let vault = self.resolve_vault(args)?;
                let filter = match args.get("status") {
                    None => TaskFilter::All,
                    Some(v) => match v.as_str().map(|s| s.to_ascii_lowercase()).as_deref() {
                        Some("all") => TaskFilter::All,
                        Some("open") => TaskFilter::Open,
                        Some("done") => TaskFilter::Done,
                        _ => bail!("Invalid 'status': expected all, open or done"),
                    },
                };
                let note_path = match args
                    .get("note")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(n) => Some(self.storage.resolve_note_path(&vault.id, n)?),
                    None => None,
                };
                let limit = parse_page_limit(args)?;
                let cursor = parse_page_cursor(args)?;
                let page = self.storage.list_tasks_page(
                    &vault.id,
                    filter,
                    note_path.as_deref(),
                    cursor,
                    Some(limit),
                )?;
                let returned = page.items.len();
                Ok(json!({
                    "count": returned,
                    "returned": returned,
                    "total": page.total,
                    "has_more": page.has_more,
                    "next_cursor": page.next_cursor,
                    "tasks": page.items,
                }))
            }
            "toggle_task" => {
                let vault = self.resolve_vault(args)?;
                let note = req_str(args, "note")?;
                let line = args.get("line").and_then(|v| v.as_u64()).ok_or_else(|| {
                    anyhow!("Missing integer argument 'line' (1-based line number)")
                })? as usize;
                let expected_text = args.get("expected_text").and_then(|v| v.as_str());
                let expected_checked = args.get("expected_checked").and_then(|v| v.as_bool());
                let expected_mtime_ms = args.get("expected_mtime_ms").and_then(|v| v.as_i64());
                let rel_path = self.storage.resolve_note_path(&vault.id, note)?;
                let abs = safe_vault_path(&vault.path, &rel_path)?;
                if let Some(exp_mtime) = expected_mtime_ms {
                    let (actual_mtime, _) = file_mtime_and_size(&abs);
                    if actual_mtime != exp_mtime {
                        bail!(
                            "Task conflict: note '{rel_path}' modified externally (expected mtime_ms {exp_mtime}, found {actual_mtime})"
                        );
                    }
                }
                let checked =
                    toggle_task_at_line_verified(&abs, line, expected_text, expected_checked)?;
                let _ = self.storage.sync_vault(&vault);
                Ok(json!({ "rel_path": rel_path, "line": line, "checked": checked }))
            }
            "find_orphans" => {
                let vault = self.resolve_vault(args)?;
                let orphans = self.storage.find_orphans(&vault.id)?;
                Ok(json!({ "count": orphans.len(), "orphans": orphans }))
            }
            "reindex_vault" => {
                let vault = self.resolve_vault_unsynced(args)?;
                let stats = self.storage.reindex_vault(&vault)?;
                Ok(serde_json::to_value(stats)?)
            }
            other => bail!("Unknown MCP tool: {other}"),
        }
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    req_nonblank_string(args, key).map(str::trim)
}

fn req_nonblank_string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("Missing required string argument '{key}'"))
}

fn mcp_read_note_hint(rel_path: &str) -> String {
    format!(
        "read_note {}",
        json!({ "note": rel_path, "compact": false })
    )
}

fn mcp_omitted(counts: Option<NoteOmittedCounts>, fields: &[&str]) -> Value {
    match counts {
        Some(c) => json!({
            "backlinks": c.backlinks,
            "outgoing_links": c.outgoing_links,
            "tags": c.tags,
            "aliases": c.aliases,
            "headings": c.headings,
            "tasks": c.tasks,
            "fields": fields,
        }),
        None => json!({
            "backlinks": Value::Null,
            "outgoing_links": Value::Null,
            "tags": Value::Null,
            "aliases": Value::Null,
            "headings": Value::Null,
            "tasks": Value::Null,
            "fields": fields,
        }),
    }
}

fn parse_page_limit(args: &Value) -> Result<usize> {
    match args.get("limit") {
        None => Ok(50),
        Some(v) => match v.as_u64() {
            Some(n) if (1..=500).contains(&n) => Ok(n as usize),
            _ => bail!("Invalid 'limit': expected an integer from 1 to 500"),
        },
    }
}

fn parse_page_cursor(args: &Value) -> Result<Option<&str>> {
    match args.get("cursor") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_str() {
            Some(s) => Ok(Some(s)),
            None => bail!("Invalid 'cursor': expected a string"),
        },
    }
}

/// Returns the typed MCP tool schemas.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "list_vaults",
            "List registered and auto-discovered vaults with note counts. Scans every vault to refresh counts, so the first call can be slow with many or large vaults.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "search_notes",
            "Search notes across a vault using SQLite FTS5. Korean/CJK queries also match inside longer words (substring scan).",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query string." },
                    "limit": { "type": "integer", "description": "Maximum results 1..200 (default 20). Out-of-range values are clamped; the response reports the limit used and has_more." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["query"]
            }),
        ),
        tool(
            "read_note",
            "Read a Markdown note. Returns a compact object by default; pass compact:false for frontmatter, tags, headings, tasks, backlinks, and outgoing links.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Relative path, note title, or alias." },
                    "compact": { "type": "boolean", "description": "Default true: return rel_path, title, raw_markdown, mtime_ms, omitted, and hint. Set false for frontmatter, tags, headings, tasks, backlinks, and outgoing links." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["note"]
            }),
        ),
        tool(
            "list_notes",
            "List notes in the active vault, optionally filtered by folder prefix.",
            json!({
                "type": "object",
                "properties": {
                    "folder": { "type": "string", "description": "Optional folder prefix filter." },
                    "limit": { "type": "integer", "description": "Page size 1..500 (default 50)." },
                    "cursor": { "type": "string", "description": "next_cursor from the previous page." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                }
            }),
        ),
        tool(
            "create_note",
            "Create a new Markdown note atomically with optional folder, initial content, and YAML frontmatter tags.",
            json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Note title or filename." },
                    "folder": { "type": "string", "description": "Optional target folder relative to vault root." },
                    "content": { "type": "string", "description": "Optional Markdown content." },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional frontmatter tags." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["title"]
            }),
        ),
        tool(
            "append_note",
            "Append Markdown text, checklist items, or callouts atomically to an existing note.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Relative path, note title, or alias." },
                    "text": { "type": "string", "description": "Markdown text to append." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["note", "text"]
            }),
        ),
        tool(
            "replace_note_text",
            "Replace a literal string only when it occurs exactly once in the Markdown body. Return the match count and whether the replacement ran.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Relative path, note title, or alias." },
                    "find": { "type": "string", "minLength": 1, "description": "Exact text to find, including spacing and line endings." },
                    "replace": { "type": "string", "description": "Replacement text. Use an empty string to delete the match." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["note", "find", "replace"]
            }),
        ),
        tool(
            "update_frontmatter",
            "Upsert or remove (null) YAML frontmatter properties on an existing note while preserving its body.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Relative path, note title, or alias." },
                    "properties": { "type": "object", "description": "Key-value pairs to merge into YAML frontmatter (null removes key)." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["note", "properties"]
            }),
        ),
        tool(
            "delete_note",
            "Safely move a note to the vault's .trash/ directory. Requires confirm: true.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Relative path, note title, or alias." },
                    "confirm": { "type": "boolean", "description": "Must be true to execute deletion to .trash/." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["note", "confirm"]
            }),
        ),
        tool(
            "daily_note",
            "Open or create today's Daily Note (or a specific YYYY-MM-DD date) and optionally append text. Calls with non-empty append return a receipt (rel_path, created, appended, hint); calls without append return the note body. For full metadata call read_note with the returned rel_path and compact:false. Each call with append appends again.",
            json!({
                "type": "object",
                "properties": {
                    "date": { "type": "string", "description": "Optional date string (e.g. 2026-11-20)." },
                    "append": { "type": "string", "description": "Optional text to append to the daily note." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                }
            }),
        ),
        tool(
            "get_backlinks",
            "Return all incoming linked mentions (backlinks) to a note with surrounding context snippets.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Target note path, title, or alias." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["note"]
            }),
        ),
        tool(
            "get_outgoing_links",
            "Return all outgoing wikilinks and embeds from a note, including resolved and unresolved targets. Each entry has a status: resolved, attachment, ambiguous, or missing.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Source note path, title, or alias." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["note"]
            }),
        ),
        tool(
            "get_local_graph",
            "Return the local BFS knowledge graph around a note (or the full vault graph when note is omitted). Omitting note returns the whole-vault graph, which is large and slow on big vaults; pass note and a small depth when possible.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Optional center note path, title, or alias." },
                    "depth": { "type": "integer", "description": "Neighborhood hop depth 1..5 (default 2; other values are clamped and the result reports the depth used)." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                }
            }),
        ),
        tool(
            "list_tags",
            "List all inline and frontmatter tags in the vault with note counts. Aggregates over the whole vault.",
            json!({
                "type": "object",
                "properties": {
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                }
            }),
        ),
        tool(
            "find_by_tag",
            "Find all notes containing a tag (including nested subtags like #project/q4).",
            json!({
                "type": "object",
                "properties": {
                    "tag": { "type": "string", "description": "Tag name with or without leading #." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["tag"]
            }),
        ),
        tool(
            "list_tasks",
            "List Markdown checklist tasks across the vault or within a single note.",
            json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "enum": ["all", "open", "done"], "description": "Filter by task status." },
                    "note": { "type": "string", "description": "Optional note filter." },
                    "limit": { "type": "integer", "description": "Page size 1..500 (default 50)." },
                    "cursor": { "type": "string", "description": "next_cursor from the previous page." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                }
            }),
        ),
        tool(
            "toggle_task",
            "Surgically toggle a Markdown checkbox (- [ ] <-> - [x]) at an exact 1-based line number, optionally verifying expected text/state to guard against shifted lines.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Note path, title, or alias." },
                    "line": { "type": "integer", "description": "1-based line number of the task checkbox." },
                    "expected_text": { "type": "string", "description": "Optional expected task text at this line; refuses toggle on mismatch." },
                    "expected_checked": { "type": "boolean", "description": "Optional expected current checked state; refuses toggle on mismatch." },
                    "expected_mtime_ms": { "type": "integer", "description": "Optional expected note mtime_ms; refuses toggle if file changed." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["note", "line"]
            }),
        ),
        tool(
            "find_orphans",
            "Find orphan notes that have zero incoming backlinks and zero outgoing wikilinks. Reads the whole note index; cost grows with vault size and the result is not paginated.",
            json!({
                "type": "object",
                "properties": {
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                }
            }),
        ),
        tool(
            "reindex_vault",
            "Force a full verification rescan: re-reads and hashes every note and rebuilds link resolution. Slow on large vaults. Rarely needed: every other tool already syncs changed files first.",
            json!({
                "type": "object",
                "properties": {
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                }
            }),
        ),
        tool(
            "move_note",
            "Move or rename a note within its vault and update incoming wikilinks that need a new target.",
            json!({
                "type": "object",
                "properties": {
                    "src": { "type": "string", "description": "Source note path relative to the vault, title, or alias." },
                    "dst": { "type": "string", "description": "Destination path relative to the same vault, including the new filename. The .md extension is optional." },
                    "vault": { "type": "string", "description": "Optional registered vault name, ID, or path (see list_vaults). Unregistered directories are rejected." }
                },
                "required": ["src", "dst"]
            }),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}
