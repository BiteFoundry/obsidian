use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, Terminal};
use tokio::sync::{mpsc, watch};

use super::*;
use std::fs;
use crate::{
    config::AppConfig,
    models::{CoreEvent, IndexStats, NoteBlock, SidebarTab},
    storage::Storage,
    ui::{
        action::Action,
        hitmap::{ButtonId, HitMap, HitTarget},
        keymap::KeyMap,
        overlays::{Overlay, OverlayKind},
        sidebar::{build_sidebar_entries, SidebarEntry},
    },
};
use crate::{
    config::ImageProtocolPreference,
    img_preview::TerminalImageProtocol,
    models::BacklinkEntry,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::{self, event::key_to_action},
    vault::vault_from_dir,
};

fn make_valid_png_bytes() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([120, 80, 240, 255]));
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

fn make_harness(root: &std::path::Path, name: &str) -> (AppState, UiState) {
    let vault = vault_from_dir(root, Some(name));
    let cfg = AppConfig {
        load_failed: true, // prevent tests from touching real user config.toml
        ..AppConfig::default()
    };
    let storage = Storage::open_in_memory().unwrap();
    storage.sync_vault(&vault).unwrap();
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let mut app = AppState::new(cfg.clone(), vault, storage, sync_ctrl);
    let mut ui = UiState::from_config(&cfg);
    app.reload_from_storage(&mut ui);
    (app, ui)
}

fn draw_frame(app: &AppState, ui: &mut UiState, width: u16, height: u16) -> HitMap {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut hitmap = HitMap::new();
    terminal
        .draw(|f| {
            hitmap = ui::draw(f, app, ui);
        })
        .unwrap();
    hitmap
}

#[test]
fn p0_overlay_backdrop_closes_modal_without_clickthrough_or_esc_trap() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("Alpha.md"), "# Alpha\n\nBody A\n").unwrap();
    fs::write(tmp.path().join("Beta.md"), "# Beta\n\nBody B\n").unwrap();
    let (mut app, mut ui) = make_harness(tmp.path(), "BackdropVault");
    app.open_note(&mut ui, "Alpha.md", false);
    ui.focus = Focus::Canvas;

    // First draw without overlay to locate Beta's sidebar row coordinates.
    let base_map = draw_frame(&app, &mut ui, 120, 34);
    let beta_row = base_map
        .region_of(HitTarget::SidebarRow(1))
        .expect("Beta row in sidebar");

    // Open Settings overlay and redraw: underlying sidebar must NOT be in hitmap!
    app.dispatch(Action::OpenOverlay(OverlayKind::Settings), &mut ui);
    assert!(ui.overlay.is_some());
    assert_eq!(ui.focus, Focus::Overlay);
    let ov_map = draw_frame(&app, &mut ui, 120, 34);
    assert_eq!(
        ov_map.hit(beta_row.y, beta_row.x + 1),
        Some(HitTarget::OverlayBackdrop)
    );

    // Clicking outside the modal where SidebarRow(1) sits must close the overlay,
    // restore Canvas focus, and NOT switch active_note to Beta!
    app.dispatch(
        Action::MouseClick {
            col: beta_row.x + 1,
            row: beta_row.y,
        },
        &mut ui,
    );
    assert!(ui.overlay.is_none());
    assert_eq!(ui.focus, Focus::Canvas);
    assert_eq!(
        app.active_note.as_ref().unwrap().summary.rel_path,
        "Alpha.md"
    );

    // Even if focus somehow diverged while an overlay is open, Esc (`ClearTagFilter`) still closes it.
    app.dispatch(Action::OpenOverlay(OverlayKind::Help), &mut ui);
    ui.focus = Focus::Sidebar;
    app.dispatch(Action::ClearTagFilter, &mut ui);
    assert!(ui.overlay.is_none());
}

#[test]
fn p0_right_click_sidebar_row_targets_clicked_note_not_active_note() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("Alpha.md"), "# Alpha\n\nKeep me.\n").unwrap();
    fs::write(tmp.path().join("Beta.md"), "# Beta\n\nTrash me.\n").unwrap();
    let (mut app, mut ui) = make_harness(tmp.path(), "ContextVault");
    app.open_note(&mut ui, "Alpha.md", false);
    assert_eq!(
        app.active_note.as_ref().unwrap().summary.rel_path,
        "Alpha.md"
    );

    let hitmap = draw_frame(&app, &mut ui, 120, 34);
    let beta_row = hitmap
        .region_of(HitTarget::SidebarRow(1))
        .expect("Beta row in sidebar");

    // Right-click Beta while Alpha is active
    app.dispatch(
        Action::OpenContextMenu {
            col: beta_row.x + 2,
            row: beta_row.y,
        },
        &mut ui,
    );
    assert!(matches!(ui.overlay, Some(Overlay::ContextMenu(_))));
    assert_eq!(
        app.active_note.as_ref().unwrap().summary.rel_path,
        "Beta.md"
    );

    // Choose "Move Note to .trash/" (last item) and confirm
    if let Some(Overlay::ContextMenu(ref mut cm)) = ui.overlay {
        cm.selected = cm.items.len() - 1;
    }
    app.dispatch(Action::Activate, &mut ui);
    assert!(matches!(ui.overlay, Some(Overlay::Confirm(_))));

    // Right-clicking while Confirm modal is open must be blocked!
    app.dispatch(Action::OpenContextMenu { col: 5, row: 5 }, &mut ui);
    assert!(matches!(ui.overlay, Some(Overlay::Confirm(_))));

    // Confirm trash -> Beta is trashed, Alpha survives!
    app.dispatch(Action::Input('y'), &mut ui);
    assert!(tmp.path().join("Alpha.md").is_file());
    assert!(!tmp.path().join("Beta.md").exists());
}

