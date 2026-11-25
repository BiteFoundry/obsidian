//! Full-screen HD inline terminal image preview modal (`v` / `Alt+V` or clicking `[IMG ...]`).

use std::path::PathBuf;

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::{FitBox, TerminalImageProtocol},
    models::NoteDetail,
    ui::{
        action::Action,
        app::{AppState, PendingPixelGraphic, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
        truncate_width,
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
    pub scroll_y: u16,
    pub scroll_x: u16,
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
            scroll_y: 0,
            scroll_x: 0,
            error: None,
        })
    }

    pub fn current(&self) -> Option<&PreviewTarget> {
        self.items.get(self.index)
    }

    pub fn cache_key(
        target: &PreviewTarget,
        view_cols: u16,
        view_rows: u16,
        zoom_pct: u16,
        cell_px: (u16, u16),
        proto: TerminalImageProtocol,
    ) -> String {
        let (mtime_ns, size_bytes) = crate::vault::fs::file_mtime_ns_and_size(&target.abs_path);
        format!(
            "modal:{}:{}:{}:{}x{}:{}%:{}x{}:{:?}",
            target.abs_path.to_string_lossy(),
            mtime_ns,
            size_bytes,
            view_cols,
            view_rows,
            zoom_pct,
            cell_px.0,
            cell_px.1,
            proto
        )
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(20).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    pub fn viewport_dimensions(term_width: u16, term_height: u16) -> (u16, u16) {
        let rect = Self::modal_rect(Rect::new(0, 0, term_width.max(24), term_height.max(12)));
        let inner_w = rect.width.saturating_sub(2).max(12);
        let inner_h = rect.height.saturating_sub(2).max(6);
        (inner_w, inner_h)
    }

    pub fn fit_box(&self, term_width: u16, term_height: u16) -> FitBox {
        let (view_cols, view_rows) = Self::viewport_dimensions(term_width, term_height);
        FitBox::modal(view_cols, view_rows, self.zoom_pct)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay
            | Action::OpenImagePreview
            | Action::Input('q')
            | Action::Input('v') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::Input('k') => {
                self.scroll_y = self.scroll_y.saturating_sub(2);
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                self.scroll_y = self.scroll_y.saturating_add(2);
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.scroll_y = self.scroll_y.saturating_sub(10);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.scroll_y = self.scroll_y.saturating_add(10);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::JumpTop | Action::Input('g') => {
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::JumpBottom | Action::Input('G') => {
                self.scroll_y = u16::MAX;
                OverlayOutcome::Handled
            }
            Action::MouseScrollUp { .. } => {
                self.scroll_y = self.scroll_y.saturating_sub(3);
                OverlayOutcome::Handled
            }
            Action::MouseScrollDown { .. } => {
                self.scroll_y = self.scroll_y.saturating_add(3);
                OverlayOutcome::Handled
            }
            Action::Input('h') | Action::ToggleShowLinkUrls => {
                self.scroll_x = self.scroll_x.saturating_sub(4);
                OverlayOutcome::Handled
            }
            Action::Input('l') => {
                self.scroll_x = self.scroll_x.saturating_add(4);
                OverlayOutcome::Handled
            }
            Action::CursorRight
            | Action::Input(']')
            | Action::Input('n')
            | Action::OpenNewNoteModal => {
                if self.items.len() > 1 {
                    self.index = (self.index + 1) % self.items.len();
                    self.scroll_y = 0;
                    self.scroll_x = 0;
                    self.error = None;
                }
                OverlayOutcome::Handled
            }
            Action::CursorLeft
            | Action::ToggleSidebar
            | Action::Input('[')
            | Action::Input('p') => {
                if self.items.len() > 1 {
                    self.index = (self.index + self.items.len() - 1) % self.items.len();
                    self.scroll_y = 0;
                    self.scroll_x = 0;
                    self.error = None;
                }
                OverlayOutcome::Handled
            }
            Action::Input('+') | Action::Input('=') => {
                self.zoom_pct = (self.zoom_pct + 25).min(300);
                OverlayOutcome::Handled
            }
            Action::Input('-') => {
                self.zoom_pct = self.zoom_pct.saturating_sub(25).max(25);
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Handled
            }
            Action::Input('0') => {
                self.zoom_pct = 100;
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = ui.theme.clone();
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let proto = app.graphics_caps.protocol;
        let close_label = "[✕ Esc]";
        let close_w = close_label.width() as u16;
        let show_close_pill = rect.width > close_w + 4;
        let title_budget = rect
            .width
            .saturating_sub(2 + if show_close_pill { close_w + 2 } else { 0 })
            as usize;

        let title = match self.current() {
            Some(c) => format!(
                " Image {}/{}: {} · {}% · {} · j/k/Wheel Scroll · [/] Prev/Next · +/- Zoom ",
                self.index + 1,
                self.items.len().max(1),
                truncate_width(&c.display_name, 22),
                self.zoom_pct,
                proto.label()
            ),
            None => " Image Preview ".to_string(),
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .border_style(theme.border_style(true))
            .title(Line::from(Span::styled(
                truncate_width(&title, title_budget),
                theme.title(true),
            )));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if show_close_pill {
            let close_x = rect.right().saturating_sub(close_w + 1);
            let close_rect = Rect::new(close_x, rect.y, close_w, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    close_label,
                    Style::default().fg(theme.fg).bg(theme.bg_elevated),
                )),
                close_rect,
            );
            hitmap.add_line(
                close_x,
                rect.y,
                close_w,
                HitTarget::Button(ButtonId::ConfirmCancel),
            );
        }

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(Span::styled(err.clone(), Style::default().fg(theme.error))),
                inner,
            );
            return;
        }

        let Some(target) = self.current() else {
            return;
        };

        let term_w = ui.term_width.max(area.width);
        let term_h = ui.term_height.max(area.height);
        let (view_cols, view_rows) = Self::viewport_dimensions(term_w, term_h);
        let cache_key = Self::cache_key(
            target,
            view_cols,
            view_rows,
            self.zoom_pct,
            app.graphics_caps.cell_px,
            proto,
        );

        if let Some(rendered) = app.preview_cache.get(&cache_key) {
            let is_pixel =
                rendered.pixel_payload.is_some() && proto != TerminalImageProtocol::Halfblock;
            let total_rows = if is_pixel {
                rendered.rows
            } else {
                rendered.lines.len() as u16
            };
            let total_cols = rendered.cols;

            let avail_h = if is_pixel
                && !app.graphics_caps.cell_px_exact
                && proto == TerminalImageProtocol::Sixel
            {
                inner.height.saturating_sub(1).max(1)
            } else {
                inner.height
            };
            let show_scrollbar = total_rows > avail_h && inner.width > 2;
            let avail_w = if show_scrollbar {
                inner.width.saturating_sub(1)
            } else {
                inner.width
            };

            let max_scroll_y = total_rows.saturating_sub(avail_h);
            let max_scroll_x = total_cols.saturating_sub(avail_w);
            self.scroll_y = self.scroll_y.min(max_scroll_y);
            self.scroll_x = self.scroll_x.min(max_scroll_x);
            let sy = self.scroll_y;
            let sx = self.scroll_x;

            if is_pixel {
                let draw_w = total_cols.saturating_sub(sx).min(avail_w);
                let draw_h = total_rows.saturating_sub(sy).min(avail_h);
                let px_x = inner.x + avail_w.saturating_sub(draw_w) / 2;
                let px_y = inner.y + avail_h.saturating_sub(draw_h) / 2;
                ui.pending_pixel = Some(PendingPixelGraphic {
                    cache_key: format!("{cache_key}#crop:{sx},{sy},{avail_w},{avail_h}"),
                    rect: Rect::new(px_x, px_y, draw_w, draw_h),
                    protocol: proto,
                });
            } else {
                let view_w = total_cols.saturating_sub(sx).min(avail_w);
                let view_h = total_rows.saturating_sub(sy).min(avail_h);
                let pad_x = avail_w.saturating_sub(view_w) / 2;
                let pad_y = avail_h.saturating_sub(view_h) / 2;
                let draw_rect = Rect::new(inner.x + pad_x, inner.y + pad_y, view_w, view_h);
                frame.render_widget(
                    Paragraph::new(rendered.lines.clone()).scroll((sy, sx)),
                    draw_rect,
                );
            }

            if show_scrollbar {
                let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
                let mut sb_state = ScrollbarState::new(max_scroll_y.saturating_add(1) as usize)
                    .position(sy as usize);
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(theme.accent))
                    .track_style(theme.muted_style());
                frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::{
        config::AppConfig,
        img_preview::{decode_and_render_for_protocol, TerminalGraphicsCaps},
        models::{NoteDetail, NoteImageRef, NoteSummary, ParsedNote},
        storage::Storage,
        sync::{FocusTarget, SyncCommand, SyncController},
        vault::vault_from_dir,
    };

    fn make_tall_png(w: u32, h: u32) -> Vec<u8> {
        let img: RgbaImage = ImageBuffer::from_fn(w, h, |x, y| {
            Rgba([
                ((x * 255) / w.max(1)) as u8,
                ((y * 255) / h.max(1)) as u8,
                160,
                255,
            ])
        });
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[test]
    fn test_image_preview_tall_scroll_clamps_scrollbar_and_close_pill() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = vault_from_dir(tmp.path(), Some("TestVault"));
        let cfg = AppConfig::default();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

        let png = make_tall_png(400, 2000);
        let img_path = tmp.path().join("tall.png");
        let second_path = tmp.path().join("second.png");
        std::fs::write(&img_path, &png).unwrap();
        std::fs::write(&second_path, &png).unwrap();

        let detail = NoteDetail {
            summary: NoteSummary {
                rel_path: "Note.md".into(),
                title: "Note".into(),
                ..Default::default()
            },
            raw_markdown: String::new(),
            parsed: ParsedNote::default(),
            backlinks: Vec::new(),
            outgoing_links: Vec::new(),
            images: vec![
                NoteImageRef {
                    raw_target: "tall.png".into(),
                    display_name: "tall.png".into(),
                    mime_type: "image/png".into(),
                    abs_path: img_path.clone(),
                    line_no: 1,
                },
                NoteImageRef {
                    raw_target: "second.png".into(),
                    display_name: "second.png".into(),
                    mime_type: "image/png".into(),
                    abs_path: second_path.clone(),
                    line_no: 2,
                },
            ],
        };

        let area = Rect::new(0, 0, 100, 30);

        for proto in [
            TerminalImageProtocol::Halfblock,
            TerminalImageProtocol::Sixel,
        ] {
            let mut app = AppState::new(
                cfg.clone(),
                vault.clone(),
                Storage::open_in_memory().unwrap(),
                sync_ctrl.clone(),
            );
            app.graphics_caps = TerminalGraphicsCaps {
                protocol: proto,
                cell_px: (8, 16),
                cell_px_exact: true,
            };
            let mut ui = UiState::from_config(&cfg);
            ui.term_width = 100;
            ui.term_height = 30;

            let mut overlay = ImagePreviewOverlay::from_note(&detail, Some(0)).unwrap();
            let (view_cols, view_rows) =
                ImagePreviewOverlay::viewport_dimensions(ui.term_width, ui.term_height);
            let fit = overlay.fit_box(ui.term_width, ui.term_height);
            let rendered =
                decode_and_render_for_protocol(&png, fit, (19, 19, 24), app.graphics_caps).unwrap();
            let total_rows = if proto == TerminalImageProtocol::Halfblock {
                rendered.lines.len() as u16
            } else {
                rendered.rows
            };
            let rect = ImagePreviewOverlay::modal_rect(area);
            let inner_h = rect.height.saturating_sub(2);
            let expected_max_scroll = total_rows.saturating_sub(inner_h);
            assert!(expected_max_scroll > 0);

            let base_key = ImagePreviewOverlay::cache_key(
                overlay.current().unwrap(),
                view_cols,
                view_rows,
                overlay.zoom_pct,
                app.graphics_caps.cell_px,
                proto,
            );
            app.preview_cache.insert(base_key.clone(), rendered);

            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            let mut hitmap = HitMap::new();

            terminal
                .draw(|frame| {
                    ui.pending_pixel = None;
                    hitmap.clear();
                    overlay.draw(frame, area, &app, &mut ui, &mut hitmap);
                })
                .unwrap();

            // Clickable [✕ Esc] close pill registered on top-right border
            let close_region = hitmap
                .region_of(HitTarget::Button(ButtonId::ConfirmCancel))
                .expect("expected [✕ Esc] close pill");
            assert_eq!(close_region.y, rect.y);

            // Scrollbar rendered in right-most inner column
            let sb_x = rect.right() - 2;
            let buf = terminal.backend().buffer();
            assert!(
                (rect.y + 1..rect.bottom() - 1).any(|y| !buf[(sb_x, y)].symbol().trim().is_empty())
            );

            if proto == TerminalImageProtocol::Sixel {
                let pg0 = ui
                    .pending_pixel
                    .clone()
                    .expect("expected pending pixel graphic");
                assert!(pg0.cache_key.starts_with(&format!("{base_key}#crop:0,0,")));
            }

            // Scroll down via j / wheel / End -> clamps to exact max_scroll_y
            assert_eq!(overlay.handle(&Action::Input('j')), OverlayOutcome::Handled);
            assert_eq!(
                overlay.handle(&Action::MouseScrollDown { col: 50, row: 15 }),
                OverlayOutcome::Handled
            );
            assert_eq!(overlay.handle(&Action::Input('G')), OverlayOutcome::Handled);
            assert_eq!(overlay.scroll_y, u16::MAX);

            terminal
                .draw(|frame| {
                    ui.pending_pixel = None;
                    hitmap.clear();
                    overlay.draw(frame, area, &app, &mut ui, &mut hitmap);
                })
                .unwrap();
            assert_eq!(overlay.scroll_y, expected_max_scroll);

            // Immediate upward response via k / wheel up
            overlay.handle(&Action::MouseScrollUp { col: 50, row: 15 });
            assert_eq!(overlay.scroll_y, expected_max_scroll.saturating_sub(3));
            overlay.handle(&Action::Input('k'));
            assert_eq!(overlay.scroll_y, expected_max_scroll.saturating_sub(5));

            // Next image resets scroll
            overlay.handle(&Action::CursorRight);
            assert_eq!(overlay.index, 1);
            assert_eq!(overlay.scroll_y, 0);
        }
        drop(storage);
    }
}
