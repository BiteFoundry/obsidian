//! Right Inspector Pane (`B` toggle):
//! - Linked Mentions (Backlinks) with surrounding snippet context (`Enter` jumps to linking note)
//! - Outgoing Links (with resolved `→` vs unresolved `◌` indicator)
//! - Local Graph Mini-Stats (words, backlinks, outgoing links, tags, tasks, modified time).

use bitefoundry_html::{wrap_styled_spans, WrapPrefix};
use chrono::{DateTime, Local, TimeZone};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::{
    models::BacklinkEntry,
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{HitMap, HitTarget},
        sidebar::selection_offset,
        truncate_width,
    },
};

/// Computes the starting backlink index so `selected` fits within `avail_rows` visual lines.
pub fn backlinks_scroll_offset(
    backlinks: &[BacklinkEntry],
    selected: usize,
    avail_rows: usize,
) -> usize {
    if backlinks.is_empty() || avail_rows == 0 {
        return 0;
    }
    let sel = selected.min(backlinks.len() - 1);
    let item_rows = |bl: &BacklinkEntry| -> usize {
        if bl.context_snippet.is_empty() {
            1
        } else {
            2
        }
    };

    // Fast path: if items `0..=sel` already fit in the viewport, start at 0.
    let prefix_rows: usize = backlinks[..=sel].iter().map(item_rows).sum();
    if prefix_rows <= avail_rows {
        return 0;
    }

    // Otherwise walk backwards from `sel` so `sel` is kept inside the viewport.
    let mut used = item_rows(&backlinks[sel]);
    let mut start = sel;
    while start > 0 {
        let prev_h = item_rows(&backlinks[start - 1]);
        if used + prev_h > avail_rows {
            break;
        }
        used += prev_h;
        start -= 1;
    }
    start
}

/// Minimum inspector height that leaves room for the Outline section; shorter panes hide it.
pub const OUTLINE_MIN_HEIGHT: u16 = 18;