#[test]
fn p0_global_keymap_does_not_hijack_overlay_shortcuts() {
    let km = KeyMap;
    for (ch, expected) in [
        ('y', Action::Input('y')),
        ('G', Action::Input('G')),
        ('?', Action::Input('?')),
        ('.', Action::Input('.')),
    ] {
        let mods = if ch.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        let act = key_to_action(
            KeyEvent::new(KeyCode::Char(ch), mods),
            Focus::Overlay,
            CanvasMode::Note,
            &km,
            false,
        );
        assert_eq!(act, Some(expected), "char '{ch}' must reach overlay");
    }
    // Ctrl+C still triggers QuitConfirm inside overlays
    let quit = key_to_action(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        Focus::Overlay,
        CanvasMode::Note,
        &km,
        false,
    );
    assert_eq!(quit, Some(Action::QuitConfirm));
}

#[test]
fn p1_inspector_scroll_windowing_and_clickable_outline() {
    let tmp = tempfile::tempdir().unwrap();
    let mut md = String::from("# Doc\n\n");
    for i in 1..=12 {
        md.push_str(&format!("## Heading {i}\n\nParagraph {i} with [[Link{i}]].\n\n"));
    }
    fs::write(tmp.path().join("Doc.md"), &md).unwrap();
    let (mut app, mut ui) = make_harness(tmp.path(), "InspectorVault");
    app.open_note(&mut ui, "Doc.md", false);

    // Populate 10 backlinks to exercise backlink scroll windowing in a compact viewport
    if let Some(detail) = app.active_note.as_mut() {
        detail.backlinks = (0..10)
            .map(|i| BacklinkEntry {
                source_path: format!("Src{i}.md"),
                source_title: format!("Source {i}"),
                context_snippet: format!("Snippet {i}"),
                heading: None,
                is_embed: false,
            })
            .collect();
    }
    ui.focus = Focus::Inspector;
    ui.inspector_selected = 9; // last backlink
    let map_bl = draw_frame(&app, &mut ui, 120, 26);
    assert!(
        map_bl.region_of(HitTarget::InspectorBacklink(9)).is_some(),
        "selected backlink #9 must scroll into viewport"
    );

    // Select last outgoing link -> must scroll into viewport
    let bl_len = app.active_note.as_ref().unwrap().backlinks.len();
    let ol_len = app.active_note.as_ref().unwrap().outgoing_links.len();
    ui.inspector_selected = bl_len + ol_len - 1;
    let map_ol = draw_frame(&app, &mut ui, 120, 26);
    assert!(
        map_ol
            .region_of(HitTarget::InspectorOutgoing(ol_len - 1))
            .is_some(),
        "selected outgoing link must scroll into viewport"
    );

    // Select last outline heading -> must scroll into viewport and clicking it jumps Canvas!
    let h_len = app.active_note.as_ref().unwrap().parsed.headings.len();
    let target_heading_idx = h_len - 1;
    let target_line = app.active_note.as_ref().unwrap().parsed.headings[target_heading_idx].line_no;
    ui.inspector_selected = bl_len + ol_len + target_heading_idx;
    let map_h = draw_frame(&app, &mut ui, 120, 26);
    let h_rect = map_h
        .region_of(HitTarget::InspectorOutline(target_heading_idx))
        .expect("selected outline heading must be visible & clickable");

    app.dispatch(
        Action::MouseClick {
            col: h_rect.x + 2,
            row: h_rect.y,
        },
        &mut ui,
    );
    assert_eq!(ui.focus, Focus::Canvas);
    let jumped_block = &app.active_note.as_ref().unwrap().parsed.blocks[ui.canvas_selected_block];
    assert!(jumped_block.line_no() >= target_line);
}

#[test]
fn p1_indented_task_checkbox_and_missing_image_index_alignment() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("Attachments")).unwrap();
    fs::write(tmp.path().join("Attachments/real.png"), make_valid_png_bytes()).unwrap();
    fs::write(
        tmp.path().join("TasksAndImages.md"),
        "# TasksAndImages\n\n    - [ ] Indented task\n\n![[missing.png]]\n\n![[real.png]]\n",
    )
    .unwrap();

    let (mut app, mut ui) = make_harness(tmp.path(), "TaskImgVault");
    app.open_note(&mut ui, "TasksAndImages.md", false);
    ui.focus = Focus::Sidebar;
    let hitmap = draw_frame(&app, &mut ui, 120, 34);

    // 1. Indented task checkbox (indent=4): checkbox is at x_off = 6..8, well past old width 6!
    let task_line_no = app.active_note.as_ref().unwrap().parsed.tasks[0].line_no;
    let chk_rect = hitmap
        .region_of(HitTarget::TaskCheckbox {
            line_no: task_line_no,
        })
        .expect("indented task checkbox hit target");
    assert!(
        chk_rect.width >= 10,
        "expected hit width >= indent(4)+6=10, got {}",
        chk_rect.width
    );
    // Clicking right on the indented checkbox (`x + 7`) toggles it AND focuses Canvas!
    app.dispatch(
        Action::MouseClick {
            col: chk_rect.x + 7,
            row: chk_rect.y,
        },
        &mut ui,
    );
    assert_eq!(ui.focus, Focus::Canvas);
    assert!(app.active_note.as_ref().unwrap().parsed.tasks[0].checked);

    // 2. Missing image must NOT shift `real.png`'s ImageChip index (must be ImageChip(0) matching detail.images[0]).
    assert_eq!(app.active_note.as_ref().unwrap().images.len(), 1);
    assert!(hitmap.region_of(HitTarget::ImageChip(0)).is_some());
    assert!(
        hitmap.region_of(HitTarget::ImageChip(1)).is_none(),
        "missing image must not shift real.png to ImageChip(1)"
    );

    // 3. Sidebar Tasks tab checkbox click toggles task directly.
    app.dispatch(Action::SetSidebarTab(SidebarTab::Tasks), &mut ui);
    let task_map = draw_frame(&app, &mut ui, 120, 34);
    let sb_chk = task_map
        .region_of(HitTarget::SidebarTaskCheckbox(0))
        .expect("sidebar task checkbox");
    app.dispatch(
        Action::MouseClick {
            col: sb_chk.x + 1,
            row: sb_chk.y,
        },
        &mut ui,
    );
    assert!(!app.active_note.as_ref().unwrap().parsed.tasks[0].checked);
}

