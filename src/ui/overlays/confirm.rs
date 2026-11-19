//! Confirmation modal overlay (delete note to `.trash/`, quit confirmation).

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
    hitmap::{ButtonId, HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone)]
pub struct ConfirmOverlay {
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub on_confirm: Action,
    pub focused_confirm: bool,
}

impl ConfirmOverlay {
    pub fn new(
        title: impl Into<String>,
        message: impl Into<String>,
        confirm_label: impl Into<String>,
        on_confirm: Action,
    ) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            confirm_label: confirm_label.into(),
            on_confirm,
            focused_confirm: true,
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay | Action::Input('n') | Action::Input('N') => OverlayOutcome::Close,
            Action::Input('y') | Action::Input('Y') => {
                OverlayOutcome::CloseAndDispatch(self.on_confirm.clone())
            }
            Action::Tab | Action::CursorLeft | Action::CursorRight => {
                self.focused_confirm = !self.focused_confirm;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                if self.focused_confirm {
                    OverlayOutcome::CloseAndDispatch(self.on_confirm.clone())
                } else {
                    OverlayOutcome::Close
                }
            }
            Action::Quit => OverlayOutcome::CloseAndDispatch(Action::Quit),
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(area, area.width.saturating_sub(12).min(58).max(34), 8);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .border_style(theme.border_style(true))
            .title(Line::from(Span::styled(
                format!(" {} ", self.title),
                theme.title(true),
            )));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("  {}", self.message),
                Style::default().fg(theme.fg_primary),
            )),
            Rect::new(inner.x, inner.y + 1, inner.width, 2),
        );

        let btn_y = inner.y + inner.height.saturating_sub(2);
        let ok_style = if self.focused_confirm {
            Style::default()
                .fg(theme.bg_canvas)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            theme.prop_pill_style()
        };
        let cancel_style = if !self.focused_confirm {
            Style::default()
                .fg(theme.bg_canvas)
                .bg(theme.fg_muted)
                .add_modifier(Modifier::BOLD)
        } else {
            theme.prop_pill_style()
        };

        let ok_text = format!(" {} [y] ", self.confirm_label);
        let cancel_text = " Cancel [n] ";
        let ok_w = ok_text.len() as u16;
        let cancel_w = cancel_text.len() as u16;
        let x0 = inner.x + 2;

        frame.render_widget(
            Paragraph::new(Span::styled(ok_text, ok_style)),
            Rect::new(x0, btn_y, ok_w, 1),
        );
        hitmap.add(Rect::new(x0, btn_y, ok_w, 1), HitTarget::Button(ButtonId::ConfirmOk));

        let x1 = x0 + ok_w + 2;
        frame.render_widget(
            Paragraph::new(Span::styled(cancel_text, cancel_style)),
            Rect::new(x1, btn_y, cancel_w, 1),
        );
        hitmap.add(
            Rect::new(x1, btn_y, cancel_w, 1),
            HitTarget::Button(ButtonId::ConfirmCancel),
        );
    }
}
