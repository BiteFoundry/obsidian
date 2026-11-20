//! Obsidian-Native 3-Pane Terminal UI layout & root renderer.
//!
//! Layout:
//! - Top Minimal Bar (`1` row): `[◧] ◆ Vault ▾  /  folder / Note Title` | `[Graph G]` `[Daily D]` `[Backlinks B]` `[Cfg ,]` `[? Help]`
//! - Main 3-Pane Workspace:
//!   - Left Sidebar (collapsible `[` / `Ctrl+B`): `1 Files · 2 Tags · 3 Bookmarks · 4 Outline · 5 Tasks`
//!   - Center Canvas: Mode A (Live Note Reader/Canvas) or Mode B (Interactive Knowledge Graph View `G`)
//!   - Right Inspector Pane (`B` toggle): Linked Mentions (Backlinks), Outgoing Links, Local Graph Mini-Stats
//! - Status Footer (`1` row): clickable keybind pills + clickable reindex stats, or live Toast notification.

pub mod action;
pub mod app;
pub mod composer;
pub mod event;
pub mod graph_view;
pub mod hitmap;
pub mod inspector;
pub mod keymap;
pub mod note_view;
pub mod notification;
pub mod overlays;
pub mod sidebar;
pub mod theme;
pub mod toast;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ui::{
    app::{AppState, CanvasMode, UiState},
    hitmap::{ButtonId, HitMap, HitTarget},
    toast::ToastKind,
};

pub fn draw(frame: &mut Frame, app: &AppState, ui: &mut UiState) -> HitMap {
    let area = frame.area();
    ui.term_width = area.width;
    ui.term_height = area.height;
    let mut hitmap = HitMap::new();
    ui.pending_pixel = None;

    frame.render_widget(
        Block::default().style(Style::default().bg(ui.theme.bg_canvas).fg(ui.theme.fg)),
        area,
    );

    if area.width < 20 || area.height < 6 {
        return hitmap;
    }

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(area);

    draw_top_bar(frame, vertical[0], app, ui, &mut hitmap);
    draw_workspace(frame, vertical[1], app, ui, &mut hitmap);
    draw_status_bar(frame, vertical[2], app, ui, &mut hitmap);

    if let Some(mut overlay) = ui.overlay.take() {
        overlay.draw(frame, area, app, ui, &mut hitmap);
        ui.overlay = Some(overlay);
    }

    ui.last_hitmap = hitmap.clone();
    hitmap
}

