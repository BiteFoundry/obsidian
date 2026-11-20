//! Built-in In-Terminal Multi-Line Markdown Note Editor Overlay (`Overlay::NoteEditor`).
//!
//! Backed by `crate::ui::composer::MultiLineComposer` so pressing `e` or clicking `[✎ Edit]`
//! edits the active Obsidian Markdown note directly inside the TUI without leaving raw mode.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::ui::{
    action::Action,
    app::UiState,
    composer::{normalize, push_line_spans, text_layout, MultiLineComposer},
    hitmap::{ButtonId, HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone)]
pub struct NoteEditorOverlay {
    pub note_path: String,
    pub note_title: String,
    pub initial_markdown: String,
    pub composer: MultiLineComposer,
    pub scroll_y: u16,
    pub follow_cursor: bool,
    pub last_viewport_height: u16,
    pub confirm_discard: bool,
}

impl NoteEditorOverlay {
    pub fn new(
        note_path: impl Into<String>,
        note_title: impl Into<String>,
        markdown: impl Into<String>,
    ) -> Self {
        let initial_markdown = normalize(&markdown.into());
        let mut composer = MultiLineComposer::new();
        composer.load_text_cursor_start(&initial_markdown);
        Self {
            note_path: note_path.into(),
            note_title: note_title.into(),
            initial_markdown,
            composer,
            scroll_y: 0,
            follow_cursor: true,
            last_viewport_height: 20,
            confirm_discard: false,
        }
    }

    pub fn accepts_text(&self) -> bool {
        true
    }

    pub fn is_modified(&self) -> bool {
        self.composer.text() != self.initial_markdown
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        self.composer.cursor_line_col()
    }

