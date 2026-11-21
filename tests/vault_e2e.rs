//! End-to-end integration tests for `@bitefoundry/obsidian`:
//! - Real temporary Obsidian vault with YAML frontmatter, wikilinks, aliases, embeds, callouts, tasks, headings
//! - Backlinks, outgoing links, unresolved links, local/global knowledge graph, orphans, daily notes
//! - Surgical task checkbox toggle file mutation & `.trash/` safe deletion
//! - SQLite WAL + FTS5 search + Korean `<3` char `LIKE` fallback
//! - HTTP API (`:8792` router) across all endpoints
//! - Stdio MCP JSON-RPC 2.0 tool discovery and vault operations
//! - `ratatui::TestBackend` rendering across 3-Pane Workspace, Tags, Daily Note, and Braille Knowledge Graph View!

use std::{fs, sync::Arc};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use obsidian_cli::{
    config::AppConfig,
    mcp::{tool_definitions, McpServer},
    models::SidebarTab,
    serve::{build_router, ServeState},
    storage::Storage,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::{
        self,
        action::Action,
        app::{AppState, CanvasMode, UiState},
    },
    vault::{
        discover_native_vaults_from,
        fs::{ensure_daily_note, toggle_task_at_line},
        init_starter_vault, vault_from_dir,
    },
};
use ratatui::{backend::TestBackend, Terminal};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tower_http::cors::CorsLayer;

fn seed_vault(root: &std::path::Path) {
    fs::create_dir_all(root.join("Projects")).unwrap();
    fs::create_dir_all(root.join("Daily Notes")).unwrap();
    fs::create_dir_all(root.join("Attachments")).unwrap();
    fs::create_dir_all(root.join(".obsidian")).unwrap();
    fs::write(
        root.join(".obsidian/daily-notes.json"),
        "{\"folder\":\"Daily Notes\",\"format\":\"YYYY-MM-DD\"}",
    )
    .unwrap();
    // 1x1 valid PNG in Attachments/diagram.png
    let png_bytes: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x60,
        0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x01, 0xE2, 0x26, 0x05, 0x9B, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    fs::write(root.join("Attachments/diagram.png"), png_bytes).unwrap();

    fs::write(
        root.join("Projects/Q4 Architecture RFC.md"),
        r#"---
status: active
aliases: [RFC-042]
tags: [architecture, rust]
---

# Q4 Architecture RFC

> [!tip] Fast Index
> Incremental scan with SQLite WAL + FTS5.

Links to [[Storage Engine|SQLite Engine]] and [[Unwritten Spec]].
Inline tag #project/q4 and code `#ignored`.

## Checklist

- [ ] Build Braille knowledge graph
- [x] Wire 18 MCP tools

![[diagram.png|400]]
"#,
    )
    .unwrap();

    fs::write(
        root.join("Storage Engine.md"),
        r#"---
tags: [storage, rust]
---

# Storage Engine

Supports [[RFC-042]] and Korean 검색 메모.

- [ ] Benchmark 10k notes
"#,
    )
    .unwrap();

    fs::write(
        root.join("Orphan Note.md"),
        "# Orphan Note\n\nCompletely isolated note.\n",
    )
    .unwrap();
}

#[test]
fn test_vault_engine_backlinks_graph_orphans_daily_task_toggle_and_korean_search() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("AcmeVault");
    fs::create_dir_all(&root).unwrap();
    seed_vault(&root);

    // Native obsidian.json discovery
    let obs_json = tmp.path().join("obsidian.json");
    fs::write(
        &obs_json,
        json!({
            "vaults": {
                "v1": { "path": root.to_string_lossy(), "ts": 1732000000i64, "open": true }
            }
        })
        .to_string(),
    )
    .unwrap();
    let discovered = discover_native_vaults_from(&obs_json);
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].name, "AcmeVault");

    let vault = vault_from_dir(&root, Some("AcmeVault"));
    let storage = Storage::open_in_memory().unwrap();
    let stats = storage.sync_vault(&vault).unwrap();
    assert_eq!(stats.total_notes, 3);
    assert_eq!(stats.updated, 3);

    // Note detail hydration (resolved wikilinks, image embeds, frontmatter, tasks)
    let detail = storage
        .load_note_detail(&vault, "RFC-042")
        .expect("resolve via alias");
    assert_eq!(detail.summary.rel_path, "Projects/Q4 Architecture RFC.md");
    assert_eq!(detail.images.len(), 1);
    assert!(detail.images[0].abs_path.is_file());
    assert_eq!(detail.backlinks.len(), 1);
    assert_eq!(detail.backlinks[0].source_path, "Storage Engine.md");
    assert!(detail
        .outgoing_links
        .iter()
        .any(|l| l.target_raw == "Storage Engine" && l.resolved));
    assert!(detail
        .outgoing_links
        .iter()
        .any(|l| l.target_raw == "Unwritten Spec" && !l.resolved));

    // Local graph
    let graph = storage
        .get_graph(&vault.id, Some("Projects/Q4 Architecture RFC.md"), 2)
        .unwrap();
    assert!(graph.nodes.len() >= 3);
    assert!(graph.edges.len() >= 2);

    // Orphans
    let orphans = storage.find_orphans(&vault.id).unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].rel_path, "Orphan Note.md");

    // Surgical task toggle on disk
    let open_task = detail.parsed.tasks.iter().find(|t| !t.checked).unwrap();
    let rfc_abs = root.join("Projects/Q4 Architecture RFC.md");
    let now_checked = toggle_task_at_line(&rfc_abs, open_task.line_no).unwrap();
    assert!(now_checked);
    let updated_raw = fs::read_to_string(&rfc_abs).unwrap();
    assert!(updated_raw.contains("- [x] Build Braille knowledge graph"));

    // Daily note creation
    let (daily_rel, daily_abs, created) = ensure_daily_note(&vault, Some("2026-11-20")).unwrap();
    assert!(created);
    assert_eq!(daily_rel, "Daily Notes/2026-11-20.md");
    assert!(daily_abs.is_file());
    let (_, _, created_again) = ensure_daily_note(&vault, Some("2026-11-20")).unwrap();
    assert!(!created_again);

    // Korean <3 char LIKE fallback + standard FTS5
    let kr = storage.search_notes(&vault.id, "검색", 10).unwrap();
    assert_eq!(kr.len(), 1);
    assert_eq!(kr[0].rel_path, "Storage Engine.md");

    let en = storage.search_notes(&vault.id, "Incremental", 10).unwrap();
    assert_eq!(en.len(), 1);
    assert_eq!(en[0].rel_path, "Projects/Q4 Architecture RFC.md");
}