#[test]
fn p1_switch_vault_resets_state_and_core_event_ignores_other_vault() {
    let tmp = tempfile::tempdir().unwrap();
    let v1_dir = tmp.path().join("VaultOne");
    let v2_dir = tmp.path().join("VaultTwo");
    fs::create_dir_all(v1_dir.join("Sub")).unwrap();
    fs::create_dir_all(&v2_dir).unwrap();
    fs::write(v1_dir.join("Sub/One.md"), "# One\n\n#alpha\n").unwrap();
    fs::write(v2_dir.join("Two.md"), "# Two\n\nHello vault 2.\n").unwrap();

    let (mut app, mut ui) = make_harness(&v1_dir, "VaultOne");
    let v2 = vault_from_dir(&v2_dir, Some("VaultTwo"));
    app.vaults.push(v2.clone());

    ui.active_tag_filter = Some("alpha".into());
    ui.collapsed_folders.insert("Sub".into());
    ui.sidebar_selected = 5;
    ui.canvas_selected_block = 3;
    ui.canvas_scroll = 12;
    ui.inspector_selected = 4;
    ui.graph_selected = 2;

    // CoreEvent for another vault must NOT overwrite last_index_stats
    let foreign_stats = IndexStats {
        vault_id: "unrelated-vault-id".into(),
        vault_name: "Other".into(),
        total_notes: 999,
        ..IndexStats::default()
    };
    app.handle_core_event(CoreEvent::VaultIndexed { stats: foreign_stats }, &mut ui);
    assert!(app.last_index_stats.is_none());

    // SwitchVault clears filters, collapsed folders, and resets selections/scroll!
    app.dispatch(Action::SwitchVault(v2.id.clone()), &mut ui);
    assert_eq!(app.active_vault.id, v2.id);
    assert!(ui.active_tag_filter.is_none());
    assert!(ui.collapsed_folders.is_empty());
    assert_eq!(ui.sidebar_selected, 0);
    assert_eq!(ui.canvas_selected_block, 0);
    assert_eq!(ui.canvas_scroll, 0);
    assert_eq!(ui.inspector_selected, 0);
    assert_eq!(ui.graph_selected, 0);
}

#[test]
fn p1_image_settings_preserve_cell_px_and_populate_inline_cache() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("Attachments")).unwrap();
    fs::write(tmp.path().join("Attachments/pic.png"), make_valid_png_bytes()).unwrap();
    fs::write(tmp.path().join("Pic.md"), "# Pic\n\n![[pic.png]]\n").unwrap();

    let (mut app, mut ui) = make_harness(tmp.path(), "ImgSettingsVault");
    app.cfg.inline_images = false;
    app.preview_cache.clear();
    app.graphics_caps.cell_px = (9, 18);
    app.graphics_caps.cell_px_exact = true;

    // Turning inline_images ON populates preview_cache for active_note
    app.dispatch(Action::ToggleInlineImages, &mut ui);
    assert!(app.cfg.inline_images);
    assert!(!app.preview_cache.is_empty());

    // SetImageProtocol preserves probed cell_px/cell_px_exact and re-populates inline cache
    app.dispatch(
        Action::SetImageProtocol(ImageProtocolPreference::Halfblock),
        &mut ui,
    );
    assert_eq!(app.graphics_caps.protocol, TerminalImageProtocol::Halfblock);
    assert_eq!(app.graphics_caps.cell_px, (9, 18));
    assert!(app.graphics_caps.cell_px_exact);
    assert!(!app.preview_cache.is_empty());
}

#[test]
fn p1_inline_editor_focus_switch_and_backlink_bookmark_freshness() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("NoteA.md"), "# NoteA\n\nInitial text.\n").unwrap();
    fs::write(tmp.path().join("NoteB.md"), "# NoteB\n\nPlain.\n").unwrap();
    let (mut app, mut ui) = make_harness(tmp.path(), "InlineVault");
    app.open_note(&mut ui, "NoteA.md", false);

    // Start inline edit and make it dirty
    app.dispatch(Action::OpenNoteEditor, &mut ui);
    assert!(ui.inline_editor.is_some());
    app.dispatch(Action::Input('!'), &mut ui);
    assert!(ui.inline_editor.as_ref().unwrap().dirty);

    // External bookmark + new incoming backlink from NoteB while inline editor is dirty
    app.storage
        .toggle_bookmark(&app.active_vault.id, "NoteA.md")
        .unwrap();
    fs::write(tmp.path().join("NoteB.md"), "# NoteB\n\nSee [[NoteA]].\n").unwrap();
    app.storage.sync_vault(&app.active_vault).unwrap();
    app.reload_from_storage(&mut ui);

    let active = app.active_note.as_ref().unwrap();
    assert!(active.summary.is_bookmarked);
    assert_eq!(active.backlinks.len(), 1);
    assert!(ui.inline_editor.as_ref().unwrap().dirty);

    // Clicking Sidebar pane flushes & exits inline editor cleanly before moving focus
    let map = draw_frame(&app, &mut ui, 120, 34);
    let sb_rect = map
        .region_of(HitTarget::Pane(Focus::Sidebar))
        .expect("sidebar pane");
    app.dispatch(
        Action::MouseClick {
            col: sb_rect.x + 1,
            row: sb_rect.bottom().saturating_sub(1),
        },
        &mut ui,
    );
    assert!(ui.inline_editor.is_none());
    assert_eq!(ui.focus, Focus::Sidebar);
}

