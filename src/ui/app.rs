//! Core TUI application state (`AppState`), presentation state (`UiState`), and action dispatcher.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
};

use bitefoundry_html::{sniff_content_kind, ContentKind, NativeHtmlServer};
use chrono::Local;

use crate::{
    config::{write_atomic, AppConfig, PageWidthMode, VaultProfile},
    img_preview::{
        color_to_rgb, decode_and_render_for_protocol, decode_and_render_halfblocks_with_cell_px,
        detect_graphics_caps, FitBox, RenderedTerminalImage, TerminalGraphicsCaps,
        TerminalImageProtocol,
    },
    models::{
        CodedError, CoreEvent, GraphData, IndexStats, LinkStatus, NoteBlock, NoteDetail,
        NoteImageRef, NoteSummary, OutgoingLinkEntry, SidebarTab, TagCount, TaskEntry, TaskFilter,
    },
    storage::Storage,
    sync::SyncController,
    ui::{
        action::Action,
        composer::read_clipboard_image_png,
        graph_view::GraphSim,
        hitmap::{ButtonId, HitMap, HitTarget},
        inline_editor::{ExternalConflict, InlineEditEvent, InlineNoteEditor},
        keymap::KeyMap,
        overlays::{
            confirm::ConfirmOverlay,
            context_menu::ContextMenuOverlay,
            file_picker::FilePickerOverlay,
            html_preview::{extract_note_html_candidates, HtmlPreviewOverlay, HtmlPreviewSource},
            image_preview::ImagePreviewOverlay,
            links::LinkExtractorOverlay,
            note_form::NoteFormOverlay,
            quick_switcher::QuickSwitcherOverlay,
            settings::SettingsOverlay,
            vault_switcher::VaultSwitcherOverlay,
            Overlay, OverlayKind, OverlayOutcome,
        },
        sidebar::{build_sidebar_entries, SidebarEntry},
        theme::Theme,
        toast::ToastQueue,
    },
    vault::{
        fs::{
            append_to_note, content_sha256_bytes, create_note, ensure_daily_note,
            extract_local_markdown_note_link, file_mtime_and_size, hydrate_parsed_note_links,
            inline_image_cache_key, is_html_file_path, move_note_to_trash, resolve_attachment_path,
            resolve_markdown_link_from_source, resolve_wikilink_from_source, safe_vault_path,
            save_vault_attachment, scan_vault_html_files, toggle_task_at_line_verified,
            VaultHtmlFile,
        },
        merged_vaults,
        parser::parse_markdown,
    },
};

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

