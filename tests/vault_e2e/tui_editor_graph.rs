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
        .starts_with("# LEDIT_READY"));

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
            .starts_with("# LEDIT_READY"),
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
        .starts_with("# LEDIT_READY"));

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
        .starts_with("# LEDIT_READY"));

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
        .starts_with("# LEDIT_READY"));

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
        .starts_with("# LEDIT_READY"));

    // Explicitly confirming ForceQuitDiscard exits cleanly WITHOUT recreating the deleted file
    app.dispatch(Action::ForceQuitDiscard, &mut ui);
    assert!(ui.should_quit);
    assert!(
        !note_abs.exists(),
        "ForceQuitDiscard must NOT recreate deleted file"
    );
}
