//! Interactive HTML Preview Overlay (`H` or clicking `[H: HTML]` / vault `.html` files).
//!
//! Backed by `bitefoundry_html::HtmlViewSession`:
//! - `0 MB` idle JS footprint on static HTML
//! - On-demand `8 MiB` sandboxed QuickJS-NG when `<script>` or `on*` handlers are present,
//!   immediately freed on close (`Drop`)
//! - Full keyboard + mouse interaction: `Tab`/`Shift+Tab` focus, `Enter`/`Space` activate,
//!   mouse `click_at(line, col)`, live text input typing, `m` Rendered <-> Raw HTML toggle,
//!   `o` lazy localhost Axum serving & external browser launch, `j`/`k`/wheel scroll + `Scrollbar`,
//!   `Esc`/`q`/click-outside close.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bitefoundry_html::{
    looks_like_html_bytes, looks_like_svg_bytes, HtmlEffect, HtmlTheme, HtmlViewSession,
};
use ratatui::{
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::{
        self, FitBox, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol,
    },
    models::{NoteBlock, NoteDetail},
    ui::{
        action::Action,
        app::{PendingPixelGraphic, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
        theme::Theme,
        truncate_width,
    },
    vault::{
        fs::{is_html_file_path, resolve_attachment_path},
        parser::spans_to_plain,
    },
};

/// An HTML snippet or linked `.html`/`.htm`/`.xhtml` document discovered in a Markdown note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlCandidate {
    pub title: String,
    pub source_label: String,
    pub html: String,
    pub block_idx: Option<usize>,
    pub file_path: Option<PathBuf>,
}

/// Tracks where an open `HtmlPreviewOverlay` loaded its HTML from so live disk updates can refresh it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HtmlPreviewSource {
    #[default]
    Inline,
    File(PathBuf),
    NoteBlock {
        note_rel_path: String,
        block_idx: usize,
    },
}

/// Returns `true` if a fenced code block language tag denotes HTML.
pub fn is_html_code_language(lang: &str) -> bool {
    let token = lang
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(token.as_str(), "html" | "htm" | "xhtml" | "html5")
}

/// Returns `true` when raw text from a Markdown block looks like an embedded HTML snippet.
pub fn looks_like_embedded_html(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if looks_like_html_bytes(trimmed.as_bytes()) || looks_like_svg_bytes(trimmed.as_bytes()) {
        return true;
    }
    // Also recognize common interactive/graphic HTML elements that can appear standalone in Markdown notes.
    let lower = trimmed.to_ascii_lowercase();
    const EXTRA_PREFIXES: &[&str] = &[
        "<details",
        "<summary",
        "<input",
        "<select",
        "<textarea",
        "<label",
        "<a ",
        "<img ",
        "<svg",
        "<canvas",
    ];
    EXTRA_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix) && trimmed.contains('>'))
}

/// Reconstructs verbatim raw text for a block in `detail.raw_markdown` when available,
/// falling back to the parsed inline spans.
pub fn extract_raw_block_text(detail: &NoteDetail, block_idx: usize) -> Option<String> {
    let block = detail.parsed.blocks.get(block_idx)?;
    let start_line = block.line_no();
    if !detail.raw_markdown.is_empty() && start_line >= 1 {
        let lines: Vec<&str> = detail.raw_markdown.lines().collect();
        let start_idx = start_line - 1;
        if start_idx < lines.len() {
            let end_idx = detail
                .parsed
                .blocks
                .get(block_idx + 1)
                .map(|next| next.line_no().saturating_sub(1).min(lines.len()))
                .unwrap_or(lines.len());
            if end_idx > start_idx {
                let mut slice = &lines[start_idx..end_idx];
                while slice.last().is_some_and(|l| l.trim().is_empty()) {
                    slice = &slice[..slice.len() - 1];
                }
                let joined = slice.join("\n");
                if !joined.trim().is_empty() {
                    return Some(joined);
                }
            }
        }
    }

    match block {
        NoteBlock::CodeBlock { code, .. } => Some(code.clone()),
        NoteBlock::Paragraph { spans, .. } => Some(spans_to_plain(spans)),
        _ => None,
    }
}

/// Returns `true` if the specific block at `block_idx` is an HTML code block or embedded raw HTML.
pub fn block_is_html(detail: &NoteDetail, block_idx: usize) -> bool {
    let Some(block) = detail.parsed.blocks.get(block_idx) else {
        return false;
    };
    match block {
        NoteBlock::CodeBlock { language, code, .. } => {
            is_html_code_language(language) && !code.trim().is_empty()
        }
        NoteBlock::Paragraph { .. } => extract_raw_block_text(detail, block_idx)
            .is_some_and(|raw| looks_like_embedded_html(&raw)),
        NoteBlock::NoteEmbed { target, .. } => is_html_file_path(target),
        _ => false,
    }
}