/// Draws the inspector and returns whether its Outline section was shown: on a short pane it is
/// hidden, and its headings must then drop out of keyboard navigation.
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) -> bool {
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
        return false;
    }

    let Some(detail) = app.active_note.as_ref() else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  Select a note to inspect.",
                theme.muted_style(),
            ))
            .wrap(Wrap { trim: false }),
            inner,
        );
        return false;
    };

    let w = inner.width as usize;
    let done_tasks = detail.parsed.tasks.iter().filter(|t| t.checked).count();
    let total_tasks = detail.parsed.tasks.len();
    let mtime_label = format_mtime_short(detail.summary.mtime_ms);
    let stat_cont = [Span::raw("  ")];
    let stat_prefix = WrapPrefix {
        first: &[],
        cont: &stat_cont,
    };
    let mut stats_lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
        truncate_width(" ◈ Note Stats", w),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ))];
    stats_lines.extend(wrap_styled_spans(
        &[
            Span::styled("  Words: ", theme.muted_style()),
            Span::styled(
                detail.summary.word_count.to_string(),
                Style::default().fg(theme.fg_primary),
            ),
            Span::styled("  Tags: ", theme.muted_style()),
            Span::styled(
                detail.summary.tags.len().to_string(),
                Style::default().fg(theme.accent_soft),
            ),
        ],
        w,
        stat_prefix,
    ));
    stats_lines.extend(wrap_styled_spans(
        &[
            Span::styled("  In: ", theme.muted_style()),
            Span::styled(
                detail.backlinks.len().to_string(),
                Style::default().fg(theme.success),
            ),
            Span::styled("  Out: ", theme.muted_style()),
            Span::styled(
                detail.outgoing_links.len().to_string(),
                Style::default().fg(theme.info),
            ),
            Span::styled("  Tasks: ", theme.muted_style()),
            Span::styled(
                format!("{done_tasks}/{total_tasks}"),
                Style::default().fg(theme.fg_primary),
            ),
        ],
        w,
        stat_prefix,
    ));
    stats_lines.extend(wrap_styled_spans(
        &[
            Span::styled("  Updated: ", theme.muted_style()),
            Span::styled(mtime_label, theme.muted_style()),
        ],
        w,
        stat_prefix,
    ));
    let stats_h = (stats_lines.len() as u16 + 1)
        .min(inner.height.saturating_sub(4))
        .max(4);

    let show_outline = inner.height >= OUTLINE_MIN_HEIGHT;
    let chunks = if show_outline {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(24),
                Constraint::Min(5),
                Constraint::Length(stats_h),
            ])
            .split(inner)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(48),
                Constraint::Percentage(32),
                Constraint::Length(0),
                Constraint::Length(stats_h),
            ])
            .split(inner)
    };

    let outline_visible = show_outline && chunks[2].height >= 3;
    let total_bl = detail.backlinks.len();
    let total_ol = detail.outgoing_links.len();
    let total_h = if outline_visible {
        detail.parsed.headings.len()
    } else {
        0
    };
    let total_items = total_bl + total_ol + total_h;
    let sel = ui.inspector_selected.min(total_items.saturating_sub(1));

    // 1. Linked Mentions (Backlinks)
    let bl_area = chunks[0];
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            truncate_width(&format!(" ← Linked Mentions ({total_bl}) "), w),
            Style::default()
                .fg(if focused && total_bl > 0 && sel < total_bl {
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
        if bl_area.height > 1 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "  No incoming links yet.",
                    theme.muted_style(),
                ))
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(theme.bg_sidebar)),
                Rect::new(
                    bl_area.x,
                    bl_area.y + 1,
                    bl_area.width,
                    bl_area.height.saturating_sub(1),
                ),
            );
        }
    } else {
        let bl_avail_rows = bl_area.height.saturating_sub(1) as usize;
        let bl_offset = if sel < total_bl {
            backlinks_scroll_offset(&detail.backlinks, sel, bl_avail_rows)
        } else {
            0
        };
        let mut y = bl_area.y + 1;
        for (idx, bl) in detail.backlinks.iter().enumerate().skip(bl_offset) {
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
            truncate_width(&format!(" → Outgoing Links ({total_ol}) "), w),
            Style::default()
                .fg(
                    if focused && total_ol > 0 && sel >= total_bl && sel < total_bl + total_ol {
                        theme.accent
                    } else {
                        theme.accent_soft
                    },
                )
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
                ))
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(theme.bg_sidebar)),
                Rect::new(
                    ol_area.x,
                    ol_area.y + 1,
                    ol_area.width,
                    ol_area.height.saturating_sub(1),
                ),
            );
        }
    } else {
        let ol_avail_rows = ol_area.height.saturating_sub(1) as usize;
        let ol_offset = if sel >= total_bl && sel < total_bl + total_ol {
            selection_offset(sel - total_bl, ol_avail_rows, total_ol)
        } else {
            0
        };
        for (row, (idx, ol)) in detail
            .outgoing_links
            .iter()
            .enumerate()
            .skip(ol_offset)
            .take(ol_avail_rows)
            .enumerate()
        {
            let y = ol_area.y.saturating_add(1 + row as u16);
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
    if outline_visible {
        let out_area = chunks[2];
        let heading_cnt = detail.parsed.headings.len();
        let outline_base = total_bl + total_ol;
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_width(&format!(" § Outline ({heading_cnt}) "), w),
                Style::default()
                    .fg(if focused && total_h > 0 && sel >= outline_base {
                        theme.accent
                    } else {
                        theme.accent_soft
                    })
                    .add_modifier(Modifier::BOLD),
            )))
            .style(Style::default().bg(theme.bg_sidebar)),
            Rect::new(out_area.x, out_area.y, out_area.width, 1),
        );

        let start_y = out_area.y + 1;
        let tags_rows: u16 = if detail.summary.tags.is_empty() || out_area.height < 4 {
            0
        } else {
            1
        };
        let heading_limit_y = out_area.bottom().saturating_sub(tags_rows);
        let h_avail_rows = heading_limit_y.saturating_sub(start_y) as usize;

        if detail.parsed.headings.is_empty() {
            if start_y < heading_limit_y {
                frame.render_widget(
                    Paragraph::new(Span::styled("  No headings yet.", theme.muted_style()))
                        .wrap(Wrap { trim: false })
                        .style(Style::default().bg(theme.bg_sidebar)),
                    Rect::new(
                        out_area.x,
                        start_y,
                        out_area.width,
                        heading_limit_y.saturating_sub(start_y),
                    ),
                );
            }
        } else if h_avail_rows > 0 {
            let h_offset = if sel >= outline_base {
                selection_offset(sel - outline_base, h_avail_rows, total_h)
            } else {
                0
            };
            for (row, (idx, h)) in detail
                .parsed
                .headings
                .iter()
                .enumerate()
                .skip(h_offset)
                .take(h_avail_rows)
                .enumerate()
            {
                let y = start_y + row as u16;
                let flat_idx = outline_base + idx;
                let is_sel = focused && sel == flat_idx;
                let row_bg = if is_sel {
                    theme.bg_selected
                } else {
                    theme.bg_sidebar
                };
                let indent = " ".repeat((h.level.saturating_sub(1) as usize).min(3));
                let marker = if is_sel { "▸" } else { " " };
                let avail = w.saturating_sub(indent.len() + 5);
                let line = Line::from(vec![
                    Span::styled(
                        marker.to_string(),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(indent),
                    Span::styled(
                        format!("H{} ", h.level),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        truncate_width(&h.text, avail),
                        Style::default()
                            .fg(theme.fg_primary)
                            .add_modifier(if is_sel {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ]);
                let row_rect = Rect::new(out_area.x, y, out_area.width, 1);
                frame.render_widget(
                    Paragraph::new(line).style(Style::default().bg(row_bg)),
                    row_rect,
                );
                hitmap.add(row_rect, HitTarget::InspectorOutline(idx));
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
    frame.render_widget(
        Paragraph::new(stats_lines)
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(theme.bg_card)),
        stats_inner,
    );
    outline_visible
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
