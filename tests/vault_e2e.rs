//! End-to-end integration tests for `@bitefoundry/obsidian`:
//! - Real temporary Obsidian vault with YAML frontmatter, wikilinks, aliases, embeds, callouts, tasks, headings
//! - Backlinks, outgoing links, unresolved links, local/global knowledge graph, orphans, daily notes
//! - Surgical task checkbox toggle file mutation & `.trash/` safe deletion
//! - SQLite WAL + FTS5 search + Korean `<3` char `LIKE` fallback
//! - HTTP API (`:8792` router) across all endpoints
//! - Stdio MCP JSON-RPC 2.0 server across all 18 typed tools
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
    assert!(detail.outgoing_links.iter().any(|l| l.target_raw == "Storage Engine" && l.resolved));
    assert!(detail.outgoing_links.iter().any(|l| l.target_raw == "Unwritten Spec" && !l.resolved));

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
    let h: Value = client.get(format!("{base}/health")).send().await.unwrap().json().await.unwrap();
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
        .get(format!("{base}/api/v1/notes/Projects/Q4%20Architecture%20RFC.md"))
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
    let _dummy_req = Request::builder().uri("/health").body(Body::empty()).unwrap();
    server.abort();
}

#[test]
fn test_all_18_mcp_tools_json_rpc() {
    assert_eq!(tool_definitions().len(), 18);

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
    assert_eq!(init["result"]["serverInfo"]["name"], "@bitefoundry/obsidian");

    let list = mcp
        .handle_rpc(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
        .unwrap();
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 18);

    // Exercise every single one of the 18 tools
    assert!(mcp.dispatch_tool("list_vaults", &json!({})).is_ok());
    assert!(mcp.dispatch_tool("list_notes", &json!({})).is_ok());
    assert!(mcp.dispatch_tool("search_notes", &json!({ "query": "검색" })).is_ok());
    assert!(mcp.dispatch_tool("read_note", &json!({ "note": "RFC-042" })).is_ok());
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
        .dispatch_tool("delete_note", &json!({ "note": "MCP Created", "confirm": false }))
        .is_err());
    let del = mcp
        .dispatch_tool("delete_note", &json!({ "note": "MCP Created", "confirm": true }))
        .unwrap();
    assert_eq!(del["deleted"], true);
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
