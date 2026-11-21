# `@bitefoundry/obsidian` Local HTTP JSON API

Start the local HTTP JSON server (default `127.0.0.1:8792`):

```bash
bitefoundry-obsidian serve --bind 127.0.0.1 --port 8792
# Optional Bearer authentication:
bitefoundry-obsidian serve --port 8792 --token "my-local-secret"
```

When `--token` is provided, include `Authorization: Bearer <token>` on all `/api/v1/*` requests.

## Endpoints

| Method | Path | Description |
| :--- | :--- | :--- |
| `GET` | `/health` | Service health, version, active vault, and indexed note count |
| `GET` | `/api/v1/vaults` | List all auto-discovered and registered Obsidian vaults |
| `GET` | `/api/v1/search?q=...&limit=25&vault=...` | Full-text FTS5 search (+ automatic `<3`-char Korean/CJK `LIKE` fallback) |
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
