//! Inline/modal image preview caching and interactive HTML preview / native HTTP serving.

use std::fs;

use bitefoundry_html::{sniff_content_kind, ContentKind};

use crate::{
    config::PageWidthMode,
    img_preview::{
        color_to_rgb, decode_and_render_for_protocol, decode_and_render_halfblocks_with_cell_px,
        FitBox,
    },
    models::{NoteDetail, NoteImageRef},
    ui::{
        overlays::{
            html_preview::{extract_note_html_candidates, HtmlPreviewOverlay, HtmlPreviewSource},
            image_preview::ImagePreviewOverlay,
            Overlay,
        },
        sidebar::{build_sidebar_entries, SidebarEntry},
    },
    vault::fs::{inline_image_cache_key, resolve_attachment_path, safe_vault_path},
};

use super::{AppState, Focus, UiState};

/// Builds a theme- and cell-pixel-aware cache key for inline halfblock note previews.
pub fn inline_preview_cache_key(
    abs_path: &std::path::Path,
    bg_rgb: (u8, u8, u8),
    cell_px: (u16, u16),
) -> String {
    format!(
        "{}:{}_{}_{}:{}x{}",
        inline_image_cache_key(abs_path),
        bg_rgb.0,
        bg_rgb.1,
        bg_rgb.2,
        cell_px.0,
        cell_px.1
    )
}


impl AppState {
    fn inline_image_content_width(ui: &UiState) -> u16 {
        let show_left = ui.show_sidebar && ui.term_width >= 60;
        let show_right = ui.show_inspector && ui.term_width >= 88;
        let left_w = if show_left {
            (ui.term_width / 5).clamp(22, 30)
        } else {
            0
        };
        let right_w = if show_right {
            (ui.term_width / 4).clamp(26, 34)
        } else {
            0
        };
        let canvas_w = ui.term_width.saturating_sub(left_w + right_w).max(24);
        match ui.page_width {
            PageWidthMode::Centered if canvas_w > 92 => 86,
            _ => canvas_w.saturating_sub(2),
        }
    }

    pub(super) fn cache_inline_images(&mut self, detail: &NoteDetail, ui: &UiState) {
        self.cache_inline_images_list(&detail.images, ui);
    }

    pub(crate) fn cache_inline_images_list(&mut self, images: &[NoteImageRef], ui: &UiState) {
        if !self.cfg.inline_images {
            return;
        }
        let bg_rgb = color_to_rgb(ui.theme.bg_canvas, (19, 19, 24));
        let width = Self::inline_image_content_width(ui);
        let thumb_cols = width.saturating_sub(4).clamp(24, 72);
        let thumb_rows = 16u16;
        let fit = FitBox::fit_to_box(thumb_cols, thumb_rows);

        for img in images {
            let key = inline_preview_cache_key(&img.abs_path, bg_rgb, self.graphics_caps.cell_px);
            if self.preview_cache.contains_key(&key) {
                continue;
            }
            let prefix = format!("inline:{}:", img.abs_path.to_string_lossy());
            self.preview_cache.retain(|k, _| !k.starts_with(&prefix));
            if let Ok(bytes) = fs::read(&img.abs_path) {
                if !matches!(
                    sniff_content_kind(Some(&img.mime_type), Some(&img.display_name), Some(&bytes)),
                    ContentKind::RasterImage | ContentKind::Svg
                ) {
                    continue;
                }
                if let Ok(rendered) = decode_and_render_halfblocks_with_cell_px(
                    &bytes,
                    fit,
                    bg_rgb,
                    self.graphics_caps.cell_px,
                ) {
                    self.preview_cache.insert(key, rendered);
                    self.bump_data_gen();
                }
            }
        }
    }

    /// Ensures the active `ImagePreview` modal target is decoded and cached.
    ///
    /// If the attachment bytes sniff as `ContentKind::Html`, returns an `HtmlPreviewOverlay`
    /// so the caller automatically routes to `Overlay::HtmlPreview` instead of failing image decode.
    pub fn ensure_modal_image_cached(
        &mut self,
        ov: &mut ImagePreviewOverlay,
        term_w: u16,
        term_h: u16,
        bg_rgb: (u8, u8, u8),
    ) -> Option<HtmlPreviewOverlay> {
        let target = ov.current()?.clone();
        let (view_cols, view_rows) = ImagePreviewOverlay::viewport_dimensions(term_w, term_h);
        let key = ImagePreviewOverlay::cache_key(
            &target,
            view_cols,
            view_rows,
            ov.zoom_pct,
            self.graphics_caps.cell_px,
            self.graphics_caps.protocol,
        );
        if self.preview_cache.contains_key(&key) {
            return None;
        }
        match fs::read(&target.abs_path) {
            Ok(bytes) => {
                if sniff_content_kind(
                    Some(&target.mime_type),
                    Some(&target.display_name),
                    Some(&bytes),
                ) == ContentKind::Html
                {
                    let html = String::from_utf8_lossy(&bytes).into_owned();
                    let src = target.abs_path.display().to_string();
                    return Some(
                        HtmlPreviewOverlay::new(target.display_name.clone(), src, html)
                            .with_source(HtmlPreviewSource::File(target.abs_path.clone())),
                    );
                }
                let fit = FitBox::modal(view_cols, view_rows, ov.zoom_pct);
                match decode_and_render_for_protocol(&bytes, fit, bg_rgb, self.graphics_caps) {
                    Ok(rendered) => {
                        let prefix = format!("modal:{}:", target.abs_path.to_string_lossy());
                        self.preview_cache.retain(|k, _| !k.starts_with(&prefix));
                        self.preview_cache.insert(key, rendered);
                        ov.error = None;
                    }
                    Err(e) => ov.error = Some(format!("Failed to render image: {e}")),
                }
            }
            Err(e) => ov.error = Some(format!("Failed to read image file: {e}")),
        }
        None
    }

