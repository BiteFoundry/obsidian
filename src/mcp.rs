//! Stdio JSON-RPC 2.0 Model Context Protocol (MCP) server (`obsidian mcp`, 18 typed tools).
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

use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::{
    config::{AppConfig, VaultProfile},
    models::{TaskFilter, VaultSummary},
    storage::Storage,
    vault::{
        fs::{
            append_to_note, create_note, ensure_daily_note, move_note_to_trash, safe_vault_path,
            toggle_task_at_line, update_note_frontmatter,
        },
        merged_vaults, resolve_active_vault,
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
                        let text = serde_json::to_string_pretty(&payload)
                            .unwrap_or_else(|_| payload.to_string());
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

    fn resolve_vault(&self, args: &Value) -> Result<VaultProfile> {
        match args
            .get("vault")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => Ok(self.default_vault.clone()),
            Some(spec) => {
                let v = resolve_active_vault(&self.cfg, Some(spec))?;
                let _ = self.storage.sync_vault(&v);
                Ok(v)
            }
        }
    }

    pub fn dispatch_tool(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "list_vaults" => {
                let vaults: Vec<VaultSummary> = merged_vaults(&self.cfg)
                    .into_iter()
                    .map(|v| {
                        let active = v.id == self.default_vault.id;
                        let count = self.storage.note_count(&v.id).unwrap_or(0);
                        VaultSummary {
                            id: v.id,
                            name: v.name,
                            exists: v.path.is_dir(),
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
                let results = self.storage.search_notes(&vault.id, query, limit)?;
                Ok(json!({ "query": query, "count": results.len(), "results": results }))
            }
            "read_note" => {
                let vault = self.resolve_vault(args)?;
                let note = req_str(args, "note")?;
                let detail = self.storage.load_note_detail(&vault, note)?;
                Ok(json!({
                    "rel_path": detail.summary.rel_path,
                    "title": detail.summary.title,
                    "folder": detail.summary.folder,
                    "frontmatter": detail.summary.frontmatter,
                    "tags": detail.summary.tags,
                    "aliases": detail.summary.aliases,
                    "word_count": detail.summary.word_count,
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
                let notes = self.storage.list_notes(&vault.id, folder)?;
                Ok(json!({ "count": notes.len(), "notes": notes }))
            }
            "create_note" => {
                let vault = self.resolve_vault(args)?;
                let title = req_str(args, "title")?;
                let folder = args.get("folder").and_then(|v| v.as_str());
                let content = args.get("content").and_then(|v| v.as_str());
                let tags: Vec<String> = args
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|i| i.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let (rel_path, _) = create_note(&vault.path, title, folder, content, &tags)?;
                let _ = self.storage.sync_vault(&vault);
                Ok(json!({ "created": true, "rel_path": rel_path }))
            }
            "append_note" => {
                let vault = self.resolve_vault(args)?;
                let note = req_str(args, "note")?;
                let text = req_str(args, "text")?;
                let rel_path = self.storage.resolve_note_path(&vault.id, note)?;
                let abs = safe_vault_path(&vault.path, &rel_path)?;
                append_to_note(&abs, text)?;
                let _ = self.storage.sync_vault(&vault);
                Ok(json!({ "appended": true, "rel_path": rel_path }))
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
                Ok(json!({ "updated": true, "rel_path": rel_path, "keys": map.keys().collect::<Vec<_>>() }))
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
            "daily_note" => {
                let vault = self.resolve_vault(args)?;
                let date = args.get("date").and_then(|v| v.as_str());
                let append = args.get("append").and_then(|v| v.as_str());
                let (rel_path, abs, created) = ensure_daily_note(&vault, date)?;
                if let Some(txt) = append.filter(|s| !s.trim().is_empty()) {
                    append_to_note(&abs, txt)?;
                }
                let _ = self.storage.sync_vault(&vault);
                let detail = self.storage.load_note_detail(&vault, &rel_path)?;
                Ok(json!({
                    "rel_path": rel_path,
                    "created": created,
                    "raw_markdown": detail.raw_markdown,
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
                let links = self.storage.get_outgoing_links(&vault.id, &rel_path)?;
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
                let graph = self.storage.get_graph(&vault.id, center.as_deref(), depth)?;
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
                Ok(json!({ "tag": tag.trim_start_matches('#'), "count": notes.len(), "notes": notes }))
            }
            "list_tasks" => {
                let vault = self.resolve_vault(args)?;
                let filter = match args
                    .get("status")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_ascii_lowercase())
                    .as_deref()
                {
                    Some("open") => TaskFilter::Open,
                    Some("done") => TaskFilter::Done,
                    _ => TaskFilter::All,
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
                let tasks = self
                    .storage
                    .list_tasks(&vault.id, filter, note_path.as_deref())?;
                Ok(json!({ "count": tasks.len(), "tasks": tasks }))
            }
            "toggle_task" => {
                let vault = self.resolve_vault(args)?;
                let note = req_str(args, "note")?;
                let line = args
                    .get("line")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| anyhow!("Missing integer argument 'line' (1-based line number)"))?
                    as usize;
                let rel_path = self.storage.resolve_note_path(&vault.id, note)?;
                let abs = safe_vault_path(&vault.path, &rel_path)?;
                let checked = toggle_task_at_line(&abs, line)?;
                let _ = self.storage.sync_vault(&vault);
                Ok(json!({ "rel_path": rel_path, "line": line, "checked": checked }))
            }
            "find_orphans" => {
                let vault = self.resolve_vault(args)?;
                let orphans = self.storage.find_orphans(&vault.id)?;
                Ok(json!({ "count": orphans.len(), "orphans": orphans }))
            }
            "reindex_vault" => {
                let vault = self.resolve_vault(args)?;
                let stats = self.storage.sync_vault(&vault)?;
                Ok(serde_json::to_value(stats)?)
            }
            other => bail!("Unknown MCP tool: {other}"),
        }
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Missing required string argument '{key}'"))
}

/// Returns the 18 typed MCP tool schemas (100% English descriptions).
pub fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "list_vaults",
            "List all auto-discovered and registered local Obsidian vaults with note counts.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "search_notes",
            "Search notes across a vault using SQLite FTS5 with automatic short Korean/CJK LIKE fallback.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query string." },
                    "limit": { "type": "integer", "description": "Maximum results (default 20)." },
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
                },
                "required": ["query"]
            }),
        ),
        tool(
            "read_note",
            "Read a Markdown note including YAML frontmatter, tags, headings, tasks, backlinks, and outgoing wikilinks.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Relative path, note title, or alias." },
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
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
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
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
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
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
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
                },
                "required": ["note", "text"]
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
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
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
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
                },
                "required": ["note", "confirm"]
            }),
        ),
        tool(
            "daily_note",
            "Open or create today's Daily Note (or a specific YYYY-MM-DD date) and optionally append text.",
            json!({
                "type": "object",
                "properties": {
                    "date": { "type": "string", "description": "Optional date string (e.g. 2026-11-20)." },
                    "append": { "type": "string", "description": "Optional text to append to the daily note." },
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
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
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
                },
                "required": ["note"]
            }),
        ),
        tool(
            "get_outgoing_links",
            "Return all outgoing wikilinks and embeds from a note, including resolved and unresolved targets.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Source note path, title, or alias." },
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
                },
                "required": ["note"]
            }),
        ),
        tool(
            "get_local_graph",
            "Return the local BFS knowledge graph around a note (or the full vault graph when note is omitted).",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Optional center note path, title, or alias." },
                    "depth": { "type": "integer", "description": "Neighborhood hop depth 1..5 (default 2)." },
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
                }
            }),
        ),
        tool(
            "list_tags",
            "List all inline and frontmatter tags in the vault with note counts.",
            json!({
                "type": "object",
                "properties": {
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
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
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
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
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
                }
            }),
        ),
        tool(
            "toggle_task",
            "Surgically toggle a Markdown checkbox (- [ ] <-> - [x]) at an exact 1-based line number.",
            json!({
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Note path, title, or alias." },
                    "line": { "type": "integer", "description": "1-based line number of the task checkbox." },
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
                },
                "required": ["note", "line"]
            }),
        ),
        tool(
            "find_orphans",
            "Find orphan notes that have zero incoming backlinks and zero outgoing wikilinks.",
            json!({
                "type": "object",
                "properties": {
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
                }
            }),
        ),
        tool(
            "reindex_vault",
            "Trigger an immediate incremental filesystem scan and SQLite FTS5 + graph index refresh.",
            json!({
                "type": "object",
                "properties": {
                    "vault": { "type": "string", "description": "Optional vault name, ID, or path." }
                }
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