#[test]
fn p1_overlay_mouse_wheel_help_clamp_and_pane_paging() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..15 {
        fs::write(tmp.path().join(format!("N{i:02}.md")), format!("# N{i:02}\n")).unwrap();
    }
    let (mut app, mut ui) = make_harness(tmp.path(), "ScrollVault");

    // PageDown/PageUp/JumpBottom/JumpTop respect Sidebar focus
    ui.focus = Focus::Sidebar;
    ui.sidebar_selected = 0;
    app.dispatch(Action::PageDown, &mut ui);
    assert_eq!(ui.sidebar_selected, 10);
    app.dispatch(Action::JumpBottom, &mut ui);
    assert_eq!(ui.sidebar_selected, 14);
    app.dispatch(Action::JumpTop, &mut ui);
    assert_eq!(ui.sidebar_selected, 0);

    // Help overlay clamps scroll and supports PageDown/PageUp/End/Home
    app.dispatch(Action::OpenOverlay(OverlayKind::Help), &mut ui);
    app.dispatch(Action::CursorEnd, &mut ui);
    draw_frame(&app, &mut ui, 100, 28);
    let clamped_after_end = match ui.overlay {
        Some(Overlay::Help { scroll }) => scroll,
        _ => panic!("expected Help overlay"),
    };
    app.dispatch(Action::MouseScrollDown { col: 50, row: 14 }, &mut ui);
    draw_frame(&app, &mut ui, 100, 28);
    if let Some(Overlay::Help { scroll }) = ui.overlay {
        assert_eq!(scroll, clamped_after_end, "Help scroll must clamp to max");
    }
    app.dispatch(Action::CursorHome, &mut ui);
    assert!(matches!(ui.overlay, Some(Overlay::Help { scroll: 0 })));
    app.dispatch(Action::CloseOverlay, &mut ui);

    // Mouse wheel works in QuickSwitcher and Settings overlays
    app.dispatch(Action::OpenOverlay(OverlayKind::Settings), &mut ui);
    app.dispatch(Action::MouseScrollDown { col: 50, row: 14 }, &mut ui);
    assert!(matches!(
        ui.overlay,
        Some(Overlay::Settings(ref s)) if s.selected == 1
    ));
    app.dispatch(Action::MouseScrollUp { col: 50, row: 14 }, &mut ui);
    assert!(matches!(
        ui.overlay,
        Some(Overlay::Settings(ref s)) if s.selected == 0
    ));
}

#[test]
fn test_note_mode_mutations_skip_graph_rebuild_html_walk_and_full_vault_rescan() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("Alpha.md"),
        "# Alpha\n\nLinks to [[Beta]].\n\n- [ ] Ship fix\n",
    )
    .unwrap();
    fs::write(tmp.path().join("Beta.md"), "# Beta\n\nTarget.\n").unwrap();
    fs::write(tmp.path().join("Untouched.md"), "# Untouched\n\nSteady.\n").unwrap();

    let (mut app, mut ui) = make_harness(tmp.path(), "LatencyVault");
    assert_eq!(ui.canvas_mode, CanvasMode::Note);
    assert!(
        app.graph.nodes.is_empty(),
        "graph must remain lazy (unbuilt) while in CanvasMode::Note"
    );

    // Open Alpha.md in Note mode: graph stays unbuilt until Graph View is toggled on.
    app.open_note(&mut ui, "Alpha.md", false);
    assert!(app.graph.nodes.is_empty());

    // Mark Untouched.md stale in SQLite and drop a new HTML file on disk.
    app.storage
        .invalidate_note_mtime(&app.active_vault.id, "Untouched.md")
        .unwrap();
    fs::write(tmp.path().join("late.html"), "<h1>late</h1>").unwrap();

    // 1. ToggleBookmark updates active_note + notes in-place (no HTML walk, no full vault scan).
    app.dispatch(Action::ToggleBookmark, &mut ui);
    assert!(app.active_note.as_ref().unwrap().summary.is_bookmarked);
    assert!(
        app.notes
            .iter()
            .find(|n| n.rel_path == "Alpha.md")
            .unwrap()
            .is_bookmarked
    );
    assert!(
        app.html_files.is_empty(),
        "ToggleBookmark must not trigger scan_vault_html_files"
    );

    // 2. QuickAppendSubmit, ToggleTaskAtLine, CreateNoteSubmit, DeleteActiveNoteConfirmed
    //    must use path-scoped sync_paths and skip HTML scanning & graph rebuilds.
    app.dispatch(
        Action::QuickAppendSubmit {
            text: "Appended line".into(),
        },
        &mut ui,
    );
    let task_line = app.active_note.as_ref().unwrap().parsed.tasks[0].line_no;
    app.dispatch(
        Action::ToggleTaskAtLine {
            note_path: "Alpha.md".into(),
            line_no: task_line,
        },
        &mut ui,
    );
    assert!(app.active_note.as_ref().unwrap().parsed.tasks[0].checked);

    app.dispatch(
        Action::CreateNoteSubmit {
            title: "Gamma".into(),
            folder: String::new(),
            tags: "perf".into(),
        },
        &mut ui,
    );
    assert_eq!(
        app.active_note.as_ref().unwrap().summary.rel_path,
        "Gamma.md"
    );
    app.dispatch(Action::DeleteActiveNoteConfirmed, &mut ui);
    assert!(!tmp.path().join("Gamma.md").exists());

    assert!(
        app.html_files.is_empty(),
        "Markdown-only mutations must not trigger full-vault scan_vault_html_files"
    );
    assert!(
        app.graph.nodes.is_empty(),
        "graph must not rebuild while CanvasMode::Note is active"
    );
    assert_eq!(
        app.storage
            .get_note_summary(&app.active_vault.id, "Untouched.md")
            .unwrap()
            .unwrap()
            .mtime_ms,
        -1,
        "single-note TUI mutations must not run full-vault sync_vault"
    );

    // 3. Entering Graph View builds the graph on demand.
    app.dispatch(Action::ToggleGraphView, &mut ui);
    assert_eq!(ui.canvas_mode, CanvasMode::Graph);
    assert!(app.graph.nodes.len() >= 2);
}