#[tokio::test]
async fn test_http_api_8792_endpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("HttpVault");
    fs::create_dir_all(&root).unwrap();
    seed_vault(&root);

    let vault = vault_from_dir(&root, Some("HttpVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let state = Arc::new(ServeState {
        cfg,
        default_vault: vault.clone(),
        storage,
        token: None,
    });
    let app = build_router(state).layer(CorsLayer::permissive());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // Health
    let h: Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(h["ok"], true);
    assert_eq!(h["notes"], 3);

    // Search (FTS5 + Korean LIKE)
    let s: Value = client
        .get(format!("{base}/api/v1/search?q=%EA%B2%80%EC%83%89"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["count"], 1);

    // Read note via wildcard path
    let r: Value = client
        .get(format!(
            "{base}/api/v1/notes/Projects/Q4%20Architecture%20RFC.md"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["ok"], true);
    assert_eq!(r["note"]["summary"]["title"], "Q4 Architecture RFC");

    // Create + Append note
    let cr = client
        .post(format!("{base}/api/v1/notes"))
        .json(&json!({ "title": "API Created", "tags": ["http", "test"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(cr.status(), StatusCode::CREATED);

    let ap: Value = client
        .post(format!("{base}/api/v1/notes/append"))
        .json(&json!({ "note": "API Created", "text": "- [ ] Verify append" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ap["ok"], true);

    // Backlinks + Graph + Tags + Tasks + Daily + Reindex
    let bl: Value = client
        .get(format!("{base}/api/v1/backlinks?note=RFC-042"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(bl["count"], 1);

    let gr: Value = client
        .get(format!("{base}/api/v1/graph?note=RFC-042&depth=2"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(gr["graph"]["nodes"].as_array().unwrap().len() >= 3);

    let tags: Value = client
        .get(format!("{base}/api/v1/tags"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(tags["count"].as_u64().unwrap() >= 4);

    let tasks: Value = client
        .get(format!("{base}/api/v1/tasks?status=open"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first_task = &tasks["tasks"][0];
    let tog: Value = client
        .post(format!("{base}/api/v1/tasks/toggle"))
        .json(&json!({ "note": first_task["note_path"], "line": first_task["line_no"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tog["checked"], true);

    let daily: Value = client
        .post(format!("{base}/api/v1/daily"))
        .json(&json!({ "date": "2026-11-20", "append": "- [ ] HTTP daily check" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(daily["ok"], true);

    let re: Value = client
        .post(format!("{base}/api/v1/reindex"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(re["ok"], true);

    // Unused imports sanity check avoided by exercising Request/Body cleanly
    let _dummy_req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    server.abort();
}

#[test]
fn test_mcp_catalog_and_core_tools_json_rpc() {
    assert_eq!(tool_definitions().len(), 20);

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("McpVault");
    fs::create_dir_all(&root).unwrap();
    seed_vault(&root);

    let vault = vault_from_dir(&root, Some("McpVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    let mcp = McpServer::new(cfg, vault.clone(), storage);

    // Initialize + tools/list
    let init = mcp
        .handle_rpc(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }))
        .unwrap();
    assert_eq!(
        init["result"]["serverInfo"]["name"],
        "@bitefoundry/obsidian"
    );

    let list = mcp
        .handle_rpc(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
        .unwrap();
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 20);

    // Exercise existing tools. Moving notes has a separate byte-level fixture.
    assert!(mcp.dispatch_tool("list_vaults", &json!({})).is_ok());
    assert!(mcp.dispatch_tool("list_notes", &json!({})).is_ok());
    assert!(mcp
        .dispatch_tool("search_notes", &json!({ "query": "검색" }))
        .is_ok());
    assert!(mcp
        .dispatch_tool("read_note", &json!({ "note": "RFC-042" }))
        .is_ok());
    let compact = mcp
        .dispatch_tool("read_note", &json!({ "note": "RFC-042", "compact": true }))
        .unwrap();
    let path = "Projects/Q4 Architecture RFC.md";
    let markdown = fs::read_to_string(root.join(path)).unwrap();
    assert_eq!(compact["rel_path"], path);
    assert_eq!(compact["title"], "Q4 Architecture RFC");
    assert_eq!(compact["raw_markdown"], markdown);
    assert!(compact["mtime_ms"].as_i64().is_some());
    assert!(compact["omitted"].is_object());
    assert_eq!(
        compact["hint"],
        format!("read_note {}", json!({ "note": path, "compact": false }))
    );
    assert!(mcp
        .dispatch_tool(
            "create_note",
            &json!({ "title": "MCP Created", "tags": ["mcp"] })
        )
        .is_ok());
    assert!(mcp
        .dispatch_tool(
            "append_note",
            &json!({ "note": "MCP Created", "text": "- [ ] MCP task" })
        )
        .is_ok());
    mcp.dispatch_tool(
        "append_note",
        &json!({ "note": "MCP Created", "text": "    line one\nline two  " }),
    )
    .unwrap();
    assert!(fs::read(root.join("MCP Created.md"))
        .unwrap()
        .ends_with(b"    line one\nline two  \n"));
    assert!(mcp
        .dispatch_tool(
            "update_frontmatter",
            &json!({ "note": "MCP Created", "properties": { "status": "verified" } })
        )
        .is_ok());
    assert!(mcp
        .dispatch_tool("daily_note", &json!({ "date": "2026-11-20" }))
        .is_ok());
    assert!(mcp
        .dispatch_tool("get_backlinks", &json!({ "note": "RFC-042" }))
        .is_ok());
    assert!(mcp
        .dispatch_tool("get_outgoing_links", &json!({ "note": "RFC-042" }))
        .is_ok());
    assert!(mcp
        .dispatch_tool("get_local_graph", &json!({ "note": "RFC-042", "depth": 2 }))
        .is_ok());
    assert!(mcp.dispatch_tool("list_tags", &json!({})).is_ok());
    assert!(mcp
        .dispatch_tool("find_by_tag", &json!({ "tag": "architecture" }))
        .is_ok());
    let tasks = mcp
        .dispatch_tool("list_tasks", &json!({ "status": "open" }))
        .unwrap();
    let t0 = &tasks["tasks"][0];
    let toggled = mcp
        .dispatch_tool(
            "toggle_task",
            &json!({ "note": t0["note_path"], "line": t0["line_no"] }),
        )
        .unwrap();
    assert_eq!(toggled["checked"], true);
    assert!(mcp.dispatch_tool("find_orphans", &json!({})).is_ok());
    assert!(mcp.dispatch_tool("reindex_vault", &json!({})).is_ok());

    // Delete guard: confirm: false fails, confirm: true moves to .trash/
    assert!(mcp
        .dispatch_tool(
            "delete_note",
            &json!({ "note": "MCP Created", "confirm": false })
        )
        .is_err());
    let del = mcp
        .dispatch_tool(
            "delete_note",
            &json!({ "note": "MCP Created", "confirm": true }),
        )
        .unwrap();
    assert_eq!(del["deleted"], true);
}

#[test]
fn mcp_move_note_rewrites_link_targets_and_preserves_other_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(root.join("Inbox")).unwrap();
    fs::create_dir_all(root.join("Elsewhere")).unwrap();
    let original = "---\r\n# metadata\naliases: [Release Alias]\r\nid: '00127'\ntracking: {owner: Ops}\r\n---\r\n\n# Scope\r\nBody ^keep";
    let incoming = "---\r\nstate: 'draft'\n---\r\n[[Draft#Scope|Read this]]\n![[Draft#^keep|Embed]]\r\n[[Release Alias#Scope|Alias]]\n`[[Draft|Code]]`\r\n```\n[[Draft|Fence]]\r\n```\n[Markdown](Inbox/Draft.md#Scope)";
    let unrelated = "# Other\r\n[[Elsewhere/Review|Other target]]\n";
    fs::write(root.join("Inbox/Draft.md"), original).unwrap();
    fs::write(root.join("Incoming.md"), incoming).unwrap();
    fs::write(root.join("Elsewhere/Review.md"), unrelated).unwrap();

    let vault = vault_from_dir(&root, Some("fixture"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let mcp = McpServer::new(cfg, vault.clone(), Storage::open_in_memory().unwrap());
    mcp.storage
        .set_bookmark(&vault.id, "Inbox/Draft.md", true)
        .unwrap();
    let tools = tool_definitions();
    let schema = &tools
        .iter()
        .find(|tool| tool["name"] == "move_note")
        .unwrap()["inputSchema"];
    assert_eq!(schema["required"], json!(["src", "dst"]));
    assert_eq!(schema["properties"]["vault"]["type"], "string");

    let call_move = |arguments| {
        mcp.handle_rpc(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "move_note", "arguments": arguments},
        }))
        .unwrap()["result"]
            .clone()
    };
    for arguments in [
        json!({"src":"Release Alias"}),
        json!({"src":"Missing","dst":"Archive/Review"}),
        json!({"src":"Release Alias","dst":"Elsewhere/Review"}),
        json!({"src":"Release Alias","dst":"../Outside"}),
    ] {
        assert_eq!(call_move(arguments)["isError"], true);
    }
    #[cfg(windows)]
    {
        let outside = tmp.path().join("Outside.md");
        assert_eq!(
            call_move(json!({"src":"Release Alias","dst":outside}))["isError"],
            true
        );
        assert!(!outside.exists());
    }
    assert!(!tmp.path().join("Outside.md").exists());
    assert_eq!(
        fs::read(root.join("Inbox/Draft.md")).unwrap(),
        original.as_bytes()
    );
    assert_eq!(
        fs::read(root.join("Incoming.md")).unwrap(),
        incoming.as_bytes()
    );
    assert_eq!(
        fs::read(root.join("Elsewhere/Review.md")).unwrap(),
        unrelated.as_bytes()
    );

    let result = call_move(json!({"src":"Release Alias","dst":"Archive/Review","vault":"fixture"}));
    assert_eq!(result["isError"], false, "{result}");
    let payload: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        payload,
        json!({
            "moved": true, "from": "Inbox/Draft.md", "to": "Archive/Review.md",
            "updated_links_in_notes": 1, "bookmark_preserved": true
        })
    );
    assert!(!root.join("Inbox/Draft.md").exists());
    assert_eq!(
        fs::read(root.join("Archive/Review.md")).unwrap(),
        original.as_bytes()
    );
    let expected = incoming
        .replace("[[Draft#Scope", "[[Archive/Review#Scope")
        .replace("[[Draft#^keep", "[[Archive/Review#^keep")
        .replace("(Inbox/Draft.md#Scope)", "(Archive/Review.md#Scope)");
    assert_eq!(
        fs::read(root.join("Incoming.md")).unwrap(),
        expected.as_bytes()
    );
    assert_eq!(
        fs::read(root.join("Elsewhere/Review.md")).unwrap(),
        unrelated.as_bytes()
    );
    assert!(
        mcp.storage
            .get_note_summary(&vault.id, "Archive/Review.md")
            .unwrap()
            .unwrap()
            .is_bookmarked
    );
    let note = mcp
        .dispatch_tool(
            "read_note",
            &json!({"note":"Archive/Review.md","compact":false}),
        )
        .unwrap();
    assert_eq!(note["frontmatter"]["id"], "00127");
    assert_eq!(note["frontmatter"]["tracking"], json!({"owner":"Ops"}));
    assert!(note["backlinks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["source_path"] == "Incoming.md"));
}

#[test]
fn cli_and_mcp_move_respect_source_folder_and_leave_ambiguous_links_verbatim() {
    for use_mcp in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        for folder in ["A", "B"] {
            fs::create_dir_all(root.join(folder)).unwrap();
            fs::write(root.join(folder).join("Shared.md"), "# Shared\n").unwrap();
        }
        fs::write(root.join("A/Shared.md"), "See [[./Ref#Top]]\n").unwrap();
        fs::write(
            root.join("A/Ref.md"),
            "# Top\r\n[[Shared#Heading|표시]] ![[./Shared#^block|Embed]]\n",
        )
        .unwrap();
        fs::write(root.join("B/Ref.md"), "[[Shared]] [[B/Shared]]\n").unwrap();
        fs::write(root.join("Root.md"), "[[Shared]] [[A/Shared]]\n").unwrap();

        let result = if use_mcp {
            let vault = vault_from_dir(&root, Some("fixture"));
            let mut cfg = AppConfig::default();
            cfg.upsert_vault(vault.clone(), true);
            let mcp = McpServer::new(cfg, vault, Storage::open_in_memory().unwrap());
            mcp.dispatch_tool(
                "move_note",
                &json!({"src": "A/Shared.md", "dst": "New/A-Moved.md"}),
            )
            .unwrap()
        } else {
            let output = isolated_cli(tmp.path(), &root)
                .args(["--json", "notes", "move", "A/Shared.md", "New/A-Moved.md"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).unwrap()
        };

        assert_eq!(result["updated_links_in_notes"], 3);
        assert_eq!(
            fs::read(root.join("A/Ref.md")).unwrap(),
            "# Top\r\n[[A-Moved#Heading|표시]] ![[A-Moved#^block|Embed]]\n".as_bytes()
        );
        assert_eq!(
            fs::read(root.join("B/Ref.md")).unwrap(),
            b"[[Shared]] [[B/Shared]]\n"
        );
        assert_eq!(
            fs::read(root.join("Root.md")).unwrap(),
            b"[[Shared]] [[A-Moved]]\n"
        );
        assert_eq!(
            fs::read(root.join("New/A-Moved.md")).unwrap(),
            b"See [[A/Ref#Top]]\n"
        );
    }
}

fn isolated_cli(test_root: &std::path::Path, vault: &std::path::Path) -> std::process::Command {
    let mut command = isolated_cli_base(test_root);
    command.arg("--vault").arg(vault);
    command
}

fn isolated_cli_base(test_root: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_bitefoundry-obsidian"));
    command
        .env("OBSIDIAN_CONFIG_DIR", test_root.join("config"))
        .env("OBSIDIAN_DATA_DIR", test_root.join("data"))
        .env("OBSIDIAN_APP_CONFIG_PATH", test_root.join("obsidian.json"));
    command
}

#[test]
fn cli_and_mcp_move_rewrites_inline_markdown_links_and_preserves_bytes() {
    for use_mcp in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        fs::create_dir_all(root.join("Projects")).unwrap();
        fs::create_dir_all(root.join("Notes")).unwrap();

        let plan_body = "# Falsify Plan\n\n## Scope\n\nBody.\n";
        fs::write(root.join("Projects/Falsify Plan.md"), plan_body).unwrap();
        fs::write(root.join("Notes/Sibling.md"), "# Sibling\n\n## Top\n").unwrap();

        // Root index with both inline Markdown link and wikilink (matching audit case)
        let index_before = "[링크](Projects/Falsify%20Plan.md)\n[[Falsify Plan|Wiki]]\n";
        fs::write(root.join("Index.md"), index_before).unwrap();

        // Markdown-only note (no `[[` at all) with titles, fragments, angle brackets, code, URLs, CRLF, no trailing newline
        let md_only_before = concat!(
            "[계획](../Projects/Falsify%20Plan.md#Scope \"제목 (유지)\")\r\n",
            "[각괄호](<../Projects/Falsify Plan.md#Heading> 'single')\n",
            "[괄호제목](../Projects/Falsify%20Plan.md (paren))\r\n",
            "[형제](Sibling.md#Top)\n",
            "[루트](/Projects/Falsify%20Plan.md)\r\n",
            "`[코드](../Projects/Falsify%20Plan.md)`\n",
            "```md\r\n",
            "[펜스](../Projects/Falsify%20Plan.md)\r\n",
            "```\n",
            "[외부](https://example.com/Projects/Falsify%20Plan.md)\r\n",
            "[메일](mailto:team@example.invalid)"
        );
        fs::write(root.join("Notes/OnlyMarkdown.md"), md_only_before).unwrap();

        // Same-depth referrer whose link stays valid when moved to a sibling folder
        let same_depth_before = "[같은깊이](../Projects/Falsify%20Plan.md)\n";
        fs::write(root.join("Notes/SameDepth.md"), same_depth_before).unwrap();

        let run_move = |src: &str, dst: &str| -> Value {
            if use_mcp {
                let vault = vault_from_dir(&root, Some("fixture"));
                let mut cfg = AppConfig::default();
                cfg.upsert_vault(vault.clone(), true);
                let mcp = McpServer::new(cfg, vault, Storage::open_in_memory().unwrap());
                mcp.dispatch_tool("move_note", &json!({ "src": src, "dst": dst }))
                    .unwrap()
            } else {
                let output = isolated_cli(tmp.path(), &root)
                    .args(["--json", "notes", "move", src, dst])
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                serde_json::from_slice(&output.stdout).unwrap()
            }
        };

        // 1. Move target note -> updates Index.md, Notes/OnlyMarkdown.md, and Notes/SameDepth.md
        let moved = run_move("Projects/Falsify Plan.md", "Archive/Renamed Plan.md");
        assert_eq!(moved["updated_links_in_notes"], 3);
        assert!(!root.join("Projects/Falsify Plan.md").exists());
        assert!(root.join("Archive/Renamed Plan.md").is_file());

        let index_after = fs::read_to_string(root.join("Index.md")).unwrap();
        assert_eq!(
            index_after,
            "[링크](Archive/Renamed%20Plan.md)\n[[Renamed Plan|Wiki]]\n"
        );
        // Verify both rewritten targets actually open the moved file on disk.
        let md_dest = index_after
            .lines()
            .next()
            .unwrap()
            .strip_prefix("[링크](")
            .unwrap()
            .strip_suffix(')')
            .unwrap();
        let md_decoded = urlencoding::decode(md_dest).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(md_decoded.as_ref())).unwrap(),
            plan_body
        );

        let vault = vault_from_dir(&root, Some("fixture"));
        let storage = Storage::open_in_memory().unwrap();
        storage.sync_vault(&vault).unwrap();
        assert_eq!(
            storage
                .resolve_note_path(&vault.id, "Renamed Plan")
                .unwrap(),
            "Archive/Renamed Plan.md"
        );

        let md_only_after_target_move = concat!(
            "[계획](../Archive/Renamed%20Plan.md#Scope \"제목 (유지)\")\r\n",
            "[각괄호](<../Archive/Renamed Plan.md#Heading> 'single')\n",
            "[괄호제목](../Archive/Renamed%20Plan.md (paren))\r\n",
            "[형제](Sibling.md#Top)\n",
            "[루트](../Archive/Renamed%20Plan.md)\r\n",
            "`[코드](../Projects/Falsify%20Plan.md)`\n",
            "```md\r\n",
            "[펜스](../Projects/Falsify%20Plan.md)\r\n",
            "```\n",
            "[외부](https://example.com/Projects/Falsify%20Plan.md)\r\n",
            "[메일](mailto:team@example.invalid)"
        );
        assert_eq!(
            fs::read(root.join("Notes/OnlyMarkdown.md")).unwrap(),
            md_only_after_target_move.as_bytes()
        );
        assert_eq!(
            fs::read(root.join("Notes/SameDepth.md")).unwrap(),
            "[같은깊이](../Archive/Renamed%20Plan.md)\n".as_bytes()
        );

        // 2. Move a referring note to a deeper folder -> updates its own broken relative links,
        //    leaving already-valid rooted link intact.
        let rooted_addition = format!(
            "{}\n[절대](/Archive/Renamed%20Plan.md)",
            md_only_after_target_move
        );
        fs::write(root.join("Notes/OnlyMarkdown.md"), &rooted_addition).unwrap();
        let moved_ref = run_move("Notes/OnlyMarkdown.md", "Deep/Nested/OnlyMarkdown.md");
        assert_eq!(moved_ref["updated_links_in_notes"], 1);
        let md_only_after_self_move = concat!(
            "[계획](../../Archive/Renamed%20Plan.md#Scope \"제목 (유지)\")\r\n",
            "[각괄호](<../../Archive/Renamed Plan.md#Heading> 'single')\n",
            "[괄호제목](../../Archive/Renamed%20Plan.md (paren))\r\n",
            "[형제](../../Notes/Sibling.md#Top)\n",
            "[루트](../../Archive/Renamed%20Plan.md)\r\n",
            "`[코드](../Projects/Falsify%20Plan.md)`\n",
            "```md\r\n",
            "[펜스](../Projects/Falsify%20Plan.md)\r\n",
            "```\n",
            "[외부](https://example.com/Projects/Falsify%20Plan.md)\r\n",
            "[메일](mailto:team@example.invalid)\n",
            "[절대](/Archive/Renamed%20Plan.md)"
        );
        assert_eq!(
            fs::read(root.join("Deep/Nested/OnlyMarkdown.md")).unwrap(),
            md_only_after_self_move.as_bytes()
        );

        // 3. Move a referring note to a sibling folder at the same depth -> relative path still
        //    points to the same file, so it is preserved byte-for-byte without rewriting.
        let moved_same_depth = run_move("Notes/SameDepth.md", "Other/SameDepth.md");
        assert_eq!(moved_same_depth["updated_links_in_notes"], 0);
        assert_eq!(
            fs::read(root.join("Other/SameDepth.md")).unwrap(),
            "[같은깊이](../Archive/Renamed%20Plan.md)\n".as_bytes()
        );
    }
}

#[test]
fn cli_hyphen_inputs_write_expected_bytes_and_preserve_flags() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path().join("vault");
    fs::create_dir(&vault).unwrap();
    fs::write(vault.join("AppendNote.md"), "# Append\n원문\n").unwrap();
    fs::write(
        vault.join("ReplaceNote.md"),
        "# Replace\n- 목록 항목\n유지\n",
    )
    .unwrap();

    // 1. `daily --append` accepts Korean bullet and two-line list, plus trailing `--json`.
    let out = isolated_cli(tmp.path(), &vault)
        .args([
            "daily",
            "--date",
            "2026-11-20",
            "--append",
            "- 목록 항목",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let daily_json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(daily_json["rel_path"], "Daily Notes/2026-11-20.md");

    let out = isolated_cli(tmp.path(), &vault)
        .args([
            "daily",
            "--date",
            "2026-11-20",
            "--append",
            "- first\n- second",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let daily_bytes = fs::read_to_string(vault.join("Daily Notes/2026-11-20.md")).unwrap();
    assert!(
        daily_bytes.ends_with("- 목록 항목\n- first\n- second\n"),
        "unexpected daily note content:\n{daily_bytes}"
    );

    // Missing value followed by `--json` or `--help` must fail without writing a file.
    for bad_args in [
        &["daily", "--date", "2026-11-21", "--append", "--json"][..],
        &["daily", "--date", "2026-11-21", "--append", "--help"][..],
    ] {
        let bad_daily = isolated_cli(tmp.path(), &vault)
            .args(bad_args)
            .output()
            .unwrap();
        assert!(
            !bad_daily.status.success(),
            "expected failure for {bad_args:?}"
        );
        assert!(!vault.join("Daily Notes/2026-11-21.md").exists());
    }

    // Actual help invocation (after a completed hyphen value) prints help with 0 writes.
    let help_daily = isolated_cli(tmp.path(), &vault)
        .args([
            "daily",
            "--date",
            "2026-11-21",
            "--append",
            "- 목록 항목",
            "--help",
        ])
        .output()
        .unwrap();
    assert!(help_daily.status.success());
    assert!(String::from_utf8_lossy(&help_daily.stdout).contains("--append <APPEND>"));
    assert!(!vault.join("Daily Notes/2026-11-21.md").exists());

    // Explicit `=` allows flag-looking literal text.
    let eq_daily = isolated_cli(tmp.path(), &vault)
        .args(["daily", "--date", "2026-11-22", "--append=--json"])
        .output()
        .unwrap();
    assert!(
        eq_daily.status.success(),
        "{}",
        String::from_utf8_lossy(&eq_daily.stderr)
    );
    assert!(fs::read_to_string(vault.join("Daily Notes/2026-11-22.md"))
        .unwrap()
        .ends_with("--json\n"));

    // 2. `notes create` accepts leading-hyphen title and `--content`, preserving missing-value checks.
    let created = isolated_cli(tmp.path(), &vault)
        .args([
            "notes",
            "create",
            "- 목록 항목",
            "--content",
            "- 첫 줄\n- 둘째 줄",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created_json: Value = serde_json::from_slice(&created.stdout).unwrap();
    assert_eq!(created_json["rel_path"], "- 목록 항목.md");
    let created_disk = fs::read_to_string(vault.join("- 목록 항목.md")).unwrap();
    assert!(
        created_disk.ends_with("# - 목록 항목\n\n- 첫 줄\n- 둘째 줄\n"),
        "unexpected created note:\n{created_disk}"
    );

    let bad_create = isolated_cli(tmp.path(), &vault)
        .args(["notes", "create", "ShouldNotExist", "--content", "--json"])
        .output()
        .unwrap();
    assert!(!bad_create.status.success());
    assert!(!vault.join("ShouldNotExist.md").exists());

    let eq_create = isolated_cli(tmp.path(), &vault)
        .args(["notes", "create", "FlagBody", "--content=--json"])
        .output()
        .unwrap();
    assert!(
        eq_create.status.success(),
        "{}",
        String::from_utf8_lossy(&eq_create.stderr)
    );
    assert!(fs::read_to_string(vault.join("FlagBody.md"))
        .unwrap()
        .ends_with("--json\n"));

    let sep_create = isolated_cli(tmp.path(), &vault)
        .args(["notes", "create", "--", "--json"])
        .output()
        .unwrap();
    assert!(
        sep_create.status.success(),
        "{}",
        String::from_utf8_lossy(&sep_create.stderr)
    );
    assert!(vault.join("--json.md").is_file());

    // 3. `search` accepts leading-hyphen query while preserving `--json` and missing query error.
    let search = isolated_cli(tmp.path(), &vault)
        .args(["search", "- 목록 항목", "--json"])
        .output()
        .unwrap();
    assert!(
        search.status.success(),
        "{}",
        String::from_utf8_lossy(&search.stderr)
    );
    let hits: Value = serde_json::from_slice(&search.stdout).unwrap();
    assert!(
        hits["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["rel_path"] == "- 목록 항목.md"),
        "expected created hyphen-title note in search results: {hits}"
    );

    let bad_search = isolated_cli(tmp.path(), &vault)
        .args(["search", "--json"])
        .output()
        .unwrap();
    assert!(!bad_search.status.success());

    // 4. Control commands: `notes append` and `notes replace` verified on disk + missing value guards.
    let append_ok = isolated_cli(tmp.path(), &vault)
        .args([
            "notes",
            "append",
            "AppendNote",
            "- 목록 항목\n- 둘째",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        append_ok.status.success(),
        "{}",
        String::from_utf8_lossy(&append_ok.stderr)
    );
    assert_eq!(
        fs::read(vault.join("AppendNote.md")).unwrap(),
        "# Append\n원문\n- 목록 항목\n- 둘째\n".as_bytes()
    );

    let append_bad = isolated_cli(tmp.path(), &vault)
        .args(["notes", "append", "AppendNote", "--json"])
        .output()
        .unwrap();
    assert!(!append_bad.status.success());
    assert_eq!(
        fs::read(vault.join("AppendNote.md")).unwrap(),
        "# Append\n원문\n- 목록 항목\n- 둘째\n".as_bytes()
    );

    let replace_ok = isolated_cli(tmp.path(), &vault)
        .args([
            "notes",
            "replace",
            "ReplaceNote",
            "--find",
            "- 목록 항목",
            "--replace",
            "- 수정 항목",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        replace_ok.status.success(),
        "{}",
        String::from_utf8_lossy(&replace_ok.stderr)
    );
    assert_eq!(
        fs::read(vault.join("ReplaceNote.md")).unwrap(),
        "# Replace\n- 수정 항목\n유지\n".as_bytes()
    );

    let replace_bad = isolated_cli(tmp.path(), &vault)
        .args([
            "notes",
            "replace",
            "ReplaceNote",
            "--find",
            "- 수정 항목",
            "--replace",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!replace_bad.status.success());
    assert_eq!(
        fs::read(vault.join("ReplaceNote.md")).unwrap(),
        "# Replace\n- 수정 항목\n유지\n".as_bytes()
    );
}

#[test]
fn cli_vault_guidance_uses_current_binary_name() {
    let tmp = tempfile::tempdir().unwrap();

    // 1. `vaults list` with no discovered or registered vaults
    let list = isolated_cli_base(tmp.path())
        .args(["vaults", "list"])
        .output()
        .unwrap();
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert_eq!(
        list_stdout.trim(),
        "No vaults found. Run `bitefoundry-obsidian init <dir>` or `bitefoundry-obsidian vaults add <path>`."
    );

    // 2. Command requiring an active vault when no vaults exist
    let empty = isolated_cli_base(tmp.path())
        .args(["notes", "list"])
        .output()
        .unwrap();
    assert!(!empty.status.success());
    let empty_stderr = String::from_utf8_lossy(&empty.stderr);
    assert!(
        empty_stderr.contains(
            "No Obsidian vaults found. Run `bitefoundry-obsidian init <dir>` to create a starter vault or `bitefoundry-obsidian vaults add <path>` to register an existing vault."
        ),
        "unexpected empty vault stderr: {empty_stderr}"
    );

    // 3. Explicit `--vault` that does not exist
    let missing = isolated_cli_base(tmp.path())
        .args(["--vault", "missing-vault", "notes", "list"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    let missing_stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        missing_stderr.contains(
            "Vault 'missing-vault' not found. Run `bitefoundry-obsidian vaults list`, `bitefoundry-obsidian vaults add <path>`, or `bitefoundry-obsidian init <dir>`."
        ),
        "unexpected missing vault stderr: {missing_stderr}"
    );
}

#[test]
fn cli_append_accepts_dash_prefixed_text_and_still_has_help() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path().join("vault");
    fs::create_dir(&vault).unwrap();
    for name in ["Bare", "Separator", "Help"] {
        fs::write(vault.join(format!("{name}.md")), "# 제목\n원래 본문\n").unwrap();
    }

    for args in [
        vec!["notes", "append", "Bare", "- [ ] 할 일"],
        vec!["notes", "append", "Separator", "--", "- [ ] 할 일"],
    ] {
        let output = isolated_cli(tmp.path(), &vault)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for name in ["Bare", "Separator"] {
        assert_eq!(
            fs::read(vault.join(format!("{name}.md"))).unwrap(),
            "# 제목\n원래 본문\n- [ ] 할 일\n".as_bytes()
        );
    }
    let output = isolated_cli(tmp.path(), &vault)
        .args(["notes", "append", "--help"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("<NOTE> <TEXT>"));
    assert_eq!(
        fs::read(vault.join("Help.md")).unwrap(),
        "# 제목\n원래 본문\n".as_bytes()
    );
}

#[test]
fn cli_and_mcp_replace_only_a_unique_literal_body_match() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir(&root).unwrap();
    let markdown = "\u{feff}---\r\nstatus: 'draft line'\n# untouched\r\n...\r\n# Title\n\r\n  draft line\r\n끝\r\n\nrepeat repeat aaaa\nTrailing spaces  ";
    let find = "  draft line\r\n끝";
    let replacement = "  published line\r\n끝!  ";
    let expected = markdown.replace(find, replacement);
    for note in ["CLI.md", "MCP.md"] {
        fs::write(root.join(note), markdown).unwrap();
    }

    let vault = vault_from_dir(&root, Some("fixture"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let mcp = McpServer::new(cfg, vault, Storage::open_in_memory().unwrap());
    let schema = tool_definitions()
        .into_iter()
        .find(|tool| tool["name"] == "replace_note_text")
        .unwrap();
    assert_eq!(
        schema["inputSchema"]["required"],
        json!(["note", "find", "replace"])
    );
    let replace = |note: &str, find: &str, replacement: &str| -> serde_json::Value {
        let rpc = mcp
            .handle_rpc(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {
                    "name": "replace_note_text",
                    "arguments": {"note": note, "find": find, "replace": replacement}
                }
            }))
            .unwrap();
        let result = &rpc["result"];
        assert_eq!(result["isError"], false, "{rpc}");
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let cli_replace = |find: &str, replacement: &str| -> serde_json::Value {
        let output = isolated_cli(tmp.path(), &root)
            .args([
                "--json",
                "notes",
                "replace",
                "CLI.md",
                "--find",
                find,
                "--replace",
                replacement,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    for (needle, expected_count) in [("# untouched", 0), ("repeat", 2), ("aaa", 2)] {
        let cli = cli_replace(needle, "new text");
        let rpc = replace("MCP.md", needle, "new text");
        assert_eq!(
            cli,
            json!({"rel_path":"CLI.md","matches":expected_count,"replaced":false})
        );
        assert_eq!(
            rpc,
            json!({"rel_path":"MCP.md","matches":expected_count,"replaced":false})
        );
        for note in ["CLI.md", "MCP.md"] {
            assert_eq!(fs::read(root.join(note)).unwrap(), markdown.as_bytes());
        }
    }
    assert_eq!(
        cli_replace(find, replacement),
        json!({"rel_path":"CLI.md","matches":1,"replaced":true})
    );
    assert_eq!(
        replace("MCP.md", find, replacement),
        json!({"rel_path":"MCP.md","matches":1,"replaced":true})
    );
    assert_eq!(fs::read(root.join("CLI.md")).unwrap(), expected.as_bytes());
    assert_eq!(fs::read(root.join("MCP.md")).unwrap(), expected.as_bytes());
    assert_eq!(
        mcp.dispatch_tool("search_notes", &json!({"query":"published"}))
            .unwrap()["count"],
        2
    );

    assert_eq!(cli_replace("Trailing spaces  ", "")["replaced"], true);
    assert_eq!(replace("MCP.md", "Trailing spaces  ", "")["replaced"], true);
    assert!(fs::read(root.join("CLI.md")).unwrap().ends_with(b"aaaa\n"));
    assert_eq!(
        fs::read(root.join("CLI.md")).unwrap(),
        fs::read(root.join("MCP.md")).unwrap()
    );
}

#[test]
fn test_ratatui_test_backend_3pane_and_braille_graph_view() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = init_starter_vault(&tmp.path().join("TuiVault")).unwrap();
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);

    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let mut app = AppState::new(cfg.clone(), vault, storage, sync_ctrl);
    let mut ui = UiState::from_config(&cfg);
    app.reload_from_storage(&mut ui);

    let backend = TestBackend::new(120, 34);
    let mut terminal = Terminal::new(backend).unwrap();

    // 1. Render default 3-Pane Note Canvas
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    let buf_text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(buf_text.contains("TuiVault"));
    assert!(buf_text.contains("Linked Mentions"));

    // 2. Toggle Interactive Knowledge Graph View (G)
    app.dispatch(Action::ToggleGraphView, &mut ui);
    assert_eq!(ui.canvas_mode, CanvasMode::Graph);
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    let graph_text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(graph_text.contains("Local Graph"));
    assert!(graph_text.contains("nodes"));

    // 3. Switch sidebar to Tags tab
    app.dispatch(Action::SetSidebarTab(SidebarTab::Tags), &mut ui);
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    let tags_text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(tags_text.contains("Tags ("));
}

#[test]
fn test_all_button_ids_registered_and_dispatched_on_mouse_click() {
    use obsidian_cli::{
        config::PageWidthMode,
        ui::{
            hitmap::{ButtonId, HitTarget},
            overlays::Overlay,
        },
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ClickVault");
    fs::create_dir_all(&root).unwrap();
    seed_vault(&root);

    let vault = vault_from_dir(&root, Some("ClickVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);

    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let mut app = AppState::new(cfg.clone(), vault, storage, sync_ctrl);
    let mut ui = UiState::from_config(&cfg);
    app.reload_from_storage(&mut ui);
    app.open_note(&mut ui, "Projects/Q4 Architecture RFC.md", false);

    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();

    let click_button = |terminal: &mut Terminal<TestBackend>,
                        app: &mut AppState,
                        ui: &mut UiState,
                        btn: ButtonId| {
        ui.toasts.clear();
        terminal
            .draw(|f| {
                let _ = ui::draw(f, app, ui);
            })
            .unwrap();
        let rect = ui
            .last_hitmap
            .region_of(HitTarget::Button(btn))
            .unwrap_or_else(|| panic!("ButtonId::{btn:?} was not registered in HitMap"));
        app.dispatch(
            Action::MouseClick {
                col: rect.x,
                row: rect.y,
            },
            ui,
        );
    };

    // 1. ToggleSidebar
    let prev_sidebar = ui.show_sidebar;
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ToggleSidebar);
    assert_eq!(ui.show_sidebar, !prev_sidebar);
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ToggleSidebar);
    assert_eq!(ui.show_sidebar, prev_sidebar);

    // 2. VaultSwitcher
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::VaultSwitcher);
    assert!(matches!(ui.overlay, Some(Overlay::VaultSwitcher(_))));
    ui.close_overlay();

    // 3. Settings
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::Settings);
    assert!(matches!(ui.overlay, Some(Overlay::Settings(_))));
    ui.close_overlay();

    // 4. Help (+ ConfirmCancel close pill)
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::Help);
    assert!(matches!(ui.overlay, Some(Overlay::Help { .. })));
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ConfirmCancel);
    assert!(ui.overlay.is_none());

    // 5. QuickSwitcher
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::QuickSwitcher);
    assert!(matches!(ui.overlay, Some(Overlay::QuickSwitcher(_))));
    ui.close_overlay();

    // 6. ToggleInspector
    let prev_insp = ui.show_inspector;
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ToggleInspector);
    assert_eq!(ui.show_inspector, !prev_insp);
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ToggleInspector);
    assert_eq!(ui.show_inspector, prev_insp);

    // 7. ToggleBookmark
    let prev_bm = app.active_note.as_ref().unwrap().summary.is_bookmarked;
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ToggleBookmark);
    assert_eq!(
        app.active_note.as_ref().unwrap().summary.is_bookmarked,
        !prev_bm
    );

    // 8. ToggleWidth
    let prev_w = ui.page_width;
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ToggleWidth);
    assert_ne!(ui.page_width, prev_w);
    assert!(matches!(
        ui.page_width,
        PageWidthMode::Full | PageWidthMode::Centered
    ));

    // 9. QuickAppend
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::QuickAppend);
    assert!(matches!(ui.overlay, Some(Overlay::NoteForm(_))));
    ui.close_overlay();

    // 10. NewNote
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::NewNote);
    assert!(matches!(ui.overlay, Some(Overlay::NoteForm(_))));
    ui.close_overlay();

    // 11. EditNote ([✎ Edit] enters IN-PLACE inline editor inside note_view, ZERO popup overlay!)
    //     + SaveInlineEdit ([Ctrl+S Save]) + FinishInlineEdit ([✓ Done])
    //     + EditExternal ([Ext $EDITOR] flushes inline edit and requests external $EDITOR)
    assert!(app.pending_external_editor.is_none());
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::EditNote);
    assert!(
        app.pending_external_editor.is_none(),
        "[✎ Edit] must open in-place inline editor, not external $EDITOR"
    );
    assert!(
        ui.inline_editor.is_some(),
        "expected in-place inline_editor"
    );
    assert!(
        ui.overlay.is_none(),
        "must NOT pop up a separate overlay window"
    );
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::SaveInlineEdit);
    assert!(
        ui.inline_editor.is_some(),
        "[Ctrl+S Save] stays in inline edit mode"
    );
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::FinishInlineEdit);
    assert!(
        ui.inline_editor.is_none(),
        "[✓ Done] returns cleanly to rendered reader view"
    );
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::EditNote);
    assert!(ui.inline_editor.is_some());
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::EditExternal);
    assert!(ui.inline_editor.is_none());
    assert!(ui.overlay.is_none());
    assert!(app.pending_external_editor.take().is_some());

    // 12. LinkExtractor
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::LinkExtractor);
    assert!(matches!(ui.overlay, Some(Overlay::LinkExtractor(_))));
    ui.close_overlay();

    // 13. ToggleHrefs
    let prev_hrefs = ui.show_inline_hrefs;
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ToggleHrefs);
    assert_eq!(ui.show_inline_hrefs, !prev_hrefs);

    // 14. ImagePreview (note has Attachments/diagram.png!) + ConfirmCancel close pill
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ImagePreview);
    assert!(matches!(ui.overlay, Some(Overlay::ImagePreview(_))));
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ConfirmCancel);
    assert!(ui.overlay.is_none());

    // 15. ReindexNow
    app.last_index_stats = None;
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ReindexNow);
    assert!(app.last_index_stats.is_some());

    // 16. ToggleGraph + 17. GraphToggleScope + GraphZoomIn/Out/ResetView
    assert_eq!(ui.canvas_mode, CanvasMode::Note);
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ToggleGraph);
    assert_eq!(ui.canvas_mode, CanvasMode::Graph);
    let prev_global = ui.graph_global;
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::GraphToggleScope);
    assert_eq!(ui.graph_global, !prev_global);
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::GraphZoomIn);
    assert!(ui.graph_sim.zoom > 1.0);
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::GraphZoomOut);
    assert!((ui.graph_sim.zoom - 1.0).abs() < 1e-6);
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::GraphZoomIn);
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::GraphResetView);
    assert!((ui.graph_sim.zoom - 1.0).abs() < 1e-6);
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ToggleGraph);
    assert_eq!(ui.canvas_mode, CanvasMode::Note);

    // 18. OpenDaily
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::OpenDaily);
    assert!(app
        .active_note
        .as_ref()
        .unwrap()
        .summary
        .rel_path
        .starts_with("Daily Notes/"));

    // 19. ConfirmOk & ConfirmCancel via ConfirmDeleteNote
    app.dispatch(Action::ConfirmDeleteNote, &mut ui);
    assert!(matches!(ui.overlay, Some(Overlay::Confirm(_))));
    click_button(&mut terminal, &mut app, &mut ui, ButtonId::ConfirmOk);
    assert!(ui.overlay.is_none());
}

#[tokio::test]
async fn test_html_preview_interactive_quickjs_native_serving_vault_files_and_auto_routing() {
    use obsidian_cli::ui::{
        hitmap::{ButtonId, HitTarget},
        overlays::Overlay,
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("HtmlVault");
    fs::create_dir_all(root.join("Attachments")).unwrap();
    fs::create_dir_all(root.join("Pages")).unwrap();

    // 1. Standalone .html file in vault
    fs::write(
        root.join("Pages/dashboard.html"),
        r#"<!DOCTYPE html><html><head><title>Vault Dashboard</title></head><body><h1>Dashboard</h1><p id="msg">Ready</p><button onclick="document.getElementById('msg').textContent='Launched'">Launch</button></body></html>"#,
    )
    .unwrap();

    // 2. Fake .png attachment whose actual bytes are HTML (must auto-route to HtmlPreview!)
    fs::write(
        root.join("Attachments/fake_chart.png"),
        b"<!DOCTYPE html><html><body><h1>Login Redirect</h1><button onclick=\"this.textContent='Retried'\">Retry</button></body></html>",
    )
    .unwrap();

    // 3. Markdown note with fenced ```html code block + fake_chart.png embed
    fs::write(
        root.join("Interactive.md"),
        r#"# Interactive Note

```html
<div>
  <p id="cnt">Clicks: 0</p>
  <button id="btn" onclick="window.__n=(window.__n||0)+1; document.getElementById('cnt').textContent='Clicks: '+window.__n;">Click Me</button>
</div>
```

![[fake_chart.png]]
"#,
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("HtmlVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);

    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let mut app = AppState::new(cfg.clone(), vault, storage, sync_ctrl);
    let mut ui = UiState::from_config(&cfg);
    app.reload_from_storage(&mut ui);
    app.open_note(&mut ui, "Interactive.md", false);

    // Vault .html file discovery
    assert_eq!(app.html_files.len(), 1);
    assert_eq!(app.html_files[0].rel_path, "Pages/dashboard.html");

    let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();

    // Render note canvas -> `[H: HTML]` pill registered as ButtonId::HtmlPreview & HitTarget::HtmlChip
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    let html_pill = ui
        .last_hitmap
        .region_of(HitTarget::Button(ButtonId::HtmlPreview))
        .expect("[H: HTML] pill should be registered in HitMap");

    // Clicking `[H: HTML]` opens Overlay::HtmlPreview with the fenced ```html block
    app.dispatch(
        Action::MouseClick {
            col: html_pill.x,
            row: html_pill.y,
        },
        &mut ui,
    );
    assert!(matches!(ui.overlay, Some(Overlay::HtmlPreview(_))));

    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();

    // Verify mouse `click_at` invokes QuickJS `<button onclick>` and mutates DOM
    let (btn_col, btn_row) = {
        let Some(Overlay::HtmlPreview(ref mut ov)) = ui.overlay else {
            panic!("expected HtmlPreview");
        };
        assert!(ov.session.js_active);
        let theme = obsidian_cli::ui::overlays::html_preview::html_theme_from_ui(&ui.theme);
        let rendered = ov.session.render(ov.last_content_rect.width, &theme, false);
        let ctrl = &rendered.controls[0];
        (
            ov.last_content_rect.x + ctrl.col_start + 1,
            ov.last_content_rect.y + ctrl.line as u16,
        )
    };
    app.dispatch(
        Action::MouseClick {
            col: btn_col,
            row: btn_row,
        },
        &mut ui,
    );
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    let after_click: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        after_click.contains("Clicks: 1"),
        "mouse click_at should execute onclick handler and render Clicks: 1"
    );

    // Toggle Raw HTML (`m`) and back
    app.dispatch(Action::Input('m'), &mut ui);
    {
        let Some(Overlay::HtmlPreview(ref ov)) = ui.overlay else {
            panic!("expected HtmlPreview");
        };
        assert!(ov.session.raw_mode);
    }
    app.dispatch(Action::Input('m'), &mut ui);

    // Native Serving (`o`) -> queues pending_html_serve -> serves over localhost Axum!
    app.dispatch(Action::Input('o'), &mut ui);
    let (serve_title, serve_html) = app
        .pending_html_serve
        .take()
        .expect("pressing 'o' should queue native HTML serving");
    let served_url = app
        .serve_html_and_open(&serve_title, &serve_html, &mut ui, false)
        .await
        .expect("NativeHtmlServer::serve_html should succeed");
    assert!(served_url.starts_with("http://127.0.0.1:"));

    let resp = reqwest::get(&served_url).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    assert!(content_type.contains("text/html"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("Click Me"));

    // Click outside modal (0, 0) closes HtmlPreview and drops QuickJS session
    app.dispatch(Action::MouseClick { col: 0, row: 0 }, &mut ui);
    assert!(ui.overlay.is_none());

    // Open vault `.html` file via `Action::OpenHtmlFile`
    app.dispatch(Action::OpenHtmlFile("Pages/dashboard.html".into()), &mut ui);
    assert!(matches!(ui.overlay, Some(Overlay::HtmlPreview(_))));
    app.dispatch(Action::Input('q'), &mut ui);
    assert!(ui.overlay.is_none());

    // HTML-vs-Image Auto-Routing: opening image preview on `fake_chart.png` (which contains HTML)
    // automatically routes to Overlay::HtmlPreview instead of failing image decode!
    app.dispatch(Action::OpenImagePreview, &mut ui);
    match ui.overlay {
        Some(Overlay::HtmlPreview(ref ov)) => {
            assert!(ov.session.raw_html().contains("Login Redirect"));
            assert!(ov.session.js_active);
        }
        ref other => panic!("expected auto-routed HtmlPreview, got {other:?}"),
    }

    app.html_server.shutdown();
}

#[test]
fn test_in_place_inline_editor_0ms_live_reflection_autosave_upsert_scroll_and_mouse() {
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use obsidian_cli::ui::{
        app::Focus,
        event::key_to_action,
        hitmap::{ButtonId, HitTarget},
        inline_editor::InlineSaveStatus,
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("EditorVault");
    fs::create_dir_all(&root).unwrap();
    seed_vault(&root);

    // Add a tall note to verify scroll position preservation & mouse-wheel scrolling
    let long_body = (1..=60)
        .map(|i| format!("Paragraph {i}: Obsidian Markdown content line"))
        .collect::<Vec<_>>()
        .join("\n\n");
    fs::write(
        root.join("Long Note.md"),
        format!("# Long Note\n\n{long_body}\n"),
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("EditorVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);

    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let mut app = AppState::new(cfg.clone(), vault.clone(), storage, sync_ctrl);
    let mut ui = UiState::from_config(&cfg);
    app.reload_from_storage(&mut ui);
    app.open_note(&mut ui, "Projects/Q4 Architecture RFC.md", false);

    // Bookmark the note to verify `upsert_single_note` preserves `is_bookmarked`
    app.dispatch(Action::ToggleBookmark, &mut ui);
    assert!(app.active_note.as_ref().unwrap().summary.is_bookmarked);

    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();

    // 1. Pressing `e` enters IN-PLACE inline edit mode inside `note_view` (ZERO popup overlay!)
    let act_e = key_to_action(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        ui.focus,
        ui.canvas_mode,
        &ui.keymap,
        ui.is_text_input_active(),
    )
    .expect("'e' should map to OpenNoteEditor");
    assert_eq!(act_e, Action::OpenNoteEditor);
    app.dispatch(act_e, &mut ui);
    assert!(
        app.pending_external_editor.is_none(),
        "'e' must never launch external editor"
    );
    assert!(
        ui.inline_editor.is_some(),
        "must activate in-place inline_editor"
    );
    assert!(
        ui.overlay.is_none(),
        "must NOT open any popup overlay window"
    );
    assert!(ui.is_text_input_active());

    // Render and confirm 3-pane layout (Sidebar + inline note_view + Inspector) stays 100% intact
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    let initial_screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(initial_screen.contains("✎ EDITING"));
    assert!(initial_screen.contains("✓ Live Saved"));
    assert!(initial_screen.contains("Linked Mentions"));
    assert!(initial_screen.contains("Files ("));
    assert!(ui.cursor_pos.is_some());
    assert!(ui
        .last_hitmap
        .region_of(HitTarget::Button(ButtonId::FinishInlineEdit))
        .is_some());
    assert!(ui
        .last_hitmap
        .region_of(HitTarget::Button(ButtonId::SaveInlineEdit))
        .is_some());
    assert!(ui
        .last_hitmap
        .region_of(HitTarget::Button(ButtonId::EditExternal))
        .is_some());
    assert!(ui
        .last_hitmap
        .region_of(HitTarget::InlineEditorBody)
        .is_some());

    // 2. Typing `# New Heading` + `#livetag` + Korean UTF-8 + `q`/`j`/`k`/`e` types normally AND
    //    updates `app.active_note.parsed` + right-side Inspector in 0ms (BEFORE any disk save!)
    for _ in 0..7 {
        app.dispatch(Action::CursorDown, &mut ui);
    }
    app.dispatch(Action::CursorEnd, &mut ui);
    app.dispatch(Action::Newline, &mut ui);
    for ch in "## Live Heading #livetag".chars() {
        let act = key_to_action(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            ui.focus,
            ui.canvas_mode,
            &ui.keymap,
            ui.is_text_input_active(),
        )
        .expect("printable char must map to Action::Input during inline edit");
        assert_eq!(act, Action::Input(ch));
        app.dispatch(act, &mut ui);
    }

    // Verify 0ms live reflection in memory BEFORE disk save!
    let disk_before_save =
        fs::read_to_string(root.join("Projects/Q4 Architecture RFC.md")).unwrap();
    assert!(
        !disk_before_save.contains("Live Heading"),
        "disk should not be written yet before 250ms debounce or Ctrl+S"
    );
    let live_note = app.active_note.as_ref().unwrap();
    assert!(
        live_note
            .parsed
            .headings
            .iter()
            .any(|h| h.text.contains("Live Heading")),
        "live heading must appear in active_note.parsed immediately (0ms)"
    );
    assert!(
        live_note.summary.tags.iter().any(|t| t == "livetag"),
        "#livetag must appear in active_note.summary.tags immediately (0ms)"
    );
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().save_status,
        InlineSaveStatus::Editing
    );

    // Render frame right now -> right-side Inspector shows `Live Heading` and `#livetag` live!
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    let live_screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(live_screen.contains("● Editing"));
    assert!(live_screen.contains("#livetag"));

    // 3. Debounced Auto-Save (simulate >= 250ms idle, then tick) writes atomically + calls upsert_single_note!
    ui.inline_editor.as_mut().unwrap().dirty_since =
        Some(Instant::now() - Duration::from_millis(300));
    app.dispatch(Action::InlineEditTick, &mut ui);
    assert!(!ui.inline_editor.as_ref().unwrap().dirty);
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().save_status,
        InlineSaveStatus::Saved
    );

    let disk_after_autosave =
        fs::read_to_string(root.join("Projects/Q4 Architecture RFC.md")).unwrap();
    assert!(disk_after_autosave.contains("## Live Heading #livetag"));

    // Verify SQLite FTS5, headings, tags, and bookmark updated WITHOUT calling sync_vault
    let fts_hits = app
        .storage
        .search_notes(&vault.id, "Live Heading", 10)
        .unwrap();
    assert_eq!(fts_hits.len(), 1);
    assert_eq!(fts_hits[0].rel_path, "Projects/Q4 Architecture RFC.md");
    assert!(app.tags.iter().any(|t| t.tag == "livetag"));
    assert!(
        app.storage
            .get_note_summary(&vault.id, "Projects/Q4 Architecture RFC.md")
            .unwrap()
            .unwrap()
            .is_bookmarked,
        "bookmark must survive single-note upsert"
    );
    // Subsequent background watcher sync_vault sees 0 changed files!
    let watcher_stats = app.storage.sync_vault(&vault).unwrap();
    assert_eq!(
        watcher_stats.updated, 0,
        "mtime_ms + size_bytes written by upsert_single_note must suppress redundant sync_vault rescans"
    );

    // 4. Test Tab (2 spaces), Korean UTF-8, Undo/Redo, Paste, and Ctrl+S (saves & STAYS in edit mode!)
    app.dispatch(Action::Newline, &mut ui);
    app.dispatch(Action::Tab, &mut ui);
    for ch in "한글 노트 편집X".chars() {
        app.dispatch(Action::Input(ch), &mut ui);
    }
    let act_undo = key_to_action(
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
        ui.focus,
        ui.canvas_mode,
        &ui.keymap,
        ui.is_text_input_active(),
    )
    .expect("Ctrl+Z should map to Undo in inline editor");
    assert_eq!(act_undo, Action::Undo);
    app.dispatch(act_undo, &mut ui);

    let act_redo = key_to_action(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
        ui.focus,
        ui.canvas_mode,
        &ui.keymap,
        ui.is_text_input_active(),
    )
    .expect("Ctrl+Y should map to Redo in inline editor");
    assert_eq!(act_redo, Action::Redo);
    app.dispatch(act_redo, &mut ui);
    app.dispatch(Action::Undo, &mut ui); // remove trailing 'X'

    app.dispatch(Action::Paste("\n- [ ] 새 작업".into()), &mut ui);

    let act_save = key_to_action(
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        ui.focus,
        ui.canvas_mode,
        &ui.keymap,
        ui.is_text_input_active(),
    )
    .expect("Ctrl+S should map to SaveInlineEdit in inline editor");
    assert_eq!(act_save, Action::SaveInlineEdit);
    app.dispatch(act_save, &mut ui);

    assert!(
        ui.inline_editor.is_some(),
        "Ctrl+S must save immediately while STAYING in inline edit mode"
    );
    assert!(!ui.inline_editor.as_ref().unwrap().dirty);
    let disk_after_ctrl_s =
        fs::read_to_string(root.join("Projects/Q4 Architecture RFC.md")).unwrap();
    assert!(disk_after_ctrl_s.contains("  한글 노트 편집"));
    assert!(disk_after_ctrl_s.contains("- [ ] 새 작업"));

    // 5. Esc flushes any pending edit and returns smoothly to rendered note_view
    app.dispatch(Action::Paste("\n#flushed_on_esc".into()), &mut ui);
    assert!(ui.inline_editor.as_ref().unwrap().dirty);
    let act_esc = key_to_action(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        ui.focus,
        ui.canvas_mode,
        &ui.keymap,
        ui.is_text_input_active(),
    )
    .expect("Esc should map to FinishInlineEdit");
    assert_eq!(act_esc, Action::FinishInlineEdit);
    app.dispatch(act_esc, &mut ui);
    assert!(ui.inline_editor.is_none());
    assert!(ui.overlay.is_none());
    assert!(
        fs::read_to_string(root.join("Projects/Q4 Architecture RFC.md"))
            .unwrap()
            .contains("#flushed_on_esc")
    );
    assert!(app
        .active_note
        .as_ref()
        .unwrap()
        .summary
        .tags
        .iter()
        .any(|t| t == "flushed_on_esc"));

    // 6. Ctrl+E still requests external $EDITOR (both outside and inside inline edit)
    app.dispatch(Action::OpenNoteEditor, &mut ui);
    assert!(ui.inline_editor.is_some());
    let act_ctrl_e = key_to_action(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
        ui.focus,
        ui.canvas_mode,
        &ui.keymap,
        ui.is_text_input_active(),
    )
    .expect("Ctrl+E should map to RequestExternalEditor");
    assert_eq!(act_ctrl_e, Action::RequestExternalEditor);
    app.dispatch(act_ctrl_e, &mut ui);
    assert!(ui.inline_editor.is_none());
    let ext_path = app
        .pending_external_editor
        .take()
        .expect("Ctrl+E must queue pending_external_editor");
    assert!(ext_path.ends_with("Q4 Architecture RFC.md"));

    // 7. Scroll position preserved 100% across reader <-> inline edit + mouse wheel + click-to-cursor
    app.open_note(&mut ui, "Long Note.md", false);
    ui.focus = Focus::Canvas;
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    let canvas_rect = ui
        .last_hitmap
        .region_of(HitTarget::Pane(Focus::Canvas))
        .expect("Canvas pane registered");
    let sidebar_rect = ui
        .last_hitmap
        .region_of(HitTarget::Pane(Focus::Sidebar))
        .expect("Sidebar pane registered");

    // Scroll reader down to offset 9
    for _ in 0..3 {
        app.dispatch(
            Action::MouseScrollDown {
                col: canvas_rect.x + 2,
                row: canvas_rect.y + 4,
            },
            &mut ui,
        );
    }
    assert_eq!(ui.canvas_scroll, 9);

    // Enter inline edit -> inherits scroll_y == 9!
    app.dispatch(Action::OpenNoteEditor, &mut ui);
    assert_eq!(ui.inline_editor.as_ref().unwrap().scroll_y, 9);
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();

    // Mouse wheel over inline editor scrolls editor viewport to 12, then back to 9
    app.dispatch(
        Action::MouseScrollDown {
            col: canvas_rect.x + 4,
            row: canvas_rect.y + 5,
        },
        &mut ui,
    );
    assert_eq!(ui.inline_editor.as_ref().unwrap().scroll_y, 12);
    app.dispatch(
        Action::MouseScrollUp {
            col: canvas_rect.x + 4,
            row: canvas_rect.y + 5,
        },
        &mut ui,
    );
    assert_eq!(ui.inline_editor.as_ref().unwrap().scroll_y, 9);

    // Wheel over Sidebar still scrolls sidebar even while inline editor is open
    ui.sidebar_selected = 0;
    app.dispatch(
        Action::MouseScrollDown {
            col: sidebar_rect.x + 2,
            row: sidebar_rect.y + 4,
        },
        &mut ui,
    );
    assert_eq!(ui.sidebar_selected, 1);

    // Click inside InlineEditorBody places cursor on that line/col
    let body_rect = ui
        .last_hitmap
        .region_of(HitTarget::InlineEditorBody)
        .expect("InlineEditorBody hit target registered");
    app.dispatch(
        Action::MouseClick {
            col: body_rect.x + 3,
            row: body_rect.y + 2,
        },
        &mut ui,
    );
    assert_eq!(ui.focus, Focus::Canvas);

    // Esc exits inline edit and returns reader view at the EXACT same scroll position (9)!
    app.dispatch(Action::FinishInlineEdit, &mut ui);
    assert!(ui.inline_editor.is_none());
    assert_eq!(ui.canvas_scroll, 9);
    assert!(!ui.canvas_follow_selected);

    // 8. Opening another note while inline editing flushes pending save cleanly first!
    app.dispatch(Action::OpenNoteEditor, &mut ui);
    app.dispatch(Action::Paste("\nFlushedBeforeSwitch".into()), &mut ui);
    assert!(ui.inline_editor.as_ref().unwrap().dirty);
    app.open_note(&mut ui, "Storage Engine.md", true);
    assert!(ui.inline_editor.is_none());
    assert!(fs::read_to_string(root.join("Long Note.md"))
        .unwrap()
        .contains("FlushedBeforeSwitch"));

    // 9. Switching vault while inline editing also flushes pending save cleanly and closes inline edit!
    let root_b = tmp.path().join("SecondVault");
    fs::create_dir_all(&root_b).unwrap();
    fs::write(root_b.join("Welcome.md"), "# Welcome Vault B\n").unwrap();
    let vault_b = vault_from_dir(&root_b, Some("SecondVault"));
    app.vaults.push(vault_b.clone());

    app.dispatch(Action::OpenNoteEditor, &mut ui);
    app.dispatch(Action::Paste("\nFlushedBeforeVaultSwitch".into()), &mut ui);
    assert!(ui.inline_editor.as_ref().unwrap().dirty);
    app.dispatch(Action::SwitchVault(vault_b.id.clone()), &mut ui);
    assert!(ui.inline_editor.is_none());
    assert_eq!(app.active_vault.id, vault_b.id);
    assert!(fs::read_to_string(root.join("Storage Engine.md"))
        .unwrap()
        .contains("FlushedBeforeVaultSwitch"));
}

#[test]
fn test_graph_stability_click_hover_cursor_centered_wheel_zoom_pan_and_keys() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use obsidian_cli::ui::{
        app::Focus,
        event::key_to_action,
        hitmap::{ButtonId, HitTarget},
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("GraphVault");
    fs::create_dir_all(&root).unwrap();
    seed_vault(&root);

    let vault = vault_from_dir(&root, Some("GraphVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);

    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let mut app = AppState::new(cfg.clone(), vault, storage, sync_ctrl);
    let mut ui = UiState::from_config(&cfg);
    app.reload_from_storage(&mut ui);
    app.open_note(&mut ui, "Projects/Q4 Architecture RFC.md", false);

    // Enter Graph View
    app.dispatch(Action::ToggleGraphView, &mut ui);
    assert_eq!(ui.canvas_mode, CanvasMode::Graph);
    assert_eq!(ui.focus, Focus::Canvas);
    assert!(app.graph.nodes.len() >= 3);

    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();

    // 1. Clicking a GraphNode without dragging must NOT wake physics or move any node
    let node_idx = (0..app.graph.nodes.len())
        .find(|&i| i != ui.graph_selected)
        .unwrap_or(0);
    let node_rect = ui
        .last_hitmap
        .region_of(HitTarget::GraphNode(node_idx))
        .expect("GraphNode should be registered in HitMap");
    let positions_before_click = ui.graph_sim.positions.clone();

    app.dispatch(
        Action::MouseClick {
            col: node_rect.x + 1,
            row: node_rect.y,
        },
        &mut ui,
    );
    assert_eq!(ui.graph_selected, node_idx);
    assert_eq!(ui.graph_sim.energy, 0.0, "click must NOT wake physics");
    assert!(
        !ui.graph_sim.is_active(),
        "click without drag must NOT activate physics loop"
    );
    // Even if GraphTick fires before MouseUp, no node moves
    app.dispatch(Action::GraphTick, &mut ui);
    app.dispatch(
        Action::MouseUp {
            col: node_rect.x + 1,
            row: node_rect.y,
        },
        &mut ui,
    );
    assert_eq!(ui.graph_sim.energy, 0.0);
    assert_eq!(
        ui.graph_sim.positions, positions_before_click,
        "clicking a node without dragging must not move any node"
    );

    // 2. Hovering a GraphNode must NOT change its pill width (zero label-jitter domino effect!)
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    let rects_unhovered: Vec<(usize, ratatui::layout::Rect)> = (0..app.graph.nodes.len())
        .filter_map(|i| {
            ui.last_hitmap
                .region_of(HitTarget::GraphNode(i))
                .map(|r| (i, r))
        })
        .collect();
    let hover_target = rects_unhovered[0];
    app.dispatch(
        Action::MouseMove {
            col: hover_target.1.x + 1,
            row: hover_target.1.y,
        },
        &mut ui,
    );
    assert_eq!(ui.graph_sim.hovered_node, Some(hover_target.0));
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    let hovered_rect = ui
        .last_hitmap
        .region_of(HitTarget::GraphNode(hover_target.0))
        .unwrap();
    assert_eq!(
        hovered_rect.width, hover_target.1.width,
        "hovering a node must never change its pill width"
    );

    // 3. MouseScrollUp / MouseScrollDown over GraphCanvas AND over a GraphNode zooms smoothly & cursor-centered
    let canvas_rect = ui.graph_sim.canvas_area.get();
    let scroll_col = canvas_rect.x + canvas_rect.width / 3;
    let scroll_row = canvas_rect.y + canvas_rect.height / 3;
    let (wx0, wy0) = ui
        .graph_sim
        .screen_to_world_raw(scroll_col, scroll_row)
        .unwrap();

    app.dispatch(
        Action::MouseScrollUp {
            col: scroll_col,
            row: scroll_row,
        },
        &mut ui,
    );
    assert!(
        ui.graph_sim.zoom > 1.1,
        "MouseScrollUp over graph canvas must zoom in"
    );
    let (wx1, wy1) = ui
        .graph_sim
        .screen_to_world_raw(scroll_col, scroll_row)
        .unwrap();
    assert!((wx1 - wx0).abs() < 1e-9 && (wy1 - wy0).abs() < 1e-9);

    // Wheel directly over a GraphNode pill zooms out cleanly
    let zoom_before_node_scroll = ui.graph_sim.zoom;
    app.dispatch(
        Action::MouseScrollDown {
            col: hovered_rect.x + 1,
            row: hovered_rect.y,
        },
        &mut ui,
    );
    assert!(
        ui.graph_sim.zoom < zoom_before_node_scroll,
        "MouseScrollDown over a GraphNode pill must zoom out"
    );

    // Wheel over Sidebar still scrolls sidebar, NOT graph zoom!
    let sidebar_rect = ui
        .last_hitmap
        .region_of(HitTarget::Pane(Focus::Sidebar))
        .expect("Sidebar pane registered");
    let zoom_before_sidebar_scroll = ui.graph_sim.zoom;
    ui.sidebar_selected = 0;
    app.dispatch(
        Action::MouseScrollDown {
            col: sidebar_rect.x + 2,
            row: sidebar_rect.y + 4,
        },
        &mut ui,
    );
    assert_eq!(ui.graph_sim.zoom, zoom_before_sidebar_scroll);
    assert_eq!(ui.sidebar_selected, 1);

    // 4. Dragging empty GraphCanvas pans smoothly
    let pan_before = (ui.graph_sim.pan_x, ui.graph_sim.pan_y);
    app.dispatch(
        Action::MouseClick {
            col: canvas_rect.x + 1,
            row: canvas_rect.y + 1,
        },
        &mut ui,
    );
    app.dispatch(
        Action::MouseDrag {
            col: canvas_rect.x + 11,
            row: canvas_rect.y + 5,
        },
        &mut ui,
    );
    app.dispatch(
        Action::MouseUp {
            col: canvas_rect.x + 11,
            row: canvas_rect.y + 5,
        },
        &mut ui,
    );
    assert!(ui.graph_sim.pan_x > pan_before.0);
    assert!(ui.graph_sim.pan_y > pan_before.1);

    // 5. Keyboard shortcuts in Graph View: `+`/`-` zoom, `r`/`0` reset, `[`/`]` depth
    let act_plus = key_to_action(
        KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE),
        Focus::Canvas,
        CanvasMode::Graph,
        &ui.keymap,
        false,
    );
    assert_eq!(act_plus, Some(Action::GraphZoomIn));

    let act_minus = key_to_action(
        KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE),
        Focus::Canvas,
        CanvasMode::Graph,
        &ui.keymap,
        false,
    );
    assert_eq!(act_minus, Some(Action::GraphZoomOut));

    let act_r = key_to_action(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        Focus::Canvas,
        CanvasMode::Graph,
        &ui.keymap,
        false,
    );
    assert_eq!(
        act_r,
        Some(Action::GraphResetView),
        "'r' in Graph View must map to GraphResetView, not RefreshAll"
    );

    let act_depth_inc = key_to_action(
        KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
        Focus::Canvas,
        CanvasMode::Graph,
        &ui.keymap,
        false,
    );
    let act_depth_dec = key_to_action(
        KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
        Focus::Canvas,
        CanvasMode::Graph,
        &ui.keymap,
        false,
    );
    assert_eq!(act_depth_inc, Some(Action::GraphDepthInc));
    assert_eq!(act_depth_dec, Some(Action::GraphDepthDec));

    app.dispatch(Action::GraphResetView, &mut ui);
    assert!((ui.graph_sim.zoom - 1.0).abs() < 1e-9);
    assert!((ui.graph_sim.pan_x - 0.0).abs() < 1e-9);
    assert!((ui.graph_sim.pan_y - 0.0).abs() < 1e-9);

    // Verify rendering at extreme zoom levels (0.25x low-zoom compact dots and 4.0x high-zoom edge clipping)
    // completes cleanly with zero panics and registers header zoom buttons!
    for target_zoom in [0.25, 0.55, 1.0, 2.5, 6.0] {
        ui.graph_sim.zoom = target_zoom;
        ui.graph_sim.pan_x = 0.25;
        ui.graph_sim.pan_y = -0.15;
        terminal
            .draw(|f| {
                let _ = ui::draw(f, &app, &mut ui);
            })
            .unwrap();
        assert!(ui
            .last_hitmap
            .region_of(HitTarget::Button(ButtonId::GraphZoomIn))
            .is_some());
        assert!(ui
            .last_hitmap
            .region_of(HitTarget::Button(ButtonId::GraphZoomOut))
            .is_some());
        assert!(ui
            .last_hitmap
            .region_of(HitTarget::Button(ButtonId::GraphResetView))
            .is_some());
    }
}

#[test]
fn test_inline_editor_delta_undo_redo_survives_autosave_and_ctrl_s_with_sqlite_index() {
    use std::time::{Duration, Instant};

    use obsidian_cli::ui::inline_editor::InlineSaveStatus;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("UndoSaveVault");
    fs::create_dir_all(&root).unwrap();
    seed_vault(&root);

    let note_rel = "Projects/Q4 Architecture RFC.md";
    let note_abs = root.join(note_rel);
    let original_disk = fs::read_to_string(&note_abs).unwrap();

    let vault = vault_from_dir(&root, Some("UndoSaveVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let mut app = AppState::new(
        cfg.clone(),
        vault.clone(),
        storage,
        SyncController::from_channels(focus_tx, cmd_tx),
    );
    let mut ui = UiState::from_config(&cfg);
    app.reload_from_storage(&mut ui);
    app.open_note(&mut ui, note_rel, false);

    let trigger_autosave = |app: &mut AppState, ui: &mut UiState| {
        ui.inline_editor.as_mut().unwrap().dirty_since =
            Some(Instant::now() - Duration::from_millis(300));
        app.dispatch(Action::InlineEditTick, ui);
    };

    // Enter inline edit mode below YAML frontmatter and paste Korean + emoji + tag.
    app.dispatch(Action::OpenNoteEditor, &mut ui);
    for _ in 0..7 {
        app.dispatch(Action::CursorDown, &mut ui);
    }
    app.dispatch(Action::CursorEnd, &mut ui);
    let cursor_before_paste = ui.inline_editor.as_ref().unwrap().composer.cursor();
    app.dispatch(
        Action::Paste("\n## 한글 요약 🚀 #undodelta".into()),
        &mut ui,
    );
    let cursor_after_paste = ui.inline_editor.as_ref().unwrap().composer.cursor();

    // 1. Autosave persists file + SQLite index without clearing Undo history.
    trigger_autosave(&mut app, &mut ui);
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().save_status,
        InlineSaveStatus::Saved
    );
    assert!(fs::read_to_string(&note_abs)
        .unwrap()
        .contains("## 한글 요약 🚀 #undodelta"));
    assert!(app.tags.iter().any(|t| t.tag == "undodelta"));
    assert_eq!(
        app.storage
            .search_notes(&vault.id, "한글 요약", 10)
            .unwrap()
            .len(),
        1
    );

    // 2. Append '!', save via Ctrl+S, move cursor away, then Undo across both saves.
    app.dispatch(Action::Input('!'), &mut ui);
    app.dispatch(Action::SaveInlineEdit, &mut ui);
    assert!(fs::read_to_string(&note_abs)
        .unwrap()
        .contains("#undodelta!"));

    app.dispatch(Action::CursorHome, &mut ui);
    app.dispatch(Action::Undo, &mut ui);
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().composer.cursor(),
        cursor_after_paste
    );

    app.dispatch(Action::Undo, &mut ui);
    let ed = ui.inline_editor.as_ref().unwrap();
    assert_eq!(
        (ed.composer.cursor(), ed.disk_markdown()),
        (cursor_before_paste, original_disk.clone())
    );

    app.dispatch(Action::SaveInlineEdit, &mut ui);
    assert_eq!(fs::read_to_string(&note_abs).unwrap(), original_disk);
    assert!(!app.tags.iter().any(|t| t.tag == "undodelta"));
    assert!(app
        .storage
        .search_notes(&vault.id, "한글 요약", 10)
        .unwrap()
        .is_empty());

    // 3. Redo + autosave restores disk and SQLite index; new edit discards remaining redo ('!').
    app.dispatch(Action::Redo, &mut ui);
    trigger_autosave(&mut app, &mut ui);
    assert!(fs::read_to_string(&note_abs)
        .unwrap()
        .contains("## 한글 요약 🚀 #undodelta"));
    assert!(app.tags.iter().any(|t| t.tag == "undodelta"));

    app.dispatch(Action::Input('?'), &mut ui);
    app.dispatch(Action::Redo, &mut ui);
    app.dispatch(Action::SaveInlineEdit, &mut ui);
    let final_disk = fs::read_to_string(&note_abs).unwrap();
    assert!(final_disk.contains("#undodelta?") && !final_disk.contains("#undodelta!"));
}

#[test]
fn test_inline_editor_external_conflict_protection_clean_reload_and_deleted_file_guard() {
    use std::time::{Duration, Instant};

    use obsidian_cli::{
        models::CoreEvent,
        ui::inline_editor::{ExternalConflict, InlineSaveStatus},
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ConflictVault");
    fs::create_dir_all(&root).unwrap();
    let note_rel = "EditNote.md";
    let note_abs = root.join(note_rel);
    fs::write(&note_abs, "# EDIT_READY\n\nOld text on a second line.\n").unwrap();

    let vault = vault_from_dir(&root, Some("ConflictVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let mut app = AppState::new(
        cfg.clone(),
        vault.clone(),
        storage,
        SyncController::from_channels(focus_tx, cmd_tx),
    );
    let mut ui = UiState::from_config(&cfg);
    app.reload_from_storage(&mut ui);
    app.open_note(&mut ui, note_rel, false);

    // 1. Clean open editor reloads external edits when VaultIndexed arrives.
    app.dispatch(Action::OpenNoteEditor, &mut ui);
    assert!(ui.inline_editor.is_some());
    assert!(!ui.inline_editor.as_ref().unwrap().dirty);

    fs::write(&note_abs, "# EDIT_READY\n\nExternal v2 on second line.\n").unwrap();
    let stats = app.storage.sync_vault(&vault).unwrap();
    app.handle_core_event(CoreEvent::VaultIndexed { stats }, &mut ui);
    {
        let ed = ui.inline_editor.as_ref().unwrap();
        assert!(!ed.dirty);
        assert_eq!(ed.save_status, InlineSaveStatus::Saved);
        assert!(
            ed.composer.text().contains("External v2 on second line."),
            "clean open editor must reload external edits"
        );
    }

    // 2. Concurrent edit: local edits line 1, external edits line 2.
    //    Autosave, Ctrl+S, and Esc must ALL preserve external line 2 on disk AND keep local input in editor!
    app.dispatch(Action::CursorHome, &mut ui);
    app.dispatch(Action::Input('L'), &mut ui);
    assert!(ui.inline_editor.as_ref().unwrap().dirty);
    assert!(ui
        .inline_editor
        .as_ref()
        .unwrap()
        .composer
        .text()
        .starts_with("L# EDIT_READY"));

    fs::write(&note_abs, "# EDIT_READY\n\nExternal v3 must survive!\n").unwrap();

    // 2a. Autosave tick
    ui.inline_editor.as_mut().unwrap().dirty_since =
        Some(Instant::now() - Duration::from_millis(400));
    app.dispatch(Action::InlineEditTick, &mut ui);
    assert_eq!(
        fs::read_to_string(&note_abs).unwrap(),
        "# EDIT_READY\n\nExternal v3 must survive!\n",
        "autosave must not overwrite external edits"
    );
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().save_status,
        InlineSaveStatus::Conflict(ExternalConflict::Modified)
    );
    assert!(
        ui.inline_editor
            .as_ref()
            .unwrap()
            .composer
            .text()
            .starts_with("L# EDIT_READY"),
        "local input must remain in open editor on conflict"
    );
    assert!(
        !ui.inline_editor.as_ref().unwrap().is_autosave_due(),
        "autosave must remain paused while conflicted"
    );

    // 2b. Manual Ctrl+S
    app.dispatch(Action::SaveInlineEdit, &mut ui);
    assert_eq!(
        fs::read_to_string(&note_abs).unwrap(),
        "# EDIT_READY\n\nExternal v3 must survive!\n",
        "Ctrl+S must not overwrite external edits"
    );
    assert!(ui
        .inline_editor
        .as_ref()
        .unwrap()
        .composer
        .text()
        .starts_with("L# EDIT_READY"));

    // 2c. FinishInlineEdit (Esc / Done) must also block overwrite and keep local edits open
    app.dispatch(Action::FinishInlineEdit, &mut ui);
    assert!(
        ui.inline_editor.is_some(),
        "FinishInlineEdit must keep conflicted editor open so local edits are not lost"
    );
    assert_eq!(
        fs::read_to_string(&note_abs).unwrap(),
        "# EDIT_READY\n\nExternal v3 must survive!\n"
    );

    // 3. External move/delete while open editor has local edits -> old path must NEVER be recreated,
    //    AND Quit / QuitConfirm must NOT drop the conflict draft without explicit confirmation!
    let moved_abs = root.join("MovedElsewhere.md");
    fs::rename(&note_abs, &moved_abs).unwrap();
    let stats_mv = app.storage.sync_vault(&vault).unwrap();
    app.handle_core_event(CoreEvent::VaultIndexed { stats: stats_mv }, &mut ui);
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().save_status,
        InlineSaveStatus::Conflict(ExternalConflict::Deleted)
    );
    app.dispatch(Action::SaveInlineEdit, &mut ui);
    app.dispatch(Action::FinishInlineEdit, &mut ui);
    assert!(
        !note_abs.exists(),
        "old path must NOT be recreated after external move/delete"
    );
    assert!(moved_abs.is_file());
    assert!(ui
        .inline_editor
        .as_ref()
        .unwrap()
        .composer
        .text()
        .starts_with("L# EDIT_READY"));

    // 4. Attempting QuitConfirm / Quit during a conflict draft must keep app open with draft preserved
    //    and prompt for explicit discard confirmation instead of dropping the draft or recreating the file!
    app.dispatch(Action::QuitConfirm, &mut ui);
    assert!(
        !ui.should_quit,
        "QuitConfirm must NOT silently drop an unsaved conflict draft"
    );
    assert!(
        matches!(
            ui.overlay,
            Some(obsidian_cli::ui::overlays::Overlay::Confirm(_))
        ),
        "QuitConfirm during conflict must open discard confirmation overlay"
    );
    assert!(
        !note_abs.exists(),
        "QuitConfirm must NOT recreate deleted file"
    );
    assert!(ui
        .inline_editor
        .as_ref()
        .unwrap()
        .composer
        .text()
        .starts_with("L# EDIT_READY"));

    // Cancelling the discard overlay returns cleanly to the preserved conflict draft
    app.dispatch(Action::CloseOverlay, &mut ui);
    assert!(ui.overlay.is_none());
    assert!(!ui.should_quit);
    assert!(ui
        .inline_editor
        .as_ref()
        .unwrap()
        .composer
        .text()
        .starts_with("L# EDIT_READY"));

    // Explicitly confirming ForceQuitDiscard exits cleanly WITHOUT recreating the deleted file
    app.dispatch(Action::ForceQuitDiscard, &mut ui);
    assert!(ui.should_quit);
    assert!(
        !note_abs.exists(),
        "ForceQuitDiscard must NOT recreate deleted file"
    );
}

#[test]
fn test_sync_vault_read_failure_preserves_index_non_utf8_and_same_mtime_same_size() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ResilientVault");
    fs::create_dir_all(&root).unwrap();

    // Seed an existing indexed note and a same-mtime/same-size note
    let existing_note = root.join("KeepGoodIndex.md");
    fs::write(
        &existing_note,
        "# KeepGoodIndex\n\nGOOD_EXISTING_TOKEN_111 #goodtag\n",
    )
    .unwrap();

    let same_tick = root.join("SameTick.md");
    fs::write(&same_tick, "# SameTick\n\n- [ ] Toggle target\n").unwrap();

    let vault = vault_from_dir(&root, Some("ResilientVault"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();
    assert_eq!(
        storage
            .search_notes(&vault.id, "GOOD_EXISTING_TOKEN_111", 10)
            .unwrap()
            .len(),
        1
    );

    // 1a. Sub-millisecond (nanosecond) mtime change with same size is detected by normal stat-only sync_vault
    let orig_modified = fs::metadata(&same_tick).unwrap().modified().unwrap();
    let sub_ms_modified = orig_modified + std::time::Duration::from_nanos(500_000); // +0.5ms (same mtime_ms!)
    fs::write(&same_tick, "# SameTick\n\n- [x] Toggle target\n").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&same_tick)
        .unwrap()
        .set_modified(sub_ms_modified)
        .unwrap();
    let s_subms = storage.sync_vault(&vault).unwrap();
    assert_eq!(
        s_subms.updated, 1,
        "normal stat-only sync_vault must detect sub-ms nanosecond mtime_ns change"
    );
    let tasks_subms = storage
        .list_tasks(
            &vault.id,
            obsidian_cli::models::TaskFilter::All,
            Some("SameTick.md"),
        )
        .unwrap();
    assert_eq!(tasks_subms.len(), 1);
    assert!(tasks_subms[0].checked);

    // 1b. Exact same-mtime_ns + same-size edit (`- [x]` -> `- [ ]` preserving exact ns):
    //     normal stat-only sync_vault avoids full-vault re-hashing (updated == 0),
    //     while explicit reindex_vault verifies content hashes and detects the change (updated == 1)!
    let exact_modified = fs::metadata(&same_tick).unwrap().modified().unwrap();
    fs::write(&same_tick, "# SameTick\n\n- [ ] Toggle target\n").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&same_tick)
        .unwrap()
        .set_modified(exact_modified)
        .unwrap();
    let s_stat = storage.sync_vault(&vault).unwrap();
    assert_eq!(
        s_stat.updated, 0,
        "normal sync_vault must rely on stat metadata without hashing every unchanged file"
    );
    let s_explicit = storage.reindex_vault(&vault).unwrap();
    assert_eq!(
        s_explicit.updated, 1,
        "explicit reindex_vault must detect exact same-mtime_ns + same-size content changes"
    );
    let tasks = storage
        .list_tasks(
            &vault.id,
            obsidian_cli::models::TaskFilter::All,
            Some("SameTick.md"),
        )
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert!(
        !tasks[0].checked,
        "task checkbox state must update to checked=false after explicit reindex"
    );

    // 2. Non-UTF-8 file must preserve readable ASCII tokens instead of indexing empty body
    let bad_utf8 = root.join("LegacyEncoding.md");
    fs::write(
        &bad_utf8,
        b"# Legacy\n\n\xff\xfeInvalid UTF8 bytes with ASCII_TOKEN_909\n",
    )
    .unwrap();
    storage.sync_vault(&vault).unwrap();
    let utf8_hits = storage
        .search_notes(&vault.id, "ASCII_TOKEN_909", 10)
        .unwrap();
    assert_eq!(
        utf8_hits.len(),
        1,
        "lossy UTF-8 decoding must keep searchable tokens in FTS5"
    );

    // 3. Exclusive lock on Windows during sync_vault must keep existing index & not poison new file
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        let new_locked = root.join("LockedNew.md");
        fs::write(
            &new_locked,
            "# LockedNew\n\nUNLOCKED_RECOVERY_TOKEN_808 #recovered\n",
        )
        .unwrap();

        // Hold exclusive locks (share_mode = 0) on both existing and new files
        let lock_existing = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&existing_note)
            .unwrap();
        let lock_new = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&new_locked)
            .unwrap();

        let _ = storage.sync_vault(&vault).unwrap();
        // Existing note must NOT be overwritten with empty content!
        assert_eq!(
            storage
                .search_notes(&vault.id, "GOOD_EXISTING_TOKEN_111", 10)
                .unwrap()
                .len(),
            1,
            "locked existing note must preserve last successful index"
        );
        // New locked note must NOT be poisoned as an empty row in notes
        assert!(
            storage
                .get_note_summary(&vault.id, "LockedNew.md")
                .unwrap()
                .is_none(),
            "unreadable new note must be skipped rather than committed empty"
        );

        drop(lock_existing);
        drop(lock_new);

        // Once unlocked, sync_vault indexes LockedNew.md cleanly!
        let s_recover = storage.sync_vault(&vault).unwrap();
        assert!(s_recover.updated >= 1);
        let hits = storage
            .search_notes(&vault.id, "UNLOCKED_RECOVERY_TOKEN_808", 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rel_path, "LockedNew.md");
    }
}

#[tokio::test]
async fn test_cross_process_delta_stealing_watcher_and_html_files_refresh() {
    use std::time::Duration;

    use obsidian_cli::sync::spawn_vault_watcher;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("SharedDbVault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Welcome.md"), "# Welcome\n\nInitial body.\n").unwrap();
    let db_file = tmp.path().join("index.db");

    let vault = vault_from_dir(&root, Some("SharedDbVault"));
    let storage_tui = Storage::open(&db_file).unwrap();
    let storage_cli = Storage::open(&db_file).unwrap();
    storage_tui.sync_vault(&vault).unwrap();

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let sync_ctrl = spawn_vault_watcher(vault.clone(), storage_tui.clone(), 500, ev_tx);

    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let mut app = AppState::new(cfg.clone(), vault.clone(), storage_tui.clone(), sync_ctrl);
    let mut ui = UiState::from_config(&cfg);

    // Consume initial startup VaultIndexed event
    let first_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
        .await
        .expect("expected initial VaultIndexed")
        .unwrap();
    app.handle_core_event(first_ev, &mut ui);
    assert!(app
        .active_note
        .as_ref()
        .unwrap()
        .raw_markdown
        .contains("Initial body."));

    // Open clean inline editor on Welcome.md too
    app.dispatch(Action::OpenNoteEditor, &mut ui);
    assert!(ui
        .inline_editor
        .as_ref()
        .unwrap()
        .composer
        .text()
        .contains("Initial body."));

    // Simulate external edit + CLI/Daemon indexing FIRST (stealing the delta!)
    fs::write(
        root.join("Welcome.md"),
        "# Welcome\n\nDELTA_STOLEN_BY_CLI_606\n",
    )
    .unwrap();
    let cli_stats = storage_cli.sync_vault(&vault).unwrap();
    assert_eq!(cli_stats.updated, 1);

    // TUI watcher must still detect the cross-process DB fingerprint change and emit VaultIndexed!
    let stolen_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
        .await
        .expect("watcher must emit VaultIndexed even when another process indexed index.db first")
        .unwrap();
    app.handle_core_event(stolen_ev, &mut ui);
    assert!(
        app.active_note
            .as_ref()
            .unwrap()
            .raw_markdown
            .contains("DELTA_STOLEN_BY_CLI_606"),
        "open active note must refresh after cross-process index update"
    );
    assert!(
        ui.inline_editor
            .as_ref()
            .unwrap()
            .composer
            .text()
            .contains("DELTA_STOLEN_BY_CLI_606"),
        "clean open inline editor must refresh after cross-process index update"
    );

    // Adding an external .html file must also trigger watcher refresh of app.html_files
    fs::write(root.join("page.html"), "<html><body>Hello</body></html>").unwrap();
    let html_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
        .await
        .expect("watcher must emit VaultIndexed on external .html change")
        .unwrap();
    app.handle_core_event(html_ev, &mut ui);
    assert_eq!(app.html_files.len(), 1);
    assert_eq!(app.html_files[0].rel_path, "page.html");
}

#[tokio::test]
async fn test_http_serve_and_mcp_live_external_sync_and_stale_task_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ServerSyncVault");
    fs::create_dir_all(&root).unwrap();
    let tasks_abs = root.join("TasksNote.md");
    fs::write(
        &tasks_abs,
        "---\nstatus: initial\ntags: [origtag]\n---\n\n# TasksNote\n\n- [ ] First task alpha\n- [ ] Second task beta\n",
    )
    .unwrap();
    fs::write(
        root.join("ToBeDeleted.md"),
        "# ToBeDeleted\n\nDELETE_TOKEN_202\n",
    )
    .unwrap();
    fs::write(
        root.join("ToBeRenamed.md"),
        "# ToBeRenamed\n\nRENAME_TOKEN_203\n",
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("ServerSyncVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let db_file = tmp.path().join("srv_index.db");
    let storage_http = Storage::open(&db_file).unwrap();
    storage_http.sync_vault(&vault).unwrap();

    let mcp = McpServer::new(cfg.clone(), vault.clone(), Storage::open(&db_file).unwrap());

    let state = Arc::new(ServeState {
        cfg: cfg.clone(),
        default_vault: vault.clone(),
        storage: storage_http,
        token: None,
    });
    let app_router = build_router(state).layer(CorsLayer::permissive());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app_router).await.unwrap();
    });
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // Capture initial task line_no (line 8) before external line insertion
    let initial_tasks: Value = client
        .get(format!("{base}/api/v1/tasks?note=TasksNote.md"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(initial_tasks["tasks"][0]["line_no"], 8);
    assert_eq!(initial_tasks["tasks"][0]["text"], "First task alpha");

    // Also set up TUI AppState snapshot BEFORE the external insertion to test TUI stale task guard
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let mut tui_app = AppState::new(
        cfg.clone(),
        vault.clone(),
        Storage::open(&db_file).unwrap(),
        SyncController::from_channels(focus_tx, cmd_tx),
    );
    let mut tui_ui = UiState::from_config(&cfg);
    tui_app.reload_from_storage(&mut tui_ui);
    tui_app.open_note(&mut tui_ui, "TasksNote.md", false);

    // External changes on disk while HTTP, MCP, and TUI are already running
    fs::write(
        root.join("ObsidianNew.md"),
        "---\ntags: [extcreated]\n---\n# ObsidianNew\n\nHTTP_EXT_CREATE_991\n",
    )
    .unwrap();
    fs::write(
        &tasks_abs,
        "---\nstatus: edited_by_obsidian\ntags: [origtag, extedited]\n---\n\n# TasksNote\n\n- [ ] Inserted by Obsidian at line 8\n- [ ] First task alpha\n- [ ] Second task beta\n\nHTTP_EXT_EDIT_772\n",
    )
    .unwrap();
    fs::rename(
        root.join("ToBeRenamed.md"),
        root.join("RenamedExternally.md"),
    )
    .unwrap();
    fs::remove_file(root.join("ToBeDeleted.md")).unwrap();

    // 1. TUI stale ToggleTaskAtLine (line 8 expected "First task alpha", now line 8 is "Inserted by Obsidian at line 8")
    //    must refuse to toggle the wrong task and refresh TUI!
    tui_app.dispatch(
        Action::ToggleTaskAtLine {
            note_path: "TasksNote.md".into(),
            line_no: 8,
        },
        &mut tui_ui,
    );
    let disk_after_tui = fs::read_to_string(&tasks_abs).unwrap();
    assert!(
        disk_after_tui.contains("- [ ] Inserted by Obsidian at line 8"),
        "TUI stale task toggle must NOT toggle the externally inserted task"
    );
    assert!(disk_after_tui.contains("- [ ] First task alpha"));

    // 2. HTTP default vault requests (no ?vault= param) immediately see create/edit/rename/delete
    let s_create: Value = client
        .get(format!("{base}/api/v1/search?q=HTTP_EXT_CREATE_991"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s_create["count"], 1);

    let r_edited: Value = client
        .get(format!("{base}/api/v1/notes/TasksNote.md"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        r_edited["note"]["summary"]["frontmatter"]["status"],
        "edited_by_obsidian"
    );
    assert_eq!(
        r_edited["note"]["summary"]["tags"], r_edited["note"]["parsed"]["tags"],
        "summary and parsed metadata must match"
    );

    let r_renamed = client
        .get(format!("{base}/api/v1/notes/RenamedExternally.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(r_renamed.status(), StatusCode::OK);

    let s_deleted: Value = client
        .get(format!("{base}/api/v1/search?q=DELETE_TOKEN_202"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s_deleted["count"], 0);

    // 3. HTTP tasks list shows updated line numbers AND stale expected_text returns 409 CONFLICT
    let fresh_tasks: Value = client
        .get(format!("{base}/api/v1/tasks?note=TasksNote.md"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(fresh_tasks["tasks"][0]["line_no"], 8);
    assert_eq!(
        fresh_tasks["tasks"][0]["text"],
        "Inserted by Obsidian at line 8"
    );
    assert_eq!(fresh_tasks["tasks"][1]["line_no"], 9);
    assert_eq!(fresh_tasks["tasks"][1]["text"], "First task alpha");

    let stale_http_toggle = client
        .post(format!("{base}/api/v1/tasks/toggle"))
        .json(&json!({
            "note": "TasksNote.md",
            "line": 8,
            "expected_text": "First task alpha"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(stale_http_toggle.status(), StatusCode::CONFLICT);
    assert!(fs::read_to_string(&tasks_abs)
        .unwrap()
        .contains("- [ ] Inserted by Obsidian at line 8"));

    // 4. MCP default vault tools immediately see all external changes and reject stale expected_text
    let mcp_search = mcp
        .dispatch_tool("search_notes", &json!({ "query": "HTTP_EXT_CREATE_991" }))
        .unwrap();
    assert_eq!(mcp_search["count"], 1);
    let mcp_del = mcp
        .dispatch_tool("search_notes", &json!({ "query": "DELETE_TOKEN_202" }))
        .unwrap();
    assert_eq!(mcp_del["count"], 0);
    let mcp_read = mcp
        .dispatch_tool(
            "read_note",
            &json!({ "note": "TasksNote.md", "compact": false }),
        )
        .unwrap();
    assert_eq!(mcp_read["frontmatter"]["status"], "edited_by_obsidian");
    assert!(
        mcp.dispatch_tool(
            "toggle_task",
            &json!({
                "note": "TasksNote.md",
                "line": 8,
                "expected_text": "First task alpha"
            })
        )
        .is_err(),
        "MCP toggle_task with mismatched expected_text must fail without editing disk"
    );
    assert!(fs::read_to_string(&tasks_abs)
        .unwrap()
        .contains("- [ ] Inserted by Obsidian at line 8"));

    server.abort();
}

#[test]
fn test_attachment_preview_cache_refreshes_on_external_overwrite_and_move_preserves_links_and_bookmark(
) {
    use std::io::Cursor;

    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
    use obsidian_cli::{
        ui::overlays::image_preview::ImagePreviewOverlay,
        vault::fs::{inline_image_cache_key, move_note, rewrite_wikilinks_for_move},
    };

    let make_solid_png = |rgba: [u8; 4]| -> Vec<u8> {
        let img: RgbaImage = ImageBuffer::from_fn(8, 8, |_, _| Rgba(rgba));
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("AttachMoveVault");
    fs::create_dir_all(root.join("Attachments")).unwrap();
    let img_abs = root.join("Attachments").join("sample.png");
    let red_png = make_solid_png([220, 20, 20, 255]);
    let blue_png = make_solid_png([20, 40, 240, 255]);
    fs::write(&img_abs, &red_png).unwrap();

    fs::write(
        root.join("TargetNote.md"),
        "# TargetNote\n\n![[sample.png]]\n",
    )
    .unwrap();
    fs::write(
        root.join("Referrer.md"),
        "# Referrer\n\nSee [[TargetNote]] and [[TargetNote|Alias]] and `[[TargetNote]]`.\n",
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("AttachMoveVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    // 1. Attachment preview cache invalidation on external overwrite (inline + modal)
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let mut app = AppState::new(
        cfg.clone(),
        vault.clone(),
        storage.clone(),
        SyncController::from_channels(focus_tx, cmd_tx),
    );
    let mut ui = UiState::from_config(&cfg);
    app.reload_from_storage(&mut ui);
    app.open_note(&mut ui, "TargetNote.md", false);

    let key1_inline = inline_image_cache_key(&img_abs);
    let rendered1_inline = app
        .preview_cache
        .get(&key1_inline)
        .expect("inline image should be cached")
        .lines
        .clone();

    let mut modal =
        ImagePreviewOverlay::from_note(app.active_note.as_ref().unwrap(), Some(0)).unwrap();
    app.ensure_modal_image_cached(&mut modal, 100, 30);
    let (cols, rows) = modal.target_dimensions(100, 30);
    let key1_modal = ImagePreviewOverlay::cache_key(
        modal.current().unwrap(),
        cols,
        rows,
        app.graphics_caps.protocol,
    );
    let rendered1_modal = app.preview_cache.get(&key1_modal).unwrap().lines.clone();

    // Overwrite image externally with a different color & bumped mtime
    let bumped =
        fs::metadata(&img_abs).unwrap().modified().unwrap() + std::time::Duration::from_secs(2);
    fs::write(&img_abs, &blue_png).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&img_abs)
        .unwrap()
        .set_modified(bumped)
        .unwrap();

    app.reload_from_storage(&mut ui);
    let key2_inline = inline_image_cache_key(&img_abs);
    assert_ne!(key1_inline, key2_inline);
    let rendered2_inline = app
        .preview_cache
        .get(&key2_inline)
        .expect("updated inline image should be cached")
        .lines
        .clone();
    assert_ne!(
        rendered1_inline, rendered2_inline,
        "inline preview must refresh when attachment file is overwritten externally"
    );

    app.ensure_modal_image_cached(&mut modal, 100, 30);
    let key2_modal = ImagePreviewOverlay::cache_key(
        modal.current().unwrap(),
        cols,
        rows,
        app.graphics_caps.protocol,
    );
    assert_ne!(key1_modal, key2_modal);
    let rendered2_modal = app.preview_cache.get(&key2_modal).unwrap().lines.clone();
    assert_ne!(
        rendered1_modal, rendered2_modal,
        "modal preview must refresh when attachment file is overwritten externally"
    );

    // 2. Supported move operation preserves bookmark and rewrites incoming wikilinks
    assert!(storage.toggle_bookmark(&vault.id, "TargetNote.md").unwrap());
    let (paths_before, aliases_before) = storage.all_paths_and_aliases(&vault.id).unwrap();
    let new_rel = move_note(&vault.path, "TargetNote.md", "Archive/TargetRenamed.md").unwrap();
    let updated_count = rewrite_wikilinks_for_move(
        &vault.path,
        "TargetNote.md",
        &new_rel,
        &paths_before,
        &aliases_before,
    )
    .unwrap();
    assert_eq!(updated_count, 1);
    storage.sync_vault(&vault).unwrap();
    storage.set_bookmark(&vault.id, &new_rel, true).unwrap();

    let ref_disk = fs::read_to_string(root.join("Referrer.md")).unwrap();
    assert!(ref_disk.contains("[[TargetRenamed]]"));
    assert!(ref_disk.contains("[[TargetRenamed|Alias]]"));
    assert!(
        ref_disk.contains("`[[TargetNote]]`"),
        "inline code spans must remain untouched during link rewrite"
    );
    let bl = storage.get_backlinks(&vault.id, &new_rel).unwrap();
    assert_eq!(bl.len(), 1);
    assert_eq!(bl[0].source_path, "Referrer.md");
    assert!(
        storage
            .get_note_summary(&vault.id, &new_rel)
            .unwrap()
            .unwrap()
            .is_bookmarked
    );
}

#[tokio::test]
async fn test_realtime_watcher_html_preview_reload_rapid_saves_backlinks_and_dirty_draft() {
    use std::time::Duration;

    use obsidian_cli::{
        config::write_atomic,
        sync::{spawn_vault_watcher_with_options, WatcherMode, WatcherOptions},
        ui::overlays::Overlay,
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("RealtimeVault");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Alpha.md"),
        "---\ntags: [init]\n---\n# Alpha\n\nLinks to [[FutureNote]].\n\n- [ ] Alpha initial\n",
    )
    .unwrap();
    fs::write(
        root.join("Report.html"),
        "<!DOCTYPE html><html><head><title>Report V1</title></head><body><h1>V1_HTML_BODY</h1></body></html>",
    )
    .unwrap();

    let db_file = tmp.path().join("rt.db");
    let vault = vault_from_dir(&root, Some("RealtimeVault"));
    let storage = Storage::open(&db_file).unwrap();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let (sync_ctrl, diag) = spawn_vault_watcher_with_options(
        vault.clone(),
        storage.clone(),
        2_500,
        ev_tx,
        WatcherOptions::default(),
    );
    assert_eq!(diag.mode(), WatcherMode::Native);

    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let mut app = AppState::new(cfg.clone(), vault.clone(), storage.clone(), sync_ctrl);
    let mut ui = UiState::from_config(&cfg);

    let init_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
        .await
        .unwrap()
        .unwrap();
    app.handle_core_event(init_ev, &mut ui);
    app.open_note(&mut ui, "Alpha.md", false);

    // 1. Open HTML preview on Report.html, then atomically replace Report.html externally -> live reload!
    assert!(app.open_html_file(&mut ui, "Report.html"));
    write_atomic(
        &root.join("Report.html"),
        b"<!DOCTYPE html><html><head><title>Report V2</title></head><body><h1>V2_REPLACED_HTML</h1></body></html>",
    )
    .unwrap();
    let ev_html = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
        .await
        .unwrap()
        .unwrap();
    app.handle_core_event(ev_html, &mut ui);
    match ui.overlay.as_ref() {
        Some(Overlay::HtmlPreview(ov)) => {
            assert!(ov.session.raw_html().contains("V2_REPLACED_HTML"));
            assert_eq!(
                ov.session.last_status.as_deref(),
                Some("Reloaded updated HTML")
            );
        }
        other => panic!("expected refreshed HtmlPreview overlay, got {other:?}"),
    }
    ui.close_overlay();

    // 2. Rapid consecutive saves on Alpha.md coalesce cleanly to final state
    for i in 1..=8 {
        write_atomic(
            &root.join("Alpha.md"),
            format!(
                "---\ntags: [init, rapid]\n---\n# Alpha\n\nRAPID_SAVE_{i}\n\nLinks to [[FutureNote]].\n\n- [ ] Alpha task {i}\n"
            )
            .as_bytes(),
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(8)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = tokio::time::timeout(rem, ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle_core_event(ev, &mut ui);
        if app
            .active_note
            .as_ref()
            .is_some_and(|d| d.raw_markdown.contains("RAPID_SAVE_8"))
        {
            break;
        }
    }
    assert_eq!(
        storage
            .search_notes(&vault.id, "RAPID_SAVE_8", 10)
            .unwrap()
            .len(),
        1
    );
    assert!(storage
        .list_tags(&vault.id)
        .unwrap()
        .iter()
        .any(|t| t.tag == "rapid"));

    // 3. Create FutureNote.md -> Alpha's unresolved link resolves and FutureNote gets backlink;
    //    then rename FutureNote.md -> RenamedNote.md -> old path cleaned up from search & links!
    fs::write(
        root.join("FutureNote.md"),
        "# FutureNote\n\nFUTURE_BODY_77\n",
    )
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = tokio::time::timeout(rem, ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle_core_event(ev, &mut ui);
        if storage
            .get_backlinks(&vault.id, "FutureNote.md")
            .unwrap()
            .len()
            == 1
        {
            break;
        }
    }
    fs::rename(root.join("FutureNote.md"), root.join("RenamedNote.md")).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = tokio::time::timeout(rem, ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle_core_event(ev, &mut ui);
        let hits_old = storage.search_notes(&vault.id, "FutureNote", 10).unwrap();
        let hits_new = storage
            .search_notes(&vault.id, "FUTURE_BODY_77", 10)
            .unwrap();
        if hits_old.iter().all(|h| h.rel_path != "FutureNote.md")
            && hits_new.len() == 1
            && hits_new[0].rel_path == "RenamedNote.md"
        {
            break;
        }
    }
    assert!(storage
        .get_backlinks(&vault.id, "FutureNote.md")
        .unwrap()
        .is_empty());

    // 4. Dirty inline editor + external atomic edit preserves both local draft and external disk bytes
    app.dispatch(Action::OpenNoteEditor, &mut ui);
    app.dispatch(Action::Input('X'), &mut ui);
    assert!(ui.inline_editor.as_ref().unwrap().dirty);
    write_atomic(
        &root.join("Alpha.md"),
        b"# Alpha\n\nEXTERNAL_CONCURRENT_EDIT_555\n",
    )
    .unwrap();
    let ev_conf = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
        .await
        .unwrap()
        .unwrap();
    app.handle_core_event(ev_conf, &mut ui);
    let ed = ui.inline_editor.as_ref().unwrap();
    assert!(ed.external_conflict.is_some());
    assert!(ed.composer.text().contains('X'));
    assert!(fs::read_to_string(root.join("Alpha.md"))
        .unwrap()
        .contains("EXTERNAL_CONCURRENT_EDIT_555"));
}

#[test]
fn regression_obsidian_obs_01_hash_filename_resolution() {
    use obsidian_cli::vault::fs::content_sha256_hex;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs01Vault");
    fs::create_dir_all(&root).unwrap();

    let meeting_orig = "# Meeting\n\n## Agenda\n\nOriginal meeting body.\n";
    let meeting_1_orig = "# Meeting #1\n\nFirst numbered meeting body.\n";
    let referrer_orig = "# Index\n\nSee [[Meeting#Agenda]].\n";
    fs::write(root.join("Meeting.md"), meeting_orig).unwrap();
    fs::write(root.join("Meeting #1.md"), meeting_1_orig).unwrap();
    fs::write(root.join("Index.md"), referrer_orig).unwrap();
    let meeting_hash_before = content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap());

    // 1. CLI create/read/append/replace/delete for `#` filenames
    let create_cs = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "create", "C# Guide"])
        .output()
        .unwrap();
    assert!(
        create_cs.status.success(),
        "create C# Guide failed: {}",
        String::from_utf8_lossy(&create_cs.stderr)
    );
    assert!(root.join("C# Guide.md").is_file());

    for ident in ["C# Guide", "C# Guide.md"] {
        let out = isolated_cli(tmp.path(), &root)
            .args(["--json", "notes", "read", ident])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "read '{ident}' failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let payload: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(payload["rel_path"], "C# Guide.md");
    }

    // Read Meeting #1.md via CLI -> must open Meeting #1.md, NOT Meeting.md
    let read_m1 = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "read", "Meeting #1.md"])
        .output()
        .unwrap();
    assert!(
        read_m1.status.success(),
        "read Meeting #1.md failed: {}",
        String::from_utf8_lossy(&read_m1.stderr)
    );
    let read_m1_json: Value = serde_json::from_slice(&read_m1.stdout).unwrap();
    assert_eq!(read_m1_json["rel_path"], "Meeting #1.md");
    assert!(read_m1_json["raw_markdown"]
        .as_str()
        .unwrap()
        .contains("First numbered meeting body."));

    // Append to Meeting #1.md -> only Meeting #1.md changes; Meeting.md hash unchanged
    let app_m1 = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "notes",
            "append",
            "Meeting #1.md",
            "ONLY_FOR_MEETING_1",
        ])
        .output()
        .unwrap();
    assert!(
        app_m1.status.success(),
        "append Meeting #1.md failed: {}",
        String::from_utf8_lossy(&app_m1.stderr)
    );
    let app_m1_json: Value = serde_json::from_slice(&app_m1.stdout).unwrap();
    assert_eq!(app_m1_json["rel_path"], "Meeting #1.md");
    assert!(fs::read_to_string(root.join("Meeting #1.md"))
        .unwrap()
        .contains("ONLY_FOR_MEETING_1"));
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap()),
        meeting_hash_before,
        "Meeting.md must not be modified when appending to Meeting #1.md"
    );

    // Control: writing to nonexistent Meeting #2.md must fail with 0 changed files
    let meeting_1_hash_before_bad =
        content_sha256_hex(&fs::read(root.join("Meeting #1.md")).unwrap());
    for args in [
        vec!["notes", "append", "Meeting #2.md", "SHOULD_NOT_WRITE"],
        vec!["notes", "replace", "Meeting #2.md", "Original", "Corrupted"],
        vec!["notes", "delete", "--yes", "Meeting #2.md"],
        vec!["notes", "move", "Meeting #2.md", "Moved.md"],
    ] {
        let bad = isolated_cli(tmp.path(), &root)
            .args(&args)
            .output()
            .unwrap();
        assert!(
            !bad.status.success(),
            "nonexistent Meeting #2.md must fail for {args:?}, stdout={}",
            String::from_utf8_lossy(&bad.stdout)
        );
    }
    assert!(root.join("Meeting.md").is_file());
    assert!(root.join("Meeting #1.md").is_file());
    assert!(!root.join("Meeting #2.md").exists());
    assert!(!root.join("Moved.md").exists());
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap()),
        meeting_hash_before
    );
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting #1.md")).unwrap()),
        meeting_1_hash_before_bad
    );

    // Delete Meeting #1.md -> moves Meeting #1.md to .trash, preserves Meeting.md
    let del_m1 = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "delete", "--yes", "Meeting #1.md"])
        .output()
        .unwrap();
    assert!(
        del_m1.status.success(),
        "delete Meeting #1.md failed: {}",
        String::from_utf8_lossy(&del_m1.stderr)
    );
    let del_m1_json: Value = serde_json::from_slice(&del_m1.stdout).unwrap();
    assert_eq!(del_m1_json["rel_path"], "Meeting #1.md");
    assert!(!root.join("Meeting #1.md").exists());
    assert!(root.join(".trash/Meeting #1.md").is_file());
    assert!(root.join("Meeting.md").is_file());
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap()),
        meeting_hash_before
    );

    // Control: wikilink [[Meeting#Agenda]] resolves to Meeting.md in link context
    let vault = vault_from_dir(&root, Some("Obs01Vault"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();
    let index_links = storage.get_outgoing_links(&vault, "Index.md").unwrap();
    assert!(
        index_links.iter().any(|l| l.target_raw == "Meeting"
            && l.heading.as_deref() == Some("Agenda")
            && l.resolved
            && l.target_resolved_path.as_deref() == Some("Meeting.md")),
        "[[Meeting#Agenda]] must resolve as section link to Meeting.md, got {index_links:?}"
    );
    let meeting_bl = storage.get_backlinks(&vault.id, "Meeting.md").unwrap();
    assert!(meeting_bl.iter().any(|b| b.source_path == "Index.md"));
}

#[tokio::test]
async fn regression_obsidian_obs_01_mcp_and_http_hash_filenames() {
    use obsidian_cli::vault::fs::content_sha256_hex;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs01ApiVault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Meeting.md"), "# Meeting\n\nBase.\n").unwrap();
    fs::write(root.join("Meeting #1.md"), "# Meeting #1\n\nOne.\n").unwrap();
    fs::write(root.join("C# Guide.md"), "# C# Guide\n\nSharp.\n").unwrap();
    let meeting_hash_before = content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap());

    let vault = vault_from_dir(&root, Some("Obs01ApiVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    // MCP read/append/replace/move + nonexistent Meeting #2.md guard
    let mcp = McpServer::new(cfg.clone(), vault.clone(), storage.clone());
    let cs = mcp
        .dispatch_tool("read_note", &json!({ "note": "C# Guide" }))
        .unwrap();
    assert_eq!(cs["rel_path"], "C# Guide.md");
    let cs_ext = mcp
        .dispatch_tool("read_note", &json!({ "note": "C# Guide.md" }))
        .unwrap();
    assert_eq!(cs_ext["rel_path"], "C# Guide.md");
    assert!(mcp
        .dispatch_tool(
            "append_note",
            &json!({ "note": "Meeting #2.md", "text": "NOPE" })
        )
        .is_err());
    let app = mcp
        .dispatch_tool(
            "append_note",
            &json!({ "note": "Meeting #1.md", "text": "MCP_LINE" }),
        )
        .unwrap();
    assert_eq!(app["rel_path"], "Meeting #1.md");
    let rep = mcp
        .dispatch_tool(
            "replace_note_text",
            &json!({ "note": "Meeting #1.md", "find": "One.", "replace": "Two." }),
        )
        .unwrap();
    assert_eq!(rep["rel_path"], "Meeting #1.md");
    assert_eq!(rep["replaced"], true);
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap()),
        meeting_hash_before
    );

    // HTTP read/append for # filename
    let state = Arc::new(ServeState {
        cfg,
        default_vault: vault.clone(),
        storage,
        token: None,
    });
    let router = build_router(state).layer(CorsLayer::permissive());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let r = client
        .get(format!("{base}/api/v1/notes/C%23%20Guide.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r_json: Value = r.json().await.unwrap();
    assert_eq!(r_json["note"]["summary"]["rel_path"], "C# Guide.md");

    let bad_ap = client
        .post(format!("{base}/api/v1/notes/append"))
        .json(&json!({ "note": "Meeting #2.md", "text": "NOPE" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_ap.status(), StatusCode::NOT_FOUND);

    let ok_ap = client
        .post(format!("{base}/api/v1/notes/append"))
        .json(&json!({ "note": "Meeting #1.md", "text": "HTTP_LINE" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok_ap.status(), StatusCode::OK);
    let ok_ap_json: Value = ok_ap.json().await.unwrap();
    assert_eq!(ok_ap_json["rel_path"], "Meeting #1.md");
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Meeting.md")).unwrap()),
        meeting_hash_before
    );

    server.abort();
}

#[test]
fn regression_obsidian_obs_13_vault_selection_conflicts_and_daemon() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use obsidian_cli::vault::fs::content_sha256_hex;

    fn run_cli_bounded(
        cmd: &mut std::process::Command,
        scratch: &std::path::Path,
        label: &str,
        timeout: Duration,
    ) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
        let out_path = scratch.join(format!("{label}.stdout"));
        let err_path = scratch.join(format!("{label}.stderr"));
        let out_file = fs::File::create(&out_path).unwrap();
        let err_file = fs::File::create(&err_path).unwrap();
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_file))
            .stderr(Stdio::from(err_file))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = fs::read_to_string(&out_path).unwrap_or_default();
                let stderr = fs::read_to_string(&err_path).unwrap_or_default();
                panic!("CLI command '{label}' timed out after {timeout:?}\nstdout={stdout}\nstderr={stderr}");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let stdout = fs::read(&out_path).unwrap_or_default();
        let stderr = fs::read(&err_path).unwrap_or_default();
        (status, stdout, stderr)
    }

    struct DaemonStopGuard(std::path::PathBuf);
    impl Drop for DaemonStopGuard {
        fn drop(&mut self) {
            if let Ok(mut child) = isolated_cli_base(&self.0)
                .env_remove("OBSIDIAN_VAULT")
                .args(["daemon", "stop"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let vault_a = tmp.path().join("VaultA");
    let vault_b = tmp.path().join("VaultB");
    let vault_c = tmp.path().join("VaultC");
    fs::create_dir_all(&vault_a).unwrap();
    fs::create_dir_all(&vault_b).unwrap();
    fs::create_dir_all(&vault_c).unwrap();

    fs::write(vault_a.join("NoteA.md"), "# NoteA\n\nAlpha body.\n").unwrap();
    fs::write(vault_b.join("NoteB.md"), "# NoteB\n\nBeta body.\n").unwrap();
    fs::write(vault_c.join("NoteC.md"), "# NoteC\n\nGamma body.\n").unwrap();

    let hash_a = content_sha256_hex(&fs::read(vault_a.join("NoteA.md")).unwrap());
    let hash_b = content_sha256_hex(&fs::read(vault_b.join("NoteB.md")).unwrap());

    // Control 1: First run with zero selection args keeps default native discovery (obsidian.json)
    let discovery_tmp = tempfile::tempdir().unwrap();
    fs::write(
        discovery_tmp.path().join("obsidian.json"),
        json!({
            "vaults": {
                "nat1": { "path": vault_a.to_string_lossy(), "ts": 1732000000i64, "open": true }
            }
        })
        .to_string(),
    )
    .unwrap();
    let first_run = isolated_cli_base(discovery_tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["--json", "notes", "list"])
        .output()
        .unwrap();
    assert!(
        first_run.status.success(),
        "default discovery failed: {}",
        String::from_utf8_lossy(&first_run.stderr)
    );
    let first_notes: Value = serde_json::from_slice(&first_run.stdout).unwrap();
    assert_eq!(first_notes["notes"].as_array().unwrap().len(), 1);
    assert_eq!(first_notes["notes"][0]["rel_path"], "NoteA.md");

    // Register VaultA so A exists in config.toml
    let add_a = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["--json", "vaults", "add"])
        .arg(&vault_a)
        .output()
        .unwrap();
    assert!(
        add_a.status.success(),
        "add VaultA failed: {}",
        String::from_utf8_lossy(&add_a.stderr)
    );
    let config_path = tmp.path().join("config").join("config.toml");
    let cfg_hash_before = content_sha256_hex(&fs::read(&config_path).unwrap());

    // 1. `vaults use <unregistered C>` must fail, advise `vaults add`, and leave config & files untouched
    let use_c = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "use"])
        .arg(&vault_c)
        .output()
        .unwrap();
    assert!(
        !use_c.status.success(),
        "vaults use on unregistered path must fail, stdout={}",
        String::from_utf8_lossy(&use_c.stdout)
    );
    let use_c_err = String::from_utf8_lossy(&use_c.stderr);
    assert!(
        use_c_err.contains("vaults add"),
        "expected vaults add guidance in stderr, got: {use_c_err}"
    );
    assert_eq!(
        content_sha256_hex(&fs::read(&config_path).unwrap()),
        cfg_hash_before,
        "config.toml must remain unchanged after rejected vaults use"
    );

    // 2. Invalid OBSIDIAN_VAULT and invalid --vault must abort append with zero writes
    let bad_env_append = isolated_cli_base(tmp.path())
        .env("OBSIDIAN_VAULT", "NonExistentVaultXYZ")
        .args(["notes", "append", "NoteA", "SHOULD_NOT_APPEND"])
        .output()
        .unwrap();
    assert!(
        !bad_env_append.status.success(),
        "invalid OBSIDIAN_VAULT must fail, stdout={}",
        String::from_utf8_lossy(&bad_env_append.stdout)
    );

    let bad_flag_append = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args([
            "--vault",
            "NonExistentVaultXYZ",
            "notes",
            "append",
            "NoteA",
            "SHOULD_NOT_APPEND",
        ])
        .output()
        .unwrap();
    assert!(!bad_flag_append.status.success());

    assert_eq!(
        content_sha256_hex(&fs::read(&config_path).unwrap()),
        cfg_hash_before
    );
    assert_eq!(
        content_sha256_hex(&fs::read(vault_a.join("NoteA.md")).unwrap()),
        hash_a
    );
    assert_eq!(
        content_sha256_hex(&fs::read(vault_b.join("NoteB.md")).unwrap()),
        hash_b
    );

    // 3. Explicitly add B then use A -> use B; next process opens B
    let add_b = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["--json", "vaults", "add"])
        .arg(&vault_b)
        .output()
        .unwrap();
    assert!(
        add_b.status.success(),
        "{}",
        String::from_utf8_lossy(&add_b.stderr)
    );
    let add_b_json: Value = serde_json::from_slice(&add_b.stdout).unwrap();
    let vault_b_id = add_b_json["vault"]["id"].as_str().unwrap().to_string();

    let use_a = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "use", "VaultA"])
        .output()
        .unwrap();
    assert!(
        use_a.status.success(),
        "{}",
        String::from_utf8_lossy(&use_a.stderr)
    );

    let use_b = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "use"])
        .arg(&vault_b)
        .output()
        .unwrap();
    assert!(
        use_b.status.success(),
        "{}",
        String::from_utf8_lossy(&use_b.stderr)
    );

    let list_b = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["--json", "notes", "list"])
        .output()
        .unwrap();
    assert!(
        list_b.status.success(),
        "{}",
        String::from_utf8_lossy(&list_b.stderr)
    );
    let notes_b: Value = serde_json::from_slice(&list_b.stdout).unwrap();
    assert_eq!(notes_b["notes"].as_array().unwrap().len(), 1);
    assert_eq!(notes_b["notes"][0]["rel_path"], "NoteB.md");

    // 4. Same name + different path -> error; same name + same path -> idempotent
    let dup_name_diff_path = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "add"])
        .arg(&vault_c)
        .args(["--name", "VaultA"])
        .output()
        .unwrap();
    assert!(
        !dup_name_diff_path.status.success(),
        "registering duplicate name for different path must fail, stdout={}",
        String::from_utf8_lossy(&dup_name_diff_path.stdout)
    );
    let cfg_after_dup = AppConfig::load_from(&config_path).unwrap();
    assert_eq!(
        obsidian_cli::config::normalize_path_key(&cfg_after_dup.find_vault("VaultA").unwrap().path),
        obsidian_cli::config::normalize_path_key(&vault_a),
        "existing VaultA path must not be overwritten"
    );

    let dup_name_same_path = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "add"])
        .arg(&vault_a)
        .args(["--name", "VaultA"])
        .output()
        .unwrap();
    assert!(
        dup_name_same_path.status.success(),
        "idempotent re-add of same name + same path must succeed: {}",
        String::from_utf8_lossy(&dup_name_same_path.stderr)
    );
    let cfg_after_idem = AppConfig::load_from(&config_path).unwrap();
    assert_eq!(cfg_after_idem.vaults.len(), 2);

    // 5. Removing active vault by ID clears active_vault selection
    let use_b_again = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "use", "VaultB"])
        .output()
        .unwrap();
    assert!(use_b_again.status.success());
    let rm_b_by_id = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "remove", &vault_b_id])
        .output()
        .unwrap();
    assert!(rm_b_by_id.status.success());
    let cfg_after_rm_id = AppConfig::load_from(&config_path).unwrap();
    assert!(
        cfg_after_rm_id.active_vault.is_empty(),
        "removing active vault by ID must clear active_vault, got '{}'",
        cfg_after_rm_id.active_vault
    );

    // Re-add B for daemon conflict test
    let readd_b = isolated_cli_base(tmp.path())
        .env_remove("OBSIDIAN_VAULT")
        .args(["vaults", "add"])
        .arg(&vault_b)
        .output()
        .unwrap();
    assert!(readd_b.status.success());

    // 6. Daemon start for VaultA, idempotent same-vault start, error on different-vault start preserving running daemon
    let daemon_io = tmp.path().join("daemon_io");
    fs::create_dir_all(&daemon_io).unwrap();
    let _guard = DaemonStopGuard(tmp.path().to_path_buf());
    let (start_a_status, start_a_out, start_a_err) = run_cli_bounded(
        isolated_cli_base(tmp.path())
            .env_remove("OBSIDIAN_VAULT")
            .args(["--json", "--vault", "VaultA", "daemon", "start"]),
        &daemon_io,
        "start_a",
        Duration::from_secs(15),
    );
    assert!(
        start_a_status.success(),
        "daemon start VaultA failed: {}",
        String::from_utf8_lossy(&start_a_err)
    );
    let st_a: Value = serde_json::from_slice(&start_a_out).unwrap();
    assert_eq!(st_a["running"], true);
    assert_eq!(st_a["vault_name"], "VaultA");
    let pid_a = st_a["pid"].as_u64().unwrap();

    // Control: same vault daemon start returns existing running status
    let (start_a2_status, start_a2_out, start_a2_err) = run_cli_bounded(
        isolated_cli_base(tmp.path())
            .env_remove("OBSIDIAN_VAULT")
            .args(["--json", "--vault", "VaultA", "daemon", "start"]),
        &daemon_io,
        "start_a_again",
        Duration::from_secs(10),
    );
    assert!(
        start_a2_status.success(),
        "same-vault daemon start failed: {}",
        String::from_utf8_lossy(&start_a2_err)
    );
    let st_a2: Value = serde_json::from_slice(&start_a2_out).unwrap();
    assert_eq!(st_a2["pid"].as_u64(), Some(pid_a));
    assert_eq!(st_a2["vault_name"], "VaultA");

    // Conflict: different vault daemon start must fail naming both VaultA and VaultB and preserve VaultA daemon
    let (start_b_status, start_b_out, start_b_err_bytes) = run_cli_bounded(
        isolated_cli_base(tmp.path())
            .env_remove("OBSIDIAN_VAULT")
            .args(["--json", "--vault", "VaultB", "daemon", "start"]),
        &daemon_io,
        "start_b",
        Duration::from_secs(10),
    );
    assert!(
        !start_b_status.success(),
        "daemon start for VaultB while VaultA daemon runs must fail, stdout={}",
        String::from_utf8_lossy(&start_b_out)
    );
    let start_b_err = String::from_utf8_lossy(&start_b_err_bytes);
    assert!(
        start_b_err.contains("VaultA") && start_b_err.contains("VaultB"),
        "daemon conflict error must include both running and requested vault names, got: {start_b_err}"
    );

    let (status_after_ok, status_after_out, status_after_err) = run_cli_bounded(
        isolated_cli_base(tmp.path())
            .env_remove("OBSIDIAN_VAULT")
            .args(["--json", "daemon", "status"]),
        &daemon_io,
        "status_after",
        Duration::from_secs(10),
    );
    assert!(
        status_after_ok.success(),
        "daemon status failed: {}",
        String::from_utf8_lossy(&status_after_err)
    );
    let st_after: Value = serde_json::from_slice(&status_after_out).unwrap();
    assert_eq!(st_after["running"], true);
    assert_eq!(st_after["pid"].as_u64(), Some(pid_a));
    assert_eq!(st_after["vault_name"], "VaultA");

    let (stop_ok, _, stop_err) = run_cli_bounded(
        isolated_cli_base(tmp.path())
            .env_remove("OBSIDIAN_VAULT")
            .args(["daemon", "stop"]),
        &daemon_io,
        "stop_a",
        Duration::from_secs(10),
    );
    assert!(
        stop_ok.success(),
        "daemon stop failed: {}",
        String::from_utf8_lossy(&stop_err)
    );
}

