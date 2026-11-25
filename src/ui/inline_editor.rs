//! In-Place Inline Markdown Note Editor directly inside `note_view`.
//!
//! Backed by `crate::ui::composer::MultiLineComposer`, featuring:
//! - Obsidian-style **Live Preview** (default) & **Source** mode toggleable via `Ctrl+M` or `[Live]`/`[Src]` pill:
//!   - Inactive logical lines render rich Markdown (styled headings hiding `# `, `☐`/`☑` tasks,
//!     `• ` bullets, `┃ ` blockquotes/callouts, `──────────` dividers, muted code fences + tinted code,
//!     muted YAML frontmatter, and hidden inline delimiters for `**bold**`, `*italic*`, `` `code` ``,
//!     `~~strike~~`, `[[wikilink]]`, `#tag`)
//!   - Active cursor logical line reveals raw syntax-highlighted Markdown with exact ASCII + Korean/CJK
//!     hardware cursor `(cx, cy)` across soft-wrapped visual lines
//! - Compact 1-line inline edit header bar with live status (`✓ Live Saved` / `● Editing...` / `⚠ Error`),
//!   clickable pills (`[Live]`/`[Src]`, `[✓ Done]`, `[Ctrl+S Save]`, `[Ext $EDITOR]`), and `Ln X, Col Y`
//! - Mouse click-to-place cursor (accurate in both LivePreview and Source layouts), mouse wheel scrolling
//!   + vertical scrollbar, UTF-8/Korean input, Tab (2 spaces), clipboard paste, and Undo/Redo
//! - 250ms debounced auto-save state tracking (`dirty`, `dirty_since`).

use std::time::{Duration, Instant};

use ratatui::{
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    models::CalloutKind,
    ui::{
        action::Action,
        composer::{truncate_display, MultiLineComposer},
        hitmap::{ButtonId, HitMap, HitTarget},
        theme::Theme,
    },
    vault::fs::content_sha256_bytes,
};

/// Debounce window before automatically flushing inline edits to disk + SQLite.
pub const INLINE_AUTOSAVE_DEBOUNCE_MS: u64 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorMode {
    #[default]
    LivePreview,
    Source,
}

