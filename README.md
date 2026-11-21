<div align="center">

# `@bitefoundry/obsidian`

**Obsidian-native terminal workspace, interactive knowledge graph & AI agent toolkit — Zero-Auth Local Vaults, Backlinks, Braille Graph View, CLI · TUI · HTTP API · MCP.**

[![npm version](https://img.shields.io/npm/v/%40bitefoundry%2Fobsidian?style=flat-square&color=7C3AED)](https://www.npmjs.com/package/@bitefoundry/obsidian)
[![platforms](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-24292F?style=flat-square)](docs/NPM.md)
[![Rust & Ratatui](https://img.shields.io/badge/built%20with-Rust%20%2B%20Ratatui-DEA584?style=flat-square&logo=rust)](https://github.com/BiteFoundry/obsidian/blob/master/Cargo.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-00875A?style=flat-square)](package.json)

<br />

![Obsidian TUI Demo](https://raw.githubusercontent.com/BiteFoundry/obsidian/master/assets/demo.gif)

</div>

---

## Highlights

| Pillar | Capabilities |
| :--- | :--- |
| **Zero-Auth Native Vault Discovery** | Automatically discovers every local Obsidian vault from `%APPDATA%\obsidian\obsidian.json` (Windows), `~/Library/Application Support/obsidian/obsidian.json` (macOS), or `~/.config/obsidian/obsidian.json` (Linux) — 100% local-first Markdown files on disk, zero API keys or cloud auth required. |
| **Obsidian-Native 3-Pane TUI** | Signature Obsidian purple (`#a78bfa` / `#7c3aed`) Minimal-Chrome workspace (`bitefoundry-obsidian`): collapsible **5-Tab Left Sidebar (`[`)** (`1 Files · 2 Tags · 3 Bookmarks · 4 Outline · 5 Tasks`), **Live Note Canvas** with YAML Frontmatter pills, `#tag` chips, semantic Callout cards, interactive `- [ ]` checkboxes (`Space`/`x` updates the `.md` file atomically), clickable `[[wikilinks]]`, inline HD `![[image.png]]` previews (`v`), **Right Backlinks & Outgoing Inspector (`B`)**, **Quick Switcher (`Ctrl+O`)**, **Daily Notes (`D`)**, and **5 curated palettes** (`Obsidian Dark`, `Catppuccin Mocha`, `Tokyo Night`, `Nord`, `Warm Paper`). |
| **Interactive Knowledge Graph (`G`)** | Force-directed & radial **Braille Canvas Knowledge Graph** rendered right inside the terminal: purple focal edges, 1-hop / 2-hop / unresolved ghost nodes, Local vs Global scope (`l`), hop depth (`+/-`), node cycling (`j`/`k`/`Tab`), and `Enter` jump-to-note. |
| **SQLite WAL + FTS5 + 4-in-1 Agent Interface** | Incremental `mtime_ms` + `size_bytes` filesystem scanner powering SQLite WAL + FTS5 search (with automatic `<3`-char Korean/CJK `LIKE` fallback), background watcher IPC daemon (`bitefoundry-obsidian daemon`), scriptable `--json` CLI, local **HTTP JSON API** (`bitefoundry-obsidian serve`, `:8792`), and **stdio MCP server** (`bitefoundry-obsidian mcp`, **20 typed tools** with `confirm: true` `.trash/` safety guard). |

---

## Quick Start

```bash
# 1. Install the native CLI globally
npm i -g @bitefoundry/obsidian

# 2. Launch directly — auto-discovers your existing local Obsidian vaults!
bitefoundry-obsidian

# Or scaffold a starter local vault anywhere:
bitefoundry-obsidian init ~/Documents/SecondBrain
bitefoundry-obsidian vaults list
```

---

### Upgrade from 0.1.x

Update shell scripts from `obsidian notes list` or `obs notes list` to `bitefoundry-obsidian notes list`. In MCP settings, change `"command": "obsidian"` or `"command": "obs"` to `"command": "bitefoundry-obsidian"` and keep `"args": ["mcp"]`.

For npm, run `npm install -g @bitefoundry/obsidian`. Use a separate prefix when another program supplies an old command in the existing npm prefix. For a source install, run `cargo install --git https://github.com/BiteFoundry/obsidian --locked`. For a standalone install, download the asset named `bitefoundry-obsidian` for your platform. [Shared npm prefix](docs/NPM.md#keep-shared-commands-in-an-existing-npm-prefix) · [Standalone updater transition](docs/NPM.md#standalone-updater-transition)

## Keybindings Cheat Sheet

| Key | Action | Context |
| :--- | :--- | :--- |
| `Ctrl+O` / `Ctrl+K` / `Ctrl+P` | **Quick Switcher** & Command Palette (fuzzy notes + `>commands`) | Global |
| `G` | Toggle **Interactive Knowledge Graph View** (Braille force-directed canvas) | Global |
| `l` / `+` `-` / `j` `k` / `Enter` | Toggle Local/Global scope · Depth · Cycle nodes · Open node | Graph View |
| `D` | Open or create **Today's Daily Note** (reads `.obsidian/daily-notes.json`) | Global |
| `[` / `Ctrl+B` | Toggle collapsible Left Sidebar | Global |
| `1` `2` `3` `4` `5` | Switch Sidebar tabs: **Files · Tags · Bookmarks · Outline · Tasks** | Global |
| `B` | Toggle Right Inspector (**Linked Mentions / Backlinks** & Outgoing Links) | Global |
| `Space` / `x` | Toggle live `- [ ]` / `- [x]` checkbox surgically in the `.md` file | Canvas / Tasks |
| `Enter` / `Backspace` / `Alt+←` | Follow `[[wikilink]]` / Navigate back in note history stack | Canvas |
| `a` / `n` / `b` | Quick Append · New Note · Bookmark | Global |
| `e` / `Ctrl+E` | Edit inline · Open in `$VISUAL` or `$EDITOR` and reindex on success | Note |
| `u` / `h` / `v` / `y` | Link Extractor · Toggle Inline Hrefs · HD Image Preview · Copy Code/Note | Global |
| `Ctrl+V` / `Ctrl+F` | Paste clipboard PNG into `Attachments/` · File Picker (`z` zoxide jump) | Global |
| `Ctrl+S` / `Ctrl+,` / `.` / `?` | Vault Switcher · Settings (5 Palettes) · Context Menu · Keybinding Help | Global |

---

## CLI, HTTP API & MCP

### CLI (`--json` & `--vault` ready)

```bash
# Discover, register, scaffold & reindex local vaults
bitefoundry-obsidian vaults list --json
bitefoundry-obsidian vaults add ~/Vaults/Work --name work
bitefoundry-obsidian vaults use work
bitefoundry-obsidian init ~/Vaults/Starter
bitefoundry-obsidian reindex

# Search, read, author, daily notes & surgical checkbox toggles
bitefoundry-obsidian search "Architecture" --json
bitefoundry-obsidian notes list --folder Projects --json
bitefoundry-obsidian notes read "Q4 Architecture RFC"
bitefoundry-obsidian notes read "Q4 Architecture RFC" --json --compact
bitefoundry-obsidian notes create "Sprint Retro" --folder Notes --tags "retro,q4"
bitefoundry-obsidian notes append "Q4 Architecture RFC" "- [ ] Verify the release"
bitefoundry-obsidian notes replace "Q4 Architecture RFC" --find "Draft review" --replace "Review complete" --json
bitefoundry-obsidian notes move "Q4 Architecture RFC" "Archive/Q4 Architecture RFC" --json
bitefoundry-obsidian daily "--append=- [ ] Review PRs"
bitefoundry-obsidian tasks --open --json
bitefoundry-obsidian toggle-task "Q4 Architecture RFC" 14

# Knowledge Graph & Link Intelligence
bitefoundry-obsidian backlinks "Q4 Architecture RFC" --json
bitefoundry-obsidian links "Q4 Architecture RFC" --json
bitefoundry-obsidian graph "Q4 Architecture RFC" --depth 2 --json
bitefoundry-obsidian orphans --json
bitefoundry-obsidian tags architecture --json

# Background watcher daemon & diagnostics
bitefoundry-obsidian daemon start
bitefoundry-obsidian doctor --json
```

`notes read` prints raw Markdown. With `--json --compact`, it returns `rel_path`, `title`, and `raw_markdown`. Use `notes edit <NOTE>` for arbitrary body edits in your editor. `notes replace` edits a single literal match in the Markdown body. The JSON result gives `replaced`, `matches`, and `rel_path`.

### Local HTTP JSON API (`bitefoundry-obsidian serve`)

```bash
bitefoundry-obsidian serve --bind 127.0.0.1 --port 8792 --token "$API_TOKEN"
curl -H "Authorization: Bearer $API_TOKEN" "http://127.0.0.1:8792/api/v1/search?q=Architecture"
curl -H "Authorization: Bearer $API_TOKEN" "http://127.0.0.1:8792/api/v1/graph?note=Q4%20Architecture%20RFC&depth=2"
```

Full endpoint catalog and OpenAPI schema: **[docs/API.md](docs/API.md)** · **[docs/openapi.yaml](docs/openapi.yaml)**.

### MCP Server (`bitefoundry-obsidian mcp`)

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

Full tool catalog and argument schemas: **[docs/MCP.md](docs/MCP.md)** · npm packaging details: **[docs/NPM.md](docs/NPM.md)**.

`move_note` renames or moves a note within a vault and updates incoming wikilinks that need a new target. Use `append_note` and `replace_note_text` for Markdown, `toggle_task` for checkboxes, and `update_frontmatter` for YAML properties. [Editing surfaces and move examples](docs/MCP.md#read-and-edit-notes)

---

## `@bitefoundry` Enterprise Terminal Suite

| Package | Commands | Workspace | Interfaces |
| :--- | :--- | :--- | :--- |
| [`@bitefoundry/teams`](https://www.npmjs.com/package/@bitefoundry/teams) | `teams` | Microsoft Teams (Chats, Channels, Meetings & Threads) | TUI · CLI · HTTP · MCP |
| [`@bitefoundry/outlook`](https://www.npmjs.com/package/@bitefoundry/outlook) | `outlook` | Microsoft Outlook (Mail, Calendar & Contacts) | TUI · CLI · HTTP · MCP |
| [`@bitefoundry/onenote`](https://www.npmjs.com/package/@bitefoundry/onenote) | `onenote` | Microsoft OneNote (Notebooks, Sections & Pages) | TUI · CLI · HTTP · MCP |
| [`@bitefoundry/sharepoint`](https://www.npmjs.com/package/@bitefoundry/sharepoint) | `sharepoint`, `bitefoundry-onedrive` | Microsoft SharePoint & OneDrive (Sites, Libraries & Lists) | TUI · CLI · HTTP · MCP |
| [`@bitefoundry/atlassian`](https://www.npmjs.com/package/@bitefoundry/atlassian) | `atlassian`, `confluence`, `conf` | Atlassian Jira (Kanban, Sprints, JQL) & Confluence Wiki | TUI · CLI · HTTP · MCP |
| [`@bitefoundry/notion`](https://www.npmjs.com/package/@bitefoundry/notion) | `notion` | Notion (Editorial Pages, Interactive Blocks & Databases) | TUI · CLI · HTTP · MCP |
| **`@bitefoundry/obsidian`** | `bitefoundry-obsidian` | Obsidian (Local Vaults, Backlinks & Interactive Knowledge Graph) | TUI · CLI · HTTP · MCP |

---

> **Disclaimer:** Unofficial terminal client for Obsidian vaults. Not affiliated with, endorsed by, or sponsored by Dynalist Inc. (Obsidian).
