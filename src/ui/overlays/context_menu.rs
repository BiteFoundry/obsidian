//! Context Menu Overlay (`.` or right-click).

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
    overlays::{OverlayKind, OverlayOutcome},
};

#[derive(Debug, Clone)]
pub struct ContextMenuItem {
    pub label: String,
    pub shortcut: String,
    pub action: Action,
}

#[derive(Debug, Clone)]
pub struct ContextMenuOverlay {
    pub col: u16,
    pub row: u16,
    pub items: Vec<ContextMenuItem>,
    pub selected: usize,
}

impl ContextMenuOverlay {
    pub fn default_menu(col: u16, row: u16) -> Self {
        let items = vec![
            ContextMenuItem {
                label: "Knowledge Graph View".into(),
                shortcut: "G".into(),
                action: Action::ToggleGraphView,
            },
            ContextMenuItem {
                label: "Open Today's Daily Note".into(),
                shortcut: "D".into(),
                action: Action::OpenDailyNote,
            },
            ContextMenuItem {
                label: "Quick Append to Note".into(),
                shortcut: "a".into(),
                action: Action::OpenQuickAppend,
            },
            ContextMenuItem {
                label: "New Note".into(),
                shortcut: "n".into(),
                action: Action::OpenNewNoteModal,
            },
            ContextMenuItem {
                label: "Edit in $EDITOR".into(),
                shortcut: "e".into(),
                action: Action::RequestExternalEditor,
            },
            ContextMenuItem {
                label: "Toggle Bookmark".into(),
                shortcut: "b".into(),
                action: Action::ToggleBookmark,
            },
            ContextMenuItem {
                label: "Extract Links".into(),
                shortcut: "u".into(),
                action: Action::OpenLinkExtractor,
            },
            ContextMenuItem {
                label: "Interactive HTML Preview".into(),
                shortcut: "H".into(),
                action: Action::OpenHtmlPreview,
            },
            ContextMenuItem {
                label: "Attach Local File".into(),
                shortcut: "Ctrl+F".into(),
                action: Action::OpenFilePicker,
            },
            ContextMenuItem {
                label: "Switch Vault".into(),
                shortcut: "Ctrl+S".into(),
                action: Action::OpenOverlay(OverlayKind::VaultSwitcher),
            },
            ContextMenuItem {
                label: "Settings & Palettes".into(),
                shortcut: "Ctrl+,".into(),
                action: Action::OpenOverlay(OverlayKind::Settings),
            },
            ContextMenuItem {
                label: "Move Note to .trash/".into(),
                shortcut: "X".into(),
                action: Action::ConfirmDeleteNote,
            },
        ];
        Self {
            col,
            row,
            items,
            selected: 0,
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay | Action::Input('q') | Action::Input('.') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::Input('k') => {
                if !self.items.is_empty() {
                    self.selected = (self.selected + self.items.len() - 1) % self.items.len();
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                if !self.items.is_empty() {
                    self.selected = (self.selected + 1) % self.items.len();
                }
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                if let Some(it) = self.items.get(self.selected) {
                    OverlayOutcome::CloseAndDispatch(it.action.clone())
                } else {
                    OverlayOutcome::Close
                }
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let w = 36u16.min(area.width.saturating_sub(2));
        let h = (self.items.len() as u16 + 2).min(area.height.saturating_sub(2));
        let x = self.col.min(area.x + area.width.saturating_sub(w));
        let y = self.row.min(area.y + area.height.saturating_sub(h));
        let rect = Rect::new(x, y, w, h);

        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .border_style(theme.border_style(true))
            .title(Line::from(Span::styled(" Actions ", theme.title(true))));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        for (idx, it) in self.items.iter().enumerate() {
            let ry = inner.y + idx as u16;
            if ry >= inner.y + inner.height {
                break;
            }
            let row_rect = Rect::new(inner.x, ry, inner.width, 1);
            let is_sel = idx == self.selected;
            let line = Line::from(vec![
                Span::styled(
                    if is_sel { " ▸ " } else { "   " },
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<22}", it.label),
                    Style::default()
                        .fg(theme.fg_primary)
                        .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() }),
                ),
                Span::styled(format!(" {}", it.shortcut), theme.muted_style()),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(if is_sel {
                    theme.selection()
                } else {
                    Style::default().bg(theme.bg_elevated)
                }),
                row_rect,
            );
            hitmap.add(row_rect, HitTarget::OverlayItem { index: idx });
        }
    }
}
