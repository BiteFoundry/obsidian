//! Core TUI application state (`AppState`), presentation state (`UiState`), and action dispatcher.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
};

use bitefoundry_html::{sniff_content_kind, ContentKind, NativeHtmlServer};
use chrono::Local;

use crate::{
    config::{write_atomic, AppConfig, DensityMode, PageWidthMode, VaultProfile},
    img_preview::{
        decode_and_render_for_protocol, decode_and_render_halfblocks, detect_graphics_caps,
        RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol,
    },
    models::{
        CoreEvent, GraphData, IndexStats, NoteBlock, NoteDetail, NoteSummary, OutgoingLinkEntry,
        SidebarTab, TagCount, TaskEntry, TaskFilter,
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
            html_preview::{
                extract_note_html_candidates, HtmlPreviewOverlay, HtmlPreviewSource,
            },
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
            file_mtime_and_size, hydrate_parsed_note_links, inline_image_cache_key,
            is_html_file_path, move_note_to_trash, resolve_attachment_path,
            resolve_wikilink_target, safe_vault_path, save_vault_attachment,
            scan_vault_html_files, toggle_task_at_line_verified, VaultHtmlFile,
        },
        merged_vaults,
        parser::parse_markdown,
    },
};

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
    pub density: DensityMode,
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
    pub pending_pixel: Option<PendingPixelGraphic>,
    pub cursor_pos: Option<(u16, u16)>,
    pub term_width: u16,
    pub term_height: u16,
    pub should_quit: bool,
    pub last_hitmap: HitMap,
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
            density: cfg.density,
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
            pending_pixel: None,
            cursor_pos: None,
            term_width: 120,
            term_height: 34,
            should_quit: false,
            last_hitmap: HitMap::new(),
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

    /// Refreshes in-memory vault note list, tags, tasks, active note detail, and graph from SQLite.
    pub fn reload_from_storage(&mut self, ui: &mut UiState) {
        let vid = self.active_vault.id.clone();
        self.notes = self.storage.list_notes(&vid, None).unwrap_or_default();
        self.html_files = scan_vault_html_files(&self.active_vault.path).unwrap_or_default();
        self.tags = self.storage.list_tags(&vid).unwrap_or_default();
        self.tasks = self
            .storage
            .list_tasks(&vid, TaskFilter::All, None)
            .unwrap_or_default();

        if let Some((ed_path, ed_base_hash, ed_dirty, prev_conflict)) = ui
            .inline_editor
            .as_ref()
            .map(|e| (e.note_path.clone(), e.base_content_hash, e.dirty, e.external_conflict.clone()))
        {
            match self.inspect_editor_disk_state(&ed_path, ed_base_hash) {
                EditorDiskState::Unchanged => {
                    if ed_dirty {
                        self.apply_live_inline_parse(ui);
                    } else if let Ok(detail) = self.storage.load_note_detail(&self.active_vault, &ed_path) {
                        self.cache_inline_images(&detail);
                        self.active_note = Some(detail);
                    }
                }
                EditorDiskState::Modified { text, hash } => {
                    if !ed_dirty {
                        if let Some(ref mut ed) = ui.inline_editor {
                            ed.reload_from_disk(&text, hash);
                        }
                        if let Ok(detail) = self.storage.load_note_detail(&self.active_vault, &ed_path) {
                            self.cache_inline_images(&detail);
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
            self.refresh_graph(ui);
            self.refresh_open_html_preview(ui);
            return;
        }

        let current_path = self
            .active_note
            .as_ref()
            .map(|d| d.summary.rel_path.clone())
            .or_else(|| self.notes.first().map(|n| n.rel_path.clone()));

        if let Some(rp) = current_path {
            if let Ok(detail) = self.storage.load_note_detail(&self.active_vault, &rp) {
                self.cache_inline_images(&detail);
                self.active_note = Some(detail);
            } else if let Some(first) = self.notes.first() {
                let first_rp = first.rel_path.clone();
                if let Ok(detail) = self.storage.load_note_detail(&self.active_vault, &first_rp) {
                    self.cache_inline_images(&detail);
                    self.active_note = Some(detail);
                }
            } else {
                self.active_note = None;
            }
        } else {
            self.active_note = None;
        }
        self.refresh_graph(ui);
        self.refresh_open_html_preview(ui);
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
            .get_graph(&self.active_vault.id, center.as_deref(), ui.graph_depth)
            .unwrap_or_default();
        ui.graph_sim.sync_from_graph(&self.graph);
    }

    fn cache_inline_images(&mut self, detail: &NoteDetail) {
        if !self.cfg.inline_images {
            return;
        }
        for img in &detail.images {
            let key = inline_image_cache_key(&img.abs_path);
            if self.preview_cache.contains_key(&key) {
                continue;
            }
            let prefix = format!("inline:{}:", img.abs_path.to_string_lossy());
            self.preview_cache.retain(|k, _| !k.starts_with(&prefix));
            if let Ok(bytes) = fs::read(&img.abs_path) {
                if sniff_content_kind(
                    Some(&img.mime_type),
                    Some(&img.display_name),
                    Some(&bytes),
                ) != ContentKind::RasterImage
                {
                    continue;
                }
                if let Ok(rendered) =
                    decode_and_render_halfblocks(&bytes, 48, 10, (19, 19, 24))
                {
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
    ) -> Option<HtmlPreviewOverlay> {
        let target = ov.current()?.clone();
        let (cols, rows) = ov.target_dimensions(term_w, term_h);
        let key = ImagePreviewOverlay::cache_key(&target, cols, rows, self.graphics_caps.protocol);
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
                match decode_and_render_for_protocol(
                    &bytes,
                    cols,
                    rows,
                    (19, 19, 24),
                    self.graphics_caps,
                ) {
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
        let reroute = if let Some(Overlay::ImagePreview(ref mut ov)) = ui.overlay {
            self.ensure_modal_image_cached(ov, term_w, term_h)
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
                Err(e) => ui.toasts.warning(format!("Served at {url} (browser failed: {e})")),
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

    pub fn open_note(&mut self, ui: &mut UiState, note_ref: &str, push_history: bool) {
        // Route `.html`/`.htm`/`.xhtml` targets directly into `Overlay::HtmlPreview`.
        if is_html_file_path(note_ref) {
            self.open_html_file(ui, note_ref);
            return;
        }
        // Flush any pending inline auto-save cleanly before switching notes
        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
            return;
        }
        if push_history {
            if let Some(cur) = self.active_note.as_ref().map(|d| d.summary.rel_path.clone()) {
                if !cur.eq_ignore_ascii_case(note_ref) {
                    self.history.push(cur);
                    if self.history.len() > 64 {
                        self.history.remove(0);
                    }
                }
            }
        }
        match self.storage.load_note_detail(&self.active_vault, note_ref) {
            Ok(detail) => {
                self.cache_inline_images(&detail);
                self.active_note = Some(detail);
                ui.canvas_mode = CanvasMode::Note;
                ui.canvas_selected_block = 0;
                ui.canvas_scroll = 0;
                ui.canvas_follow_selected = true;
                ui.inspector_selected = 0;
                self.refresh_graph(ui);
            }
            Err(_) => {
                // Create note on demand if user clicked an unresolved [[wikilink]]
                let clean = note_ref.strip_prefix("unresolved:").unwrap_or(note_ref);
                if let Ok((rel_path, _)) = create_note(&self.active_vault.path, clean, None, None, &[]) {
                    let _ = self.storage.sync_vault(&self.active_vault);
                    self.reload_from_storage(ui);
                    if let Ok(detail) = self.storage.load_note_detail(&self.active_vault, &rel_path) {
                        self.active_note = Some(detail);
                        ui.canvas_mode = CanvasMode::Note;
                        ui.toasts.success(format!("Created note: {rel_path}"));
                        self.refresh_graph(ui);
                    }
                } else {
                    ui.toasts.warning(format!("Note not found: {note_ref}"));
                }
            }
        }
    }

    /// Toggles in-place inline Markdown editing directly inside `note_view` (zero popup window).
    pub fn toggle_inline_edit(&mut self, ui: &mut UiState) {
        if ui.inline_editor.is_some() {
            self.finish_inline_edit(ui);
            return;
        }
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
        let initial_scroll = ui.canvas_scroll.min(u16::MAX as usize) as u16;
        ui.canvas_mode = CanvasMode::Note;
        ui.focus = Focus::Canvas;
        ui.inline_editor = Some(InlineNoteEditor::with_base_hash(
            rel_path,
            title,
            &raw,
            base_hash,
            initial_scroll,
        ));
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

        let outgoing_links: Vec<OutgoingLinkEntry> = parsed
            .wikilinks
            .iter()
            .map(|wl| {
                let resolved_path =
                    resolve_wikilink_target(&wl.target_raw, &all_paths, &aliases_map);
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
                }
            })
            .collect();

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
                    let _ = self.storage.sync_vault(&self.active_vault);
                    if let Ok(detail) = self.storage.load_note_detail(&self.active_vault, &note_path) {
                        self.cache_inline_images(&detail);
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
                if let Ok(ol) = self.storage.get_outgoing_links(&vid, &note_path) {
                    detail.summary.outgoing_count = ol.len();
                    detail.outgoing_links = ol;
                }
                let updated_summary = detail.summary.clone();
                if let Some(slot) = self.notes.iter_mut().find(|n| n.rel_path == note_path) {
                    *slot = updated_summary;
                }
            }
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
        if ui.inline_editor.as_ref().is_some_and(|e| e.dirty) {
            if !self.flush_inline_save(ui) {
                return false;
            }
        }
        if let Some(ed) = ui.inline_editor.take() {
            let disk_changed = !matches!(
                self.inspect_editor_disk_state(&ed.note_path, ed.base_content_hash),
                EditorDiskState::Unchanged
            );
            ui.canvas_scroll = ed.scroll_y as usize;
            ui.canvas_follow_selected = false;
            ui.focus = Focus::Canvas;
            if disk_changed {
                let _ = self.storage.sync_vault(&self.active_vault);
                self.reload_from_storage(ui);
            }
        }
        true
    }

    pub fn handle_core_event(&mut self, ev: CoreEvent, ui: &mut UiState) {
        match ev {
            CoreEvent::VaultIndexed { stats } => {
                self.last_index_stats = Some(stats);
                self.reload_from_storage(ui);
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
            let outcome = if let Some(mut ov) = ui.overlay.take() {
                let res = ov.handle(&action, self);
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
                        matches!(action, Action::Quit | Action::QuitConfirm)
                            .then(|| action.clone())
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
                if let Ok(stats) = self.storage.reindex_vault(&self.active_vault) {
                    let msg = format!(
                        "Indexed {} notes ({} updated, {}ms)",
                        stats.total_notes, stats.updated, stats.elapsed_ms
                    );
                    self.last_index_stats = Some(stats);
                    self.preview_cache.clear();
                    self.reload_from_storage(ui);
                    ui.toasts.success(msg);
                }
            }

            Action::NextFocus => {
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
                        let total = d.backlinks.len() + d.outgoing_links.len();
                        if total > 0 {
                            ui.inspector_selected = (ui.inspector_selected + 1).min(total - 1);
                        }
                    }
                }
                Focus::Overlay => {}
            },

            Action::PageUp => {
                ui.canvas_follow_selected = false;
                ui.canvas_scroll = ui.canvas_scroll.saturating_sub(10);
                ui.canvas_selected_block = ui.canvas_selected_block.saturating_sub(5);
            }
            Action::PageDown => {
                ui.canvas_follow_selected = false;
                ui.canvas_scroll = ui.canvas_scroll.saturating_add(10);
                if let Some(d) = &self.active_note {
                    let max_b = d.parsed.blocks.len().saturating_sub(1);
                    ui.canvas_selected_block = (ui.canvas_selected_block + 5).min(max_b);
                }
            }
            Action::JumpTop => {
                ui.canvas_selected_block = 0;
                ui.canvas_scroll = 0;
                ui.canvas_follow_selected = true;
            }
            Action::JumpBottom => {
                if let Some(d) = &self.active_note {
                    ui.canvas_selected_block = d.parsed.blocks.len().saturating_sub(1);
                    ui.canvas_follow_selected = true;
                }
            }

            Action::Activate => match ui.focus {
                Focus::Sidebar => self.activate_sidebar_selected(ui),
                Focus::Canvas => {
                    if ui.canvas_mode == CanvasMode::Graph {
                        if let Some(node) = self.graph.nodes.get(ui.graph_selected).cloned() {
                            self.open_note(ui, &node.id, true);
                        }
                    } else {
                        self.dispatch_core(Action::FollowSelectedWikilink, ui);
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
                ui.sidebar_tab = tab;
                ui.sidebar_selected = 0;
                ui.show_sidebar = true;
                ui.focus = Focus::Sidebar;
            }
            Action::ClearTagFilter => {
                if ui.active_tag_filter.is_some() {
                    ui.active_tag_filter = None;
                    ui.sidebar_selected = 0;
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
                ui.graph_sim.zoom_by(crate::ui::graph_view::GRAPH_WHEEL_FACTOR);
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
                if let Some(d) = &self.active_note {
                    let rp = d.summary.rel_path.clone();
                    if let Ok(bm) = self.storage.toggle_bookmark(&self.active_vault.id, &rp) {
                        self.reload_from_storage(ui);
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
                    let _ = self.storage.sync_vault(&self.active_vault);
                    self.reload_from_storage(ui);
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
                        note_path,
                        line_no,
                        ..
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
                        self.dispatch_core(Action::ToggleTaskAtLine { note_path, line_no: ln }, ui);
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
                            let _ = self.storage.sync_vault(&self.active_vault);
                            self.reload_from_storage(ui);
                            ui.toasts.success(if checked {
                                "Marked task [x] complete"
                            } else {
                                "Reopened task [ ]"
                            });
                        }
                        Err(e) => {
                            let _ = self.storage.sync_vault(&self.active_vault);
                            self.reload_from_storage(ui);
                            ui.toasts.warning(format!("Task toggle skipped: {e}"));
                        }
                    }
                }
            }

            Action::JumpToHeadingLine(line_no) => {
                if let Some(d) = &self.active_note {
                    if let Some(idx) = d
                        .parsed
                        .blocks
                        .iter()
                        .position(|b| b.line_no() >= line_no)
                    {
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
                            self.open_note(ui, &dest, true);
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
                        let _ = self.storage.sync_vault(&self.active_vault);
                        self.reload_from_storage(ui);
                        self.open_note(ui, &rel_path, true);
                        ui.toasts.success(format!("Created {rel_path}"));
                    }
                    Err(e) => ui.toasts.error(format!("Create failed: {e}")),
                }
            }
            Action::QuickAppendSubmit { text } => {
                if let Some(d) = &self.active_note {
                    let rp = d.summary.rel_path.clone();
                    if let Ok(abs) = safe_vault_path(&self.active_vault.path, &rp) {
                        match append_to_note(&abs, &text) {
                            Ok(()) => {
                                let _ = self.storage.sync_vault(&self.active_vault);
                                self.reload_from_storage(ui);
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
                            let _ = self.storage.sync_vault(&self.active_vault);
                            self.reload_from_storage(ui);
                            ui.toasts.success(format!("Moved {rp} to .trash/"));
                        }
                        Err(e) => ui.toasts.error(format!("Delete failed: {e}")),
                    }
                }
            }

            Action::OpenFilePicker => {
                ui.open_overlay(Overlay::FilePicker(FilePickerOverlay::new_in_dir(
                    self.active_vault.path.clone(),
                )));
            }
            Action::AttachLocalFile(src_path) => {
                if let Ok(bytes) = fs::read(&src_path) {
                    let fname = src_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("attachment.bin");
                    match save_vault_attachment(&self.active_vault, fname, &bytes) {
                        Ok((saved_name, _)) => {
                            if let Some(d) = &self.active_note {
                                if let Ok(abs) =
                                    safe_vault_path(&self.active_vault.path, &d.summary.rel_path)
                                {
                                    let _ = append_to_note(&abs, &format!("\n![[{saved_name}]]"));
                                    let _ = self.storage.sync_vault(&self.active_vault);
                                    self.reload_from_storage(ui);
                                    ui.toasts.success(format!("Embedded ![[{saved_name}]]"));
                                }
                            }
                        }
                        Err(e) => ui.toasts.error(format!("Attachment save failed: {e}")),
                    }
                }
            }
            Action::PasteClipboard => {
                match read_clipboard_image_png() {
                    Ok(Some((filename, bytes))) => {
                        let name = if filename.trim().is_empty() {
                            format!("pasted-{}.png", Local::now().format("%Y%m%d-%H%M%S"))
                        } else {
                            filename
                        };
                        match save_vault_attachment(&self.active_vault, &name, &bytes) {
                            Ok((saved_name, _)) => {
                                if let Some(d) = &self.active_note {
                                    if let Ok(abs) = safe_vault_path(
                                        &self.active_vault.path,
                                        &d.summary.rel_path,
                                    ) {
                                        let _ =
                                            append_to_note(&abs, &format!("\n![[{saved_name}]]"));
                                        let _ = self.storage.sync_vault(&self.active_vault);
                                        self.reload_from_storage(ui);
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
                ui.open_overlay(Overlay::ContextMenu(ContextMenuOverlay::default_menu(
                    col, row,
                )));
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
                    self.history.clear();
                    self.preview_cache.clear();
                    self.sync_ctrl.switch_vault(next.clone());
                    let _ = self.storage.sync_vault(&next);
                    self.reload_from_storage(ui);
                    ui.toasts
                        .success(format!("Switched vault → {}", next.display_name()));
                }
            }

            Action::SetTheme(preset) => {
                self.cfg.theme = preset;
                ui.theme = Theme::detect(preset);
                let _ = self.cfg.save();
            }
            Action::SetDensity(d) => {
                self.cfg.density = d;
                ui.density = d;
                let _ = self.cfg.save();
            }
            Action::SetPageWidth(w) => {
                self.cfg.page_width = w;
                ui.page_width = w;
                let _ = self.cfg.save();
            }
            Action::SetImageProtocol(p) => {
                self.cfg.image_protocol = p;
                self.graphics_caps = detect_graphics_caps(p);
                self.preview_cache.clear();
                let _ = self.cfg.save();
            }
            Action::ToggleInlineImages => {
                self.cfg.inline_images = !self.cfg.inline_images;
                let _ = self.cfg.save();
            }
            Action::ToggleNotify => {
                self.cfg.notify = !self.cfg.notify;
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
                            ui.graph_sim
                                .pan_by_screen_delta(col as i32 - lc as i32, row as i32 - lr as i32);
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
                                let total = d.backlinks.len() + d.outgoing_links.len();
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
            }
            SidebarEntry::TagHeader { .. } => {
                ui.active_tag_filter = None;
                ui.sidebar_selected = 0;
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
        if ui.inspector_selected < bl_len {
            let src = detail.backlinks[ui.inspector_selected].source_path.clone();
            self.open_note(ui, &src, true);
        } else if let Some(ol) = detail.outgoing_links.get(ui.inspector_selected - bl_len) {
            let dest = ol
                .target_resolved_path
                .clone()
                .unwrap_or_else(|| ol.target_raw.clone());
            self.open_note(ui, &dest, true);
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
                | HitTarget::Pane(Focus::Sidebar),
            ) => Focus::Sidebar,
            Some(
                HitTarget::InspectorBacklink(_)
                | HitTarget::InspectorOutgoing(_)
                | HitTarget::Pane(Focus::Inspector),
            ) => Focus::Inspector,
            Some(
                HitTarget::CanvasBlock(_)
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
                Some(HitTarget::Button(ButtonId::ConfirmCancel)) => {
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

        let Some(target) = ui.last_hitmap.hit(row, col) else {
            if ui.overlay.is_some() {
                ui.close_overlay();
            }
            return;
        };
        match target {
            HitTarget::Pane(focus) => {
                if focus != Focus::Overlay {
                    ui.focus = focus;
                }
            }
            HitTarget::SidebarTab(tab) => self.dispatch_core(Action::SetSidebarTab(tab), ui),
            HitTarget::SidebarRow(idx) => {
                ui.focus = Focus::Sidebar;
                ui.sidebar_selected = idx;
                self.activate_sidebar_selected(ui);
            }
            HitTarget::CanvasBlock(idx) => {
                ui.focus = Focus::Canvas;
                ui.canvas_selected_block = idx;
            }
            HitTarget::InlineEditorBody => {
                ui.focus = Focus::Canvas;
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.place_cursor_at_screen_pos(col, row);
                }
            }
            HitTarget::TaskCheckbox { line_no } => {
                if let Some(d) = &self.active_note {
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
                ui.focus = Focus::Inspector;
                ui.inspector_selected = idx;
                self.activate_inspector_selected(ui);
            }
            HitTarget::InspectorOutgoing(idx) => {
                let bl_len = self
                    .active_note
                    .as_ref()
                    .map(|d| d.backlinks.len())
                    .unwrap_or(0);
                ui.focus = Focus::Inspector;
                ui.inspector_selected = bl_len + idx;
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
            HitTarget::OverlayItem { index } => {
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
            HitTarget::OverlayBody => {}
        }
    }
}