    /// Prepares the active overlay prior to drawing (including HTML-vs-Image auto-routing).
    pub fn prepare_active_overlay(&mut self, ui: &mut UiState, term_w: u16, term_h: u16) {
        let bg_rgb = color_to_rgb(ui.theme.bg_elevated, (19, 19, 24));
        let reroute = if let Some(Overlay::ImagePreview(ref mut ov)) = ui.overlay {
            self.ensure_modal_image_cached(ov, term_w, term_h, bg_rgb)
        } else {
            None
        };
        if let Some(html_ov) = reroute {
            let name = html_ov.title.clone();
            ui.open_overlay(Overlay::HtmlPreview(html_ov));
            ui.toasts.info(format!(
                "Detected HTML in '{name}' — routed to interactive HTML preview"
            ));
        }
    }

    /// Lazily serves an HTML document via `NativeHtmlServer` and optionally opens it in the default browser.
    pub async fn serve_html_and_open(
        &mut self,
        title: &str,
        html: &str,
        ui: &mut UiState,
        open_browser: bool,
    ) -> anyhow::Result<String> {
        let url = self.html_server.serve_html(title, html).await?;
        self.last_served_html_url = Some(url.clone());
        if let Some(Overlay::HtmlPreview(ref mut ov)) = ui.overlay {
            ov.session.last_status = Some(format!("Served at {url}"));
        }
        if open_browser {
            match open::that(&url) {
                Ok(()) => ui.toasts.success(format!("Opened in browser: {url}")),
                Err(e) => ui
                    .toasts
                    .warning(format!("Served at {url} (browser failed: {e})")),
            }
        } else {
            ui.toasts.success(format!("Served HTML at {url}"));
        }
        Ok(url)
    }

    /// Opens a vault `.html`/`.htm`/`.xhtml` file in `Overlay::HtmlPreview`.
    pub fn open_html_file(&mut self, ui: &mut UiState, rel_or_target: &str) -> bool {
        let resolved = safe_vault_path(&self.active_vault.path, rel_or_target)
            .ok()
            .filter(|p| p.is_file())
            .or_else(|| {
                let note_rel = self
                    .active_note
                    .as_ref()
                    .map(|d| d.summary.rel_path.as_str())
                    .unwrap_or("");
                resolve_attachment_path(&self.active_vault.path, note_rel, rel_or_target)
            });

        let Some(abs_path) = resolved else {
            ui.toasts
                .warning(format!("HTML file not found: {rel_or_target}"));
            return false;
        };

        match fs::read(&abs_path) {
            Ok(bytes) => {
                let html = String::from_utf8_lossy(&bytes).into_owned();
                let title = abs_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(rel_or_target)
                    .to_string();
                let source = abs_path.display().to_string();
                ui.open_overlay(Overlay::HtmlPreview(
                    HtmlPreviewOverlay::new(title, source, html)
                        .with_source(HtmlPreviewSource::File(abs_path)),
                ));
                true
            }
            Err(e) => {
                ui.toasts.error(format!("Failed to read HTML file: {e}"));
                false
            }
        }
    }

    /// Opens `Overlay::HtmlPreview` for the selected HTML block, active note HTML, or vault `.html` file.
    pub fn open_html_preview(&mut self, ui: &mut UiState, preferred_block: Option<usize>) {
        // 1. If sidebar is focused on a vault HTML file and no specific block was requested, open it.
        if preferred_block.is_none() && ui.focus == Focus::Sidebar {
            let entries = build_sidebar_entries(self, ui);
            if let Some(SidebarEntry::HtmlFile { rel_path, .. }) = entries.get(ui.sidebar_selected)
            {
                let rp = rel_path.clone();
                self.open_html_file(ui, &rp);
                return;
            }
        }

        // 2. Check active note for HTML code blocks, embedded raw HTML, or linked `.html` files.
        if let Some(detail) = self.active_note.as_ref() {
            let candidates =
                extract_note_html_candidates(detail, Some(self.active_vault.path.as_path()));
            if !candidates.is_empty() {
                let target_idx = preferred_block.unwrap_or(ui.canvas_selected_block);
                let chosen = candidates
                    .iter()
                    .find(|c| c.block_idx == Some(target_idx))
                    .unwrap_or(&candidates[0]);
                let preview_source = if let Some(abs) = chosen.file_path.clone() {
                    HtmlPreviewSource::File(abs)
                } else if let Some(b_idx) = chosen.block_idx {
                    HtmlPreviewSource::NoteBlock {
                        note_rel_path: detail.summary.rel_path.clone(),
                        block_idx: b_idx,
                    }
                } else {
                    HtmlPreviewSource::Inline
                };
                ui.open_overlay(Overlay::HtmlPreview(
                    HtmlPreviewOverlay::new(
                        chosen.title.clone(),
                        chosen.source_label.clone(),
                        chosen.html.clone(),
                    )
                    .with_source(preview_source),
                ));
                return;
            }
        }

        // 3. Fall back to the first `.html`/`.htm`/`.xhtml` file in the vault.
        if let Some(first_html) = self.html_files.first().cloned() {
            self.open_html_file(ui, &first_html.rel_path);
            return;
        }

        ui.toasts
            .info("No HTML blocks or .html files found in active note or vault");
    }

}
