#[allow(unused_imports)]
use std::{fs, sync::Arc};

#[allow(unused_imports)]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
#[allow(unused_imports)]
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
#[allow(unused_imports)]
use ratatui::{backend::TestBackend, Terminal};
#[allow(unused_imports)]
use serde_json::{json, Value};
#[allow(unused_imports)]
use tokio::sync::{mpsc, watch};

#[allow(unused_imports)]
use crate::common::*;

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
        .get_graph(&vault, Some("Projects/Q4 Architecture RFC.md"), 2)
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
    let app = build_router(state);
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
        let md_decoded = percent_encoding::percent_decode_str(md_dest)
            .decode_utf8()
            .unwrap();
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