impl EditorMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::LivePreview => Self::Source,
            Self::Source => Self::LivePreview,
        }
    }

    pub fn pill_label(self) -> &'static str {
        match self {
            Self::LivePreview => "[Live]",
            Self::Source => "[Src]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalConflict {
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineSaveStatus {
    Saved,
    Editing,
    Conflict(ExternalConflict),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineEditEvent {
    TextChanged,
    CursorOrScrollChanged,
    Ignored,
}

#[derive(Debug, Clone)]
pub struct InlineNoteEditor {
    pub note_path: String,
    pub note_title: String,
    pub base_content_hash: [u8; 32],
    pub external_conflict: Option<ExternalConflict>,
    pub mode: EditorMode,
    pub composer: MultiLineComposer,
    pub scroll_y: u16,
    pub follow_cursor: bool,
    pub last_viewport_height: u16,
    pub last_editor_area: Rect,
    pub last_content_width: u16,
    pub dirty: bool,
    pub has_edited: bool,
    pub dirty_since: Option<Instant>,
    pub save_status: InlineSaveStatus,
}

impl InlineNoteEditor {
    pub fn new(
        note_path: impl Into<String>,
        note_title: impl Into<String>,
        raw_markdown: &str,
        initial_scroll_y: u16,
    ) -> Self {
        let base_content_hash = content_sha256_bytes(raw_markdown.as_bytes());
        Self::with_base_hash(
            note_path,
            note_title,
            raw_markdown,
            base_content_hash,
            initial_scroll_y,
        )
    }

    pub fn with_base_hash(
        note_path: impl Into<String>,
        note_title: impl Into<String>,
        raw_markdown: &str,
        base_content_hash: [u8; 32],
        initial_scroll_y: u16,
    ) -> Self {
        let mut composer = MultiLineComposer::new();
        composer.load_text_preserving_line_endings(raw_markdown);
        if initial_scroll_y > 0 {
            composer.set_cursor_line(initial_scroll_y as usize);
        }
        Self {
            note_path: note_path.into(),
            note_title: note_title.into(),
            base_content_hash,
            external_conflict: None,
            mode: EditorMode::LivePreview,
            composer,
            scroll_y: initial_scroll_y,
            follow_cursor: initial_scroll_y == 0,
            last_viewport_height: 20,
            last_editor_area: Rect::default(),
            last_content_width: 72,
            dirty: false,
            has_edited: false,
            dirty_since: None,
            save_status: InlineSaveStatus::Saved,
        }
    }

    pub fn toggle_mode(&mut self) -> EditorMode {
        self.mode = self.mode.toggle();
        self.follow_cursor = true;
        self.mode
    }

    /// Cleanly reloads editor text from disk after an external modification (when buffer is clean).
    pub fn reload_from_disk(&mut self, raw_markdown: &str, disk_hash: [u8; 32]) {
        let (prev_line, _) = self.cursor_line_col();
        let prev_scroll = self.scroll_y;
        self.base_content_hash = disk_hash;
        self.external_conflict = None;
        self.composer
            .load_text_preserving_line_endings(raw_markdown);
        if prev_line > 1 {
            self.composer.set_cursor_line(prev_line - 1);
        }
        self.scroll_y = prev_scroll;
        self.dirty = false;
        self.dirty_since = None;
        self.save_status = InlineSaveStatus::Saved;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        self.has_edited = true;
        if let Some(conflict) = self.external_conflict.clone() {
            // Keep autosave paused while an external conflict is active.
            self.dirty_since = None;
            self.save_status = InlineSaveStatus::Conflict(conflict);
        } else {
            self.dirty_since = Some(Instant::now());
            self.save_status = InlineSaveStatus::Editing;
        }
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
        self.dirty_since = None;
        self.external_conflict = None;
        self.save_status = InlineSaveStatus::Saved;
    }

    pub fn mark_saved_with_hash(&mut self, hash: [u8; 32]) {
        self.base_content_hash = hash;
        self.mark_saved();
    }

    pub fn mark_conflict(&mut self, conflict: ExternalConflict) {
        self.external_conflict = Some(conflict.clone());
        self.dirty_since = None;
        self.save_status = InlineSaveStatus::Conflict(conflict);
    }

    pub fn mark_error(&mut self, err: impl Into<String>) {
        self.save_status = InlineSaveStatus::Error(err.into());
    }

    pub fn is_autosave_due(&self) -> bool {
        self.dirty
            && self.external_conflict.is_none()
            && self
                .dirty_since
                .is_some_and(|t| t.elapsed() >= Duration::from_millis(INLINE_AUTOSAVE_DEBOUNCE_MS))
    }

    /// Serializes the edited buffer with its per-line endings.
    pub fn disk_markdown(&self) -> String {
        self.composer.text_with_line_endings()
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        self.composer.cursor_line_col()
    }

    /// Places the cursor on a 0-based logical line at the UTF-8 byte corresponding to
    /// `display_col` (respecting LivePreview delimiter hiding when in `LivePreview` mode).
    pub fn place_cursor_at_logical_line_col(&mut self, zero_based_line: usize, display_col: usize) {
        let text = self.composer.text();
        let ctx = build_line_context(text);
        if ctx.is_empty() {
            self.composer.set_cursor(0);
            self.follow_cursor = true;
            return;
        }
        let line_idx = zero_based_line.min(ctx.len().saturating_sub(1));
        let info = &ctx[line_idx];
        let new_pos = if display_col == 0 {
            info.start
        } else if self.mode == EditorMode::LivePreview {
            let dummy_theme = Theme::no_color();
            let line_text = text.get(info.start..info.end).unwrap_or("");
            let segs = build_live_segments(
                line_text,
                info.start,
                info.kind,
                self.last_content_width.max(1) as usize,
                &dummy_theme,
            );
            byte_at_live_segments(&segs, info.end, display_col)
        } else {
            byte_at_display_col(text, info.start, info.end, display_col)
        };
        self.composer.set_cursor(new_pos);
        self.follow_cursor = true;
    }

    pub fn handle_edit_action(&mut self, action: &Action) -> InlineEditEvent {
        match action {
            Action::ToggleEditorMode => {
                self.toggle_mode();
                InlineEditEvent::CursorOrScrollChanged
            }
            Action::Input(ch) => {
                self.composer.insert_char(*ch);
                self.follow_cursor = true;
                self.mark_dirty();
                InlineEditEvent::TextChanged
            }
            Action::Activate | Action::Newline => {
                self.composer.newline();
                self.follow_cursor = true;
                self.mark_dirty();
                InlineEditEvent::TextChanged
            }
            Action::Backspace => {
                let before = self.composer.text().len();
                let before_cursor = self.composer.cursor();
                self.composer.backspace();
                self.follow_cursor = true;
                if self.composer.text().len() != before || self.composer.cursor() != before_cursor {
                    self.mark_dirty();
                    InlineEditEvent::TextChanged
                } else {
                    InlineEditEvent::CursorOrScrollChanged
                }
            }
            Action::Delete => {
                let before = self.composer.text().len();
                self.composer.delete_forward();
                self.follow_cursor = true;
                if self.composer.text().len() != before {
                    self.mark_dirty();
                    InlineEditEvent::TextChanged
                } else {
                    InlineEditEvent::CursorOrScrollChanged
                }
            }
            Action::Tab => {
                self.composer.insert_str("  ");
                self.follow_cursor = true;
                self.mark_dirty();
                InlineEditEvent::TextChanged
            }
            Action::Paste(text) => {
                if !text.is_empty() {
                    self.composer.insert_str(text);
                    self.follow_cursor = true;
                    self.mark_dirty();
                    InlineEditEvent::TextChanged
                } else {
                    InlineEditEvent::CursorOrScrollChanged
                }
            }
            Action::Undo => {
                self.follow_cursor = true;
                if self.composer.undo() {
                    self.mark_dirty();
                    InlineEditEvent::TextChanged
                } else {
                    InlineEditEvent::CursorOrScrollChanged
                }
            }
            Action::Redo => {
                self.follow_cursor = true;
                if self.composer.redo() {
                    self.mark_dirty();
                    InlineEditEvent::TextChanged
                } else {
                    InlineEditEvent::CursorOrScrollChanged
                }
            }
            Action::CursorLeft | Action::MoveLeft => {
                self.composer.move_left(false);
                self.follow_cursor = true;
                InlineEditEvent::CursorOrScrollChanged
            }
            Action::CursorRight | Action::MoveRight => {
                self.composer.move_right(false);
                self.follow_cursor = true;
                InlineEditEvent::CursorOrScrollChanged
            }
            Action::CursorUp | Action::MoveUp => {
                self.composer.move_up(false);
                self.follow_cursor = true;
                InlineEditEvent::CursorOrScrollChanged
            }
            Action::CursorDown | Action::MoveDown => {
                self.composer.move_down(false);
                self.follow_cursor = true;
                InlineEditEvent::CursorOrScrollChanged
            }
            Action::CursorHome => {
                self.composer.move_home(false);
                self.follow_cursor = true;
                InlineEditEvent::CursorOrScrollChanged
            }
            Action::CursorEnd => {
                self.composer.move_end(false);
                self.follow_cursor = true;
                InlineEditEvent::CursorOrScrollChanged
            }
            Action::PageUp => {
                let steps = self.last_viewport_height.saturating_sub(2).max(5);
                for _ in 0..steps {
                    self.composer.move_up(false);
                }
                self.follow_cursor = true;
                InlineEditEvent::CursorOrScrollChanged
            }
            Action::PageDown => {
                let steps = self.last_viewport_height.saturating_sub(2).max(5);
                for _ in 0..steps {
                    self.composer.move_down(false);
                }
                self.follow_cursor = true;
                InlineEditEvent::CursorOrScrollChanged
            }
            Action::MouseScrollUp { .. } => {
                self.follow_cursor = false;
                self.scroll_y = self.scroll_y.saturating_sub(3);
                InlineEditEvent::CursorOrScrollChanged
            }
            Action::MouseScrollDown { .. } => {
                self.follow_cursor = false;
                self.scroll_y = self.scroll_y.saturating_add(3);
                InlineEditEvent::CursorOrScrollChanged
            }
            _ => InlineEditEvent::Ignored,
        }
    }

    /// Places the cursor at the UTF-8 byte position corresponding to terminal screen `(col, row)`,
    /// handling both LivePreview visual rows (with hidden Markdown delimiters) and Source rows.
    pub fn place_cursor_at_screen_pos(&mut self, col: u16, row: u16) -> bool {
        if !self.last_editor_area.contains(Position::new(col, row)) {
            return false;
        }
        let rel_row = (row - self.last_editor_area.y) as usize;
        let target_col = (col - self.last_editor_area.x) as usize;
        let width = self.last_content_width.max(1) as usize;
        let text = self.composer.text();
        let dummy_theme = Theme::no_color();
        let layout = build_visual_layout(
            text,
            width,
            self.composer.cursor(),
            self.mode,
            self.composer.selection_range(),
            &dummy_theme,
        );

        let visual_idx = (self.scroll_y as usize).saturating_add(rel_row);
        let new_pos = if visual_idx >= layout.rows.len() {
            text.len()
        } else {
            let vrow = &layout.rows[visual_idx];
            if let Some((start, end)) = vrow.raw_slice {
                byte_at_display_col(text, start, end, target_col)
            } else {
                byte_at_live_segments(&vrow.live_segs, vrow.raw_line_end, target_col)
            }
        };
        self.composer.set_cursor(new_pos);
        self.follow_cursor = true;
        true
    }

    /// Renders the 1-line inline edit header bar + LivePreview/Source Markdown directly
    /// inside `note_view`'s `content_area`, returning the live terminal cursor `(x, y)` if visible.
    pub fn draw(
        &mut self,
        frame: &mut Frame,
        content_area: Rect,
        theme: &Theme,
        hitmap: &mut HitMap,
    ) -> Option<(u16, u16)> {
        if content_area.width < 10 || content_area.height < 2 {
            return None;
        }

        // 1. Compact 1-line inline edit header bar at top of `content_area`
        let header_rect = Rect::new(content_area.x, content_area.y, content_area.width, 1);
        self.draw_header_bar(frame, header_rect, theme, hitmap);

        // 2. Editable Markdown body below header
        let body_y = content_area.y.saturating_add(1);
        let body_h = content_area.height.saturating_sub(1);
        if body_h == 0 {
            return None;
        }
        self.last_viewport_height = body_h;

        let cursor = self.composer.cursor();
        let selection = self.composer.selection_range();

        // Probe line count to decide whether to reserve 1 col for the right scrollbar
        let probe_w = content_area.width.saturating_sub(1).max(1) as usize;
        let probe_layout = build_visual_layout(
            self.composer.text(),
            probe_w,
            cursor,
            self.mode,
            selection,
            theme,
        );
        let show_scrollbar = probe_layout.rows.len() > body_h as usize && content_area.width > 3;
        let content_w = if show_scrollbar {
            content_area.width.saturating_sub(1)
        } else {
            content_area.width
        };
        self.last_content_width = content_w;
        self.composer.last_inner_width = content_w;
        let text = self.composer.text();

        let layout = if content_w as usize == probe_w {
            probe_layout
        } else {
            build_visual_layout(
                text,
                content_w.max(1) as usize,
                cursor,
                self.mode,
                selection,
                theme,
            )
        };

        let total_rows = layout.rows.len();
        let cursor_row = layout.cursor_row;
        let cursor_col = layout.cursor_col;
        let view_h = body_h as usize;
        let max_scroll = total_rows.saturating_sub(view_h);

        let mut offset = self.scroll_y as usize;
        if self.follow_cursor {
            if cursor_row < offset {
                offset = cursor_row;
            } else if cursor_row >= offset.saturating_add(view_h) {
                offset = cursor_row.saturating_add(1).saturating_sub(view_h);
            }
        }
        offset = offset.min(max_scroll);
        self.scroll_y = offset.min(u16::MAX as usize) as u16;

        let editor_area = Rect::new(content_area.x, body_y, content_w, body_h);
        self.last_editor_area = editor_area;
        hitmap.add(editor_area, HitTarget::InlineEditorBody);

        let surface_bg = match self.mode {
            EditorMode::LivePreview => theme.bg_canvas,
            EditorMode::Source => theme.bg_composer,
        };
        let surface_style = Style::default().bg(surface_bg).fg(theme.fg_primary);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(view_h);

        if text.is_empty() {
            lines.push(Line::from(Span::styled(
                truncate_display(
                    "Type Markdown here (# Heading, - [ ] Task, [[Wikilink]], #tag, ```code```)...",
                    content_w as usize,
                ),
                theme.muted_style().add_modifier(Modifier::ITALIC),
            )));
        } else {
            for vrow in layout.rows.iter().skip(offset).take(view_h) {
                if let Some((start, end)) = vrow.raw_slice {
                    lines.push(highlight_visual_slice(
                        text,
                        start,
                        end,
                        &layout.ctx,
                        selection,
                        theme,
                    ));
                } else {
                    lines.push(render_live_visual_row(vrow, theme));
                }
            }
        }

        frame.render_widget(Paragraph::new(lines).style(surface_style), editor_area);

        if show_scrollbar {
            let sb_area = Rect::new(content_area.right().saturating_sub(1), body_y, 1, body_h);
            let mut sb_state = ScrollbarState::new(max_scroll.saturating_add(1))
                .position(offset)
                .viewport_content_length(view_h);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(theme.accent))
                .track_style(theme.muted_style());
            frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
        }

        if cursor_row >= offset && cursor_row < offset.saturating_add(view_h) {
            let cy = editor_area
                .y
                .saturating_add((cursor_row - offset) as u16)
                .min(editor_area.bottom().saturating_sub(1));
            let cx = editor_area
                .x
                .saturating_add(cursor_col as u16)
                .min(editor_area.right().saturating_sub(1));
            frame.set_cursor_position((cx, cy));
            Some((cx, cy))
        } else {
            None
        }
    }

    fn draw_header_bar(&self, frame: &mut Frame, area: Rect, theme: &Theme, hitmap: &mut HitMap) {
        let header_bg = theme.bg_header;
        let (status_full, status_short, status_color) = match &self.save_status {
            InlineSaveStatus::Saved => ("✓ Live Saved", "✓ Saved", theme.success),
            InlineSaveStatus::Editing => ("● Editing...", "● Editing", theme.warning),
            InlineSaveStatus::Conflict(ExternalConflict::Modified) => {
                ("⚠ Conflict: modified on disk", "⚠ Conflict", theme.error)
            }
            InlineSaveStatus::Conflict(ExternalConflict::Deleted) => {
                ("⚠ Conflict: deleted/moved", "⚠ Deleted", theme.error)
            }
            InlineSaveStatus::Error(_) => ("⚠ Error", "⚠ Err", theme.error),
        };

        let (ln, col) = self.cursor_line_col();
        let ln_col_label = format!("Ln {ln}, Col {col}");
        let mode_pill = self.mode.pill_label();
        let done_pill = "[✓ Done]";
        let save_pill = "[Ctrl+S Save]";
        let ext_pill = "[Ext $EDITOR]";

        let pills_w = (mode_pill.width()
            + 1
            + done_pill.width()
            + 1
            + save_pill.width()
            + 1
            + ext_pill.width()) as u16;
        let ln_w = ln_col_label.width() as u16;
        let total_w = area.width;

        // Prioritize clickable pills + full status; show `Ln X, Col Y` when width comfortably allows both.
        let show_pills = total_w >= pills_w + 10;
        let show_ln_col = show_pills && total_w >= pills_w + ln_w + 25;
        let right_w = if show_ln_col {
            pills_w + 1 + ln_w
        } else if show_pills {
            pills_w
        } else {
            0
        };

        let left_budget = total_w.saturating_sub(right_w + 1) as usize;
        let (prefix_str, status_str) =
            if "✎ EDITING · ".width() + status_full.width() <= left_budget {
                ("✎ EDITING · ", status_full)
            } else if "✎ EDITING · ".width() + status_short.width() <= left_budget {
                ("✎ EDITING · ", status_short)
            } else if "✎ EDIT · ".width() + status_short.width() <= left_budget {
                ("✎ EDIT · ", status_short)
            } else {
                ("✎ ", status_short)
            };

        let left_line = Line::from(vec![
            Span::styled(
                prefix_str,
                Style::default()
                    .fg(theme.accent)
                    .bg(header_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                crate::ui::truncate_width(
                    status_str,
                    left_budget.saturating_sub(prefix_str.width()),
                ),
                Style::default()
                    .fg(status_color)
                    .bg(header_bg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);

        // Paint full header bar background + left badge
        frame.render_widget(
            Paragraph::new(left_line).style(Style::default().bg(header_bg)),
            area,
        );

        if show_pills {
            let mode_style = match self.mode {
                EditorMode::LivePreview => Style::default()
                    .fg(theme.bg_canvas)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
                EditorMode::Source => Style::default()
                    .fg(theme.accent_soft)
                    .bg(theme.bg_card)
                    .add_modifier(Modifier::BOLD),
            };
            let mut px = area.right().saturating_sub(right_w);
            for (label, btn, style) in [
                (mode_pill, ButtonId::ToggleEditorMode, mode_style),
                (
                    done_pill,
                    ButtonId::FinishInlineEdit,
                    Style::default()
                        .fg(theme.bg_canvas)
                        .bg(theme.success)
                        .add_modifier(Modifier::BOLD),
                ),
                (
                    save_pill,
                    ButtonId::SaveInlineEdit,
                    Style::default()
                        .fg(theme.accent_soft)
                        .bg(header_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                (
                    ext_pill,
                    ButtonId::EditExternal,
                    Style::default().fg(theme.fg_muted).bg(header_bg),
                ),
            ] {
                let w = label.width() as u16;
                frame.render_widget(
                    Paragraph::new(Span::styled(label, style)),
                    Rect::new(px, area.y, w, 1),
                );
                hitmap.add_line(px, area.y, w, HitTarget::Button(btn));
                px = px.saturating_add(w + 1);
            }
            if show_ln_col {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        ln_col_label,
                        Style::default().fg(theme.fg_muted).bg(header_bg),
                    )),
                    Rect::new(px, area.y, ln_w, 1),
                );
            }
        }
    }
}

/// Maps a visual display column offset inside `text[start..end]` to the exact UTF-8 byte offset.
fn byte_at_display_col(text: &str, start: usize, end: usize, target_col: usize) -> usize {
    let slice = text.get(start..end).unwrap_or("");
    let mut col = 0usize;
    for (rel_idx, ch) in slice.char_indices() {
        let cw = if ch == '\t' {
            4
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        if cw > 0 && target_col < col + cw {
            return start + rel_idx;
        }
        col += cw;
    }
    end
}

#[derive(Debug, Clone)]
struct LiveSeg {
    text: String,
    style: Style,
    raw_start: usize,
    raw_end: usize,
    is_literal: bool,
}

#[derive(Debug, Clone)]
struct VisualRow {
    raw_line_end: usize,
    /// When `Some((start, end))`, this row renders raw source `text[start..end]`.
    raw_slice: Option<(usize, usize)>,
    /// When `raw_slice` is `None`, this row renders LivePreview styled segments.
    live_segs: Vec<LiveSeg>,
    /// Optional line-level background tint (e.g. fenced code body in LivePreview).
    bg_tint: Option<ratatui::style::Color>,
}

struct EditorVisualLayout {
    ctx: Vec<LogicalLineInfo>,
    rows: Vec<VisualRow>,
    cursor_row: usize,
    cursor_col: usize,
}

fn byte_at_live_segments(segs: &[LiveSeg], fallback_end: usize, target_col: usize) -> usize {
    let mut col = 0usize;
    for seg in segs {
        if seg.is_literal {
            for (rel_idx, ch) in seg.text.char_indices() {
                let cw = if ch == '\t' {
                    4
                } else {
                    UnicodeWidthChar::width(ch).unwrap_or(0)
                };
                if cw > 0 && target_col < col + cw {
                    return (seg.raw_start + rel_idx).min(seg.raw_end);
                }
                col += cw;
            }
        } else {
            let sw = seg
                .text
                .chars()
                .map(|ch| {
                    if ch == '\t' {
                        4
                    } else {
                        UnicodeWidthChar::width(ch).unwrap_or(0)
                    }
                })
                .sum::<usize>();
            if sw > 0 && target_col < col + sw {
                return if target_col == col && col == 0 {
                    seg.raw_start
                } else {
                    seg.raw_end
                };
            }
            col += sw;
        }
    }
    fallback_end
}

fn render_live_visual_row(vrow: &VisualRow, _theme: &Theme) -> Line<'static> {
    if vrow.live_segs.is_empty() {
        return match vrow.bg_tint {
            Some(bg) => Line::from(Span::styled(" ", Style::default().bg(bg))),
            None => Line::from(""),
        };
    }
    let spans: Vec<Span<'static>> = vrow
        .live_segs
        .iter()
        .map(|seg| {
            let content = if seg.text.contains('\t') {
                seg.text.replace('\t', "    ")
            } else {
                seg.text.clone()
            };
            let style = match vrow.bg_tint {
                Some(bg) if seg.style.bg.is_none() => seg.style.bg(bg),
                _ => seg.style,
            };
            Span::styled(content, style)
        })
        .collect();
    Line::from(spans)
}

fn build_visual_layout(
    text: &str,
    width: usize,
    cursor: usize,
    mode: EditorMode,
    selection: Option<(usize, usize)>,
    theme: &Theme,
) -> EditorVisualLayout {
    let width = width.max(1);
    let ctx = build_line_context(text);
    let active_line_idx = ctx
        .partition_point(|info| info.end < cursor)
        .min(ctx.len().saturating_sub(1));

    let mut rows: Vec<VisualRow> = Vec::with_capacity(ctx.len());
    let mut cursor_pos: Option<(usize, usize)> = None;

    for (line_idx, info) in ctx.iter().enumerate() {
        let sel_overlaps = selection.is_some_and(|(from, to)| from <= info.end && to >= info.start);
        let render_raw = mode == EditorMode::Source || line_idx == active_line_idx || sel_overlaps;

        if render_raw {
            let line_slice = text.get(info.start..info.end).unwrap_or("");
            let mut seg_start = info.start;
            let mut col = 0usize;

            for (rel_idx, ch) in line_slice.char_indices() {
                let abs_idx = info.start + rel_idx;
                let cw = if ch == '\t' {
                    4
                } else {
                    UnicodeWidthChar::width(ch).unwrap_or(0)
                };
                if col > 0 && col + cw > width {
                    rows.push(VisualRow {
                        raw_line_end: info.end,
                        raw_slice: Some((seg_start, abs_idx)),
                        live_segs: Vec::new(),
                        bg_tint: None,
                    });
                    seg_start = abs_idx;
                    col = 0;
                }
                if abs_idx == cursor {
                    cursor_pos = Some((rows.len(), col));
                }
                col += cw;
            }
            if cursor == info.end && line_idx == active_line_idx {
                cursor_pos = Some((rows.len(), col));
            }
            rows.push(VisualRow {
                raw_line_end: info.end,
                raw_slice: Some((seg_start, info.end)),
                live_segs: Vec::new(),
                bg_tint: None,
            });
        } else {
            let line_slice = text.get(info.start..info.end).unwrap_or("");
            let segs = build_live_segments(line_slice, info.start, info.kind, width, theme);
            let bg_tint = (info.kind == LogicalLineKind::CodeBody).then_some(theme.bg_code);
            wrap_live_segments(segs, info.end, width, bg_tint, &mut rows);
        }
    }

    let (cursor_row, cursor_col) = cursor_pos.unwrap_or((rows.len().saturating_sub(1), 0));
    EditorVisualLayout {
        ctx,
        rows,
        cursor_row,
        cursor_col,
    }
}

fn wrap_live_segments(
    segs: Vec<LiveSeg>,
    raw_line_end: usize,
    width: usize,
    bg_tint: Option<ratatui::style::Color>,
    out: &mut Vec<VisualRow>,
) {
    let width = width.max(1);
    let mut cur_segs: Vec<LiveSeg> = Vec::new();
    let mut col = 0usize;

    for seg in segs {
        if seg.text.is_empty() {
            continue;
        }
        let mut run = String::new();
        let mut run_raw_start = seg.raw_start;
        for (rel_idx, ch) in seg.text.char_indices() {
            let cw = if ch == '\t' {
                4
            } else {
                UnicodeWidthChar::width(ch).unwrap_or(0)
            };
            if col > 0 && col + cw > width {
                if !run.is_empty() {
                    let run_raw_end = if seg.is_literal {
                        (seg.raw_start + rel_idx).min(seg.raw_end)
                    } else {
                        seg.raw_end
                    };
                    cur_segs.push(LiveSeg {
                        text: std::mem::take(&mut run),
                        style: seg.style,
                        raw_start: run_raw_start,
                        raw_end: run_raw_end,
                        is_literal: seg.is_literal,
                    });
                    if seg.is_literal {
                        run_raw_start = run_raw_end;
                    }
                }
                out.push(VisualRow {
                    raw_line_end,
                    raw_slice: None,
                    live_segs: std::mem::take(&mut cur_segs),
                    bg_tint,
                });
                col = 0;
            }
            run.push(ch);
            col += cw;
        }
        if !run.is_empty() {
            cur_segs.push(LiveSeg {
                text: run,
                style: seg.style,
                raw_start: run_raw_start,
                raw_end: seg.raw_end,
                is_literal: seg.is_literal,
            });
        }
    }

    out.push(VisualRow {
        raw_line_end,
        raw_slice: None,
        live_segs: cur_segs,
        bg_tint,
    });
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    let mut rule_ch: Option<char> = None;
    let mut count = 0usize;
    for ch in trimmed.chars() {
        if ch == ' ' || ch == '\t' {
            continue;
        }
        if !matches!(ch, '-' | '*' | '_') {
            return false;
        }
        match rule_ch {
            None => {
                rule_ch = Some(ch);
                count = 1;
            }
            Some(prev) if prev == ch => count += 1,
            Some(_) => return false,
        }
    }
    count >= 3
}

/// Converts a single inactive Markdown logical line into rich styled LivePreview segments
/// with raw Markdown delimiters hidden and exact raw-byte hit mappings preserved.
fn build_live_segments(
    line: &str,
    line_start: usize,
    kind: LogicalLineKind,
    content_w: usize,
    theme: &Theme,
) -> Vec<LiveSeg> {
    let mut out = Vec::new();
    let line_end = line_start + line.len();

    match kind {
        LogicalLineKind::FrontmatterDelimiter => {
            let bar_w = content_w.clamp(3, 32);
            out.push(LiveSeg {
                text: "─".repeat(bar_w),
                style: theme.subtle_style(),
                raw_start: line_start,
                raw_end: line_end,
                is_literal: false,
            });
            return out;
        }
        LogicalLineKind::FrontmatterBody => {
            if let Some(colon) = line.find(':') {
                let key_end = colon + 1;
                out.push(LiveSeg {
                    text: line[..key_end].to_string(),
                    style: theme.muted_style().add_modifier(Modifier::BOLD),
                    raw_start: line_start,
                    raw_end: line_start + key_end,
                    is_literal: true,
                });
                if key_end < line.len() {
                    out.push(LiveSeg {
                        text: line[key_end..].to_string(),
                        style: theme.muted_style(),
                        raw_start: line_start + key_end,
                        raw_end: line_end,
                        is_literal: true,
                    });
                }
            } else {
                out.push(LiveSeg {
                    text: line.to_string(),
                    style: theme.muted_style(),
                    raw_start: line_start,
                    raw_end: line_end,
                    is_literal: true,
                });
            }
            return out;
        }
        LogicalLineKind::CodeFenceOpen => {
            let trimmed = line.trim();
            let fence_ch = trimmed.chars().next().unwrap_or('`');
            let fence_len = trimmed.chars().take_while(|&c| c == fence_ch).count();
            let lang = trimmed[fence_len..].trim();
            let marker = if lang.is_empty() {
                "╭─ code".to_string()
            } else {
                format!("╭─ {lang}")
            };
            out.push(LiveSeg {
                text: marker,
                style: Style::default()
                    .fg(theme.code_fg)
                    .add_modifier(Modifier::DIM),
                raw_start: line_start,
                raw_end: line_end,
                is_literal: false,
            });
            return out;
        }
        LogicalLineKind::CodeFenceClose => {
            out.push(LiveSeg {
                text: "╰───".to_string(),
                style: Style::default()
                    .fg(theme.code_fg)
                    .add_modifier(Modifier::DIM),
                raw_start: line_start,
                raw_end: line_end,
                is_literal: false,
            });
            return out;
        }
        LogicalLineKind::CodeBody => {
            if !line.is_empty() {
                out.push(LiveSeg {
                    text: line.to_string(),
                    style: Style::default().fg(theme.code_fg).bg(theme.bg_code),
                    raw_start: line_start,
                    raw_end: line_end,
                    is_literal: true,
                });
            }
            return out;
        }
        LogicalLineKind::Normal => {}
    }

    let trimmed_start = line.len() - line.trim_start().len();
    let indent = &line[..trimmed_start];
    let rest = &line[trimmed_start..];
    let trimmed = rest.trim_end();

    if trimmed.is_empty() {
        return out;
    }

    // 1. Horizontal rule (`---`, `***`, `___`)
    if is_horizontal_rule(trimmed) {
        let bar_w = content_w.clamp(10, 48);
        out.push(LiveSeg {
            text: "─".repeat(bar_w),
            style: Style::default().fg(theme.divider),
            raw_start: line_start,
            raw_end: line_end,
            is_literal: false,
        });
        return out;
    }

    // 2. ATX Headings (`#`–`###### `) -> hide leading `# ` markers when heading has text
    let hashes = rest.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes)
        && rest
            .as_bytes()
            .get(hashes)
            .is_some_and(|&b| b == b' ' || b == b'\t')
    {
        let after_hashes = &rest[hashes..];
        let ws = after_hashes.len() - after_hashes.trim_start_matches([' ', '\t']).len();
        let content_rel = trimmed_start + hashes + ws;
        if !line[content_rel..].trim().is_empty() {
            let h_style = match hashes {
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
            parse_live_inline_segments(
                &line[content_rel..],
                line_start + content_rel,
                h_style,
                theme,
                &mut out,
            );
            return out;
        }
    }

    // 3. Blockquotes & Obsidian Callouts (`> `, `>> `, `> [!tip] Title`)
    if rest.starts_with('>') {
        if !indent.is_empty() {
            out.push(LiveSeg {
                text: indent.to_string(),
                style: Style::default().fg(theme.fg_primary),
                raw_start: line_start,
                raw_end: line_start + trimmed_start,
                is_literal: true,
            });
        }
        let mut cursor_rel = trimmed_start;
        let mut depth = 0usize;
        while line[cursor_rel..].starts_with('>') {
            depth += 1;
            cursor_rel += 1;
            if line[cursor_rel..].starts_with(' ') {
                cursor_rel += 1;
            }
        }
        let body = &line[cursor_rel..];
        if let Some(after_open) = body.strip_prefix("[!") {
            if let Some(close) = after_open.find(']') {
                let kind_str = &after_open[..close];
                let ck = CalloutKind::parse_str(kind_str);
                let c_color = theme.callout_color(ck);
                let bar_style = Style::default().fg(c_color).add_modifier(Modifier::BOLD);
                out.push(LiveSeg {
                    text: "┃ ".repeat(depth),
                    style: bar_style,
                    raw_start: line_start + trimmed_start,
                    raw_end: line_start + cursor_rel,
                    is_literal: false,
                });
                let after_tag_rel = cursor_rel + 2 + close + 1;
                let rest_after_tag = &line[after_tag_rel..];
                let sp = rest_after_tag.len() - rest_after_tag.trim_start().len();
                let title_rel = after_tag_rel + sp;
                out.push(LiveSeg {
                    text: format!("{} {}", ck.glyph(), ck.label()),
                    style: bar_style,
                    raw_start: line_start + cursor_rel,
                    raw_end: line_start + after_tag_rel,
                    is_literal: false,
                });
                if title_rel < line.len() {
                    out.push(LiveSeg {
                        text: " ".to_string(),
                        style: bar_style,
                        raw_start: line_start + after_tag_rel,
                        raw_end: line_start + title_rel,
                        is_literal: false,
                    });
                    parse_live_inline_segments(
                        &line[title_rel..],
                        line_start + title_rel,
                        Style::default()
                            .fg(theme.fg_primary)
                            .add_modifier(Modifier::BOLD),
                        theme,
                        &mut out,
                    );
                }
                return out;
            }
        }
        out.push(LiveSeg {
            text: "┃ ".repeat(depth),
            style: Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
            raw_start: line_start + trimmed_start,
            raw_end: line_start + cursor_rel,
            is_literal: false,
        });
        parse_live_inline_segments(
            body,
            line_start + cursor_rel,
            theme.quote_style(),
            theme,
            &mut out,
        );
        return out;
    }

    // 4. Task lists (`- [ ] `, `- [x] `, `* [ ] `, `+ [x] `) and Bullet lists (`- `, `* `, `+ `)
    for prefix in ["- ", "* ", "+ "] {
        if let Some(after_bullet) = rest.strip_prefix(prefix) {
            if !indent.is_empty() {
                out.push(LiveSeg {
                    text: indent.to_string(),
                    style: Style::default().fg(theme.fg_primary),
                    raw_start: line_start,
                    raw_end: line_start + trimmed_start,
                    is_literal: true,
                });
            }
            let bullet_end = trimmed_start + prefix.len();
            let is_unchecked = after_bullet.starts_with("[ ]")
                && after_bullet
                    .as_bytes()
                    .get(3)
                    .is_none_or(|&b| b == b' ' || b == b'\t');
            let is_checked = (after_bullet.starts_with("[x]") || after_bullet.starts_with("[X]"))
                && after_bullet
                    .as_bytes()
                    .get(3)
                    .is_none_or(|&b| b == b' ' || b == b'\t');

            if is_unchecked || is_checked {
                let mut content_rel = bullet_end + 3;
                if line.as_bytes().get(content_rel).is_some_and(|&b| b == b' ') {
                    content_rel += 1;
                }
                let (glyph, glyph_style, text_style) = if is_checked {
                    (
                        "☑ ",
                        Style::default()
                            .fg(theme.success)
                            .add_modifier(Modifier::BOLD),
                        theme
                            .muted_style()
                            .add_modifier(Modifier::DIM | Modifier::CROSSED_OUT),
                    )
                } else {
                    (
                        "☐ ",
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(theme.fg_primary),
                    )
                };
                out.push(LiveSeg {
                    text: glyph.to_string(),
                    style: glyph_style,
                    raw_start: line_start + trimmed_start,
                    raw_end: line_start + content_rel,
                    is_literal: false,
                });
                parse_live_inline_segments(
                    &line[content_rel..],
                    line_start + content_rel,
                    text_style,
                    theme,
                    &mut out,
                );
                return out;
            }

            // Plain bullet list item
            out.push(LiveSeg {
                text: "• ".to_string(),
                style: Style::default()
                    .fg(theme.accent_soft)
                    .add_modifier(Modifier::BOLD),
                raw_start: line_start + trimmed_start,
                raw_end: line_start + bullet_end,
                is_literal: false,
            });
            parse_live_inline_segments(
                &line[bullet_end..],
                line_start + bullet_end,
                Style::default().fg(theme.fg_primary),
                theme,
                &mut out,
            );
            return out;
        }
    }

    // 5. Ordinary paragraph / numbered list with inline Markdown formatting
    parse_live_inline_segments(
        line,
        line_start,
        Style::default().fg(theme.fg_primary),
        theme,
        &mut out,
    );
    out
}

/// Parses inline Markdown on an inactive LivePreview line, hiding raw delimiters for
/// `` `code` ``, `[[wikilink]]`, `***bold italic***`, `**bold**`, `*italic*`, `~~strike~~`,
/// `==highlight==`, and styling `#tag` badges.
fn parse_live_inline_segments(
    slice: &str,
    base_offset: usize,
    base_style: Style,
    theme: &Theme,
    out: &mut Vec<LiveSeg>,
) {
    let bytes = slice.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut plain_start = 0usize;

    let flush_plain = |out: &mut Vec<LiveSeg>, from: usize, to: usize| {
        if to > from {
            out.push(LiveSeg {
                text: slice[from..to].to_string(),
                style: base_style,
                raw_start: base_offset + from,
                raw_end: base_offset + to,
                is_literal: true,
            });
        }
    };

    while i < len {
        // 1. Inline code `` `...` ``
        if bytes[i] == b'`' {
            if let Some(rel_end) = slice[i + 1..].find('`') {
                if rel_end > 0 {
                    flush_plain(out, plain_start, i);
                    let inner_start = i + 1;
                    let inner_end = inner_start + rel_end;
                    out.push(LiveSeg {
                        text: slice[inner_start..inner_end].to_string(),
                        style: theme.code_style(),
                        raw_start: base_offset + inner_start,
                        raw_end: base_offset + inner_end,
                        is_literal: true,
                    });
                    i = inner_end + 1;
                    plain_start = i;
                    continue;
                }
            }
        }

        // 2. Wikilinks / embeds: `![[...]]` or `[[...]]`
        let is_embed = slice[i..].starts_with("![[");
        let is_wiki = !is_embed && slice[i..].starts_with("[[");
        if is_embed || is_wiki {
            let open_len = if is_embed { 3 } else { 2 };
            if let Some(rel_close) = slice[i + open_len..].find("]]") {
                if rel_close > 0 {
                    flush_plain(out, plain_start, i);
                    let inner_start = i + open_len;
                    let inner_end = inner_start + rel_close;
                    let inner = &slice[inner_start..inner_end];
                    if is_embed {
                        out.push(LiveSeg {
                            text: "↗ ".to_string(),
                            style: Style::default().fg(theme.accent),
                            raw_start: base_offset + i,
                            raw_end: base_offset + inner_start,
                            is_literal: false,
                        });
                    }
                    let (disp_start, disp_end) = if let Some(pipe) = inner.find('|') {
                        if pipe + 1 < inner.len() {
                            (inner_start + pipe + 1, inner_end)
                        } else {
                            (inner_start, inner_end)
                        }
                    } else {
                        (inner_start, inner_end)
                    };
                    out.push(LiveSeg {
                        text: slice[disp_start..disp_end].to_string(),
                        style: theme.wikilink_style(true),
                        raw_start: base_offset + disp_start,
                        raw_end: base_offset + disp_end,
                        is_literal: true,
                    });
                    i = inner_end + 2;
                    plain_start = i;
                    continue;
                }
            }
        }

        // 3. Bold + Italic `***...***`
        if slice[i..].starts_with("***") {
            if let Some(rel_end) = slice[i + 3..].find("***") {
                if rel_end > 0 {
                    flush_plain(out, plain_start, i);
                    let inner_start = i + 3;
                    let inner_end = inner_start + rel_end;
                    out.push(LiveSeg {
                        text: slice[inner_start..inner_end].to_string(),
                        style: base_style.add_modifier(Modifier::BOLD | Modifier::ITALIC),
                        raw_start: base_offset + inner_start,
                        raw_end: base_offset + inner_end,
                        is_literal: true,
                    });
                    i = inner_end + 3;
                    plain_start = i;
                    continue;
                }
            }
        }

        // 4. Bold `**...**`
        if slice[i..].starts_with("**") {
            if let Some(rel_end) = slice[i + 2..].find("**") {
                if rel_end > 0 {
                    flush_plain(out, plain_start, i);
                    let inner_start = i + 2;
                    let inner_end = inner_start + rel_end;
                    out.push(LiveSeg {
                        text: slice[inner_start..inner_end].to_string(),
                        style: base_style.add_modifier(Modifier::BOLD),
                        raw_start: base_offset + inner_start,
                        raw_end: base_offset + inner_end,
                        is_literal: true,
                    });
                    i = inner_end + 2;
                    plain_start = i;
                    continue;
                }
            }
        }

        // 5. Strikethrough `~~...~~`
        if slice[i..].starts_with("~~") {
            if let Some(rel_end) = slice[i + 2..].find("~~") {
                if rel_end > 0 {
                    flush_plain(out, plain_start, i);
                    let inner_start = i + 2;
                    let inner_end = inner_start + rel_end;
                    out.push(LiveSeg {
                        text: slice[inner_start..inner_end].to_string(),
                        style: theme.muted_style().add_modifier(Modifier::CROSSED_OUT),
                        raw_start: base_offset + inner_start,
                        raw_end: base_offset + inner_end,
                        is_literal: true,
                    });
                    i = inner_end + 2;
                    plain_start = i;
                    continue;
                }
            }
        }

        // 6. Highlight `==...==`
        if slice[i..].starts_with("==") {
            if let Some(rel_end) = slice[i + 2..].find("==") {
                if rel_end > 0 {
                    flush_plain(out, plain_start, i);
                    let inner_start = i + 2;
                    let inner_end = inner_start + rel_end;
                    out.push(LiveSeg {
                        text: slice[inner_start..inner_end].to_string(),
                        style: Style::default()
                            .fg(theme.bg_canvas)
                            .bg(theme.warning)
                            .add_modifier(Modifier::BOLD),
                        raw_start: base_offset + inner_start,
                        raw_end: base_offset + inner_end,
                        is_literal: true,
                    });
                    i = inner_end + 2;
                    plain_start = i;
                    continue;
                }
            }
        }

        // 7. Italic `*...*` (single asterisk, non-whitespace opening/closing)
        if bytes[i] == b'*'
            && bytes
                .get(i + 1)
                .is_some_and(|&b| b != b'*' && b != b' ' && b != b'\t')
        {
            if let Some(rel_end) = slice[i + 1..].find('*') {
                let inner_start = i + 1;
                let inner_end = inner_start + rel_end;
                let last_ok = rel_end > 0
                    && !matches!(bytes.get(inner_end - 1), Some(b' ') | Some(b'\t'));
                if last_ok {
                    flush_plain(out, plain_start, i);
                    out.push(LiveSeg {
                        text: slice[inner_start..inner_end].to_string(),
                        style: base_style.add_modifier(Modifier::ITALIC),
                        raw_start: base_offset + inner_start,
                        raw_end: base_offset + inner_end,
                        is_literal: true,
                    });
                    i = inner_end + 1;
                    plain_start = i;
                    continue;
                }
            }
        }

        // 8. Italic `_..._` (word-boundary guarded so `snake_case` is preserved)
        if bytes[i] == b'_'
            && (i == 0
                || slice[..i]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_alphanumeric()))
            && bytes
                .get(i + 1)
                .is_some_and(|&b| b != b'_' && b != b' ' && b != b'\t')
        {
            if let Some(rel_end) = slice[i + 1..].find('_') {
                let inner_start = i + 1;
                let inner_end = inner_start + rel_end;
                let after_ok = slice[inner_end + 1..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric());
                let last_ok = rel_end > 0
                    && !matches!(bytes.get(inner_end - 1), Some(b' ') | Some(b'\t'));
                if last_ok && after_ok {
                    flush_plain(out, plain_start, i);
                    out.push(LiveSeg {
                        text: slice[inner_start..inner_end].to_string(),
                        style: base_style.add_modifier(Modifier::ITALIC),
                        raw_start: base_offset + inner_start,
                        raw_end: base_offset + inner_end,
                        is_literal: true,
                    });
                    i = inner_end + 1;
                    plain_start = i;
                    continue;
                }
            }
        }

        // 9. Inline `#tag` styled badge
        if bytes[i] == b'#' {
            let valid_prev = i == 0
                || bytes[i - 1].is_ascii_whitespace()
                || matches!(bytes[i - 1], b'(' | b'[' | b',' | b';');
            if valid_prev && i + 1 < len {
                let mut end = i + 1;
                for (off, ch) in slice[i + 1..].char_indices() {
                    if ch.is_alphanumeric() || matches!(ch, '_' | '-' | '/') {
                        end = i + 1 + off + ch.len_utf8();
                    } else {
                        break;
                    }
                }
                if end > i + 1 {
                    flush_plain(out, plain_start, i);
                    out.push(LiveSeg {
                        text: slice[i..end].to_string(),
                        style: theme.tag_pill_style().add_modifier(Modifier::BOLD),
                        raw_start: base_offset + i,
                        raw_end: base_offset + end,
                        is_literal: true,
                    });
                    i = end;
                    plain_start = i;
                    continue;
                }
            }
        }

        let ch_len = slice[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        i += ch_len;
    }

    flush_plain(out, plain_start, len);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogicalLineKind {
    Normal,
    FrontmatterDelimiter,
    FrontmatterBody,
    CodeFenceOpen,
    CodeFenceClose,
    CodeBody,
}

struct LogicalLineInfo {
    start: usize,
    end: usize,
    kind: LogicalLineKind,
}

fn build_line_context(text: &str) -> Vec<LogicalLineInfo> {
    let mut infos = Vec::new();
    let mut start = 0usize;
    let mut line_idx = 0usize;
    let mut in_frontmatter = false;
    let mut in_code_fence: Option<(char, usize)> = None;

    let mut push_line = |s: usize, e: usize| {
        let raw = text.get(s..e).unwrap_or("");
        let trimmed = raw.trim();
        let kind = if line_idx == 0 && trimmed == "---" {
            in_frontmatter = true;
            LogicalLineKind::FrontmatterDelimiter
        } else if in_frontmatter {
            if trimmed == "---" || trimmed == "..." {
                in_frontmatter = false;
                LogicalLineKind::FrontmatterDelimiter
            } else {
                LogicalLineKind::FrontmatterBody
            }
        } else if let Some((fence_ch, fence_len)) = in_code_fence {
            let close_len = trimmed.chars().take_while(|&c| c == fence_ch).count();
            if close_len >= fence_len && trimmed[close_len..].trim().is_empty() {
                in_code_fence = None;
                LogicalLineKind::CodeFenceClose
            } else {
                LogicalLineKind::CodeBody
            }
        } else if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let fence_ch = trimmed.chars().next().unwrap_or('`');
            let fence_len = trimmed.chars().take_while(|&c| c == fence_ch).count();
            in_code_fence = Some((fence_ch, fence_len));
            LogicalLineKind::CodeFenceOpen
        } else {
            LogicalLineKind::Normal
        };
        infos.push(LogicalLineInfo {
            start: s,
            end: e,
            kind,
        });
        line_idx += 1;
    };

    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            push_line(start, idx);
            start = idx + 1;
        }
    }
    push_line(start, text.len());
    infos
}

fn find_logical_line(ctx: &[LogicalLineInfo], byte_pos: usize) -> Option<&LogicalLineInfo> {
    let idx = ctx
        .partition_point(|info| info.end < byte_pos)
        .min(ctx.len().saturating_sub(1));
    ctx.get(idx)
}

/// Highlights a single soft-wrapped raw visual slice `text[start..end]` with clean Markdown syntax
/// colors while preserving exact display widths (`\t` -> 4 spaces) and selection overlay.
fn highlight_visual_slice(
    text: &str,
    start: usize,
    end: usize,
    ctx: &[LogicalLineInfo],
    selection: Option<(usize, usize)>,
    theme: &Theme,
) -> Line<'static> {
    let Some(info) = find_logical_line(ctx, start) else {
        return Line::from("");
    };
    let line_text = text.get(info.start..info.end).unwrap_or("");
    let line_len = line_text.len();

    // Compute per-byte style for the logical line (only called for visible rows)
    let base_style = Style::default().fg(theme.fg_primary);
    let mut byte_styles = vec![base_style; line_len];

    match info.kind {
        LogicalLineKind::FrontmatterDelimiter => {
            byte_styles.fill(theme.muted_style());
        }
        LogicalLineKind::FrontmatterBody => {
            if let Some(colon) = line_text.find(':') {
                let key_style = Style::default()
                    .fg(theme.accent_soft)
                    .add_modifier(Modifier::BOLD);
                let val_style = Style::default().fg(theme.fg_secondary);
                for st in &mut byte_styles[..=colon] {
                    *st = key_style;
                }
                for st in &mut byte_styles[colon + 1..] {
                    *st = val_style;
                }
            } else {
                byte_styles.fill(Style::default().fg(theme.fg_secondary));
            }
        }
        LogicalLineKind::CodeFenceOpen | LogicalLineKind::CodeFenceClose => {
            byte_styles.fill(
                Style::default()
                    .fg(theme.code_fg)
                    .add_modifier(Modifier::DIM),
            );
        }
        LogicalLineKind::CodeBody => {
            byte_styles.fill(Style::default().fg(theme.code_fg));
        }
        LogicalLineKind::Normal => {
            style_normal_markdown_line(line_text, &mut byte_styles, theme);
        }
    }

    let slice = text.get(start..end).unwrap_or("");
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;

    for (rel_off, ch) in slice.char_indices() {
        let abs_pos = start + rel_off;
        let line_off = abs_pos.saturating_sub(info.start);
        let selected = selection
            .map(|(from, to)| abs_pos >= from && abs_pos < to)
            .unwrap_or(false);
        let style = if selected {
            theme.selection()
        } else {
            byte_styles.get(line_off).copied().unwrap_or(base_style)
        };

        if let Some(prev) = run_style {
            if prev != style && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), prev));
            }
        }
        run_style = Some(style);
        if ch == '\t' {
            run.push_str("    ");
        } else {
            run.push(ch);
        }
    }

    if !run.is_empty() {
        spans.push(Span::styled(run, run_style.unwrap_or(base_style)));
    }
    if spans.is_empty() {
        Line::from("")
    } else {
        Line::from(spans)
    }
}