#[test]
fn regression_obsidian_obs_02_source_relative_wikilink_index_matches_move() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs02Vault");
    fs::create_dir_all(root.join("Alpha")).unwrap();
    fs::create_dir_all(root.join("LongBetaFolder")).unwrap();
    fs::create_dir_all(root.join("Aliased")).unwrap();
    fs::create_dir_all(root.join("Sub")).unwrap();

    fs::write(root.join("Alpha/Spec.md"), "# Alpha Spec\n").unwrap();
    fs::write(root.join("LongBetaFolder/Spec.md"), "# Beta Spec\n").unwrap();
    fs::write(
        root.join("LongBetaFolder/Guide.md"),
        "# Guide\n\nSame folder: [[Spec]]\nDot relative: [[./Spec]]\nParent relative: [[../Alpha/Spec]]\n",
    )
    .unwrap();
    fs::write(
        root.join("Aliased/Target.md"),
        "---\naliases: [UniqueAlias]\n---\n# Target\n",
    )
    .unwrap();
    fs::write(
        root.join("RootAmbiguous.md"),
        "# Root\n\nAmbiguous bare: [[Spec]]\nFull path: [[LongBetaFolder/Spec]]\nAlias: [[UniqueAlias]]\n",
    )
    .unwrap();
    fs::write(root.join("Sub/UniqueRelTarget.md"), "# UniqueRelTarget\n").unwrap();
    fs::write(
        root.join("Sub/UniqueRelSource.md"),
        "# UniqueRelSource\n\nLink: [[./UniqueRelTarget]]\n",
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("Obs02Vault"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    // 1. Guide.md outgoing links resolve source-relatively: Beta, Beta, Alpha
    let guide_links = storage
        .get_outgoing_links(&vault, "LongBetaFolder/Guide.md")
        .unwrap();
    let resolved_for = |raw: &str| {
        guide_links
            .iter()
            .find(|l| l.target_raw == raw)
            .and_then(|l| l.target_resolved_path.as_deref())
    };
    assert_eq!(
        resolved_for("Spec"),
        Some("LongBetaFolder/Spec.md"),
        "[[Spec]] in LongBetaFolder/Guide.md must resolve to LongBetaFolder/Spec.md, got {guide_links:?}"
    );
    assert_eq!(
        resolved_for("./Spec"),
        Some("LongBetaFolder/Spec.md"),
        "[[./Spec]] in LongBetaFolder/Guide.md must resolve to LongBetaFolder/Spec.md, got {guide_links:?}"
    );
    assert_eq!(
        resolved_for("../Alpha/Spec"),
        Some("Alpha/Spec.md"),
        "[[../Alpha/Spec]] in LongBetaFolder/Guide.md must resolve to Alpha/Spec.md, got {guide_links:?}"
    );

    // 2. Control: RootAmbiguous.md leaves ambiguous [[Spec]] unresolved while resolving full path & unique alias
    let root_links = storage
        .get_outgoing_links(&vault, "RootAmbiguous.md")
        .unwrap();
    assert_eq!(
        root_links
            .iter()
            .find(|l| l.target_raw == "Spec")
            .and_then(|l| l.target_resolved_path.as_deref()),
        None,
        "ambiguous bare [[Spec]] from root must remain unresolved, got {root_links:?}"
    );
    assert_eq!(
        root_links
            .iter()
            .find(|l| l.target_raw == "LongBetaFolder/Spec")
            .and_then(|l| l.target_resolved_path.as_deref()),
        Some("LongBetaFolder/Spec.md")
    );
    assert_eq!(
        root_links
            .iter()
            .find(|l| l.target_raw == "UniqueAlias")
            .and_then(|l| l.target_resolved_path.as_deref()),
        Some("Aliased/Target.md")
    );

    // 3. Backlinks & Graph agree with outgoing link resolution
    let beta_bl = storage
        .get_backlinks(&vault.id, "LongBetaFolder/Spec.md")
        .unwrap();
    assert!(beta_bl
        .iter()
        .any(|b| b.source_path == "LongBetaFolder/Guide.md"));
    let alpha_bl = storage.get_backlinks(&vault.id, "Alpha/Spec.md").unwrap();
    assert!(alpha_bl
        .iter()
        .any(|b| b.source_path == "LongBetaFolder/Guide.md"));
    assert!(
        !alpha_bl.iter().any(|b| b.source_path == "RootAmbiguous.md"),
        "ambiguous bare link in RootAmbiguous.md must not create backlink to Alpha/Spec.md"
    );

    let graph = storage
        .get_graph(&vault.id, Some("LongBetaFolder/Guide.md"), 1)
        .unwrap();
    assert!(graph
        .edges
        .iter()
        .any(|e| e.source == "LongBetaFolder/Guide.md" && e.target == "LongBetaFolder/Spec.md"));
    assert!(graph
        .edges
        .iter()
        .any(|e| e.source == "LongBetaFolder/Guide.md" && e.target == "Alpha/Spec.md"));

    // 4. Unique relative link target is excluded from orphans
    let orphans = storage.find_orphans(&vault.id).unwrap();
    assert!(
        !orphans
            .iter()
            .any(|o| o.rel_path == "Sub/UniqueRelTarget.md"),
        "Sub/UniqueRelTarget.md linked via [[./UniqueRelTarget]] must not be orphan: {orphans:?}"
    );

    // 5. Incremental sync (upsert_single_note / sync_paths) passes source_rel
    fs::write(root.join("Alpha/Ref.md"), "# Ref\n\n[[Spec]]\n").unwrap();
    storage
        .sync_paths(&vault, &["Alpha/Ref.md".to_string()], &[])
        .unwrap();
    let alpha_ref_links = storage.get_outgoing_links(&vault, "Alpha/Ref.md").unwrap();
    assert_eq!(
        alpha_ref_links[0].target_resolved_path.as_deref(),
        Some("Alpha/Spec.md"),
        "incremental sync_paths must resolve [[Spec]] in Alpha/Ref.md to Alpha/Spec.md"
    );

    // 6. Move LongBetaFolder/Spec.md -> LongBetaFolder/BetaSpec.md matches indexed targets and leaves ambiguous bare link verbatim
    let moved = storage
        .move_note_with_links(
            &vault,
            "LongBetaFolder/Spec.md",
            "LongBetaFolder/BetaSpec.md",
        )
        .unwrap();
    assert_eq!(moved.to, "LongBetaFolder/BetaSpec.md");

    let guide_after = fs::read_to_string(root.join("LongBetaFolder/Guide.md")).unwrap();
    assert!(
        guide_after.contains("Same folder: [[BetaSpec]]"),
        "{guide_after}"
    );
    assert!(
        guide_after.contains("Dot relative: [[BetaSpec]]"),
        "{guide_after}"
    );
    assert!(
        guide_after.contains("Parent relative: [[../Alpha/Spec]]"),
        "{guide_after}"
    );

    let root_after = fs::read_to_string(root.join("RootAmbiguous.md")).unwrap();
    assert!(
        root_after.contains("Ambiguous bare: [[Spec]]"),
        "ambiguous bare link must stay verbatim on move: {root_after}"
    );
    assert!(
        root_after.contains("Full path: [[BetaSpec]]"),
        "unambiguous full path link must update on move: {root_after}"
    );
}

#[test]
fn regression_obsidian_obs_05_formatting_table_inline_code_and_link_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs05Vault");
    fs::create_dir_all(&root).unwrap();

    fs::write(root.join("Dest.md"), "# Dest\n\n## Section\n\nBody.\n").unwrap();
    fs::write(
        root.join("BoldNote.md"),
        "# BoldNote\n\n**[[Dest]]** and **#boldtag**\n",
    )
    .unwrap();
    fs::write(
        root.join("ItalicNote.md"),
        "# ItalicNote\n\n*[[Dest]]* and *#italictag*\n",
    )
    .unwrap();
    fs::write(
        root.join("HighlightNote.md"),
        "# HighlightNote\n\n==[[Dest]]== and ==#hltag==\n",
    )
    .unwrap();
    fs::write(
        root.join("TableHeaderNote.md"),
        "# TableHeader\n\n| [[Dest]] #hdrtag | Value |\n| --- | --- |\n| [[Dest\\|TableAlias]] #celltag | ok |\n",
    )
    .unwrap();
    fs::write(
        root.join("InlineTripleTick.md"),
        "# InlineTripleTick\n\n```inline code```\nAfter tick [[Dest]] #aftertick\n",
    )
    .unwrap();
    fs::write(
        root.join("HeadingSelfLink.md"),
        "# HeadingSelfLink\n\nSee [[#Section]] and [[#Nested/Heading]].\n",
    )
    .unwrap();
    fs::write(
        root.join("ParenUrlNote.md"),
        "# ParenUrl\n\nRead [Wiki](https://en.wikipedia.org/wiki/Rust_(programming_language)).\n",
    )
    .unwrap();
    fs::write(
        root.join("UnclosedBracketNote.md"),
        "# Bracket\n\n[note] see [RealLink](https://example.com/path) and [unclosed see [Second](https://example.com/two)\n",
    )
    .unwrap();
    fs::write(
        root.join("ControlFenceCode.md"),
        "# Control\n\n```md\n[[Dest]] #fencetag\n```\n\nInline `[[Dest]] #codetag` and ``[[Dest]] #dblcodetag``.\n\nPlain [[Dest|PlainAlias]] #plaintag\n",
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("Obs05Vault"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    // 1. Emphasis / italic / highlight contain wikilinks and tags exactly once
    for (note, tag) in [
        ("BoldNote.md", "boldtag"),
        ("ItalicNote.md", "italictag"),
        ("HighlightNote.md", "hltag"),
    ] {
        let links = storage.get_outgoing_links(&vault, note).unwrap();
        assert_eq!(
            links.len(),
            1,
            "{note} should index [[Dest]] once, got {links:?}"
        );
        assert_eq!(links[0].target_resolved_path.as_deref(), Some("Dest.md"));
        let summary = storage.get_note_summary(&vault.id, note).unwrap().unwrap();
        assert_eq!(
            summary.tags,
            vec![tag.to_string()],
            "{note} should extract #{tag}, got {:?}",
            summary.tags
        );
    }

    // 2. Table header links & tags + escaped pipe alias in table cell
    let tbl_links = storage
        .get_outgoing_links(&vault, "TableHeaderNote.md")
        .unwrap();
    assert_eq!(
        tbl_links.len(),
        2,
        "TableHeaderNote.md should extract header + cell links once each, got {tbl_links:?}"
    );
    assert!(tbl_links
        .iter()
        .all(|l| l.target_resolved_path.as_deref() == Some("Dest.md")));
    assert!(
        tbl_links
            .iter()
            .any(|l| l.target_raw == "Dest" && l.alias.as_deref() == Some("TableAlias")),
        "escaped pipe [[Dest\\|TableAlias]] must parse target=Dest alias=TableAlias, got {tbl_links:?}"
    );
    let tbl_summary = storage
        .get_note_summary(&vault.id, "TableHeaderNote.md")
        .unwrap()
        .unwrap();
    assert!(
        tbl_summary.tags.contains(&"hdrtag".to_string()),
        "{:?}",
        tbl_summary.tags
    );
    assert!(
        tbl_summary.tags.contains(&"celltag".to_string()),
        "{:?}",
        tbl_summary.tags
    );

    // 3. Same-line closed triple-backtick inline code must NOT swallow subsequent lines
    let tick_links = storage
        .get_outgoing_links(&vault, "InlineTripleTick.md")
        .unwrap();
    assert_eq!(
        tick_links.len(),
        1,
        "line after ```inline code``` must be parsed, got {tick_links:?}"
    );
    assert_eq!(
        tick_links[0].target_resolved_path.as_deref(),
        Some("Dest.md")
    );
    let tick_summary = storage
        .get_note_summary(&vault.id, "InlineTripleTick.md")
        .unwrap()
        .unwrap();
    assert_eq!(tick_summary.tags, vec!["aftertick".to_string()]);

    // 4. [[#Section]] must NOT create a false `section` tag
    let head_summary = storage
        .get_note_summary(&vault.id, "HeadingSelfLink.md")
        .unwrap()
        .unwrap();
    assert!(
        head_summary.tags.is_empty(),
        "[[#Section]] must produce 0 tags, got {:?}",
        head_summary.tags
    );

    // 5. Balanced parens in Markdown link URL
    let paren_detail = storage.load_note_detail(&vault, "ParenUrlNote.md").unwrap();
    assert_eq!(paren_detail.parsed.md_links.len(), 1);
    assert_eq!(
        paren_detail.parsed.md_links[0].url,
        "https://en.wikipedia.org/wiki/Rust_(programming_language)"
    );

    // 6. Preceding [note] or unclosed `[` must not swallow independent [RealLink](...)
    let bracket_detail = storage
        .load_note_detail(&vault, "UnclosedBracketNote.md")
        .unwrap();
    assert_eq!(
        bracket_detail.parsed.md_links.len(),
        2,
        "got {:?}",
        bracket_detail.parsed.md_links
    );
    assert_eq!(bracket_detail.parsed.md_links[0].label, "RealLink");
    assert_eq!(
        bracket_detail.parsed.md_links[0].url,
        "https://example.com/path"
    );
    assert_eq!(bracket_detail.parsed.md_links[1].label, "Second");
    assert_eq!(
        bracket_detail.parsed.md_links[1].url,
        "https://example.com/two"
    );

    // 7. Control: real code fence & inline code stay excluded; move rewrites same-line tick note & table escaped alias cleanly
    let ctl_links = storage
        .get_outgoing_links(&vault, "ControlFenceCode.md")
        .unwrap();
    assert_eq!(
        ctl_links.len(),
        1,
        "fence/inline code must be excluded: {ctl_links:?}"
    );
    assert_eq!(ctl_links[0].alias.as_deref(), Some("PlainAlias"));
    let ctl_summary = storage
        .get_note_summary(&vault.id, "ControlFenceCode.md")
        .unwrap()
        .unwrap();
    assert_eq!(ctl_summary.tags, vec!["plaintag".to_string()]);

    // Move Dest.md -> DestMoved.md: InlineTripleTick.md & TableHeaderNote.md & emphasis notes update and reindex identically
    let moved = storage
        .move_note_with_links(&vault, "Dest.md", "DestMoved.md")
        .unwrap();
    assert_eq!(moved.to, "DestMoved.md");
    let tick_after = fs::read_to_string(root.join("InlineTripleTick.md")).unwrap();
    assert!(
        tick_after.contains("After tick [[DestMoved]]"),
        "move must also treat ```inline code``` as inline code rather than multiline fence: {tick_after}"
    );
    let ctl_after = fs::read_to_string(root.join("ControlFenceCode.md")).unwrap();
    assert!(ctl_after.contains("```md\n[[Dest]] #fencetag\n```"));
    assert!(ctl_after.contains("Inline `[[Dest]] #codetag` and ``[[Dest]] #dblcodetag``."));
    assert!(ctl_after.contains("Plain [[DestMoved|PlainAlias]]"));
    let bl_after = storage.get_backlinks(&vault.id, "DestMoved.md").unwrap();
    for src in [
        "BoldNote.md",
        "ItalicNote.md",
        "HighlightNote.md",
        "TableHeaderNote.md",
        "InlineTripleTick.md",
        "ControlFenceCode.md",
    ] {
        assert!(
            bl_after.iter().any(|b| b.source_path == src),
            "after move, {src} must remain backlink of DestMoved.md: {bl_after:?}"
        );
    }
}

#[test]
fn regression_obsidian_obs_06_local_markdown_note_links_in_graph() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs06Vault");
    fs::create_dir_all(root.join("Projects")).unwrap();
    fs::create_dir_all(root.join("Attachments")).unwrap();
    fs::create_dir_all(root.join("Sub")).unwrap();

    fs::write(root.join("Attachments/pic.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    fs::write(
        root.join("Projects/Plan.md"),
        "# Plan\n\n## Goals\n\nDeliver local markdown links.\n",
    )
    .unwrap();
    fs::write(root.join("Projects/Space Note.md"), "# Space Note\n").unwrap();
    fs::write(
        root.join("R.md"),
        "# Referrer\n\nSee [The Plan](Projects/Plan.md#Goals \"Plan Title\"), [Spaced](Projects/Space%20Note.md), [Ext](https://example.com/Projects/Plan.md), and ![img](Attachments/pic.png).\n",
    )
    .unwrap();
    fs::write(
        root.join("OrphanOnlyExt.md"),
        "# External Only\n\n[Site](https://example.com) and ![img](Attachments/pic.png)\n",
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("Obs06Vault"));
    let db_path = tmp.path().join("obs06.db");
    let storage = Storage::open(&db_path).unwrap();
    storage.sync_vault(&vault).unwrap();

    // 1. Outgoing links from R.md reflect only the 2 local markdown note targets (no HTTPS/image edges)
    let r_links = storage.get_outgoing_links(&vault, "R.md").unwrap();
    assert_eq!(
        r_links.len(),
        2,
        "R.md should have exactly 2 local note links (Plan + Space Note), got {r_links:?}"
    );
    assert!(
        r_links.iter().any(
            |l| l.target_resolved_path.as_deref() == Some("Projects/Plan.md")
                && l.heading.as_deref() == Some("Goals")
        ),
        "expected Projects/Plan.md#Goals link, got {r_links:?}"
    );
    assert!(
        r_links
            .iter()
            .any(|l| l.target_resolved_path.as_deref() == Some("Projects/Space Note.md")),
        "expected Projects/Space Note.md link, got {r_links:?}"
    );

    // 2. Backlinks, graph, and orphans agree; external HTTPS + image-only note stays orphan
    let plan_bl = storage
        .get_backlinks(&vault.id, "Projects/Plan.md")
        .unwrap();
    assert!(
        plan_bl.iter().any(|b| b.source_path == "R.md"),
        "Projects/Plan.md must have R.md as backlink: {plan_bl:?}"
    );
    let graph = storage.get_graph(&vault.id, Some("R.md"), 1).unwrap();
    assert!(graph
        .edges
        .iter()
        .any(|e| e.source == "R.md" && e.target == "Projects/Plan.md"));
    assert!(graph
        .edges
        .iter()
        .any(|e| e.source == "R.md" && e.target == "Projects/Space Note.md"));
    assert!(
        !graph
            .edges
            .iter()
            .any(|e| e.target.contains("https://") || e.target.contains("pic.png")),
        "external URLs and images must not be note graph edges: {:?}",
        graph.edges
    );

    let orphans = storage.find_orphans(&vault.id).unwrap();
    assert!(!orphans.iter().any(|o| o.rel_path == "R.md"), "{orphans:?}");
    assert!(
        !orphans.iter().any(|o| o.rel_path == "Projects/Plan.md"),
        "{orphans:?}"
    );
    assert!(
        orphans.iter().any(|o| o.rel_path == "OrphanOnlyExt.md"),
        "note with only HTTPS link and image must remain orphan: {orphans:?}"
    );

    // 3. Upgrade from old user_version=1 DB (without markdown link rows) automatically reindexes once
    drop(storage);
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DELETE FROM links WHERE vault_id = ?1", [&vault.id])
            .unwrap();
        conn.pragma_update(None, "user_version", 1i64).unwrap();
    }
    let upgraded = Storage::open(&db_path).unwrap();
    let up_stats = upgraded.sync_vault(&vault).unwrap();
    assert!(
        up_stats.updated >= 1,
        "v1 DB must trigger one-time reindex on upgrade"
    );
    assert!(upgraded
        .get_backlinks(&vault.id, "Projects/Plan.md")
        .unwrap()
        .iter()
        .any(|b| b.source_path == "R.md"));

    // 4. Incremental sync_paths computes local markdown links identically
    fs::write(
        root.join("Sub/IncRef.md"),
        "# IncRef\n\nRelative [Plan](../Projects/Plan.md#Goals).\n",
    )
    .unwrap();
    upgraded
        .sync_paths(&vault, &["Sub/IncRef.md".to_string()], &[])
        .unwrap();
    assert!(upgraded
        .get_backlinks(&vault.id, "Projects/Plan.md")
        .unwrap()
        .iter()
        .any(|b| b.source_path == "Sub/IncRef.md"));

    // 5. Move target -> preserves label/title/anchor and only new path remains indexed
    let moved = upgraded
        .move_note_with_links(&vault, "Projects/Plan.md", "Archive/New Plan.md")
        .unwrap();
    assert_eq!(moved.to, "Archive/New Plan.md");
    let r_after = fs::read_to_string(root.join("R.md")).unwrap();
    assert!(
        r_after.contains("[The Plan](Archive/New%20Plan.md#Goals \"Plan Title\")"),
        "move must preserve label, anchor, and title: {r_after}"
    );
    assert!(upgraded
        .get_backlinks(&vault.id, "Projects/Plan.md")
        .unwrap()
        .is_empty());
    let new_bl = upgraded
        .get_backlinks(&vault.id, "Archive/New Plan.md")
        .unwrap();
    assert!(new_bl.iter().any(|b| b.source_path == "R.md"));
    assert!(new_bl.iter().any(|b| b.source_path == "Sub/IncRef.md"));
}

#[test]
fn regression_obsidian_obs_08_windows_case_only_rename() {
    use obsidian_cli::vault::fs::content_sha256_hex;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs08Vault");
    fs::create_dir_all(root.join("notes")).unwrap();

    let plan_bytes = b"# plan\n\nOriginal exact bytes.\r\n";
    let existing_other_bytes = b"# Existing\n\nDo not clobber.\n";
    fs::write(root.join("notes/plan.md"), plan_bytes).unwrap();
    fs::write(root.join("notes/Existing.md"), existing_other_bytes).unwrap();
    fs::write(
        root.join("Index.md"),
        "# Index\n\nSee [[notes/plan]] and [md](notes/plan.md).\n",
    )
    .unwrap();
    let orig_hash = content_sha256_hex(plan_bytes);
    let other_hash = content_sha256_hex(existing_other_bytes);

    // 1. Case-only rename notes/plan.md -> notes/Plan.md via CLI
    let mv = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "move", "notes/plan.md", "notes/Plan.md"])
        .output()
        .unwrap();
    assert!(
        mv.status.success(),
        "case-only move notes/plan.md -> notes/Plan.md failed: {}",
        String::from_utf8_lossy(&mv.stderr)
    );
    let mv_json: Value = serde_json::from_slice(&mv.stdout).unwrap();
    assert_eq!(mv_json["from"], "notes/plan.md");
    assert_eq!(mv_json["to"], "notes/Plan.md");

    // Enumerate actual directory entries on disk to verify exact on-disk casing
    let mut disk_names: Vec<String> = fs::read_dir(root.join("notes"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    disk_names.sort();
    assert_eq!(
        disk_names,
        vec!["Existing.md".to_string(), "Plan.md".to_string()],
        "on-disk directory entries must show exact Plan.md casing, got {disk_names:?}"
    );
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("notes/Plan.md")).unwrap()),
        orig_hash
    );

    // CLI notes list & SQLite show exactly one note with new casing, and incoming links point to notes/Plan.md
    let list_out = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "list"])
        .output()
        .unwrap();
    assert!(list_out.status.success());
    let list_json: Value = serde_json::from_slice(&list_out.stdout).unwrap();
    let plan_entries: Vec<&str> = list_json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["rel_path"].as_str())
        .filter(|p| p.eq_ignore_ascii_case("notes/plan.md"))
        .collect();
    assert_eq!(
        plan_entries,
        vec!["notes/Plan.md"],
        "must not duplicate note under old and new casing"
    );

    let index_after = fs::read_to_string(root.join("Index.md")).unwrap();
    assert!(
        index_after.contains("notes/Plan") || index_after.contains("[[Plan]]"),
        "Index.md should reference updated casing or valid link: {index_after}"
    );
    let bl_out = isolated_cli(tmp.path(), &root)
        .args(["--json", "backlinks", "notes/Plan.md"])
        .output()
        .unwrap();
    assert!(
        bl_out.status.success(),
        "{}",
        String::from_utf8_lossy(&bl_out.stderr)
    );
    let bl_json: Value = serde_json::from_slice(&bl_out.stdout).unwrap();
    assert!(
        bl_json
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["source_path"] == "Index.md"),
        "Index.md must remain a backlink to notes/Plan.md: {bl_json}"
    );

    // 2. Control: exact same path move retains existing conflict/error behavior
    let same_path = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "move", "notes/Plan.md", "notes/Plan.md"])
        .output()
        .unwrap();
    assert!(
        !same_path.status.success(),
        "identical path move must retain existing already-exists error"
    );
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("notes/Plan.md")).unwrap()),
        orig_hash
    );

    // 3. Control: moving onto a DIFFERENT existing file fails and preserves both files
    let collide = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "notes",
            "move",
            "notes/Plan.md",
            "notes/Existing.md",
        ])
        .output()
        .unwrap();
    assert!(!collide.status.success());
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("notes/Plan.md")).unwrap()),
        orig_hash
    );
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("notes/Existing.md")).unwrap()),
        other_hash
    );

    // 4. Control: normal move to new folder updates links cleanly
    let normal_mv = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "notes",
            "move",
            "notes/Plan.md",
            "Archive/FinalPlan.md",
        ])
        .output()
        .unwrap();
    assert!(
        normal_mv.status.success(),
        "{}",
        String::from_utf8_lossy(&normal_mv.stderr)
    );
    assert!(root.join("Archive/FinalPlan.md").is_file());
    assert_eq!(
        content_sha256_hex(&fs::read(root.join("Archive/FinalPlan.md")).unwrap()),
        orig_hash
    );
}

