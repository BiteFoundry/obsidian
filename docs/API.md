# `@bitefoundry/obsidian` Local HTTP JSON API

Start the local HTTP JSON server (default `127.0.0.1:8792`):

```bash
# Prints a random Bearer token for this run on stderr:
bitefoundry-obsidian serve --bind 127.0.0.1 --port 8792
# Or choose the token yourself:
bitefoundry-obsidian serve --port 8792 --token "my-local-secret"
```

## Access control

Every `/api/v1/*` request needs `Authorization: Bearer <token>`. `/health` needs no token.

| Rule | Behavior |
| :--- | :--- |
| Token | Without `--token`, the server generates a 32-byte random token for the run and prints it once on stderr (`obsidian HTTP API token (generated for this run): ...`). Pass `--allow-keyless` to serve a loopback address with no token at all. |
| Non-loopback bind | `--bind 0.0.0.0` (or any non-loopback address) refuses to start without `--token`; `--allow-keyless` does not lift this. |
| `Host` header | Must be `127.0.0.1`, `localhost`, `[::1]`, or the `--bind` address. Add other names (a LAN IP or host name clients use) with `--allow-host <host>`. Anything else gets `403 Host not allowed`. |
| `Origin` header | Requests without `Origin` (curl, agents, native clients) pass. Browser requests pass only from loopback origins or an origin listed with `--allow-origin <origin>` (alias `--cors-origin`); others get `403 Origin not allowed`. |
| CORS | No `Access-Control-Allow-*` headers are sent by default. `--allow-origin https://app.example.com` (repeatable, exact match, no wildcards) enables CORS for that origin with `GET`/`POST` and the `Authorization`/`Content-Type` headers. |
| `vault` parameter | Accepts the vault the server was started with and registered or auto-discovered vaults (name, ID, or path; see `GET /api/v1/vaults`). Any other directory returns `404` and is never scanned. Register a vault first with `bitefoundry-obsidian vaults add <path>`. |

## Endpoints

| Method | Path | Description |
| :--- | :--- | :--- |
| `GET` | `/health` | Service health, version, active vault, and indexed note count |
| `GET` | `/api/v1/vaults` | List all auto-discovered and registered Obsidian vaults |
| `GET` | `/api/v1/search?q=...&limit=25&vault=...` | Full-text FTS5 search; Korean/CJK queries also match inside longer words (`회의록` finds `주간회의록`) |
| `GET` | `/api/v1/notes?folder=...&vault=...` | List notes in the vault, optionally filtered by folder prefix |
| `GET` | `/api/v1/notes/{*path}?vault=...` | Read full note Markdown, frontmatter, headings, tasks, backlinks & outgoing links |
| `POST` | `/api/v1/notes` | Create a new Markdown note atomically (`{ title, folder?, content?, tags?, vault? }`) |
| `POST` | `/api/v1/notes/append` | Append Markdown text to an existing note (`{ note, text, vault? }`) |
| `POST` | `/api/v1/daily` | Open or create today's Daily Note and optionally append (`{ date?, append?, vault? }`) |
| `GET` | `/api/v1/backlinks?note=...&vault=...` | Incoming linked mentions (backlinks) with surrounding context snippets |
| `GET` | `/api/v1/graph?note=...&depth=2&vault=...` | Local BFS knowledge graph (or global vault graph when `note` is omitted) |
| `GET` | `/api/v1/tags?tag=...&vault=...` | List all tags with counts, or list notes matching `tag` |
| `GET` | `/api/v1/tasks?status=open\|done\|all&note=...` | List Markdown checklist tasks across vault or within one note |
| `POST` | `/api/v1/tasks/toggle` | Surgically toggle `- [ ]` <-> `- [x]` at 1-based line (`{ note, line, vault? }`) |
| `POST` | `/api/v1/reindex` | Trigger immediate incremental filesystem scan & SQLite index refresh |

HTTP response shapes are unchanged: list endpoints return full arrays and note reads return full `NoteDetail` objects. Shared domain updates are reflected in field values: outgoing links include `status` (and `candidates` when ambiguous), graph responses clamp `depth` to `1..5` and report distinct-source `in_degree` and distinct-target `out_degree` (excluding image embeds and self-links), and ambiguous note references return `404` with the `Ambiguous note reference ...` error message.

See **[openapi.yaml](openapi.yaml)** for the full OpenAPI 3.1 specification.

## Daily notes

`POST /api/v1/daily`, the MCP `daily_note` tool, the CLI `daily` command and the TUI `D` key all name the note the way the vault's `.obsidian/daily-notes.json` says:

- **Folder**: `"folder": ""` (or `"/"`) puts daily notes at the vault root. When the file or the `folder` key is missing, the folder is `Daily Notes`.
- **Format**: the Moment tokens `YYYY` `YY` `MMMM` `MMM` `MM` `M` `DDDD` `DDD` `DD` `Do` `D` `dddd` `ddd` `dd` `d` `Q` `WW` `W` `GGGG` and `[literal text]` are supported, so `YYYY-MM-DD dddd` gives `2026-11-23 Monday.md`. A format containing `/` (for example `YYYY/MM/YYYY-MM-DD`) creates the folders in between. Other characters, including time tokens such as `HH`, are copied as written.
- **`date`**: a `YYYY-MM-DD` value selects that day and is formatted with the vault's format. Any other value is used as the file name as given.

Known limitation: month and weekday names are always English. Obsidian writes them in the app language, so a vault whose format uses `MMM`, `MMMM`, `ddd` or `dddd` under a non-English Obsidian gets a differently named file here.