fn draw_top_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg_header)),
        area,
    );

    // Sidebar toggle pill on far left
    let sidebar_pill = "[◧]";
    let sidebar_w = sidebar_pill.width() as u16;
    let sidebar_style = if ui.show_sidebar {
        Style::default()
            .fg(theme.accent)
            .bg(theme.bg_header)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg_muted).bg(theme.bg_header)
    };
    hitmap.add_line(
        area.x,
        area.y,
        sidebar_w,
        HitTarget::Button(ButtonId::ToggleSidebar),
    );

    let vault_label = format!(" ◆ {} ▾ ", app.active_vault.display_name());
    let vault_w = vault_label.width() as u16;
    hitmap.add_line(
        area.x + sidebar_w,
        area.y,
        vault_w,
        HitTarget::Button(ButtonId::VaultSwitcher),
    );

    let (folder, title) = match &app.active_note {
        Some(d) => (d.summary.folder.as_str(), d.summary.title.as_str()),
        None => ("", "No Note Selected"),
    };

    // Right-aligned action pills
    let pills: [(&str, bool, ButtonId); 5] = [
        ("Graph G", ui.canvas_mode == CanvasMode::Graph, ButtonId::ToggleGraph),
        ("Daily D", false, ButtonId::OpenDaily),
        ("Backlinks B", ui.show_inspector, ButtonId::ToggleInspector),
        ("Cfg ,", false, ButtonId::Settings),
        ("? Help", false, ButtonId::Help),
    ];

    let right_width: u16 = pills
        .iter()
        .map(|(txt, _, _)| (txt.width() + 3) as u16)
        .sum::<u16>()
        + 1;

    let left_prefix_w = (sidebar_w + vault_w) as usize;
    let left_avail = area.width.saturating_sub(right_width + 2) as usize;
    let mut left_spans = vec![
        Span::styled(sidebar_pill, sidebar_style),
        Span::styled(
            vault_label,
            Style::default()
                .fg(theme.accent)
                .bg(theme.bg_header)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    if left_avail > left_prefix_w + 6 {
        let crumb_budget = left_avail.saturating_sub(left_prefix_w + 3);
        left_spans.push(Span::styled(" / ", theme.subtle_style()));
        if !folder.is_empty() && crumb_budget > 20 {
            left_spans.push(Span::styled(
                truncate_width(folder, crumb_budget / 2),
                theme.muted_style(),
            ));
            left_spans.push(Span::styled(" / ", theme.subtle_style()));
        }
        left_spans.push(Span::styled(
            truncate_width(title, crumb_budget.saturating_sub(6)),
            Style::default()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(left_spans)).style(Style::default().bg(theme.bg_header)),
        Rect::new(area.x, area.y, area.width.saturating_sub(right_width), 1),
    );

    // Draw right pills
    if area.width > right_width + 12 {
        let mut rx = area.x + area.width.saturating_sub(right_width);
        for (label, active, btn) in pills {
            let text = format!("[{label}]");
            let w = text.width() as u16;
            let style = if active {
                Style::default()
                    .fg(theme.bg_canvas)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.accent_soft).bg(theme.bg_header)
            };
            frame.render_widget(
                Paragraph::new(Span::styled(text, style)),
                Rect::new(rx, area.y, w, 1),
            );
            hitmap.add_line(rx, area.y, w, HitTarget::Button(btn));
            rx += w + 1;
        }
    }
}

fn draw_workspace(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let show_left = ui.show_sidebar && area.width >= 60;
    let show_right = ui.show_inspector && area.width >= 88;

    let left_w: u16 = if show_left {
        (area.width / 5).clamp(22, 30)
    } else {
        0
    };
    let right_w: u16 = if show_right {
        (area.width / 4).clamp(26, 34)
    } else {
        0
    };

    let mut constraints = Vec::new();
    if show_left {
        constraints.push(Constraint::Length(left_w));
    }
    constraints.push(Constraint::Min(24));
    if show_right {
        constraints.push(Constraint::Length(right_w));
    }

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    let mut pane_idx = 0usize;
    if show_left {
        sidebar::draw(frame, panes[pane_idx], app, ui, hitmap);
        pane_idx += 1;
    }

    let canvas_area = panes[pane_idx];
    pane_idx += 1;
    match ui.canvas_mode {
        CanvasMode::Note => note_view::draw(frame, canvas_area, app, ui, hitmap),
        CanvasMode::Graph => graph_view::draw(frame, canvas_area, app, ui, hitmap),
    }

    if show_right {
        inspector::draw(frame, panes[pane_idx], app, ui, hitmap);
    }
}

fn draw_status_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg_header)),
        area,
    );

    if let Some(toast) = ui.toasts.current() {
        let color = match toast.kind {
            ToastKind::Success => theme.success,
            ToastKind::Warning => theme.warning,
            ToastKind::Error => theme.error,
            ToastKind::Info => theme.accent_soft,
        };
        let line = Line::from(vec![
            Span::styled(" ● ", Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(toast.message.clone(), Style::default().fg(color)),
        ]);
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(theme.bg_header)),
            area,
        );
        return;
    }

    let stats_right = format!(
        " ↻ {} notes · {} tags ",
        app.notes.len(),
        app.tags.len()
    );
    let stats_w = stats_right.width() as u16;
    let left_limit = if area.width > stats_w + 10 {
        area.x + area.width.saturating_sub(stats_w)
    } else {
        area.x + area.width
    };

    let items: &[(&str, Option<ButtonId>)] = match ui.canvas_mode {
        CanvasMode::Graph => &[
            ("G Note", Some(ButtonId::ToggleGraph)),
            ("l Local/Global", Some(ButtonId::GraphToggleScope)),
            ("+/- Depth", None),
            ("Ctrl+O Switch", Some(ButtonId::QuickSwitcher)),
            ("Ctrl+, Theme", Some(ButtonId::Settings)),
        ],
        CanvasMode::Note => &[
            ("Ctrl+O Switch", Some(ButtonId::QuickSwitcher)),
            ("G Graph", Some(ButtonId::ToggleGraph)),
            ("D Daily", Some(ButtonId::OpenDaily)),
            ("a Append", Some(ButtonId::QuickAppend)),
            ("n New", Some(ButtonId::NewNote)),
            ("e Edit", Some(ButtonId::EditExternal)),
            ("u Links", Some(ButtonId::LinkExtractor)),
            ("h Hrefs", Some(ButtonId::ToggleHrefs)),
            ("v Img", Some(ButtonId::ImagePreview)),
            ("H HTML", Some(ButtonId::HtmlPreview)),
            ("Ctrl+, Theme", Some(ButtonId::Settings)),
        ],
    };

    let mut cur_x = area.x.saturating_add(1);
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    for (idx, &(label, btn_opt)) in items.iter().enumerate() {
        let sep = if idx > 0 { " · " } else { "" };
        let sep_w = sep.width() as u16;
        let lbl_w = label.width() as u16;
        if cur_x.saturating_add(sep_w).saturating_add(lbl_w) > left_limit {
            break;
        }
        if !sep.is_empty() {
            spans.push(Span::styled(sep.to_string(), theme.subtle_style()));
            cur_x += sep_w;
        }
        spans.push(Span::styled(label.to_string(), theme.muted_style()));
        if let Some(btn) = btn_opt {
            hitmap.add_line(cur_x, area.y, lbl_w, HitTarget::Button(btn));
        }
        cur_x += lbl_w;
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg_header)),
        Rect::new(area.x, area.y, left_limit.saturating_sub(area.x), 1),
    );

    if area.width > stats_w + 10 {
        let rx = area.x + area.width - stats_w;
        frame.render_widget(
            Paragraph::new(Span::styled(stats_right, theme.subtle_style()))
                .style(Style::default().bg(theme.bg_header)),
            Rect::new(rx, area.y, stats_w, 1),
        );
        hitmap.add_line(rx, area.y, stats_w, HitTarget::Button(ButtonId::ReindexNow));
    }
}

pub fn truncate_width(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut cur = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        let w = ch.width().unwrap_or(0);
        if cur + w > max_width {
            if max_width >= 1 && !out.is_empty() {
                while out.width() + 1 > max_width {
                    if out.pop().is_none() {
                        break;
                    }
                }
                out.push('…');
            }
            break;
        }
        out.push(ch);
        cur += w;
    }
    out
}