#[tokio::test]
async fn test_refresh_all_and_switch_vault_delegate_to_live_watcher_without_blocking() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let tmp = tempfile::tempdir().unwrap();
    let v1_dir = tmp.path().join("V1");
    let v2_dir = tmp.path().join("V2");
    fs::create_dir_all(&v1_dir).unwrap();
    fs::create_dir_all(&v2_dir).unwrap();
    fs::write(v1_dir.join("One.md"), "# One\n").unwrap();
    fs::write(v2_dir.join("Two.md"), "# Two\n").unwrap();

    let v1 = vault_from_dir(&v1_dir, Some("V1"));
    let v2 = vault_from_dir(&v2_dir, Some("V2"));
    let cfg = AppConfig {
        load_failed: true,
        ..AppConfig::default()
    };
    let storage = Storage::open_in_memory().unwrap();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let (sync_ctrl, diag) = crate::sync::spawn_vault_watcher_with_options(
        v1.clone(),
        storage.clone(),
        2_500,
        ev_tx,
        crate::sync::WatcherOptions::default(),
    );

    let mut app = AppState::new(cfg.clone(), v1.clone(), storage, sync_ctrl);
    app.vaults.push(v2.clone());
    let mut ui = UiState::from_config(&cfg);

    // Wait for the single initial background sync of V1.
    let init_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
        .await
        .unwrap()
        .unwrap();
    app.handle_core_event(init_ev, &mut ui);
    assert_eq!(app.notes.len(), 1);
    assert_eq!(diag.initial_scans.load(Ordering::Acquire), 1);

    // 1. Action::RefreshAll delegates to background watcher instead of blocking UI thread.
    let full_before = diag.post_init_full_scans.load(Ordering::Acquire);
    app.dispatch(Action::RefreshAll, &mut ui);
    let reindex_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
        .await
        .expect("watcher must complete background RefreshAll")
        .unwrap();
    app.handle_core_event(reindex_ev, &mut ui);
    assert_eq!(
        diag.post_init_full_scans.load(Ordering::Acquire),
        full_before + 1
    );

    // 2. Action::SwitchVault delegates to background watcher (single scan, no duplicate sync_vault).
    app.dispatch(Action::SwitchVault(v2.id.clone()), &mut ui);
    let switch_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
        .await
        .expect("watcher must complete background SwitchVault sync")
        .unwrap();
    app.handle_core_event(switch_ev, &mut ui);
    assert_eq!(app.active_vault.id, v2.id);
    assert_eq!(app.notes.len(), 1);
    assert_eq!(app.notes[0].rel_path, "Two.md");
    assert_eq!(diag.initial_scans.load(Ordering::Acquire), 2);
}

#[test]
fn test_ctrl_m_toggles_live_preview_and_source_mode() {
    use crate::ui::inline_editor::EditorMode;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("ModeNote.md"),
        "# Mode Heading\n\nSome **bold** text.\n",
    )
    .unwrap();
    let (mut app, mut ui) = make_harness(tmp.path(), "ModeVault");
    app.open_note(&mut ui, "ModeNote.md", false);

    // Pressing `i` enters Rendered WYSIWYG mode immediately (new default)
    let act_i = key_to_action(
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
        ui.focus,
        ui.canvas_mode,
        &ui.keymap,
        ui.is_text_input_active(),
    );
    assert_eq!(act_i, Some(Action::OpenNoteEditor));
    app.dispatch(act_i.unwrap(), &mut ui);
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().mode,
        EditorMode::Rendered
    );

    // Ctrl+M maps to Action::ToggleEditorMode and cycles Rendered -> LivePreview -> Source -> Rendered
    let act_m = key_to_action(
        KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
        ui.focus,
        ui.canvas_mode,
        &ui.keymap,
        ui.is_text_input_active(),
    );
    assert_eq!(act_m, Some(Action::ToggleEditorMode));
    app.dispatch(act_m.unwrap(), &mut ui);
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().mode,
        EditorMode::LivePreview
    );

    app.dispatch(Action::ToggleEditorMode, &mut ui);
    assert_eq!(ui.inline_editor.as_ref().unwrap().mode, EditorMode::Source);

    // Clicking the `[Src]` pill in the inline header bar cycles back to Rendered
    let hitmap = draw_frame(&app, &mut ui, 120, 30);
    let pill = hitmap
        .region_of(HitTarget::Button(ButtonId::ToggleEditorMode))
        .expect("mode toggle pill must be registered in hitmap");
    app.dispatch(
        Action::MouseClick {
            col: pill.x + 1,
            row: pill.y,
        },
        &mut ui,
    );
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().mode,
        EditorMode::Rendered
    );
}

#[test]
fn test_click_rendered_note_line_and_enter_launch_live_preview_without_breaking_direct_actions()
{
    use crate::ui::inline_editor::EditorMode;

    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("ClickNote.md"),
        "# ClickNote\n\nPlain editable paragraph line\n\n- [ ] Direct toggle task\n\nSee [[TargetNote]] link\n",
    )
    .unwrap();
    fs::write(tmp.path().join("TargetNote.md"), "# TargetNote\n\nLinked!\n").unwrap();

    let (mut app, mut ui) = make_harness(tmp.path(), "ClickVault");
    app.open_note(&mut ui, "ClickNote.md", false);
    ui.focus = Focus::Canvas;

    // 1. Clicking an ordinary rendered paragraph immediately enters LivePreview at that line/col
    let hitmap = draw_frame(&app, &mut ui, 120, 30);
    let para_idx = app
        .active_note
        .as_ref()
        .unwrap()
        .parsed
        .blocks
        .iter()
        .position(|b| {
            matches!(b, NoteBlock::Paragraph { spans, .. } if spans.iter().any(|s| s.plain_text().contains("Plain editable")))
        })
        .expect("paragraph block");
    let para_rect = hitmap
        .region_of(HitTarget::CanvasBlock(para_idx))
        .expect("paragraph CanvasBlock rect");
    app.dispatch(
        Action::MouseClick {
            col: para_rect.x + 6,
            row: para_rect.y,
        },
        &mut ui,
    );
    let ed = ui
        .inline_editor
        .as_ref()
        .expect("clicking rendered paragraph must enter inline editor immediately");
    assert_eq!(ed.mode, EditorMode::Rendered);
    let (ln, col) = ed.cursor_line_col();
    assert_eq!(ln, 3, "cursor must land on paragraph source line 3");
    assert!(col >= 5, "cursor col should land near clicked column, got {col}");

    // Esc cleanly exits inline edit back to Canvas navigation
    app.dispatch(Action::FinishInlineEdit, &mut ui);
    assert!(ui.inline_editor.is_none());

    // 2. Pressing Enter on an ordinary text block enters Rendered mode immediately
    ui.focus = Focus::Canvas;
    ui.canvas_selected_block = para_idx;
    app.dispatch(Action::Activate, &mut ui);
    assert!(ui.inline_editor.is_some());
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().mode,
        EditorMode::Rendered
    );
    app.dispatch(Action::FinishInlineEdit, &mut ui);
    assert!(ui.inline_editor.is_none());

    // 3. Direct click on checkbox STILL toggles task without entering inline editor
    let hitmap2 = draw_frame(&app, &mut ui, 120, 30);
    let task_ln = app.active_note.as_ref().unwrap().parsed.tasks[0].line_no;
    let cb_rect = hitmap2
        .region_of(HitTarget::TaskCheckbox { line_no: task_ln })
        .expect("task checkbox rect");
    app.dispatch(
        Action::MouseClick {
            col: cb_rect.x + 2,
            row: cb_rect.y,
        },
        &mut ui,
    );
    assert!(ui.inline_editor.is_none());
    assert!(app.active_note.as_ref().unwrap().parsed.tasks[0].checked);

    // 4. Direct click on wikilink block STILL navigates to TargetNote without entering inline editor
    let hitmap3 = draw_frame(&app, &mut ui, 120, 30);
    let wl_idx = app
        .active_note
        .as_ref()
        .unwrap()
        .parsed
        .blocks
        .iter()
        .position(|b| b.first_wikilink().is_some())
        .expect("wikilink block");
    let wl_rect = hitmap3
        .region_of(HitTarget::WikilinkBlock(wl_idx))
        .expect("wikilink rect");
    app.dispatch(
        Action::MouseClick {
            col: wl_rect.x + 4,
            row: wl_rect.y,
        },
        &mut ui,
    );
    assert!(ui.inline_editor.is_none());
    assert_eq!(
        app.active_note.as_ref().unwrap().summary.rel_path,
        "TargetNote.md"
    );
}

