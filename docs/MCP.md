# `@bitefoundry/obsidian` MCP Server (`bitefoundry-obsidian mcp`)

`@bitefoundry/obsidian` ships a native stdio JSON-RPC 2.0 Model Context Protocol (MCP) server with **20 typed tools** for local vaults.

## Configuration

Add to Claude Desktop, Cursor, Windsurf, or any MCP-compatible host:

```json
{
  "mcpServers": {
    "obsidian": {
      "command": "bitefoundry-obsidian",
      "args": ["mcp"]
    }
  }
}
```

Optionally pin a specific vault via `"args": ["--vault", "My Vault", "mcp"]` or `"env": { "OBSIDIAN_VAULT": "My Vault" }`.

The optional `vault` argument of each tool accepts the vault the server was started with and registered or auto-discovered vaults (name, ID, or path; see `list_vaults`). Any other directory is rejected and never scanned; register it first with `bitefoundry-obsidian vaults add <path>`.

Existing 0.1.x settings use `"command": "obsidian"` or `"command": "obs"`. Set `"command": "bitefoundry-obsidian"` to upgrade.

## Tool Catalog

| # | Tool | Arguments | Purpose |
| :-: | :--- | :--- | :--- |
| 1 | `list_vaults` | *(none)* | List registered & auto-discovered Obsidian vaults with note counts (scans every vault to refresh the counts, so the first call can be slow with many or large vaults) |
| 2 | `search_notes` | `query`, `limit?`, `vault?` | SQLite FTS5 search; Korean/CJK queries also match inside longer words (`limit` default 20, clamped `1..200`; response includes `limit`, `returned`, `has_more`, `next_cursor`) |
| 3 | `read_note` | `note`, `compact?`, `vault?` | Compact by default (`rel_path`, `title`, `raw_markdown`, `mtime_ms`, `omitted`, `hint`); pass `compact: false` for frontmatter, tags, headings, tasks, backlinks & outgoing links |
| 4 | `list_notes` | `folder?`, `limit?`, `cursor?`, `vault?` | Paginated note list (`limit` `1..500`, default 50; returns `count`, `returned`, `total`, `has_more`, `next_cursor`, `notes`) |
| 5 | `create_note` | `title`, `folder?`, `content?`, `tags?`, `vault?` | Create a new `.md` note atomically with YAML frontmatter (`tags` must be an array of strings and errors if `content` frontmatter already defines `tags`) |
| 6 | `append_note` | `note`, `text`, `vault?` | Append Markdown text, tasks, or callouts atomically |
| 7 | `update_frontmatter` | `note`, `properties`, `vault?` | Set or delete (`null`) top-level YAML properties |
| 8 | `delete_note` | `note`, `confirm` (`true`), `vault?` | Safely move a note into `<vault>/.trash/` (requires `confirm: true`) |
| 9 | `daily_note` | `date?`, `append?`, `vault?` | Open or create today's Daily Note and optionally append text (folder and file name follow `.obsidian/daily-notes.json`; a `YYYY-MM-DD` `date` is formatted with the vault's format, see [Daily notes](API.md#daily-notes)); calls with non-empty `append` return a receipt (`rel_path`, `created`, `appended`, `hint`), while calls without `append` return `rel_path`, `created`, `raw_markdown`, `omitted`, `hint` (call `read_note` with `compact: false` for metadata; each call with `append` appends again) |
| 10 | `get_backlinks` | `note`, `vault?` | Return incoming linked mentions (backlinks) with snippet context |
| 11 | `get_outgoing_links` | `note`, `vault?` | Return outgoing wikilinks and embeds with `status` (`resolved`, `attachment`, `ambiguous`, `missing`) and `candidates` |
| 12 | `get_local_graph` | `note?`, `depth?`, `vault?` | Return local BFS neighborhood (`depth` clamped `1..5`, applied `depth` returned; each node includes `status`: `resolved`, `ambiguous`, or `missing`; `in_degree` = distinct referring notes, `out_degree` = distinct targets excluding existing attachments and self-links; omitting `note` returns the full vault graph, which is large and slow on big vaults: pass `note` and a small `depth` when possible) |
| 13 | `list_tags` | `vault?` | List all inline and frontmatter `#tags` with note counts (aggregates over the whole vault) |
| 14 | `find_by_tag` | `tag`, `vault?` | Find all notes matching a tag (including nested subtags) |
| 15 | `list_tasks` | `status?`, `note?`, `limit?`, `cursor?`, `vault?` | Paginated checklist tasks (`all`, `open`, `done`; `limit` `1..500`, default 50; returns `count`, `returned`, `total`, `has_more`, `next_cursor`, `tasks`) |
| 16 | `toggle_task` | `note`, `line`, `expected_text?`, `expected_checked?`, `expected_mtime_ms?`, `vault?` | Toggle a Markdown checkbox at an exact 1-based line number, with optional conflict checks |
| 17 | `find_orphans` | `vault?` | Find orphan notes with 0 backlinks and 0 outgoing links (reads the whole note index; cost grows with vault size and the result is not paginated) |
| 18 | `reindex_vault` | `vault?` | Force a full verification rescan: re-reads and hashes every note and rebuilds link resolution. Slow on large vaults and rarely needed, because every other tool already syncs changed files first |
| 19 | `move_note` | `src`, `dst`, `vault?` | Move or rename a note within its vault, update incoming wikilinks/Markdown links, and rewrite relative Markdown links, images, and attachments inside the moved note so they keep pointing to the same files |
| 20 | `replace_note_text` | `note`, `find`, `replace`, `vault?` | Replace a literal string that occurs exactly once in the Markdown body |

## Read and edit notes

`read_note` returns a compact object by default (`rel_path`, `title`, `raw_markdown`, `mtime_ms`, `omitted`, and `hint`). Pass `{"note":"Projects/Review.md","compact":false}` for frontmatter, tags, headings, tasks, backlinks, and outgoing links (also including `mtime_ms`). You can pass the returned `mtime_ms` directly as `expected_mtime_ms` to `toggle_task` (for example `{"note":"Projects/Review.md","line":6,"expected_mtime_ms":1795261200000}`) to guard against concurrent edits.

Pass a vault-relative path for `note` (or `src`) whenever possible. If a title or alias matches multiple notes, tools return `Ambiguous note reference ...` and neither read nor modify any file. `get_outgoing_links` classifies every entry with `status`: `resolved` (note), `attachment` (existing image/attachment on disk), `ambiguous` (multiple matching notes, listed in `candidates`), or `missing`.

- For arbitrary Markdown edits, use `bitefoundry-obsidian notes edit <NOTE>`, TUI `e` for inline editing, or TUI `Ctrl+E` for an external editor.
- Use MCP `append_note` to add Markdown with its indentation and trailing spaces intact. For a checklist item, run `bitefoundry-obsidian notes append "Release Review" "- [ ] Verify the release"`.
- Use MCP `toggle_task` or `bitefoundry-obsidian toggle-task <NOTE> <LINE>` for a checkbox. To change YAML properties, call MCP `update_frontmatter` with a properties object such as `{"status":"ready","obsolete":null}`. This sets `status` and removes `obsolete`.
- For HTTP edit routes, see the append and task toggle entries in the [HTTP API catalog](API.md).

### Replace a sentence or paragraph

```json
{"note":"Projects/Review.md","find":"Draft review","replace":"Review complete"}
```

The tool replaces a literal match when it occurs exactly once in the Markdown body. Supply the original spaces and line endings when matching several lines. Pass an empty `replace` string to delete the match. Both `replace_note_text` and `bitefoundry-obsidian notes replace <NOTE> --find <TEXT> --replace <TEXT> --json` return `matches`, `replaced`, and `rel_path`. With zero or multiple matches, `replaced` is false and the note is preserved.

### Move or rename a note

Call `move_note` with these arguments, or run `bitefoundry-obsidian notes move "Release Review" "Archive/Release Review" --json`.

```json
{"src":"Release Review","dst":"Archive/Release Review","vault":"work"}
```

`src` accepts a vault-relative path, note title, or frontmatter alias. `dst` gives the new path and filename within the selected vault. The command adds `.md` when omitted, creates missing parent directories, and rejects an existing destination.

Both surfaces report `moved`, `from`, `to`, `updated_links_in_notes`, and `bookmark_preserved`. The link count is the number of files rewritten. Moves resolve a short wikilink from its source folder, then from a unique match in the vault. An ambiguous short wikilink stays as written. Matching wikilinks keep heading and block fragments, display text, and the embed marker. Moves also rewrite inbound Markdown links in other notes and relative Markdown destinations (notes, images, and attachments) inside the moved note so they continue pointing to the same files. Destinations rooted with `/` and vault-root-relative destinations stay as written as long as they still resolve to the same file. External URLs, links inside inline or fenced code, reference-style link definitions, and HTML tags are left untouched, and attachment files themselves are not moved. [Obsidian internal link formats](https://obsidian.md/help/links)