#[test]
fn regression_obsidian_obs_10_surgical_task_toggle_eol_fence_and_bom() {
    use obsidian_cli::vault::fs::toggle_task_at_line_verified;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs10Vault");
    fs::create_dir_all(&root).unwrap();

    // 1. Mixed CRLF/LF note: toggling task changes ONLY the single checkbox byte (' ' <-> 'x')
    let mixed_path = root.join("Mixed.md");
    let mixed_before = b"# Mixed\r\n\n- [ ] Toggle target\n- [x] Keep LF\r\nTail";
    fs::write(&mixed_path, mixed_before).unwrap();
    let checked = toggle_task_at_line(&mixed_path, 3).unwrap();
    assert!(checked);
    let mixed_after = fs::read(&mixed_path).unwrap();
    assert_eq!(mixed_after.len(), mixed_before.len());
    let diffs: Vec<(usize, u8, u8)> = mixed_before
        .iter()
        .zip(mixed_after.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(idx, (&a, &b))| (idx, a, b))
        .collect();
    assert_eq!(
        diffs.len(),
        1,
        "mixed CRLF/LF toggle must change exactly 1 byte, diffs={diffs:?}"
    );
    assert_eq!((diffs[0].1, diffs[0].2), (b' ', b'x'));
    // Toggle back x -> ' '
    assert!(!toggle_task_at_line(&mixed_path, 3).unwrap());
    assert_eq!(fs::read(&mixed_path).unwrap(), mixed_before);

    // 2. Task inside code fence must error with 0 disk writes
    let fence_path = root.join("CodeTask.md");
    let fence_before = b"# CodeTask\n\n```md\n- [ ] Inside fence\n```\n\n- [ ] Real task\n";
    fs::write(&fence_path, fence_before).unwrap();
    assert!(
        toggle_task_at_line(&fence_path, 4).is_err(),
        "toggling - [ ] inside a fenced code block must fail"
    );
    assert_eq!(
        fs::read(&fence_path).unwrap(),
        fence_before,
        "failed toggle inside fence must leave bytes untouched"
    );
    assert!(toggle_task_at_line(&fence_path, 7).unwrap());
    assert_eq!(
        fs::read(&fence_path).unwrap(),
        b"# CodeTask\n\n```md\n- [ ] Inside fence\n```\n\n- [x] Real task\n"
    );

    // 3. UTF-8 BOM on line 1 (heading AND first-line task without frontmatter)
    let bom_heading_path = root.join("BomHeading.md");
    let bom_heading_bytes = "\u{feff}# BOM Heading\n- [ ] Second line task\n".as_bytes();
    fs::write(&bom_heading_path, bom_heading_bytes).unwrap();

    let bom_task_path = root.join("BomFirstLineTask.md");
    let bom_task_bytes = "\u{feff}- [ ] First line BOM task\n- [ ] Second task".as_bytes();
    fs::write(&bom_task_path, bom_task_bytes).unwrap();

    let vault = vault_from_dir(&root, Some("Obs10Vault"));
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let bom_h_detail = storage.load_note_detail(&vault, "BomHeading.md").unwrap();
    assert_eq!(
        bom_h_detail.parsed.headings.len(),
        1,
        "BOM line-1 heading must be indexed: {:?}",
        bom_h_detail.parsed.headings
    );
    assert_eq!(bom_h_detail.parsed.headings[0].text, "BOM Heading");
    assert_eq!(bom_h_detail.parsed.headings[0].line_no, 1);

    let bom_t_tasks = storage
        .list_tasks(
            &vault.id,
            obsidian_cli::models::TaskFilter::All,
            Some("BomFirstLineTask.md"),
        )
        .unwrap();
    assert_eq!(
        bom_t_tasks.len(),
        2,
        "BOM line-1 task must be indexed: {bom_t_tasks:?}"
    );
    assert_eq!(bom_t_tasks[0].line_no, 1);
    assert_eq!(bom_t_tasks[0].text, "First line BOM task");

    // Toggle BOM line-1 task -> preserves leading UTF-8 BOM and no-trailing-newline byte-for-byte
    assert!(toggle_task_at_line(&bom_task_path, 1).unwrap());
    let bom_task_after = fs::read(&bom_task_path).unwrap();
    assert_eq!(
        bom_task_after,
        "\u{feff}- [x] First line BOM task\n- [ ] Second task".as_bytes(),
        "BOM and non-checkbox bytes must be preserved exactly"
    );

    // 4. Controls: pure CRLF, BOM+frontmatter, no trailing newline, expected-text mismatch
    let crlf_path = root.join("PureCrlf.md");
    let crlf_before = b"# CRLF\r\n- [ ] One\r\n- [ ] Two";
    fs::write(&crlf_path, crlf_before).unwrap();
    assert!(toggle_task_at_line(&crlf_path, 3).unwrap());
    assert_eq!(
        fs::read(&crlf_path).unwrap(),
        b"# CRLF\r\n- [ ] One\r\n- [x] Two"
    );

    let bom_fm_path = root.join("BomFM.md");
    let bom_fm_before =
        "\u{feff}---\r\ntags: [t]\r\n---\r\n\r\n# Title\r\n- [ ] After FM".as_bytes();
    fs::write(&bom_fm_path, bom_fm_before).unwrap();
    assert!(toggle_task_at_line(&bom_fm_path, 6).unwrap());
    assert_eq!(
        fs::read(&bom_fm_path).unwrap(),
        "\u{feff}---\r\ntags: [t]\r\n---\r\n\r\n# Title\r\n- [x] After FM".as_bytes()
    );

    // Expected-text mismatch must fail with 0 byte changes
    let before_mismatch = fs::read(&bom_fm_path).unwrap();
    assert!(toggle_task_at_line_verified(&bom_fm_path, 6, Some("Wrong text"), None).is_err());
    assert_eq!(fs::read(&bom_fm_path).unwrap(), before_mismatch);
}

