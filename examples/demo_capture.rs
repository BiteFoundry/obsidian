//! Captures 8 real `ratatui::TestBackend` frames (120x34) from `obsidian_cli::ui::draw`
//! on a fictional "Acme Second Brain" vault and renders `assets/demo.gif`.
//!
//! The frames are turned into a GIF by an external Python script, `render_tui_gif.py`, which is
//! not part of this repository. Two environment variables say where things are:
//!
//! - `DEMO_RENDERER_SCRIPT`: path of `render_tui_gif.py`. Default: `scripts/render_tui_gif.py`
//!   in this repository (not shipped, so set the variable unless you put the script there).
//! - `DEMO_PREVIEW_DIR`: directory for the per-frame preview PNGs. Default:
//!   `target/demo-previews/obsidian`.
//!
//! Usage:
//!
//! ```text
//! DEMO_RENDERER_SCRIPT=/path/to/render_tui_gif.py DEMO_PREVIEW_DIR=/path/to/previews \
//!     cargo run --example demo_capture
//! ```

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use obsidian_cli::{
    config::{AppConfig, PageWidthMode, ThemePreset, VaultProfile},
    models::{CoreEvent, SidebarTab},
    storage::Storage,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::{
        self,
        action::Action,
        app::{AppState, CanvasMode, Focus, UiState},
        overlays::{
            links::LinkExtractorOverlay, quick_switcher::QuickSwitcherOverlay,
            settings::SettingsOverlay, Overlay,
        },
        theme::Theme,
    },
    vault::vault_from_dir,
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
    Terminal,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 34;

/// A path from the environment, or `fallback` under the repository root when the variable is
/// unset or blank.
fn env_path_or(var: &str, fallback: &[&str]) -> PathBuf {
    match env::var_os(var).filter(|value| !value.to_string_lossy().trim().is_empty()) {
        Some(value) => PathBuf::from(value),
        None => fallback
            .iter()
            .fold(PathBuf::from(env!("CARGO_MANIFEST_DIR")), |path, part| {
                path.join(part)
            }),
    }
}

/// `DEMO_RENDERER_SCRIPT`, else `scripts/render_tui_gif.py` in this repository.
fn renderer_script() -> PathBuf {
    env_path_or("DEMO_RENDERER_SCRIPT", &["scripts", "render_tui_gif.py"])
}

/// `DEMO_PREVIEW_DIR`, else `target/demo-previews/obsidian` (git-ignored build output).
fn preview_dir() -> PathBuf {
    env_path_or("DEMO_PREVIEW_DIR", &["target", "demo-previews", "obsidian"])
}

fn color_to_json(c: Color) -> Value {
    match c {
        Color::Reset => Value::Null,
        Color::Rgb(r, g, b) => json!([r, g, b]),
        Color::Black => json!("Black"),
        Color::Red => json!("Red"),
        Color::Green => json!("Green"),
        Color::Yellow => json!("Yellow"),
        Color::Blue => json!("Blue"),
        Color::Magenta => json!("Magenta"),
        Color::Cyan => json!("Cyan"),
        Color::Gray => json!("Gray"),
        Color::DarkGray => json!("DarkGray"),
        Color::LightRed => json!("LightRed"),
        Color::LightGreen => json!("LightGreen"),
        Color::LightYellow => json!("LightYellow"),
        Color::LightBlue => json!("LightBlue"),
        Color::LightMagenta => json!("LightMagenta"),
        Color::LightCyan => json!("LightCyan"),
        Color::White => json!("White"),
        Color::Indexed(idx) => {
            let (r, g, b) = xterm256_to_rgb(idx);
            json!([r, g, b])
        }
    }
}

fn xterm256_to_rgb(idx: u8) -> (u8, u8, u8) {
    if idx < 16 {
        const ANSI16: [(u8, u8, u8); 16] = [
            (24, 26, 33),
            (247, 118, 142),
            (158, 206, 106),
            (224, 175, 104),
            (122, 162, 247),
            (187, 154, 247),
            (125, 207, 255),
            (169, 177, 214),
            (86, 95, 137),
            (255, 140, 160),
            (175, 224, 128),
            (240, 200, 130),
            (148, 184, 255),
            (205, 176, 255),
            (155, 222, 255),
            (240, 244, 252),
        ];
        return ANSI16[idx as usize];
    }
    if idx >= 232 {
        let v = 8 + (idx - 232) * 10;
        return (v, v, v);
    }
    let n = idx - 16;
    let r = n / 36;
    let g = (n % 36) / 6;
    let b = n % 6;
    let map = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
    (map(r), map(g), map(b))
}

fn modifier_flags(m: Modifier) -> u8 {
    let mut flags = 0u8;
    if m.contains(Modifier::BOLD) {
        flags |= 1;
    }
    if m.contains(Modifier::ITALIC) {
        flags |= 2;
    }
    if m.contains(Modifier::DIM) {
        flags |= 4;
    }
    if m.contains(Modifier::UNDERLINED) {
        flags |= 8;
    }
    if m.contains(Modifier::REVERSED) {
        flags |= 16;
    }
    flags
}

fn cell_single_char(symbol: &str) -> char {
    let ch = symbol
        .chars()
        .find(|c| *c != '\u{fe0e}' && *c != '\u{fe0f}' && *c != '\u{200d}')
        .unwrap_or(' ');
    match ch {
        '▎' | '▏' | '▍' => '▌',
        '⌕' => '/',
        '★' => '*',
        '✦' => '*',
        '◉' | '◆' | '◈' => '●',
        '◌' => '○',
        '☐' => '▫',
        '☑' => '■',
        'ℹ' => 'i',
        '❝' => '"',
        '⚡' => '▸',
        other => other,
    }
}

fn buffer_to_rows(buf: &Buffer, width: u16, height: u16) -> Vec<Value> {
    let mut rows = Vec::with_capacity(height as usize);
    for y in 0..height {
        let mut runs: Vec<Value> = Vec::new();
        let mut cur_text = String::new();
        let mut cur_style: Option<(Color, Color, u8)> = None;

        for x in 0..width {
            let cell = &buf[(x, y)];
            let flags = modifier_flags(cell.modifier);
            let ch = cell_single_char(cell.symbol());
            let style_key = (cell.fg, cell.bg, flags);
            if cur_style == Some(style_key) {
                cur_text.push(ch);
            } else {
                if let Some((fg, bg, fl)) = cur_style {
                    runs.push(json!([cur_text, color_to_json(fg), color_to_json(bg), fl]));
                    cur_text = String::new();
                }
                cur_text.push(ch);
                cur_style = Some(style_key);
            }
        }
        if let Some((fg, bg, fl)) = cur_style {
            runs.push(json!([cur_text, color_to_json(fg), color_to_json(bg), fl]));
        }
        rows.push(Value::Array(runs));
    }
    rows
}

fn capture_frame(
    app: &AppState,
    ui: &mut UiState,
    duration_ms: u64,
    caption: &str,
    key_badge: &str,
) -> Value {
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|f| {
            let _ = ui::draw(f, app, ui);
        })
        .expect("draw frame");
    let rows = buffer_to_rows(terminal.backend().buffer(), WIDTH, HEIGHT);
    json!({
        "duration_ms": duration_ms,
        "caption": caption,
        "key_badge": key_badge,
        "rows": rows,
    })
}

