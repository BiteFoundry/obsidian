//! Left Navigation Sidebar (`[` / `Ctrl+B`):
//! 5 switchable tabs: `1 Files` · `2 Tags` · `3 Bookmarks` · `4 Outline` · `5 Tasks`.

use std::collections::{BTreeMap, HashSet};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

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
    HtmlFile {
        rel_path: String,
        title: String,
        depth: usize,
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
        SidebarTab::Files => build_file_tree(&app.notes, &app.html_files, &ui.collapsed_folders),
        SidebarTab::Tags => {
            let mut out = Vec::new();
            if let Some(ref active_tag) = ui.active_tag_filter {
                let matching: Vec<&NoteSummary> = app
                    .notes
                    .iter()
                    .filter(|n| {
                        n.tags
                            .iter()
                            .any(|t| t == active_tag || t.starts_with(&format!("{active_tag}/")))
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

/// Proper ancestor folders of `folder`, outermost first: `A/B/C` yields `A` then `A/B`.
fn folder_ancestors(folder: &str) -> impl Iterator<Item = &str> {
    folder
        .match_indices('/')
        .map(move |(idx, _)| &folder[..idx])
}

fn build_file_tree(
    notes: &[NoteSummary],
    html_files: &[crate::vault::fs::VaultHtmlFile],
    collapsed: &HashSet<String>,
) -> Vec<SidebarEntry> {
    let mut by_folder: BTreeMap<String, Vec<&NoteSummary>> = BTreeMap::new();
    for n in notes {
        by_folder.entry(n.folder.clone()).or_default().push(n);
    }
    let mut html_by_folder: BTreeMap<String, Vec<&crate::vault::fs::VaultHtmlFile>> =
        BTreeMap::new();
    for h in html_files {
        html_by_folder.entry(h.folder.clone()).or_default().push(h);
        by_folder.entry(h.folder.clone()).or_default();
    }

    // Every ancestor of a folder gets its own row, so `A/B` never shows without `A` above it.
    let ancestors: Vec<String> = by_folder
        .keys()
        .flat_map(|folder| folder_ancestors(folder).map(str::to_string))
        .collect();
    for ancestor in ancestors {
        by_folder.entry(ancestor).or_default();
    }

    // Order by path components, not by the raw string: `-` sorts before `/`, so a plain string
    // order would put `Projects-old` between `Projects` and `Projects/Sub`.
    let mut folders: Vec<&String> = by_folder.keys().filter(|f| !f.is_empty()).collect();
    folders.sort_by(|a, b| a.split('/').cmp(b.split('/')));

    let mut out = Vec::new();
    // Folders first, then root notes & root HTML files
    for folder in folders {
        // A collapsed folder hides its whole subtree, not only its direct items.
        if folder_ancestors(folder).any(|ancestor| collapsed.contains(ancestor)) {
            continue;
        }
        let items = &by_folder[folder];
        let html_items = html_by_folder.get(folder).map(Vec::as_slice).unwrap_or(&[]);
        let expanded = !collapsed.contains(folder);
        let depth = folder.matches('/').count();
        let display = folder.rsplit('/').next().unwrap_or(folder).to_string();
        out.push(SidebarEntry::Folder {
            path: folder.clone(),
            name: display,
            depth,
            expanded,
            count: items.len() + html_items.len(),
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
            for h in html_items {
                out.push(SidebarEntry::HtmlFile {
                    rel_path: h.rel_path.clone(),
                    title: h.title.clone(),
                    depth: depth + 1,
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
    if let Some(root_html) = html_by_folder.get("") {
        for h in root_html {
            out.push(SidebarEntry::HtmlFile {
                rel_path: h.rel_path.clone(),
                title: h.title.clone(),
                depth: 0,
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
    hitmap.add(area, HitTarget::Pane(Focus::Sidebar));

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
        Paragraph::new(Line::from(vec![Span::styled(
            truncate_width(&sub_title, tab_area.width as usize),
            Style::default()
                .fg(if focused {
                    theme.accent
                } else {
                    theme.fg_muted
                })
                .add_modifier(Modifier::BOLD),
        )]))
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
                .wrap(Wrap { trim: false })
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
        let is_active_note =
            matches!(entry, SidebarEntry::Note { rel_path, .. } if rel_path == active_rel);

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
                let count_suffix = format!(" ({count})");
                let count_w = UnicodeWidthStr::width(count_suffix.as_str());
                let avail = w.saturating_sub(1 + indent.len() + 2 + count_w);
                Line::from(vec![
                    Span::raw(format!(" {indent}")),
                    Span::styled(format!("{arrow} "), Style::default().fg(theme.accent_soft)),
                    Span::styled(
                        truncate_width(name, avail),
                        Style::default()
                            .fg(theme.fg_secondary)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(count_suffix, theme.subtle_style()),
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
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
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
            SidebarEntry::HtmlFile { title, depth, .. } => {
                let indent = "  ".repeat(*depth);
                let marker = if is_sel && focused { "▸" } else { " " };
                let avail = w.saturating_sub(indent.len() + 9);
                Line::from(vec![
                    Span::styled(
                        marker.to_string(),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(indent),
                    Span::styled("◈ ", Style::default().fg(theme.info)),
                    Span::styled(
                        truncate_width(title, avail),
                        Style::default()
                            .fg(theme.fg_primary)
                            .add_modifier(if is_sel && focused {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::styled(" HTML", Style::default().fg(theme.info)),
                ])
            }
            SidebarEntry::TagHeader { tag, count } => {
                let suffix = format!("  {count} notes");
                let suffix_w = UnicodeWidthStr::width(suffix.as_str());
                let prefix_w = 3; // " ▾ "
                if prefix_w + 1 + suffix_w < w {
                    let avail = w - prefix_w - 1 - suffix_w;
                    Line::from(vec![
                        Span::styled(" ▾ ", Style::default().fg(theme.accent)),
                        Span::styled(
                            format!("#{}", truncate_width(tag, avail)),
                            theme.tag_pill_style(),
                        ),
                        Span::styled(suffix, theme.muted_style()),
                    ])
                } else {
                    let full = format!(" ▾ #{tag}{suffix}");
                    Line::from(Span::styled(
                        truncate_width(&full, w),
                        theme.tag_pill_style(),
                    ))
                }
            }
            SidebarEntry::Tag {
                tag,
                count,
                active_filter,
            } => {
                let count_suffix = format!(" ({count})");
                let count_w = UnicodeWidthStr::width(count_suffix.as_str());
                let avail = w.saturating_sub(4 + count_w);
                Line::from(vec![
                    Span::styled(
                        if *active_filter { " ▸ " } else { "   " },
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("#{}", truncate_width(tag, avail)),
                        if *active_filter {
                            theme.tag_pill_style().add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme.accent_soft)
                        },
                    ),
                    Span::styled(count_suffix, theme.muted_style()),
                ])
            }
            SidebarEntry::Heading { level, text, .. } => {
                let indent = " ".repeat((level.saturating_sub(1) as usize) * 2);
                let avail = w.saturating_sub(indent.len() + 5);
                Line::from(vec![
                    Span::raw(format!(" {indent}")),
                    Span::styled(
                        format!("H{level} "),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        truncate_width(text, avail),
                        Style::default().fg(theme.fg_primary),
                    ),
                ])
            }
            SidebarEntry::Task { text, checked, .. } => {
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

        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(bg)),
            row_rect,
        );
        hitmap.add(row_rect, HitTarget::SidebarRow(idx));
        if matches!(entry, SidebarEntry::Task { .. }) {
            let chk_w = 4u16.min(row_rect.width);
            hitmap.add_line(
                row_rect.x,
                row_rect.y,
                chk_w,
                HitTarget::SidebarTaskCheckbox(idx),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(rel_path: &str) -> NoteSummary {
        let (folder, file) = rel_path.rsplit_once('/').unwrap_or(("", rel_path));
        NoteSummary {
            rel_path: rel_path.to_string(),
            title: file.trim_end_matches(".md").to_string(),
            folder: folder.to_string(),
            ..NoteSummary::default()
        }
    }

    fn html(rel_path: &str) -> crate::vault::fs::VaultHtmlFile {
        let (folder, file) = rel_path.rsplit_once('/').unwrap_or(("", rel_path));
        crate::vault::fs::VaultHtmlFile {
            rel_path: rel_path.to_string(),
            abs_path: std::path::PathBuf::from(rel_path),
            title: file.to_string(),
            folder: folder.to_string(),
            mtime_ns: 0,
            size_bytes: 0,
        }
    }

    /// One line per entry: `F <path> d<depth> <open|closed> (<count>)`, `N <path> d<depth>` or
    /// `H <path> d<depth>`.
    fn tree(notes: &[&str], html_files: &[&str], collapsed: &[&str]) -> Vec<String> {
        let notes: Vec<NoteSummary> = notes.iter().map(|p| note(p)).collect();
        let html_files: Vec<_> = html_files.iter().map(|p| html(p)).collect();
        let collapsed: HashSet<String> = collapsed.iter().map(|c| c.to_string()).collect();
        build_file_tree(&notes, &html_files, &collapsed)
            .into_iter()
            .map(|entry| match entry {
                SidebarEntry::Folder {
                    path,
                    depth,
                    expanded,
                    count,
                    ..
                } => format!(
                    "F {path} d{depth} {} ({count})",
                    if expanded { "open" } else { "closed" }
                ),
                SidebarEntry::Note {
                    rel_path, depth, ..
                } => format!("N {rel_path} d{depth}"),
                SidebarEntry::HtmlFile {
                    rel_path, depth, ..
                } => format!("H {rel_path} d{depth}"),
                other => panic!("unexpected entry in the file tree: {other:?}"),
            })
            .collect()
    }

    const OBS11_NOTES: &[&str] = &[
        "Other/o.md",
        "Projects/p.md",
        "Projects/Sub/s.md",
        "Projects/Sub/Deep/d.md",
        "root.md",
    ];

    #[test]
    fn obs11_collapsed_parent_hides_descendants() {
        assert_eq!(
            tree(OBS11_NOTES, &[], &["Projects"]),
            [
                "F Other d0 open (1)",
                "N Other/o.md d1",
                "F Projects d0 closed (1)",
                "N root.md d0",
            ]
        );
        // Nothing is collapsed: the whole tree, parents before children.
        assert_eq!(
            tree(OBS11_NOTES, &[], &[]),
            [
                "F Other d0 open (1)",
                "N Other/o.md d1",
                "F Projects d0 open (1)",
                "N Projects/p.md d1",
                "F Projects/Sub d1 open (1)",
                "N Projects/Sub/s.md d2",
                "F Projects/Sub/Deep d2 open (1)",
                "N Projects/Sub/Deep/d.md d3",
                "N root.md d0",
            ]
        );
    }

    #[test]
    fn obs11_collapsed_child_keeps_parent_notes() {
        assert_eq!(
            tree(OBS11_NOTES, &[], &["Projects/Sub"]),
            [
                "F Other d0 open (1)",
                "N Other/o.md d1",
                "F Projects d0 open (1)",
                "N Projects/p.md d1",
                "F Projects/Sub d1 closed (1)",
                "N root.md d0",
            ]
        );
        // A collapsed descendant stays remembered while its parent is collapsed too.
        assert_eq!(
            tree(OBS11_NOTES, &[], &["Projects", "Projects/Sub/Deep"]),
            tree(OBS11_NOTES, &[], &["Projects"])
        );
    }

    #[test]
    fn obs11_synthesizes_missing_ancestors() {
        assert_eq!(
            tree(&["A/B/n.md"], &[], &[]),
            ["F A d0 open (0)", "F A/B d1 open (1)", "N A/B/n.md d2"]
        );
        assert_eq!(tree(&["A/B/n.md"], &[], &["A"]), ["F A d0 closed (0)"]);
    }

    #[test]
    fn obs11_sibling_with_dash_sorts_after_subtree() {
        assert_eq!(
            tree(
                &["Projects/p.md", "Projects/Sub/s.md", "Projects-old/x.md"],
                &[],
                &[]
            ),
            [
                "F Projects d0 open (1)",
                "N Projects/p.md d1",
                "F Projects/Sub d1 open (1)",
                "N Projects/Sub/s.md d2",
                "F Projects-old d0 open (1)",
                "N Projects-old/x.md d1",
            ]
        );
    }

    #[test]
    fn obs11_html_only_folder_follows_collapse() {
        let notes = ["Projects/p.md"];
        let html_files = ["Projects/Site/index.html", "home.html"];
        assert_eq!(
            tree(&notes, &html_files, &[]),
            [
                "F Projects d0 open (1)",
                "N Projects/p.md d1",
                "F Projects/Site d1 open (1)",
                "H Projects/Site/index.html d2",
                "H home.html d0",
            ]
        );
        assert_eq!(
            tree(&notes, &html_files, &["Projects"]),
            ["F Projects d0 closed (1)", "H home.html d0"]
        );
    }

    #[test]
    fn obs11_flat_vault_renders_as_before() {
        // One level of folders: same rows and order as the previous string-ordered tree.
        assert_eq!(
            tree(
                &[
                    "Zeta/z.md",
                    "Alpha/b.md",
                    "Alpha/a.md",
                    "Daily Notes/2026-11-20.md",
                    "Inbox.md"
                ],
                &["Alpha/page.html", "index.html"],
                &["Zeta"]
            ),
            [
                "F Alpha d0 open (3)",
                "N Alpha/b.md d1",
                "N Alpha/a.md d1",
                "H Alpha/page.html d1",
                "F Daily Notes d0 open (1)",
                "N Daily Notes/2026-11-20.md d1",
                "F Zeta d0 closed (1)",
                "N Inbox.md d0",
                "H index.html d0",
            ]
        );
    }
}