#[tokio::test]
async fn regression_obsidian_obs_12_http_get_append_note_and_literal_percent_filename() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs12Vault");
    fs::create_dir_all(root.join("Sub Folder")).unwrap();

    fs::write(root.join("append.md"), "# append\n\nAppend note body.\n").unwrap();
    fs::write(
        root.join("Ratio 50%20Test.md"),
        "# Ratio 50%20Test\n\nLiteral percent body.\n",
    )
    .unwrap();
    fs::write(
        root.join("Sub Folder/C++ 한글 노트.md"),
        "# C++ 한글 노트\n\nNormal encoded path body.\n",
    )
    .unwrap();

    let vault = vault_from_dir(&root, Some("Obs12Vault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();

    let state = Arc::new(ServeState {
        cfg,
        default_vault: vault.clone(),
        storage,
        token: None,
    });
    let app = build_router(state).layer(CorsLayer::permissive());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // 1. GET /api/v1/notes/append and /api/v1/notes/append.md return the same note
    let r_bare = client
        .get(format!("{base}/api/v1/notes/append"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r_bare.status(),
        StatusCode::OK,
        "GET /api/v1/notes/append must return 200 OK"
    );
    let bare_json: Value = r_bare.json().await.unwrap();
    let r_ext = client
        .get(format!("{base}/api/v1/notes/append.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(r_ext.status(), StatusCode::OK);
    let ext_json: Value = r_ext.json().await.unwrap();
    assert_eq!(bare_json["note"]["summary"]["rel_path"], "append.md");
    assert_eq!(ext_json["note"]["summary"]["rel_path"], "append.md");
    assert_eq!(
        bare_json["note"]["raw_markdown"],
        ext_json["note"]["raw_markdown"]
    );

    // 2. Literal %20 in filename (`Ratio 50%20Test.md`) requested with % -> %25 returns exact file
    let r_pct = client
        .get(format!("{base}/api/v1/notes/Ratio%2050%2520Test.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r_pct.status(),
        StatusCode::OK,
        "GET literal percent filename must not double-decode"
    );
    let pct_json: Value = r_pct.json().await.unwrap();
    assert_eq!(
        pct_json["note"]["summary"]["rel_path"],
        "Ratio 50%20Test.md"
    );
    assert!(pct_json["note"]["raw_markdown"]
        .as_str()
        .unwrap()
        .contains("Literal percent body."));

    // 3. Control: POST /api/v1/notes/append still appends exactly once
    let post_ap = client
        .post(format!("{base}/api/v1/notes/append"))
        .json(&json!({ "note": "append", "text": "APPENDED_ONCE_MARKER" }))
        .send()
        .await
        .unwrap();
    assert_eq!(post_ap.status(), StatusCode::OK);
    let on_disk = fs::read_to_string(root.join("append.md")).unwrap();
    assert_eq!(on_disk.matches("APPENDED_ONCE_MARKER").count(), 1);

    // 4. Control: spaces, '+', Korean, subfolder normal GET still works
    let r_sub = client
        .get(format!(
            "{base}/api/v1/notes/Sub%20Folder/C++%20%ED%95%9C%EA%B8%80%20%EB%85%B8%ED%8A%B8.md"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r_sub.status(), StatusCode::OK);
    let sub_json: Value = r_sub.json().await.unwrap();
    assert_eq!(
        sub_json["note"]["summary"]["rel_path"],
        "Sub Folder/C++ 한글 노트.md"
    );

    server.abort();
}

#[test]
fn regression_obsidian_obs_14_hyphen_titles_and_toggle_task_expected_text() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("Obs14Vault");
    fs::create_dir_all(&root).unwrap();

    // 1. Create `- 목록 항목`, then read, append, replace, move, and delete with the same spelling
    let title = "- 목록 항목";
    let cr = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "create", title])
        .output()
        .unwrap();
    assert!(
        cr.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&cr.stderr)
    );
    assert!(root.join("- 목록 항목.md").is_file());

    let rd = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "read", title])
        .output()
        .unwrap();
    assert!(
        rd.status.success(),
        "read failed: {}",
        String::from_utf8_lossy(&rd.stderr)
    );
    let rd_json: Value = serde_json::from_slice(&rd.stdout).unwrap();
    assert_eq!(rd_json["rel_path"], "- 목록 항목.md");

    let rd_compact = isolated_cli(tmp.path(), &root)
        .args(["notes", "read", title, "--compact", "--json"])
        .output()
        .unwrap();
    assert!(
        rd_compact.status.success(),
        "read --compact --json failed: {}",
        String::from_utf8_lossy(&rd_compact.stderr)
    );
    let rd_compact_json: Value = serde_json::from_slice(&rd_compact.stdout).unwrap();
    assert_eq!(rd_compact_json["rel_path"], "- 목록 항목.md");

    let ap = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "append", title, "- appended bullet"])
        .output()
        .unwrap();
    assert!(
        ap.status.success(),
        "append failed: {}",
        String::from_utf8_lossy(&ap.stderr)
    );
    assert!(fs::read_to_string(root.join("- 목록 항목.md"))
        .unwrap()
        .contains("- appended bullet"));

    let rep = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "notes",
            "replace",
            title,
            "--find",
            "- appended bullet",
            "--replace",
            "- updated bullet",
        ])
        .output()
        .unwrap();
    assert!(
        rep.status.success(),
        "replace failed: {}",
        String::from_utf8_lossy(&rep.stderr)
    );
    assert!(fs::read_to_string(root.join("- 목록 항목.md"))
        .unwrap()
        .contains("- updated bullet"));

    let moved_title = "- 이동된 항목";
    let mv = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "move", title, moved_title])
        .output()
        .unwrap();
    assert!(
        mv.status.success(),
        "move failed: {}",
        String::from_utf8_lossy(&mv.stderr)
    );
    assert!(root.join("- 이동된 항목.md").is_file());

    // Control: `--` separator still works for hyphen note identifier
    let rd_sep = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "read", "--", moved_title])
        .output()
        .unwrap();
    assert!(
        rd_sep.status.success(),
        "read after -- failed: {}",
        String::from_utf8_lossy(&rd_sep.stderr)
    );

    let del = isolated_cli(tmp.path(), &root)
        .args(["--json", "notes", "delete", "--yes", moved_title])
        .output()
        .unwrap();
    assert!(
        del.status.success(),
        "delete failed: {}",
        String::from_utf8_lossy(&del.stderr)
    );
    assert!(!root.join("- 이동된 항목.md").exists());
    assert!(root.join(".trash/- 이동된 항목.md").is_file());

    // 2. toggle-task --expected-text with separate argv `- item` and control `--expected-text=- item`
    fs::write(
        root.join("Tasks.md"),
        "# Tasks\n\n- [ ] - item\n- [ ] other task\n",
    )
    .unwrap();
    let tog_ok = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "toggle-task",
            "Tasks.md",
            "3",
            "--expected-text",
            "- item",
        ])
        .output()
        .unwrap();
    assert!(
        tog_ok.status.success(),
        "toggle-task --expected-text '- item' failed: {}",
        String::from_utf8_lossy(&tog_ok.stderr)
    );
    assert!(fs::read_to_string(root.join("Tasks.md"))
        .unwrap()
        .contains("- [x] - item"));

    // Mismatched expected-text must run comparison and fail with Task conflict (not clap exit 2)
    let tog_mismatch = isolated_cli(tmp.path(), &root)
        .args(["toggle-task", "Tasks.md", "4", "--expected-text", "- item"])
        .output()
        .unwrap();
    assert!(!tog_mismatch.status.success());
    let mismatch_err = String::from_utf8_lossy(&tog_mismatch.stderr);
    assert!(
        mismatch_err.contains("Task conflict"),
        "expected Task conflict comparison error, got: {mismatch_err}"
    );
    assert!(fs::read_to_string(root.join("Tasks.md"))
        .unwrap()
        .contains("- [ ] other task"));

    // Control: `--expected-text=- item` toggles back
    let tog_eq = isolated_cli(tmp.path(), &root)
        .args([
            "--json",
            "toggle-task",
            "Tasks.md",
            "3",
            "--expected-text=- item",
        ])
        .output()
        .unwrap();
    assert!(
        tog_eq.status.success(),
        "toggle-task --expected-text=- item failed: {}",
        String::from_utf8_lossy(&tog_eq.stderr)
    );
    assert!(fs::read_to_string(root.join("Tasks.md"))
        .unwrap()
        .contains("- [ ] - item"));

    // 3. Controls: --help, --json, missing values, and option typo diagnostics remain intact
    let help_read = isolated_cli(tmp.path(), &root)
        .args(["notes", "read", "--help"])
        .output()
        .unwrap();
    assert!(help_read.status.success());
    assert!(String::from_utf8_lossy(&help_read.stdout).contains("--compact"));

    let bad_flag_read = isolated_cli(tmp.path(), &root)
        .args(["notes", "read", "--compct"])
        .output()
        .unwrap();
    assert!(!bad_flag_read.status.success());
    assert_eq!(
        bad_flag_read.status.code(),
        Some(2),
        "option typo `--compct` must still be rejected by clap as an unknown option, stderr={}",
        String::from_utf8_lossy(&bad_flag_read.stderr)
    );

    let missing_expected_val = isolated_cli(tmp.path(), &root)
        .args(["toggle-task", "Tasks.md", "3", "--expected-text", "--json"])
        .output()
        .unwrap();
    assert!(!missing_expected_val.status.success());
    assert_eq!(missing_expected_val.status.code(), Some(2));

    let missing_delete_note = isolated_cli(tmp.path(), &root)
        .args(["notes", "delete", "--yes", "--json"])
        .output()
        .unwrap();
    assert!(!missing_delete_note.status.success());
    assert_eq!(missing_delete_note.status.code(), Some(2));
}