fn populate_demo_vault(root: &Path) {
    fs::create_dir_all(root.join("Projects")).unwrap();
    fs::create_dir_all(root.join("Engineering")).unwrap();
    fs::create_dir_all(root.join("Daily Notes")).unwrap();
    fs::create_dir_all(root.join(".obsidian")).unwrap();

    fs::write(
        root.join(".obsidian/daily-notes.json"),
        "{\"folder\": \"Daily Notes\", \"format\": \"YYYY-MM-DD\"}\n",
    )
    .unwrap();

    fs::write(
        root.join("Projects/Q4 Architecture RFC.md"),
        r#"---
status: active
owner: Alex Rivera
created: 2026-11-12
aliases: [RFC-042, Core Architecture]
tags: [architecture, rust, knowledge-graph]
---

# Q4 Architecture RFC

> [!tip] Zero-Auth Local-First Design
> Read native `obsidian.json` vaults directly with incremental `mtime_ms` indexing and <2ms FTS5 queries.

## Core Pillars

- [x] Incremental SQLite WAL + FTS5 index in [[Storage Engine|SQLite Storage]]
- [ ] Interactive Braille graph renderer in [[Graph Layout]]
- [ ] Ship 18-tool stdio MCP server per [[MCP Agent Spec]]

See [[Release Checklist]] and [OpenAPI Reference](https://github.com/BiteFoundry/obsidian) before tagging `v0.1.0`.
Also evaluating [[Future Sync Protocol]] for peer-to-peer delta sync.

```rust
let graph = storage.get_graph(&vault.id, Some("Projects/Q4 Architecture RFC.md"), 2)?;
```
"#,
    )
    .unwrap();

    fs::write(
        root.join("Engineering/Storage Engine.md"),
        r#"---
status: shipped
tags: [architecture, sqlite, performance]
---

# Storage Engine

Backed by SQLite WAL (`busy_timeout = 5000`) and FTS5 for [[Q4 Architecture RFC]].
Uses [[SQLite FTS5]] with automatic `<3`-char Korean `LIKE` fallback and feeds [[Graph Layout]].
Benchmarked in [[Benchmarks]].
"#,
    )
    .unwrap();

    fs::write(
        root.join("Engineering/Graph Layout.md"),
        r#"---
status: in-progress
tags: [architecture, tui, knowledge-graph]
---

# Graph Layout

Force-directed + radial layout for [[Q4 Architecture RFC]] rendered via Ratatui Braille Canvas.
Reads adjacency lists from [[Storage Engine]] and pairs with [[Terminal Image Preview]].
"#,
    )
    .unwrap();

    fs::write(
        root.join("Engineering/MCP Agent Spec.md"),
        r#"---
status: shipped
tags: [mcp, agent, architecture]
---

# MCP Agent Spec

Defines 18 typed JSON-RPC 2.0 tools for [[Q4 Architecture RFC]] with `confirm: true` trash safety.
Verified by [[Release Checklist]].
"#,
    )
    .unwrap();

    fs::write(
        root.join("Engineering/SQLite FTS5.md"),
        r#"---
tags: [sqlite, search]
---

# SQLite FTS5

Tokenizer `unicode61 remove_diacritics 2` supporting [[Storage Engine]] and [[Benchmarks]].
"#,
    )
    .unwrap();

    fs::write(
        root.join("Engineering/Terminal Image Preview.md"),
        r#"---
tags: [tui, graphics]
---

# Terminal Image Preview

Kitty, iTerm2, Sixel & Halfblock rendering integrated with [[Graph Layout]].
"#,
    )
    .unwrap();

    fs::write(
        root.join("Engineering/Benchmarks.md"),
        r#"---
tags: [performance, sqlite]
---

# Benchmarks

Incremental reindex scans 1,000 notes in 4ms on [[Storage Engine]] and [[SQLite FTS5]].
"#,
    )
    .unwrap();

    fs::write(
        root.join("Projects/Release Checklist.md"),
        r#"---
status: ready
tags: [release, project/q4]
---

# Release Checklist

Tracking v0.1.0 launch for [[Q4 Architecture RFC]] and [[MCP Agent Spec]].

- [x] Verify 6 npm platform packages & GLIBC 2.28 ceiling
- [x] Zero compiler warnings (`RUSTFLAGS="-D warnings"`)
- [ ] Publish `@bitefoundry/obsidian` to npm
"#,
    )
    .unwrap();

    fs::write(
        root.join("Daily Notes/2026-11-20.md"),
        r#"---
created: 2026-11-20
tags: [daily, standup]
---

# 2026-11-20

> [!info] Morning Focus
> Ship `@bitefoundry/obsidian` v0.1.0 TUI, Braille Knowledge Graph, HTTP `:8792`, and 18 MCP tools.

## Today's Priorities

- [x] Review [[Q4 Architecture RFC]] and [[Storage Engine]]
- [x] Verify interactive Knowledge Graph view in [[Graph Layout]]
- [ ] Run final E2E suite for [[Release Checklist]]

## Standup Notes

- Backlink snippets now render live in the Right Inspector pane (`B`).
- Quick Switcher (`Ctrl+O`) fuzzy-matches titles, folders, aliases, and `#tags`.
"#,
    )
    .unwrap();
}

fn main() {
    let tmp = tempfile::tempdir().expect("temp demo vault");
    let vault_root = tmp.path().join("Acme Second Brain");
    fs::create_dir_all(&vault_root).unwrap();
    populate_demo_vault(&vault_root);

    let vault: VaultProfile = vault_from_dir(&vault_root, Some("Acme Second Brain"));
    let second_vault = VaultProfile {
        id: "v-personal".into(),
        name: "Personal Zettelkasten".into(),
        path: tmp.path().join("Personal Zettelkasten"),
        discovered: true,
        open_in_obsidian: false,
        ts: None,
        daily_folder: None,
        daily_format: None,
        attachments_folder: None,
    };

    let cfg = AppConfig {
        active_vault: vault.name.clone(),
        vaults: vec![vault.clone(), second_vault],
        theme: ThemePreset::ObsidianDark,
        page_width: PageWidthMode::Centered,
        show_sidebar: true,
        show_inspector: true,
        inline_images: true,
        ..Default::default()
    };

    let storage = Storage::open_in_memory().expect("sqlite in-memory");
    storage.sync_vault(&vault).expect("sync demo vault");
    storage
        .set_bookmark(&vault.id, "Projects/Q4 Architecture RFC.md", true)
        .unwrap();
    storage
        .set_bookmark(&vault.id, "Daily Notes/2026-11-20.md", true)
        .unwrap();

    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let (_ev_tx, _ev_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let mut app = AppState::new(cfg.clone(), vault.clone(), storage, sync_ctrl);
    let mut ui = UiState::from_config(&cfg);
    ui.theme = Theme::obsidian_dark();
    app.reload_from_storage(&mut ui);
    app.open_note(&mut ui, "Projects/Q4 Architecture RFC.md", false);
    ui.focus = Focus::Canvas;
    ui.canvas_selected_block = 4; // First open task (`- [ ] Interactive Braille graph renderer`)

    let mut frames: Vec<Value> = Vec::new();

    // Frame 1: 3-Pane Obsidian Workspace (Files | Note with Frontmatter, #tags, Callout, Tasks, Wikilinks, Code | Backlinks & Outgoing)
    frames.push(capture_frame(
        &app,
        &mut ui,
        2200,
        "3-Pane Obsidian Workspace — Frontmatter pills, #tags, Callouts, Tasks & Live Backlinks",
        "obsidian",
    ));

    // Frame 2: Interactive Checkbox toggled [x] + Wikilink highlighted
    app.dispatch(Action::ToggleSelectedTask, &mut ui);
    ui.canvas_selected_block = 6; // Paragraph with [[Release Checklist]]
    frames.push(capture_frame(
        &app,
        &mut ui,
        1900,
        "Live Markdown Checkbox toggled [x] on disk + [[wikilink]] highlighted for instant jump",
        "Space / Enter",
    ));

    // Frame 3: Interactive Knowledge Graph View (G) around Q4 Architecture RFC
    ui.toasts.clear();
    app.dispatch(Action::ToggleGraphView, &mut ui);
    ui.graph_selected = 1; // Highlight connected 1-hop node
    frames.push(capture_frame(
        &app,
        &mut ui,
        2600,
        "Interactive Knowledge Graph View — Force-directed Braille canvas around Q4 Architecture RFC",
        "G",
    ));

    // Frame 4: Sidebar Tags (2) view filtering #architecture
    ui.canvas_mode = CanvasMode::Note;
    ui.sidebar_tab = SidebarTab::Tags;
    ui.active_tag_filter = Some("architecture".into());
    ui.focus = Focus::Sidebar;
    ui.sidebar_selected = 1;
    frames.push(capture_frame(
        &app,
        &mut ui,
        1900,
        "Sidebar Tags (2) — Filter vault notes instantaneously by #architecture",
        "2 · Tags",
    ));

    // Frame 5: Daily Note (D) view (2026-11-20.md)
    ui.sidebar_tab = SidebarTab::Files;
    ui.active_tag_filter = None;
    ui.focus = Focus::Canvas;
    app.open_note(&mut ui, "Daily Notes/2026-11-20.md", true);
    frames.push(capture_frame(
        &app,
        &mut ui,
        1900,
        "Daily Notes (D) — Jump straight into 2026-11-20.md with auto-linked backlinks",
        "D · Daily",
    ));

    // Frame 6: Quick Switcher (Ctrl+O) fuzzy search overlay
    app.open_note(&mut ui, "Projects/Q4 Architecture RFC.md", false);
    let mut qs = QuickSwitcherOverlay::new();
    qs.query = "arch".into();
    ui.open_overlay(Overlay::QuickSwitcher(qs));
    frames.push(capture_frame(
        &app,
        &mut ui,
        1900,
        "Quick Switcher (Ctrl+O) — Fuzzy note & >command launcher across your vault",
        "Ctrl+O",
    ));

    // Frame 7: Link Extractor (u) overlay
    ui.close_overlay();
    if let Some(detail) = app.active_note.as_ref() {
        ui.open_overlay(Overlay::LinkExtractor(LinkExtractorOverlay::from_note(
            detail,
        )));
    }
    frames.push(capture_frame(
        &app,
        &mut ui,
        1900,
        "Link Extractor (u) — Inspect & jump to all [[wikilinks]] and external URLs",
        "u · Links",
    ));

    // Frame 8: Settings (Ctrl+,) theme switcher
    ui.close_overlay();
    let mut settings = SettingsOverlay::from_state(&app, &ui);
    settings.theme = ThemePreset::Catppuccin;
    ui.theme = Theme::catppuccin();
    ui.open_overlay(Overlay::Settings(settings));
    frames.push(capture_frame(
        &app,
        &mut ui,
        2100,
        "Settings (Ctrl+,) — 5 curated palettes: Obsidian Dark, Catppuccin, Tokyo Night, Nord, Warm Paper",
        "Ctrl+,",
    ));

    let spec = json!({
        "title": "obsidian — Acme Second Brain",
        "badge": "@bitefoundry/obsidian v0.1.0",
        "width": WIDTH,
        "height": HEIGHT,
        "bg": [19, 19, 24],
        "fg": [234, 234, 242],
        "frames": frames,
    });

    let tmp_json = tmp.path().join("obsidian_demo_frames.json");
    fs::write(&tmp_json, serde_json::to_vec(&spec).unwrap()).expect("write frames json");

    let out_gif = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("demo.gif");
    if let Some(parent) = out_gif.parent() {
        fs::create_dir_all(parent).unwrap();
    }

    let renderer = renderer_script();
    if !renderer.is_file() {
        eprintln!(
            "Renderer script not found at {}. Set DEMO_RENDERER_SCRIPT to render_tui_gif.py.",
            renderer.display()
        );
        std::process::exit(1);
    }
    let previews = preview_dir();
    fs::create_dir_all(&previews).expect("create preview directory");

    let status = Command::new("python")
        .arg(&renderer)
        .arg(&tmp_json)
        .arg(&out_gif)
        .arg("--preview-png")
        .arg(&previews)
        .status()
        .expect("invoke render_tui_gif.py");

    assert!(status.success(), "render_tui_gif.py failed");
    println!("Demo GIF generated at {}", out_gif.display());
}
