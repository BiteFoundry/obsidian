//! Full-screen HD inline terminal image preview modal (`v` / `Alt+V` or clicking `[IMG ...]`).

use std::path::PathBuf;

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    img_preview::TerminalImageProtocol,
    models::NoteDetail,
    ui::{
        action::Action,
        app::{AppState, PendingPixelGraphic, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewTarget {
    pub note_path: String,
    pub image_index: usize,
    pub abs_path: PathBuf,
    pub display_name: String,
    pub mime_type: String,
}

#[derive(Debug, Clone)]
pub struct ImagePreviewOverlay {
    pub items: Vec<PreviewTarget>,
    pub index: usize,
    pub zoom_pct: u16,
    pub error: Option<String>,
}

impl ImagePreviewOverlay {
    pub fn from_note(detail: &NoteDetail, preferred_idx: Option<usize>) -> Option<Self> {
        if detail.images.is_empty() {
            return None;
        }
        let items: Vec<PreviewTarget> = detail
            .images
            .iter()
            .enumerate()
            .map(|(idx, img)| PreviewTarget {
                note_path: detail.summary.rel_path.clone(),
                image_index: idx,
                abs_path: img.abs_path.clone(),
                display_name: img.display_name.clone(),
                mime_type: img.mime_type.clone(),
            })
            .collect();
        let start = preferred_idx.unwrap_or(0).min(items.len() - 1);
        Some(Self {
            items,
            index: start,
            zoom_pct: 100,
            error: None,
        })
    }

    pub fn current(&self) -> Option<&PreviewTarget> {
        self.items.get(self.index)
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(20).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    pub fn target_dimensions(&self, term_width: u16, term_height: u16) -> (u16, u16) {
        let rect = Self::modal_rect(Rect::new(0, 0, term_width.max(24), term_height.max(12)));
        let inner_w = rect.width.saturating_sub(2).max(12);
        let inner_h = rect.height.saturating_sub(3).max(6);
        let scaled_cols = ((inner_w as u32 * self.zoom_pct as u32) / 100)
            .clamp(8, inner_w as u32) as u16;
        let scaled_rows = ((inner_h as u32 * self.zoom_pct as u32) / 100)
            .clamp(4, inner_h as u32) as u16;
        (scaled_cols, scaled_rows)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay | Action::Input('q') | Action::Input('v') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::CursorRight | Action::MoveDown | Action::Input('n') => {
                if !self.items.is_empty() {
                    self.index = (self.index + 1) % self.items.len();
                }
                OverlayOutcome::Handled
            }
            Action::CursorLeft | Action::MoveUp | Action::Input('p') => {
                if !self.items.is_empty() {
                    self.index = (self.index + self.items.len() - 1) % self.items.len();
                }
                OverlayOutcome::Handled
            }
            Action::Input('+') | Action::Input('=') => {
                self.zoom_pct = (self.zoom_pct + 20).min(180);
                OverlayOutcome::Handled
            }
            Action::Input('-') => {
                self.zoom_pct = self.zoom_pct.saturating_sub(20).max(40);
                OverlayOutcome::Handled
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
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let cur = self.current();
        let name = cur.map(|c| c.display_name.as_str()).unwrap_or("Image");
        let title = format!(
            " Image {}/{} · {} ({}%) · ←/→ Prev/Next · +/- Zoom · Esc Close ",
            self.index + 1,
            self.items.len().max(1),
            name,
            self.zoom_pct
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .border_style(theme.border_style(true))
            .title(Line::from(Span::styled(title, theme.title(true))));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(Span::styled(err.clone(), Style::default().fg(theme.error))),
                inner,
            );
            return;
        }

        let Some(target) = cur else {
            return;
        };

        let key = format!("{}@{}", target.abs_path.to_string_lossy(), self.zoom_pct);
        if let Some(rendered) = app.preview_cache.get(&key) {
            if rendered.protocol.is_pixel() {
                if let Some(payload) = &rendered.pixel_payload {
                    let _ = ui.pending_pixel.set(Some(PendingPixelGraphic {
                        col: inner.x,
                        row: inner.y,
                        protocol: rendered.protocol,
                        payload: payload.clone(),
                    }));
                }
            }
            if rendered.protocol == TerminalImageProtocol::Halfblock || rendered.pixel_payload.is_none() {
                frame.render_widget(Paragraph::new(rendered.lines.clone()), inner);
            }
        } else {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("  Rendering {} ...", target.display_name),
                    theme.muted_style(),
                )),
                inner,
            );
        }
    }
}