fn plan_mcp_server(root: &std::path::Path) -> McpServer {
    let vault = vault_from_dir(root, Some("PlanVault"));
    let mut cfg = AppConfig::default();
    cfg.upsert_vault(vault.clone(), true);
    McpServer::new(cfg, vault, Storage::open_in_memory().unwrap())
}

fn plan_json_keys(v: &Value) -> std::collections::BTreeSet<String> {
    v.as_object()
        .expect("expected JSON object")
        .keys()
        .cloned()
        .collect()
}

fn plan_snapshot_dir(root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(
        base: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, Vec<u8>>,
    ) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    let mut map = std::collections::BTreeMap::new();
    walk(root, root, &mut map);
    map
}

#[test]
fn plan_cli_json_flag_order_and_full_compact_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("N.md"), "# N\n").unwrap();

    let run = |args: &[&str]| {
        isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap()
    };

    let c1 = run(&["--json", "notes", "read", "N", "--compact"]);
    let c2 = run(&["notes", "read", "N", "--compact", "--json"]);
    let c3 = run(&["--json", "notes", "read", "N"]);
    for (idx, out) in [(1, &c1), (2, &c2), (3, &c3)] {
        assert!(
            out.status.success(),
            "case {idx} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(c1.stdout, c2.stdout);
    assert_eq!(c1.stdout, c3.stdout);
    let compact_val: Value = serde_json::from_slice(&c1.stdout).unwrap();
    let expected_keys: std::collections::BTreeSet<String> = [
        "hint",
        "mtime_ms",
        "omitted",
        "raw_markdown",
        "rel_path",
        "title",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(plan_json_keys(&compact_val), expected_keys);

    let c4 = run(&["notes", "read", "N", "--json", "--full"]);
    assert!(
        c4.status.success(),
        "{}",
        String::from_utf8_lossy(&c4.stderr)
    );
    let full_val: Value = serde_json::from_slice(&c4.stdout).unwrap();
    assert!(full_val.get("summary").is_some());
    assert!(full_val.get("parsed").is_some());

    let c5 = run(&["notes", "read", "N", "--json", "--compact", "--full"]);
    assert_eq!(c5.status.code(), Some(2));
    assert!(c5.stdout.is_empty());
    assert!(String::from_utf8_lossy(&c5.stderr).contains("cannot be used with"));

    let c6 = run(&["notes", "read", "N", "--full"]);
    assert_eq!(c6.status.code(), Some(2));
    assert!(c6.stdout.is_empty());
    assert!(String::from_utf8_lossy(&c6.stderr).contains("--compact and --full require --json"));

    let c7 = run(&["notes", "read", "N"]);
    assert!(c7.status.success());
    assert_eq!(c7.stdout, b"# N\n");

    let snap_before_daily = plan_snapshot_dir(&root);
    let c8 = run(&["daily", "--full"]);
    assert_eq!(c8.status.code(), Some(2));
    assert!(c8.stdout.is_empty());
    assert!(String::from_utf8_lossy(&c8.stderr).contains("--compact and --full require --json"));
    assert_eq!(plan_snapshot_dir(&root), snap_before_daily);
}

#[test]
fn plan_compact_note_object_reports_exact_omissions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("T.md"),
        "---\naliases: [Tee]\ntags: [x]\n---\n\n# Heading One\n\n[[A]]\n[[A]]\n[[Nope]]\n- [ ] one\n- [x] two\n",
    )
    .unwrap();
    fs::write(root.join("A.md"), "[[T]] and [[T]]\n").unwrap();
    fs::write(root.join("B.md"), "[[T]]\n").unwrap();
    fs::write(root.join("Lonely.md"), "# Lonely\n\nNothing here.\n").unwrap();

    let run = |args: &[&str]| {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };

    let cli_compact = run(&["notes", "read", "T", "--json"]);
    let cli_full = run(&["notes", "read", "T", "--json", "--full"]);
    assert_eq!(
        cli_compact["omitted"],
        json!({
            "backlinks": 2,
            "outgoing_links": 3,
            "images": Value::Null,
            "fields": ["summary", "parsed"],
        })
    );
    assert_eq!(
        cli_compact["omitted"]["backlinks"].as_u64().unwrap() as usize,
        cli_full["backlinks"].as_array().unwrap().len()
    );
    assert_eq!(
        cli_compact["omitted"]["outgoing_links"].as_u64().unwrap() as usize,
        cli_full["outgoing_links"].as_array().unwrap().len()
    );
    let cli_hint = cli_compact["hint"].as_str().unwrap();
    assert!(cli_hint.ends_with("--json --full"), "got hint: {cli_hint}");
    assert!(cli_hint.contains("\"T.md\""), "got hint: {cli_hint}");

    let mcp = plan_mcp_server(&root);
    let mcp_compact = mcp
        .dispatch_tool("read_note", &json!({ "note": "T" }))
        .unwrap();
    let mcp_full = mcp
        .dispatch_tool("read_note", &json!({ "note": "T", "compact": false }))
        .unwrap();
    assert_eq!(
        mcp_compact["omitted"],
        json!({
            "backlinks": 2,
            "outgoing_links": 3,
            "tags": 1,
            "aliases": 1,
            "headings": 1,
            "tasks": 2,
            "fields": ["folder", "frontmatter", "word_count"],
        })
    );
    for key in [
        "backlinks",
        "outgoing_links",
        "tags",
        "aliases",
        "headings",
        "tasks",
    ] {
        assert_eq!(
            mcp_compact["omitted"][key].as_u64().unwrap() as usize,
            mcp_full[key].as_array().unwrap().len(),
            "mismatch for omitted.{key}"
        );
    }
    assert_eq!(
        mcp_compact["hint"],
        r#"read_note {"compact":false,"note":"T.md"}"#
    );

    let lonely = run(&["notes", "read", "Lonely", "--json"]);
    assert_eq!(lonely["omitted"]["backlinks"], json!(0));
    assert_eq!(lonely["omitted"]["outgoing_links"], json!(0));
    assert!(lonely["omitted"]["images"].is_null());

    let daily_compact = run(&["daily", "--date", "2026-11-21", "--json"]);
    let expected_note_keys: std::collections::BTreeSet<String> = [
        "hint",
        "mtime_ms",
        "omitted",
        "raw_markdown",
        "rel_path",
        "title",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(plan_json_keys(&daily_compact["note"]), expected_note_keys);

    let daily_full = run(&["daily", "--date", "2026-11-21", "--json", "--full"]);
    assert!(daily_full["note"].get("summary").is_some());
}

#[test]
fn plan_daily_note_receipt_appends_once_and_points_to_read_note() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();

    let mcp = plan_mcp_server(&root);
    let first = mcp
        .dispatch_tool(
            "daily_note",
            &json!({ "date": "2026-11-21", "append": "- [ ] once" }),
        )
        .unwrap();
    let expected_keys: std::collections::BTreeSet<String> =
        ["created", "hint", "omitted", "raw_markdown", "rel_path"]
            .into_iter()
            .map(String::from)
            .collect();
    assert_eq!(plan_json_keys(&first), expected_keys);
    assert_eq!(first["created"], true);

    let rel_path = first["rel_path"].as_str().unwrap().to_string();
    let hint = first["hint"].as_str().unwrap();
    assert_eq!(
        hint,
        format!(
            "read_note {}",
            json!({ "note": rel_path, "compact": false })
        )
    );
    assert!(!hint.contains("daily_note"));

    let full = mcp
        .dispatch_tool("read_note", &json!({ "note": rel_path, "compact": false }))
        .unwrap();
    let compact = mcp
        .dispatch_tool("read_note", &json!({ "note": rel_path }))
        .unwrap();

    assert_eq!(
        first["omitted"]["fields"],
        json!(["title", "mtime_ms", "folder", "frontmatter", "word_count"])
    );
    for key in [
        "backlinks",
        "outgoing_links",
        "tags",
        "aliases",
        "headings",
        "tasks",
    ] {
        assert!(
            first["omitted"][key].is_u64(),
            "expected numeric omitted.{key}, got {:?}",
            first["omitted"][key]
        );
        assert_eq!(
            first["omitted"][key].as_u64().unwrap() as usize,
            full[key].as_array().unwrap().len(),
            "omitted.{key} must equal read_note compact:false length"
        );
    }
    assert!(first["omitted"]["tasks"].as_u64().unwrap() >= 1);

    let cli_daily_out = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["daily", "--date", "2026-11-21", "--json"])
        .output()
        .unwrap();
    assert!(
        cli_daily_out.status.success(),
        "{}",
        String::from_utf8_lossy(&cli_daily_out.stderr)
    );
    let cli_daily: Value = serde_json::from_slice(&cli_daily_out.stdout).unwrap();

    let disk_after_reads = fs::read_to_string(root.join(&rel_path)).unwrap();
    assert_eq!(disk_after_reads.matches("- [ ] once").count(), 1);
    assert_eq!(full["raw_markdown"].as_str().unwrap(), disk_after_reads);
    assert_eq!(compact["raw_markdown"].as_str().unwrap(), disk_after_reads);
    assert_eq!(
        cli_daily["note"]["raw_markdown"].as_str().unwrap(),
        disk_after_reads
    );

    let second = mcp
        .dispatch_tool("daily_note", &json!({ "date": "2026-11-21" }))
        .unwrap();
    assert_eq!(second["created"], false);
    assert_eq!(
        fs::read_to_string(root.join(&rel_path)).unwrap(),
        disk_after_reads
    );
}

