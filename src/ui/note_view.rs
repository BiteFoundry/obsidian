//! Center Canvas Mode A: Live Obsidian Note Reader & Interactive Markdown Canvas.
//!
//! Renders:
//! - YAML Frontmatter Properties Strip (key/value pills + `#tag` chips + aliases)
//! - Rich Obsidian Markdown: Headings (`H1`–`H3`), semantic Callout cards (`┃ ✦ TIP ...`),
//!   interactive `- [ ]` / `- [x]` checkboxes (`Space`/`x`), clickable `[[wikilinks]]` (`Enter`),
//!   inline `![[image.png]]` previews (`v` HD modal), bordered Code blocks (`y` copy),
//!   Markdown Tables, Blockquotes, and Dividers.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};

use crate::{
    config::PageWidthMode,
    models::{InlineSpan, NoteBlock, NoteDetail},
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::html_preview::{block_is_html, is_html_code_language, note_has_html},
        theme::Theme,
        truncate_width,
    },
};

struct RenderedLine {
    line: Line<'static>,
    block_idx: Option<usize>,
    task_line_no: Option<usize>,
    has_wikilink: bool,
    image_idx: Option<usize>,
    html_pill: Option<(u16, u16, usize)>,
    action_pills: Vec<(u16, u16, ButtonId)>,
    bg_override: Option<ratatui::style::Color>,
}

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState, hitmap: &mut HitMap) {
    let theme = ui.theme.clone();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.bg_canvas)),
        area,
    );
    hitmap.add(area, HitTarget::Pane(Focus::Canvas));

    let Some(detail) = app.active_note.as_ref() else {
        let msg = vec![
            Line::from(""),
            Line::from(Span::styled(
                "   ◆ Obsidian Terminal Workspace",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "   Press Ctrl+O to open a note, D for Today's Daily Note, n to create a note, or G for Knowledge Graph.",
                theme.muted_style(),
            )),
        ];
        frame.render_widget(Paragraph::new(msg), area);
        return;
    };

    // Centered editorial measure or full width
    let content_area = match ui.page_width {
        PageWidthMode::Centered if area.width > 92 => {
            let pad = (area.width - 86) / 2;
            Rect::new(area.x + pad, area.y, 86, area.height)
        }
        _ => Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        ),
    };

    let width = content_area.width as usize;
    if width < 10 || content_area.height < 3 {
        return;
    }

    let focused = ui.focus == Focus::Canvas;
    let selected_block = ui
        .canvas_selected_block
        .min(detail.parsed.blocks.len().saturating_sub(1));

    let lines = build_canvas_lines(detail, app, ui, &theme, width, selected_block, focused);

    // Find visual line of the selected block so scrolling follows selection smoothly
    let total_lines = lines.len();
    let visible = content_area.height as usize;
    let max_scroll = total_lines.saturating_sub(visible);
    let selected_visual_row = lines
        .iter()
        .position(|rl| rl.block_idx == Some(selected_block))
        .unwrap_or(0);

    let scroll = if total_lines <= visible {
        0
    } else if !ui.canvas_follow_selected {
        ui.canvas_scroll.min(max_scroll)
    } else if selected_visual_row >= visible.saturating_sub(4) {
        selected_visual_row
            .saturating_sub(visible / 2)
            .min(max_scroll)
    } else {
        0
    };
    ui.canvas_scroll = scroll;

    for (i, rl) in lines.iter().skip(scroll).take(visible).enumerate() {
        let y = content_area.y + i as u16;
        let row_rect = Rect::new(content_area.x, y, content_area.width, 1);
        let bg = rl.bg_override.unwrap_or(theme.bg_canvas);
        frame.render_widget(
            Paragraph::new(rl.line.clone()).style(Style::default().bg(bg)),
            row_rect,
        );
        if let Some(b_idx) = rl.block_idx {
            hitmap.add(row_rect, HitTarget::CanvasBlock(b_idx));
            if let Some(t_ln) = rl.task_line_no {
                hitmap.add_line(content_area.x, y, 6, HitTarget::TaskCheckbox { line_no: t_ln });
            } else if rl.has_wikilink {
                hitmap.add(row_rect, HitTarget::WikilinkBlock(b_idx));
            }
            if let Some(img_i) = rl.image_idx {
                hitmap.add(row_rect, HitTarget::ImageChip(img_i));
            }
        }
        if let Some((x_off, w, b_idx)) = rl.html_pill {
            if x_off < content_area.width {
                let clamped_w = w.min(content_area.width.saturating_sub(x_off));
                hitmap.add_line(
                    content_area.x + x_off,
                    y,
                    clamped_w,
                    HitTarget::HtmlChip(b_idx),
                );
            }
        }
        for &(x_off, w, btn) in &rl.action_pills {
            if x_off < content_area.width {
                let clamped_w = w.min(content_area.width.saturating_sub(x_off));
                hitmap.add_line(content_area.x + x_off, y, clamped_w, HitTarget::Button(btn));
            }
        }
    }
}

