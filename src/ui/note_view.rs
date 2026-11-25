//! Center Canvas Mode A: Live Obsidian Note Reader & Interactive Markdown Canvas.
//!
//! Renders:
//! - YAML Frontmatter Properties Strip (key/value pills + `#tag` chips + aliases)
//! - Rich Obsidian Markdown: Headings (`H1`–`H6`), nested semantic Callout cards (`┃ ✦ TIP ...`),
//!   interactive `- [ ]` / `- [x]` checkboxes (`Space`/`x`), clickable `[[wikilinks]]` (`Enter`),
//!   inline `![[image.png]]` previews (`v` HD modal), bordered Code blocks (`y` copy),
//!   styled wrapped Markdown Tables (`bitefoundry_html::layout::render_wrapped_table`),
//!   sanitized `HtmlEmbed` cards (`[H: HTML]`), Math, Footnotes, Blockquotes, and Dividers.

use bitefoundry_html::{
    render_wrapped_table, wrap_styled_spans, TableAlign, TableBorder, TableSpec, WrapPrefix,
};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};

pub use crate::ui::inline_editor::{InlineNoteEditor, InlineSaveStatus};

use crate::{
    config::PageWidthMode,
    models::{InlineSpan, NoteBlock, NoteDetail, TableAlignment},
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::html_preview::{html_theme_from_ui, is_html_code_language, note_has_html},
        theme::Theme,
        truncate_width,
    },
};

struct RenderedLine {
    line: Line<'static>,
    block_idx: Option<usize>,
    task_checkbox: Option<(usize, u16)>,
    has_wikilink: bool,
    image_idx: Option<usize>,
    html_pill: Option<(u16, u16, usize)>,
    action_pills: Vec<(u16, u16, ButtonId)>,
    bg_override: Option<Color>,
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