#[test]
fn test_esc_after_live_edit_keeps_typed_text_visible_across_long_notes_frontmatter_and_lists() {
    let tmp = tempfile::tempdir().unwrap();
    let mut long_md = String::from("---\ntags: [work]\n---\n# Long Note\n\n");
    for i in 1..=25 {
        long_md.push_str(&format!("Paragraph line {i}\n\n"));
    }
    long_md.push_str("- Parent item\n  - Nested child\n\n  Continuation paragraph after nested list\n");
    fs::write(tmp.path().join("Long.md"), &long_md).unwrap();

    let (mut app, mut ui) = make_harness(tmp.path(), "LongVault");
    app.open_note(&mut ui, "Long.md", false);

    // 1. Continuation paragraph inside list item after nested list is NOT dropped by parser.
    let body_start_line = {
        let detail = app.active_note.as_ref().unwrap();
        assert!(
            detail.parsed.body_plain.contains("Continuation paragraph after nested list"),
            "continuation paragraph after nested list must not be dropped"
        );
        detail.parsed.body_start_line
    };

    // 2. Pressing `e`/`i` (`OpenNoteEditor`) on a note with YAML frontmatter lands cursor on
    //    the first body line (AFTER closing `---`), NOT on line 0 inside frontmatter.
    app.dispatch(Action::OpenNoteEditor, &mut ui);
    let (ln_on_open, _) = ui.inline_editor.as_ref().unwrap().cursor_line_col();
    assert_eq!(
        ln_on_open, body_start_line,
        "cursor must start at first body line, not inside YAML frontmatter"
    );

    // 3. Typing at the bottom of the long note and pressing Esc scrolls `note_view` to keep
    //    the newly typed text visible on screen immediately (never clipped below viewport).
    if let Some(ref mut ed) = ui.inline_editor {
        ed.composer.move_doc_end(false);
    }
    app.dispatch(Action::Paste("\n\nTypedAtBottomVisibleOnEsc".into()), &mut ui);
    app.dispatch(Action::FinishInlineEdit, &mut ui);
    assert!(ui.inline_editor.is_none());
    assert!(ui.canvas_follow_selected);

    let backend = ratatui::backend::TestBackend::new(100, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            let _ = crate::ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    let mut rendered = String::new();
    for y in 0..24 {
        for x in 0..100 {
            rendered.push_str(buf[(x, y)].symbol());
        }
        rendered.push('\n');
    }
    assert!(
        rendered.contains("TypedAtBottomVisibleOnEsc"),
        "newly typed text at bottom of note must remain visible immediately after Esc, got:\n{rendered}"
    );
}

fn sidebar_row_label(app: &AppState, ui: &UiState) -> String {
    match build_sidebar_entries(app, ui).get(ui.sidebar_selected) {
        Some(SidebarEntry::Folder { path, expanded, .. }) => {
            format!("F {path} {}", if *expanded { "open" } else { "closed" })
        }
        Some(SidebarEntry::Note { rel_path, .. }) => format!("N {rel_path}"),
        other => format!("{other:?}"),
    }
}

fn sidebar_row_of(app: &AppState, ui: &UiState, path: &str) -> usize {
    build_sidebar_entries(app, ui)
        .iter()
        .position(|e| match e {
            SidebarEntry::Folder { path: p, .. } => p == path,
            SidebarEntry::Note { rel_path, .. } => rel_path == path,
            _ => false,
        })
        .unwrap_or_else(|| panic!("no sidebar row for {path}"))
}

#[test]
fn p0_space_in_files_sidebar_toggles_folder_and_never_writes_open_notes_task() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("Projects")).unwrap();
    fs::write(tmp.path().join("Projects/Plan.md"), "# Plan\n\nBody\n").unwrap();
    let todo_src = "# Todo\n\n- [ ] Ship it\n";
    let todo_path = tmp.path().join("Todo.md");
    fs::write(&todo_path, todo_src).unwrap();
    let (mut app, mut ui) = make_harness(tmp.path(), "SpaceVault");
    app.open_note(&mut ui, "Todo.md", false);

    // The canvas selection rests on the task block: exactly the state that used to let a
    // sidebar Space fall through and flip this checkbox on disk.
    let task_block = app
        .active_note
        .as_ref()
        .unwrap()
        .parsed
        .blocks
        .iter()
        .position(|b| matches!(b, NoteBlock::Task { .. }))
        .expect("task block");
    ui.canvas_selected_block = task_block;
    ui.canvas_mode = CanvasMode::Note;
    ui.focus = Focus::Sidebar;
    ui.sidebar_tab = SidebarTab::Files;
    ui.sidebar_selected = sidebar_row_of(&app, &ui, "Projects");
    assert_eq!(sidebar_row_label(&app, &ui), "F Projects open");

    let space = key_to_action(
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        ui.focus,
        ui.canvas_mode,
        &ui.keymap,
        false,
    )
    .expect("Space maps to an action");
    assert_eq!(space, Action::ToggleSelectedTask);
    let task_checked =
        |app: &AppState| app.active_note.as_ref().unwrap().parsed.tasks[0].checked;

    // Space on a folder row collapses, then re-expands, that folder only.
    app.dispatch(space.clone(), &mut ui);
    assert!(ui.collapsed_folders.contains("Projects"));
    assert_eq!(sidebar_row_label(&app, &ui), "F Projects closed");
    assert_eq!(fs::read_to_string(&todo_path).unwrap(), todo_src);
    assert!(!task_checked(&app));
    app.dispatch(space.clone(), &mut ui);
    assert!(ui.collapsed_folders.is_empty());
    assert_eq!(fs::read_to_string(&todo_path).unwrap(), todo_src);

    // Space on a file row is a no-op.
    ui.sidebar_selected = sidebar_row_of(&app, &ui, "Todo.md");
    app.dispatch(space.clone(), &mut ui);
    assert!(ui.collapsed_folders.is_empty());
    assert_eq!(fs::read_to_string(&todo_path).unwrap(), todo_src);

    // Other sidebar tabs, the inspector and the graph canvas never reach the note either.
    for tab in [SidebarTab::Tags, SidebarTab::Bookmarks, SidebarTab::Outline] {
        ui.sidebar_tab = tab;
        ui.sidebar_selected = 0;
        app.dispatch(Action::ToggleSelectedTask, &mut ui);
        assert_eq!(fs::read_to_string(&todo_path).unwrap(), todo_src, "{tab:?}");
    }
    ui.focus = Focus::Inspector;
    app.dispatch(Action::ToggleSelectedTask, &mut ui);
    ui.focus = Focus::Canvas;
    ui.canvas_mode = CanvasMode::Graph;
    app.dispatch(Action::ToggleSelectedTask, &mut ui);
    assert_eq!(fs::read_to_string(&todo_path).unwrap(), todo_src);
    assert!(!task_checked(&app));

    // With the note canvas focused, Space still toggles the selected task.
    ui.canvas_mode = CanvasMode::Note;
    ui.canvas_selected_block = task_block;
    app.dispatch(space, &mut ui);
    assert!(fs::read_to_string(&todo_path)
        .unwrap()
        .contains("- [x] Ship it"));
    assert!(task_checked(&app));
}

