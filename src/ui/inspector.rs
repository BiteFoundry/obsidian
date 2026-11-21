//! Right Inspector Pane (`B` toggle):
//! - Linked Mentions (Backlinks) with surrounding snippet context (`Enter` jumps to linking note)
//! - Outgoing Links (with resolved `→` vs unresolved `◌` indicator)
//! - Local Graph Mini-Stats (words, backlinks, outgoing links, tags, tasks, modified time).

use chrono::{DateTime, Local, TimeZone};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::ui::{
    app::{AppState, Focus, UiState},
    hitmap::{HitMap, HitTarget},
    truncate_width,
};

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let focused = ui.focus == Focus::Inspector;

    let outer = Block::default()
        .borders(Borders::LEFT)
        .border_style(theme.border_style(focused))
        .style(Style::default().bg(theme.bg_sidebar).fg(theme.fg));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    hitmap.add(area, HitTarget::Pane(Focus::Inspector));

    if inner.width < 10 || inner.height < 6 {
        return;
    }

    let Some(detail) = app.active_note.as_ref() else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  Select a note to inspect.",
                theme.muted_style(),
            )),
            inner,
        );
        return;
    };

    let show_outline = inner.height >= 18;
    let chunks = if show_outline {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(24),
                Constraint::Min(5),
                Constraint::Length(6),
            ])
            .split(inner)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(48),
                Constraint::Percentage(32),
                Constraint::Length(0),
                Constraint::Length(6),
            ])
            .split(inner)
    };

    let w = inner.width as usize;
    let total_bl = detail.backlinks.len();
    let total_ol = detail.outgoing_links.len();
    let total_items = total_bl + total_ol;
    let sel = ui.inspector_selected.min(total_items.saturating_sub(1));

    // 1. Linked Mentions (Backlinks)
    let bl_area = chunks[0];
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!(" ← Linked Mentions ({total_bl}) "),
            Style::default()
                .fg(if focused && sel < total_bl {
                    theme.accent
                } else {
                    theme.accent_soft
                })
                .add_modifier(Modifier::BOLD),
        )]))
        .style(Style::default().bg(theme.bg_sidebar)),
        Rect::new(bl_area.x, bl_area.y, bl_area.width, 1),
    );

    if detail.backlinks.is_empty() {
        if bl_area.height > 2 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "  No incoming links yet.",
                    theme.muted_style(),
                )),
                Rect::new(bl_area.x, bl_area.y + 1, bl_area.width, 1),
            );
        }
    } else {
        let mut y = bl_area.y + 1;
        for (idx, bl) in detail.backlinks.iter().enumerate() {
            if y >= bl_area.y + bl_area.height {
                break;
            }
            let is_sel = focused && sel == idx;
            let row_bg = if is_sel {
                theme.bg_selected
            } else {
                theme.bg_sidebar
            };
            let title_line = Line::from(vec![
                Span::styled(
                    if is_sel { " ▸ " } else { " ▪ " },
                    Style::default().fg(theme.accent),
                ),
                Span::styled(
                    truncate_width(&bl.source_title, w.saturating_sub(4)),
                    Style::default()
                        .fg(theme.fg_primary)
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
            let r1 = Rect::new(bl_area.x, y, bl_area.width, 1);
            frame.render_widget(
                Paragraph::new(title_line).style(Style::default().bg(row_bg)),
                r1,
            );
            hitmap.add(r1, HitTarget::InspectorBacklink(idx));
            y += 1;

            if y < bl_area.y + bl_area.height && !bl.context_snippet.is_empty() {
                let snip = Line::from(vec![
                    Span::raw("   "),
                    Span::styled(
                        truncate_width(&bl.context_snippet, w.saturating_sub(4)),
                        theme.muted_style(),
                    ),
                ]);
                let r2 = Rect::new(bl_area.x, y, bl_area.width, 1);
                frame.render_widget(Paragraph::new(snip).style(Style::default().bg(row_bg)), r2);
                hitmap.add(r2, HitTarget::InspectorBacklink(idx));
                y += 1;
            }
        }
    }

    // 2. Outgoing Links
    let ol_area = chunks[1];
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!(" → Outgoing Links ({total_ol}) "),
            Style::default()
                .fg(if focused && sel >= total_bl {
                    theme.accent
                } else {
                    theme.accent_soft
                })
                .add_modifier(Modifier::BOLD),
        )]))
        .style(Style::default().bg(theme.bg_sidebar)),
        Rect::new(ol_area.x, ol_area.y, ol_area.width, 1),
    );

    if detail.outgoing_links.is_empty() {
        if ol_area.height > 1 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "  No outgoing wikilinks.",
                    theme.muted_style(),
                )),
                Rect::new(ol_area.x, ol_area.y + 1, ol_area.width, 1),
            );
        }
    } else {
        for (y, (idx, ol)) in (ol_area.y.saturating_add(1)
            ..ol_area.y.saturating_add(ol_area.height))
            .zip(detail.outgoing_links.iter().enumerate())
        {
            let flat_idx = total_bl + idx;
            let is_sel = focused && sel == flat_idx;
            let row_bg = if is_sel {
                theme.bg_selected
            } else {
                theme.bg_sidebar
            };
            let glyph = if ol.resolved { "→" } else { "◌" };
            let style = if ol.resolved {
                Style::default().fg(theme.fg_primary)
            } else {
                Style::default().fg(theme.warning)
            };
            let line = Line::from(vec![
                Span::styled(
                    format!(" {glyph} "),
                    Style::default().fg(if ol.resolved {
                        theme.accent
                    } else {
                        theme.warning
                    }),
                ),
                Span::styled(truncate_width(&ol.target_title, w.saturating_sub(5)), style),
            ]);
            let r = Rect::new(ol_area.x, y, ol_area.width, 1);
            frame.render_widget(Paragraph::new(line).style(Style::default().bg(row_bg)), r);
            hitmap.add(r, HitTarget::InspectorOutgoing(idx));
        }
    }

    // 3. Live Outline & Tags (updates on every keystroke in inline edit mode!)
    if show_outline && chunks[2].height >= 3 {
        let out_area = chunks[2];
        let heading_cnt = detail.parsed.headings.len();
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" § Outline ({heading_cnt}) "),
                Style::default()
                    .fg(theme.accent_soft)
                    .add_modifier(Modifier::BOLD),
            )))
            .style(Style::default().bg(theme.bg_sidebar)),
            Rect::new(out_area.x, out_area.y, out_area.width, 1),
        );

        let mut y = out_area.y + 1;
        let tags_rows: u16 = if detail.summary.tags.is_empty() || out_area.height < 4 {
            0
        } else {
            1
        };
        let heading_limit_y = out_area.bottom().saturating_sub(tags_rows);

        if detail.parsed.headings.is_empty() {
            if y < heading_limit_y {
                frame.render_widget(
                    Paragraph::new(Span::styled("  No headings yet.", theme.muted_style())),
                    Rect::new(out_area.x, y, out_area.width, 1),
                );
            }
        } else {
            for h in &detail.parsed.headings {
                if y >= heading_limit_y {
                    break;
                }
                let indent = " ".repeat((h.level.saturating_sub(1) as usize).min(3));
                let avail = w.saturating_sub(indent.len() + 5);
                let line = Line::from(vec![
                    Span::raw(format!(" {indent}")),
                    Span::styled(
                        format!("H{} ", h.level),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        truncate_width(&h.text, avail),
                        Style::default().fg(theme.fg_primary),
                    ),
                ]);
                frame.render_widget(
                    Paragraph::new(line).style(Style::default().bg(theme.bg_sidebar)),
                    Rect::new(out_area.x, y, out_area.width, 1),
                );
                y += 1;
            }
        }

        if tags_rows > 0 {
            let tags_y = out_area.bottom().saturating_sub(1);
            let tags_joined = detail
                .summary
                .tags
                .iter()
                .map(|t| format!("#{t}"))
                .collect::<Vec<_>>()
                .join(" ");
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {}", truncate_width(&tags_joined, w.saturating_sub(2))),
                    Style::default().fg(theme.tag_fg),
                ))
                .style(Style::default().bg(theme.bg_sidebar)),
                Rect::new(out_area.x, tags_y, out_area.width, 1),
            );
        }
    }

    // 4. Local Graph Mini-Stats Card
    let stats_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.divider))
        .style(Style::default().bg(theme.bg_card));
    let stats_inner = stats_block.inner(chunks[3]);
    frame.render_widget(stats_block, chunks[3]);

    let done_tasks = detail.parsed.tasks.iter().filter(|t| t.checked).count();
    let total_tasks = detail.parsed.tasks.len();
    let mtime_label = format_mtime_short(detail.summary.mtime_ms);
    let stats_lines = vec![
        Line::from(Span::styled(
            " ◈ Note Stats",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("  Words: ", theme.muted_style()),
            Span::styled(
                format!("{:<6}", detail.summary.word_count),
                Style::default().fg(theme.fg_primary),
            ),
            Span::styled("Tags: ", theme.muted_style()),
            Span::styled(
                detail.summary.tags.len().to_string(),
                Style::default().fg(theme.accent_soft),
            ),
        ]),
        Line::from(vec![
            Span::styled("  In: ", theme.muted_style()),
            Span::styled(
                format!("{:<4}", detail.backlinks.len()),
                Style::default().fg(theme.success),
            ),
            Span::styled("Out: ", theme.muted_style()),
            Span::styled(
                format!("{:<4}", detail.outgoing_links.len()),
                Style::default().fg(theme.info),
            ),
            Span::styled("Tasks: ", theme.muted_style()),
            Span::styled(
                format!("{done_tasks}/{total_tasks}"),
                Style::default().fg(theme.fg_primary),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Updated: ", theme.muted_style()),
            Span::styled(
                truncate_width(&mtime_label, w.saturating_sub(12)),
                theme.muted_style(),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(stats_lines), stats_inner);
}

fn format_mtime_short(mtime_ms: i64) -> String {
    if mtime_ms <= 0 {
        return "—".to_string();
    }
    let secs = mtime_ms / 1000;
    let dt: Option<DateTime<Local>> = Local.timestamp_opt(secs, 0).single();
    dt.map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "—".to_string())
}