#[test]
fn plan_read_note_mtime_round_trips_into_toggle_task() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("K.md"), "- [ ] one\n").unwrap();

    let mcp = plan_mcp_server(&root);
    let read = mcp
        .dispatch_tool("read_note", &json!({ "note": "K.md" }))
        .unwrap();
    let mtime_ms = read["mtime_ms"].as_i64().unwrap();

    let toggled = mcp
        .dispatch_tool(
            "toggle_task",
            &json!({ "note": "K.md", "line": 1, "expected_mtime_ms": mtime_ms }),
        )
        .unwrap();
    assert_eq!(toggled["checked"], true);
    assert_eq!(
        fs::read_to_string(root.join("K.md")).unwrap(),
        "- [x] one\n"
    );

    let stale = mcp.dispatch_tool(
        "toggle_task",
        &json!({ "note": "K.md", "line": 1, "expected_mtime_ms": 1 }),
    );
    assert!(stale.is_err());
    assert_eq!(
        fs::read_to_string(root.join("K.md")).unwrap(),
        "- [x] one\n"
    );

    let full = mcp
        .dispatch_tool("read_note", &json!({ "note": "K.md", "compact": false }))
        .unwrap();
    let listed = mcp.dispatch_tool("list_notes", &json!({})).unwrap();
    let list_mtime = listed["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["rel_path"] == "K.md")
        .unwrap()["mtime_ms"]
        .as_i64()
        .unwrap();
    assert_eq!(full["mtime_ms"].as_i64().unwrap(), list_mtime);
}

#[test]
fn plan_ambiguous_note_reference_never_reads_or_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    for d in ["a", "b", "x", "y", "Tasks", "Other"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    fs::write(root.join("a/Dup.md"), "A").unwrap();
    fs::write(root.join("b/Dup.md"), "B").unwrap();
    fs::write(root.join("Solo.md"), "SOLO").unwrap();
    fs::write(root.join("x/Al.md"), "---\naliases: [Shared]\n---\nX\n").unwrap();
    fs::write(root.join("y/Bl.md"), "---\naliases: [Shared]\n---\nY\n").unwrap();
    fs::write(root.join("Tasks/Dup2.md"), "- [ ] t\n").unwrap();
    fs::write(root.join("Other/Dup2.md"), "- [ ] t\n").unwrap();

    let snap_before = plan_snapshot_dir(&root);

    let dup_msg = "Ambiguous note reference 'Dup': 2 notes match (a/Dup.md, b/Dup.md). Use a vault-relative path.";
    let shared_msg = "Ambiguous note reference 'Shared': 2 notes match (x/Al.md, y/Bl.md). Use a vault-relative path.";
    let dup2_msg = "Ambiguous note reference 'Dup2': 2 notes match (Other/Dup2.md, Tasks/Dup2.md). Use a vault-relative path.";

    let cli_cases: &[(&[&str], &str)] = &[
        (&["notes", "read", "Dup"], dup_msg),
        (&["notes", "append", "Dup", "X"], dup_msg),
        (
            &["notes", "replace", "Dup", "--find", "A", "--replace", "Z"],
            dup_msg,
        ),
        (&["notes", "delete", "--yes", "Dup"], dup_msg),
        (&["notes", "move", "Dup", "c/Dup"], dup_msg),
        (&["toggle-task", "Dup2", "1"], dup2_msg),
        (&["links", "Dup"], dup_msg),
        (&["notes", "read", "Shared"], shared_msg),
    ];
    for (args, expected_err) in cli_cases {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(*args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "expected exit 1 for {args:?}");
        assert!(out.stdout.is_empty(), "expected empty stdout for {args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(expected_err),
            "expected {expected_err:?} in stderr for {args:?}, got: {stderr}"
        );
    }

    let mcp = plan_mcp_server(&root);
    let mcp_cases: &[(&str, Value, &str)] = &[
        ("read_note", json!({ "note": "Dup" }), dup_msg),
        (
            "append_note",
            json!({ "note": "Dup", "text": "X" }),
            dup_msg,
        ),
        (
            "replace_note_text",
            json!({ "note": "Dup", "find": "A", "replace": "Z" }),
            dup_msg,
        ),
        (
            "update_frontmatter",
            json!({ "note": "Dup", "properties": { "k": "v" } }),
            dup_msg,
        ),
        (
            "delete_note",
            json!({ "note": "Dup", "confirm": true }),
            dup_msg,
        ),
        (
            "move_note",
            json!({ "src": "Dup", "dst": "c/Dup" }),
            dup_msg,
        ),
        (
            "toggle_task",
            json!({ "note": "Dup2", "line": 1 }),
            dup2_msg,
        ),
    ];
    for (tool, args, expected_err) in mcp_cases {
        let err = mcp
            .dispatch_tool(tool, args)
            .expect_err(&format!("expected Err for {tool} {args}"));
        assert!(
            err.to_string().contains(expected_err),
            "expected {expected_err:?} for {tool}, got: {err}"
        );
    }

    assert_eq!(plan_snapshot_dir(&root), snap_before);

    // Controls
    let read_a = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "read", "a/Dup"])
        .output()
        .unwrap();
    assert!(read_a.status.success());
    assert_eq!(read_a.stdout, b"A");

    let read_b = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "read", "b/Dup.md"])
        .output()
        .unwrap();
    assert!(read_b.status.success());
    assert_eq!(read_b.stdout, b"B");

    let app_b = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "append", "b/Dup.md", "X"])
        .output()
        .unwrap();
    assert!(app_b.status.success());
    assert_eq!(fs::read_to_string(root.join("a/Dup.md")).unwrap(), "A");
    assert!(fs::read_to_string(root.join("b/Dup.md"))
        .unwrap()
        .contains('X'));

    let read_solo = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "read", "Solo"])
        .output()
        .unwrap();
    assert!(read_solo.status.success());
    assert_eq!(read_solo.stdout, b"SOLO");

    let read_ci = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "read", "A/DUP.MD", "--json"])
        .output()
        .unwrap();
    assert!(read_ci.status.success());
    let ci_json: Value = serde_json::from_slice(&read_ci.stdout).unwrap();
    assert_eq!(ci_json["rel_path"], "a/Dup.md");
}

