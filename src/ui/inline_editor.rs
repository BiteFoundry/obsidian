//! In-Place Inline Markdown Note Editor directly inside `note_view`.
//!
//! Backed by `crate::ui::composer::MultiLineComposer`, featuring:
//! - Compact 1-line inline edit header bar with live status (`✓ Live Saved` / `● Editing...` / `⚠ Error`),
//!   clickable pills (`[✓ Done]`, `[Ctrl+S Save]`, `[Ext $EDITOR]`), and `Ln X, Col Y`
//! - Live Markdown syntax highlighting (`#` headings, `[[wikilinks]]`, `#tags`, `- [ ]`/`- [x]` tasks,
//!   `>` callouts/quotes, `` ` `` code, YAML frontmatter)
//! - Mouse click-to-place cursor, mouse wheel scrolling + vertical scrollbar, UTF-8/Korean input,
//!   Tab (2 spaces), clipboard paste, and Undo/Redo
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
    ui::{
        action::Action,
        composer::{text_layout, truncate_display, MultiLineComposer},
        hitmap::{ButtonId, HitMap, HitTarget},
        theme::Theme,
    },
    vault::fs::content_sha256_bytes,
};

/// Debounce window before automatically flushing inline edits to disk + SQLite.
pub const INLINE_AUTOSAVE_DEBOUNCE_MS: u64 = 250;

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
    pub composer: MultiLineComposer,
    pub scroll_y: u16,
    pub follow_cursor: bool,
    pub last_viewport_height: u16,
    pub last_editor_area: Rect,
    pub last_content_width: u16,
    pub dirty: bool,
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
            composer,
            scroll_y: initial_scroll_y,
            follow_cursor: initial_scroll_y == 0,
            last_viewport_height: 20,
            last_editor_area: Rect::default(),
            last_content_width: 72,
            dirty: false,
            dirty_since: None,
            save_status: InlineSaveStatus::Saved,
        }
    }

    /// Cleanly reloads editor text from disk after an external modification (when buffer is clean).
    pub fn reload_from_disk(&mut self, raw_markdown: &str, disk_hash: [u8; 32]) {
        let (prev_line, _) = self.cursor_line_col();
        let prev_scroll = self.scroll_y;
        self.base_content_hash = disk_hash;
        self.external_conflict = None;
        self.composer.load_text_preserving_line_endings(raw_markdown);
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

    pub fn handle_edit_action(&mut self, action: &Action) -> InlineEditEvent {
        match action {
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

    /// Places the cursor at the UTF-8 byte position corresponding to terminal screen `(col, row)`.
    pub fn place_cursor_at_screen_pos(&mut self, col: u16, row: u16) -> bool {
        if !self.last_editor_area.contains(Position::new(col, row)) {
            return false;
        }
        let rel_row = (row - self.last_editor_area.y) as usize;
        let target_col = (col - self.last_editor_area.x) as usize;
        let width = self.last_content_width.max(1) as usize;
        let text = self.composer.text();
        let (visual, _, _) = text_layout(text, width, self.composer.cursor());

        let visual_idx = (self.scroll_y as usize).saturating_add(rel_row);
        let new_pos = if visual_idx >= visual.len() {
            text.len()
        } else {
            let (start, end) = visual[visual_idx];
            byte_at_display_col(text, start, end, target_col)
        };
        self.composer.set_cursor(new_pos);
        self.follow_cursor = true;
        true
    }

    /// Renders the 1-line inline edit header bar + syntax-highlighted editable Markdown directly
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

        // Probe line count to decide whether to reserve 1 col for the right scrollbar
        let probe_w = content_area.width.saturating_sub(1).max(1) as usize;
        let (probe_visual, _, _) =
            text_layout(self.composer.text(), probe_w, self.composer.cursor());
        let show_scrollbar = probe_visual.len() > body_h as usize && content_area.width > 3;
        let content_w = if show_scrollbar {
            content_area.width.saturating_sub(1)
        } else {
            content_area.width
        };
        self.last_content_width = content_w;
        self.composer.last_inner_width = content_w;

        let text = self.composer.text();
        let (visual, cursor_row, cursor_col) =
            text_layout(text, content_w.max(1) as usize, self.composer.cursor());
        let total_rows = visual.len();
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

        let surface_style = Style::default().bg(theme.bg_composer).fg(theme.fg_primary);
        let selection = self.composer.selection_range();
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
            let ctx = build_line_context(text);
            for &(start, end) in visual.iter().skip(offset).take(view_h) {
                lines.push(highlight_visual_slice(
                    text,
                    start,
                    end,
                    &ctx,
                    selection,
                    theme,
                ));
            }
        }

        frame.render_widget(Paragraph::new(lines).style(surface_style), editor_area);

        if show_scrollbar {
            let sb_area = Rect::new(
                content_area.right().saturating_sub(1),
                body_y,
                1,
                body_h,
            );
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

    fn draw_header_bar(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        hitmap: &mut HitMap,
    ) {
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
        let done_pill = "[✓ Done]";
        let save_pill = "[Ctrl+S Save]";
        let ext_pill = "[Ext $EDITOR]";

        let pills_w = (done_pill.width() + 1 + save_pill.width() + 1 + ext_pill.width()) as u16;
        let ln_w = ln_col_label.width() as u16;
        let total_w = area.width;

        // Decide right-side elements: always prioritize clickable pills, add `Ln X, Col Y` when space allows
        let show_pills = total_w >= pills_w + 10;
        let show_ln_col = show_pills && total_w >= pills_w + ln_w + 18;
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
            let mut px = area.right().saturating_sub(right_w);
            for (label, btn, style) in [
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogicalLineKind {
    Normal,
    FrontmatterDelimiter,
    FrontmatterBody,
    CodeFence,
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
                LogicalLineKind::CodeFence
            } else {
                LogicalLineKind::CodeBody
            }
        } else if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let fence_ch = trimmed.chars().next().unwrap_or('`');
            let fence_len = trimmed.chars().take_while(|&c| c == fence_ch).count();
            in_code_fence = Some((fence_ch, fence_len));
            LogicalLineKind::CodeFence
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

fn find_logical_line<'a>(ctx: &'a [LogicalLineInfo], byte_pos: usize) -> Option<&'a LogicalLineInfo> {
    let idx = ctx
        .partition_point(|info| info.end < byte_pos)
        .min(ctx.len().saturating_sub(1));
    ctx.get(idx)
}

/// Highlights a single soft-wrapped visual slice `text[start..end]` with clean Markdown syntax
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
        LogicalLineKind::CodeFence => {
            byte_styles.fill(Style::default().fg(theme.code_fg).add_modifier(Modifier::DIM));
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
    if rest.starts_with('>') {
        styles[trimmed_start..].fill(theme.quote_style());
        styles[trimmed_start] = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let after_gt = &rest[1..];
        let ws = after_gt.len() - after_gt.trim_start().len();
        let after_ws = &after_gt[ws..];
        if after_ws.starts_with("[!") {
            if let Some(close) = after_ws.find(']') {
                let tag_start = trimmed_start + 1 + ws;
                let tag_end = (tag_start + close + 1).min(styles.len());
                let callout_style = Style::default()
                    .fg(theme.info)
                    .add_modifier(Modifier::BOLD);
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
        let is_embed = i + 2 < len && bytes[i] == b'!' && bytes[i + 1] == b'[' && bytes[i + 2] == b'[';
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

    #[test]
    fn test_inline_editor_multiline_korean_utf8_dirty_undo_redo_and_crlf() {
        let mut ed = InlineNoteEditor::new(
            "Projects/회의록.md",
            "회의록",
            "# 제목\r\n\r\n기존 내용",
            0,
        );
        assert!(!ed.dirty);
        assert_eq!(ed.save_status, InlineSaveStatus::Saved);
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
        assert_eq!(ed.handle_edit_action(&Action::Undo), InlineEditEvent::TextChanged);
        assert_eq!(ed.composer.text(), "# 제목\n  한글 편\n\n기존 내용");
        assert_eq!(ed.handle_edit_action(&Action::Redo), InlineEditEvent::TextChanged);
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

        assert!(cursor.is_some(), "inline editor must expose live terminal cursor");
        let screen = buffer_text(&terminal);
        assert!(screen.contains("✎ EDITING"));
        assert!(screen.contains("✓ Live Saved"));
        assert!(screen.contains("[✓ Done]"));
        assert!(screen.contains("[Ctrl+S Save]"));
        assert!(screen.contains("[Ext $EDITOR]"));
        assert!(screen.contains("Ln 1, Col 1"));
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::FinishInlineEdit)).is_some());
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::SaveInlineEdit)).is_some());
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::EditExternal)).is_some());
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
}
