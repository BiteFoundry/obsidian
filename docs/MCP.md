# `@bitefoundry/obsidian` MCP Server (`obsidian mcp`)

`@bitefoundry/obsidian` ships a native stdio JSON-RPC 2.0 Model Context Protocol (MCP) server with **19 typed tools** operating directly on local vaults. Both npm commands, `obsidian mcp` and `obs mcp`, start this server. Use `obs` if your PATH also contains the desktop application's command. [Obsidian CLI setup](https://obsidian.md/help/cli)

## Configuration

Add to Claude Desktop, Cursor, Windsurf, or any MCP-compatible host:

```json
{
  "mcpServers": {
    "obsidian": {
      "command": "obsidian",
      "args": ["mcp"]
    }
  }
}
```

Optionally pin a specific vault via `"args": ["--vault", "My Vault", "mcp"]` or `"env": { "OBSIDIAN_VAULT": "My Vault" }`.

## Tool Catalog (19 Tools)

| # | Tool | Arguments | Purpose |
| :-: | :--- | :--- | :--- |
| 1 | `list_vaults` | *(none)* | List auto-discovered & registered Obsidian vaults with note counts |
| 2 | `search_notes` | `query`, `limit?`, `vault?` | SQLite FTS5 search with `<3`-char Korean/CJK `LIKE` fallback |
| 3 | `read_note` | `note`, `compact?`, `vault?` | Read raw Markdown, frontmatter, tags, headings, tasks, backlinks & outgoing links |
| 4 | `list_notes` | `folder?`, `vault?` | List notes in the vault, optionally filtered by folder prefix |
| 5 | `create_note` | `title`, `folder?`, `content?`, `tags?`, `vault?` | Create a new `.md` note atomically with YAML frontmatter |
| 6 | `append_note` | `note`, `text`, `vault?` | Append Markdown text, tasks, or callouts atomically |
| 7 | `update_frontmatter` | `note`, `properties`, `vault?` | Set or delete (`null`) top-level YAML properties |
| 8 | `delete_note` | `note`, `confirm` (`true`), `vault?` | Safely move a note into `<vault>/.trash/` (requires `confirm: true`) |
| 9 | `daily_note` | `date?`, `append?`, `vault?` | Open or create today's Daily Note and optionally append text |
| 10 | `get_backlinks` | `note`, `vault?` | Return incoming linked mentions (backlinks) with snippet context |
| 11 | `get_outgoing_links` | `note`, `vault?` | Return outgoing wikilinks and embeds (resolved & unresolved) |
| 12 | `get_local_graph` | `note?`, `depth?`, `vault?` | Return local BFS neighborhood (or global vault graph when `note` omitted) |
| 13 | `list_tags` | `vault?` | List all inline and frontmatter `#tags` with note counts |
| 14 | `find_by_tag` | `tag`, `vault?` | Find all notes matching a tag (including nested subtags) |
| 15 | `list_tasks` | `status?`, `note?`, `vault?` | List Markdown checklist tasks (`all`, `open`, or `done`) |
| 16 | `toggle_task` | `note`, `line`, `expected_text?`, `expected_checked?`, `expected_mtime_ms?`, `vault?` | Toggle a Markdown checkbox at an exact 1-based line number, with optional conflict checks |
| 17 | `find_orphans` | `vault?` | Find orphan notes with 0 backlinks and 0 outgoing links |
| 18 | `reindex_vault` | `vault?` | Trigger immediate incremental scan & SQLite FTS5/graph reindex |
| 19 | `move_note` | `src`, `dst`, `vault?` | Move or rename a note within its vault and update incoming wikilinks that need a new target |

## Read and edit notes

Call `read_note` with `{"note":"Projects/Review.md","compact":true}` to get `rel_path`, `title`, and `raw_markdown`. Omit `compact` for metadata and links.

- For arbitrary Markdown edits, use `obs notes edit <NOTE>`, TUI `e` for inline editing, or TUI `Ctrl+E` for an external editor.
- Use MCP `append_note` to add Markdown with its indentation and trailing spaces intact. The CLI form is `obs notes append <NOTE> -- <TEXT>`.
- Use MCP `toggle_task` or `obs toggle-task <NOTE> <LINE>` for a checkbox. To change YAML properties, call MCP `update_frontmatter` with a properties object such as `{"status":"ready","obsolete":null}`. This sets `status` and removes `obsolete`.
- For HTTP edit routes, see the append and task toggle entries in the [HTTP API catalog](API.md).

### Move or rename a note

Call `move_note` with these arguments, or run `obs notes move "Release Review" "Archive/Release Review" --json`.

```json
{"src":"Release Review","dst":"Archive/Release Review","vault":"work"}
```

`src` accepts a vault-relative path, note title, or frontmatter alias. `dst` gives the new path and filename within the selected vault. The command adds `.md` when omitted, creates missing parent directories, and rejects an existing destination.

Both surfaces report `moved`, `from`, `to`, `updated_links_in_notes`, and `bookmark_preserved`. The link count is the number of files rewritten. Matching wikilinks keep heading and block fragments, display text, and the embed marker. Wikilinks in inline and fenced code stay literal. To adjust Markdown-style `[label](path)` destinations, edit their source notes. [Obsidian internal link formats](https://obsidian.md/help/links)
