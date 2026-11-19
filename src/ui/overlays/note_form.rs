//! New Note (`n`) & Quick Append (`a`) modal overlay.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteFormMode {
    NewNote,
    QuickAppend { target_title: String },
}

#[derive(Debug, Clone)]
pub struct NoteFormOverlay {
    pub mode: NoteFormMode,
    pub field: usize,
    pub title: String,
    pub folder: String,
    pub tags: String,
    pub text: String,
}

impl NoteFormOverlay {
    pub fn new_note(default_folder: String) -> Self {
        Self {
            mode: NoteFormMode::NewNote,
            field: 0,
            title: String::new(),
            folder: default_folder,
            tags: String::new(),
            text: String::new(),
        }
    }

    pub fn quick_append(target_title: String) -> Self {
        Self {
            mode: NoteFormMode::QuickAppend { target_title },
            field: 0,
            title: String::new(),
            folder: String::new(),
            tags: String::new(),
            text: String::new(),
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::Tab | Action::MoveDown => {
                if matches!(self.mode, NoteFormMode::NewNote) {
                    self.field = (self.field + 1) % 3;
                }
                OverlayOutcome::Handled
            }
            Action::PrevFocus | Action::MoveUp => {
                if matches!(self.mode, NoteFormMode::NewNote) {
                    self.field = (self.field + 2) % 3;
                }
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.active_buf().pop();
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.active_buf().push(*ch);
                OverlayOutcome::Handled
            }
            Action::Newline => {
                if matches!(self.mode, NoteFormMode::QuickAppend { .. }) {
                    self.text.push('\n');
                }
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => match &self.mode {
                NoteFormMode::NewNote => {
                    if self.title.trim().is_empty() {
                        return OverlayOutcome::Handled;
                    }
                    OverlayOutcome::CloseAndDispatch(Action::CreateNoteSubmit {
                        title: self.title.trim().to_string(),
                        folder: self.folder.trim().to_string(),
                        tags: self.tags.trim().to_string(),
                    })
                }
                NoteFormMode::QuickAppend { .. } => {
                    if self.text.trim().is_empty() {
                        return OverlayOutcome::Close;
                    }
                    OverlayOutcome::CloseAndDispatch(Action::QuickAppendSubmit {
                        text: self.text.clone(),
                    })
                }
            },
            _ => OverlayOutcome::Handled,
        }
    }

    fn active_buf(&mut self) -> &mut String {
        match self.mode {
            NoteFormMode::NewNote => match self.field {
                0 => &mut self.title,
                1 => &mut self.folder,
                _ => &mut self.tags,
            },
            NoteFormMode::QuickAppend { .. } => &mut self.text,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(area, area.width.saturating_sub(10).min(72).max(40), 12);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let header = match &self.mode {
            NoteFormMode::NewNote => " New Note · Tab Switch Field · Enter Create · Esc Cancel ".to_string(),
            NoteFormMode::QuickAppend { target_title } => {
                format!(" Quick Append → {target_title} · Enter Save · Esc Cancel ")
            }
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .border_style(theme.border_style(true))
            .title(Line::from(Span::styled(header, theme.title(true))));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        match &self.mode {
            NoteFormMode::NewNote => {
                let fields = [
                    ("Title", &self.title, "e.g. Q4 Architecture RFC"),
                    ("Folder", &self.folder, "e.g. Projects (optional)"),
                    ("Tags", &self.tags, "e.g. architecture, rust"),
                ];
                for (i, (label, val, placeholder)) in fields.iter().enumerate() {
                    let y = inner.y + 1 + (i as u16) * 2;
                    if y >= inner.y + inner.height {
                        break;
                    }
                    let active = self.field == i;
                    let line = Line::from(vec![
                        Span::styled(
                            format!(" {:<8} ", label),
                            if active {
                                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
                            } else {
                                theme.muted_style()
                            },
                        ),
                        if val.is_empty() && !active {
                            Span::styled((*placeholder).to_string(), theme.subtle_style())
                        } else {
                            Span::styled(
                                if active { format!("{val}▏") } else { (*val).clone() },
                                Style::default().fg(theme.fg_primary),
                            )
                        },
                    ]);
                    frame.render_widget(
                        Paragraph::new(line).style(if active {
                            Style::default().bg(theme.bg_composer)
                        } else {
                            Style::default().bg(theme.bg_elevated)
                        }),
                        Rect::new(inner.x, y, inner.width, 1),
                    );
                }
            }
            NoteFormMode::QuickAppend { .. } => {
                let hint = Line::from(Span::styled(
                    " Tip: type a line, `- [ ] task`, `> [!tip] callout`, or `[[Wikilink]]`",
                    theme.muted_style(),
                ));
                frame.render_widget(Paragraph::new(hint), Rect::new(inner.x, inner.y + 1, inner.width, 1));

                let input = Line::from(vec![
                    Span::styled(" > ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("{}▏", self.text),
                        Style::default().fg(theme.fg_primary),
                    ),
                ]);
                frame.render_widget(
                    Paragraph::new(input).style(Style::default().bg(theme.bg_composer)),
                    Rect::new(inner.x, inner.y + 3, inner.width, 3),
                );
            }
        }
    }
}
