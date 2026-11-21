//! Vault Switcher Overlay (`Ctrl+S`): switch across all auto-discovered & registered Obsidian vaults.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
    truncate_width,
};

#[derive(Debug, Clone, Default)]
pub struct VaultSwitcherOverlay {
    pub selected: usize,
}

impl VaultSwitcherOverlay {
    pub fn new(app: &AppState) -> Self {
        let idx = app
            .vaults
            .iter()
            .position(|v| v.id == app.active_vault.id)
            .unwrap_or(0);
        Self { selected: idx }
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        let len = app.vaults.len();
        match action {
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::Input('k') => {
                if len > 0 {
                    self.selected = (self.selected + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                if len > 0 {
                    self.selected = (self.selected + 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                if let Some(v) = app.vaults.get(self.selected) {
                    OverlayOutcome::CloseAndDispatch(Action::SwitchVault(v.id.clone()))
                } else {
                    OverlayOutcome::Close
                }
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(10).clamp(40, 76),
            (app.vaults.len() as u16 + 5).clamp(8, 18),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .border_style(theme.border_style(true))
            .title(Line::from(Span::styled(
                format!(
                    " Switch Vault ({}) · Enter Open · Esc Close ",
                    app.vaults.len()
                ),
                theme.title(true),
            )));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        for (idx, v) in app.vaults.iter().enumerate() {
            let y = inner.y + idx as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let row_rect = Rect::new(inner.x, y, inner.width, 1);
            let is_sel = idx == self.selected;
            let is_active = v.id == app.active_vault.id;
            let badge = if v.discovered { "auto" } else { "local" };
            let line = Line::from(vec![
                Span::styled(
                    if is_sel { " ▸ " } else { "   " },
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if is_active { "◆ " } else { "◇ " },
                    Style::default().fg(if is_active {
                        theme.accent
                    } else {
                        theme.fg_muted
                    }),
                ),
                Span::styled(
                    truncate_width(v.display_name(), 24),
                    Style::default()
                        .fg(theme.fg_primary)
                        .add_modifier(if is_sel || is_active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(format!("  [{badge}]  "), theme.tag_pill_style()),
                Span::styled(
                    truncate_width(
                        &v.path.to_string_lossy(),
                        (inner.width as usize).saturating_sub(42),
                    ),
                    theme.muted_style(),
                ),
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