fn style_normal_markdown_line(line: &str, styles: &mut [Style], theme: &Theme) {
    let trimmed_start = line.len() - line.trim_start().len();
    let rest = &line[trimmed_start..];

    // 1. ATX Headings (`#`..`###### `)
    let hashes = rest.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes)
        && rest
            .as_bytes()
            .get(hashes)
            .is_none_or(|&b| b == b' ' || b == b'\t')
    {
        let h_style = match hashes {
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
        styles.fill(h_style);
        apply_inline_highlights(line, trimmed_start + hashes, styles, theme);
        return;
    }

    // 2. Blockquotes & Obsidian Callouts (`> [!tip] ...`)
    if let Some(after_gt) = rest.strip_prefix('>') {
        styles[trimmed_start..].fill(theme.quote_style());
        styles[trimmed_start] = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let ws = after_gt.len() - after_gt.trim_start().len();
        let after_ws = &after_gt[ws..];
        if after_ws.starts_with("[!") {
            if let Some(close) = after_ws.find(']') {
                let tag_start = trimmed_start + 1 + ws;
                let tag_end = (tag_start + close + 1).min(styles.len());
                let callout_style = Style::default().fg(theme.info).add_modifier(Modifier::BOLD);
                styles[tag_start..tag_end].fill(callout_style);
            }
        }
        apply_inline_highlights(line, trimmed_start + 1, styles, theme);
        return;
    }

    // 3. Task checkboxes (`- [ ] `, `- [x] `, `* [ ] `, `+ [x] `)
    for prefix in ["- ", "* ", "+ "] {
        if let Some(after_bullet) = rest.strip_prefix(prefix) {
            let bullet_end = trimmed_start + prefix.len();
            styles[trimmed_start..bullet_end].fill(Style::default().fg(theme.accent_soft));
            if after_bullet.starts_with("[ ]") {
                let cb_end = (bullet_end + 3).min(styles.len());
                styles[bullet_end..cb_end].fill(
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                );
                apply_inline_highlights(line, cb_end, styles, theme);
                return;
            } else if after_bullet.starts_with("[x]") || after_bullet.starts_with("[X]") {
                let cb_end = (bullet_end + 3).min(styles.len());
                styles[bullet_end..cb_end].fill(
                    Style::default()
                        .fg(theme.success)
                        .add_modifier(Modifier::BOLD),
                );
                if cb_end < styles.len() {
                    styles[cb_end..].fill(theme.muted_style());
                }
                apply_inline_highlights(line, cb_end, styles, theme);
                return;
            }
            apply_inline_highlights(line, bullet_end, styles, theme);
            return;
        }
    }

    // 4. Inline highlights on ordinary paragraphs
    apply_inline_highlights(line, 0, styles, theme);
}