#[test]
fn p1_left_right_arrows_walk_the_files_tree() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("Projects/Sub")).unwrap();
    fs::write(tmp.path().join("Projects/p.md"), "# p\n").unwrap();
    fs::write(tmp.path().join("Projects/Sub/s.md"), "# s\n").unwrap();
    fs::write(tmp.path().join("root.md"), "# root\n").unwrap();
    let (mut app, mut ui) = make_harness(tmp.path(), "ArrowVault");
    ui.focus = Focus::Sidebar;
    ui.sidebar_tab = SidebarTab::Files;

    let key = |code| {
        key_to_action(
            KeyEvent::new(code, KeyModifiers::NONE),
            Focus::Sidebar,
            CanvasMode::Note,
            &KeyMap,
            false,
        )
        .unwrap()
    };
    let (left, right) = (key(KeyCode::Left), key(KeyCode::Right));
    assert_eq!((&left, &right), (&Action::MoveLeft, &Action::MoveRight));

    // Left: file -> its folder, expanded folder -> collapsed, collapsed folder -> parent.
    ui.sidebar_selected = sidebar_row_of(&app, &ui, "Projects/Sub/s.md");
    let mut walk = Vec::new();
    for _ in 0..5 {
        app.dispatch(left.clone(), &mut ui);
        walk.push(sidebar_row_label(&app, &ui));
    }
    assert_eq!(
        walk,
        [
            "F Projects/Sub open",
            "F Projects/Sub closed",
            "F Projects open",
            "F Projects closed",
            // A top-level row has no parent: Left stays put.
            "F Projects closed",
        ]
    );

    // Right: collapsed folder -> expanded, expanded folder -> first child, file -> opened.
    app.dispatch(right.clone(), &mut ui);
    assert_eq!(sidebar_row_label(&app, &ui), "F Projects open");
    app.dispatch(right.clone(), &mut ui);
    assert_eq!(sidebar_row_label(&app, &ui), "N Projects/p.md");
    app.dispatch(left.clone(), &mut ui);
    assert_eq!(sidebar_row_label(&app, &ui), "F Projects open");

    app.dispatch(Action::MoveDown, &mut ui);
    app.dispatch(Action::MoveDown, &mut ui);
    assert_eq!(sidebar_row_label(&app, &ui), "F Projects/Sub closed");
    app.dispatch(right.clone(), &mut ui);
    assert_eq!(sidebar_row_label(&app, &ui), "F Projects/Sub open");
    app.dispatch(right.clone(), &mut ui);
    assert_eq!(sidebar_row_label(&app, &ui), "N Projects/Sub/s.md");

    // Root-level file: Left has no parent to go to.
    app.dispatch(Action::JumpBottom, &mut ui);
    assert_eq!(sidebar_row_label(&app, &ui), "N root.md");
    app.dispatch(left.clone(), &mut ui);
    assert_eq!(sidebar_row_label(&app, &ui), "N root.md");

    // Arrows outside the Files tab leave the tree alone.
    ui.sidebar_selected = sidebar_row_of(&app, &ui, "Projects");
    for (focus, tab) in [
        (Focus::Canvas, SidebarTab::Files),
        (Focus::Sidebar, SidebarTab::Tags),
    ] {
        ui.focus = focus;
        ui.sidebar_tab = tab;
        app.dispatch(left.clone(), &mut ui);
        assert!(ui.collapsed_folders.is_empty(), "{focus:?} {tab:?}");
    }
    ui.focus = Focus::Sidebar;
    ui.sidebar_tab = SidebarTab::Files;

    // Right on a file opens it and moves focus to the canvas.
    ui.sidebar_selected = sidebar_row_of(&app, &ui, "Projects/Sub/s.md");
    app.dispatch(right, &mut ui);
    assert_eq!(
        app.active_note.as_ref().unwrap().summary.rel_path,
        "Projects/Sub/s.md"
    );
    assert_eq!(ui.focus, Focus::Canvas);
}

