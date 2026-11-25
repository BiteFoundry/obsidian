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
    widgets::{Block, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

pub use crate::ui::inline_editor::{InlineNoteEditor, InlineSaveStatus};

use crate::{
    config::PageWidthMode,
    models::{InlineSpan, NoteBlock, NoteDetail, TableAlignment},
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::html_preview::{html_theme_from_ui, is_html_code_language, note_has_html},
        theme::Theme,
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
        frame.render_widget(Paragraph::new(msg).wrap(Wrap { trim: false }), area);
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
    let mut title_spans = vec![Span::styled(
        detail.summary.title.clone(),
        Style::default()
            .fg(theme.fg_primary)
            .add_modifier(Modifier::BOLD),
    )];
    if detail.summary.is_bookmarked {
        title_spans.push(Span::styled(" ★", Style::default().fg(theme.warning)));
    }
    for line in wrap_styled_spans(&title_spans, width, WrapPrefix::empty()) {
        push_plain(&mut out, line, None);
    }

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
            let lw = UnicodeWidthStr::width(label.as_str());
            let style = if active {
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.bg_card)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.muted_style()
            };
            let gap = if x_off > 0 { 1 } else { 0 };
            if x_off > 0 && x_off + gap + lw > width {
                out.push(RenderedLine {
                    line: Line::from(std::mem::take(&mut spans)),
                    block_idx: None,
                    task_checkbox: None,
                    has_wikilink: false,
                    image_idx: None,
                    html_pill: None,
                    action_pills: std::mem::take(&mut hit_pills),
                    bg_override: None,
                });
                x_off = 0;
            }
            if lw > width {
                for wrapped in
                    wrap_styled_spans(&[Span::styled(label, style)], width, WrapPrefix::empty())
                {
                    let ww = wrapped.width().min(width) as u16;
                    out.push(RenderedLine {
                        line: wrapped,
                        block_idx: None,
                        task_checkbox: None,
                        has_wikilink: false,
                        image_idx: None,
                        html_pill: None,
                        action_pills: vec![(0, ww, btn)],
                        bg_override: None,
                    });
                }
                continue;
            }
            if x_off > 0 {
                spans.push(Span::raw(" "));
                x_off += 1;
            }
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
        for line in wrap_styled_spans(&prop_spans, width, WrapPrefix::empty()) {
            push_plain(&mut out, line, None);
        }
    }

    if !detail.summary.tags.is_empty() {
        let mut tag_spans: Vec<Span<'static>> = Vec::new();
        for (i, t) in detail.summary.tags.iter().enumerate() {
            if i > 0 {
                tag_spans.push(Span::raw(" "));
            }
            tag_spans.push(Span::styled(format!(" #{t} "), theme.tag_pill_style()));
        }
        for line in wrap_styled_spans(&tag_spans, width, WrapPrefix::empty()) {
            push_plain(&mut out, line, None);
        }
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
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();

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
            let mut cont_pfx = outer_prefix.to_vec();
            cont_pfx.push(Span::raw(" ".repeat(UnicodeWidthStr::width(prefix))));
            let rendered = render_inline_spans(spans, theme, show_hrefs, style);
            for line in wrap_styled_spans(
                &rendered,
                width,
                WrapPrefix {
                    first: &first_pfx,
                    cont: &cont_pfx,
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
                    for line in wrap_styled_spans(
                        &[Span::styled("┌─ math", theme.muted_style())],
                        width,
                        WrapPrefix {
                            first: outer_prefix,
                            cont: outer_prefix,
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
                    for line in wrap_styled_spans(
                        &[Span::styled("└─", theme.muted_style())],
                        width,
                        WrapPrefix {
                            first: outer_prefix,
                            cont: outer_prefix,
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
            let pill_w = UnicodeWidthStr::width(pill) as u16;
            let card_bg = if is_sel {
                theme.bg_selected
            } else {
                theme.bg_card
            };
            let banner_content = vec![
                Span::styled(" ◈ Embedded HTML · ", theme.muted_style()),
                Span::styled(
                    pill,
                    Style::default()
                        .fg(theme.accent)
                        .bg(theme.bg_card)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            for wrapped in wrap_styled_spans(
                &banner_content,
                width,
                WrapPrefix {
                    first: outer_prefix,
                    cont: outer_prefix,
                },
            ) {
                let mut pill_hit = None;
                let mut col = 0usize;
                for sp in &wrapped.spans {
                    if let Some(pos) = sp.content.find(pill) {
                        let x_off = col + UnicodeWidthStr::width(&sp.content[..pos]);
                        pill_hit = Some((x_off as u16, pill_w, b_idx));
                        break;
                    }
                    col += UnicodeWidthStr::width(sp.content.as_ref());
                }
                out.push(RenderedLine {
                    line: wrapped,
                    block_idx: Some(b_idx),
                    task_checkbox: None,
                    has_wikilink: false,
                    image_idx: None,
                    html_pill: pill_hit,
                    action_pills: Vec::new(),
                    bg_override: Some(card_bg),
                });
            }

            // Render ONLY a sanitized `bitefoundry_html::render_dom()` summary preview (max 8 lines).
            // Never fall through to raw HTML text.
            let doc = bitefoundry_html::DomDocument::parse(raw_html);
            let html_theme = html_theme_from_ui(theme);
            let preview_w = width
                .saturating_sub(prefix_w + 2)
                .clamp(1, u16::MAX as usize) as u16;
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
                    for wrapped in wrap_styled_spans(
                        &[Span::styled(
                            "◇ Embedded graphic · press H to view",
                            theme.muted_style(),
                        )],
                        width,
                        WrapPrefix {
                            first: &body_pfx,
                            cont: &body_pfx,
                        },
                    ) {
                        out.push(RenderedLine {
                            line: wrapped,
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
            } else {
                let truncated = non_blank.len() > MAX_HTML_PREVIEW_LINES;
                let take_n = if truncated {
                    MAX_HTML_PREVIEW_LINES - 1
                } else {
                    non_blank.len()
                };
                for line in non_blank.into_iter().take(take_n) {
                    for wrapped in wrap_styled_spans(
                        &line.spans,
                        width,
                        WrapPrefix {
                            first: &body_pfx,
                            cont: &body_pfx,
                        },
                    ) {
                        out.push(RenderedLine {
                            line: wrapped,
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
                if truncated {
                    for wrapped in wrap_styled_spans(
                        &[Span::styled(
                            "… press H for full HTML preview",
                            theme.muted_style(),
                        )],
                        width,
                        WrapPrefix {
                            first: &body_pfx,
                            cont: &body_pfx,
                        },
                    ) {
                        out.push(RenderedLine {
                            line: wrapped,
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
            let mut nested_prefix = outer_prefix.to_vec();
            nested_prefix.push(bar);

            let mut header_content = vec![Span::styled(
                format!("{fold}{} {} ", callout_kind.glyph(), callout_kind.label()),
                Style::default().fg(c_color).add_modifier(Modifier::BOLD),
            )];
            if !title.eq_ignore_ascii_case(callout_kind.label()) {
                header_content.push(Span::styled(
                    format!("· {title}"),
                    Style::default()
                        .fg(theme.fg_primary)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            for wrapped in wrap_styled_spans(
                &header_content,
                width,
                WrapPrefix {
                    first: &nested_prefix,
                    cont: &nested_prefix,
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
                    bg_override: Some(card_bg),
                });
            }

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
            let marker_w = UnicodeWidthStr::width(pad.as_str())
                + UnicodeWidthStr::width(cursor)
                + UnicodeWidthStr::width(checkbox);
            let mut cont_pfx = outer_prefix.to_vec();
            cont_pfx.push(Span::raw(" ".repeat(marker_w)));

            let rendered = render_inline_spans(spans, theme, show_hrefs, base_style);
            let hit_w = (prefix_w + marker_w + 2).min(width) as u16;
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
            let marker = format!("{pad}  • ");
            let cont_spaces = " ".repeat(UnicodeWidthStr::width(marker.as_str()));
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

        NoteBlock::NumberedItem {
            number,
            indent,
            spans,
            ..
        } => {
            let pad = " ".repeat((*indent).min(8));
            let base_style = container_text_style.unwrap_or(Style::default().fg(theme.fg_primary));
            let marker = format!("{pad}  {number}. ");
            let cont_spaces = " ".repeat(UnicodeWidthStr::width(marker.as_str()));
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
            let lang_w = UnicodeWidthStr::width(lang_label);
            let html_suffix_w = if is_html { 10 } else { 0 };
            let fixed_w = 3 + lang_w + 1 + 9 + html_suffix_w;
            let bar_w = width.saturating_sub(prefix_w + fixed_w).min(50);

            let mut hdr_content = vec![
                Span::styled("╭─ ", Style::default().fg(theme.border)),
                Span::styled(
                    format!("{lang_label} "),
                    Style::default()
                        .fg(theme.accent_soft)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if bar_w > 0 {
                hdr_content.push(Span::styled(
                    "─".repeat(bar_w),
                    Style::default().fg(theme.border),
                ));
            }
            hdr_content.push(Span::styled(" [y copy]", theme.muted_style()));
            if is_html {
                hdr_content.push(Span::raw(" "));
                hdr_content.push(Span::styled(
                    "[H: HTML]",
                    Style::default()
                        .fg(theme.accent)
                        .bg(theme.bg_card)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            for wrapped in wrap_styled_spans(
                &hdr_content,
                width,
                WrapPrefix {
                    first: outer_prefix,
                    cont: outer_prefix,
                },
            ) {
                let html_pill = if is_html {
                    let mut col = 0usize;
                    let mut found = None;
                    for sp in &wrapped.spans {
                        if let Some(pos) = sp.content.find("[H: HTML]") {
                            let x_off = col + UnicodeWidthStr::width(&sp.content[..pos]);
                            found = Some((x_off as u16, 9u16, b_idx));
                            break;
                        }
                        col += UnicodeWidthStr::width(sp.content.as_ref());
                    }
                    found
                } else {
                    None
                };
                out.push(RenderedLine {
                    line: wrapped,
                    block_idx: Some(b_idx),
                    task_checkbox: None,
                    has_wikilink: false,
                    image_idx: None,
                    html_pill,
                    action_pills: Vec::new(),
                    bg_override: Some(code_bg),
                });
            }

            let mut code_pfx = outer_prefix.to_vec();
            code_pfx.push(Span::styled("│ ", Style::default().fg(theme.border)));
            let code_style = Style::default().fg(theme.code_fg);
            for code_line in code.lines() {
                for wrapped in wrap_styled_spans(
                    &[Span::styled(code_line.to_string(), code_style)],
                    width,
                    WrapPrefix {
                        first: &code_pfx,
                        cont: &code_pfx,
                    },
                ) {
                    out.push(RenderedLine {
                        line: wrapped,
                        block_idx: Some(b_idx),
                        task_checkbox: None,
                        has_wikilink: false,
                        image_idx: None,
                        html_pill: None,
                        action_pills: Vec::new(),
                        bg_override: Some(code_bg),
                    });
                }
            }

            let dash_w = width.saturating_sub(prefix_w + 1).min(64);
            let ftr_text = format!("╰{}", "─".repeat(dash_w));
            for wrapped in wrap_styled_spans(
                &[Span::styled(ftr_text, Style::default().fg(theme.border))],
                width,
                WrapPrefix {
                    first: outer_prefix,
                    cont: outer_prefix,
                },
            ) {
                out.push(RenderedLine {
                    line: wrapped,
                    block_idx: Some(b_idx),
                    task_checkbox: None,
                    has_wikilink: false,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: Some(code_bg),
                });
            }
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
            let badge_style = if resolved_path.is_some() {
                theme.tag_pill_style()
            } else {
                Style::default().fg(theme.warning).bg(theme.bg_card)
            };
            for wrapped in wrap_styled_spans(
                &[Span::styled(badge, badge_style)],
                width,
                WrapPrefix {
                    first: outer_prefix,
                    cont: outer_prefix,
                },
            ) {
                out.push(RenderedLine {
                    line: wrapped,
                    block_idx: Some(b_idx),
                    task_checkbox: None,
                    has_wikilink: false,
                    image_idx: img_idx,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: sel_bg,
                });
            }

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
            let mut embed_pfx = outer_prefix.to_vec();
            embed_pfx.push(Span::styled("┃ ", Style::default().fg(theme.accent)));
            let embed_content = vec![
                Span::styled("↗ Embed: ", Style::default().fg(theme.accent)),
                Span::styled(label, theme.wikilink_style(resolved_path.is_some())),
            ];
            let embed_bg = sel_bg.unwrap_or(theme.bg_card);
            for wrapped in wrap_styled_spans(
                &embed_content,
                width,
                WrapPrefix {
                    first: &embed_pfx,
                    cont: &embed_pfx,
                },
            ) {
                out.push(RenderedLine {
                    line: wrapped,
                    block_idx: Some(b_idx),
                    task_checkbox: None,
                    has_wikilink: true,
                    image_idx: None,
                    html_pill: None,
                    action_pills: Vec::new(),
                    bg_override: Some(embed_bg),
                });
            }
            if top_level {
                push_plain(out, Line::from(""), None);
            }
        }

        NoteBlock::Divider { .. } => {
            let dash_w = width.saturating_sub(prefix_w).clamp(1, 72);
            for wrapped in wrap_styled_spans(
                &[Span::styled(
                    "─".repeat(dash_w),
                    Style::default().fg(theme.divider),
                )],
                width,
                WrapPrefix {
                    first: outer_prefix,
                    cont: outer_prefix,
                },
            ) {
                out.push(RenderedLine {
                    line: wrapped,
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

    fn is_box_table_line(l: &str) -> bool {
        let t = l.trim();
        (t.starts_with('┌') && t.ends_with('┐'))
            || (t.starts_with('│') && t.ends_with('│'))
            || (t.starts_with('├') && t.ends_with('┤'))
            || (t.starts_with('└') && t.ends_with('┘'))
    }

    fn normalize_non_table_text(lines: &[String]) -> String {
        lines
            .iter()
            .filter(|l| !is_box_table_line(l))
            .map(|l| {
                l.replace("[w Wide]", "[w Mode]")
                    .replace("[w Center]", "[w Mode]")
            })
            .collect::<String>()
            .chars()
            .filter(|ch| {
                !ch.is_whitespace()
                    && !matches!(
                        ch,
                        '┃' | '│' | '▎' | '┌' | '┬' | '┐' | '├' | '┼' | '┤' | '└' | '┴' | '┘'
                            | '╭' | '╰' | '─'
                    )
            })
            .collect()
    }

    fn extract_table_columns(lines: &[String]) -> (Vec<String>, Vec<Vec<usize>>) {
        let mut cols: Vec<String> = Vec::new();
        let mut border_cols_per_row: Vec<Vec<usize>> = Vec::new();

        for line in lines {
            let trimmed = line.trim();
            if !is_box_table_line(trimmed) {
                continue;
            }
            let mut col_pos = 0usize;
            let mut positions = Vec::new();
            for ch in trimmed.chars() {
                if matches!(
                    ch,
                    '┌' | '┬' | '┐' | '├' | '┼' | '┤' | '└' | '┴' | '┘' | '│'
                ) {
                    positions.push(col_pos);
                }
                col_pos += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            }
            border_cols_per_row.push(positions);

            if trimmed.starts_with('│') {
                let inner = trimmed
                    .strip_prefix('│')
                    .and_then(|s| s.strip_suffix('│'))
                    .unwrap_or(trimmed);
                for (c_idx, cell) in inner.split('│').enumerate() {
                    if cols.len() <= c_idx {
                        cols.resize(c_idx + 1, String::new());
                    }
                    for ch in cell.chars() {
                        if !ch.is_whitespace() {
                            cols[c_idx].push(ch);
                        }
                    }
                }
            }
        }
        (cols, border_cols_per_row)
    }

    fn render_full_canvas_via_test_backend(
        app: &AppState,
        ui: &mut UiState,
        mode: PageWidthMode,
        total_visual_lines: usize,
    ) -> (Vec<String>, usize) {
        use ratatui::{backend::TestBackend, Terminal};

        ui.page_width = mode;
        ui.canvas_follow_selected = false;
        let term_w = 160u16;
        let term_h = 40u16;
        let content_x = match mode {
            PageWidthMode::Centered => (term_w - 86) / 2,
            PageWidthMode::Full => 1,
        };
        let content_w = match mode {
            PageWidthMode::Centered => 86u16,
            PageWidthMode::Full => term_w - 2,
        };

        let mut collected: Vec<String> = Vec::new();
        let mut offset = 0usize;
        let page_h = term_h as usize;

        while offset < total_visual_lines {
            let remaining = total_visual_lines - offset;
            let take = remaining.min(page_h);
            // `draw()` clamps `scroll` to `total_lines.saturating_sub(visible)`, so when the
            // final page is shorter than `page_h`, read only the trailing `take` rows.
            let max_scroll = total_visual_lines.saturating_sub(page_h);
            let actual_scroll = offset.min(max_scroll);
            let row_start = offset - actual_scroll;

            ui.canvas_scroll = offset;
            let backend = TestBackend::new(term_w, term_h);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut hitmap = HitMap::new();
            terminal
                .draw(|f| {
                    let area = f.area();
                    draw(f, area, app, ui, &mut hitmap);
                })
                .unwrap();

            let buf = terminal.backend().buffer();
            for y in row_start..(row_start + take) {
                let y_u16 = y as u16;
                // Margins outside `content_area` must remain completely blank (0 overflow).
                for x in 0..content_x {
                    assert_eq!(
                        buf[(x, y_u16)].symbol(),
                        " ",
                        "left margin overflow at ({x},{y_u16}) in {mode:?}"
                    );
                }
                for x in (content_x + content_w)..term_w {
                    assert_eq!(
                        buf[(x, y_u16)].symbol(),
                        " ",
                        "right margin overflow at ({x},{y_u16}) in {mode:?}"
                    );
                }
                let mut row_str = String::new();
                let mut x = content_x;
                while x < content_x + content_w {
                    let sym = buf[(x, y_u16)].symbol();
                    let w = UnicodeWidthStr::width(sym).max(1) as u16;
                    row_str.push_str(sym);
                    x += w;
                }
                collected.push(row_str.trim_end().to_string());
            }
            offset += take;
        }
        (collected, content_w as usize)
    }

    #[test]
    fn test_centered_and_full_width_modes_lossless_parity() {
        use ratatui::{backend::TestBackend, Terminal};

        let md = concat!(
            "---\n",
            "status: active\n",
            "owner: 플랫폼아키텍처팀\n",
            "quarter: 2026-Q4\n",
            "priority: critical\n",
            "reviewer: 김철수\n",
            "tags: [architecture, rust, obsidian, korean-wrap, layout, terminal-ui, parity-test, unicode-width]\n",
            "---\n\n",
            "# 트랙D 무손실 텍스트 래핑 동등성 테스트 문서 제목이 매우 길어서 여든여섯 컬럼을 초과합니다\n\n",
            "## 한국어 헤딩이 여든여섯 컬럼을 훨씬 초과하더라도 행잉 인덴트와 함께 무손실로 줄바꿈되어야 합니다\n\n",
            "- [ ] 태스크 항목의 체크박스 히트 영역은 첫 번째 줄에 유지되면서 아주 긴 한국어 설명 문장은 다음 줄로 깔끔하게 정렬되어 손실 없이 표시되어야 합니다\n\n",
            "> 인용문 블록 내부에서도 한국어 장문 텍스트가 여든여섯 컬럼 제한을 넘어가면 연속 줄마다 왼쪽 세로 막대 접두사가 유지되면서 단 한 글자도 사라지지 않아야 합니다.\n\n",
            "> [!tip] 콜아웃 헤더 제목도 매우 길게 작성되어 여든여섯 컬럼을 넘어갈 때 세로 막대와 카드 배경을 유지하며 줄바꿈됩니다\n",
            "> 바깥쪽 콜아웃 본문 역시 아주 긴 한국어 문장으로 작성되어 중앙 정렬 모드와 전체 폭 모드에서 동일한 텍스트를 무손실로 보여줍니다.\n",
            ">> [!note] 중첩 콜아웃 헤더도 길게 작성하여 여든여섯 컬럼 이상에서 안전하게 줄바꿈되는지 검증합니다\n",
            ">> 중첩된 콜아웃 본문도 손실 없이 안전하게 래핑됩니다.\n\n",
            "![[한국어_임베드_대상_노트_제목이_매우_길어서_여든여섯_컬럼을_초과하는_경우의_테스트_문서#세부섹션헤더]]\n\n",
            "![[architecture_diagram_2026_final_very_long_filename_아키텍처설계도.png|아주 긴 이미지 대체 텍스트 설명이 잘리지 않고 래핑됩니다]]\n\n",
            "공백없이연속으로이어지는매우긴한국어문자열테스트가여든여섯컬럼을초과할때도단한글자도유실되지않고안전하게분할되어야합니다 ",
            "https://example.com/very/long/path/without/any/spaces/for/lossless/wrapping/verification/2026/track-d ",
            "[[아주긴위키링크대상문서이름공백없이이어짐|표시별칭도매우긴한국어공백없는문자열테스트]]\n\n",
            "| 항목 | 담당 팀 | 설명 및 비고 | 상태 |\n",
            "| :--- | :---: | :--- | ---: |\n",
            "| 텍스트 래핑 | 플랫폼팀 | 한글과 English가 섞인 긴 셀 설명 텍스트가 여러 줄로 감깁니다 | 완료 |\n",
            "| 폭 모드 동등성 | 코어팀 | Centered 86 컬럼과 Full 폭 모드에서 단 한 글자도 사라지지 않음 | 검증중 |\n\n",
            "```rust\n",
            "\tfn verify_lossless_code_block_wrapping_with_tabs_and_long_identifier(viewport_width: usize, expected_token: &str) -> bool { viewport_width >= 10 && !expected_token.is_empty() }\n",
            "```\n",
        );

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("ParityNote.md"), md).unwrap();
        std::fs::write(root.join("Backlinker.md"), "# Backlinker\n\nLinks to [[ParityNote]]!\n")
            .unwrap();

        let vault = vault_from_dir(root, Some("ParityVault"));
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
        app.open_note(&mut ui, "ParityNote.md", false);

        let detail = app.active_note.as_ref().expect("ParityNote loaded");
        assert!(detail.summary.frontmatter.len() >= 5);
        assert_eq!(detail.summary.tags.len(), 8);

        // 1. Direct width invariants across narrow, centered (86), and full (158) widths.
        for &w in &[26usize, 48, 86, 158] {
            ui.page_width = if w == 86 {
                PageWidthMode::Centered
            } else {
                PageWidthMode::Full
            };
            let rendered = build_canvas_lines(detail, &app, &ui, &ui.theme, w, 0, true);
            for (idx, rl) in rendered.iter().enumerate() {
                let lw = rl.line.width();
                assert!(
                    lw <= w,
                    "canvas line {idx} exceeded width {w} (got {lw}): {:?}",
                    rl.line
                );
                for &(x_off, pw, _btn) in &rl.action_pills {
                    assert!(
                        (x_off as usize) + (pw as usize) <= w,
                        "action pill exceeded width {w} on line {idx}: x={x_off}, w={pw}"
                    );
                }
            }
            // All 9 quick action pills must survive via multi-line flow wrap even at narrow width.
            let total_pills: usize = rendered.iter().map(|rl| rl.action_pills.len()).sum();
            assert_eq!(
                total_pills, 9,
                "expected all 9 quick action pills preserved at width {w}, got {total_pills}"
            );
        }

        // Verify structural continuation prefixes and hits at Centered (86) width.
        ui.page_width = PageWidthMode::Centered;
        let centered_rendered = build_canvas_lines(detail, &app, &ui, &ui.theme, 86, 0, true);
        let centered_text = lines_to_text(&centered_rendered);
        assert!(
            centered_rendered.iter().any(|rl| {
                let s: String = rl.line.spans.iter().map(|sp| sp.content.as_ref()).collect();
                s.starts_with("   ") && s.contains("줄바꿈되어야")
            }),
            "expected wrapped Heading continuation with 3-space hanging indent in:\n{centered_text}"
        );
        let task_lines: Vec<&RenderedLine> = centered_rendered
            .iter()
            .filter(|rl| {
                let s: String = rl.line.spans.iter().map(|sp| sp.content.as_ref()).collect();
                s.contains("태스크 항목") || s.contains("표시되어야 합니다")
            })
            .collect();
        assert!(
            task_lines.len() >= 2,
            "expected wrapped Task to span >= 2 lines at width 86"
        );
        assert!(
            task_lines[0].task_checkbox.is_some(),
            "task_checkbox hit must stay on first wrapped Task row"
        );
        assert!(
            task_lines[1].task_checkbox.is_none(),
            "task_checkbox hit must not duplicate onto continuation rows"
        );
        let callout_cont_lines: Vec<&RenderedLine> = centered_rendered
            .iter()
            .filter(|rl| {
                let s: String = rl.line.spans.iter().map(|sp| sp.content.as_ref()).collect();
                s.starts_with("┃ ") && rl.bg_override == Some(ui.theme.bg_card)
            })
            .collect();
        assert!(
            callout_cont_lines.len() >= 6,
            "expected wrapped Callout header + body + nested callout lines with ┃ prefix & card_bg"
        );
        assert!(
            centered_text.contains("│     fn")
                && centered_text.contains("verify_lossless_code_block_wrapping"),
            "expected \\t -> 4-space expansion in CodeBlock:\n{centered_text}"
        );
        assert!(
            centered_text.contains("!expected_token.is_empty() }"),
            "expected 120-char CodeBlock tail preserved without truncation:\n{centered_text}"
        );

        ui.page_width = PageWidthMode::Full;
        let full_rendered = build_canvas_lines(detail, &app, &ui, &ui.theme, 158, 0, true);

        // 2. TestBackend::new(160, 40) Centered (86) <-> Full (158) lossless parity check.
        let (centered_buf_lines, _) = render_full_canvas_via_test_backend(
            &app,
            &mut ui,
            PageWidthMode::Centered,
            centered_rendered.len(),
        );
        let (full_buf_lines, _) = render_full_canvas_via_test_backend(
            &app,
            &mut ui,
            PageWidthMode::Full,
            full_rendered.len(),
        );

        let norm_centered = normalize_non_table_text(&centered_buf_lines);
        let norm_full = normalize_non_table_text(&full_buf_lines);
        assert_eq!(
            norm_centered, norm_full,
            "Centered(86) and Full(158) non-table normalized content must match losslessly"
        );

        // Table column-by-column lossless parity + exact `│` vertical border alignment.
        let (c_cols, c_borders) = extract_table_columns(&centered_buf_lines);
        let (f_cols, f_borders) = extract_table_columns(&full_buf_lines);
        assert_eq!(c_cols.len(), 4, "expected 4 table columns in centered mode");
        assert_eq!(
            c_cols, f_cols,
            "Centered(86) and Full(158) table column content must match losslessly"
        );
        assert!(
            c_borders.len() >= 5 && f_borders.len() >= 5,
            "expected box table border rows"
        );
        for (mode_name, borders) in [("centered", &c_borders), ("full", &f_borders)] {
            let expected_pos = &borders[0];
            assert_eq!(
                expected_pos.len(),
                5,
                "{mode_name}: 4-col box table must have 5 vertical border columns"
            );
            for (r_idx, row_pos) in borders.iter().enumerate() {
                assert_eq!(
                    row_pos, expected_pos,
                    "{mode_name}: table row {r_idx} vertical border columns misaligned: {row_pos:?} vs {expected_pos:?}"
                );
            }
        }

        // Spot-check critical tokens present in both modes.
        let joined_centered = centered_buf_lines.join("\n");
        for must_have in [
            "플랫폼아키텍처팀",
            "김철수",
            "#unicode-width",
            "#parity-test",
            "줄바꿈되어야",
            "표시되어야",
            "중첩된",
            "세부섹션헤더",
            "verify_lossless_code_block_wrapping",
            "expected_token.is_empty()",
            "track-d",
            "표시별칭도매우긴한국어",
        ] {
            let needle: String = must_have.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                norm_centered.contains(&needle),
                "missing expected token `{must_have}` in Centered(86) output:\n{joined_centered}"
            );
        }

        // 3. Inspector 25-col 0 overflow (both populated note & empty note states).
        {
            let backend = TestBackend::new(40, 28);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut hitmap = HitMap::new();
            let insp_rect = Rect::new(0, 0, 25, 28);
            terminal
                .draw(|f| {
                    crate::ui::inspector::draw(f, insp_rect, &app, &ui, &mut hitmap);
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let mut insp_text = String::new();
            for y in 0..28u16 {
                for x in 25..40u16 {
                    assert_eq!(
                        buf[(x, y)].symbol(),
                        " ",
                        "inspector overflowed past 25 cols at ({x},{y})"
                    );
                }
                let mut x = 0u16;
                while x < 25 {
                    let sym = buf[(x, y)].symbol();
                    insp_text.push_str(sym);
                    x += UnicodeWidthStr::width(sym).max(1) as u16;
                }
                insp_text.push('\n');
            }
            assert!(
                insp_text.contains("Tasks:") && insp_text.contains("0/1"),
                "25-col inspector must wrap and preserve Tasks stat:\n{insp_text}"
            );
            assert!(
                insp_text.contains("Linked Mentions"),
                "25-col inspector must render clamped Linked Mentions header:\n{insp_text}"
            );

            // Empty-note state at 25 cols must wrap `"Select a note to inspect."` without clipping.
            let mut empty_app = AppState::new(
                cfg.clone(),
                vault.clone(),
                Storage::open_in_memory().unwrap(),
                {
                    let (ftx, _frx) = watch::channel(FocusTarget::default());
                    let (ctx, _crx) = mpsc::unbounded_channel::<SyncCommand>();
                    SyncController::from_channels(ftx, ctx)
                },
            );
            empty_app.active_note = None;
            let backend_empty = TestBackend::new(40, 20);
            let mut term_empty = Terminal::new(backend_empty).unwrap();
            let mut hm_empty = HitMap::new();
            term_empty
                .draw(|f| {
                    crate::ui::inspector::draw(
                        f,
                        Rect::new(0, 0, 25, 20),
                        &empty_app,
                        &ui,
                        &mut hm_empty,
                    );
                })
                .unwrap();
            let ebuf = term_empty.backend().buffer();
            let mut empty_text = String::new();
            for y in 0..20u16 {
                for x in 25..40u16 {
                    assert_eq!(
                        ebuf[(x, y)].symbol(),
                        " ",
                        "empty inspector overflowed past 25 cols at ({x},{y})"
                    );
                }
                for x in 0..25u16 {
                    empty_text.push_str(ebuf[(x, y)].symbol());
                }
                empty_text.push('\n');
            }
            assert!(
                empty_text.contains("Select a note") && empty_text.contains("inspect."),
                "25-col empty inspector must wrap full message:\n{empty_text}"
            );
        }

        // 4. Sidebar TagHeader 0 overflow at narrow 24-col width.
        {
            ui.sidebar_tab = crate::models::SidebarTab::Tags;
            ui.active_tag_filter = Some(
                "아주긴한글태그이름_very_long_nested/subtag/architecture_2026_overflow_check"
                    .to_string(),
            );
            let backend = TestBackend::new(40, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut hitmap = HitMap::new();
            let side_rect = Rect::new(0, 0, 24, 20);
            terminal
                .draw(|f| {
                    crate::ui::sidebar::draw(f, side_rect, &app, &ui, &mut hitmap);
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let mut side_text = String::new();
            for y in 0..20u16 {
                // Right border of sidebar must stay intact at x = 23 and columns 24..40 blank.
                assert_eq!(
                    buf[(23, y)].symbol(),
                    "│",
                    "sidebar right border overwritten at row {y}"
                );
                for x in 24..40u16 {
                    assert_eq!(
                        buf[(x, y)].symbol(),
                        " ",
                        "sidebar TagHeader overflowed past 24 cols at ({x},{y})"
                    );
                }
                let mut x = 0u16;
                while x < 23 {
                    let sym = buf[(x, y)].symbol();
                    side_text.push_str(sym);
                    x += UnicodeWidthStr::width(sym).max(1) as u16;
                }
                side_text.push('\n');
            }
            assert!(
                side_text.contains("▾") && side_text.contains("…") && side_text.contains("notes"),
                "narrow sidebar TagHeader must clamp with ellipsis and preserve count:\n{side_text}"
            );
        }
    }
}