/// Extracts all HTML candidates from a Markdown note:
/// 1. Fenced ```` ```html ```` / ```` ```htm ```` / ```` ```xhtml ```` code blocks
/// 2. Embedded raw HTML blocks inside paragraphs
/// 3. Referenced/embedded `.html`/`.htm`/`.xhtml` files inside the vault.
pub fn extract_note_html_candidates(
    detail: &NoteDetail,
    vault_root: Option<&Path>,
) -> Vec<HtmlCandidate> {
    let mut out = Vec::new();
    let mut seen_files: Vec<String> = Vec::new();

    for (b_idx, block) in detail.parsed.blocks.iter().enumerate() {
        match block {
            NoteBlock::CodeBlock {
                language,
                code,
                line_no,
            } if is_html_code_language(language) && !code.trim().is_empty() => {
                out.push(HtmlCandidate {
                    title: format!("{} (HTML)", detail.summary.title),
                    source_label: format!("```{} · line {}", language.trim(), line_no),
                    html: code.clone(),
                    block_idx: Some(b_idx),
                    file_path: None,
                });
            }
            NoteBlock::Paragraph { line_no, .. } => {
                if let Some(raw) = extract_raw_block_text(detail, b_idx) {
                    if looks_like_embedded_html(&raw) {
                        out.push(HtmlCandidate {
                            title: format!("{} (Embedded HTML)", detail.summary.title),
                            source_label: format!("inline HTML · line {line_no}"),
                            html: raw,
                            block_idx: Some(b_idx),
                            file_path: None,
                        });
                    }
                }
            }
            NoteBlock::NoteEmbed { target, .. } if is_html_file_path(target) => {
                if let Some(root) = vault_root {
                    if let Some(abs) =
                        resolve_attachment_path(root, &detail.summary.rel_path, target)
                    {
                        if let Ok(html) = fs::read_to_string(&abs) {
                            let key = abs.to_string_lossy().to_ascii_lowercase();
                            if !seen_files.contains(&key) {
                                seen_files.push(key);
                                out.push(HtmlCandidate {
                                    title: target.clone(),
                                    source_label: abs.display().to_string(),
                                    html,
                                    block_idx: Some(b_idx),
                                    file_path: Some(abs),
                                });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Also include local `.html`/`.htm`/`.xhtml` files referenced via Markdown links or wikilinks.
    if let Some(root) = vault_root {
        for ml in &detail.parsed.md_links {
            if is_html_file_path(&ml.url) {
                if let Some(abs) = resolve_attachment_path(root, &detail.summary.rel_path, &ml.url)
                {
                    let key = abs.to_string_lossy().to_ascii_lowercase();
                    if !seen_files.contains(&key) {
                        if let Ok(html) = fs::read_to_string(&abs) {
                            seen_files.push(key);
                            out.push(HtmlCandidate {
                                title: if ml.label.trim().is_empty() {
                                    ml.url.clone()
                                } else {
                                    ml.label.clone()
                                },
                                source_label: abs.display().to_string(),
                                html,
                                block_idx: None,
                                file_path: Some(abs),
                            });
                        }
                    }
                }
            }
        }
    }

    out
}

/// Fast check for whether the active note contains any HTML code block, raw HTML, or `.html` link.
pub fn note_has_html(detail: &NoteDetail) -> bool {
    if (0..detail.parsed.blocks.len()).any(|idx| block_is_html(detail, idx)) {
        return true;
    }
    detail
        .parsed
        .md_links
        .iter()
        .any(|ml| is_html_file_path(&ml.url))
}

/// Maps Obsidian's active `Theme` palette to `bitefoundry_html::HtmlTheme`.
pub fn html_theme_from_ui(theme: &Theme) -> HtmlTheme {
    if theme.no_color {
        return HtmlTheme::no_color();
    }
    HtmlTheme {
        text: theme.fg_primary,
        muted: theme.fg_muted,
        heading: theme.accent,
        accent: theme.accent_soft,
        link: theme.info,
        code_fg: theme.code_fg,
        code_bg: theme.bg_code,
        quote_border: theme.fg_muted,
        border: theme.border,
        control_fg: theme.fg_primary,
        control_bg: theme.bg_card,
        focus_fg: theme.bg_canvas,
        focus_bg: theme.accent,
        raw_tag: theme.accent,
        raw_attr: theme.code_fg,
    }
}

/// Interactive modal HTML preview overlay backed by `bitefoundry_html::HtmlViewSession`.
pub struct HtmlPreviewOverlay {
    pub session: Box<HtmlViewSession>,
    pub title: String,
    pub source_label: String,
    pub source: HtmlPreviewSource,
    pub last_modal_rect: Rect,
    pub last_content_rect: Rect,
    pub last_total_lines: usize,
    pub last_visible_lines: usize,
    pending_scroll_to_focus: bool,
    /// Rendered inline graphics keyed by `(node_id, width, protocol)` (cleared on `Drop`).
    graphic_cache: HashMap<(usize, u16, TerminalImageProtocol), (Arc<[u8]>, RenderedTerminalImage)>,
    graphic_cache_cell_px: (u16, u16),
    graphic_cache_bg: (u8, u8, u8),
}

impl Drop for HtmlPreviewOverlay {
    fn drop(&mut self) {
        self.graphic_cache.clear();
    }
}

impl fmt::Debug for HtmlPreviewOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HtmlPreviewOverlay")
            .field("title", &self.title)
            .field("source_label", &self.source_label)
            .field("source", &self.source)
            .field("raw_mode", &self.session.raw_mode)
            .field("scroll_y", &self.session.scroll_y)
            .field("focused_control", &self.session.focused_control)
            .field("js_active", &self.session.js_active)
            .field("last_status", &self.session.last_status)
            .field("cached_graphics", &self.graphic_cache.len())
            .finish()
    }
}

impl HtmlPreviewOverlay {
    pub fn new(title: impl Into<String>, source_label: impl Into<String>, html: String) -> Self {
        let title_str = title.into();
        let mut session = HtmlViewSession::new(
            if title_str.trim().is_empty() {
                None
            } else {
                Some(title_str.clone())
            },
            html,
        );
        let resolved_title = session.title().map(ToString::to_string).unwrap_or_else(|| {
            if title_str.trim().is_empty() {
                "HTML Preview".to_string()
            } else {
                title_str
            }
        });
        // Drain any startup effects so startup alerts show cleanly in last_status.
        let _ = session.take_pending_effects();
        Self {
            session: Box::new(session),
            title: resolved_title,
            source_label: source_label.into(),
            source: HtmlPreviewSource::Inline,
            last_modal_rect: Rect::default(),
            last_content_rect: Rect::default(),
            last_total_lines: 0,
            last_visible_lines: 0,
            pending_scroll_to_focus: false,
            graphic_cache: HashMap::new(),
            graphic_cache_cell_px: (0, 0),
            graphic_cache_bg: (0, 0, 0),
        }
    }

    pub fn with_source(mut self, source: HtmlPreviewSource) -> Self {
        self.source = source;
        self
    }

    /// Rebuilds the preview session when backing HTML changed on disk, preserving view mode and scroll.
    pub fn reload_html_if_changed(&mut self, html: String) -> bool {
        if self.session.raw_html() == html {
            return false;
        }
        let was_raw = self.session.raw_mode;
        let prev_scroll = self.session.scroll_y;
        let mut session = HtmlViewSession::new(
            if self.title.trim().is_empty() {
                None
            } else {
                Some(self.title.clone())
            },
            html,
        );
        if let Some(t) = session.title() {
            if !t.trim().is_empty() {
                self.title = t.to_string();
            }
        }
        let _ = session.take_pending_effects();
        session.raw_mode = was_raw;
        session.scroll_y = prev_scroll;
        session.last_status = Some("Reloaded updated HTML".to_string());
        *self.session = session;
        self.graphic_cache.clear();
        true
    }

    pub fn is_editing_input(&self) -> bool {
        self.session.editing_input.is_some()
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(24).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    /// Directly activates a control at document-space `(line_index, col)` coordinates.
    pub fn click_at_doc(&mut self, line_index: usize, col: u16) -> Vec<HtmlEffect> {
        self.session.click_at(line_index, col)
    }

    /// Translates terminal screen coordinates `(col, row)` inside `last_content_rect`
    /// into document coordinates `(scroll_y + rel_row, rel_col)` and dispatches `session.click_at`.
    pub fn click_at_screen(&mut self, col: u16, row: u16) -> Vec<HtmlEffect> {
        let content = self.last_content_rect;
        if content.width == 0 || content.height == 0 {
            return Vec::new();
        }
        if !content.contains(Position::new(col, row)) {
            if self.session.editing_input.is_some() {
                return self.session.finish_editing();
            }
            return Vec::new();
        }
        let rel_row = (row - content.y) as usize;
        let line_index = self.session.scroll_y as usize + rel_row;
        let rel_col = col - content.x;
        self.session.click_at(line_index, rel_col)
    }

    fn effects_to_outcome(&mut self, effects: Vec<HtmlEffect>) -> OverlayOutcome {
        let mut open_url: Option<String> = None;
        let mut toast: Option<String> = None;
        for eff in effects {
            match eff {
                HtmlEffect::OpenUrl(url) => {
                    self.session.last_status = Some(format!("Opening {url}"));
                    open_url = Some(url);
                }
                HtmlEffect::Toast(msg) => {
                    self.session.last_status = Some(msg.clone());
                    toast = Some(msg);
                }
                HtmlEffect::FormSubmitted { action, fields } => {
                    let target = if action.trim().is_empty() {
                        "(inline form)"
                    } else {
                        action.as_str()
                    };
                    let summary = format!("Form submitted -> {target} ({} fields)", fields.len());
                    self.session.last_status = Some(summary.clone());
                    toast = Some(summary);
                }
                HtmlEffect::DomChanged | HtmlEffect::None => {}
            }
        }
        if let Some(url) = open_url {
            OverlayOutcome::Dispatch(Action::OpenUrl(url))
        } else if let Some(msg) = toast {
            OverlayOutcome::Dispatch(Action::ShowInfoToast(msg))
        } else {
            OverlayOutcome::Handled
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        // Explicit border button actions always work even while editing an input.
        match action {
            Action::HtmlToggleRawMode => {
                let raw = self.session.toggle_raw_mode();
                self.session.scroll_y = 0;
                self.session.last_status = Some(if raw {
                    "Raw HTML source view".to_string()
                } else {
                    "Rendered HTML view".to_string()
                });
                return OverlayOutcome::Handled;
            }
            Action::ServeHtmlInBrowser { .. } => {
                return OverlayOutcome::Dispatch(Action::ServeHtmlInBrowser {
                    title: self.title.clone(),
                    html: self.session.raw_html().to_string(),
                });
            }
            _ => {}
        }

        // When editing an HTML <input> or <textarea>, route typing directly to the field.
        if self.session.editing_input.is_some() {
            return match action {
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                Action::CloseOverlay | Action::Activate | Action::OverlaySubmit => {
                    let effects = self.session.finish_editing();
                    self.effects_to_outcome(effects)
                }
                Action::Tab | Action::NextFocus => {
                    let effects = self.session.finish_editing();
                    self.session.focus_next_control();
                    self.pending_scroll_to_focus = true;
                    self.effects_to_outcome(effects)
                }
                Action::PrevFocus => {
                    let effects = self.session.finish_editing();
                    self.session.focus_prev_control();
                    self.pending_scroll_to_focus = true;
                    self.effects_to_outcome(effects)
                }
                Action::Backspace => {
                    self.session.backspace();
                    OverlayOutcome::Handled
                }
                Action::Input(ch) => {
                    self.session.input_char(*ch);
                    OverlayOutcome::Handled
                }
                Action::Paste(text) => {
                    for ch in text.chars() {
                        if ch != '\r' && ch != '\n' {
                            self.session.input_char(ch);
                        }
                    }
                    OverlayOutcome::Handled
                }
                Action::MouseScrollDown { .. } => {
                    self.session.scroll_y = self.session.scroll_y.saturating_add(3);
                    OverlayOutcome::Handled
                }
                Action::MouseScrollUp { .. } => {
                    self.session.scroll_y = self.session.scroll_y.saturating_sub(3);
                    OverlayOutcome::Handled
                }
                Action::MouseClick { col, row } => {
                    if !self.last_modal_rect.is_empty()
                        && !self.last_modal_rect.contains(Position::new(*col, *row))
                    {
                        return OverlayOutcome::Close;
                    }
                    let effects = self.click_at_screen(*col, *row);
                    self.effects_to_outcome(effects)
                }
                _ => OverlayOutcome::Handled,
            };
        }

        match action {
            Action::CloseOverlay
            | Action::OpenHtmlPreview
            | Action::Input('q')
            | Action::Input('H') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::Tab | Action::NextFocus => {
                self.session.focus_next_control();
                self.pending_scroll_to_focus = true;
                OverlayOutcome::Handled
            }
            Action::PrevFocus => {
                self.session.focus_prev_control();
                self.pending_scroll_to_focus = true;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit | Action::Input(' ') => {
                let effects = self.session.activate_focused();
                self.effects_to_outcome(effects)
            }
            Action::Input('m') | Action::Input('M') => {
                let raw = self.session.toggle_raw_mode();
                self.session.scroll_y = 0;
                self.session.last_status = Some(if raw {
                    "Raw HTML source view".to_string()
                } else {
                    "Rendered HTML view".to_string()
                });
                OverlayOutcome::Handled
            }
            Action::Input('o') | Action::Input('O') => {
                OverlayOutcome::Dispatch(Action::ServeHtmlInBrowser {
                    title: self.title.clone(),
                    html: self.session.raw_html().to_string(),
                })
            }
            Action::MoveDown | Action::Input('j') => {
                self.session.scroll_y = self.session.scroll_y.saturating_add(1);
                OverlayOutcome::Handled
            }
            Action::MoveUp | Action::Input('k') => {
                self.session.scroll_y = self.session.scroll_y.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.session.scroll_y = self.session.scroll_y.saturating_add(10);
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.session.scroll_y = self.session.scroll_y.saturating_sub(10);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::JumpTop | Action::Input('g') => {
                self.session.scroll_y = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd
            | Action::JumpBottom
            | Action::Input('G')
            | Action::ToggleGraphView => {
                self.session.scroll_y = u16::MAX;
                OverlayOutcome::Handled
            }
            Action::MouseScrollDown { .. } => {
                self.session.scroll_y = self.session.scroll_y.saturating_add(3);
                OverlayOutcome::Handled
            }
            Action::MouseScrollUp { .. } => {
                self.session.scroll_y = self.session.scroll_y.saturating_sub(3);
                OverlayOutcome::Handled
            }
            Action::MouseClick { col, row } => {
                if !self.last_modal_rect.is_empty()
                    && !self.last_modal_rect.contains(Position::new(*col, *row))
                {
                    return OverlayOutcome::Close;
                }
                let effects = self.click_at_screen(*col, *row);
                self.effects_to_outcome(effects)
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        caps: TerminalGraphicsCaps,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = ui.theme.clone();
        let rect = Self::modal_rect(area);
        self.last_modal_rect = rect;
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let mode_badge = if self.session.raw_mode {
            "Raw"
        } else {
            "Rendered"
        };
        let js_badge = if self.session.js_active {
            "JS:on"
        } else {
            "Static"
        };

        let raw_pill = if self.session.raw_mode {
            "[m View]"
        } else {
            "[m Raw]"
        };
        let open_pill = "[o Browser]";
        let close_pill = "[✕ Esc]";
        let raw_w = raw_pill.width() as u16;
        let open_w = open_pill.width() as u16;
        let close_w = close_pill.width() as u16;

        let show_close = rect.width > close_w + 6;
        let show_action_pills = rect.width > raw_w + open_w + close_w + 20;
        let pills_reserved = if show_action_pills {
            raw_w + open_w + close_w + 5
        } else if show_close {
            close_w + 2
        } else {
            0
        };
        let title_budget = rect.width.saturating_sub(2 + pills_reserved) as usize;

        let title_str = format!(
            " HTML: {} · {} · {} · Tab Focus · Enter Act · j/k Scroll ",
            truncate_width(&self.title, 24),
            mode_badge,
            js_badge,
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .border_style(theme.border_style(true))
            .title(Line::from(Span::styled(
                truncate_width(&title_str, title_budget),
                theme.title(true),
            )));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        // Right-aligned interactive border pills: [m Raw] [o Browser] [✕ Esc]
        let mut right_cursor = rect.right().saturating_sub(1);
        if show_close {
            let x = right_cursor.saturating_sub(close_w);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    close_pill,
                    Style::default().fg(theme.fg).bg(theme.bg_elevated),
                )),
                Rect::new(x, rect.y, close_w, 1),
            );
            hitmap.add_line(
                x,
                rect.y,
                close_w,
                HitTarget::Button(ButtonId::ConfirmCancel),
            );
            right_cursor = x.saturating_sub(1);
        }
        if show_action_pills {
            let ox = right_cursor.saturating_sub(open_w);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    open_pill,
                    Style::default().fg(theme.accent_soft).bg(theme.bg_elevated),
                )),
                Rect::new(ox, rect.y, open_w, 1),
            );
            hitmap.add_line(
                ox,
                rect.y,
                open_w,
                HitTarget::Button(ButtonId::HtmlServeBrowser),
            );
            right_cursor = ox.saturating_sub(1);

            let mx = right_cursor.saturating_sub(raw_w);
            let m_style = if self.session.raw_mode {
                Style::default()
                    .fg(theme.bg_canvas)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.accent_soft).bg(theme.bg_elevated)
            };
            frame.render_widget(
                Paragraph::new(Span::styled(raw_pill, m_style)),
                Rect::new(mx, rect.y, raw_w, 1),
            );
            hitmap.add_line(
                mx,
                rect.y,
                raw_w,
                HitTarget::Button(ButtonId::HtmlToggleRaw),
            );
        }

        if inner.width == 0 || inner.height == 0 {
            self.last_content_rect = Rect::default();
            return;
        }

        let has_status_row = inner.height >= 4;
        let content_h = if has_status_row {
            inner.height.saturating_sub(1)
        } else {
            inner.height
        };
        let visible = content_h as usize;

        let html_theme = html_theme_from_ui(&theme);

        // First render at full inner width to check whether a vertical scrollbar is needed.
        let initial_lines = self
            .session
            .render(inner.width, &html_theme, ui.show_inline_hrefs)
            .lines
            .len();
        let show_scrollbar = initial_lines > visible && inner.width > 2;
        let content_w = if show_scrollbar {
            inner.width.saturating_sub(1)
        } else {
            inner.width
        };

        let (mut lines, total_lines, control_count, focused_line, graphics) = {
            let focused_idx = self.session.focused_control;
            let rendered = self
                .session
                .render(content_w, &html_theme, ui.show_inline_hrefs);
            let focused_line = focused_idx
                .and_then(|idx| rendered.controls.get(idx))
                .map(|c| c.line);
            (
                rendered.lines.clone(),
                rendered.lines.len(),
                rendered.controls.len(),
                focused_line,
                rendered.graphics.clone(),
            )
        };

        let max_scroll = total_lines.saturating_sub(visible) as u16;
        if self.pending_scroll_to_focus {
            if let Some(fl) = focused_line {
                let sy = self.session.scroll_y as usize;
                if fl < sy {
                    self.session.scroll_y = fl.min(u16::MAX as usize) as u16;
                } else if fl >= sy + visible && visible > 0 {
                    self.session.scroll_y =
                        fl.saturating_sub(visible - 1).min(u16::MAX as usize) as u16;
                }
            }
            self.pending_scroll_to_focus = false;
        }
        self.session.scroll_y = self.session.scroll_y.min(max_scroll);

        let content_rect = Rect::new(inner.x, inner.y, content_w, content_h);
        self.last_content_rect = content_rect;
        self.last_total_lines = total_lines;
        self.last_visible_lines = visible;

        let mut pixel_placements: Vec<(Rect, String, String, TerminalImageProtocol)> = Vec::new();
        if !self.session.raw_mode && !graphics.is_empty() && content_w > 0 && visible > 0 {
            let bg_rgb = img_preview::color_to_rgb(theme.bg_elevated, (19, 19, 24));
            if self.graphic_cache_cell_px != caps.cell_px || self.graphic_cache_bg != bg_rgb {
                self.graphic_cache.clear();
                self.graphic_cache_cell_px = caps.cell_px;
                self.graphic_cache_bg = bg_rgb;
            }

            let vp_start = self.session.scroll_y as usize;
            let vp_end = vp_start.saturating_add(visible);

            for g in &graphics {
                let g_start = g.line_start;
                let g_end = g.line_start.saturating_add(g.line_count);
                if g.line_count == 0 || g_end <= vp_start || g_start >= vp_end {
                    continue;
                }

                let cache_key = (g.node_id.0, content_w, caps.protocol);
                let needs_render = match self.graphic_cache.get(&cache_key) {
                    Some((cached_bytes, _)) => !Arc::ptr_eq(cached_bytes, &g.image_bytes),
                    None => true,
                };
                if needs_render {
                    let fit = FitBox::fit_to_box(content_w.max(4), (g.line_count as u16).max(2));
                    let decoded = if caps.protocol == TerminalImageProtocol::Halfblock {
                        img_preview::decode_and_render_halfblocks_with_cell_px(
                            &g.image_bytes,
                            fit,
                            bg_rgb,
                            caps.cell_px,
                        )
                    } else {
                        img_preview::decode_and_render_for_protocol(
                            &g.image_bytes,
                            fit,
                            bg_rgb,
                            caps,
                        )
                    };
                    if let Ok(img) = decoded {
                        self.graphic_cache
                            .insert(cache_key, (Arc::clone(&g.image_bytes), img));
                    }
                }

                let Some((_, rendered_img)) = self.graphic_cache.get(&cache_key) else {
                    continue;
                };

                if caps.protocol == TerminalImageProtocol::Halfblock {
                    for (offset, img_line) in rendered_img
                        .lines
                        .iter()
                        .take(g.line_count)
                        .enumerate()
                    {
                        let dst_idx = g.line_start + offset;
                        if let Some(slot) = lines.get_mut(dst_idx) {
                            *slot = img_line.clone();
                        }
                    }
                } else {
                    let vis_start = g_start.max(vp_start);
                    let vis_end = g_end.min(vp_end);
                    let crop_top = (vis_start - g_start) as u16;
                    let vis_rows = (vis_end - vis_start) as u16;
                    if vis_rows == 0 {
                        continue;
                    }
                    if let Some((mut payload, draw_cols, draw_rows)) =
                        rendered_img.viewport_pixel_payload(0, crop_top, content_w, vis_rows)
                    {
                        let draw_cols = draw_cols.min(content_w);
                        let draw_rows = draw_rows.min(vis_rows);
                        if draw_cols > 0 && draw_rows > 0 {
                            if caps.protocol == TerminalImageProtocol::Kitty {
                                let kitty_id = 5000u32.saturating_add(g.node_id.0 as u32);
                                let from = format!(",i={},", img_preview::KITTY_PREVIEW_IMAGE_ID);
                                let to = format!(",i={kitty_id},");
                                payload = payload.replacen(&from, &to, 1);
                            }
                            let screen_y = content_rect.y + (vis_start - vp_start) as u16;
                            let draw_rect = Rect::new(content_rect.x, screen_y, draw_cols, draw_rows);
                            let pending_key = format!(
                                "html:{}:{}x{}:{:?}#crop:0,{},{},{}",
                                g.node_id.0,
                                content_w,
                                g.line_count,
                                caps.protocol,
                                crop_top,
                                draw_cols,
                                draw_rows,
                            );
                            pixel_placements.push((draw_rect, pending_key, payload, caps.protocol));
                        }
                    }
                }
            }
        }

        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(theme.bg_elevated).fg(theme.fg_primary))
                .scroll((self.session.scroll_y, 0)),
            content_rect,
        );

        if !pixel_placements.is_empty() {
            let buf = frame.buffer_mut();
            let buf_area = *buf.area();
            for (draw_rect, pending_key, payload, protocol) in pixel_placements {
                let x_end = draw_rect
                    .right()
                    .min(content_rect.right())
                    .min(buf_area.right());
                let y_end = draw_rect
                    .bottom()
                    .min(content_rect.bottom())
                    .min(buf_area.bottom());
                for y in draw_rect.y..y_end {
                    for x in draw_rect.x..x_end {
                        let cell = &mut buf[(x, y)];
                        cell.set_symbol(" ");
                        cell.set_skip(true);
                    }
                }
                ui.pending_pixel_graphics.push(PendingPixelGraphic {
                    cache_key: pending_key,
                    rect: draw_rect,
                    protocol,
                    payload: Some(payload),
                });
            }
        }

        if show_scrollbar {
            let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, content_h);
            let mut sb_state = ScrollbarState::new(max_scroll.saturating_add(1) as usize)
                .position(self.session.scroll_y as usize);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(theme.accent))
                .track_style(theme.muted_style());
            frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
        }

        if has_status_row {
            let status_y = inner.bottom().saturating_sub(1);
            let status_rect = Rect::new(inner.x, status_y, inner.width, 1);
            let left_status = if let Some(ref st) = self.session.last_status {
                st.clone()
            } else if self.session.editing_input.is_some() {
                "Editing input — Type text · Backspace delete · Enter finish".to_string()
            } else if control_count > 0 {
                let focus_info = match self.session.focused_control {
                    Some(i) => format!("Control {}/{}", i + 1, control_count),
                    None => format!("{control_count} interactive controls (Tab / Click)"),
                };
                format!("{focus_info} · {}", self.source_label)
            } else {
                self.source_label.clone()
            };

            let scroll_info = if total_lines > visible {
                format!(" L{}/{} ", self.session.scroll_y as usize + 1, total_lines)
            } else {
                format!(" {}L ", total_lines)
            };
            let right_w = scroll_info.width();
            let left_budget = (inner.width as usize).saturating_sub(right_w + 2);
            let left_trunc = truncate_width(&left_status, left_budget);
            let pad = left_budget.saturating_sub(left_trunc.width());

            let status_line = Line::from(vec![
                Span::styled(
                    format!(" {left_trunc}"),
                    if self.session.editing_input.is_some() {
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        theme.muted_style()
                    },
                ),
                Span::raw(" ".repeat(pad)),
                Span::styled(scroll_info, theme.subtle_style()),
            ]);
            frame.render_widget(
                Paragraph::new(status_line).style(Style::default().bg(theme.bg_header)),
                status_rect,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::{
        config::AppConfig,
        models::{NoteDetail, NoteSummary},
        vault::parser::parse_markdown,
    };

    fn render_overlay_text(
        overlay: &mut HtmlPreviewOverlay,
        ui: &mut UiState,
        width: u16,
        height: u16,
    ) -> (String, HitMap) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hitmap = HitMap::new();
        terminal
            .draw(|frame| {
                ui.pending_pixel_graphics.clear();
                hitmap.clear();
                overlay.draw(
                    frame,
                    frame.area(),
                    TerminalGraphicsCaps::default(),
                    ui,
                    &mut hitmap,
                );
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        (text, hitmap)
    }

    #[test]
    fn test_html_preview_button_onclick_click_at_raw_toggle_and_serve_action() {
        let html = r#"<!DOCTYPE html>
<html>
<head><title>Interactive Counter</title></head>
<body>
  <h1>Counter Demo</h1>
  <p id="status">Count: 0</p>
  <button id="inc" onclick="window.__c=(window.__c||0)+1; document.getElementById('status').textContent='Count: '+window.__c;">Increment</button>
  <input type="text" name="note" value="hi" />
  <details><summary>Details</summary><p>Hidden Secret</p></details>
</body>
</html>"#;

        let cfg = AppConfig::default();
        let mut ui = UiState::from_config(&cfg);
        let mut overlay = HtmlPreviewOverlay::new("Counter", "demo.html", html.to_string());
        assert!(
            overlay.session.js_active,
            "QuickJS should activate for onclick"
        );

        // Initial render populates last_content_rect & controls
        let (buf0, hitmap0) = render_overlay_text(&mut overlay, &mut ui, 100, 28);
        assert!(buf0.contains("Count: 0"));
        assert!(buf0.contains("[ Increment ]"));
        assert!(!buf0.contains("Hidden Secret"));
        assert!(hitmap0
            .region_of(HitTarget::Button(ButtonId::ConfirmCancel))
            .is_some());
        assert!(hitmap0
            .region_of(HitTarget::Button(ButtonId::HtmlToggleRaw))
            .is_some());
        assert!(hitmap0
            .region_of(HitTarget::Button(ButtonId::HtmlServeBrowser))
            .is_some());

        // 1. Activate <button onclick> via Tab + Enter
        assert_eq!(overlay.handle(&Action::Tab), OverlayOutcome::Handled);
        assert_eq!(overlay.session.focused_control, Some(0));
        assert_eq!(overlay.handle(&Action::Activate), OverlayOutcome::Handled);
        let (buf1, _) = render_overlay_text(&mut overlay, &mut ui, 100, 28);
        assert!(
            buf1.contains("Count: 1"),
            "button onclick should update DOM to Count: 1"
        );

        // 2. Activate <button onclick> via screen-space mouse click (`click_at`)
        let btn_ctrl = {
            let theme = html_theme_from_ui(&ui.theme);
            let r = overlay
                .session
                .render(overlay.last_content_rect.width, &theme, false);
            r.controls[0].clone()
        };
        let click_col = overlay.last_content_rect.x + btn_ctrl.col_start + 1;
        let click_row = overlay.last_content_rect.y + btn_ctrl.line as u16;
        assert_eq!(
            overlay.handle(&Action::MouseClick {
                col: click_col,
                row: click_row,
            }),
            OverlayOutcome::Handled
        );
        let (buf2, _) = render_overlay_text(&mut overlay, &mut ui, 100, 28);
        assert!(
            buf2.contains("Count: 2"),
            "click_at should invoke onclick -> Count: 2"
        );

        // 3. Text input focus + typing + Enter finishes editing
        assert_eq!(overlay.handle(&Action::Tab), OverlayOutcome::Handled);
        assert_eq!(overlay.session.focused_control, Some(1));
        assert_eq!(overlay.handle(&Action::Input(' ')), OverlayOutcome::Handled);
        assert!(overlay.is_editing_input());
        overlay.handle(&Action::Input('!'));
        overlay.handle(&Action::Backspace);
        overlay.handle(&Action::Input('2'));
        assert_eq!(overlay.handle(&Action::Activate), OverlayOutcome::Handled);
        assert!(!overlay.is_editing_input());
        let (buf_in, _) = render_overlay_text(&mut overlay, &mut ui, 100, 28);
        assert!(buf_in.contains("hi2"));

        // 4. Tab to <details> and expand via Space
        overlay.handle(&Action::Tab);
        overlay.handle(&Action::Input(' '));
        let (buf_det, _) = render_overlay_text(&mut overlay, &mut ui, 100, 28);
        assert!(buf_det.contains("Hidden Secret"));

        // 5. `m` toggles Rendered <-> Raw HTML
        assert!(!overlay.session.raw_mode);
        assert_eq!(overlay.handle(&Action::Input('m')), OverlayOutcome::Handled);
        assert!(overlay.session.raw_mode);
        let (buf_raw, _) = render_overlay_text(&mut overlay, &mut ui, 100, 28);
        assert!(buf_raw.contains("<!DOCTYPE html>"));
        assert!(buf_raw.contains("<button"));
        assert_eq!(overlay.handle(&Action::Input('m')), OverlayOutcome::Handled);
        assert!(!overlay.session.raw_mode);

        // 6. `o` dispatches native browser serving action
        match overlay.handle(&Action::Input('o')) {
            OverlayOutcome::Dispatch(Action::ServeHtmlInBrowser {
                title,
                html: payload,
            }) => {
                assert_eq!(title, "Counter");
                assert!(payload.contains("Counter Demo"));
            }
            other => panic!("expected ServeHtmlInBrowser dispatch, got {other:?}"),
        }

        // 7. Click outside modal closes overlay; `q` and `Esc` close overlay
        assert_eq!(
            overlay.handle(&Action::MouseClick { col: 0, row: 0 }),
            OverlayOutcome::Close
        );
        assert_eq!(overlay.handle(&Action::Input('q')), OverlayOutcome::Close);
        assert_eq!(overlay.handle(&Action::CloseOverlay), OverlayOutcome::Close);
    }

    #[test]
    fn test_static_html_keeps_js_inactive_and_scrolls_with_scrollbar() {
        let mut body = String::from("<h1>Long Static Doc</h1>");
        for i in 1..=60 {
            body.push_str(&format!(
                "<p>Paragraph line number {i} for scrolling check.</p>"
            ));
        }
        let cfg = AppConfig::default();
        let mut ui = UiState::from_config(&cfg);
        let mut overlay = HtmlPreviewOverlay::new("Static Doc", "static.html", body);
        assert!(
            !overlay.session.js_active,
            "static HTML must keep QuickJS inactive (0 MB)"
        );

        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        let mut hitmap = HitMap::new();
        let caps = TerminalGraphicsCaps::default();
        terminal
            .draw(|f| overlay.draw(f, f.area(), caps, &mut ui, &mut hitmap))
            .unwrap();
        assert!(overlay.last_total_lines > overlay.last_visible_lines);

        // Scrollbar painted in right-most inner column of modal
        let sb_x = overlay.last_modal_rect.right() - 2;
        let buf = terminal.backend().buffer();
        assert!(
            (overlay.last_content_rect.y..overlay.last_content_rect.bottom()).any(|y| !buf
                [(sb_x, y)]
                .symbol()
                .trim()
                .is_empty())
        );

        overlay.handle(&Action::Input('j'));
        assert_eq!(overlay.session.scroll_y, 1);
        overlay.handle(&Action::MouseScrollDown { col: 40, row: 10 });
        assert_eq!(overlay.session.scroll_y, 4);
        overlay.handle(&Action::Input('k'));
        assert_eq!(overlay.session.scroll_y, 3);
        overlay.handle(&Action::Input('G'));
        terminal
            .draw(|f| overlay.draw(f, f.area(), caps, &mut ui, &mut hitmap))
            .unwrap();
        let expected_max = (overlay.last_total_lines - overlay.last_visible_lines) as u16;
        assert_eq!(overlay.session.scroll_y, expected_max);
    }

    #[test]
    fn test_html_preview_inline_svg_renders_halfblocks_and_preserves_button_click_coords() {
        let html = r##"<!DOCTYPE html>
<html>
<body>
  <p id="msg">Before SVG</p>
  <svg width="100" height="40"><rect width="100" height="40" fill="#e11d48"/></svg>
  <button id="go" onclick="document.getElementById('msg').textContent='Clicked Below SVG'">Action</button>
</body>
</html>"##;

        let cfg = AppConfig::default();
        let mut ui = UiState::from_config(&cfg);
        let mut overlay = HtmlPreviewOverlay::new("SVG + Button", "chart.html", html.to_string());

        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        let mut hitmap = HitMap::new();
        let hb_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Halfblock,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        terminal
            .draw(|frame| {
                ui.pending_pixel_graphics.clear();
                hitmap.clear();
                overlay.draw(frame, frame.area(), hb_caps, &mut ui, &mut hitmap);
            })
            .unwrap();

        let (graphic_slot, btn_ctrl) = {
            let html_theme = html_theme_from_ui(&ui.theme);
            let rendered = overlay.session.render(
                overlay.last_content_rect.width,
                &html_theme,
                ui.show_inline_hrefs,
            );
            assert_eq!(rendered.graphics.len(), 1, "expected 1 inline SVG graphic");
            assert_eq!(rendered.controls.len(), 1, "expected 1 button control below SVG");
            (rendered.graphics[0].clone(), rendered.controls[0].clone())
        };
        assert!(
            btn_ctrl.line >= graphic_slot.line_start + graphic_slot.line_count,
            "button must sit below reserved SVG slot"
        );

        // Verify Halfblock `▀` cells are rendered inside the reserved slot
        let buf = terminal.backend().buffer();
        let mut halfblock_cells = 0usize;
        for row_off in graphic_slot.line_start..graphic_slot.line_start + graphic_slot.line_count {
            let y = overlay.last_content_rect.y + row_off as u16;
            for x in overlay.last_content_rect.x..overlay.last_content_rect.right() {
                if buf[(x, y)].symbol() == "▀" {
                    halfblock_cells += 1;
                }
            }
        }
        assert!(
            halfblock_cells > 0,
            "expected Halfblock `▀` cells in reserved SVG slot lines {}..{}",
            graphic_slot.line_start,
            graphic_slot.line_start + graphic_slot.line_count
        );

        // Verify clicking the `<button>` below the SVG hits its exact coordinates
        let click_col = overlay.last_content_rect.x + btn_ctrl.col_start + 1;
        let click_row = overlay.last_content_rect.y + btn_ctrl.line as u16;
        assert_eq!(
            overlay.handle(&Action::MouseClick {
                col: click_col,
                row: click_row,
            }),
            OverlayOutcome::Handled
        );
        let (after_text, _) = render_overlay_text(&mut overlay, &mut ui, 90, 30);
        assert!(
            after_text.contains("Clicked Below SVG"),
            "clicking button below inline SVG must update DOM text"
        );

        // Verify pixel protocol (Sixel) pushes pending_pixel_graphics and marks covered cells skip=true
        let sixel_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let mut covered_cell_skipped = false;
        terminal
            .draw(|frame| {
                ui.pending_pixel_graphics.clear();
                hitmap.clear();
                overlay.draw(frame, frame.area(), sixel_caps, &mut ui, &mut hitmap);
                if let Some(pg) = ui.pending_pixel_graphics.first() {
                    covered_cell_skipped = frame.buffer_mut()[(pg.rect.x, pg.rect.y)].skip;
                }
            })
            .unwrap();
        assert_eq!(ui.pending_pixel_graphics.len(), 1);
        let pg = &ui.pending_pixel_graphics[0];
        assert!(pg.cache_key.starts_with("html:"));
        assert!(pg
            .payload
            .as_deref()
            .is_some_and(|p| p.starts_with("\x1bP0;1;q")));
        assert!(
            covered_cell_skipped,
            "covered pixel cells must be marked set_skip(true)"
        );
    }

    #[test]
    fn test_looks_like_embedded_html_detects_svg_and_canvas() {
        assert!(looks_like_embedded_html(
            "<svg width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\"/></svg>"
        ));
        assert!(looks_like_embedded_html(
            "<canvas id=\"chart\" width=\"200\" height=\"100\"></canvas>"
        ));
        assert!(!looks_like_embedded_html("Plain markdown paragraph"));
    }

    #[test]
    fn test_extract_note_html_code_blocks_raw_html_and_linked_files() {
        let tmp = tempfile::tempdir().unwrap();
        let html_file = tmp.path().join("widget.html");
        fs::write(&html_file, "<h1>Vault Widget</h1><button>Go</button>").unwrap();

        let md = r#"# HTML Note

Here is a fenced HTML block:

```html
<div class="box"><button onclick="alert('hi')">Tap</button></div>
```

And embedded raw HTML:

<details><summary>Expand</summary><p>Inside</p></details>

And a linked HTML page: [Widget](widget.html)
"#;
        let parsed = parse_markdown("HTML Note.md", "HTML Note", md);
        let detail = NoteDetail {
            summary: NoteSummary {
                rel_path: "HTML Note.md".into(),
                title: "HTML Note".into(),
                ..Default::default()
            },
            raw_markdown: md.to_string(),
            parsed,
            backlinks: Vec::new(),
            outgoing_links: Vec::new(),
            images: Vec::new(),
        };

        assert!(note_has_html(&detail));
        let candidates = extract_note_html_candidates(&detail, Some(tmp.path()));
        assert_eq!(candidates.len(), 3);
        assert!(candidates[0]
            .html
            .contains("<button onclick=\"alert('hi')\">Tap</button>"));
        assert!(candidates[1]
            .html
            .contains("<details><summary>Expand</summary>"));
        assert!(candidates[2].html.contains("Vault Widget"));
    }
}