    pub fn modal_rect(area: Rect) -> Rect {
        let w = ((area.width as u32 * 92) / 100)
            .max(24)
            .min(area.width as u32) as u16;
        let h = ((area.height as u32 * 90) / 100)
            .max(10)
            .min(area.height as u32) as u16;
        centered_rect(area, w, h)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay => {
                if self.is_modified() && !self.confirm_discard {
                    self.confirm_discard = true;
                    OverlayOutcome::Handled
                } else {
                    OverlayOutcome::Close
                }
            }
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::OverlaySubmit => OverlayOutcome::CloseAndDispatch(Action::SubmitNoteEdit {
                note_path: self.note_path.clone(),
                markdown: self.composer.text().to_string(),
            }),
            Action::RequestExternalEditor => {
                OverlayOutcome::CloseAndDispatch(Action::RequestExternalEditor)
            }
            Action::PasteClipboard => {
                self.confirm_discard = false;
                OverlayOutcome::Dispatch(Action::PasteClipboard)
            }
            Action::Input(ch) => {
                self.confirm_discard = false;
                self.composer.insert_char(*ch);
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::Newline => {
                self.confirm_discard = false;
                self.composer.newline();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.confirm_discard = false;
                self.composer.backspace();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::Delete => {
                self.confirm_discard = false;
                self.composer.delete_forward();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::Tab => {
                self.confirm_discard = false;
                self.composer.insert_str("  ");
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.confirm_discard = false;
                self.composer.insert_str(text);
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::CursorLeft => {
                self.confirm_discard = false;
                self.composer.move_left(false);
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::CursorRight => {
                self.confirm_discard = false;
                self.composer.move_right(false);
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::MoveUp | Action::CursorUp => {
                self.confirm_discard = false;
                self.composer.move_up(false);
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown => {
                self.confirm_discard = false;
                self.composer.move_down(false);
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::CursorHome => {
                self.confirm_discard = false;
                self.composer.move_home(false);
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::CursorEnd => {
                self.confirm_discard = false;
                self.composer.move_end(false);
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.confirm_discard = false;
                let steps = self.last_viewport_height.saturating_sub(2).max(5);
                for _ in 0..steps {
                    self.composer.move_up(false);
                }
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.confirm_discard = false;
                let steps = self.last_viewport_height.saturating_sub(2).max(5);
                for _ in 0..steps {
                    self.composer.move_down(false);
                }
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::Undo => {
                self.confirm_discard = false;
                self.composer.undo();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::Redo => {
                self.confirm_discard = false;
                self.composer.redo();
                self.follow_cursor = true;
                OverlayOutcome::Handled
            }
            Action::MouseScrollUp { .. } => {
                self.follow_cursor = false;
                self.scroll_y = self.scroll_y.saturating_sub(3);
                OverlayOutcome::Handled
            }
            Action::MouseScrollDown { .. } => {
                self.follow_cursor = false;
                self.scroll_y = self.scroll_y.saturating_add(3);
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) -> Option<(u16, u16)> {
        let theme = &ui.theme;
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let save_pill = "[Ctrl+S Save]";
        let ext_pill = "[Ext $EDITOR]";
        let cancel_pill = "[Esc Cancel]";
        let pills_w = (save_pill.width() + 1 + ext_pill.width() + 1 + cancel_pill.width()) as u16;
        let show_pills = rect.width > pills_w + 10;

        let mod_badge = if self.is_modified() {
            " [Modified]"
        } else {
            ""
        };
        let title_budget = rect
            .width
            .saturating_sub(2 + if show_pills { pills_w + 2 } else { 0 })
            as usize;
        let display_label = if self.note_path.is_empty() {
            self.note_title.as_str()
        } else {
            self.note_path.as_str()
        };
        let header = format!(
            " ✎ Edit: {}{} ",
            crate::ui::truncate_width(display_label, 42),
            mod_badge
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg_primary))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&header, title_budget),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if show_pills {
            let mut px = rect.right().saturating_sub(pills_w + 1);
            for (label, btn, style) in [
                (
                    save_pill,
                    ButtonId::ConfirmOk,
                    Style::default()
                        .fg(theme.success)
                        .bg(theme.bg_elevated)
                        .add_modifier(Modifier::BOLD),
                ),
                (
                    ext_pill,
                    ButtonId::EditExternal,
                    Style::default()
                        .fg(theme.accent_soft)
                        .bg(theme.bg_elevated)
                        .add_modifier(Modifier::BOLD),
                ),
                (
                    cancel_pill,
                    ButtonId::ConfirmCancel,
                    Style::default().fg(theme.fg_primary).bg(theme.bg_elevated),
                ),
            ] {
                let w = label.width() as u16;
                frame.render_widget(
                    Paragraph::new(Span::styled(label, style)),
                    Rect::new(px, rect.y, w, 1),
                );
                hitmap.add_line(px, rect.y, w, HitTarget::Button(btn));
                px = px.saturating_add(w + 1);
            }
        }

        if inner.width < 4 || inner.height < 2 {
            return None;
        }

        let body_h = inner.height.saturating_sub(1).max(1);
        self.last_viewport_height = body_h;
        let footer_y = inner.y + inner.height.saturating_sub(1);

        // Probe layout width accounting for optional right scrollbar.
        let probe_w = inner.width.saturating_sub(1).max(1) as usize;
        let (probe_visual, _, _) =
            text_layout(self.composer.text(), probe_w, self.composer.cursor());
        let show_scrollbar = probe_visual.len() > body_h as usize && inner.width > 3;
        let content_w = if show_scrollbar {
            inner.width.saturating_sub(1)
        } else {
            inner.width
        };
        self.composer.last_inner_width = content_w;

        let (visual, cursor_row, cursor_col) = text_layout(
            self.composer.text(),
            content_w.max(1) as usize,
            self.composer.cursor(),
        );
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

        let editor_area = Rect::new(inner.x, inner.y, content_w, body_h);
        let surface_style = Style::default().bg(theme.bg_composer).fg(theme.fg_primary);
        let selection = self.composer.selection_range();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(view_h);

        if self.composer.text().is_empty() {
            lines.push(Line::from(Span::styled(
                crate::ui::composer::truncate_display(
                    "Type Markdown here (# Heading, - [ ] Task, [[Wikilink]], ```code```)...",
                    content_w as usize,
                ),
                theme.muted_style().add_modifier(Modifier::ITALIC),
            )));
        } else {
            for &(start, end) in visual.iter().skip(offset).take(view_h) {
                push_line_spans(
                    &mut lines,
                    self.composer.text(),
                    start,
                    end,
                    selection,
                    theme.selection(),
                );
            }
        }
        frame.render_widget(Paragraph::new(lines).style(surface_style), editor_area);

        if show_scrollbar {
            let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, body_h);
            let mut sb_state = ScrollbarState::new(max_scroll.saturating_add(1))
                .position(offset)
                .viewport_content_length(view_h);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(theme.accent))
                .track_style(theme.muted_style());
            frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
        }

        // Status footer
        let (ln, col) = self.cursor_line_col();
        let (footer_text, footer_style) = if self.confirm_discard {
            (
                format!(
                    " Ln {ln}, Col {col}  ·  [Modified]  ·  Unsaved changes! Press Esc again to discard · Ctrl+S Save "
                ),
                Style::default()
                    .fg(theme.warning)
                    .bg(theme.bg_header)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            let status_tag = if self.is_modified() {
                "[Modified]"
            } else {
                "Saved"
            };
            (
                format!(
                    " Ln {ln}, Col {col}  ·  {status_tag}  ·  Ctrl+S Save · Esc Cancel · Ctrl+Z/Y Undo/Redo · Ctrl+V Paste · Ctrl+E External $EDITOR "
                ),
                Style::default().fg(theme.fg_muted).bg(theme.bg_header),
            )
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                crate::ui::truncate_width(&footer_text, inner.width as usize),
                footer_style,
            ))
            .style(Style::default().bg(theme.bg_header)),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );

        if cursor_row >= offset && cursor_row < offset.saturating_add(view_h) {
            let cy = editor_area
                .y
                .saturating_add((cursor_row - offset) as u16)
                .min(editor_area.bottom().saturating_sub(1));
            let cx = editor_area
                .x
                .saturating_add(cursor_col as u16)
                .min(editor_area.right().saturating_sub(1));
            Some((cx, cy))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::config::AppConfig;

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
    fn test_note_editor_multiline_korean_utf8_dirty_undo_redo_and_submit() {
        let mut ed = NoteEditorOverlay::new(
            "Projects/회의록.md",
            "회의록",
            "# 제목\n\n기존 내용",
        );
        assert!(ed.accepts_text());
        assert!(!ed.is_modified());
        assert_eq!(ed.cursor_line_col(), (1, 1));

        // Unmodified Esc closes immediately
        assert_eq!(ed.handle(&Action::CloseOverlay), OverlayOutcome::Close);

        // Move to end of line 1, insert newline, Tab (2 spaces), and Korean UTF-8
        assert_eq!(ed.handle(&Action::CursorEnd), OverlayOutcome::Handled);
        assert_eq!(ed.handle(&Action::Activate), OverlayOutcome::Handled);
        assert_eq!(ed.handle(&Action::Tab), OverlayOutcome::Handled);
        for ch in "한글 편집".chars() {
            assert_eq!(ed.handle(&Action::Input(ch)), OverlayOutcome::Handled);
        }
        assert!(ed.is_modified());
        assert_eq!(ed.composer.text(), "# 제목\n  한글 편집\n\n기존 내용");
        assert_eq!(ed.cursor_line_col(), (2, 8));

        // Undo removes last character, Redo brings it back
        assert_eq!(ed.handle(&Action::Undo), OverlayOutcome::Handled);
        assert_eq!(ed.composer.text(), "# 제목\n  한글 편\n\n기존 내용");
        assert_eq!(ed.handle(&Action::Redo), OverlayOutcome::Handled);
        assert_eq!(ed.composer.text(), "# 제목\n  한글 편집\n\n기존 내용");

        // Multi-line paste preserves newlines
        assert_eq!(
            ed.handle(&Action::Paste("\n- [ ] 할 일\n- 완료".into())),
            OverlayOutcome::Handled
        );
        assert!(ed.composer.text().contains("- [ ] 할 일\n- 완료"));

        // First Esc while modified arms discard confirmation; second Esc closes cleanly
        assert_eq!(ed.handle(&Action::CloseOverlay), OverlayOutcome::Handled);
        assert!(ed.confirm_discard);
        assert_eq!(ed.handle(&Action::CloseOverlay), OverlayOutcome::Close);

        // Editing resets confirm_discard
        ed.confirm_discard = true;
        assert_eq!(ed.handle(&Action::CursorHome), OverlayOutcome::Handled);
        assert!(!ed.confirm_discard);

        // Ctrl+S submits Action::SubmitNoteEdit with edited Markdown
        match ed.handle(&Action::OverlaySubmit) {
            OverlayOutcome::CloseAndDispatch(Action::SubmitNoteEdit {
                note_path,
                markdown,
            }) => {
                assert_eq!(note_path, "Projects/회의록.md");
                assert!(markdown.contains("한글 편집"));
                assert!(markdown.contains("- [ ] 할 일"));
            }
            other => panic!("expected CloseAndDispatch(SubmitNoteEdit), got {other:?}"),
        }

        // Ctrl+E hands off to RequestExternalEditor
        assert_eq!(
            ed.handle(&Action::RequestExternalEditor),
            OverlayOutcome::CloseAndDispatch(Action::RequestExternalEditor)
        );
    }

    #[test]
    fn test_note_editor_renders_pills_footer_scrollbar_and_mouse_scroll() {
        let long_md = (1..=60)
            .map(|i| format!("Line {i}: Obsidian Markdown content"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut ed = NoteEditorOverlay::new("Notes/Long.md", "Long", long_md);
        let ui = UiState::from_config(&AppConfig::default());
        let mut hitmap = HitMap::new();
        let mut terminal = Terminal::new(TestBackend::new(114, 30)).unwrap();

        let mut cursor = None;
        terminal
            .draw(|f| {
                hitmap.clear();
                cursor = ed.draw(f, f.area(), &ui, &mut hitmap);
            })
            .unwrap();

        assert!(cursor.is_some(), "expected visible editor cursor position");
        let screen = buffer_text(&terminal);
        assert!(screen.contains("[Ctrl+S Save]"));
        assert!(screen.contains("[Ext $EDITOR]"));
        assert!(screen.contains("[Esc Cancel]"));
        assert!(screen.contains("Ln 1, Col 1"));
        assert!(screen.contains("Ctrl+Z/Y Undo/Redo"));
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::ConfirmOk)).is_some());
        assert!(hitmap
            .region_of(HitTarget::Button(ButtonId::EditExternal))
            .is_some());
        assert!(hitmap
            .region_of(HitTarget::Button(ButtonId::ConfirmCancel))
            .is_some());

        // Mouse wheel scrolls editor viewport cleanly without snapping back
        ed.handle(&Action::MouseScrollDown { col: 40, row: 12 });
        ed.handle(&Action::MouseScrollDown { col: 40, row: 12 });
        assert!(!ed.follow_cursor);
        terminal
            .draw(|f| {
                hitmap.clear();
                ed.draw(f, f.area(), &ui, &mut hitmap);
            })
            .unwrap();
        assert_eq!(ed.scroll_y, 6);

        ed.handle(&Action::MouseScrollUp { col: 40, row: 12 });
        terminal
            .draw(|f| {
                hitmap.clear();
                ed.draw(f, f.area(), &ui, &mut hitmap);
            })
            .unwrap();
        assert_eq!(ed.scroll_y, 3);

        // Typing marks [Modified] in header and footer
        ed.handle(&Action::Input('!'));
        assert!(ed.is_modified());
        terminal
            .draw(|f| {
                hitmap.clear();
                ed.draw(f, f.area(), &ui, &mut hitmap);
            })
            .unwrap();
        let mod_screen = buffer_text(&terminal);
        assert!(mod_screen.contains("[Modified]"));
    }
}
