//! Left Navigation Sidebar (`[` / `Ctrl+B`):
//! 5 switchable tabs: `1 Files` · `2 Tags` · `3 Bookmarks` · `4 Outline` · `5 Tasks`.

use std::collections::{BTreeMap, HashSet};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{
    models::{NoteSummary, SidebarTab},
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{HitMap, HitTarget},
        truncate_width,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarEntry {
    Folder {
        path: String,
        name: String,
        depth: usize,
        expanded: bool,
        count: usize,
    },
    Note {
        rel_path: String,
        title: String,
        depth: usize,
        is_bookmarked: bool,
        backlinks: usize,
    },
    Tag {
        tag: String,
        count: usize,
        active_filter: bool,
    },
    TagHeader {
        tag: String,
        count: usize,
    },
    Heading {
        level: u8,
        text: String,
        line_no: usize,
    },
    Task {
        note_path: String,
        note_title: String,
        line_no: usize,
        text: String,
        checked: bool,
    },
}

pub fn selection_offset(selected: usize, visible: usize, total: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    let half = visible / 2;
    let max_off = total.saturating_sub(visible);
    selected.saturating_sub(half).min(max_off)
}

pub fn build_sidebar_entries(app: &AppState, ui: &UiState) -> Vec<SidebarEntry> {
    match ui.sidebar_tab {
        SidebarTab::Files => build_file_tree(&app.notes, &ui.collapsed_folders),
        SidebarTab::Tags => {
            let mut out = Vec::new();
            if let Some(ref active_tag) = ui.active_tag_filter {
                let matching: Vec<&NoteSummary> = app
                    .notes
                    .iter()
                    .filter(|n| {
                        n.tags.iter().any(|t| {
                            t == active_tag || t.starts_with(&format!("{active_tag}/"))
                        })
                    })
                    .collect();
                out.push(SidebarEntry::TagHeader {
                    tag: active_tag.clone(),
                    count: matching.len(),
                });
                for n in matching {
                    out.push(SidebarEntry::Note {
                        rel_path: n.rel_path.clone(),
                        title: n.title.clone(),
                        depth: 1,
                        is_bookmarked: n.is_bookmarked,
                        backlinks: n.backlink_count,
                    });
                }
            }
            for tc in &app.tags {
                let is_active = ui
                    .active_tag_filter
                    .as_deref()
                    .map(|f| f == tc.tag)
                    .unwrap_or(false);
                out.push(SidebarEntry::Tag {
                    tag: tc.tag.clone(),
                    count: tc.count,
                    active_filter: is_active,
                });
            }
            out
        }
        SidebarTab::Bookmarks => app
            .notes
            .iter()
            .filter(|n| n.is_bookmarked)
            .map(|n| SidebarEntry::Note {
                rel_path: n.rel_path.clone(),
                title: n.title.clone(),
                depth: 0,
                is_bookmarked: true,
                backlinks: n.backlink_count,
            })
            .collect(),
        SidebarTab::Outline => app
            .active_note
            .as_ref()
            .map(|d| {
                d.parsed
                    .headings
                    .iter()
                    .map(|h| SidebarEntry::Heading {
                        level: h.level,
                        text: h.text.clone(),
                        line_no: h.line_no,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        SidebarTab::Tasks => app
            .tasks
            .iter()
            .map(|t| SidebarEntry::Task {
                note_path: t.note_path.clone(),
                note_title: t.note_title.clone(),
                line_no: t.line_no,
                text: t.text.clone(),
                checked: t.checked,
            })
            .collect(),
    }
}

fn build_file_tree(notes: &[NoteSummary], collapsed: &HashSet<String>) -> Vec<SidebarEntry> {
    let mut by_folder: BTreeMap<String, Vec<&NoteSummary>> = BTreeMap::new();
    for n in notes {
        by_folder.entry(n.folder.clone()).or_default().push(n);
    }

    let mut out = Vec::new();
    // Folders first, then root notes
    for (folder, items) in &by_folder {
        if folder.is_empty() {
            continue;
        }
        let expanded = !collapsed.contains(folder);
        let depth = folder.matches('/').count();
        let display = folder.rsplit('/').next().unwrap_or(folder).to_string();
        out.push(SidebarEntry::Folder {
            path: folder.clone(),
            name: display,
            depth,
            expanded,
            count: items.len(),
        });
        if expanded {
            for n in items {
                out.push(SidebarEntry::Note {
                    rel_path: n.rel_path.clone(),
                    title: n.title.clone(),
                    depth: depth + 1,
                    is_bookmarked: n.is_bookmarked,
                    backlinks: n.backlink_count,
                });
            }
        }
    }

    if let Some(root_notes) = by_folder.get("") {
        for n in root_notes {
            out.push(SidebarEntry::Note {
                rel_path: n.rel_path.clone(),
                title: n.title.clone(),
                depth: 0,
                is_bookmarked: n.is_bookmarked,
                backlinks: n.backlink_count,
            });
        }
    }
    out
}

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let focused = ui.focus == Focus::Sidebar;

    let outer = Block::default()
        .borders(Borders::RIGHT)
        .border_style(theme.border_style(focused))
        .style(Style::default().bg(theme.bg_sidebar).fg(theme.fg));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    if inner.width < 6 || inner.height < 3 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(inner);

    // 1. Compact tab bar (`1 Files · 2 Tags · 3 Marks · 4 Outl · 5 Tasks`)
    let tab_area = chunks[0];
    let mut tab_spans: Vec<Span<'static>> = Vec::new();
    let mut cur_x = tab_area.x;
    for tab in SidebarTab::all() {
        let active = ui.sidebar_tab == *tab;
        let label = format!("{}{}", tab.key_num(), &tab.label()[..1]);
        let full_label = if tab_area.width >= 30 {
            format!(" {}{} ", tab.key_num(), tab.label())
        } else {
            format!(" {label} ")
        };
        let style = if active {
            Style::default()
                .fg(theme.bg_canvas)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg_muted).bg(theme.bg_sidebar)
        };
        let w = full_label.chars().count() as u16;
        if cur_x + w <= tab_area.x + tab_area.width {
            hitmap.add_line(cur_x, tab_area.y, w, HitTarget::SidebarTab(*tab));
            cur_x += w;
        }
        tab_spans.push(Span::styled(full_label, style));
    }
    frame.render_widget(
        Paragraph::new(Line::from(tab_spans)).style(Style::default().bg(theme.bg_sidebar)),
        Rect::new(tab_area.x, tab_area.y, tab_area.width, 1),
    );

    // Subheader line naming active tab
    let sub_title = match ui.sidebar_tab {
        SidebarTab::Files => format!(" Files ({}) ", app.notes.len()),
        SidebarTab::Tags => match &ui.active_tag_filter {
            Some(t) => format!(" Tag #{t} (Esc clear) "),
            None => format!(" Tags ({}) ", app.tags.len()),
        },
        SidebarTab::Bookmarks => {
            let cnt = app.notes.iter().filter(|n| n.is_bookmarked).count();
            format!(" Bookmarks ({cnt}) ")
        }
        SidebarTab::Outline => {
            let cnt = app
                .active_note
                .as_ref()
                .map(|n| n.parsed.headings.len())
                .unwrap_or(0);
            format!(" Outline ({cnt}) ")
        }
        SidebarTab::Tasks => {
            let open = app.tasks.iter().filter(|t| !t.checked).count();
            format!(" Tasks ({open} open) ")
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                truncate_width(&sub_title, tab_area.width as usize),
                Style::default()
                    .fg(if focused { theme.accent } else { theme.fg_muted })
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .style(Style::default().bg(theme.bg_sidebar)),
        Rect::new(tab_area.x, tab_area.y + 1, tab_area.width, 1),
    );

    // 2. Entries list
    let list_area = chunks[1];
    let entries = build_sidebar_entries(app, ui);
    if entries.is_empty() {
        let empty_msg = match ui.sidebar_tab {
            SidebarTab::Files => "  No notes in vault.",
            SidebarTab::Tags => "  No #tags indexed.",
            SidebarTab::Bookmarks => "  No bookmarks (press b).",
            SidebarTab::Outline => "  No headings in note.",
            SidebarTab::Tasks => "  No checklist tasks.",
        };
        frame.render_widget(
            Paragraph::new(Span::styled(empty_msg, theme.muted_style()))
                .style(Style::default().bg(theme.bg_sidebar)),
            list_area,
        );
        return;
    }

    let visible = list_area.height as usize;
    let sel = ui.sidebar_selected.min(entries.len().saturating_sub(1));
    let offset = selection_offset(sel, visible, entries.len());
    let active_rel = app
        .active_note
        .as_ref()
        .map(|d| d.summary.rel_path.as_str())
        .unwrap_or("");

    for (i, entry) in entries.iter().skip(offset).take(visible).enumerate() {
        let idx = offset + i;
        let y = list_area.y + i as u16;
        let row_rect = Rect::new(list_area.x, y, list_area.width, 1);
        let is_sel = idx == sel;
        let is_active_note = matches!(entry, SidebarEntry::Note { rel_path, .. } if rel_path == active_rel);

        let bg = if is_sel && focused {
            theme.bg_selected
        } else if is_active_note {
            theme.bg_card
        } else {
            theme.bg_sidebar
        };

        let w = list_area.width as usize;
        let line = match entry {
            SidebarEntry::Folder {
                name,
                depth,
                expanded,
                count,
                ..
            } => {
                let indent = "  ".repeat(*depth);
                let arrow = if *expanded { "▾" } else { "▸" };
                let avail = w.saturating_sub(indent.len() + 7);
                Line::from(vec![
                    Span::raw(format!(" {indent}")),
                    Span::styled(format!("{arrow} "), Style::default().fg(theme.accent_soft)),
                    Span::styled(
                        truncate_width(name, avail),
                        Style::default()
                            .fg(theme.fg_secondary)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" ({count})"), theme.subtle_style()),
                ])
            }
            SidebarEntry::Note {
                title,
                depth,
                is_bookmarked,
                ..
            } => {
                let indent = "  ".repeat(*depth);
                let marker = if is_active_note {
                    "▎"
                } else if is_sel && focused {
                    "▸"
                } else {
                    " "
                };
                let icon = if *is_bookmarked { "★ " } else { "▪ " };
                let avail = w.saturating_sub(indent.len() + 4);
                Line::from(vec![
                    Span::styled(
                        marker.to_string(),
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(indent),
                    Span::styled(
                        icon.to_string(),
                        Style::default().fg(if *is_bookmarked {
                            theme.warning
                        } else if is_active_note {
                            theme.accent
                        } else {
                            theme.fg_muted
                        }),
                    ),
                    Span::styled(
                        truncate_width(title, avail),
                        Style::default()
                            .fg(if is_active_note {
                                theme.accent_soft
                            } else {
                                theme.fg_primary
                            })
                            .add_modifier(if is_active_note || (is_sel && focused) {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ])
            }
            SidebarEntry::TagHeader { tag, count } => Line::from(vec![
                Span::styled(" ▾ ", Style::default().fg(theme.accent)),
                Span::styled(format!("#{tag}"), theme.tag_pill_style()),
                Span::styled(format!("  {count} notes"), theme.muted_style()),
            ]),
            SidebarEntry::Tag {
                tag,
                count,
                active_filter,
            } => {
                let avail = w.saturating_sub(10);
                Line::from(vec![
                    Span::styled(
                        if *active_filter { " ▸ " } else { "   " },
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("#{}", truncate_width(tag, avail)),
                        if *active_filter {
                            theme.tag_pill_style().add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme.accent_soft)
                        },
                    ),
                    Span::styled(format!(" ({count})"), theme.muted_style()),
                ])
            }
            SidebarEntry::Heading { level, text, .. } => {
                let indent = " ".repeat((level.saturating_sub(1) as usize) * 2);
                let avail = w.saturating_sub(indent.len() + 5);
                Line::from(vec![
                    Span::raw(format!(" {indent}")),
                    Span::styled(
                        format!("H{level} "),
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        truncate_width(text, avail),
                        Style::default().fg(theme.fg_primary),
                    ),
                ])
            }
            SidebarEntry::Task {
                text,
                checked,
                ..
            } => {
                let box_str = if *checked { "☑ " } else { "☐ " };
                let avail = w.saturating_sub(5);
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        box_str.to_string(),
                        Style::default().fg(if *checked {
                            theme.success
                        } else {
                            theme.accent
                        }),
                    ),
                    Span::styled(
                        truncate_width(text, avail),
                        if *checked {
                            theme.muted_style().add_modifier(Modifier::CROSSED_OUT)
                        } else {
                            Style::default().fg(theme.fg_primary)
                        },
                    ),
                ])
            }
        };

        frame.render_widget(Paragraph::new(line).style(Style::default().bg(bg)), row_rect);
        hitmap.add(row_rect, HitTarget::SidebarRow(idx));
    }
}