#[test]
fn p1_deeply_nested_tree_rows_keep_chevron_and_name_on_screen() {
    let tmp = tempfile::tempdir().unwrap();
    let deep_dir = tmp.path().join("A/B/C/D/E/F/G/H");
    fs::create_dir_all(&deep_dir).unwrap();
    fs::write(deep_dir.join("DeepNote.md"), "# DeepNote\n").unwrap();
    let (app, mut ui) = make_harness(tmp.path(), "DeepVault");
    ui.show_sidebar = true;
    ui.sidebar_tab = SidebarTab::Files;

    let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
    terminal
        .draw(|f| {
            let _ = ui::draw(f, &app, &mut ui);
        })
        .unwrap();
    // 120 cols -> 24-col sidebar whose right border sits at x = 23.
    let buf = terminal.backend().buffer();
    let rows: Vec<String> = (0..34u16)
        .map(|y| (0..23u16).map(|x| buf[(x, y)].symbol()).collect())
        .collect();
    let sidebar = rows.join("\n");
    assert!(
        rows.iter().any(|r| r.contains("▾ H (1)")),
        "deepest folder chevron, name and count must stay visible:\n{sidebar}"
    );
    assert!(
        rows.iter().any(|r| r.contains("▾ A (1)")),
        "parent-only top folder counts its descendant note:\n{sidebar}"
    );
    assert!(
        rows.iter().any(|r| r.contains("DeepNote")),
        "depth-8 note title must not be clipped:\n{sidebar}"
    );
}

#[test]
fn p1_nested_tag_rows_synthesize_parents_and_match_filter_counts() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("One.md"), "# One\n\n#a/b/c\n").unwrap();
    fs::write(tmp.path().join("Two.md"), "# Two\n\n#a/b #a/b/d\n").unwrap();
    fs::write(tmp.path().join("Three.md"), "# Three\n\n#a/x #other\n").unwrap();
    fs::write(tmp.path().join("Four.md"), "# Four\n\n#other\n").unwrap();
    let (mut app, mut ui) = make_harness(tmp.path(), "NestedTagVault");
    app.dispatch(Action::SetSidebarTab(SidebarTab::Tags), &mut ui);

    let tag_rows = |app: &AppState, ui: &UiState| -> Vec<(String, usize)> {
        build_sidebar_entries(app, ui)
            .into_iter()
            .filter_map(|e| match e {
                SidebarEntry::Tag { tag, count, .. } => Some((tag, count)),
                _ => None,
            })
            .collect()
    };
    let rows = tag_rows(&app, &ui);
    assert_eq!(
        rows,
        [
            ("a".to_string(), 3),
            ("a/b".to_string(), 2),
            ("other".to_string(), 2),
            ("a/b/c".to_string(), 1),
            ("a/b/d".to_string(), 1),
            ("a/x".to_string(), 1),
        ]
    );

    // Selecting any row lists exactly as many notes as that row displayed.
    for (idx, (tag, count)) in rows.iter().enumerate() {
        ui.sidebar_selected = idx;
        app.dispatch(Action::Activate, &mut ui);
        let entries = build_sidebar_entries(&app, &ui);
        assert_eq!(
            entries.first(),
            Some(&SidebarEntry::TagHeader {
                tag: tag.clone(),
                count: *count,
            }),
            "#{tag}"
        );
        let listed = entries
            .iter()
            .filter(|e| matches!(e, SidebarEntry::Note { .. }))
            .count();
        assert_eq!(listed, *count, "#{tag}");
        app.dispatch(Action::ClearTagFilter, &mut ui);
    }
}

#[test]
fn p1_short_inspector_hides_outline_from_keyboard_selection() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("Doc.md"),
        "# Doc\n\n## One\n\nSee [[Other]].\n\n## Two\n\nText.\n",
    )
    .unwrap();
    fs::write(tmp.path().join("Other.md"), "# Other\n\nBack to [[Doc]].\n").unwrap();
    let (mut app, mut ui) = make_harness(tmp.path(), "ShortInspectorVault");
    app.open_note(&mut ui, "Doc.md", false);
    let detail = app.active_note.as_ref().unwrap();
    let links = detail.backlinks.len() + detail.outgoing_links.len();
    let headings = detail.parsed.headings.len();
    assert!(links > 0 && headings > 0);
    ui.show_inspector = true;
    ui.focus = Focus::Inspector;

    // Tall terminal: the Outline is shown and its headings are selectable.
    let tall = draw_frame(&app, &mut ui, 120, 34);
    assert!(ui.inspector_outline_visible);
    assert!(tall.region_of(HitTarget::InspectorOutline(0)).is_some());
    app.dispatch(Action::JumpBottom, &mut ui);
    assert_eq!(ui.inspector_selected, links + headings - 1);

    // Short terminal (inspector < 18 rows): the Outline is hidden, so a selection inside it
    // is pulled back onto the last link and navigation can no longer reach the headings.
    let short = draw_frame(&app, &mut ui, 120, 18);
    assert!(!ui.inspector_outline_visible);
    assert!(short.region_of(HitTarget::InspectorOutline(0)).is_none());
    assert_eq!(ui.inspector_selected, links - 1);
    for action in [Action::JumpBottom, Action::MoveDown, Action::PageDown] {
        app.dispatch(action.clone(), &mut ui);
        assert_eq!(ui.inspector_selected, links - 1, "{action:?}");
    }

    // Growing the terminal again brings the headings back.
    draw_frame(&app, &mut ui, 120, 34);
    assert!(ui.inspector_outline_visible);
    app.dispatch(Action::JumpBottom, &mut ui);
    assert_eq!(ui.inspector_selected, links + headings - 1);
}