#[test]
fn plan_exact_root_path_wins_over_same_named_nested_note() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    fs::write(root.join("Dup.md"), "ROOT").unwrap();
    fs::write(root.join("a/Dup.md"), "A").unwrap();
    fs::write(root.join("b/Dup.md"), "B").unwrap();

    let rd = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "read", "Dup"])
        .output()
        .unwrap();
    assert!(rd.status.success());
    assert_eq!(rd.stdout, b"ROOT");

    let ap = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "append", "Dup", "X"])
        .output()
        .unwrap();
    assert!(ap.status.success());
    assert!(fs::read_to_string(root.join("Dup.md"))
        .unwrap()
        .contains('X'));
    assert_eq!(fs::read_to_string(root.join("a/Dup.md")).unwrap(), "A");
    assert_eq!(fs::read_to_string(root.join("b/Dup.md")).unwrap(), "B");
}

#[test]
fn plan_outgoing_links_classify_attachment_ambiguous_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    for d in ["Attachments", "a", "b"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    fs::write(root.join("Attachments/pic.png"), b"png").unwrap();
    fs::write(root.join("a/Dup.md"), "A").unwrap();
    fs::write(root.join("b/Dup.md"), "B").unwrap();
    fs::write(root.join("Real.md"), "# Real\n").unwrap();
    fs::write(
        root.join("a/Src.md"),
        "![[pic.png]]\n![[nope.png]]\n[[Dup]]\n[[Ghost]]\n[[Real]]\n",
    )
    .unwrap();
    fs::write(root.join("Root.md"), "[[Dup]]\n").unwrap();

    let run = |args: &[&str]| {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };

    let src_json: Value =
        serde_json::from_slice(&run(&["links", "a/Src.md", "--json"]).stdout).unwrap();
    let find_entry = |arr: &Value, target: &str| -> Value {
        arr.as_array()
            .unwrap()
            .iter()
            .find(|e| e["target_raw"] == target)
            .cloned()
            .unwrap_or_else(|| panic!("missing {target} in {arr}"))
    };

    let pic = find_entry(&src_json, "pic.png");
    assert_eq!(pic["status"], "attachment");
    assert_eq!(pic["resolved"], true);
    assert_eq!(pic["target_resolved_path"], "Attachments/pic.png");

    let nope = find_entry(&src_json, "nope.png");
    assert_eq!(nope["status"], "missing");
    assert_eq!(nope["resolved"], false);

    let local_dup = find_entry(&src_json, "Dup");
    assert_eq!(local_dup["status"], "resolved");
    assert_eq!(local_dup["resolved"], true);
    assert_eq!(local_dup["target_resolved_path"], "a/Dup.md");

    let ghost = find_entry(&src_json, "Ghost");
    assert_eq!(ghost["status"], "missing");
    assert!(ghost.get("candidates").is_none());

    let real = find_entry(&src_json, "Real");
    assert_eq!(real["status"], "resolved");
    assert_eq!(real["target_resolved_path"], "Real.md");

    let root_json: Value =
        serde_json::from_slice(&run(&["links", "Root.md", "--json"]).stdout).unwrap();
    let root_dup = find_entry(&root_json, "Dup");
    assert_eq!(root_dup["status"], "ambiguous");
    assert_eq!(root_dup["resolved"], false);
    assert_eq!(root_dup["candidates"], json!(["a/Dup.md", "b/Dup.md"]));

    let src_text = String::from_utf8(run(&["links", "a/Src.md"]).stdout).unwrap();
    let missing_lines: Vec<&str> = src_text
        .lines()
        .filter(|line| line.contains("(missing)"))
        .collect();
    assert_eq!(missing_lines.len(), 2, "got:\n{src_text}");
    assert!(missing_lines.iter().any(|l| l.contains("nope.png")));
    assert!(missing_lines.iter().any(|l| l.contains("Ghost")));
    assert!(src_text.contains("→ pic.png (Attachments/pic.png)"));

    let root_text = String::from_utf8(run(&["links", "Root.md"]).stdout).unwrap();
    assert!(root_text.contains("(ambiguous: a/Dup.md, b/Dup.md)"));

    let mcp = plan_mcp_server(&root);
    let mcp_src = mcp
        .dispatch_tool("get_outgoing_links", &json!({ "note": "a/Src.md" }))
        .unwrap();
    assert_eq!(
        find_entry(&mcp_src["outgoing_links"], "pic.png")["status"],
        "attachment"
    );
    let mcp_full = mcp
        .dispatch_tool(
            "read_note",
            &json!({ "note": "a/Src.md", "compact": false }),
        )
        .unwrap();
    assert_eq!(
        find_entry(&mcp_full["outgoing_links"], "pic.png")["status"],
        "attachment"
    );

    let list_json: Value =
        serde_json::from_slice(&run(&["notes", "list", "--json"]).stdout).unwrap();
    let src_summary = list_json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["rel_path"] == "a/Src.md")
        .unwrap();
    assert_eq!(src_summary["outgoing_count"], 5);

    fs::remove_file(root.join("Attachments/pic.png")).unwrap();
    let after_del: Value =
        serde_json::from_slice(&run(&["links", "a/Src.md", "--json"]).stdout).unwrap();
    let pic_after = find_entry(&after_del, "pic.png");
    assert_eq!(pic_after["status"], "missing");
    assert_eq!(pic_after["resolved"], false);
    assert!(pic_after["target_resolved_path"].is_null());
}

#[test]
fn plan_move_rewrites_moved_note_destinations_without_retargeting() {
    for use_mcp in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        for d in ["sub", "img", "deep/nested/img", "incoming"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("sub/Note.md"), "# Note\n").unwrap();
        fs::write(root.join("img/pic.png"), b"real").unwrap();
        fs::write(root.join("deep/nested/img/pic.png"), b"decoy").unwrap();

        let mover_before = concat!(
            "[plain](sub/Note.md)\n",
            "[dot](./sub/Note.md)\n",
            "![img](img/pic.png)\n",
            "![imgdot](./img/pic.png)\n",
            "[abs](/sub/Note.md)\n",
            "![a](/img/pic.png)\n",
            "[anchor](#local)\n",
            "[e](https://example.com/img/pic.png)\n",
            "[m](mailto:a@example.invalid)\n",
            "[p](//host/x.png)\n",
            "`[inline](sub/Note.md)`\n",
            "```md\n",
            "![fence](img/pic.png)\n",
            "```\n",
            "![embed](sub/Note.md)\n",
            "[shot](img/pic.png)\n",
            "![x](nope.png)\n",
        );
        let mover_expected = concat!(
            "[plain](../../sub/Note.md)\n",
            "[dot](../../sub/Note.md)\n",
            "![img](../../img/pic.png)\n",
            "![imgdot](../../img/pic.png)\n",
            "[abs](/sub/Note.md)\n",
            "![a](/img/pic.png)\n",
            "[anchor](#local)\n",
            "[e](https://example.com/img/pic.png)\n",
            "[m](mailto:a@example.invalid)\n",
            "[p](//host/x.png)\n",
            "`[inline](sub/Note.md)`\n",
            "```md\n",
            "![fence](img/pic.png)\n",
            "```\n",
            "![embed](../../sub/Note.md)\n",
            "[shot](../../img/pic.png)\n",
            "![x](nope.png)\n",
        );
        fs::write(root.join("Mover.md"), mover_before).unwrap();
        fs::write(
            root.join("Incoming.md"),
            "[a](Mover.md)\n[b](./Mover.md)\n[[Mover]]\n",
        )
        .unwrap();
        fs::write(
            root.join("incoming/Reader.md"),
            "[m](../Mover.md)\n![[Mover]]\n",
        )
        .unwrap();

        let res = if use_mcp {
            let mcp = plan_mcp_server(&root);
            mcp.dispatch_tool(
                "move_note",
                &json!({ "src": "Mover.md", "dst": "deep/nested/Mover.md" }),
            )
            .unwrap()
        } else {
            let out = isolated_cli(tmp.path(), &root)
                .env("OBSIDIAN_AUTO_DAEMON", "0")
                .args([
                    "--json",
                    "notes",
                    "move",
                    "Mover.md",
                    "deep/nested/Mover.md",
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice(&out.stdout).unwrap()
        };

        assert_eq!(res["updated_links_in_notes"], 3);
        assert_eq!(
            fs::read(root.join("deep/nested/Mover.md")).unwrap(),
            mover_expected.as_bytes()
        );
        assert_eq!(
            fs::read(root.join("deep/nested").join("../../img/pic.png")).unwrap(),
            b"real"
        );
        assert_eq!(
            fs::read_to_string(root.join("Incoming.md")).unwrap(),
            "[a](deep/nested/Mover.md)\n[b](deep/nested/Mover.md)\n[[Mover]]\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("incoming/Reader.md")).unwrap(),
            "[m](../deep/nested/Mover.md)\n![[Mover]]\n"
        );

        let full_out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(["notes", "read", "deep/nested/Mover.md", "--json", "--full"])
            .output()
            .unwrap();
        assert!(full_out.status.success());
        let full: Value = serde_json::from_slice(&full_out.stdout).unwrap();
        let img_entry = full["images"]
            .as_array()
            .unwrap()
            .iter()
            .find(|img| img["raw_target"] == "../../img/pic.png")
            .expect("expected ../../img/pic.png in images");
        let resolved_abs = std::path::PathBuf::from(img_entry["abs_path"].as_str().unwrap());
        assert_eq!(
            fs::canonicalize(&resolved_abs).unwrap(),
            fs::canonicalize(root.join("img/pic.png")).unwrap()
        );
    }
}

#[test]
fn plan_move_back_up_preserves_anchor_title_percent_and_angle_forms() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    for d in ["deep/nested", "sub", "my files"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    fs::write(root.join("sub/Note.md"), "# Note\n\n## Sec\n").unwrap();
    fs::write(root.join("my files/spec v1.pdf"), b"%PDF").unwrap();
    let before = concat!(
        "[up](../../sub/Note.md#Sec \"t\")\r\n",
        "[enc](../../my%20files/spec%20v1.pdf)\n",
        "[sp](<../../my files/spec v1.pdf>)",
    );
    let expected = concat!(
        "[up](sub/Note.md#Sec \"t\")\r\n",
        "[enc](my%20files/spec%20v1.pdf)\n",
        "[sp](<my files/spec v1.pdf>)",
    );
    fs::write(root.join("deep/nested/Mover.md"), before).unwrap();

    let out = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args([
            "--json",
            "notes",
            "move",
            "deep/nested/Mover.md",
            "Mover.md",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(root.join("Mover.md")).unwrap(),
        expected.as_bytes()
    );
}

#[test]
fn plan_move_keeps_location_independent_destinations() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    for d in ["img", "x", "y/img", "y/z", "Attachments", "a", "b", "sub"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    fs::write(root.join("img/pic.png"), b"root-img").unwrap();
    fs::write(root.join("y/img/pic.png"), b"shadow-img").unwrap();
    fs::write(root.join("Attachments/pic.png"), b"att-img").unwrap();
    fs::write(root.join("sub/pic.png"), b"sub-shadow").unwrap();
    fs::write(root.join("a/local.png"), b"local").unwrap();
    fs::write(root.join("a/Peer.md"), "# Peer\n").unwrap();

    let m6 = "![r](img/pic.png)\n";
    let m7 = "![r](img/pic.png)\n";
    let m17 = "![[pic.png]]\n";
    let m18 = "![[pic.png|200]]\n";
    let m19 = "[p](local.png)\n[n](Peer.md)\n";
    fs::write(root.join("x/Mover.md"), m6).unwrap();
    fs::write(root.join("x/M.md"), m7).unwrap();
    fs::write(root.join("a/W.md"), m17).unwrap();
    fs::write(root.join("a/W2.md"), m18).unwrap();
    fs::write(root.join("a/X.md"), m19).unwrap();

    let move_cli = |src: &str, dst: &str| -> Value {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(["--json", "notes", "move", src, dst])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    // M6: vault-root-relative stays untouched when unshadowed
    let r6 = move_cli("x/Mover.md", "y/z/Mover.md");
    assert_eq!(r6["updated_links_in_notes"], 0);
    assert_eq!(fs::read(root.join("y/z/Mover.md")).unwrap(), m6.as_bytes());

    // M7: vault-root-relative rewritten when destination shadows it
    let r7 = move_cli("x/M.md", "y/M.md");
    assert_eq!(r7["updated_links_in_notes"], 1);
    assert_eq!(
        fs::read_to_string(root.join("y/M.md")).unwrap(),
        "![r](../img/pic.png)\n"
    );

    // M17: wikilink image embed stays untouched when unshadowed
    let r17 = move_cli("a/W.md", "b/W.md");
    assert_eq!(r17["updated_links_in_notes"], 0);
    assert_eq!(fs::read(root.join("b/W.md")).unwrap(), m17.as_bytes());

    // M18: wikilink image embed rewritten when destination folder shadows it
    let r18 = move_cli("a/W2.md", "sub/W2.md");
    assert_eq!(r18["updated_links_in_notes"], 1);
    assert_eq!(
        fs::read_to_string(root.join("sub/W2.md")).unwrap(),
        "![[Attachments/pic.png|200]]\n"
    );
    let links_out = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["links", "sub/W2.md", "--json"])
        .output()
        .unwrap();
    assert!(links_out.status.success());
    let links_json: Value = serde_json::from_slice(&links_out.stdout).unwrap();
    assert_eq!(links_json[0]["status"], "attachment");
    assert_eq!(links_json[0]["target_resolved_path"], "Attachments/pic.png");

    // M19: same-folder rename leaves own destinations untouched
    let r19 = move_cli("a/X.md", "a/Y.md");
    assert_eq!(r19["updated_links_in_notes"], 0);
    assert_eq!(fs::read(root.join("a/Y.md")).unwrap(), m19.as_bytes());
}

#[test]
fn plan_graph_degrees_depth_and_order_are_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("A.md"),
        "[[Ghost]]\n[[C]]\n[[C]]\n[[C]]\n![[pic.png]]\n[[A]]\n",
    )
    .unwrap();
    fs::write(root.join("B.md"), "[[Ghost]]\n").unwrap();
    fs::write(root.join("C.md"), "# C\n").unwrap();

    let run = |args: &[&str]| {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    };

    let g1_bytes = run(&["graph", "--json"]);
    let g2_bytes = run(&["graph", "--json"]);
    assert_eq!(
        g1_bytes, g2_bytes,
        "global graph output must be byte-identical"
    );

    let g1: Value = serde_json::from_slice(&g1_bytes).unwrap();
    let find_node = |id: &str| -> Value {
        g1["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("missing node {id} in {g1}"))
    };
    assert_eq!(find_node("unresolved:Ghost")["in_degree"], 2);
    assert_eq!(find_node("A.md")["out_degree"], 2);
    assert_eq!(find_node("C.md")["in_degree"], 1);

    let g_hi: Value =
        serde_json::from_slice(&run(&["graph", "A", "--depth", "9", "--json"])).unwrap();
    assert_eq!(g_hi["depth"], 5);
    let g_lo: Value =
        serde_json::from_slice(&run(&["graph", "A", "--depth", "0", "--json"])).unwrap();
    assert_eq!(g_lo["depth"], 1);

    let mcp = plan_mcp_server(&root);
    let mcp_g = mcp
        .dispatch_tool("get_local_graph", &json!({ "note": "A", "depth": 9 }))
        .unwrap();
    assert_eq!(mcp_g["depth"], 5);

    let list_json: Value = serde_json::from_slice(&run(&["notes", "list", "--json"])).unwrap();
    let a_note = list_json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["rel_path"] == "A.md")
        .unwrap();
    assert_eq!(a_note["outgoing_count"], 6);
}

fn seed_plan_paging_vault(root: &std::path::Path) {
    for d in ["Alpha", "Beta", "Gamma", "x", "y"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    let folders = ["Alpha", "Beta", "Gamma"];
    for i in 0..118usize {
        let folder = folders[i % folders.len()];
        let name = format!("Note-{i:03}.md");
        fs::write(
            root.join(folder).join(name),
            format!("# Note {i:03}\n- [ ] open {i}\n- [x] done {i}\n"),
        )
        .unwrap();
    }
    fs::write(
        root.join("x/Same.md"),
        "# Same\n- [ ] open x\n- [x] done x\n",
    )
    .unwrap();
    fs::write(
        root.join("y/same.md"),
        "# same\n- [ ] open y\n- [x] done y\n",
    )
    .unwrap();
}

#[test]
fn plan_note_and_task_pages_cover_everything_exactly_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    seed_plan_paging_vault(&root);

    let run_json = |args: &[&str]| -> Value {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let first_page = run_json(&["notes", "list", "--json"]);
    assert_eq!(first_page["returned"], 50);
    assert_eq!(first_page["total"], 120);
    assert_eq!(first_page["has_more"], true);
    assert!(first_page["next_cursor"].is_string());

    let all_notes = run_json(&["notes", "list", "--json", "--all"]);
    let all_paths: Vec<String> = all_notes["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["rel_path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(all_paths.len(), 120);

    let mut paged_paths = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;
    loop {
        let mut args = vec!["notes", "list", "--json", "--limit", "50"];
        if let Some(ref c) = cursor {
            args.push("--cursor");
            args.push(c.as_str());
        }
        let p = run_json(&args);
        pages += 1;
        for n in p["notes"].as_array().unwrap() {
            paged_paths.push(n["rel_path"].as_str().unwrap().to_string());
        }
        if p["has_more"].as_bool().unwrap() {
            cursor = Some(p["next_cursor"].as_str().unwrap().to_string());
        } else {
            assert!(p["next_cursor"].is_null());
            break;
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(paged_paths, all_paths);

    let lim1 = run_json(&["notes", "list", "--json", "--limit", "1"]);
    assert_eq!(lim1["returned"], 1);
    assert_eq!(lim1["has_more"], true);

    let lim500 = run_json(&["notes", "list", "--json", "--limit", "500"]);
    assert_eq!(lim500["returned"], 120);
    assert_eq!(lim500["has_more"], false);
    assert!(lim500["next_cursor"].is_null());

    let folder_page = run_json(&["notes", "list", "--json", "--folder", "Alpha"]);
    assert_eq!(folder_page["total"], 40);
    assert_eq!(folder_page["returned"], 40);
    assert_eq!(folder_page["has_more"], false);

    let past_end = run_json(&["notes", "list", "--json", "--cursor", "zzzz/PastEnd.md"]);
    assert_eq!(past_end["returned"], 0);
    assert_eq!(past_end["has_more"], false);
    assert!(past_end["next_cursor"].is_null());

    // Tasks paging
    let all_tasks = run_json(&["tasks", "--json", "--all"]);
    let all_task_keys: Vec<(String, u64, bool)> = all_tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["note_path"].as_str().unwrap().to_string(),
                t["line_no"].as_u64().unwrap(),
                t["checked"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(all_task_keys.len(), 240);
    assert!(all_task_keys[..120].iter().all(|(_, _, checked)| !checked));
    assert!(all_task_keys[120..].iter().all(|(_, _, checked)| *checked));

    let mut paged_tasks = Vec::new();
    let mut tcursor: Option<String> = None;
    loop {
        let mut args = vec!["tasks", "--json", "--limit", "100"];
        if let Some(ref c) = tcursor {
            args.push("--cursor");
            args.push(c.as_str());
        }
        let p = run_json(&args);
        for t in p["tasks"].as_array().unwrap() {
            paged_tasks.push((
                t["note_path"].as_str().unwrap().to_string(),
                t["line_no"].as_u64().unwrap(),
                t["checked"].as_bool().unwrap(),
            ));
        }
        if p["has_more"].as_bool().unwrap() {
            tcursor = Some(p["next_cursor"].as_str().unwrap().to_string());
        } else {
            assert!(p["next_cursor"].is_null());
            break;
        }
    }
    assert_eq!(paged_tasks, all_task_keys);

    let open_all = run_json(&["tasks", "--open", "--json", "--all"]);
    assert_eq!(open_all["returned"], 120);
    assert_eq!(open_all["total"], 120);

    // MCP paging
    let mcp = plan_mcp_server(&root);
    let m1 = mcp.dispatch_tool("list_notes", &json!({})).unwrap();
    assert_eq!(m1["count"], 50);
    assert_eq!(m1["returned"], 50);
    assert_eq!(m1["total"], 120);
    let mut mcp_paths = Vec::new();
    let mut mcur: Option<String> = None;
    loop {
        let mut arg = json!({ "limit": 50 });
        if let Some(ref c) = mcur {
            arg["cursor"] = json!(c);
        }
        let p = mcp.dispatch_tool("list_notes", &arg).unwrap();
        assert_eq!(p["count"], p["returned"]);
        for n in p["notes"].as_array().unwrap() {
            mcp_paths.push(n["rel_path"].as_str().unwrap().to_string());
        }
        if p["has_more"].as_bool().unwrap() {
            mcur = Some(p["next_cursor"].as_str().unwrap().to_string());
        } else {
            break;
        }
    }
    assert_eq!(mcp_paths, all_paths);

    let mt = mcp
        .dispatch_tool("list_tasks", &json!({ "limit": 5 }))
        .unwrap();
    assert_eq!(mt["count"], 5);
    assert_eq!(mt["returned"], 5);
    assert_eq!(mt["total"], 240);
    assert_eq!(mt["has_more"], true);

    // Text `notes list` remains unbounded
    let text_out = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["notes", "list"])
        .output()
        .unwrap();
    assert!(text_out.status.success());
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert_eq!(text.lines().count(), 120);
}

#[test]
fn plan_paging_survives_deletion_of_cursor_note() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    seed_plan_paging_vault(&root);

    let run_json = |args: &[&str]| -> Value {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let p1 = run_json(&["notes", "list", "--json", "--limit", "50"]);
    let deleted_cursor = p1["next_cursor"].as_str().unwrap().to_string();
    fs::remove_file(root.join(&deleted_cursor)).unwrap();

    let mut remaining = Vec::new();
    let mut cursor = Some(deleted_cursor.clone());
    while let Some(c) = cursor {
        let p = run_json(&["notes", "list", "--json", "--limit", "50", "--cursor", &c]);
        for n in p["notes"].as_array().unwrap() {
            remaining.push(n["rel_path"].as_str().unwrap().to_string());
        }
        cursor = if p["has_more"].as_bool().unwrap() {
            Some(p["next_cursor"].as_str().unwrap().to_string())
        } else {
            None
        };
    }

    assert_eq!(remaining.len(), 70);
    assert!(!remaining.contains(&deleted_cursor));
    let unique: std::collections::HashSet<&String> = remaining.iter().collect();
    assert_eq!(unique.len(), 70);
}

#[test]
fn plan_paging_rejects_bad_limits_and_cursors() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("One.md"), "# One\n- [ ] task\n").unwrap();

    for bad_args in [
        &["notes", "list", "--limit", "0"][..],
        &["notes", "list", "--limit", "501"][..],
        &["notes", "list", "--all", "--limit", "5"][..],
    ] {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(bad_args)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "expected exit 2 for {bad_args:?}"
        );
        assert!(out.stdout.is_empty());
    }

    let bad_cur = isolated_cli(tmp.path(), &root)
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["tasks", "--cursor", "abc"])
        .output()
        .unwrap();
    assert_eq!(bad_cur.status.code(), Some(1));
    assert!(bad_cur.stdout.is_empty());
    assert!(String::from_utf8_lossy(&bad_cur.stderr).contains("Invalid cursor: 'abc'"));

    let mcp = plan_mcp_server(&root);
    let e1 = mcp
        .dispatch_tool("list_notes", &json!({ "limit": 0 }))
        .unwrap_err()
        .to_string();
    assert!(
        e1.contains("Invalid 'limit': expected an integer from 1 to 500"),
        "{e1}"
    );
    let e2 = mcp
        .dispatch_tool("list_notes", &json!({ "limit": "5" }))
        .unwrap_err()
        .to_string();
    assert!(
        e2.contains("Invalid 'limit': expected an integer from 1 to 500"),
        "{e2}"
    );
    let e3 = mcp
        .dispatch_tool("list_notes", &json!({ "cursor": 5 }))
        .unwrap_err()
        .to_string();
    assert!(e3.contains("Invalid 'cursor': expected a string"), "{e3}");
    let e4 = mcp
        .dispatch_tool("list_tasks", &json!({ "cursor": "abc" }))
        .unwrap_err()
        .to_string();
    assert!(e4.contains("Invalid cursor: 'abc'"), "{e4}");
}

#[test]
fn plan_search_reports_applied_limit_and_truncation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).unwrap();
    for i in 0..25usize {
        fs::write(
            root.join(format!("Hit-{i:02}.md")),
            format!("# Hit {i:02}\n\nContains needle token {i}.\n"),
        )
        .unwrap();
    }

    let run_json = |args: &[&str]| -> Value {
        let out = isolated_cli(tmp.path(), &root)
            .env("OBSIDIAN_AUTO_DAEMON", "0")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let def = run_json(&["search", "needle", "--json"]);
    assert_eq!(def["returned"], 20);
    assert_eq!(def["limit"], 20);
    assert_eq!(def["has_more"], true);
    assert!(def["next_cursor"].is_null());

    let exact = run_json(&["search", "needle", "--json", "--limit", "25"]);
    assert_eq!(exact["returned"], 25);
    assert_eq!(exact["limit"], 25);
    assert_eq!(exact["has_more"], false);

    let hi = run_json(&["search", "needle", "--json", "--limit", "500"]);
    assert_eq!(hi["limit"], 200);
    assert_eq!(hi["returned"], 25);
    assert_eq!(hi["has_more"], false);

    let lo = run_json(&["search", "needle", "--json", "--limit", "0"]);
    assert_eq!(lo["limit"], 1);
    assert_eq!(lo["returned"], 1);
    assert_eq!(lo["has_more"], true);

    let mcp = plan_mcp_server(&root);
    let m_def = mcp
        .dispatch_tool("search_notes", &json!({ "query": "needle" }))
        .unwrap();
    assert_eq!(m_def["limit"], 20);
    assert_eq!(m_def["returned"], 20);
    assert_eq!(m_def["count"], 20);
    assert_eq!(m_def["has_more"], true);
    assert!(m_def["next_cursor"].is_null());

    let m_25 = mcp
        .dispatch_tool("search_notes", &json!({ "query": "needle", "limit": 25 }))
        .unwrap();
    assert_eq!(m_25["limit"], 25);
    assert_eq!(m_25["returned"], 25);
    assert_eq!(m_25["count"], 25);
    assert_eq!(m_25["has_more"], false);
}

#[test]
fn plan_daemon_status_reports_unknown_note_total_when_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let out = isolated_cli_base(tmp.path())
        .env("OBSIDIAN_AUTO_DAEMON", "0")
        .args(["--json", "daemon", "status"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let st: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(st["running"], false);
    assert!(
        st["total_notes"].is_null(),
        "expected null total_notes, got {st}"
    );
}