/// Why a note is being opened. Decides whether a target that does not exist may be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenIntent {
    /// Opening something expected to exist: sidebar, history, quick switcher, backlinks.
    Navigate,
    /// Following a wikilink or Markdown link, where a missing target is created on demand.
    FollowLink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Canvas,
    Inspector,
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasMode {
    Note,
    Graph,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPixelGraphic {
    pub cache_key: String,
    pub rect: ratatui::layout::Rect,
    pub protocol: TerminalImageProtocol,
    /// Optional pre-encoded terminal graphics payload (used by `HtmlPreviewOverlay` inline graphics).
    pub payload: Option<String>,
}

enum EditorDiskState {
    Unchanged,
    Modified { text: String, hash: [u8; 32] },
    Deleted,
    ReadError(String),
}

pub struct AppState {
    pub cfg: AppConfig,
    pub vaults: Vec<VaultProfile>,
    pub active_vault: VaultProfile,
    pub storage: Storage,
    pub sync_ctrl: SyncController,
    pub notes: Vec<NoteSummary>,
    pub html_files: Vec<VaultHtmlFile>,
    pub tags: Vec<TagCount>,
    pub tasks: Vec<TaskEntry>,
    pub active_note: Option<NoteDetail>,
    pub graph: GraphData,
    pub history: Vec<String>,
    pub preview_cache: HashMap<String, RenderedTerminalImage>,
    pub graphics_caps: TerminalGraphicsCaps,
    pub html_server: NativeHtmlServer,
    pub pending_html_serve: Option<(String, String)>,
    pub last_served_html_url: Option<String>,
    pub last_index_stats: Option<IndexStats>,
    pub pending_external_editor: Option<PathBuf>,
}

pub struct UiState {
    pub theme: Theme,
    pub focus: Focus,
    pub prev_focus: Focus,
    pub canvas_mode: CanvasMode,
    pub show_sidebar: bool,
    pub show_inspector: bool,
    pub page_width: PageWidthMode,
    pub show_inline_hrefs: bool,
    pub sidebar_tab: SidebarTab,
    pub sidebar_selected: usize,
    pub collapsed_folders: HashSet<String>,
    pub active_tag_filter: Option<String>,
    pub canvas_selected_block: usize,
    pub canvas_scroll: usize,
    pub canvas_follow_selected: bool,
    pub graph_global: bool,
    pub graph_depth: usize,
    pub graph_selected: usize,
    pub graph_sim: GraphSim,
    pub inspector_selected: usize,
    pub inline_editor: Option<InlineNoteEditor>,
    pub overlay: Option<Overlay>,
    pub toasts: ToastQueue,
    pub keymap: KeyMap,
    pub pending_pixel_graphics: Vec<PendingPixelGraphic>,
    pub cursor_pos: Option<(u16, u16)>,
    pub term_width: u16,
    pub term_height: u16,
    pub should_quit: bool,
    pub last_hitmap: HitMap,
    pub last_note_content_area: ratatui::layout::Rect,
}

impl UiState {
    pub fn from_config(cfg: &AppConfig) -> Self {
        Self {
            theme: Theme::detect(cfg.theme),
            focus: Focus::Canvas,
            prev_focus: Focus::Canvas,
            canvas_mode: CanvasMode::Note,
            show_sidebar: cfg.show_sidebar,
            show_inspector: cfg.show_inspector,
            page_width: cfg.page_width,
            show_inline_hrefs: cfg.show_inline_hrefs,
            sidebar_tab: SidebarTab::Files,
            sidebar_selected: 0,
            collapsed_folders: HashSet::new(),
            active_tag_filter: None,
            canvas_selected_block: 0,
            canvas_scroll: 0,
            canvas_follow_selected: true,
            graph_global: false,
            graph_depth: 2,
            graph_selected: 0,
            graph_sim: GraphSim::default(),
            inspector_selected: 0,
            inline_editor: None,
            overlay: None,
            toasts: ToastQueue::new(),
            keymap: KeyMap,
            pending_pixel_graphics: Vec::new(),
            cursor_pos: None,
            term_width: 120,
            term_height: 34,
            should_quit: false,
            last_hitmap: HitMap::new(),
            last_note_content_area: ratatui::layout::Rect::default(),
        }
    }

    pub fn open_overlay(&mut self, overlay: Overlay) {
        if self.focus != Focus::Overlay {
            self.prev_focus = self.focus;
        }
        self.overlay = Some(overlay);
        self.focus = Focus::Overlay;
    }

    pub fn close_overlay(&mut self) {
        self.overlay = None;
        self.focus = if self.prev_focus == Focus::Overlay {
            Focus::Canvas
        } else {
            self.prev_focus
        };
    }

    pub fn is_text_input_active(&self) -> bool {
        self.overlay
            .as_ref()
            .map(|o| o.is_text_input())
            .unwrap_or_else(|| self.inline_editor.is_some())
    }
}

impl AppState {
    pub fn new(
        cfg: AppConfig,
        active_vault: VaultProfile,
        storage: Storage,
        sync_ctrl: SyncController,
    ) -> Self {
        let mut vaults = merged_vaults(&cfg);
        if !vaults.iter().any(|v| v.id == active_vault.id) {
            vaults.insert(0, active_vault.clone());
        }
        let graphics_caps = detect_graphics_caps(cfg.image_protocol);
        Self {
            cfg,
            vaults,
            active_vault,
            storage,
            sync_ctrl,
            notes: Vec::new(),
            html_files: Vec::new(),
            tags: Vec::new(),
            tasks: Vec::new(),
            active_note: None,
            graph: GraphData::default(),
            history: Vec::new(),
            preview_cache: HashMap::new(),
            graphics_caps,
            html_server: NativeHtmlServer::new(),
            pending_html_serve: None,
            last_served_html_url: None,
            last_index_stats: None,
            pending_external_editor: None,
        }
    }

    fn inspect_editor_disk_state(&self, note_path: &str, base_hash: [u8; 32]) -> EditorDiskState {
        let abs = match safe_vault_path(&self.active_vault.path, note_path) {
            Ok(p) => p,
            Err(e) => return EditorDiskState::ReadError(e.to_string()),
        };
        match fs::read(&abs) {
            Ok(bytes) => {
                let hash = content_sha256_bytes(&bytes);
                if hash == base_hash {
                    EditorDiskState::Unchanged
                } else {
                    let text = String::from_utf8(bytes)
                        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                    EditorDiskState::Modified { text, hash }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => EditorDiskState::Deleted,
            Err(e) => EditorDiskState::ReadError(e.to_string()),
        }
    }

    fn refresh_vault_lists(&mut self) {
        let vid = self.active_vault.id.clone();
        self.notes = self.storage.list_notes(&vid, None).unwrap_or_default();
        self.tags = self.storage.list_tags(&vid).unwrap_or_default();
        self.tasks = self
            .storage
            .list_tasks(&vid, TaskFilter::All, None)
            .unwrap_or_default();
    }

    /// Refreshes in-memory vault note list, HTML files, tags, tasks, active note detail, and (when visible) graph.
    pub fn reload_from_storage(&mut self, ui: &mut UiState) {
        self.reload_from_storage_inner(ui, true);
    }

    /// Refreshes in-memory vault state from SQLite after a Markdown-only mutation without walking the
    /// filesystem for `.html` files on the UI thread.
    pub(crate) fn reload_from_storage_without_html_scan(&mut self, ui: &mut UiState) {
        self.reload_from_storage_inner(ui, false);
    }

    fn reload_from_storage_inner(&mut self, ui: &mut UiState, refresh_html: bool) {
        let vid = self.active_vault.id.clone();
        self.refresh_vault_lists();
        if refresh_html {
            self.html_files = scan_vault_html_files(&self.active_vault.path).unwrap_or_default();
        }

        if let Some((ed_path, ed_base_hash, ed_dirty, prev_conflict)) =
            ui.inline_editor.as_ref().map(|e| {
                (
                    e.note_path.clone(),
                    e.base_content_hash,
                    e.dirty,
                    e.external_conflict.clone(),
                )
            })
        {
            let stored_bookmarked = self
                .notes
                .iter()
                .find(|n| n.rel_path == ed_path)
                .map(|n| n.is_bookmarked);
            match self.inspect_editor_disk_state(&ed_path, ed_base_hash) {
                EditorDiskState::Unchanged => {
                    if ed_dirty {
                        self.apply_live_inline_parse(ui);
                    } else if let Ok(detail) =
                        self.storage.load_note_detail(&self.active_vault, &ed_path)
                    {
                        self.cache_inline_images(&detail, ui);
                        self.active_note = Some(detail);
                    }
                }
                EditorDiskState::Modified { text, hash } => {
                    if !ed_dirty {
                        if let Some(ref mut ed) = ui.inline_editor {
                            ed.reload_from_disk(&text, hash);
                        }
                        if let Ok(detail) =
                            self.storage.load_note_detail(&self.active_vault, &ed_path)
                        {
                            self.cache_inline_images(&detail, ui);
                            self.active_note = Some(detail);
                        } else {
                            self.apply_live_inline_parse(ui);
                        }
                        ui.toasts.info("Reloaded externally modified note");
                    } else {
                        if let Some(ref mut ed) = ui.inline_editor {
                            ed.mark_conflict(ExternalConflict::Modified);
                        }
                        self.apply_live_inline_parse(ui);
                        if prev_conflict != Some(ExternalConflict::Modified) {
                            ui.toasts.warning(
                                "External change detected — autosave paused; local edits preserved",
                            );
                        }
                    }
                }
                EditorDiskState::Deleted => {
                    if let Some(ref mut ed) = ui.inline_editor {
                        ed.mark_conflict(ExternalConflict::Deleted);
                    }
                    self.apply_live_inline_parse(ui);
                    if prev_conflict != Some(ExternalConflict::Deleted) {
                        ui.toasts.warning(
                            "File deleted or moved externally — autosave paused; old path will not be recreated",
                        );
                    }
                }
                EditorDiskState::ReadError(_) => {
                    if ed_dirty {
                        self.apply_live_inline_parse(ui);
                    }
                }
            }
            // Keep backlinks & bookmark status fresh even when inline_editor is dirty.
            if let Some(detail) = self.active_note.as_mut() {
                if detail.summary.rel_path == ed_path {
                    if let Some(bm) = stored_bookmarked {
                        detail.summary.is_bookmarked = bm;
                    }
                    if let Ok(bl) = self.storage.get_backlinks(&vid, &ed_path) {
                        detail.summary.backlink_count = bl.len();
                        detail.backlinks = bl;
                    }
                    let updated_summary = detail.summary.clone();
                    if let Some(slot) = self.notes.iter_mut().find(|n| n.rel_path == ed_path) {
                        *slot = updated_summary;
                    }
                }
            }
            if ui.canvas_mode == CanvasMode::Graph {
                self.refresh_graph(ui);
            }
            self.refresh_open_html_preview(ui);
            self.clamp_selections(ui);
            return;
        }

        let current_path = self
            .active_note
            .as_ref()
            .map(|d| d.summary.rel_path.clone())
            .or_else(|| self.notes.first().map(|n| n.rel_path.clone()));

        if let Some(rp) = current_path {
            if let Ok(detail) = self.storage.load_note_detail(&self.active_vault, &rp) {
                self.cache_inline_images(&detail, ui);
                self.active_note = Some(detail);
            } else if let Some(first) = self.notes.first() {
                let first_rp = first.rel_path.clone();
                if let Ok(detail) = self.storage.load_note_detail(&self.active_vault, &first_rp) {
                    self.cache_inline_images(&detail, ui);
                    self.active_note = Some(detail);
                } else {
                    self.active_note = None;
                }
                ui.canvas_scroll = 0;
                ui.canvas_selected_block = 0;
                ui.canvas_follow_selected = true;
                ui.inspector_selected = 0;
            } else {
                self.active_note = None;
                ui.canvas_scroll = 0;
                ui.canvas_selected_block = 0;
                ui.canvas_follow_selected = true;
                ui.inspector_selected = 0;
            }
        } else {
            self.active_note = None;
            ui.canvas_scroll = 0;
            ui.canvas_selected_block = 0;
            ui.canvas_follow_selected = true;
            ui.inspector_selected = 0;
        }
        if ui.canvas_mode == CanvasMode::Graph {
            self.refresh_graph(ui);
        }
        self.refresh_open_html_preview(ui);
        self.clamp_selections(ui);
    }

    fn refresh_open_html_preview(&self, ui: &mut UiState) {
        let Some(Overlay::HtmlPreview(ref mut ov)) = ui.overlay else {
            return;
        };
        match ov.source.clone() {
            HtmlPreviewSource::File(abs_path) => match fs::read(&abs_path) {
                Ok(bytes) => {
                    let html = String::from_utf8_lossy(&bytes).into_owned();
                    ov.reload_html_if_changed(html);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    ov.session.last_status = Some("HTML file deleted or moved on disk".to_string());
                }
                Err(_) => {}
            },
            HtmlPreviewSource::NoteBlock {
                note_rel_path,
                block_idx,
            } => {
                if let Some(detail) = self.active_note.as_ref() {
                    if detail.summary.rel_path.eq_ignore_ascii_case(&note_rel_path) {
                        let candidates = extract_note_html_candidates(
                            detail,
                            Some(self.active_vault.path.as_path()),
                        );
                        if let Some(cand) = candidates
                            .iter()
                            .find(|c| c.block_idx == Some(block_idx))
                            .or_else(|| candidates.first())
                        {
                            ov.reload_html_if_changed(cand.html.clone());
                        }
                    }
                }
            }
            HtmlPreviewSource::Inline => {}
        }
    }

    fn inspector_item_count(detail: &NoteDetail) -> usize {
        detail.backlinks.len() + detail.outgoing_links.len() + detail.parsed.headings.len()
    }

    pub fn clamp_selections(&self, ui: &mut UiState) {
        let sidebar_len = build_sidebar_entries(self, ui).len();
        ui.sidebar_selected = ui.sidebar_selected.min(sidebar_len.saturating_sub(1));

        let graph_len = self.graph.nodes.len();
        ui.graph_selected = ui.graph_selected.min(graph_len.saturating_sub(1));

        if let Some(detail) = self.active_note.as_ref() {
            let blocks_len = detail.parsed.blocks.len();
            ui.canvas_selected_block = ui.canvas_selected_block.min(blocks_len.saturating_sub(1));

            let inspector_len = Self::inspector_item_count(detail);
            ui.inspector_selected = ui.inspector_selected.min(inspector_len.saturating_sub(1));
        } else {
            ui.canvas_selected_block = 0;
            ui.canvas_scroll = 0;
            ui.inspector_selected = 0;
        }
    }

    pub fn refresh_graph(&mut self, ui: &mut UiState) {
        let center = if ui.graph_global {
            None
        } else {
            self.active_note
                .as_ref()
                .map(|d| d.summary.rel_path.clone())
        };
        self.graph = self
            .storage
            .get_graph(&self.active_vault, center.as_deref(), ui.graph_depth)
            .unwrap_or_default();
        ui.graph_sim.sync_from_graph(&self.graph);
        self.clamp_selections(ui);
    }

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

    fn cache_inline_images(&mut self, detail: &NoteDetail, ui: &UiState) {
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

    /// Opens an existing note. A reference that does not resolve is reported, never created,
    /// unless it is an `unresolved:<name>` graph node (see [`AppState::open_note_with`]).
    pub fn open_note(&mut self, ui: &mut UiState, note_ref: &str, push_history: bool) {
        self.open_note_with(ui, note_ref, push_history, OpenIntent::Navigate);
    }

    /// Opens a note, creating it only when that is what the user asked for: the reference is
    /// not found, `push_history` is set (a forward navigation), and the target is an unresolved
    /// link, either an `unresolved:<name>` graph node or a link followed with
    /// [`OpenIntent::FollowLink`]. Ambiguous references and read errors never create a file.
    pub fn open_note_with(
        &mut self,
        ui: &mut UiState,
        note_ref: &str,
        push_history: bool,
        intent: OpenIntent,
    ) {
        // Route `.html`/`.htm`/`.xhtml` targets directly into `Overlay::HtmlPreview`.
        if is_html_file_path(note_ref) {
            self.open_html_file(ui, note_ref);
            return;
        }
        // Flush any pending inline auto-save cleanly before switching notes
        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
            return;
        }
        // `unresolved:<name>` is the graph's node ID for a link target that has no note yet.
        let (target, creation_target) = match note_ref.strip_prefix("unresolved:") {
            Some(name) => (name, true),
            None => (note_ref, intent == OpenIntent::FollowLink),
        };
        // The note to return to. It joins the history only once something was opened.
        let previous = self
            .active_note
            .as_ref()
            .map(|d| d.summary.rel_path.clone())
            .filter(|cur| push_history && !cur.eq_ignore_ascii_case(target));
        match self.storage.load_note_detail(&self.active_vault, target) {
            Ok(detail) => {
                self.push_history_entry(previous);
                self.cache_inline_images(&detail, ui);
                self.active_note = Some(detail);
                ui.canvas_mode = CanvasMode::Note;
                ui.canvas_selected_block = 0;
                ui.canvas_scroll = 0;
                ui.canvas_follow_selected = true;
                ui.inspector_selected = 0;
                self.clamp_selections(ui);
            }
            Err(err) => {
                let code = err.downcast_ref::<CodedError>().map(|coded| coded.code);
                match code {
                    // Create note on demand if user followed an unresolved [[wikilink]]
                    Some("not_found") if push_history && creation_target => {
                        match create_note(&self.active_vault.path, target, None, None, &[]) {
                            Ok((rel_path, _)) => {
                                let _ = self.storage.sync_paths(
                                    &self.active_vault,
                                    std::slice::from_ref(&rel_path),
                                    &[],
                                );
                                self.refresh_vault_lists();
                                if let Ok(detail) =
                                    self.storage.load_note_detail(&self.active_vault, &rel_path)
                                {
                                    self.push_history_entry(previous);
                                    self.cache_inline_images(&detail, ui);
                                    self.active_note = Some(detail);
                                    ui.canvas_mode = CanvasMode::Note;
                                    ui.canvas_selected_block = 0;
                                    ui.canvas_scroll = 0;
                                    ui.canvas_follow_selected = true;
                                    ui.inspector_selected = 0;
                                    self.clamp_selections(ui);
                                    ui.toasts.success(format!("Created note: {rel_path}"));
                                }
                            }
                            Err(create_err) => ui.toasts.warning(format!(
                                "Note not found: {target} (could not create it: {create_err})"
                            )),
                        }
                    }
                    Some("not_found") => ui.toasts.warning(format!("Note not found: {target}")),
                    // The resolver's message lists the candidates.
                    Some("ambiguous") => ui.toasts.warning(err.to_string()),
                    _ => ui.toasts.error(format!("Could not open {target}: {err}")),
                }
            }
        }
    }

    fn push_history_entry(&mut self, previous: Option<String>) {
        if let Some(previous) = previous {
            self.history.push(previous);
            if self.history.len() > 64 {
                self.history.remove(0);
            }
        }
    }

    /// Toggles in-place inline Markdown editing directly inside `note_view` (zero popup window).
    pub fn toggle_inline_edit(&mut self, ui: &mut UiState) {
        if ui.inline_editor.is_some() {
            self.finish_inline_edit(ui);
            return;
        }
        self.open_inline_edit_at(ui, None, None);
    }

    /// Opens the in-place Live Preview inline editor directly inside `note_view`, optionally
    /// positioning the cursor at a 0-based logical line and display column.
    pub fn open_inline_edit_at(
        &mut self,
        ui: &mut UiState,
        target_line: Option<usize>,
        target_col: Option<usize>,
    ) {
        let Some(d) = self.active_note.as_ref() else {
            ui.toasts.info("Open a note to edit");
            return;
        };
        let rel_path = d.summary.rel_path.clone();
        let title = d.summary.title.clone();
        let (raw, base_hash) = safe_vault_path(&self.active_vault.path, &rel_path)
            .ok()
            .and_then(|abs| fs::read(&abs).ok())
            .map(|bytes| {
                let hash = content_sha256_bytes(&bytes);
                let text = String::from_utf8(bytes)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                (text, hash)
            })
            .unwrap_or_else(|| {
                let text = d.raw_markdown.clone();
                let hash = content_sha256_bytes(text.as_bytes());
                (text, hash)
            });
        let body_first_line = d.parsed.body_start_line.saturating_sub(1);
        let initial_scroll = ui.canvas_scroll.min(u16::MAX as usize) as u16;
        let mut editor =
            InlineNoteEditor::with_base_hash(rel_path, title, &raw, base_hash, initial_scroll);
        if ui.last_note_content_area.width >= 10 {
            editor.last_content_width = ui.last_note_content_area.width;
        }
        if let Some(line_idx) = target_line {
            editor.place_cursor_at_logical_line_col(line_idx, target_col.unwrap_or(0));
        } else if body_first_line > 0 && initial_scroll == 0 {
            editor.place_cursor_at_logical_line_col(body_first_line, 0);
        }
        ui.canvas_mode = CanvasMode::Note;
        ui.focus = Focus::Canvas;
        ui.inline_editor = Some(editor);
    }

    /// Re-parses the active note's Markdown in memory (< 0.2ms) on every keystroke so
    /// the right-side Inspector (Outline, Tags, Word Count, Links) and Sidebar Outline update live at 0ms.
    pub fn apply_live_inline_parse(&mut self, ui: &UiState) {
        let Some(ed) = ui.inline_editor.as_ref() else {
            return;
        };
        let Some(detail) = self.active_note.as_mut() else {
            return;
        };
        if detail.summary.rel_path != ed.note_path {
            return;
        }
        let markdown = ed.composer.text().to_string();
        let rel_path = detail.summary.rel_path.clone();
        let title = detail.summary.title.clone();
        let mut parsed = parse_markdown(&rel_path, &title, &markdown);

        let mut all_paths: Vec<String> = Vec::with_capacity(self.notes.len());
        let mut aliases_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut title_by_path: HashMap<String, String> = HashMap::with_capacity(self.notes.len());
        for n in &self.notes {
            all_paths.push(n.rel_path.clone());
            title_by_path.insert(n.rel_path.clone(), n.title.clone());
            if n.rel_path == rel_path {
                if !parsed.aliases.is_empty() {
                    aliases_map.insert(n.rel_path.clone(), parsed.aliases.clone());
                }
            } else if !n.aliases.is_empty() {
                aliases_map.insert(n.rel_path.clone(), n.aliases.clone());
            }
        }

        let images = hydrate_parsed_note_links(
            &self.active_vault.path,
            &rel_path,
            &mut parsed,
            &all_paths,
            &aliases_map,
        );

        let mut outgoing_links: Vec<OutgoingLinkEntry> = parsed
            .wikilinks
            .iter()
            .map(|wl| {
                let resolved_path = resolve_wikilink_from_source(
                    &wl.target_raw,
                    &rel_path,
                    &all_paths,
                    &aliases_map,
                );
                let resolved = resolved_path.is_some();
                let target_title = resolved_path
                    .as_ref()
                    .and_then(|rp| title_by_path.get(rp).cloned())
                    .unwrap_or_else(|| wl.target_raw.clone());
                OutgoingLinkEntry {
                    target_raw: wl.target_raw.clone(),
                    target_resolved_path: resolved_path,
                    target_title,
                    alias: wl.alias.clone(),
                    heading: wl.heading.clone(),
                    is_embed: wl.is_embed,
                    resolved,
                    status: if resolved {
                        LinkStatus::Resolved
                    } else {
                        LinkStatus::Missing
                    },
                    candidates: Vec::new(),
                }
            })
            .collect();
        for ml in &parsed.md_links {
            if ml.is_image {
                continue;
            }
            if let Some((target_raw, heading)) = extract_local_markdown_note_link(&ml.url) {
                let resolved_path =
                    resolve_markdown_link_from_source(&target_raw, &rel_path, &all_paths);
                let resolved = resolved_path.is_some();
                let target_title = resolved_path
                    .as_ref()
                    .and_then(|rp| title_by_path.get(rp).cloned())
                    .unwrap_or_else(|| target_raw.clone());
                let alias = (!ml.label.trim().is_empty()).then(|| ml.label.trim().to_string());
                outgoing_links.push(OutgoingLinkEntry {
                    target_raw,
                    target_resolved_path: resolved_path,
                    target_title,
                    alias,
                    heading,
                    is_embed: false,
                    resolved,
                    status: if resolved {
                        LinkStatus::Resolved
                    } else {
                        LinkStatus::Missing
                    },
                    candidates: Vec::new(),
                });
            }
        }

        detail.raw_markdown = markdown;
        detail.summary.frontmatter = parsed.frontmatter.clone();
        detail.summary.aliases = parsed.aliases.clone();
        detail.summary.tags = parsed.tags.clone();
        detail.summary.word_count = parsed.word_count;
        detail.summary.outgoing_count = outgoing_links.len();
        detail.parsed = parsed;
        detail.outgoing_links = outgoing_links;
        detail.images = images;

        let updated_summary = detail.summary.clone();
        if let Some(slot) = self
            .notes
            .iter_mut()
            .find(|n| n.rel_path == updated_summary.rel_path)
        {
            *slot = updated_summary;
        }
    }

    /// Atomically writes the inline editor buffer to disk (preserving CRLF) after verifying that
    /// the file on disk still matches the baseline recorded on open or last save, and updates ONLY
    /// that single note in SQLite via `Storage::upsert_single_note` (< 5ms, zero full vault scan).
    pub fn flush_inline_save(&mut self, ui: &mut UiState) -> bool {
        let Some((note_path, disk_md, base_hash, is_dirty)) = ui.inline_editor.as_ref().map(|ed| {
            (
                ed.note_path.clone(),
                ed.disk_markdown(),
                ed.base_content_hash,
                ed.dirty,
            )
        }) else {
            return false;
        };

        let abs = match safe_vault_path(&self.active_vault.path, &note_path) {
            Ok(p) => p,
            Err(e) => {
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.mark_error(e.to_string());
                }
                ui.toasts.error(format!("Invalid note path: {e}"));
                return false;
            }
        };

        // Guard against overwriting concurrent external edits or resurrecting deleted/moved files.
        match self.inspect_editor_disk_state(&note_path, base_hash) {
            EditorDiskState::Unchanged => {}
            EditorDiskState::Modified { text, hash } => {
                if !is_dirty {
                    if let Some(ref mut ed) = ui.inline_editor {
                        ed.reload_from_disk(&text, hash);
                    }
                    let _ = self.storage.sync_paths(
                        &self.active_vault,
                        std::slice::from_ref(&note_path),
                        &[],
                    );
                    if let Ok(detail) = self
                        .storage
                        .load_note_detail(&self.active_vault, &note_path)
                    {
                        self.cache_inline_images(&detail, ui);
                        self.active_note = Some(detail);
                    } else {
                        self.apply_live_inline_parse(ui);
                    }
                    ui.toasts.info("Reloaded externally modified note");
                    return false;
                }
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.mark_conflict(ExternalConflict::Modified);
                }
                ui.toasts.warning(
                    "Save blocked: file changed externally on disk — external text preserved and local edits kept",
                );
                return false;
            }
            EditorDiskState::Deleted => {
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.mark_conflict(ExternalConflict::Deleted);
                }
                ui.toasts.warning(
                    "Save blocked: file was deleted or moved externally — old path not recreated",
                );
                return false;
            }
            EditorDiskState::ReadError(err) => {
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.mark_error(err.clone());
                }
                ui.toasts.error(format!("Save check failed: {err}"));
                return false;
            }
        }

        if let Err(e) = write_atomic(&abs, disk_md.as_bytes()) {
            if let Some(ref mut ed) = ui.inline_editor {
                ed.mark_error(e.to_string());
            }
            ui.toasts.error(format!("Save failed: {e}"));
            return false;
        }

        let new_hash = content_sha256_bytes(disk_md.as_bytes());
        let (mtime_ms, size_bytes) = file_mtime_and_size(&abs);
        if let Err(e) = self.storage.upsert_single_note(
            &self.active_vault,
            &note_path,
            &disk_md,
            mtime_ms,
            size_bytes,
        ) {
            if let Some(ref mut ed) = ui.inline_editor {
                ed.mark_error(e.to_string());
            }
            ui.toasts.error(format!("Index update failed: {e}"));
            return false;
        }

        if let Some(ref mut ed) = ui.inline_editor {
            ed.mark_saved_with_hash(new_hash);
        }

        // Sync live parsed AST + lightweight SQLite aggregates (NO disk scan!)
        self.apply_live_inline_parse(ui);
        let vid = self.active_vault.id.clone();
        self.tags = self.storage.list_tags(&vid).unwrap_or_default();
        self.tasks = self
            .storage
            .list_tasks(&vid, TaskFilter::All, None)
            .unwrap_or_default();
        if let Some(detail) = self.active_note.as_mut() {
            if detail.summary.rel_path == note_path {
                detail.summary.mtime_ms = mtime_ms;
                detail.summary.size_bytes = size_bytes;
                if let Ok(bl) = self.storage.get_backlinks(&vid, &note_path) {
                    detail.summary.backlink_count = bl.len();
                    detail.backlinks = bl;
                }
                if let Ok(ol) = self
                    .storage
                    .get_outgoing_links(&self.active_vault, &note_path)
                {
                    detail.summary.outgoing_count = ol.len();
                    detail.outgoing_links = ol;
                }
                let updated_summary = detail.summary.clone();
                if let Some(slot) = self.notes.iter_mut().find(|n| n.rel_path == note_path) {
                    *slot = updated_summary;
                }
            }
        }
        if let Some(images) = self.active_note.as_ref().map(|d| d.images.clone()) {
            self.cache_inline_images_list(&images, ui);
        }
        if ui.canvas_mode == CanvasMode::Graph {
            self.refresh_graph(ui);
        } else {
            self.clamp_selections(ui);
        }
        true
    }

    /// Flushes debounced auto-save when due and reports whether the editor or toast changed.
    pub fn tick_inline_autosave(&mut self, ui: &mut UiState) -> bool {
        let Some(editor) = ui
            .inline_editor
            .as_ref()
            .filter(|editor| editor.is_autosave_due())
        else {
            return false;
        };
        let save_status = editor.save_status.clone();
        let toast = ui.toasts.current().map(|t| (t.kind, t.message.clone()));
        if self.flush_inline_save(ui) {
            return true;
        }
        ui.inline_editor
            .as_ref()
            .is_some_and(|editor| editor.save_status != save_status)
            || ui.toasts.current().map(|t| (t.kind, t.message.as_str()))
                != toast
                    .as_ref()
                    .map(|(kind, message)| (*kind, message.as_str()))
    }

    /// Flushes any pending inline auto-save and exits cleanly back to the rendered reader view
    /// at the exact same scroll offset on frame 0. Returns `false` if save was blocked (e.g.
    /// external conflict) so the editor stays open and local edits remain on screen.
    pub fn finish_inline_edit(&mut self, ui: &mut UiState) -> bool {
        if ui.inline_editor.as_ref().is_some_and(|e| e.dirty) && !self.flush_inline_save(ui) {
            return false;
        }
        if let Some(ed) = ui.inline_editor.take() {
            let cursor_line_1based = ed.cursor_line_col().0;
            let edited_in_session = ed.has_edited;
            let disk_changed = !matches!(
                self.inspect_editor_disk_state(&ed.note_path, ed.base_content_hash),
                EditorDiskState::Unchanged
            );
            ui.focus = Focus::Canvas;
            if disk_changed {
                let _ = self.storage.sync_paths(
                    &self.active_vault,
                    std::slice::from_ref(&ed.note_path),
                    &[],
                );
                self.reload_from_storage_without_html_scan(ui);
            }
            if let Some(detail) = self.active_note.as_ref() {
                if !detail.parsed.blocks.is_empty() {
                    let target_block = detail
                        .parsed
                        .blocks
                        .iter()
                        .rposition(|b| b.line_no() <= cursor_line_1based)
                        .unwrap_or(0);
                    ui.canvas_selected_block = target_block;
                }
            }
            if edited_in_session {
                ui.canvas_follow_selected = true;
            } else {
                ui.canvas_scroll = ed.scroll_y as usize;
                ui.canvas_follow_selected = false;
            }
        }
        true
    }

    pub fn handle_core_event(&mut self, ev: CoreEvent, ui: &mut UiState) {
        match ev {
            CoreEvent::VaultIndexed { stats } => {
                if stats.vault_id == self.active_vault.id {
                    self.last_index_stats = Some(stats);
                    self.reload_from_storage(ui);
                }
            }
            CoreEvent::NoteChanged { .. } => {
                self.reload_from_storage(ui);
            }
            CoreEvent::SyncStatus { summary, .. } => {
                ui.toasts.info(summary);
            }
            CoreEvent::Toast { level, message } => match level.as_str() {
                "error" => ui.toasts.error(message),
                "warning" => ui.toasts.warning(message),
                "success" => ui.toasts.success(message),
                _ => ui.toasts.info(message),
            },
        }
    }

    pub fn dispatch(&mut self, action: Action, ui: &mut UiState) {
        if matches!(action, Action::InlineEditTick) {
            self.tick_inline_autosave(ui);
            return;
        }

        // Route overlay input first when an overlay is open
        if ui.overlay.is_some() && !matches!(action, Action::MouseClick { .. }) {
            // Block opening a context menu while another modal overlay is open; if a ContextMenu
            // itself is already open, right-clicking repositions it cleanly.
            if let Action::OpenContextMenu { col, row } = action {
                if matches!(ui.overlay, Some(Overlay::ContextMenu(_))) {
                    ui.overlay = Some(Overlay::ContextMenu(ContextMenuOverlay::default_menu(
                        col, row,
                    )));
                }
                return;
            }
            if matches!(action, Action::OpenContextMenuKeyboard) {
                if matches!(ui.overlay, Some(Overlay::ContextMenu(_))) {
                    ui.close_overlay();
                }
                return;
            }
            let effective_action = if matches!(action, Action::ClearTagFilter) {
                Action::CloseOverlay
            } else {
                action
            };
            let outcome = if let Some(mut ov) = ui.overlay.take() {
                let res = ov.handle(&effective_action, self);
                match res {
                    OverlayOutcome::Close => {
                        ui.close_overlay();
                        None
                    }
                    OverlayOutcome::CloseAndDispatch(next) => {
                        ui.close_overlay();
                        Some(next)
                    }
                    OverlayOutcome::Dispatch(next) => {
                        ui.overlay = Some(ov);
                        Some(next)
                    }
                    OverlayOutcome::Handled => {
                        ui.overlay = Some(ov);
                        None
                    }
                    OverlayOutcome::NotHandled => {
                        ui.overlay = Some(ov);
                        matches!(effective_action, Action::Quit | Action::QuitConfirm)
                            .then(|| effective_action.clone())
                    }
                }
            } else {
                None
            };
            if let Some(next_act) = outcome {
                self.dispatch_core(next_act, ui);
            }
            return;
        }

        // Route inline editor keys directly when in-place editing is active inside `note_view`
        if ui.overlay.is_none() && ui.inline_editor.is_some() {
            match &action {
                Action::ToggleEditorMode => {
                    if let Some(ref mut ed) = ui.inline_editor {
                        ed.toggle_mode();
                    }
                    return;
                }
                Action::SaveInlineEdit | Action::OverlaySubmit => {
                    self.flush_inline_save(ui);
                    return;
                }
                Action::FinishInlineEdit | Action::CloseOverlay => {
                    self.finish_inline_edit(ui);
                    return;
                }
                Action::RequestExternalEditor => {
                    if self.finish_inline_edit(ui) {
                        self.dispatch_core(Action::RequestExternalEditor, ui);
                    }
                    return;
                }
                Action::PasteClipboard => {
                    match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                        Ok(text) if !text.is_empty() => {
                            let changed = ui.inline_editor.as_mut().is_some_and(|ed| {
                                ed.handle_edit_action(&Action::Paste(text))
                                    == InlineEditEvent::TextChanged
                            });
                            if changed {
                                self.apply_live_inline_parse(ui);
                            }
                        }
                        Ok(_) => ui.toasts.info("Clipboard is empty"),
                        Err(e) => ui.toasts.warning(format!("Clipboard error: {e}")),
                    }
                    return;
                }
                Action::MouseScrollUp { col, row } | Action::MouseScrollDown { col, row } => {
                    if self.scroll_pane_at(*col, *row, ui) == Focus::Canvas {
                        if let Some(ref mut ed) = ui.inline_editor {
                            ed.handle_edit_action(&action);
                        }
                        return;
                    }
                }
                _ => {
                    let ev = ui
                        .inline_editor
                        .as_mut()
                        .map(|ed| ed.handle_edit_action(&action))
                        .unwrap_or(InlineEditEvent::Ignored);
                    match ev {
                        InlineEditEvent::TextChanged => {
                            self.apply_live_inline_parse(ui);
                            return;
                        }
                        InlineEditEvent::CursorOrScrollChanged => return,
                        InlineEditEvent::Ignored => {}
                    }
                }
            }
        }

        self.dispatch_core(action, ui);
    }

    fn dispatch_core(&mut self, action: Action, ui: &mut UiState) {
        match action {
            Action::Quit | Action::QuitConfirm => {
                if let Some(note_path) = ui
                    .inline_editor
                    .as_ref()
                    .filter(|e| e.dirty)
                    .map(|e| e.note_path.clone())
                {
                    if !self.flush_inline_save(ui) {
                        // Never recreate a deleted note, overwrite external edits, or drop a conflict draft on exit.
                        if !matches!(ui.overlay, Some(Overlay::Confirm(_))) {
                            ui.open_overlay(Overlay::Confirm(ConfirmOverlay::new(
                                "Unsaved Conflict Draft",
                                format!(
                                    "'{}' changed or was deleted on disk. Discard local draft and quit?",
                                    note_path
                                ),
                                "Discard & Quit",
                                Action::ForceQuitDiscard,
                            )));
                        }
                        return;
                    }
                }
                ui.should_quit = true;
            }
            Action::ForceQuitDiscard => {
                // Explicitly confirmed discard of an unsaved conflict draft: exit without touching disk.
                ui.should_quit = true;
            }
            Action::RefreshAll => {
                self.preview_cache.clear();
                if self.sync_ctrl.try_reindex_now() {
                    ui.toasts.info("Reindexing vault...");
                } else if let Ok(stats) = self.storage.reindex_vault(&self.active_vault) {
                    let msg = format!(
                        "Indexed {} notes ({} updated, {}ms)",
                        stats.total_notes, stats.updated, stats.elapsed_ms
                    );
                    self.last_index_stats = Some(stats);
                    self.reload_from_storage(ui);
                    ui.toasts.success(msg);
                }
            }

            Action::NextFocus => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                if ui.canvas_mode == CanvasMode::Graph && ui.focus == Focus::Canvas {
                    let len = self.graph.nodes.len();
                    if len > 0 {
                        ui.graph_selected = (ui.graph_selected + 1) % len;
                    }
                } else {
                    ui.focus = match ui.focus {
                        Focus::Sidebar => Focus::Canvas,
                        Focus::Canvas => {
                            if ui.show_inspector {
                                Focus::Inspector
                            } else if ui.show_sidebar {
                                Focus::Sidebar
                            } else {
                                Focus::Canvas
                            }
                        }
                        Focus::Inspector => {
                            if ui.show_sidebar {
                                Focus::Sidebar
                            } else {
                                Focus::Canvas
                            }
                        }
                        Focus::Overlay => Focus::Canvas,
                    };
                }
            }
            Action::PrevFocus => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                ui.focus = match ui.focus {
                    Focus::Sidebar => {
                        if ui.show_inspector {
                            Focus::Inspector
                        } else {
                            Focus::Canvas
                        }
                    }
                    Focus::Canvas => {
                        if ui.show_sidebar {
                            Focus::Sidebar
                        } else if ui.show_inspector {
                            Focus::Inspector
                        } else {
                            Focus::Canvas
                        }
                    }
                    Focus::Inspector => Focus::Canvas,
                    Focus::Overlay => Focus::Canvas,
                };
            }

            Action::MoveUp => match ui.focus {
                Focus::Sidebar => {
                    ui.sidebar_selected = ui.sidebar_selected.saturating_sub(1);
                }
                Focus::Canvas => {
                    if ui.canvas_mode == CanvasMode::Graph {
                        let len = self.graph.nodes.len();
                        if len > 0 {
                            ui.graph_selected = (ui.graph_selected + len - 1) % len;
                        }
                    } else {
                        ui.canvas_selected_block = ui.canvas_selected_block.saturating_sub(1);
                        ui.canvas_follow_selected = true;
                    }
                }
                Focus::Inspector => {
                    ui.inspector_selected = ui.inspector_selected.saturating_sub(1);
                }
                Focus::Overlay => {}
            },

            Action::MoveDown => match ui.focus {
                Focus::Sidebar => {
                    let len = build_sidebar_entries(self, ui).len();
                    if len > 0 {
                        ui.sidebar_selected = (ui.sidebar_selected + 1).min(len - 1);
                    }
                }
                Focus::Canvas => {
                    if ui.canvas_mode == CanvasMode::Graph {
                        let len = self.graph.nodes.len();
                        if len > 0 {
                            ui.graph_selected = (ui.graph_selected + 1) % len;
                        }
                    } else if let Some(d) = &self.active_note {
                        let max_b = d.parsed.blocks.len().saturating_sub(1);
                        ui.canvas_selected_block = (ui.canvas_selected_block + 1).min(max_b);
                        ui.canvas_follow_selected = true;
                    }
                }
                Focus::Inspector => {
                    if let Some(d) = &self.active_note {
                        let total = Self::inspector_item_count(d);
                        if total > 0 {
                            ui.inspector_selected = (ui.inspector_selected + 1).min(total - 1);
                        }
                    }
                }
                Focus::Overlay => {}
            },

            Action::PageUp => match ui.focus {
                Focus::Sidebar => {
                    ui.sidebar_selected = ui.sidebar_selected.saturating_sub(10);
                }
                Focus::Inspector => {
                    ui.inspector_selected = ui.inspector_selected.saturating_sub(10);
                }
                Focus::Canvas | Focus::Overlay => {
                    ui.canvas_follow_selected = false;
                    ui.canvas_scroll = ui.canvas_scroll.saturating_sub(10);
                    ui.canvas_selected_block = ui.canvas_selected_block.saturating_sub(5);
                }
            },
            Action::PageDown => match ui.focus {
                Focus::Sidebar => {
                    let len = build_sidebar_entries(self, ui).len();
                    if len > 0 {
                        ui.sidebar_selected = (ui.sidebar_selected + 10).min(len - 1);
                    }
                }
                Focus::Inspector => {
                    if let Some(d) = &self.active_note {
                        let total = Self::inspector_item_count(d);
                        if total > 0 {
                            ui.inspector_selected = (ui.inspector_selected + 10).min(total - 1);
                        }
                    }
                }
                Focus::Canvas | Focus::Overlay => {
                    ui.canvas_follow_selected = false;
                    ui.canvas_scroll = ui.canvas_scroll.saturating_add(10);
                    if let Some(d) = &self.active_note {
                        let max_b = d.parsed.blocks.len().saturating_sub(1);
                        ui.canvas_selected_block = (ui.canvas_selected_block + 5).min(max_b);
                    }
                }
            },
            Action::JumpTop => match ui.focus {
                Focus::Sidebar => {
                    ui.sidebar_selected = 0;
                }
                Focus::Inspector => {
                    ui.inspector_selected = 0;
                }
                Focus::Canvas | Focus::Overlay => {
                    ui.canvas_selected_block = 0;
                    ui.canvas_scroll = 0;
                    ui.canvas_follow_selected = true;
                }
            },
            Action::JumpBottom => match ui.focus {
                Focus::Sidebar => {
                    let len = build_sidebar_entries(self, ui).len();
                    ui.sidebar_selected = len.saturating_sub(1);
                }
                Focus::Inspector => {
                    if let Some(d) = &self.active_note {
                        ui.inspector_selected = Self::inspector_item_count(d).saturating_sub(1);
                    }
                }
                Focus::Canvas | Focus::Overlay => {
                    if let Some(d) = &self.active_note {
                        ui.canvas_selected_block = d.parsed.blocks.len().saturating_sub(1);
                        ui.canvas_follow_selected = true;
                    }
                }
            },

            Action::Activate => match ui.focus {
                Focus::Sidebar => self.activate_sidebar_selected(ui),
                Focus::Canvas => {
                    if ui.canvas_mode == CanvasMode::Graph {
                        if let Some(node) = self.graph.nodes.get(ui.graph_selected).cloned() {
                            self.open_note(ui, &node.id, true);
                        }
                    } else if let Some(d) = &self.active_note {
                        match d.parsed.blocks.get(ui.canvas_selected_block) {
                            Some(block) if block.first_wikilink().is_some() => {
                                self.dispatch_core(Action::FollowSelectedWikilink, ui);
                            }
                            Some(NoteBlock::Task { .. }) => {
                                self.dispatch_core(Action::FollowSelectedWikilink, ui);
                            }
                            Some(NoteBlock::ImageEmbed { .. } | NoteBlock::HtmlEmbed { .. }) => {}
                            Some(block) => {
                                let target_line = block.line_no().saturating_sub(1);
                                self.open_inline_edit_at(ui, Some(target_line), None);
                            }
                            None => {
                                self.open_inline_edit_at(ui, Some(0), None);
                            }
                        }
                    }
                }
                Focus::Inspector => self.activate_inspector_selected(ui),
                Focus::Overlay => {}
            },

            Action::NavigateBack => {
                if let Some(prev) = self.history.pop() {
                    self.open_note(ui, &prev, false);
                } else {
                    ui.toasts.info("History stack is empty");
                }
            }

            Action::ToggleSidebar => {
                ui.show_sidebar = !ui.show_sidebar;
                if !ui.show_sidebar && ui.focus == Focus::Sidebar {
                    ui.focus = Focus::Canvas;
                }
            }
            Action::ToggleInspector => {
                ui.show_inspector = !ui.show_inspector;
                if !ui.show_inspector && ui.focus == Focus::Inspector {
                    ui.focus = Focus::Canvas;
                }
            }
            Action::TogglePageWidth => {
                ui.page_width = ui.page_width.toggle();
                self.cfg.page_width = ui.page_width;
                let _ = self.cfg.save();
            }
            Action::SetSidebarTab(tab) => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                ui.sidebar_tab = tab;
                ui.sidebar_selected = 0;
                ui.show_sidebar = true;
                ui.focus = Focus::Sidebar;
                self.clamp_selections(ui);
            }
            Action::ClearTagFilter => {
                if ui.active_tag_filter.is_some() {
                    ui.active_tag_filter = None;
                    ui.sidebar_selected = 0;
                    self.clamp_selections(ui);
                }
            }

            Action::OpenNote(path) => self.open_note(ui, &path, true),

            Action::ToggleGraphView => {
                ui.canvas_mode = match ui.canvas_mode {
                    CanvasMode::Note => {
                        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                            return;
                        }
                        self.refresh_graph(ui);
                        ui.graph_selected = 0;
                        ui.focus = Focus::Canvas;
                        CanvasMode::Graph
                    }
                    CanvasMode::Graph => CanvasMode::Note,
                };
            }
            Action::ToggleGraphScope => {
                ui.graph_global = !ui.graph_global;
                ui.graph_selected = 0;
                self.refresh_graph(ui);
            }
            Action::GraphDepthInc => {
                ui.graph_depth = (ui.graph_depth + 1).min(5);
                self.refresh_graph(ui);
            }
            Action::GraphDepthDec => {
                ui.graph_depth = ui.graph_depth.saturating_sub(1).max(1);
                self.refresh_graph(ui);
            }
            Action::GraphZoomIn => {
                ui.graph_sim
                    .zoom_by(crate::ui::graph_view::GRAPH_WHEEL_FACTOR);
            }
            Action::GraphZoomOut => {
                ui.graph_sim
                    .zoom_by(1.0 / crate::ui::graph_view::GRAPH_WHEEL_FACTOR);
            }
            Action::GraphResetView => {
                ui.graph_sim.reset_view(&self.graph);
                ui.toasts.info("Reset graph view");
            }
            Action::GraphTick => {
                if ui.canvas_mode == CanvasMode::Graph {
                    ui.graph_sim.step_physics(&self.graph);
                }
            }

            Action::ToggleBookmark => {
                if let Some(rp) = self.active_note.as_ref().map(|d| d.summary.rel_path.clone()) {
                    if let Ok(bm) = self.storage.toggle_bookmark(&self.active_vault.id, &rp) {
                        if let Some(detail) = self.active_note.as_mut() {
                            if detail.summary.rel_path == rp {
                                detail.summary.is_bookmarked = bm;
                            }
                        }
                        if let Some(slot) = self.notes.iter_mut().find(|n| n.rel_path == rp) {
                            slot.is_bookmarked = bm;
                        }
                        self.clamp_selections(ui);
                        ui.toasts.success(if bm {
                            "Bookmarked note"
                        } else {
                            "Removed bookmark"
                        });
                    }
                }
            }

            Action::OpenDailyNote => match ensure_daily_note(&self.active_vault, None) {
                Ok((rel_path, _, created)) => {
                    if created
                        || !self
                            .notes
                            .iter()
                            .any(|n| n.rel_path.eq_ignore_ascii_case(&rel_path))
                    {
                        let _ = self.storage.sync_paths(
                            &self.active_vault,
                            std::slice::from_ref(&rel_path),
                            &[],
                        );
                        self.refresh_vault_lists();
                    }
                    self.open_note(ui, &rel_path, true);
                    if created {
                        ui.toasts.success(format!("Created daily note: {rel_path}"));
                    }
                }
                Err(e) => ui.toasts.error(format!("Daily note failed: {e}")),
            },

            Action::ToggleSelectedTask => {
                if ui.focus == Focus::Sidebar && ui.sidebar_tab == SidebarTab::Tasks {
                    let entries = build_sidebar_entries(self, ui);
                    if let Some(SidebarEntry::Task {
                        note_path, line_no, ..
                    }) = entries.get(ui.sidebar_selected).cloned()
                    {
                        self.dispatch_core(Action::ToggleTaskAtLine { note_path, line_no }, ui);
                    }
                } else if let Some(d) = &self.active_note {
                    if let Some(NoteBlock::Task { line_no, .. }) =
                        d.parsed.blocks.get(ui.canvas_selected_block)
                    {
                        let note_path = d.summary.rel_path.clone();
                        let ln = *line_no;
                        self.dispatch_core(
                            Action::ToggleTaskAtLine {
                                note_path,
                                line_no: ln,
                            },
                            ui,
                        );
                    }
                }
            }

            Action::ToggleTaskAtLine { note_path, line_no } => {
                if let Ok(abs) = safe_vault_path(&self.active_vault.path, &note_path) {
                    let expected = self
                        .active_note
                        .as_ref()
                        .filter(|d| d.summary.rel_path.eq_ignore_ascii_case(&note_path))
                        .and_then(|d| {
                            d.parsed
                                .tasks
                                .iter()
                                .find(|t| t.line_no == line_no)
                                .map(|t| (t.text.clone(), t.checked))
                        })
                        .or_else(|| {
                            self.tasks
                                .iter()
                                .find(|t| {
                                    t.note_path.eq_ignore_ascii_case(&note_path)
                                        && t.line_no == line_no
                                })
                                .map(|t| (t.text.clone(), t.checked))
                        });
                    let expected_text = expected.as_ref().map(|(txt, _)| txt.as_str());
                    let expected_checked = expected.as_ref().map(|(_, chk)| *chk);

                    match toggle_task_at_line_verified(
                        &abs,
                        line_no,
                        expected_text,
                        expected_checked,
                    ) {
                        Ok(checked) => {
                            let _ = self.storage.sync_paths(
                                &self.active_vault,
                                std::slice::from_ref(&note_path),
                                &[],
                            );
                            self.reload_from_storage_without_html_scan(ui);
                            ui.toasts.success(if checked {
                                "Marked task [x] complete"
                            } else {
                                "Reopened task [ ]"
                            });
                        }
                        Err(e) => {
                            let _ = self.storage.sync_paths(
                                &self.active_vault,
                                std::slice::from_ref(&note_path),
                                &[],
                            );
                            self.reload_from_storage_without_html_scan(ui);
                            ui.toasts.warning(format!("Task toggle skipped: {e}"));
                        }
                    }
                }
            }

            Action::JumpToHeadingLine(line_no) => {
                if let Some(d) = &self.active_note {
                    if let Some(idx) = d.parsed.blocks.iter().position(|b| b.line_no() >= line_no) {
                        ui.canvas_mode = CanvasMode::Note;
                        ui.canvas_selected_block = idx;
                        ui.canvas_scroll = 0;
                        ui.canvas_follow_selected = true;
                        ui.focus = Focus::Canvas;
                    }
                }
            }

            Action::FollowSelectedWikilink => {
                if let Some(d) = &self.active_note {
                    if let Some(block) = d.parsed.blocks.get(ui.canvas_selected_block) {
                        if let Some((target, resolved)) = block.first_wikilink() {
                            let dest = resolved.unwrap_or(target);
                            self.open_note_with(ui, &dest, true, OpenIntent::FollowLink);
                        } else if let NoteBlock::Task { line_no, .. } = block {
                            let note_path = d.summary.rel_path.clone();
                            let ln = *line_no;
                            self.dispatch_core(
                                Action::ToggleTaskAtLine {
                                    note_path,
                                    line_no: ln,
                                },
                                ui,
                            );
                        }
                    }
                }
            }

            Action::OpenQuickAppend => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                let title = self
                    .active_note
                    .as_ref()
                    .map(|d| d.summary.title.clone())
                    .unwrap_or_else(|| "Note".into());
                ui.open_overlay(Overlay::NoteForm(NoteFormOverlay::quick_append(title)));
            }
            Action::OpenNewNoteModal => {
                let default_folder = self
                    .active_note
                    .as_ref()
                    .map(|d| d.summary.folder.clone())
                    .unwrap_or_default();
                ui.open_overlay(Overlay::NoteForm(NoteFormOverlay::new_note(default_folder)));
            }
            Action::OpenNoteEditor => {
                self.toggle_inline_edit(ui);
            }
            Action::ToggleEditorMode => {
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.toggle_mode();
                }
            }
            Action::SaveInlineEdit => {
                self.flush_inline_save(ui);
            }
            Action::FinishInlineEdit => {
                self.finish_inline_edit(ui);
            }
            Action::InlineEditTick => {
                self.tick_inline_autosave(ui);
            }
            Action::CreateNoteSubmit {
                title,
                folder,
                tags,
            } => {
                let tag_vec: Vec<String> = tags
                    .split([',', ' '])
                    .map(|s| s.trim().trim_start_matches('#').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let folder_opt = if folder.trim().is_empty() {
                    None
                } else {
                    Some(folder.as_str())
                };
                match create_note(&self.active_vault.path, &title, folder_opt, None, &tag_vec) {
                    Ok((rel_path, _)) => {
                        let _ = self.storage.sync_paths(
                            &self.active_vault,
                            std::slice::from_ref(&rel_path),
                            &[],
                        );
                        self.refresh_vault_lists();
                        self.open_note(ui, &rel_path, true);
                        ui.toasts.success(format!("Created {rel_path}"));
                    }
                    Err(e) => ui.toasts.error(format!("Create failed: {e}")),
                }
            }
            Action::QuickAppendSubmit { text } => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                if let Some(d) = &self.active_note {
                    let rp = d.summary.rel_path.clone();
                    if let Ok(abs) = safe_vault_path(&self.active_vault.path, &rp) {
                        match append_to_note(&abs, &text) {
                            Ok(()) => {
                                let _ = self.storage.sync_paths(
                                    &self.active_vault,
                                    std::slice::from_ref(&rp),
                                    &[],
                                );
                                self.reload_from_storage_without_html_scan(ui);
                                ui.toasts.success("Appended to note");
                            }
                            Err(e) => ui.toasts.error(format!("Append failed: {e}")),
                        }
                    }
                }
            }

            Action::RequestExternalEditor => {
                if let Some(d) = &self.active_note {
                    if let Ok(abs) = safe_vault_path(&self.active_vault.path, &d.summary.rel_path) {
                        self.pending_external_editor = Some(abs);
                    }
                }
            }

            Action::ConfirmDeleteNote => {
                if let Some(d) = &self.active_note {
                    ui.open_overlay(Overlay::Confirm(ConfirmOverlay::new(
                        "Move Note to .trash/",
                        format!("Move '{}' to .trash/?", d.summary.rel_path),
                        "Move to Trash",
                        Action::DeleteActiveNoteConfirmed,
                    )));
                }
            }
            Action::DeleteActiveNoteConfirmed => {
                if let Some(d) = &self.active_note {
                    ui.inline_editor = None;
                    let rp = d.summary.rel_path.clone();
                    match move_note_to_trash(&self.active_vault.path, &rp) {
                        Ok(_) => {
                            self.active_note = None;
                            ui.canvas_scroll = 0;
                            ui.canvas_selected_block = 0;
                            ui.canvas_follow_selected = true;
                            ui.inspector_selected = 0;
                            let _ = self.storage.sync_paths(
                                &self.active_vault,
                                std::slice::from_ref(&rp),
                                &[],
                            );
                            self.reload_from_storage_without_html_scan(ui);
                            ui.toasts.success(format!("Moved {rp} to .trash/"));
                        }
                        Err(e) => ui.toasts.error(format!("Delete failed: {e}")),
                    }
                }
            }

            Action::OpenFilePicker => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                ui.open_overlay(Overlay::FilePicker(FilePickerOverlay::new_in_dir(
                    self.active_vault.path.clone(),
                )));
            }
            Action::AttachLocalFile(src_path) => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                if let Ok(bytes) = fs::read(&src_path) {
                    let fname = src_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("attachment.bin");
                    match save_vault_attachment(&self.active_vault, fname, &bytes) {
                        Ok((saved_name, _)) => {
                            if let Some(rp) =
                                self.active_note.as_ref().map(|d| d.summary.rel_path.clone())
                            {
                                if let Ok(abs) = safe_vault_path(&self.active_vault.path, &rp) {
                                    let _ = append_to_note(&abs, &format!("\n![[{saved_name}]]"));
                                    let _ = self.storage.sync_paths(
                                        &self.active_vault,
                                        std::slice::from_ref(&rp),
                                        &[],
                                    );
                                    self.reload_from_storage_without_html_scan(ui);
                                    ui.toasts.success(format!("Embedded ![[{saved_name}]]"));
                                }
                            }
                        }
                        Err(e) => ui.toasts.error(format!("Attachment save failed: {e}")),
                    }
                }
            }
            Action::PasteClipboard => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                match read_clipboard_image_png() {
                    Ok(Some((filename, bytes))) => {
                        let name = if filename.trim().is_empty() {
                            format!("pasted-{}.png", Local::now().format("%Y%m%d-%H%M%S"))
                        } else {
                            filename
                        };
                        match save_vault_attachment(&self.active_vault, &name, &bytes) {
                            Ok((saved_name, _)) => {
                                if let Some(rp) =
                                    self.active_note.as_ref().map(|d| d.summary.rel_path.clone())
                                {
                                    if let Ok(abs) = safe_vault_path(&self.active_vault.path, &rp) {
                                        let _ =
                                            append_to_note(&abs, &format!("\n![[{saved_name}]]"));
                                        let _ = self.storage.sync_paths(
                                            &self.active_vault,
                                            std::slice::from_ref(&rp),
                                            &[],
                                        );
                                        self.reload_from_storage_without_html_scan(ui);
                                        ui.toasts.success(format!("Pasted ![[{saved_name}]]"));
                                    }
                                }
                            }
                            Err(e) => ui.toasts.error(format!("Clipboard save failed: {e}")),
                        }
                    }
                    Ok(None) => ui.toasts.info("No image found in clipboard"),
                    Err(e) => ui.toasts.warning(format!("Clipboard error: {e}")),
                }
            }

            Action::OpenLinkExtractor => {
                if let Some(d) = &self.active_note {
                    ui.open_overlay(Overlay::LinkExtractor(LinkExtractorOverlay::from_note(d)));
                }
            }
            Action::ToggleShowLinkUrls => {
                ui.show_inline_hrefs = !ui.show_inline_hrefs;
                self.cfg.show_inline_hrefs = ui.show_inline_hrefs;
            }
            Action::OpenImagePreview => {
                self.dispatch_core(Action::OpenImagePreviewAt(0), ui);
            }
            Action::OpenImagePreviewAt(idx) => {
                if let Some(d) = &self.active_note {
                    if let Some(ov) = ImagePreviewOverlay::from_note(d, Some(idx)) {
                        if let Some(target) = ov.current() {
                            if let Ok(bytes) = fs::read(&target.abs_path) {
                                if sniff_content_kind(
                                    Some(&target.mime_type),
                                    Some(&target.display_name),
                                    Some(&bytes),
                                ) == ContentKind::Html
                                {
                                    let html = String::from_utf8_lossy(&bytes).into_owned();
                                    let src = target.abs_path.display().to_string();
                                    let name = target.display_name.clone();
                                    ui.open_overlay(Overlay::HtmlPreview(
                                        HtmlPreviewOverlay::new(name.clone(), src, html)
                                            .with_source(HtmlPreviewSource::File(
                                                target.abs_path.clone(),
                                            )),
                                    ));
                                    ui.toasts.info(format!(
                                        "Detected HTML in '{name}' — routed to interactive HTML preview"
                                    ));
                                    return;
                                }
                            }
                        }
                        ui.open_overlay(Overlay::ImagePreview(ov));
                    } else {
                        ui.toasts.info("No embedded images in this note");
                    }
                }
            }
            Action::OpenHtmlPreview => {
                self.open_html_preview(ui, None);
            }
            Action::OpenHtmlBlock(block_idx) => {
                self.open_html_preview(ui, Some(block_idx));
            }
            Action::OpenHtmlFile(rel_path) => {
                self.open_html_file(ui, &rel_path);
            }
            Action::OpenHtmlContent {
                title,
                source_label,
                html,
            } => {
                ui.open_overlay(Overlay::HtmlPreview(HtmlPreviewOverlay::new(
                    title,
                    source_label,
                    html,
                )));
            }
            Action::ServeHtmlInBrowser { title, html } => {
                self.pending_html_serve = Some((title, html));
            }
            Action::ShowInfoToast(msg) => {
                ui.toasts.info(msg);
            }
            Action::OpenUrl(url) => {
                if let Err(e) = open::that(&url) {
                    ui.toasts.error(format!("Failed to open URL: {e}"));
                } else {
                    ui.toasts.info("Opened link in browser");
                }
            }
            Action::CopyUrlToClipboard(text) => {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    if cb.set_text(text).is_ok() {
                        ui.toasts.success("Copied to clipboard");
                    }
                }
            }
            Action::CopySelectionOrMarkdown => {
                if let Some(d) = &self.active_note {
                    let payload = match d.parsed.blocks.get(ui.canvas_selected_block) {
                        Some(NoteBlock::CodeBlock { code, .. }) => code.clone(),
                        _ => d.raw_markdown.clone(),
                    };
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        if cb.set_text(payload).is_ok() {
                            ui.toasts.success("Copied to clipboard");
                        }
                    }
                }
            }

            Action::OpenOverlay(kind) => match kind {
                OverlayKind::QuickSwitcher => {
                    ui.open_overlay(Overlay::QuickSwitcher(QuickSwitcherOverlay::new()));
                }
                OverlayKind::VaultSwitcher => {
                    let mut vaults = merged_vaults(&self.cfg);
                    if !vaults.iter().any(|v| v.id == self.active_vault.id) {
                        vaults.insert(0, self.active_vault.clone());
                    }
                    self.vaults = vaults;
                    ui.open_overlay(Overlay::VaultSwitcher(VaultSwitcherOverlay::new(self)));
                }
                OverlayKind::Settings => {
                    ui.open_overlay(Overlay::Settings(SettingsOverlay::from_state(self, ui)));
                }
                OverlayKind::Help => {
                    ui.open_overlay(Overlay::Help { scroll: 0 });
                }
            },
            Action::OpenContextMenu { col, row } => {
                // Target the actual item under the right-click cursor so actions like
                // "Move Note to .trash/" or "Edit" operate on the right-clicked note/block.
                match ui.last_hitmap.hit(row, col) {
                    Some(HitTarget::SidebarRow(idx)) | Some(HitTarget::SidebarTaskCheckbox(idx)) => {
                        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                            return;
                        }
                        let entries = build_sidebar_entries(self, ui);
                        if let Some(entry) = entries.get(idx).cloned() {
                            ui.sidebar_selected = idx;
                            match entry {
                                SidebarEntry::Note { rel_path, .. } => {
                                    self.open_note(ui, &rel_path, true);
                                    if ui.inline_editor.is_some() {
                                        return;
                                    }
                                }
                                SidebarEntry::Task { note_path, .. } => {
                                    self.open_note(ui, &note_path, true);
                                    if ui.inline_editor.is_some() {
                                        return;
                                    }
                                }
                                _ => {}
                            }
                            ui.focus = Focus::Sidebar;
                        }
                    }
                    Some(HitTarget::CanvasBlock(idx)) | Some(HitTarget::WikilinkBlock(idx)) => {
                        ui.canvas_selected_block = idx;
                        ui.focus = Focus::Canvas;
                    }
                    Some(HitTarget::GraphNode(idx)) => {
                        ui.graph_selected = idx;
                        ui.focus = Focus::Canvas;
                    }
                    Some(HitTarget::InspectorBacklink(idx)) => {
                        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                            return;
                        }
                        ui.inspector_selected = idx;
                        ui.focus = Focus::Inspector;
                    }
                    Some(HitTarget::InspectorOutgoing(idx)) => {
                        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                            return;
                        }
                        let bl_len = self
                            .active_note
                            .as_ref()
                            .map(|d| d.backlinks.len())
                            .unwrap_or(0);
                        ui.inspector_selected = bl_len + idx;
                        ui.focus = Focus::Inspector;
                    }
                    Some(HitTarget::InspectorOutline(idx)) => {
                        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                            return;
                        }
                        let base = self
                            .active_note
                            .as_ref()
                            .map(|d| d.backlinks.len() + d.outgoing_links.len())
                            .unwrap_or(0);
                        ui.inspector_selected = base + idx;
                        ui.focus = Focus::Inspector;
                    }
                    _ => {}
                }
                ui.open_overlay(Overlay::ContextMenu(ContextMenuOverlay::default_menu(
                    col, row,
                )));
            }
            Action::OpenContextMenuKeyboard => {
                ui.open_overlay(Overlay::ContextMenu(ContextMenuOverlay::default_menu(14, 4)));
            }
            Action::CloseOverlay => ui.close_overlay(),

            Action::SwitchVault(vault_id) => {
                if let Some(next) = self.vaults.iter().find(|v| v.id == vault_id).cloned() {
                    if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                        return;
                    }
                    self.active_vault = next.clone();
                    self.cfg.active_vault = next.name.clone();
                    let _ = self.cfg.save();
                    self.active_note = None;
                    self.graph = GraphData::default();
                    self.history.clear();
                    self.preview_cache.clear();
                    ui.active_tag_filter = None;
                    ui.collapsed_folders.clear();
                    ui.sidebar_selected = 0;
                    ui.canvas_selected_block = 0;
                    ui.canvas_scroll = 0;
                    ui.canvas_follow_selected = true;
                    ui.inspector_selected = 0;
                    ui.graph_selected = 0;
                    if !self.sync_ctrl.try_switch_vault(next.clone()) {
                        let _ = self.storage.sync_vault(&next);
                    }
                    self.reload_from_storage(ui);
                    ui.toasts
                        .success(format!("Switched vault → {}", next.display_name()));
                }
            }

            Action::SetTheme(preset) => {
                self.cfg.theme = preset;
                ui.theme = Theme::detect(preset);
                self.preview_cache.clear();
                if let Some(images) = self.active_note.as_ref().map(|d| d.images.clone()) {
                    self.cache_inline_images_list(&images, ui);
                }
                let _ = self.cfg.save();
            }
            Action::SetPageWidth(w) => {
                self.cfg.page_width = w;
                ui.page_width = w;
                let _ = self.cfg.save();
            }
            Action::SetImageProtocol(p) => {
                self.cfg.image_protocol = p;
                let prev_cell_px = self.graphics_caps.cell_px;
                let prev_cell_px_exact = self.graphics_caps.cell_px_exact;
                let mut caps = detect_graphics_caps(p);
                if prev_cell_px_exact || !caps.cell_px_exact {
                    caps.cell_px = prev_cell_px;
                    caps.cell_px_exact = prev_cell_px_exact;
                }
                self.graphics_caps = caps;
                self.preview_cache.clear();
                if let Some(images) = self.active_note.as_ref().map(|d| d.images.clone()) {
                    self.cache_inline_images_list(&images, ui);
                }
                let _ = self.cfg.save();
            }
            Action::ToggleInlineImages => {
                self.cfg.inline_images = !self.cfg.inline_images;
                if self.cfg.inline_images {
                    if let Some(images) = self.active_note.as_ref().map(|d| d.images.clone()) {
                        self.cache_inline_images_list(&images, ui);
                    }
                }
                let _ = self.cfg.save();
            }

            Action::MouseClick { col, row } => self.handle_mouse_click(col, row, ui),
            Action::MouseDrag { col, row } => {
                if ui.overlay.is_none() && ui.canvas_mode == CanvasMode::Graph {
                    if ui.graph_sim.dragging_node.is_some() {
                        if ui.graph_sim.last_mouse != Some((col, row)) {
                            ui.graph_sim.last_mouse = Some((col, row));
                            ui.graph_sim.move_dragged_node(&self.graph, col, row);
                        }
                    } else if ui.graph_sim.panning {
                        if let Some((lc, lr)) = ui.graph_sim.last_mouse {
                            ui.graph_sim.pan_by_screen_delta(
                                col as i32 - lc as i32,
                                row as i32 - lr as i32,
                            );
                        }
                        ui.graph_sim.last_mouse = Some((col, row));
                    }
                }
            }
            Action::MouseUp { .. } => {
                if ui.overlay.is_none() && ui.canvas_mode == CanvasMode::Graph {
                    if let Some(idx) = ui.graph_sim.dragging_node.take() {
                        if !ui.graph_sim.drag_moved && ui.graph_sim.drag_started_selected {
                            if let Some(node) = self.graph.nodes.get(idx).cloned() {
                                self.open_note(ui, &node.id, true);
                            }
                        } else if ui.graph_sim.drag_moved {
                            // Let direct 1-hop neighbors gently settle after drag release
                            ui.graph_sim.energy = ui.graph_sim.energy.max(0.015);
                            ui.graph_sim.settle_frames = 0;
                        }
                    }
                    ui.graph_sim.panning = false;
                    ui.graph_sim.drag_moved = false;
                    ui.graph_sim.last_mouse = None;
                }
            }
            Action::MouseMove { col, row } => {
                if ui.overlay.is_none() && ui.canvas_mode == CanvasMode::Graph {
                    ui.graph_sim.hovered_node = match ui.last_hitmap.hit(row, col) {
                        Some(HitTarget::GraphNode(idx)) => Some(idx),
                        _ => None,
                    };
                }
            }
            Action::MouseScrollUp { col, row } => {
                if self.is_over_graph_view(col, row, ui) {
                    ui.graph_sim
                        .zoom_at(col, row, crate::ui::graph_view::GRAPH_WHEEL_FACTOR);
                } else {
                    match self.scroll_pane_at(col, row, ui) {
                        Focus::Sidebar => {
                            ui.sidebar_selected = ui.sidebar_selected.saturating_sub(1);
                        }
                        Focus::Inspector => {
                            ui.inspector_selected = ui.inspector_selected.saturating_sub(1);
                        }
                        Focus::Canvas | Focus::Overlay => {
                            ui.canvas_follow_selected = false;
                            ui.canvas_scroll = ui.canvas_scroll.saturating_sub(3);
                        }
                    }
                }
            }
            Action::MouseScrollDown { col, row } => {
                if self.is_over_graph_view(col, row, ui) {
                    ui.graph_sim
                        .zoom_at(col, row, 1.0 / crate::ui::graph_view::GRAPH_WHEEL_FACTOR);
                } else {
                    match self.scroll_pane_at(col, row, ui) {
                        Focus::Sidebar => {
                            let len = build_sidebar_entries(self, ui).len();
                            if len > 0 {
                                ui.sidebar_selected = (ui.sidebar_selected + 1).min(len - 1);
                            }
                        }
                        Focus::Inspector => {
                            if let Some(d) = &self.active_note {
                                let total = Self::inspector_item_count(d);
                                if total > 0 {
                                    ui.inspector_selected =
                                        (ui.inspector_selected + 1).min(total - 1);
                                }
                            }
                        }
                        Focus::Canvas | Focus::Overlay => {
                            ui.canvas_follow_selected = false;
                            ui.canvas_scroll = ui.canvas_scroll.saturating_add(3);
                        }
                    }
                }
            }

            _ => {}
        }
    }

    fn activate_sidebar_selected(&mut self, ui: &mut UiState) {
        let entries = build_sidebar_entries(self, ui);
        let Some(entry) = entries.get(ui.sidebar_selected).cloned() else {
            return;
        };
        match entry {
            SidebarEntry::Folder { path, .. } => {
                if !ui.collapsed_folders.remove(&path) {
                    ui.collapsed_folders.insert(path);
                }
                // Collapsing hides the whole subtree, so keep the selection inside the list.
                self.clamp_selections(ui);
            }
            SidebarEntry::Note { rel_path, .. } => {
                self.open_note(ui, &rel_path, true);
                ui.focus = Focus::Canvas;
            }
            SidebarEntry::HtmlFile { rel_path, .. } => {
                self.open_html_file(ui, &rel_path);
            }
            SidebarEntry::Tag { tag, .. } => {
                if ui.active_tag_filter.as_deref() == Some(tag.as_str()) {
                    ui.active_tag_filter = None;
                } else {
                    ui.active_tag_filter = Some(tag);
                    ui.sidebar_selected = 0;
                }
                self.clamp_selections(ui);
            }
            SidebarEntry::TagHeader { .. } => {
                ui.active_tag_filter = None;
                ui.sidebar_selected = 0;
                self.clamp_selections(ui);
            }
            SidebarEntry::Heading { line_no, .. } => {
                self.dispatch_core(Action::JumpToHeadingLine(line_no), ui);
            }
            SidebarEntry::Task {
                note_path, line_no, ..
            } => {
                self.open_note(ui, &note_path, true);
                self.dispatch_core(Action::JumpToHeadingLine(line_no), ui);
            }
        }
    }

    fn activate_inspector_selected(&mut self, ui: &mut UiState) {
        let Some(detail) = self.active_note.as_ref() else {
            return;
        };
        let bl_len = detail.backlinks.len();
        let ol_len = detail.outgoing_links.len();
        if ui.inspector_selected < bl_len {
            let src = detail.backlinks[ui.inspector_selected].source_path.clone();
            self.open_note(ui, &src, true);
        } else if ui.inspector_selected < bl_len + ol_len {
            if let Some(ol) = detail.outgoing_links.get(ui.inspector_selected - bl_len) {
                let dest = ol
                    .target_resolved_path
                    .clone()
                    .unwrap_or_else(|| ol.target_raw.clone());
                self.open_note_with(ui, &dest, true, OpenIntent::FollowLink);
            }
        } else if let Some(h) = detail
            .parsed
            .headings
            .get(ui.inspector_selected - bl_len - ol_len)
        {
            let line_no = h.line_no;
            self.dispatch_core(Action::JumpToHeadingLine(line_no), ui);
        }
    }

    fn is_over_graph_view(&self, col: u16, row: u16, ui: &UiState) -> bool {
        if ui.overlay.is_some() || ui.canvas_mode != CanvasMode::Graph {
            return false;
        }
        let pos = ratatui::layout::Position::new(col, row);
        if ui.graph_sim.pane_area.get().contains(pos)
            || ui.graph_sim.canvas_area.get().contains(pos)
        {
            return true;
        }
        matches!(
            ui.last_hitmap.hit(row, col),
            Some(
                HitTarget::GraphCanvas
                    | HitTarget::GraphNode(_)
                    | HitTarget::Pane(Focus::Canvas)
                    | HitTarget::Button(
                        ButtonId::GraphToggleScope
                            | ButtonId::GraphZoomOut
                            | ButtonId::GraphZoomIn
                            | ButtonId::GraphResetView
                    )
            )
        )
    }

    fn scroll_pane_at(&self, col: u16, row: u16, ui: &UiState) -> Focus {
        match ui.last_hitmap.hit(row, col) {
            Some(
                HitTarget::SidebarTab(_)
                | HitTarget::SidebarRow(_)
                | HitTarget::SidebarTaskCheckbox(_)
                | HitTarget::Pane(Focus::Sidebar),
            ) => Focus::Sidebar,
            Some(
                HitTarget::InspectorBacklink(_)
                | HitTarget::InspectorOutgoing(_)
                | HitTarget::InspectorOutline(_)
                | HitTarget::Pane(Focus::Inspector),
            ) => Focus::Inspector,
            Some(
                HitTarget::CanvasBlock(_)
                | HitTarget::NoteBodyEmpty
                | HitTarget::InlineEditorBody
                | HitTarget::TaskCheckbox { .. }
                | HitTarget::WikilinkBlock(_)
                | HitTarget::ImageChip(_)
                | HitTarget::HtmlChip(_)
                | HitTarget::GraphCanvas
                | HitTarget::GraphNode(_)
                | HitTarget::Pane(Focus::Canvas),
            ) => Focus::Canvas,
            _ => match ui.focus {
                Focus::Sidebar => Focus::Sidebar,
                Focus::Inspector => Focus::Inspector,
                _ => Focus::Canvas,
            },
        }
    }

    fn handle_mouse_click(&mut self, col: u16, row: u16, ui: &mut UiState) {
        if matches!(ui.overlay, Some(Overlay::HtmlPreview(_))) {
            match ui.last_hitmap.hit(row, col) {
                Some(HitTarget::OverlayBackdrop)
                | Some(HitTarget::Button(ButtonId::ConfirmCancel)) => {
                    ui.close_overlay();
                    return;
                }
                Some(HitTarget::Button(ButtonId::HtmlToggleRaw)) => {
                    self.dispatch(Action::HtmlToggleRawMode, ui);
                    return;
                }
                Some(HitTarget::Button(ButtonId::HtmlServeBrowser)) => {
                    self.dispatch(
                        Action::ServeHtmlInBrowser {
                            title: String::new(),
                            html: String::new(),
                        },
                        ui,
                    );
                    return;
                }
                _ => {
                    let outcome = if let Some(Overlay::HtmlPreview(mut ov)) = ui.overlay.take() {
                        let res = ov.handle(&Action::MouseClick { col, row });
                        match res {
                            OverlayOutcome::Close => {
                                ui.close_overlay();
                                None
                            }
                            OverlayOutcome::CloseAndDispatch(next) => {
                                ui.close_overlay();
                                Some(next)
                            }
                            OverlayOutcome::Dispatch(next) => {
                                ui.overlay = Some(Overlay::HtmlPreview(ov));
                                Some(next)
                            }
                            OverlayOutcome::Handled | OverlayOutcome::NotHandled => {
                                ui.overlay = Some(Overlay::HtmlPreview(ov));
                                None
                            }
                        }
                    } else {
                        None
                    };
                    if let Some(next_act) = outcome {
                        self.dispatch_core(next_act, ui);
                    }
                    return;
                }
            }
        }

        // Strict modal click isolation: while ANY overlay is open, only overlay targets are
        // dispatched; any click outside the modal (backdrop or None) closes it cleanly without
        // ever clicking through or changing workspace focus while the overlay remains open.
        if ui.overlay.is_some() {
            match ui.last_hitmap.hit(row, col) {
                Some(HitTarget::OverlayBody) => {}
                Some(HitTarget::OverlayItem { index }) => {
                    if let Some(ref mut ov) = ui.overlay {
                        match ov {
                            Overlay::QuickSwitcher(q) => q.selected = index,
                            Overlay::VaultSwitcher(v) => v.selected = index,
                            Overlay::Settings(s) => s.selected = index,
                            Overlay::LinkExtractor(l) => l.selected = index,
                            Overlay::FilePicker(f) => f.select(index),
                            Overlay::ContextMenu(c) => c.selected = index,
                            _ => {}
                        }
                    }
                    self.dispatch(Action::Activate, ui);
                }
                Some(HitTarget::Button(ButtonId::ConfirmOk)) => {
                    self.dispatch(Action::Input('y'), ui);
                }
                Some(HitTarget::Button(ButtonId::ConfirmCancel)) => {
                    self.dispatch(Action::CloseOverlay, ui);
                }
                _ => {
                    ui.close_overlay();
                }
            }
            return;
        }

        let Some(target) = ui.last_hitmap.hit(row, col) else {
            return;
        };
        match target {
            HitTarget::Pane(focus) => {
                if focus != Focus::Overlay {
                    if focus != Focus::Canvas
                        && ui.inline_editor.is_some()
                        && !self.finish_inline_edit(ui)
                    {
                        return;
                    }
                    ui.focus = focus;
                }
            }
            HitTarget::SidebarTab(tab) => self.dispatch_core(Action::SetSidebarTab(tab), ui),
            HitTarget::SidebarRow(idx) => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                ui.focus = Focus::Sidebar;
                ui.sidebar_selected = idx;
                self.activate_sidebar_selected(ui);
            }
            HitTarget::SidebarTaskCheckbox(idx) => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                ui.focus = Focus::Sidebar;
                ui.sidebar_selected = idx;
                let entries = build_sidebar_entries(self, ui);
                if let Some(SidebarEntry::Task {
                    note_path, line_no, ..
                }) = entries.get(idx).cloned()
                {
                    self.dispatch_core(Action::ToggleTaskAtLine { note_path, line_no }, ui);
                }
            }
            HitTarget::CanvasBlock(idx) => {
                ui.focus = Focus::Canvas;
                ui.canvas_selected_block = idx;
                let target_line = self
                    .active_note
                    .as_ref()
                    .and_then(|d| d.parsed.blocks.get(idx))
                    .map(|b| b.line_no().saturating_sub(1));
                let rel_col = col.saturating_sub(ui.last_note_content_area.x) as usize;
                self.open_inline_edit_at(ui, target_line, Some(rel_col));
            }
            HitTarget::NoteBodyEmpty => {
                ui.focus = Focus::Canvas;
                let target_line = self.active_note.as_ref().map(|d| {
                    let body_min = d.parsed.body_start_line.saturating_sub(1);
                    let total_lines = d.raw_markdown.lines().count();
                    total_lines.saturating_sub(1).max(body_min)
                });
                self.open_inline_edit_at(ui, target_line, Some(usize::MAX));
            }
            HitTarget::InlineEditorBody => {
                ui.focus = Focus::Canvas;
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.place_cursor_at_screen_pos(col, row);
                }
            }
            HitTarget::TaskCheckbox { line_no } => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                ui.focus = Focus::Canvas;
                if let Some(d) = &self.active_note {
                    if let Some(b_idx) = d
                        .parsed
                        .blocks
                        .iter()
                        .position(|b| matches!(b, NoteBlock::Task { line_no: ln, .. } if *ln == line_no))
                    {
                        ui.canvas_selected_block = b_idx;
                    }
                    let note_path = d.summary.rel_path.clone();
                    self.dispatch_core(Action::ToggleTaskAtLine { note_path, line_no }, ui);
                }
            }
            HitTarget::WikilinkBlock(idx) => {
                ui.focus = Focus::Canvas;
                ui.canvas_selected_block = idx;
                self.dispatch_core(Action::FollowSelectedWikilink, ui);
            }
            HitTarget::ImageChip(img_idx) => {
                self.dispatch_core(Action::OpenImagePreviewAt(img_idx), ui);
            }
            HitTarget::HtmlChip(block_idx) => {
                self.dispatch_core(Action::OpenHtmlBlock(block_idx), ui);
            }
            HitTarget::GraphCanvas => {
                ui.focus = Focus::Canvas;
                ui.graph_sim.panning = true;
                ui.graph_sim.dragging_node = None;
                ui.graph_sim.last_mouse = Some((col, row));
            }
            HitTarget::GraphNode(idx) => {
                ui.focus = Focus::Canvas;
                let was_selected = ui.graph_selected == idx;
                ui.graph_selected = idx;
                ui.graph_sim.dragging_node = Some(idx);
                ui.graph_sim.drag_started_selected = was_selected;
                ui.graph_sim.drag_moved = false;
                ui.graph_sim.panning = false;
                ui.graph_sim.last_mouse = Some((col, row));
            }
            HitTarget::InspectorBacklink(idx) => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                ui.focus = Focus::Inspector;
                ui.inspector_selected = idx;
                self.activate_inspector_selected(ui);
            }
            HitTarget::InspectorOutgoing(idx) => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                let bl_len = self
                    .active_note
                    .as_ref()
                    .map(|d| d.backlinks.len())
                    .unwrap_or(0);
                ui.focus = Focus::Inspector;
                ui.inspector_selected = bl_len + idx;
                self.activate_inspector_selected(ui);
            }
            HitTarget::InspectorOutline(idx) => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                let base = self
                    .active_note
                    .as_ref()
                    .map(|d| d.backlinks.len() + d.outgoing_links.len())
                    .unwrap_or(0);
                ui.focus = Focus::Inspector;
                ui.inspector_selected = base + idx;
                self.activate_inspector_selected(ui);
            }
            HitTarget::Button(btn) => match btn {
                ButtonId::VaultSwitcher => {
                    self.dispatch_core(Action::OpenOverlay(OverlayKind::VaultSwitcher), ui);
                }
                ButtonId::ToggleGraph => self.dispatch_core(Action::ToggleGraphView, ui),
                ButtonId::OpenDaily => self.dispatch_core(Action::OpenDailyNote, ui),
                ButtonId::ToggleInspector => self.dispatch_core(Action::ToggleInspector, ui),
                ButtonId::Help => self.dispatch_core(Action::OpenOverlay(OverlayKind::Help), ui),
                ButtonId::Settings => {
                    self.dispatch_core(Action::OpenOverlay(OverlayKind::Settings), ui);
                }
                ButtonId::QuickSwitcher => {
                    self.dispatch_core(Action::OpenOverlay(OverlayKind::QuickSwitcher), ui);
                }
                ButtonId::ToggleSidebar => self.dispatch_core(Action::ToggleSidebar, ui),
                ButtonId::ToggleBookmark => self.dispatch_core(Action::ToggleBookmark, ui),
                ButtonId::ToggleWidth => self.dispatch_core(Action::TogglePageWidth, ui),
                ButtonId::QuickAppend => self.dispatch_core(Action::OpenQuickAppend, ui),
                ButtonId::NewNote => self.dispatch_core(Action::OpenNewNoteModal, ui),
                ButtonId::EditNote => self.dispatch_core(Action::OpenNoteEditor, ui),
                ButtonId::ToggleEditorMode => self.dispatch_core(Action::ToggleEditorMode, ui),
                ButtonId::FinishInlineEdit => {
                    self.finish_inline_edit(ui);
                }
                ButtonId::SaveInlineEdit => {
                    self.flush_inline_save(ui);
                }
                ButtonId::EditExternal => self.dispatch(Action::RequestExternalEditor, ui),
                ButtonId::LinkExtractor => self.dispatch_core(Action::OpenLinkExtractor, ui),
                ButtonId::ToggleHrefs => self.dispatch_core(Action::ToggleShowLinkUrls, ui),
                ButtonId::ImagePreview => self.dispatch_core(Action::OpenImagePreview, ui),
                ButtonId::HtmlPreview => self.dispatch_core(Action::OpenHtmlPreview, ui),
                ButtonId::HtmlToggleRaw => self.dispatch(Action::HtmlToggleRawMode, ui),
                ButtonId::HtmlServeBrowser => self.dispatch(
                    Action::ServeHtmlInBrowser {
                        title: String::new(),
                        html: String::new(),
                    },
                    ui,
                ),
                ButtonId::ReindexNow => self.dispatch_core(Action::RefreshAll, ui),
                ButtonId::GraphToggleScope => self.dispatch_core(Action::ToggleGraphScope, ui),
                ButtonId::GraphZoomOut => self.dispatch_core(Action::GraphZoomOut, ui),
                ButtonId::GraphZoomIn => self.dispatch_core(Action::GraphZoomIn, ui),
                ButtonId::GraphResetView => self.dispatch_core(Action::GraphResetView, ui),
                ButtonId::ConfirmOk => self.dispatch(Action::Input('y'), ui),
                ButtonId::ConfirmCancel => self.dispatch(Action::CloseOverlay, ui),
            },
            HitTarget::OverlayBackdrop | HitTarget::OverlayBody | HitTarget::OverlayItem { .. } => {
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::{
        config::ImageProtocolPreference,
        img_preview::TerminalImageProtocol,
        models::BacklinkEntry,
        sync::{FocusTarget, SyncCommand, SyncController},
        ui::{self, event::key_to_action},
        vault::vault_from_dir,
    };

    fn make_valid_png_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([120, 80, 240, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    fn make_harness(root: &std::path::Path, name: &str) -> (AppState, UiState) {
        let vault = vault_from_dir(root, Some(name));
        let mut cfg = AppConfig::default();
        cfg.load_failed = true; // prevent tests from touching real user config.toml
        let storage = Storage::open_in_memory().unwrap();
        storage.sync_vault(&vault).unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
        let mut app = AppState::new(cfg.clone(), vault, storage, sync_ctrl);
        let mut ui = UiState::from_config(&cfg);
        app.reload_from_storage(&mut ui);
        (app, ui)
    }

    fn draw_frame(app: &AppState, ui: &mut UiState, width: u16, height: u16) -> HitMap {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hitmap = HitMap::new();
        terminal
            .draw(|f| {
                hitmap = ui::draw(f, app, ui);
            })
            .unwrap();
        hitmap
    }

    #[test]
    fn p0_overlay_backdrop_closes_modal_without_clickthrough_or_esc_trap() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Alpha.md"), "# Alpha\n\nBody A\n").unwrap();
        fs::write(tmp.path().join("Beta.md"), "# Beta\n\nBody B\n").unwrap();
        let (mut app, mut ui) = make_harness(tmp.path(), "BackdropVault");
        app.open_note(&mut ui, "Alpha.md", false);
        ui.focus = Focus::Canvas;

        // First draw without overlay to locate Beta's sidebar row coordinates.
        let base_map = draw_frame(&app, &mut ui, 120, 34);
        let beta_row = base_map
            .region_of(HitTarget::SidebarRow(1))
            .expect("Beta row in sidebar");

        // Open Settings overlay and redraw: underlying sidebar must NOT be in hitmap!
        app.dispatch(Action::OpenOverlay(OverlayKind::Settings), &mut ui);
        assert!(ui.overlay.is_some());
        assert_eq!(ui.focus, Focus::Overlay);
        let ov_map = draw_frame(&app, &mut ui, 120, 34);
        assert_eq!(
            ov_map.hit(beta_row.y, beta_row.x + 1),
            Some(HitTarget::OverlayBackdrop)
        );

        // Clicking outside the modal where SidebarRow(1) sits must close the overlay,
        // restore Canvas focus, and NOT switch active_note to Beta!
        app.dispatch(
            Action::MouseClick {
                col: beta_row.x + 1,
                row: beta_row.y,
            },
            &mut ui,
        );
        assert!(ui.overlay.is_none());
        assert_eq!(ui.focus, Focus::Canvas);
        assert_eq!(
            app.active_note.as_ref().unwrap().summary.rel_path,
            "Alpha.md"
        );

        // Even if focus somehow diverged while an overlay is open, Esc (`ClearTagFilter`) still closes it.
        app.dispatch(Action::OpenOverlay(OverlayKind::Help), &mut ui);
        ui.focus = Focus::Sidebar;
        app.dispatch(Action::ClearTagFilter, &mut ui);
        assert!(ui.overlay.is_none());
    }

    #[test]
    fn p0_right_click_sidebar_row_targets_clicked_note_not_active_note() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Alpha.md"), "# Alpha\n\nKeep me.\n").unwrap();
        fs::write(tmp.path().join("Beta.md"), "# Beta\n\nTrash me.\n").unwrap();
        let (mut app, mut ui) = make_harness(tmp.path(), "ContextVault");
        app.open_note(&mut ui, "Alpha.md", false);
        assert_eq!(
            app.active_note.as_ref().unwrap().summary.rel_path,
            "Alpha.md"
        );

        let hitmap = draw_frame(&app, &mut ui, 120, 34);
        let beta_row = hitmap
            .region_of(HitTarget::SidebarRow(1))
            .expect("Beta row in sidebar");

        // Right-click Beta while Alpha is active
        app.dispatch(
            Action::OpenContextMenu {
                col: beta_row.x + 2,
                row: beta_row.y,
            },
            &mut ui,
        );
        assert!(matches!(ui.overlay, Some(Overlay::ContextMenu(_))));
        assert_eq!(
            app.active_note.as_ref().unwrap().summary.rel_path,
            "Beta.md"
        );

        // Choose "Move Note to .trash/" (last item) and confirm
        if let Some(Overlay::ContextMenu(ref mut cm)) = ui.overlay {
            cm.selected = cm.items.len() - 1;
        }
        app.dispatch(Action::Activate, &mut ui);
        assert!(matches!(ui.overlay, Some(Overlay::Confirm(_))));

        // Right-clicking while Confirm modal is open must be blocked!
        app.dispatch(Action::OpenContextMenu { col: 5, row: 5 }, &mut ui);
        assert!(matches!(ui.overlay, Some(Overlay::Confirm(_))));

        // Confirm trash -> Beta is trashed, Alpha survives!
        app.dispatch(Action::Input('y'), &mut ui);
        assert!(tmp.path().join("Alpha.md").is_file());
        assert!(!tmp.path().join("Beta.md").exists());
    }

    #[test]
    fn p0_global_keymap_does_not_hijack_overlay_shortcuts() {
        let km = KeyMap;
        for (ch, expected) in [
            ('y', Action::Input('y')),
            ('G', Action::Input('G')),
            ('?', Action::Input('?')),
            ('.', Action::Input('.')),
        ] {
            let mods = if ch.is_ascii_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            let act = key_to_action(
                KeyEvent::new(KeyCode::Char(ch), mods),
                Focus::Overlay,
                CanvasMode::Note,
                &km,
                false,
            );
            assert_eq!(act, Some(expected), "char '{ch}' must reach overlay");
        }
        // Ctrl+C still triggers QuitConfirm inside overlays
        let quit = key_to_action(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Focus::Overlay,
            CanvasMode::Note,
            &km,
            false,
        );
        assert_eq!(quit, Some(Action::QuitConfirm));
    }

    #[test]
    fn p1_inspector_scroll_windowing_and_clickable_outline() {
        let tmp = tempfile::tempdir().unwrap();
        let mut md = String::from("# Doc\n\n");
        for i in 1..=12 {
            md.push_str(&format!("## Heading {i}\n\nParagraph {i} with [[Link{i}]].\n\n"));
        }
        fs::write(tmp.path().join("Doc.md"), &md).unwrap();
        let (mut app, mut ui) = make_harness(tmp.path(), "InspectorVault");
        app.open_note(&mut ui, "Doc.md", false);

        // Populate 10 backlinks to exercise backlink scroll windowing in a compact viewport
        if let Some(detail) = app.active_note.as_mut() {
            detail.backlinks = (0..10)
                .map(|i| BacklinkEntry {
                    source_path: format!("Src{i}.md"),
                    source_title: format!("Source {i}"),
                    context_snippet: format!("Snippet {i}"),
                    heading: None,
                    is_embed: false,
                })
                .collect();
        }
        ui.focus = Focus::Inspector;
        ui.inspector_selected = 9; // last backlink
        let map_bl = draw_frame(&app, &mut ui, 120, 26);
        assert!(
            map_bl.region_of(HitTarget::InspectorBacklink(9)).is_some(),
            "selected backlink #9 must scroll into viewport"
        );

        // Select last outgoing link -> must scroll into viewport
        let bl_len = app.active_note.as_ref().unwrap().backlinks.len();
        let ol_len = app.active_note.as_ref().unwrap().outgoing_links.len();
        ui.inspector_selected = bl_len + ol_len - 1;
        let map_ol = draw_frame(&app, &mut ui, 120, 26);
        assert!(
            map_ol
                .region_of(HitTarget::InspectorOutgoing(ol_len - 1))
                .is_some(),
            "selected outgoing link must scroll into viewport"
        );

        // Select last outline heading -> must scroll into viewport and clicking it jumps Canvas!
        let h_len = app.active_note.as_ref().unwrap().parsed.headings.len();
        let target_heading_idx = h_len - 1;
        let target_line = app.active_note.as_ref().unwrap().parsed.headings[target_heading_idx].line_no;
        ui.inspector_selected = bl_len + ol_len + target_heading_idx;
        let map_h = draw_frame(&app, &mut ui, 120, 26);
        let h_rect = map_h
            .region_of(HitTarget::InspectorOutline(target_heading_idx))
            .expect("selected outline heading must be visible & clickable");

        app.dispatch(
            Action::MouseClick {
                col: h_rect.x + 2,
                row: h_rect.y,
            },
            &mut ui,
        );
        assert_eq!(ui.focus, Focus::Canvas);
        let jumped_block = &app.active_note.as_ref().unwrap().parsed.blocks[ui.canvas_selected_block];
        assert!(jumped_block.line_no() >= target_line);
    }

    #[test]
    fn p1_indented_task_checkbox_and_missing_image_index_alignment() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("Attachments")).unwrap();
        fs::write(tmp.path().join("Attachments/real.png"), make_valid_png_bytes()).unwrap();
        fs::write(
            tmp.path().join("TasksAndImages.md"),
            "# TasksAndImages\n\n    - [ ] Indented task\n\n![[missing.png]]\n\n![[real.png]]\n",
        )
        .unwrap();

        let (mut app, mut ui) = make_harness(tmp.path(), "TaskImgVault");
        app.open_note(&mut ui, "TasksAndImages.md", false);
        ui.focus = Focus::Sidebar;
        let hitmap = draw_frame(&app, &mut ui, 120, 34);

        // 1. Indented task checkbox (indent=4): checkbox is at x_off = 6..8, well past old width 6!
        let task_line_no = app.active_note.as_ref().unwrap().parsed.tasks[0].line_no;
        let chk_rect = hitmap
            .region_of(HitTarget::TaskCheckbox {
                line_no: task_line_no,
            })
            .expect("indented task checkbox hit target");
        assert!(
            chk_rect.width >= 10,
            "expected hit width >= indent(4)+6=10, got {}",
            chk_rect.width
        );
        // Clicking right on the indented checkbox (`x + 7`) toggles it AND focuses Canvas!
        app.dispatch(
            Action::MouseClick {
                col: chk_rect.x + 7,
                row: chk_rect.y,
            },
            &mut ui,
        );
        assert_eq!(ui.focus, Focus::Canvas);
        assert!(app.active_note.as_ref().unwrap().parsed.tasks[0].checked);

        // 2. Missing image must NOT shift `real.png`'s ImageChip index (must be ImageChip(0) matching detail.images[0]).
        assert_eq!(app.active_note.as_ref().unwrap().images.len(), 1);
        assert!(hitmap.region_of(HitTarget::ImageChip(0)).is_some());
        assert!(
            hitmap.region_of(HitTarget::ImageChip(1)).is_none(),
            "missing image must not shift real.png to ImageChip(1)"
        );

        // 3. Sidebar Tasks tab checkbox click toggles task directly.
        app.dispatch(Action::SetSidebarTab(SidebarTab::Tasks), &mut ui);
        let task_map = draw_frame(&app, &mut ui, 120, 34);
        let sb_chk = task_map
            .region_of(HitTarget::SidebarTaskCheckbox(0))
            .expect("sidebar task checkbox");
        app.dispatch(
            Action::MouseClick {
                col: sb_chk.x + 1,
                row: sb_chk.y,
            },
            &mut ui,
        );
        assert!(!app.active_note.as_ref().unwrap().parsed.tasks[0].checked);
    }

    #[test]
    fn p1_switch_vault_resets_state_and_core_event_ignores_other_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let v1_dir = tmp.path().join("VaultOne");
        let v2_dir = tmp.path().join("VaultTwo");
        fs::create_dir_all(v1_dir.join("Sub")).unwrap();
        fs::create_dir_all(&v2_dir).unwrap();
        fs::write(v1_dir.join("Sub/One.md"), "# One\n\n#alpha\n").unwrap();
        fs::write(v2_dir.join("Two.md"), "# Two\n\nHello vault 2.\n").unwrap();

        let (mut app, mut ui) = make_harness(&v1_dir, "VaultOne");
        let v2 = vault_from_dir(&v2_dir, Some("VaultTwo"));
        app.vaults.push(v2.clone());

        ui.active_tag_filter = Some("alpha".into());
        ui.collapsed_folders.insert("Sub".into());
        ui.sidebar_selected = 5;
        ui.canvas_selected_block = 3;
        ui.canvas_scroll = 12;
        ui.inspector_selected = 4;
        ui.graph_selected = 2;

        // CoreEvent for another vault must NOT overwrite last_index_stats
        let foreign_stats = IndexStats {
            vault_id: "unrelated-vault-id".into(),
            vault_name: "Other".into(),
            total_notes: 999,
            ..IndexStats::default()
        };
        app.handle_core_event(CoreEvent::VaultIndexed { stats: foreign_stats }, &mut ui);
        assert!(app.last_index_stats.is_none());

        // SwitchVault clears filters, collapsed folders, and resets selections/scroll!
        app.dispatch(Action::SwitchVault(v2.id.clone()), &mut ui);
        assert_eq!(app.active_vault.id, v2.id);
        assert!(ui.active_tag_filter.is_none());
        assert!(ui.collapsed_folders.is_empty());
        assert_eq!(ui.sidebar_selected, 0);
        assert_eq!(ui.canvas_selected_block, 0);
        assert_eq!(ui.canvas_scroll, 0);
        assert_eq!(ui.inspector_selected, 0);
        assert_eq!(ui.graph_selected, 0);
    }

    #[test]
    fn p1_image_settings_preserve_cell_px_and_populate_inline_cache() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("Attachments")).unwrap();
        fs::write(tmp.path().join("Attachments/pic.png"), make_valid_png_bytes()).unwrap();
        fs::write(tmp.path().join("Pic.md"), "# Pic\n\n![[pic.png]]\n").unwrap();

        let (mut app, mut ui) = make_harness(tmp.path(), "ImgSettingsVault");
        app.cfg.inline_images = false;
        app.preview_cache.clear();
        app.graphics_caps.cell_px = (9, 18);
        app.graphics_caps.cell_px_exact = true;

        // Turning inline_images ON populates preview_cache for active_note
        app.dispatch(Action::ToggleInlineImages, &mut ui);
        assert!(app.cfg.inline_images);
        assert!(!app.preview_cache.is_empty());

        // SetImageProtocol preserves probed cell_px/cell_px_exact and re-populates inline cache
        app.dispatch(
            Action::SetImageProtocol(ImageProtocolPreference::Halfblock),
            &mut ui,
        );
        assert_eq!(app.graphics_caps.protocol, TerminalImageProtocol::Halfblock);
        assert_eq!(app.graphics_caps.cell_px, (9, 18));
        assert!(app.graphics_caps.cell_px_exact);
        assert!(!app.preview_cache.is_empty());
    }

    #[test]
    fn p1_inline_editor_focus_switch_and_backlink_bookmark_freshness() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("NoteA.md"), "# NoteA\n\nInitial text.\n").unwrap();
        fs::write(tmp.path().join("NoteB.md"), "# NoteB\n\nPlain.\n").unwrap();
        let (mut app, mut ui) = make_harness(tmp.path(), "InlineVault");
        app.open_note(&mut ui, "NoteA.md", false);

        // Start inline edit and make it dirty
        app.dispatch(Action::OpenNoteEditor, &mut ui);
        assert!(ui.inline_editor.is_some());
        app.dispatch(Action::Input('!'), &mut ui);
        assert!(ui.inline_editor.as_ref().unwrap().dirty);

        // External bookmark + new incoming backlink from NoteB while inline editor is dirty
        app.storage
            .toggle_bookmark(&app.active_vault.id, "NoteA.md")
            .unwrap();
        fs::write(tmp.path().join("NoteB.md"), "# NoteB\n\nSee [[NoteA]].\n").unwrap();
        app.storage.sync_vault(&app.active_vault).unwrap();
        app.reload_from_storage(&mut ui);

        let active = app.active_note.as_ref().unwrap();
        assert!(active.summary.is_bookmarked);
        assert_eq!(active.backlinks.len(), 1);
        assert!(ui.inline_editor.as_ref().unwrap().dirty);

        // Clicking Sidebar pane flushes & exits inline editor cleanly before moving focus
        let map = draw_frame(&app, &mut ui, 120, 34);
        let sb_rect = map
            .region_of(HitTarget::Pane(Focus::Sidebar))
            .expect("sidebar pane");
        app.dispatch(
            Action::MouseClick {
                col: sb_rect.x + 1,
                row: sb_rect.bottom().saturating_sub(1),
            },
            &mut ui,
        );
        assert!(ui.inline_editor.is_none());
        assert_eq!(ui.focus, Focus::Sidebar);
    }

    #[test]
    fn p1_overlay_mouse_wheel_help_clamp_and_pane_paging() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..15 {
            fs::write(tmp.path().join(format!("N{i:02}.md")), format!("# N{i:02}\n")).unwrap();
        }
        let (mut app, mut ui) = make_harness(tmp.path(), "ScrollVault");

        // PageDown/PageUp/JumpBottom/JumpTop respect Sidebar focus
        ui.focus = Focus::Sidebar;
        ui.sidebar_selected = 0;
        app.dispatch(Action::PageDown, &mut ui);
        assert_eq!(ui.sidebar_selected, 10);
        app.dispatch(Action::JumpBottom, &mut ui);
        assert_eq!(ui.sidebar_selected, 14);
        app.dispatch(Action::JumpTop, &mut ui);
        assert_eq!(ui.sidebar_selected, 0);

        // Help overlay clamps scroll and supports PageDown/PageUp/End/Home
        app.dispatch(Action::OpenOverlay(OverlayKind::Help), &mut ui);
        app.dispatch(Action::CursorEnd, &mut ui);
        draw_frame(&app, &mut ui, 100, 28);
        let clamped_after_end = match ui.overlay {
            Some(Overlay::Help { scroll }) => scroll,
            _ => panic!("expected Help overlay"),
        };
        app.dispatch(Action::MouseScrollDown { col: 50, row: 14 }, &mut ui);
        draw_frame(&app, &mut ui, 100, 28);
        if let Some(Overlay::Help { scroll }) = ui.overlay {
            assert_eq!(scroll, clamped_after_end, "Help scroll must clamp to max");
        }
        app.dispatch(Action::CursorHome, &mut ui);
        assert!(matches!(ui.overlay, Some(Overlay::Help { scroll: 0 })));
        app.dispatch(Action::CloseOverlay, &mut ui);

        // Mouse wheel works in QuickSwitcher and Settings overlays
        app.dispatch(Action::OpenOverlay(OverlayKind::Settings), &mut ui);
        app.dispatch(Action::MouseScrollDown { col: 50, row: 14 }, &mut ui);
        assert!(matches!(
            ui.overlay,
            Some(Overlay::Settings(ref s)) if s.selected == 1
        ));
        app.dispatch(Action::MouseScrollUp { col: 50, row: 14 }, &mut ui);
        assert!(matches!(
            ui.overlay,
            Some(Overlay::Settings(ref s)) if s.selected == 0
        ));
    }

    #[test]
    fn test_note_mode_mutations_skip_graph_rebuild_html_walk_and_full_vault_rescan() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Alpha.md"),
            "# Alpha\n\nLinks to [[Beta]].\n\n- [ ] Ship fix\n",
        )
        .unwrap();
        fs::write(tmp.path().join("Beta.md"), "# Beta\n\nTarget.\n").unwrap();
        fs::write(tmp.path().join("Untouched.md"), "# Untouched\n\nSteady.\n").unwrap();

        let (mut app, mut ui) = make_harness(tmp.path(), "LatencyVault");
        assert_eq!(ui.canvas_mode, CanvasMode::Note);
        assert!(
            app.graph.nodes.is_empty(),
            "graph must remain lazy (unbuilt) while in CanvasMode::Note"
        );

        // Open Alpha.md in Note mode: graph stays unbuilt until Graph View is toggled on.
        app.open_note(&mut ui, "Alpha.md", false);
        assert!(app.graph.nodes.is_empty());

        // Mark Untouched.md stale in SQLite and drop a new HTML file on disk.
        app.storage
            .invalidate_note_mtime(&app.active_vault.id, "Untouched.md")
            .unwrap();
        fs::write(tmp.path().join("late.html"), "<h1>late</h1>").unwrap();

        // 1. ToggleBookmark updates active_note + notes in-place (no HTML walk, no full vault scan).
        app.dispatch(Action::ToggleBookmark, &mut ui);
        assert!(app.active_note.as_ref().unwrap().summary.is_bookmarked);
        assert!(
            app.notes
                .iter()
                .find(|n| n.rel_path == "Alpha.md")
                .unwrap()
                .is_bookmarked
        );
        assert!(
            app.html_files.is_empty(),
            "ToggleBookmark must not trigger scan_vault_html_files"
        );

        // 2. QuickAppendSubmit, ToggleTaskAtLine, CreateNoteSubmit, DeleteActiveNoteConfirmed
        //    must use path-scoped sync_paths and skip HTML scanning & graph rebuilds.
        app.dispatch(
            Action::QuickAppendSubmit {
                text: "Appended line".into(),
            },
            &mut ui,
        );
        let task_line = app.active_note.as_ref().unwrap().parsed.tasks[0].line_no;
        app.dispatch(
            Action::ToggleTaskAtLine {
                note_path: "Alpha.md".into(),
                line_no: task_line,
            },
            &mut ui,
        );
        assert!(app.active_note.as_ref().unwrap().parsed.tasks[0].checked);

        app.dispatch(
            Action::CreateNoteSubmit {
                title: "Gamma".into(),
                folder: String::new(),
                tags: "perf".into(),
            },
            &mut ui,
        );
        assert_eq!(
            app.active_note.as_ref().unwrap().summary.rel_path,
            "Gamma.md"
        );
        app.dispatch(Action::DeleteActiveNoteConfirmed, &mut ui);
        assert!(!tmp.path().join("Gamma.md").exists());

        assert!(
            app.html_files.is_empty(),
            "Markdown-only mutations must not trigger full-vault scan_vault_html_files"
        );
        assert!(
            app.graph.nodes.is_empty(),
            "graph must not rebuild while CanvasMode::Note is active"
        );
        assert_eq!(
            app.storage
                .get_note_summary(&app.active_vault.id, "Untouched.md")
                .unwrap()
                .unwrap()
                .mtime_ms,
            -1,
            "single-note TUI mutations must not run full-vault sync_vault"
        );

        // 3. Entering Graph View builds the graph on demand.
        app.dispatch(Action::ToggleGraphView, &mut ui);
        assert_eq!(ui.canvas_mode, CanvasMode::Graph);
        assert!(app.graph.nodes.len() >= 2);
    }

    #[tokio::test]
    async fn test_refresh_all_and_switch_vault_delegate_to_live_watcher_without_blocking() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let v1_dir = tmp.path().join("V1");
        let v2_dir = tmp.path().join("V2");
        fs::create_dir_all(&v1_dir).unwrap();
        fs::create_dir_all(&v2_dir).unwrap();
        fs::write(v1_dir.join("One.md"), "# One\n").unwrap();
        fs::write(v2_dir.join("Two.md"), "# Two\n").unwrap();

        let v1 = vault_from_dir(&v1_dir, Some("V1"));
        let v2 = vault_from_dir(&v2_dir, Some("V2"));
        let mut cfg = AppConfig::default();
        cfg.load_failed = true;
        let storage = Storage::open_in_memory().unwrap();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let (sync_ctrl, diag) = crate::sync::spawn_vault_watcher_with_options(
            v1.clone(),
            storage.clone(),
            2_500,
            ev_tx,
            crate::sync::WatcherOptions::default(),
        );

        let mut app = AppState::new(cfg.clone(), v1.clone(), storage, sync_ctrl);
        app.vaults.push(v2.clone());
        let mut ui = UiState::from_config(&cfg);

        // Wait for the single initial background sync of V1.
        let init_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle_core_event(init_ev, &mut ui);
        assert_eq!(app.notes.len(), 1);
        assert_eq!(diag.initial_scans.load(Ordering::Acquire), 1);

        // 1. Action::RefreshAll delegates to background watcher instead of blocking UI thread.
        let full_before = diag.post_init_full_scans.load(Ordering::Acquire);
        app.dispatch(Action::RefreshAll, &mut ui);
        let reindex_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
            .await
            .expect("watcher must complete background RefreshAll")
            .unwrap();
        app.handle_core_event(reindex_ev, &mut ui);
        assert_eq!(
            diag.post_init_full_scans.load(Ordering::Acquire),
            full_before + 1
        );

        // 2. Action::SwitchVault delegates to background watcher (single scan, no duplicate sync_vault).
        app.dispatch(Action::SwitchVault(v2.id.clone()), &mut ui);
        let switch_ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
            .await
            .expect("watcher must complete background SwitchVault sync")
            .unwrap();
        app.handle_core_event(switch_ev, &mut ui);
        assert_eq!(app.active_vault.id, v2.id);
        assert_eq!(app.notes.len(), 1);
        assert_eq!(app.notes[0].rel_path, "Two.md");
        assert_eq!(diag.initial_scans.load(Ordering::Acquire), 2);
    }

    #[test]
    fn test_ctrl_m_toggles_live_preview_and_source_mode() {
        use crate::ui::inline_editor::EditorMode;

        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("ModeNote.md"),
            "# Mode Heading\n\nSome **bold** text.\n",
        )
        .unwrap();
        let (mut app, mut ui) = make_harness(tmp.path(), "ModeVault");
        app.open_note(&mut ui, "ModeNote.md", false);

        // Pressing `i` enters LivePreview immediately
        let act_i = key_to_action(
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
            ui.focus,
            ui.canvas_mode,
            &ui.keymap,
            ui.is_text_input_active(),
        );
        assert_eq!(act_i, Some(Action::OpenNoteEditor));
        app.dispatch(act_i.unwrap(), &mut ui);
        assert_eq!(
            ui.inline_editor.as_ref().unwrap().mode,
            EditorMode::LivePreview
        );

        // Ctrl+M maps to Action::ToggleEditorMode and toggles LivePreview -> Source -> LivePreview
        let act_m = key_to_action(
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
            ui.focus,
            ui.canvas_mode,
            &ui.keymap,
            ui.is_text_input_active(),
        );
        assert_eq!(act_m, Some(Action::ToggleEditorMode));
        app.dispatch(act_m.unwrap(), &mut ui);
        assert_eq!(ui.inline_editor.as_ref().unwrap().mode, EditorMode::Source);

        // Clicking the `[Src]` pill in the inline header bar toggles back to LivePreview
        let hitmap = draw_frame(&app, &mut ui, 120, 30);
        let pill = hitmap
            .region_of(HitTarget::Button(ButtonId::ToggleEditorMode))
            .expect("mode toggle pill must be registered in hitmap");
        app.dispatch(
            Action::MouseClick {
                col: pill.x + 1,
                row: pill.y,
            },
            &mut ui,
        );
        assert_eq!(
            ui.inline_editor.as_ref().unwrap().mode,
            EditorMode::LivePreview
        );
    }

    #[test]
    fn test_click_rendered_note_line_and_enter_launch_live_preview_without_breaking_direct_actions()
    {
        use crate::ui::inline_editor::EditorMode;

        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("ClickNote.md"),
            "# ClickNote\n\nPlain editable paragraph line\n\n- [ ] Direct toggle task\n\nSee [[TargetNote]] link\n",
        )
        .unwrap();
        fs::write(tmp.path().join("TargetNote.md"), "# TargetNote\n\nLinked!\n").unwrap();

        let (mut app, mut ui) = make_harness(tmp.path(), "ClickVault");
        app.open_note(&mut ui, "ClickNote.md", false);
        ui.focus = Focus::Canvas;

        // 1. Clicking an ordinary rendered paragraph immediately enters LivePreview at that line/col
        let hitmap = draw_frame(&app, &mut ui, 120, 30);
        let para_idx = app
            .active_note
            .as_ref()
            .unwrap()
            .parsed
            .blocks
            .iter()
            .position(|b| {
                matches!(b, NoteBlock::Paragraph { spans, .. } if spans.iter().any(|s| s.plain_text().contains("Plain editable")))
            })
            .expect("paragraph block");
        let para_rect = hitmap
            .region_of(HitTarget::CanvasBlock(para_idx))
            .expect("paragraph CanvasBlock rect");
        app.dispatch(
            Action::MouseClick {
                col: para_rect.x + 6,
                row: para_rect.y,
            },
            &mut ui,
        );
        let ed = ui
            .inline_editor
            .as_ref()
            .expect("clicking rendered paragraph must enter LivePreview immediately");
        assert_eq!(ed.mode, EditorMode::LivePreview);
        let (ln, col) = ed.cursor_line_col();
        assert_eq!(ln, 3, "cursor must land on paragraph source line 3");
        assert!(col >= 5, "cursor col should land near clicked column, got {col}");

        // Esc cleanly exits inline edit back to Canvas navigation
        app.dispatch(Action::FinishInlineEdit, &mut ui);
        assert!(ui.inline_editor.is_none());

        // 2. Pressing Enter on an ordinary text block enters LivePreview immediately
        ui.focus = Focus::Canvas;
        ui.canvas_selected_block = para_idx;
        app.dispatch(Action::Activate, &mut ui);
        assert!(ui.inline_editor.is_some());
        assert_eq!(
            ui.inline_editor.as_ref().unwrap().mode,
            EditorMode::LivePreview
        );
        app.dispatch(Action::FinishInlineEdit, &mut ui);
        assert!(ui.inline_editor.is_none());

        // 3. Direct click on checkbox STILL toggles task without entering inline editor
        let hitmap2 = draw_frame(&app, &mut ui, 120, 30);
        let task_ln = app.active_note.as_ref().unwrap().parsed.tasks[0].line_no;
        let cb_rect = hitmap2
            .region_of(HitTarget::TaskCheckbox { line_no: task_ln })
            .expect("task checkbox rect");
        app.dispatch(
            Action::MouseClick {
                col: cb_rect.x + 2,
                row: cb_rect.y,
            },
            &mut ui,
        );
        assert!(ui.inline_editor.is_none());
        assert!(app.active_note.as_ref().unwrap().parsed.tasks[0].checked);

        // 4. Direct click on wikilink block STILL navigates to TargetNote without entering inline editor
        let hitmap3 = draw_frame(&app, &mut ui, 120, 30);
        let wl_idx = app
            .active_note
            .as_ref()
            .unwrap()
            .parsed
            .blocks
            .iter()
            .position(|b| b.first_wikilink().is_some())
            .expect("wikilink block");
        let wl_rect = hitmap3
            .region_of(HitTarget::WikilinkBlock(wl_idx))
            .expect("wikilink rect");
        app.dispatch(
            Action::MouseClick {
                col: wl_rect.x + 4,
                row: wl_rect.y,
            },
            &mut ui,
        );
        assert!(ui.inline_editor.is_none());
        assert_eq!(
            app.active_note.as_ref().unwrap().summary.rel_path,
            "TargetNote.md"
        );
    }

    #[test]
    fn test_esc_after_live_edit_keeps_typed_text_visible_across_long_notes_frontmatter_and_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let mut long_md = String::from("---\ntags: [work]\n---\n# Long Note\n\n");
        for i in 1..=25 {
            long_md.push_str(&format!("Paragraph line {i}\n\n"));
        }
        long_md.push_str("- Parent item\n  - Nested child\n\n  Continuation paragraph after nested list\n");
        fs::write(tmp.path().join("Long.md"), &long_md).unwrap();

        let (mut app, mut ui) = make_harness(tmp.path(), "LongVault");
        app.open_note(&mut ui, "Long.md", false);

        // 1. Continuation paragraph inside list item after nested list is NOT dropped by parser.
        let body_start_line = {
            let detail = app.active_note.as_ref().unwrap();
            assert!(
                detail.parsed.body_plain.contains("Continuation paragraph after nested list"),
                "continuation paragraph after nested list must not be dropped"
            );
            detail.parsed.body_start_line
        };

        // 2. Pressing `e`/`i` (`OpenNoteEditor`) on a note with YAML frontmatter lands cursor on
        //    the first body line (AFTER closing `---`), NOT on line 0 inside frontmatter.
        app.dispatch(Action::OpenNoteEditor, &mut ui);
        let (ln_on_open, _) = ui.inline_editor.as_ref().unwrap().cursor_line_col();
        assert_eq!(
            ln_on_open, body_start_line,
            "cursor must start at first body line, not inside YAML frontmatter"
        );

        // 3. Typing at the bottom of the long note and pressing Esc scrolls `note_view` to keep
        //    the newly typed text visible on screen immediately (never clipped below viewport).
        if let Some(ref mut ed) = ui.inline_editor {
            ed.composer.move_doc_end(false);
        }
        app.dispatch(Action::Paste("\n\nTypedAtBottomVisibleOnEsc".into()), &mut ui);
        app.dispatch(Action::FinishInlineEdit, &mut ui);
        assert!(ui.inline_editor.is_none());
        assert!(ui.canvas_follow_selected);

        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let _ = crate::ui::draw(f, &app, &mut ui);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..24 {
            for x in 0..100 {
                rendered.push_str(buf[(x, y)].symbol());
            }
            rendered.push('\n');
        }
        assert!(
            rendered.contains("TypedAtBottomVisibleOnEsc"),
            "newly typed text at bottom of note must remain visible immediately after Esc, got:\n{rendered}"
        );
    }
}