fn apply_inline_highlights(line: &str, start_from: usize, styles: &mut [Style], theme: &Theme) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = start_from.min(len);

    while i < len {
        // Inline code `` `...` ``
        if bytes[i] == b'`' {
            if let Some(rel_end) = line[i + 1..].find('`') {
                let code_end = i + 1 + rel_end + 1;
                styles[i..code_end].fill(theme.code_style());
                i = code_end;
                continue;
            }
        }

        // Wikilinks / embeds: `![[...]]` or `[[...]]`
        let is_embed =
            i + 2 < len && bytes[i] == b'!' && bytes[i + 1] == b'[' && bytes[i + 2] == b'[';
        let is_wiki = i + 1 < len && bytes[i] == b'[' && bytes[i + 1] == b'[';
        if is_embed || is_wiki {
            let open_len = if is_embed { 3 } else { 2 };
            if let Some(rel_close) = line[i + open_len..].find("]]") {
                let link_end = i + open_len + rel_close + 2;
                styles[i..link_end].fill(theme.wikilink_style(true));
                i = link_end;
                continue;
            }
        }

        // Bold `**...**`
        if line[i..].starts_with("**") {
            if let Some(rel_end) = line[i + 2..].find("**") {
                if rel_end > 0 {
                    let end = i + 2 + rel_end + 2;
                    for st in &mut styles[i..end] {
                        *st = st.add_modifier(Modifier::BOLD);
                    }
                    i = end;
                    continue;
                }
            }
        }

        // Strikethrough `~~...~~`
        if line[i..].starts_with("~~") {
            if let Some(rel_end) = line[i + 2..].find("~~") {
                if rel_end > 0 {
                    let end = i + 2 + rel_end + 2;
                    styles[i..end].fill(theme.muted_style().add_modifier(Modifier::CROSSED_OUT));
                    i = end;
                    continue;
                }
            }
        }

        // Inline `#tag` (must be at line start or preceded by whitespace/opening punct)
        if bytes[i] == b'#' {
            let valid_prev = i == 0
                || bytes[i - 1].is_ascii_whitespace()
                || matches!(bytes[i - 1], b'(' | b'[' | b',' | b';');
            if valid_prev && i + 1 < len {
                let mut end = i + 1;
                for (off, ch) in line[i + 1..].char_indices() {
                    if ch.is_alphanumeric() || matches!(ch, '_' | '-' | '/') {
                        end = i + 1 + off + ch.len_utf8();
                    } else {
                        break;
                    }
                }
                if end > i + 1 {
                    let tag_style = Style::default()
                        .fg(theme.tag_fg)
                        .add_modifier(Modifier::BOLD);
                    styles[i..end].fill(tag_style);
                    i = end;
                    continue;
                }
            }
        }

        // Advance by one UTF-8 char
        let ch_len = line[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        i += ch_len;
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn buffer_row_text(terminal: &Terminal<TestBackend>, row: u16) -> String {
        let buf = terminal.backend().buffer();
        let width = buf.area.width;
        let mut out = String::new();
        let mut x = 0u16;
        while x < width {
            let sym = buf[(x, row)].symbol();
            let w = UnicodeWidthStr::width(sym).max(1) as u16;
            out.push_str(sym);
            x += w;
        }
        out.trim_end().to_string()
    }

    #[test]
    fn test_inline_editor_multiline_korean_utf8_dirty_undo_redo_and_crlf() {
        let mut ed =
            InlineNoteEditor::new("Projects/회의록.md", "회의록", "# 제목\r\n\r\n기존 내용", 0);
        assert!(!ed.dirty);
        assert_eq!(ed.save_status, InlineSaveStatus::Saved);
        assert_eq!(ed.mode, EditorMode::LivePreview);
        assert_eq!(ed.cursor_line_col(), (1, 1));

        // Move to end of line 1, insert newline, Tab (2 spaces), and Korean UTF-8
        assert_eq!(
            ed.handle_edit_action(&Action::CursorEnd),
            InlineEditEvent::CursorOrScrollChanged
        );
        assert_eq!(
            ed.handle_edit_action(&Action::Newline),
            InlineEditEvent::TextChanged
        );
        assert_eq!(
            ed.handle_edit_action(&Action::Tab),
            InlineEditEvent::TextChanged
        );
        for ch in "한글 편집".chars() {
            assert_eq!(
                ed.handle_edit_action(&Action::Input(ch)),
                InlineEditEvent::TextChanged
            );
        }
        assert!(ed.dirty);
        assert!(ed.dirty_since.is_some());
        assert_eq!(ed.save_status, InlineSaveStatus::Editing);
        assert_eq!(ed.composer.text(), "# 제목\n  한글 편집\n\n기존 내용");
        assert_eq!(ed.cursor_line_col(), (2, 8));

        // CRLF preservation on disk_markdown()
        assert_eq!(ed.disk_markdown(), "# 제목\r\n  한글 편집\r\n\r\n기존 내용");

        // Undo & Redo
        assert_eq!(
            ed.handle_edit_action(&Action::Undo),
            InlineEditEvent::TextChanged
        );
        assert_eq!(ed.composer.text(), "# 제목\n  한글 편\n\n기존 내용");
        assert_eq!(
            ed.handle_edit_action(&Action::Redo),
            InlineEditEvent::TextChanged
        );
        assert_eq!(ed.composer.text(), "# 제목\n  한글 편집\n\n기존 내용");

        // Mark saved resets dirty flags and status
        ed.mark_saved();
        assert!(!ed.dirty);
        assert!(ed.dirty_since.is_none());
        assert_eq!(ed.save_status, InlineSaveStatus::Saved);
    }

    #[test]
    fn mixed_line_endings_survive_text_edits_and_undo() {
        let raw = "# One\r\nTwo\nThree\r\nFour";
        let mut editor = InlineNoteEditor::new("Test.md", "Test", raw, 0);
        editor.handle_edit_action(&Action::Input('!'));
        assert_eq!(editor.disk_markdown(), format!("!{raw}"));
        editor.composer.set_cursor_line(2);
        editor.handle_edit_action(&Action::Backspace);
        assert_eq!(editor.disk_markdown(), "!# One\r\nTwoThree\r\nFour");
        editor.handle_edit_action(&Action::Undo);
        assert_eq!(editor.disk_markdown(), format!("!{raw}"));
        editor.handle_edit_action(&Action::Redo);
        assert_eq!(editor.disk_markdown(), "!# One\r\nTwoThree\r\nFour");
        editor.handle_edit_action(&Action::Newline);
        assert_eq!(editor.disk_markdown(), "!# One\r\nTwo\r\nThree\r\nFour");
    }

    #[test]
    fn test_inline_editor_renders_header_pills_syntax_highlighting_click_to_cursor_and_scroll() {
        let md = [
            "# Heading One",
            "Link to [[Target]] and #livetag and `code()`",
            "- [ ] Open task",
            "- [x] Done task",
            "> [!tip] Callout line",
            "한글 테스트",
        ]
        .join("\n")
            + "\n"
            + &(1..=40)
                .map(|i| format!("Line {i}: extra scrollable body"))
                .collect::<Vec<_>>()
                .join("\n");

        let mut ed = InlineNoteEditor::new("Notes/Live.md", "Live", &md, 0);
        let theme = Theme::obsidian_dark();
        let mut hitmap = HitMap::new();
        let mut terminal = Terminal::new(TestBackend::new(86, 20)).unwrap();

        let mut cursor = None;
        terminal
            .draw(|f| {
                hitmap.clear();
                cursor = ed.draw(f, f.area(), &theme, &mut hitmap);
            })
            .unwrap();

        assert!(
            cursor.is_some(),
            "inline editor must expose live terminal cursor"
        );
        let screen = buffer_text(&terminal);
        assert!(screen.contains("✎ EDITING"));
        assert!(screen.contains("✓ Live Saved"));
        assert!(screen.contains("[Live]"));
        assert!(screen.contains("[✓ Done]"));
        assert!(screen.contains("[Ctrl+S Save]"));
        assert!(screen.contains("[Ext $EDITOR]"));
        assert!(screen.contains("Ln 1, Col 1"));
        assert!(hitmap
            .region_of(HitTarget::Button(ButtonId::ToggleEditorMode))
            .is_some());
        assert!(hitmap
            .region_of(HitTarget::Button(ButtonId::FinishInlineEdit))
            .is_some());
        assert!(hitmap
            .region_of(HitTarget::Button(ButtonId::SaveInlineEdit))
            .is_some());
        assert!(hitmap
            .region_of(HitTarget::Button(ButtonId::EditExternal))
            .is_some());
        assert!(hitmap.region_of(HitTarget::InlineEditorBody).is_some());

        // Verify syntax highlighting colored Heading 1 differently from body text
        let buf = terminal.backend().buffer();
        let h1_cell = &buf[(0u16, 1u16)];
        assert_eq!(h1_cell.fg, theme.accent);

        // Mouse click on line 6 ("한글 테스트"), display col 2 -> places cursor on second Korean char '글'
        assert!(ed.place_cursor_at_screen_pos(2, 6));
        assert_eq!(ed.cursor_line_col(), (6, 2));

        // Mouse wheel scrolls editor viewport smoothly
        ed.handle_edit_action(&Action::MouseScrollDown { col: 10, row: 5 });
        assert!(!ed.follow_cursor);
        terminal
            .draw(|f| {
                hitmap.clear();
                ed.draw(f, f.area(), &theme, &mut hitmap);
            })
            .unwrap();
        assert_eq!(ed.scroll_y, 3);
    }

    #[test]
    fn test_live_preview_hides_inactive_delimiters_and_reveals_active_line_with_cjk_cursor() {
        let md = [
            "# Title Heading",
            "Body with **bold**, *italic*, `code()`, ~~gone~~, [[WikiPage]], #rust",
            "- [ ] Pending task",
            "- [x] Completed task",
            "- Bullet item",
            "> Blockquote line",
            "---",
            "한글 커서",
        ]
        .join("\n");

        let mut ed = InlineNoteEditor::new("Notes/Preview.md", "Preview", &md, 0);
        assert_eq!(ed.mode, EditorMode::LivePreview);

        // Place cursor on line 8 ("한글 커서") right on '글' (byte offset of '글')
        let line8_start = md.rfind("한글 커서").unwrap();
        let geul_pos = line8_start + '한'.len_utf8();
        ed.composer.set_cursor(geul_pos);

        let theme = Theme::obsidian_dark();
        let mut hitmap = HitMap::new();
        let mut terminal = Terminal::new(TestBackend::new(86, 16)).unwrap();
        let mut cursor = None;
        terminal
            .draw(|f| {
                hitmap.clear();
                cursor = ed.draw(f, f.area(), &theme, &mut hitmap);
            })
            .unwrap();

        // Inactive lines (rows 1..=7) hide raw delimiters and render rich Markdown:
        assert_eq!(buffer_row_text(&terminal, 1), "Title Heading");
        let row2 = buffer_row_text(&terminal, 2);
        assert!(row2.contains("Body with bold, italic, code(), gone, WikiPage, #rust"));
        assert!(!row2.contains("**"));
        assert!(!row2.contains("~~"));
        assert!(!row2.contains("[["));
        assert!(!row2.contains('`'));

        let row3 = buffer_row_text(&terminal, 3);
        assert!(row3.starts_with("☐ Pending task"));
        assert!(!row3.contains("- [ ]"));

        let row4 = buffer_row_text(&terminal, 4);
        assert!(row4.starts_with("☑ Completed task"));
        assert!(!row4.contains("- [x]"));
        // Completed task text carries dim + crossed-out modifiers
        let done_text_cell = &terminal.backend().buffer()[(2u16, 4u16)];
        assert!(done_text_cell.modifier.contains(Modifier::CROSSED_OUT));
        assert!(done_text_cell.modifier.contains(Modifier::DIM));

        assert_eq!(buffer_row_text(&terminal, 5), "• Bullet item");
        assert_eq!(buffer_row_text(&terminal, 6), "┃ Blockquote line");
        assert!(buffer_row_text(&terminal, 7).starts_with("──────────"));

        // Active line (row 8): raw text + exact Korean/CJK wide-char hardware cursor `(2, 8)`
        // ('한' has display width 2, body starts at y=1 -> row index 7 is y=8)
        assert_eq!(buffer_row_text(&terminal, 8), "한글 커서");
        assert_eq!(cursor, Some((2, 8)));

        // Move cursor to line 1 ("# Title Heading") -> line 1 reveals raw `# Title Heading`
        ed.composer.set_cursor(0);
        terminal
            .draw(|f| {
                hitmap.clear();
                cursor = ed.draw(f, f.area(), &theme, &mut hitmap);
            })
            .unwrap();
        assert_eq!(buffer_row_text(&terminal, 1), "# Title Heading");
        assert_eq!(cursor, Some((0, 1)));

        // Clicking on 'b' of rendered "bold" on inactive row 2 places cursor inside `**bold**`!
        let bold_col = buffer_row_text(&terminal, 2).find("bold").unwrap() as u16;
        assert!(ed.place_cursor_at_screen_pos(bold_col, 2));
        let cur = ed.composer.cursor();
        assert_eq!(&ed.composer.text()[cur..cur + 4], "bold");

        // Toggle to Source mode -> all lines reveal raw Markdown syntax & pill changes to `[Src]`
        ed.handle_edit_action(&Action::ToggleEditorMode);
        assert_eq!(ed.mode, EditorMode::Source);
        terminal
            .draw(|f| {
                hitmap.clear();
                cursor = ed.draw(f, f.area(), &theme, &mut hitmap);
            })
            .unwrap();
        assert!(buffer_row_text(&terminal, 0).contains("[Src]"));
        assert_eq!(buffer_row_text(&terminal, 1), "# Title Heading");
        assert_eq!(buffer_row_text(&terminal, 3), "- [ ] Pending task");
        assert_eq!(buffer_row_text(&terminal, 5), "- Bullet item");
        assert_eq!(buffer_row_text(&terminal, 7), "---");
    }
}
