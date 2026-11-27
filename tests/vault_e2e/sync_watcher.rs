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
    let app_router = build_router(state);
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
        img_preview::color_to_rgb,
        ui::{app::inline_preview_cache_key, overlays::image_preview::ImagePreviewOverlay},
        vault::fs::{move_note, rewrite_wikilinks_for_move},
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

    let inline_bg = color_to_rgb(ui.theme.bg_canvas, (19, 19, 24));
    let modal_bg = color_to_rgb(ui.theme.bg_elevated, (19, 19, 24));
    let key1_inline = inline_preview_cache_key(&img_abs, inline_bg, app.graphics_caps.cell_px);
    let rendered1_inline = app
        .preview_cache
        .get(&key1_inline)
        .expect("inline image should be cached")
        .lines
        .clone();

    let mut modal =
        ImagePreviewOverlay::from_note(app.active_note.as_ref().unwrap(), Some(0)).unwrap();
    app.ensure_modal_image_cached(&mut modal, 100, 30, modal_bg);
    let (cols, rows) = ImagePreviewOverlay::viewport_dimensions(100, 30);
    let key1_modal = ImagePreviewOverlay::cache_key(
        modal.current().unwrap(),
        cols,
        rows,
        modal.zoom_pct,
        app.graphics_caps.cell_px,
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
    let key2_inline = inline_preview_cache_key(&img_abs, inline_bg, app.graphics_caps.cell_px);
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

    app.ensure_modal_image_cached(&mut modal, 100, 30, modal_bg);
    let key2_modal = ImagePreviewOverlay::cache_key(
        modal.current().unwrap(),
        cols,
        rows,
        modal.zoom_pct,
        app.graphics_caps.cell_px,
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
