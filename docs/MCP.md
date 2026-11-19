# `@bitefoundry/obsidian` MCP Server (`obsidian mcp`)

`@bitefoundry/obsidian` ships a native stdio JSON-RPC 2.0 Model Context Protocol (MCP) server with **18 typed tools** operating directly on your local Obsidian vaults with zero cloud auth.

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

## Tool Catalog (18 Tools)

| # | Tool | Arguments | Purpose |
| :-: | :--- | :--- | :--- |
| 1 | `list_vaults` | *(none)* | List auto-discovered & registered Obsidian vaults with note counts |
| 2 | `search_notes` | `query`, `limit?`, `vault?` | SQLite FTS5 search with `<3`-char Korean/CJK `LIKE` fallback |
| 3 | `read_note` | `note`, `vault?` | Read raw Markdown, frontmatter, tags, headings, tasks, backlinks & outgoing links |
| 4 | `list_notes` | `folder?`, `vault?` | List notes in the vault, optionally filtered by folder prefix |
| 5 | `create_note` | `title`, `folder?`, `content?`, `tags?`, `vault?` | Create a new `.md` note atomically with YAML frontmatter |
| 6 | `append_note` | `note`, `text`, `vault?` | Append Markdown text, tasks, or callouts atomically |
| 7 | `update_frontmatter` | `note`, `properties`, `vault?` | Merge or delete (`null`) YAML frontmatter keys preserving body |
| 8 | `delete_note` | `note`, `confirm` (`true`), `vault?` | Safely move a note into `<vault>/.trash/` (requires `confirm: true`) |
| 9 | `daily_note` | `date?`, `append?`, `vault?` | Open or create today's Daily Note and optionally append text |
| 10 | `get_backlinks` | `note`, `vault?` | Return incoming linked mentions (backlinks) with snippet context |
| 11 | `get_outgoing_links` | `note`, `vault?` | Return outgoing wikilinks and embeds (resolved & unresolved) |
| 12 | `get_local_graph` | `note?`, `depth?`, `vault?` | Return local BFS neighborhood (or global vault graph when `note` omitted) |
| 13 | `list_tags` | `vault?` | List all inline and frontmatter `#tags` with note counts |
| 14 | `find_by_tag` | `tag`, `vault?` | Find all notes matching a tag (including nested subtags) |
| 15 | `list_tasks` | `status?`, `note?`, `vault?` | List Markdown checklist tasks (`all`, `open`, or `done`) |
| 16 | `toggle_task` | `note`, `line`, `vault?` | Surgically toggle `- [ ]` <-> `- [x]` at exact 1-based line number |
| 17 | `find_orphans` | `vault?` | Find orphan notes with 0 backlinks and 0 outgoing links |
| 18 | `reindex_vault` | `vault?` | Trigger immediate incremental scan & SQLite FTS5/graph reindex |