fn build_canvas_lines(
    detail: &NoteDetail,
    app: &AppState,
    ui: &UiState,
    theme: &Theme,
    width: usize,
    selected_block: usize,
    focused: bool,
) -> Vec<RenderedLine> {
    let mut out: Vec<RenderedLine> = Vec::new();
    let push_plain = |out: &mut Vec<RenderedLine>, line: Line<'static>, bg: Option<ratatui::style::Color>| {
        out.push(RenderedLine {
            line,
            block_idx: None,
            task_line_no: None,
            has_wikilink: false,
            image_idx: None,
            html_pill: None,
            action_pills: Vec::new(),
            bg_override: bg,
        });
    };

    // 1. Note Title + Bookmark indicator
    let bm = if detail.summary.is_bookmarked { " ★" } else { "" };
    push_plain(
        &mut out,
        Line::from(vec![
            Span::styled(
                truncate_width(&detail.summary.title, width.saturating_sub(4)),
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(bm.to_string(), Style::default().fg(theme.warning)),
        ]),
        None,
    );

    // 1b. Interactive Note Action Pill Bar (Bookmark, Width, Append, New, Edit, Links, Hrefs, Img)
    {
        let bm_label = if detail.summary.is_bookmarked {
            "[★ Mark]"
        } else {
            "[☆ Mark]"
        };
        let width_label = match ui.page_width {
            PageWidthMode::Centered => "[w Wide]",
            PageWidthMode::Full => "[w Center]",
        };
        let img_label = if detail.images.is_empty() {
            "[v Img]".to_string()
        } else {
            format!("[v Img({})]", detail.images.len())
        };
        let has_html = note_has_html(detail) || !app.html_files.is_empty();
        let pills: Vec<(String, bool, ButtonId)> = vec![
            (bm_label.to_string(), detail.summary.is_bookmarked, ButtonId::ToggleBookmark),
            (width_label.to_string(), ui.page_width == PageWidthMode::Full, ButtonId::ToggleWidth),
            ("[a Append]".to_string(), false, ButtonId::QuickAppend),
            ("[n New]".to_string(), false, ButtonId::NewNote),
            ("[✎ Edit]".to_string(), false, ButtonId::EditNote),
            ("[u Links]".to_string(), false, ButtonId::LinkExtractor),
            ("[h Hrefs]".to_string(), ui.show_inline_hrefs, ButtonId::ToggleHrefs),
            (img_label, !detail.images.is_empty(), ButtonId::ImagePreview),
            ("[H: HTML]".to_string(), has_html, ButtonId::HtmlPreview),
        ];

        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut hit_pills: Vec<(u16, u16, ButtonId)> = Vec::new();
        let mut x_off = 0usize;
        for (label, active, btn) in pills {
            let lw = unicode_width::UnicodeWidthStr::width(label.as_str());
            let gap = if x_off > 0 { 1 } else { 0 };
            if x_off + gap + lw > width {
                break;
            }
            if gap > 0 {
                spans.push(Span::raw(" "));
                x_off += 1;
            }
            let style = if active {
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.bg_card)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.muted_style()
            };
            hit_pills.push((x_off as u16, lw as u16, btn));
            spans.push(Span::styled(label, style));
            x_off += lw;
        }
        if !spans.is_empty() {
            out.push(RenderedLine {
                line: Line::from(spans),
                block_idx: None,
                task_line_no: None,
                has_wikilink: false,
                image_idx: None,
                html_pill: None,
                action_pills: hit_pills,
                bg_override: None,
            });
        }
    }

    // 2. YAML Frontmatter Properties Strip (clean pills + #tags + aliases)
    let mut prop_spans: Vec<Span<'static>> = Vec::new();
    for (k, v) in &detail.summary.frontmatter {
        if k == "tags" || k == "tag" || k == "aliases" || k == "alias" {
            continue;
        }
        let val_str = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|i| i.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            _ => continue,
        };
        if !prop_spans.is_empty() {
            prop_spans.push(Span::raw(" "));
        }
        prop_spans.push(Span::styled(
            format!(" {k}: {val_str} "),
            theme.prop_pill_style(),
        ));
    }
    if !detail.summary.aliases.is_empty() {
        if !prop_spans.is_empty() {
            prop_spans.push(Span::raw(" "));
        }
        prop_spans.push(Span::styled(
            format!(" alias: {} ", detail.summary.aliases.join(", ")),
            theme.prop_pill_style(),
        ));
    }
    if !prop_spans.is_empty() {
        push_plain(&mut out, Line::from(prop_spans), None);
    }

    if !detail.summary.tags.is_empty() {
        let mut tag_spans: Vec<Span<'static>> = Vec::new();
        for (i, t) in detail.summary.tags.iter().enumerate() {
            if i > 0 {
                tag_spans.push(Span::raw(" "));
            }
            tag_spans.push(Span::styled(format!(" #{t} "), theme.tag_pill_style()));
        }
        push_plain(&mut out, Line::from(tag_spans), None);
    }

    // Subtle divider under frontmatter
    push_plain(
        &mut out,
        Line::from(Span::styled(
            "─".repeat(width.min(84)),
            Style::default().fg(theme.divider),
        )),
        None,
    );

    // 3. Rendered Obsidian Markdown blocks
    let mut image_counter = 0usize;
    for (b_idx, block) in detail.parsed.blocks.iter().enumerate() {
        let is_sel = focused && b_idx == selected_block;
        let sel_bg = if is_sel { Some(theme.bg_selected) } else { None };
        let has_wl = block.first_wikilink().is_some();

        match block {
            NoteBlock::Heading {
                level,
                text,
                spans,
                ..
            } => {
                // Skip duplicate top H1 if identical to note title and first block
                if b_idx == 0 && *level == 1 && text.eq_ignore_ascii_case(&detail.summary.title) {
                    continue;
                }
                if !out.is_empty() {
                    push_plain(&mut out, Line::from(""), None);
                }
                let prefix = match level {
                    1 => "▍ ",
                    2 => "## ",
                    3 => "### ",
                    _ => "#### ",
                };
                let style = match level {
                    1 => Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                    2 => Style::default()
                        .fg(theme.accent_soft)
                        .add_modifier(Modifier::BOLD),
                    _ => Style::default()
                        .fg(theme.fg_primary)
                        .add_modifier(Modifier::BOLD),
                };
                let mut line_spans = vec![Span::styled(prefix.to_string(), style)];
                line_spans.extend(render_inline_spans(spans, theme, ui.show_inline_hrefs, style));
                out.push(RenderedLine {
                    line: Line::from(line_spans),
                    block_idx: Some(b_idx),
                    task_line_no: None,
                    has_wikilink: has_wl,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: sel_bg,
                });
            }

            NoteBlock::Paragraph { spans, .. } => {
                if block_is_html(detail, b_idx) {
                    let pill = "[H: HTML]";
                    let pill_w = unicode_width::UnicodeWidthStr::width(pill) as u16;
                    out.push(RenderedLine {
                        line: Line::from(vec![
                            Span::styled(" ◈ Embedded HTML · ", theme.muted_style()),
                            Span::styled(
                                pill,
                                Style::default()
                                    .fg(theme.accent)
                                    .bg(theme.bg_card)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        block_idx: Some(b_idx),
                        task_line_no: None,
                        has_wikilink: false,
                        image_idx: None,
                        html_pill: Some((19, pill_w, b_idx)),
                        action_pills: Vec::new(),
                        bg_override: Some(if is_sel { theme.bg_selected } else { theme.bg_card }),
                    });
                }
                let mut rendered = render_inline_spans(
                    spans,
                    theme,
                    ui.show_inline_hrefs,
                    Style::default().fg(theme.fg_primary),
                );
                if is_sel {
                    rendered.insert(
                        0,
                        Span::styled("▎ ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                    );
                }
                for wrapped in wrap_spans(rendered, width) {
                    out.push(RenderedLine {
                        line: wrapped,
                        block_idx: Some(b_idx),
                        task_line_no: None,
                        has_wikilink: has_wl,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: sel_bg,
                    });
                }
                push_plain(&mut out, Line::from(""), None);
            }

            NoteBlock::Callout {
                callout_kind,
                title,
                collapsed,
                body_lines,
                ..
            } => {
                let c_color = theme.callout_color(*callout_kind);
                let card_bg = if is_sel { theme.bg_selected } else { theme.bg_card };
                let fold = if *collapsed { "▸ " } else { "" };
                let header = Line::from(vec![
                    Span::styled("┃ ", Style::default().fg(c_color).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("{fold}{} {} ", callout_kind.glyph(), callout_kind.label()),
                        Style::default().fg(c_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if title.eq_ignore_ascii_case(callout_kind.label()) {
                            String::new()
                        } else {
                            format!("· {title}")
                        },
                        Style::default()
                            .fg(theme.fg_primary)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]);
                out.push(RenderedLine {
                    line: header,
                    block_idx: Some(b_idx),
                    task_line_no: None,
                    has_wikilink: has_wl,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: Some(card_bg),
                });

                for bl in body_lines {
                    let body_spans = render_inline_spans(
                        bl,
                        theme,
                        ui.show_inline_hrefs,
                        Style::default().fg(theme.fg_secondary),
                    );
                    for mut wrapped in wrap_spans(body_spans, width.saturating_sub(3)) {
                        wrapped.spans.insert(
                            0,
                            Span::styled(
                                "┃ ",
                                Style::default().fg(c_color).add_modifier(Modifier::BOLD),
                            ),
                        );
                        out.push(RenderedLine {
                            line: wrapped,
                            block_idx: Some(b_idx),
                            task_line_no: None,
                            has_wikilink: has_wl,
                            image_idx: None,
                            html_pill: None,
                            action_pills: Vec::new(),
                            bg_override: Some(card_bg),
                        });
                    }
                }
                push_plain(&mut out, Line::from(""), None);
            }

            NoteBlock::Task {
                checked,
                indent,
                spans,
                line_no,
                ..
            } => {
                let pad = " ".repeat((*indent).min(8));
                let cursor = if is_sel { "▸ " } else { "  " };
                let checkbox = if *checked { "☑ " } else { "☐ " };
                let base_style = if *checked {
                    theme.muted_style().add_modifier(Modifier::CROSSED_OUT)
                } else {
                    Style::default().fg(theme.fg_primary)
                };
                let mut line_spans = vec![
                    Span::styled(
                        format!("{pad}{cursor}"),
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        checkbox.to_string(),
                        Style::default()
                            .fg(if *checked { theme.success } else { theme.accent })
                            .add_modifier(Modifier::BOLD),
                    ),
                ];
                line_spans.extend(render_inline_spans(
                    spans,
                    theme,
                    ui.show_inline_hrefs,
                    base_style,
                ));
                out.push(RenderedLine {
                    line: Line::from(line_spans),
                    block_idx: Some(b_idx),
                    task_line_no: Some(*line_no),
                    has_wikilink: has_wl,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: sel_bg,
                });
            }

            NoteBlock::BulletItem { indent, spans, .. } => {
                let pad = " ".repeat((*indent).min(8));
                let mut line_spans = vec![Span::styled(
                    format!("{pad}  • "),
                    Style::default().fg(theme.accent_soft),
                )];
                line_spans.extend(render_inline_spans(
                    spans,
                    theme,
                    ui.show_inline_hrefs,
                    Style::default().fg(theme.fg_primary),
                ));
                for wrapped in wrap_spans(line_spans, width) {
                    out.push(RenderedLine {
                        line: wrapped,
                        block_idx: Some(b_idx),
                        task_line_no: None,
                        has_wikilink: has_wl,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: sel_bg,
                    });
                }
            }

            NoteBlock::NumberedItem {
                number,
                indent,
                spans,
                ..
            } => {
                let pad = " ".repeat((*indent).min(8));
                let mut line_spans = vec![Span::styled(
                    format!("{pad}  {number}. "),
                    Style::default().fg(theme.accent_soft),
                )];
                line_spans.extend(render_inline_spans(
                    spans,
                    theme,
                    ui.show_inline_hrefs,
                    Style::default().fg(theme.fg_primary),
                ));
                for wrapped in wrap_spans(line_spans, width) {
                    out.push(RenderedLine {
                        line: wrapped,
                        block_idx: Some(b_idx),
                        task_line_no: None,
                        has_wikilink: has_wl,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: sel_bg,
                    });
                }
            }

            NoteBlock::CodeBlock { language, code, .. } => {
                let code_bg = if is_sel { theme.bg_selected } else { theme.bg_code };
                let lang_label = if language.is_empty() { "code" } else { language.as_str() };
                let is_html = is_html_code_language(language) && !code.trim().is_empty();
                let html_suffix = if is_html { " [H: HTML]" } else { "" };
                let bar_w = width
                    .saturating_sub(lang_label.len() + 14 + html_suffix.len())
                    .min(50);
                let mut hdr_spans = vec![
                    Span::styled("╭─ ", Style::default().fg(theme.border)),
                    Span::styled(
                        format!("{lang_label} "),
                        Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("─".repeat(bar_w), Style::default().fg(theme.border)),
                    Span::styled(" [y copy]", theme.muted_style()),
                ];
                let html_pill = if is_html {
                    let prefix_w: usize = hdr_spans
                        .iter()
                        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                        .sum();
                    hdr_spans.push(Span::raw(" "));
                    hdr_spans.push(Span::styled(
                        "[H: HTML]",
                        Style::default()
                            .fg(theme.accent)
                            .bg(theme.bg_card)
                            .add_modifier(Modifier::BOLD),
                    ));
                    Some(((prefix_w + 1) as u16, 9u16, b_idx))
                } else {
                    None
                };
                out.push(RenderedLine {
                    line: Line::from(hdr_spans),
                    block_idx: Some(b_idx),
                    task_line_no: None,
                    has_wikilink: false,
                    image_idx: None,
                    html_pill,
                    action_pills: Vec::new(),
                    bg_override: Some(code_bg),
                });
                for code_line in code.lines() {
                    out.push(RenderedLine {
                        line: Line::from(vec![
                            Span::styled("│ ", Style::default().fg(theme.border)),
                            Span::styled(
                                truncate_width(code_line, width.saturating_sub(3)),
                                Style::default().fg(theme.code_fg),
                            ),
                        ]),
                        block_idx: Some(b_idx),
                        task_line_no: None,
                        has_wikilink: false,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: Some(code_bg),
                    });
                }
                out.push(RenderedLine {
                    line: Line::from(Span::styled(
                        format!("╰{}", "─".repeat(width.saturating_sub(2).min(64))),
                        Style::default().fg(theme.border),
                    )),
                    block_idx: Some(b_idx),
                    task_line_no: None,
                    has_wikilink: false,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: Some(code_bg),
                });
                push_plain(&mut out, Line::from(""), None);
            }

            NoteBlock::Blockquote { lines: qlines, .. } => {
                for ql in qlines {
                    let mut spans = vec![Span::styled("▎ ", Style::default().fg(theme.fg_muted))];
                    spans.extend(render_inline_spans(
                        ql,
                        theme,
                        ui.show_inline_hrefs,
                        theme.quote_style(),
                    ));
                    out.push(RenderedLine {
                        line: Line::from(spans),
                        block_idx: Some(b_idx),
                        task_line_no: None,
                        has_wikilink: has_wl,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: sel_bg,
                    });
                }
                push_plain(&mut out, Line::from(""), None);
            }

            NoteBlock::Table { headers, rows, .. } => {
                let cols = headers.len().max(1);
                let col_w = (width.saturating_sub(cols * 3 + 1) / cols).clamp(8, 28);
                let hdr_cells: Vec<String> = headers
                    .iter()
                    .map(|h| format!("{:<w$}", truncate_width(h, col_w), w = col_w))
                    .collect();
                out.push(RenderedLine {
                    line: Line::from(Span::styled(
                        format!("│ {} │", hdr_cells.join(" │ ")),
                        Style::default()
                            .fg(theme.accent_soft)
                            .add_modifier(Modifier::BOLD),
                    )),
                    block_idx: Some(b_idx),
                    task_line_no: None,
                    has_wikilink: false,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: Some(theme.bg_card),
                });
                let sep_cells: Vec<String> = (0..cols).map(|_| "─".repeat(col_w)).collect();
                out.push(RenderedLine {
                    line: Line::from(Span::styled(
                        format!("├─{}─┤", sep_cells.join("─┼─")),
                        Style::default().fg(theme.border),
                    )),
                    block_idx: Some(b_idx),
                    task_line_no: None,
                    has_wikilink: false,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: None,
                });
                for r in rows {
                    let cells: Vec<String> = (0..cols)
                        .map(|ci| {
                            let val = r.get(ci).map(String::as_str).unwrap_or("");
                            format!("{:<w$}", truncate_width(val, col_w), w = col_w)
                        })
                        .collect();
                    out.push(RenderedLine {
                        line: Line::from(Span::styled(
                            format!("│ {} │", cells.join(" │ ")),
                            Style::default().fg(theme.fg_primary),
                        )),
                        block_idx: Some(b_idx),
                        task_line_no: None,
                        has_wikilink: false,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: sel_bg,
                    });
                }
                push_plain(&mut out, Line::from(""), None);
            }

            NoteBlock::ImageEmbed {
                raw_target,
                alt,
                resolved_path,
                ..
            } => {
                let img_idx = image_counter;
                image_counter += 1;
                let badge = if resolved_path.is_some() {
                    format!(" ▣ IMG: {alt} ({raw_target}) · [v HD Preview] ")
                } else {
                    format!(" ▣ IMG (missing): {raw_target} ")
                };
                out.push(RenderedLine {
                    line: Line::from(Span::styled(
                        truncate_width(&badge, width),
                        if resolved_path.is_some() {
                            theme.tag_pill_style()
                        } else {
                            Style::default().fg(theme.warning).bg(theme.bg_card)
                        },
                    )),
                    block_idx: Some(b_idx),
                    task_line_no: None,
                    has_wikilink: false,
                    image_idx: Some(img_idx),
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: sel_bg,
                });

                // Render inline halfblock preview if cached
                if app.cfg.inline_images {
                    if let Some(abs) = resolved_path {
                        let cache_key = format!("inline:{}", abs.to_string_lossy());
                        if let Some(rendered) = app.preview_cache.get(&cache_key) {
                            for img_line in &rendered.lines {
                                out.push(RenderedLine {
                                    line: img_line.clone(),
                                    block_idx: Some(b_idx),
                                    task_line_no: None,
                                    has_wikilink: false,
                                    image_idx: Some(img_idx),
                                    html_pill: None,
                                    action_pills: Vec::new(),
                                    bg_override: None,
                                });
                            }
                        }
                    }
                }
                push_plain(&mut out, Line::from(""), None);
            }

            NoteBlock::NoteEmbed {
                target,
                heading,
                resolved_path,
                ..
            } => {
                let label = match heading {
                    Some(h) => format!("![[{target}#{h}]]"),
                    None => format!("![[{target}]]"),
                };
                out.push(RenderedLine {
                    line: Line::from(vec![
                        Span::styled("┃ ↗ Embed: ", Style::default().fg(theme.accent)),
                        Span::styled(label, theme.wikilink_style(resolved_path.is_some())),
                    ]),
                    block_idx: Some(b_idx),
                    task_line_no: None,
                    has_wikilink: true,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: Some(theme.bg_card),
                });
                push_plain(&mut out, Line::from(""), None);
            }

            NoteBlock::Divider { .. } => {
                out.push(RenderedLine {
                    line: Line::from(Span::styled(
                        "─".repeat(width.min(72)),
                        Style::default().fg(theme.divider),
                    )),
                    block_idx: Some(b_idx),
                    task_line_no: None,
                    has_wikilink: false,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: sel_bg,
                });
            }
        }
    }

    out
}

fn render_inline_spans(
    spans: &[InlineSpan],
    theme: &Theme,
    show_hrefs: bool,
    base_style: Style,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for s in spans {
        match s {
            InlineSpan::Text { text } => out.push(Span::styled(text.clone(), base_style)),
            InlineSpan::Bold { text } => out.push(Span::styled(
                text.clone(),
                base_style.add_modifier(Modifier::BOLD),
            )),
            InlineSpan::Italic { text } => out.push(Span::styled(
                text.clone(),
                base_style.add_modifier(Modifier::ITALIC),
            )),
            InlineSpan::Code { text } => {
                out.push(Span::styled(format!(" {text} "), theme.code_style()));
            }
            InlineSpan::Strikethrough { text } => out.push(Span::styled(
                text.clone(),
                theme.muted_style().add_modifier(Modifier::CROSSED_OUT),
            )),
            InlineSpan::Highlight { text } => out.push(Span::styled(
                format!(" {text} "),
                Style::default()
                    .fg(theme.bg_canvas)
                    .bg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            )),
            InlineSpan::Wikilink {
                target,
                heading,
                alias,
                resolved_path,
            } => {
                let display = if let Some(a) = alias {
                    a.clone()
                } else if let Some(h) = heading {
                    format!("{target}#{h}")
                } else {
                    target.clone()
                };
                out.push(Span::styled(
                    format!("[[{display}]]"),
                    theme.wikilink_style(resolved_path.is_some()),
                ));
            }
            InlineSpan::ExternalLink { label, url } => {
                out.push(Span::styled(
                    label.clone(),
                    Style::default()
                        .fg(theme.info)
                        .add_modifier(Modifier::UNDERLINED),
                ));
                if show_hrefs && label != url {
                    out.push(Span::styled(format!(" [{url}]"), theme.muted_style()));
                }
            }
            InlineSpan::Tag { tag } => {
                out.push(Span::styled(format!("#{tag}"), theme.tag_pill_style()));
            }
        }
    }
    out
}

fn wrap_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Line<'static>> {
    if max_width < 12 {
        return vec![Line::from(spans)];
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;

    for span in spans {
        let style = span.style;
        let text = span.content.into_owned();
        if text.is_empty() {
            continue;
        }
        let words: Vec<&str> = text.split_inclusive(' ').collect();
        for w in words {
            let wl = unicode_width::UnicodeWidthStr::width(w);
            if col > 0 && col + wl > max_width {
                lines.push(Line::from(std::mem::take(&mut current)));
                col = 0;
            }
            current.push(Span::styled(w.to_string(), style));
            col += wl;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}