    // In-place inline live editor directly inside `note_view`'s `content_area` (ZERO popup window!)
    if let Some(mut editor) = ui.inline_editor.take() {
        let cursor = editor.draw(frame, content_area, &theme, hitmap);
        ui.cursor_pos = cursor;
        ui.inline_editor = Some(editor);
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
            if rl.has_wikilink {
                hitmap.add(row_rect, HitTarget::WikilinkBlock(b_idx));
            }
            if let Some((t_ln, hit_w)) = rl.task_checkbox {
                hitmap.add_line(
                    content_area.x,
                    y,
                    hit_w.min(content_area.width),
                    HitTarget::TaskCheckbox { line_no: t_ln },
                );
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

fn push_plain(out: &mut Vec<RenderedLine>, line: Line<'static>, bg: Option<Color>) {
    out.push(RenderedLine {
        line,
        block_idx: None,
        task_checkbox: None,
        has_wikilink: false,
        image_idx: None,
        html_pill: None,
        action_pills: Vec::new(),
        bg_override: bg,
    });
}

struct BlockRenderCtx<'a> {
    detail: &'a NoteDetail,
    app: &'a AppState,
    ui: &'a UiState,
    theme: &'a Theme,
    width: usize,
    image_counter: usize,
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

    // 1. Note Title + Bookmark indicator
    let bm = if detail.summary.is_bookmarked {
        " ★"
    } else {
        ""
    };
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
            (
                bm_label.to_string(),
                detail.summary.is_bookmarked,
                ButtonId::ToggleBookmark,
            ),
            (
                width_label.to_string(),
                ui.page_width == PageWidthMode::Full,
                ButtonId::ToggleWidth,
            ),
            ("[a Append]".to_string(), false, ButtonId::QuickAppend),
            ("[n New]".to_string(), false, ButtonId::NewNote),
            ("[✎ Edit]".to_string(), false, ButtonId::EditNote),
            ("[u Links]".to_string(), false, ButtonId::LinkExtractor),
            (
                "[h Hrefs]".to_string(),
                ui.show_inline_hrefs,
                ButtonId::ToggleHrefs,
            ),
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
                task_checkbox: None,
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
    let mut ctx = BlockRenderCtx {
        detail,
        app,
        ui,
        theme,
        width,
        image_counter: 0,
    };

    for (b_idx, block) in detail.parsed.blocks.iter().enumerate() {
        let is_sel = focused && b_idx == selected_block;
        let sel_bg = if is_sel {
            Some(theme.bg_selected)
        } else {
            None
        };
        let has_wl = block.first_wikilink().is_some();
        render_block_into(
            &mut ctx,
            block,
            b_idx,
            is_sel,
            sel_bg,
            has_wl,
            &[],
            None,
            true,
            &mut out,
        );
    }

    out
}

#[allow(clippy::too_many_arguments)]
fn render_block_into(
    ctx: &mut BlockRenderCtx<'_>,
    block: &NoteBlock,
    b_idx: usize,
    is_sel: bool,
    sel_bg: Option<Color>,
    has_wl: bool,
    outer_prefix: &[Span<'static>],
    container_text_style: Option<Style>,
    top_level: bool,
    out: &mut Vec<RenderedLine>,
) {
    let theme = ctx.theme;
    let width = ctx.width;
    let show_hrefs = ctx.ui.show_inline_hrefs;
    let prefix_w: usize = outer_prefix
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let inner_width = width.saturating_sub(prefix_w).max(8);

    match block {
        NoteBlock::Heading {
            level, text, spans, ..
        } => {
            if top_level
                && b_idx == 0
                && *level == 1
                && text.eq_ignore_ascii_case(&ctx.detail.summary.title)
            {
                return;
            }
            if top_level && !out.is_empty() {
                push_plain(out, Line::from(""), None);
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
            let mut first_pfx = outer_prefix.to_vec();
            first_pfx.push(Span::styled(prefix.to_string(), style));
            let rendered = render_inline_spans(spans, theme, show_hrefs, style);
            for line in wrap_styled_spans(
                &rendered,
                width,
                WrapPrefix {
                    first: &first_pfx,
                    cont: outer_prefix,
                },
            ) {
                out.push(RenderedLine {
                    line,
                    block_idx: Some(b_idx),
                    task_checkbox: None,
                    has_wikilink: has_wl,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: sel_bg,
                });
            }
        }

        NoteBlock::Paragraph { spans, .. } => {
            // Standalone display math block (`$$...$$`) renders as a clean multiline math box.
            if spans.len() == 1 {
                if let InlineSpan::Math {
                    text,
                    display: true,
                } = &spans[0]
                {
                    let mut hdr = outer_prefix.to_vec();
                    hdr.push(Span::styled("┌─ math", theme.muted_style()));
                    out.push(RenderedLine {
                        line: Line::from(hdr),
                        block_idx: Some(b_idx),
                        task_checkbox: None,
                        has_wikilink: false,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: sel_bg,
                    });
                    let math_style = Style::default()
                        .fg(theme.accent_soft)
                        .add_modifier(Modifier::ITALIC);
                    let mut body_pfx = outer_prefix.to_vec();
                    body_pfx.push(Span::styled("│ ", Style::default().fg(theme.border)));
                    let trimmed = text.trim_matches('\n');
                    for line in wrap_styled_spans(
                        &[Span::styled(trimmed.to_string(), math_style)],
                        width,
                        WrapPrefix {
                            first: &body_pfx,
                            cont: &body_pfx,
                        },
                    ) {
                        out.push(RenderedLine {
                            line,
                            block_idx: Some(b_idx),
                            task_checkbox: None,
                            has_wikilink: false,
                            image_idx: None,
                            html_pill: None,
                            action_pills: Vec::new(),
                            bg_override: sel_bg,
                        });
                    }
                    let mut ftr = outer_prefix.to_vec();
                    ftr.push(Span::styled("└─", theme.muted_style()));
                    out.push(RenderedLine {
                        line: Line::from(ftr),
                        block_idx: Some(b_idx),
                        task_checkbox: None,
                        has_wikilink: false,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: sel_bg,
                    });
                    if top_level {
                        push_plain(out, Line::from(""), None);
                    }
                    return;
                }
            }

            let base_style = container_text_style.unwrap_or(Style::default().fg(theme.fg_primary));
            let rendered = render_inline_spans(spans, theme, show_hrefs, base_style);
            let mut first_pfx = outer_prefix.to_vec();
            if top_level && is_sel {
                first_pfx.push(Span::styled(
                    "▎ ",
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            for wrapped in wrap_styled_spans(
                &rendered,
                width,
                WrapPrefix {
                    first: &first_pfx,
                    cont: outer_prefix,
                },
            ) {
                out.push(RenderedLine {
                    line: wrapped,
                    block_idx: Some(b_idx),
                    task_checkbox: None,
                    has_wikilink: has_wl,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: sel_bg,
                });
            }
            if top_level {
                push_plain(out, Line::from(""), None);
            }
        }

        NoteBlock::HtmlEmbed {
            raw_html,
            summary_lines,
            ..
        } => {
            let pill = "[H: HTML]";
            let pill_w = unicode_width::UnicodeWidthStr::width(pill) as u16;
            let card_bg = if is_sel {
                theme.bg_selected
            } else {
                theme.bg_card
            };
            let mut banner_spans = outer_prefix.to_vec();
            let banner_label = " ◈ Embedded HTML · ";
            let pill_x = prefix_w + unicode_width::UnicodeWidthStr::width(banner_label);
            banner_spans.push(Span::styled(banner_label, theme.muted_style()));
            banner_spans.push(Span::styled(
                pill,
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.bg_card)
                    .add_modifier(Modifier::BOLD),
            ));
            out.push(RenderedLine {
                line: Line::from(banner_spans),
                block_idx: Some(b_idx),
                task_checkbox: None,
                has_wikilink: false,
                image_idx: None,
                html_pill: Some((pill_x as u16, pill_w, b_idx)),
                action_pills: Vec::new(),
                bg_override: Some(card_bg),
            });

            // Render ONLY a sanitized `bitefoundry_html::render_dom()` summary preview (max 8 lines).
            // Never fall through to raw HTML text.
            let doc = bitefoundry_html::DomDocument::parse(raw_html);
            let html_theme = html_theme_from_ui(theme);
            let preview_w = inner_width.saturating_sub(2).clamp(12, u16::MAX as usize) as u16;
            let rendered_html = bitefoundry_html::render_dom(
                &doc,
                preview_w,
                &html_theme,
                None,
                None,
                show_hrefs,
            );
            let mut non_blank: Vec<Line<'static>> = rendered_html
                .lines
                .into_iter()
                .filter(|l| l.spans.iter().any(|s| !s.content.trim().is_empty()))
                .collect();

            if non_blank.is_empty() && !summary_lines.is_empty() {
                non_blank = summary_lines
                    .iter()
                    .map(|s| Line::from(Span::styled(s.clone(), theme.muted_style())))
                    .collect();
            }

            const MAX_HTML_PREVIEW_LINES: usize = 8;
            let mut body_pfx = outer_prefix.to_vec();
            body_pfx.push(if top_level && is_sel {
                Span::styled(
                    "▎ ",
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled("  ", theme.muted_style())
            });

            if non_blank.is_empty() {
                if !rendered_html.graphics.is_empty() {
                    let mut spans = body_pfx;
                    spans.push(Span::styled(
                        "◇ Embedded graphic · press H to view",
                        theme.muted_style(),
                    ));
                    out.push(RenderedLine {
                        line: Line::from(spans),
                        block_idx: Some(b_idx),
                        task_checkbox: None,
                        has_wikilink: false,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: sel_bg,
                    });
                }
            } else {
                let truncated = non_blank.len() > MAX_HTML_PREVIEW_LINES;
                let take_n = if truncated {
                    MAX_HTML_PREVIEW_LINES - 1
                } else {
                    non_blank.len()
                };
                for line in non_blank.into_iter().take(take_n) {
                    let mut prefixed = body_pfx.clone();
                    prefixed.extend(line.spans);
                    out.push(RenderedLine {
                        line: Line::from(prefixed),
                        block_idx: Some(b_idx),
                        task_checkbox: None,
                        has_wikilink: false,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: sel_bg,
                    });
                }
                if truncated {
                    let mut spans = body_pfx;
                    spans.push(Span::styled(
                        "… press H for full HTML preview",
                        theme.muted_style(),
                    ));
                    out.push(RenderedLine {
                        line: Line::from(spans),
                        block_idx: Some(b_idx),
                        task_checkbox: None,
                        has_wikilink: false,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: sel_bg,
                    });
                }
            }

            if top_level {
                push_plain(out, Line::from(""), None);
            }
        }

        NoteBlock::Callout {
            callout_kind,
            title,
            collapsed,
            body_blocks,
            ..
        } => {
            let c_color = theme.callout_color(*callout_kind);
            let card_bg = if is_sel {
                theme.bg_selected
            } else {
                theme.bg_card
            };
            let fold = if *collapsed { "▸ " } else { "" };
            let bar = Span::styled(
                "┃ ",
                Style::default().fg(c_color).add_modifier(Modifier::BOLD),
            );
            let mut header_spans = outer_prefix.to_vec();
            header_spans.push(bar.clone());
            header_spans.push(Span::styled(
                format!("{fold}{} {} ", callout_kind.glyph(), callout_kind.label()),
                Style::default().fg(c_color).add_modifier(Modifier::BOLD),
            ));
            if !title.eq_ignore_ascii_case(callout_kind.label()) {
                header_spans.push(Span::styled(
                    format!("· {title}"),
                    Style::default()
                        .fg(theme.fg_primary)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            out.push(RenderedLine {
                line: Line::from(header_spans),
                block_idx: Some(b_idx),
                task_checkbox: None,
                has_wikilink: has_wl,
                image_idx: None,
                html_pill: None,
                action_pills: Vec::new(),
                bg_override: Some(card_bg),
            });

            let mut nested_prefix = outer_prefix.to_vec();
            nested_prefix.push(bar);
            let body_style = Style::default().fg(theme.fg_secondary);
            for child in body_blocks {
                render_block_into(
                    ctx,
                    child,
                    b_idx,
                    is_sel,
                    Some(card_bg),
                    has_wl,
                    &nested_prefix,
                    Some(body_style),
                    false,
                    out,
                );
            }
            if top_level {
                push_plain(out, Line::from(""), None);
            }
        }

        NoteBlock::Blockquote { blocks, .. } => {
            let mut nested_prefix = outer_prefix.to_vec();
            nested_prefix.push(Span::styled("▎ ", Style::default().fg(theme.fg_muted)));
            for child in blocks {
                render_block_into(
                    ctx,
                    child,
                    b_idx,
                    is_sel,
                    sel_bg,
                    has_wl,
                    &nested_prefix,
                    Some(theme.quote_style()),
                    false,
                    out,
                );
            }
            if top_level {
                push_plain(out, Line::from(""), None);
            }
        }

        NoteBlock::Task {
            checked,
            indent,
            spans,
            line_no,
            ..
        } => {
            let pad = " ".repeat((*indent).min(8));
            let cursor = if top_level && is_sel { "▸ " } else { "  " };
            let checkbox = if *checked { "☑ " } else { "☐ " };
            let base_style = if *checked {
                theme.muted_style().add_modifier(Modifier::CROSSED_OUT)
            } else {
                container_text_style.unwrap_or(Style::default().fg(theme.fg_primary))
            };
            let mut first_pfx = outer_prefix.to_vec();
            first_pfx.push(Span::styled(
                format!("{pad}{cursor}"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
            first_pfx.push(Span::styled(
                checkbox.to_string(),
                Style::default()
                    .fg(if *checked {
                        theme.success
                    } else {
                        theme.accent
                    })
                    .add_modifier(Modifier::BOLD),
            ));
            let mut cont_pfx = outer_prefix.to_vec();
            cont_pfx.push(Span::raw(format!("{pad}    ")));

            let rendered = render_inline_spans(spans, theme, show_hrefs, base_style);
            let hit_w = (prefix_w + (*indent).min(8) + 6) as u16;
            for (li, line) in wrap_styled_spans(
                &rendered,
                width,
                WrapPrefix {
                    first: &first_pfx,
                    cont: &cont_pfx,
                },
            )
            .into_iter()
            .enumerate()
            {
                out.push(RenderedLine {
                    line,
                    block_idx: Some(b_idx),
                    task_checkbox: (li == 0).then_some((*line_no, hit_w)),
                    has_wikilink: has_wl,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: sel_bg,
                });
            }
        }

        NoteBlock::BulletItem { indent, spans, .. } => {
            let pad = " ".repeat((*indent).min(8));
            let base_style = container_text_style.unwrap_or(Style::default().fg(theme.fg_primary));
            let mut first_pfx = outer_prefix.to_vec();
            first_pfx.push(Span::styled(
                format!("{pad}  • "),
                Style::default().fg(theme.accent_soft),
            ));
            let mut cont_pfx = outer_prefix.to_vec();
            cont_pfx.push(Span::raw(format!("{pad}    ")));

            let rendered = render_inline_spans(spans, theme, show_hrefs, base_style);
            for wrapped in wrap_styled_spans(
                &rendered,
                width,
                WrapPrefix {
                    first: &first_pfx,
                    cont: &cont_pfx,
                },
            ) {
                out.push(RenderedLine {
                    line: wrapped,
                    block_idx: Some(b_idx),
                    task_checkbox: None,
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
            let base_style = container_text_style.unwrap_or(Style::default().fg(theme.fg_primary));
            let marker = format!("{pad}  {number}. ");
            let cont_spaces = " ".repeat(unicode_width::UnicodeWidthStr::width(marker.as_str()));
            let mut first_pfx = outer_prefix.to_vec();
            first_pfx.push(Span::styled(
                marker,
                Style::default().fg(theme.accent_soft),
            ));
            let mut cont_pfx = outer_prefix.to_vec();
            cont_pfx.push(Span::raw(cont_spaces));

            let rendered = render_inline_spans(spans, theme, show_hrefs, base_style);
            for wrapped in wrap_styled_spans(
                &rendered,
                width,
                WrapPrefix {
                    first: &first_pfx,
                    cont: &cont_pfx,
                },
            ) {
                out.push(RenderedLine {
                    line: wrapped,
                    block_idx: Some(b_idx),
                    task_checkbox: None,
                    has_wikilink: has_wl,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: sel_bg,
                });
            }
        }

        NoteBlock::CodeBlock { language, code, .. } => {
            let code_bg = if is_sel {
                theme.bg_selected
            } else {
                theme.bg_code
            };
            let lang_label = if language.is_empty() {
                "code"
            } else {
                language.as_str()
            };
            let is_html = is_html_code_language(language) && !code.trim().is_empty();
            let html_suffix = if is_html { " [H: HTML]" } else { "" };
            let bar_w = inner_width
                .saturating_sub(lang_label.len() + 14 + html_suffix.len())
                .min(50);
            let mut hdr_spans = outer_prefix.to_vec();
            hdr_spans.push(Span::styled("╭─ ", Style::default().fg(theme.border)));
            hdr_spans.push(Span::styled(
                format!("{lang_label} "),
                Style::default()
                    .fg(theme.accent_soft)
                    .add_modifier(Modifier::BOLD),
            ));
            hdr_spans.push(Span::styled(
                "─".repeat(bar_w),
                Style::default().fg(theme.border),
            ));
            hdr_spans.push(Span::styled(" [y copy]", theme.muted_style()));

            let html_pill = if is_html {
                let pfx_w: usize = hdr_spans
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
                Some(((pfx_w + 1) as u16, 9u16, b_idx))
            } else {
                None
            };
            out.push(RenderedLine {
                line: Line::from(hdr_spans),
                block_idx: Some(b_idx),
                task_checkbox: None,
                has_wikilink: false,
                image_idx: None,
                html_pill,
                action_pills: Vec::new(),
                bg_override: Some(code_bg),
            });
            for code_line in code.lines() {
                let mut spans = outer_prefix.to_vec();
                spans.push(Span::styled("│ ", Style::default().fg(theme.border)));
                spans.push(Span::styled(
                    truncate_width(code_line, inner_width.saturating_sub(3)),
                    Style::default().fg(theme.code_fg),
                ));
                out.push(RenderedLine {
                    line: Line::from(spans),
                    block_idx: Some(b_idx),
                    task_checkbox: None,
                    has_wikilink: false,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: Some(code_bg),
                });
            }
            let mut ftr = outer_prefix.to_vec();
            ftr.push(Span::styled(
                format!("╰{}", "─".repeat(inner_width.saturating_sub(2).min(64))),
                Style::default().fg(theme.border),
            ));
            out.push(RenderedLine {
                line: Line::from(ftr),
                block_idx: Some(b_idx),
                task_checkbox: None,
                has_wikilink: false,
                image_idx: None,
                html_pill: None,
                action_pills: Vec::new(),
                bg_override: Some(code_bg),
            });
            if top_level {
                push_plain(out, Line::from(""), None);
            }
        }

        NoteBlock::Table {
            headers,
            rows,
            alignments,
            ..
        } => {
            let header_style = Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD);
            let cell_style = Style::default().fg(theme.fg_primary);
            let border_style = Style::default().fg(theme.border);

            let rendered_headers: Vec<Vec<Span<'static>>> = headers
                .iter()
                .map(|cell| render_inline_spans(cell, theme, show_hrefs, header_style))
                .collect();
            let rendered_rows: Vec<Vec<Vec<Span<'static>>>> = rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|cell| render_inline_spans(cell, theme, show_hrefs, cell_style))
                        .collect()
                })
                .collect();
            let table_aligns: Vec<TableAlign> = alignments
                .iter()
                .map(|a| match a {
                    TableAlignment::Left => TableAlign::Left,
                    TableAlignment::Center => TableAlign::Center,
                    TableAlignment::Right => TableAlign::Right,
                })
                .collect();

            let spec = TableSpec {
                headers: &rendered_headers,
                rows: &rendered_rows,
                alignments: &table_aligns,
                border: TableBorder::Box,
                prefix: outer_prefix,
                border_style,
                header_style,
                cell_style,
            };
            let table_lines = render_wrapped_table(&spec, width);
            for line in table_lines {
                out.push(RenderedLine {
                    line,
                    block_idx: Some(b_idx),
                    task_checkbox: None,
                    has_wikilink: has_wl,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: sel_bg,
                });
            }
            if top_level {
                push_plain(out, Line::from(""), None);
            }
        }

        NoteBlock::FootnoteDef { label, spans, .. } => {
            let mut first_pfx = outer_prefix.to_vec();
            first_pfx.push(Span::styled(
                format!("[^{label}]: "),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
            let mut cont_pfx = outer_prefix.to_vec();
            cont_pfx.push(Span::raw("  "));
            let rendered = render_inline_spans(spans, theme, show_hrefs, theme.muted_style());
            for wrapped in wrap_styled_spans(
                &rendered,
                width,
                WrapPrefix {
                    first: &first_pfx,
                    cont: &cont_pfx,
                },
            ) {
                out.push(RenderedLine {
                    line: wrapped,
                    block_idx: Some(b_idx),
                    task_checkbox: None,
                    has_wikilink: has_wl,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: sel_bg,
                });
            }
        }

        NoteBlock::ImageEmbed {
            raw_target,
            alt,
            resolved_path,
            ..
        } => {
            let img_idx = if resolved_path.is_some() {
                let idx = ctx.image_counter;
                ctx.image_counter += 1;
                Some(idx)
            } else {
                None
            };
            let badge = if resolved_path.is_some() {
                format!(" ▣ IMG: {alt} ({raw_target}) · [v HD Preview] ")
            } else {
                format!(" ▣ IMG (missing): {raw_target} ")
            };
            let mut badge_spans = outer_prefix.to_vec();
            badge_spans.push(Span::styled(
                truncate_width(&badge, inner_width),
                if resolved_path.is_some() {
                    theme.tag_pill_style()
                } else {
                    Style::default().fg(theme.warning).bg(theme.bg_card)
                },
            ));
            out.push(RenderedLine {
                line: Line::from(badge_spans),
                block_idx: Some(b_idx),
                task_checkbox: None,
                has_wikilink: false,
                image_idx: img_idx,
                html_pill: None,
                action_pills: Vec::new(),
                bg_override: sel_bg,
            });

            // Render inline halfblock preview if cached
            if ctx.app.cfg.inline_images {
                if let Some(abs) = resolved_path {
                    let bg_rgb = crate::img_preview::color_to_rgb(theme.bg_canvas, (19, 19, 24));
                    let cache_key = crate::ui::app::inline_preview_cache_key(
                        abs,
                        bg_rgb,
                        ctx.app.graphics_caps.cell_px,
                    );
                    if let Some(rendered) = ctx.app.preview_cache.get(&cache_key) {
                        for img_line in &rendered.lines {
                            out.push(RenderedLine {
                                line: img_line.clone(),
                                block_idx: Some(b_idx),
                                task_checkbox: None,
                                has_wikilink: false,
                                image_idx: img_idx,
                                html_pill: None,
                                action_pills: Vec::new(),
                                bg_override: None,
                            });
                        }
                    }
                }
            }
            if top_level {
                push_plain(out, Line::from(""), None);
            }
        }

        NoteBlock::NoteEmbed {
            target,
            heading,
            resolved_path,
            ..
        } => {
            let label = match heading {
                Some(h) => format!("{target}#{h}"),
                None => target.clone(),
            };
            let mut spans = outer_prefix.to_vec();
            spans.push(Span::styled(
                "┃ ↗ Embed: ",
                Style::default().fg(theme.accent),
            ));
            spans.push(Span::styled(
                label,
                theme.wikilink_style(resolved_path.is_some()),
            ));
            out.push(RenderedLine {
                line: Line::from(spans),
                block_idx: Some(b_idx),
                task_checkbox: None,
                has_wikilink: true,
                image_idx: None,
                html_pill: None,
                action_pills: Vec::new(),
                bg_override: Some(theme.bg_card),
            });
            if top_level {
                push_plain(out, Line::from(""), None);
            }
        }

        NoteBlock::Divider { .. } => {
            let mut spans = outer_prefix.to_vec();
            spans.push(Span::styled(
                "─".repeat(inner_width.min(72)),
                Style::default().fg(theme.divider),
            ));
            out.push(RenderedLine {
                line: Line::from(spans),
                block_idx: Some(b_idx),
                task_checkbox: None,
                has_wikilink: false,
                image_idx: None,
                html_pill: None,
                action_pills: Vec::new(),
                bg_override: sel_bg,
            });
        }
    }
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
            InlineSpan::Underline { text } => out.push(Span::styled(
                text.clone(),
                base_style.add_modifier(Modifier::UNDERLINED),
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
            InlineSpan::Styled {
                text,
                bold,
                italic,
                strike,
                highlight,
                underline,
            } => {
                let mut st = if *highlight {
                    Style::default()
                        .fg(theme.bg_canvas)
                        .bg(theme.warning)
                        .add_modifier(Modifier::BOLD)
                } else if *strike {
                    theme.muted_style()
                } else {
                    base_style
                };
                if *bold {
                    st = st.add_modifier(Modifier::BOLD);
                }
                if *italic {
                    st = st.add_modifier(Modifier::ITALIC);
                }
                if *strike {
                    st = st.add_modifier(Modifier::CROSSED_OUT);
                }
                if *underline {
                    st = st.add_modifier(Modifier::UNDERLINED);
                }
                out.push(Span::styled(text.clone(), st));
            }
            InlineSpan::Math { text, .. } => {
                let st = if theme.no_color {
                    base_style.add_modifier(Modifier::ITALIC)
                } else {
                    Style::default()
                        .fg(theme.accent_soft)
                        .bg(theme.bg_code)
                        .add_modifier(Modifier::ITALIC)
                };
                out.push(Span::styled(text.clone(), st));
            }
            InlineSpan::FootnoteRef { label } => {
                let st = if theme.no_color {
                    Style::default().add_modifier(Modifier::DIM)
                } else {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                };
                out.push(Span::styled(format!("[^{label}]"), st));
            }
            InlineSpan::Subscript { text } => {
                out.push(Span::styled(text.clone(), theme.muted_style()));
            }
            InlineSpan::Superscript { text } => {
                out.push(Span::styled(
                    text.clone(),
                    Style::default().fg(theme.accent_soft),
                ));
            }
            InlineSpan::LineBreak => {
                out.push(Span::raw("\n"));
            }
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
                    display,
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

#[cfg(test)]
mod tests {
    use tokio::sync::{mpsc, watch};
    use unicode_width::UnicodeWidthStr;

    use super::*;
    use crate::{
        config::AppConfig,
        models::{CalloutKind, NoteDetail, NoteSummary},
        storage::Storage,
        sync::{FocusTarget, SyncCommand, SyncController},
        vault::{
            fs::toggle_task_at_line_verified,
            parser::parse_markdown,
            vault_from_dir,
        },
    };

    fn render_note_lines(md: &str, title: &str, width: usize) -> (NoteDetail, Vec<RenderedLine>) {
        let tmp = tempfile::tempdir().unwrap();
        let vault = vault_from_dir(tmp.path(), Some("CanvasTestVault"));
        let cfg = AppConfig::default();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
        let app = AppState::new(cfg.clone(), vault, storage, sync_ctrl);
        let ui = UiState::from_config(&cfg);

        let parsed = parse_markdown("Test.md", title, md);
        let detail = NoteDetail {
            summary: NoteSummary {
                rel_path: "Test.md".into(),
                title: title.into(),
                ..Default::default()
            },
            raw_markdown: md.to_string(),
            parsed,
            backlinks: Vec::new(),
            outgoing_links: Vec::new(),
            images: Vec::new(),
        };
        let rendered = build_canvas_lines(&detail, &app, &ui, &ui.theme, width, 0, true);
        (detail, rendered)
    }

    fn lines_to_text(rendered: &[RenderedLine]) -> String {
        rendered
            .iter()
            .map(|rl| {
                rl.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn embedded_html_does_not_fall_through_raw_tags() {
        let md = "# Embedded HTML Test\n\n<div><style>.x{color:red}</style><svg><desc>secret</desc><rect width=\"10\" height=\"10\"/></svg><p>Hello</p></div>\n";
        let (detail, rendered) = render_note_lines(md, "Embedded HTML Test", 80);
        let full_text = lines_to_text(&rendered);

        assert!(
            detail
                .parsed
                .blocks
                .iter()
                .any(|b| matches!(b, NoteBlock::HtmlEmbed { .. })),
            "expected NoteBlock::HtmlEmbed block, got: {:?}",
            detail.parsed.blocks
        );
        assert!(
            full_text.contains("[H: HTML]"),
            "expected [H: HTML] badge in:\n{full_text}"
        );
        assert!(
            full_text.contains("◈ Embedded HTML"),
            "expected Embedded HTML banner in:\n{full_text}"
        );
        assert!(
            full_text.contains("Hello"),
            "expected sanitized paragraph text 'Hello' in:\n{full_text}"
        );
        for forbidden in [
            "<div",
            "<style",
            ".x{color:red}",
            "color:red",
            "<svg",
            "<desc>",
            "secret",
            "<rect",
            "<p>",
            "</div>",
        ] {
            assert!(
                !full_text.contains(forbidden),
                "raw HTML/CSS/SVG leak detected ({forbidden}) in:\n{full_text}"
            );
        }
    }

    #[test]
    fn table_cells_and_inline_syntax_no_raw_leakage() {
        let md = concat!(
            "---\n",
            "status: active\n",
            "---\n\n",
            "# Track C Spec\n\n",
            "| Item | Description |\n",
            "| :--- | ---: |\n",
            "| **Bold** [[Note|Alias]] `code` | 한글 셀 |\n\n",
            "Inline: _italic_, __bold__, ***both***, \\*esc\\*, $x^2$, ref[^1], <u>under</u>, and %%secret%% hidden.\n\n",
            "%%\nmultiline hidden secret\n%%\n\n",
            "> [!note]\n",
            "> Outer callout\n",
            ">> [!tip]\n",
            ">> Nested tip\n\n",
            "- [ ] Verify exact line_no toggle\n\n",
            "[^1]: Footnote detail body.\n",
        );

        let width = 64;
        let (detail, rendered) = render_note_lines(md, "Track C Spec", width);
        let full_text = lines_to_text(&rendered);

        // 1. Wikilinks index preserved from table cell `[[Note|Alias]]`
        assert!(
            detail
                .parsed
                .wikilinks
                .iter()
                .any(|w| w.target_raw == "Note" && w.alias.as_deref() == Some("Alias")),
            "expected wikilink [[Note|Alias]] in index, got: {:?}",
            detail.parsed.wikilinks
        );

        // 2. Table cells hold parsed `Vec<InlineSpan>` (no raw `**` / `[[`)
        let table = detail
            .parsed
            .blocks
            .iter()
            .find(|b| matches!(b, NoteBlock::Table { .. }))
            .expect("expected NoteBlock::Table");
        if let NoteBlock::Table { rows, .. } = table {
            let cell0 = &rows[0][0];
            assert!(
                cell0.iter().any(|s| matches!(s, InlineSpan::Bold { text } if text == "Bold")),
                "expected Bold span in table cell, got: {cell0:?}"
            );
            assert!(
                cell0
                    .iter()
                    .any(|s| matches!(s, InlineSpan::Wikilink { target, alias, .. } if target == "Note" && alias.as_deref() == Some("Alias"))),
                "expected Wikilink span in table cell, got: {cell0:?}"
            );
            assert!(
                cell0.iter().any(|s| matches!(s, InlineSpan::Code { text } if text == "code")),
                "expected Code span in table cell, got: {cell0:?}"
            );
        }

        // 3. Nested Callout (`> [!note]\n>> [!tip]`) parsed into nested AST
        let outer_callout = detail
            .parsed
            .blocks
            .iter()
            .find(|b| matches!(b, NoteBlock::Callout { callout_kind: CalloutKind::Note, .. }))
            .expect("expected outer Note callout");
        if let NoteBlock::Callout { body_blocks, .. } = outer_callout {
            assert!(
                body_blocks.iter().any(|b| matches!(
                    b,
                    NoteBlock::Callout {
                        callout_kind: CalloutKind::Tip,
                        ..
                    }
                )),
                "expected nested Tip callout inside Note callout, got: {body_blocks:?}"
            );
        }

        // 4. Zero raw Markdown syntax or comment leakage in rendered canvas
        for forbidden in [
            "**",
            "[[",
            "%%secret%%",
            "secret",
            "multiline hidden",
            "_italic_",
            "__bold__",
            "***both***",
            "\\*esc\\*",
            "$x^2$",
            "<u>",
            "</u>",
        ] {
            assert!(
                !full_text.contains(forbidden),
                "unexpected raw syntax/comment leak ({forbidden}) in:\n{full_text}"
            );
        }

        // 5. Expected rendered content present
        for expected in [
            "Bold",
            "Alias",
            "code",
            "한글 셀",
            "italic",
            "bold",
            "both",
            "*esc*",
            "x^2",
            "[^1]",
            "under",
            "NOTE",
            "TIP",
            "Nested tip",
            "Footnote detail body.",
        ] {
            assert!(
                full_text.contains(expected),
                "missing expected rendered text ({expected}) in:\n{full_text}"
            );
        }

        // 6. CJK table rows stay strictly aligned within viewport width
        for rl in &rendered {
            let line_str: String = rl.line.spans.iter().map(|s| s.content.as_ref()).collect();
            if line_str.starts_with('┌')
                || line_str.starts_with('│')
                || line_str.starts_with('├')
                || line_str.starts_with('└')
            {
                let lw = UnicodeWidthStr::width(line_str.as_str());
                assert!(
                    lw <= width,
                    "table line exceeded width {width} (got {lw}): {line_str}"
                );
            }
        }

        // 7. Exact 1-based `line_no` survives frontmatter + multiline `%%comment%%` + nested callouts
        let task = detail
            .parsed
            .tasks
            .iter()
            .find(|t| t.text.contains("Verify exact line_no toggle"))
            .expect("expected task entry");
        let tmp = tempfile::tempdir().unwrap();
        let note_file = tmp.path().join("TrackC.md");
        std::fs::write(&note_file, md).unwrap();
        let now_checked = toggle_task_at_line_verified(
            &note_file,
            task.line_no,
            Some(&task.text),
            Some(false),
        )
        .expect("toggle_task_at_line_verified must succeed with exact 1-based line_no");
        assert!(now_checked);
        let updated = std::fs::read_to_string(&note_file).unwrap();
        assert!(updated.contains("- [x] Verify exact line_no toggle"));
    }
}
