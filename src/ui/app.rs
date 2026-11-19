//! Core TUI application state (`AppState`), presentation state (`UiState`), and action dispatcher.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
};

use chrono::Local;

use crate::{
    config::{AppConfig, DensityMode, PageWidthMode, VaultProfile},
    img_preview::{
        decode_and_render_for_protocol, decode_and_render_halfblocks, detect_graphics_caps,
        RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol,
    },
    models::{
        CoreEvent, GraphData, IndexStats, NoteBlock, NoteDetail, NoteSummary, SidebarTab,
        TagCount, TaskEntry, TaskFilter,
    },
    storage::Storage,
    sync::SyncController,
    ui::{
        action::Action,
        composer::read_clipboard_image_png,
        hitmap::{ButtonId, HitMap, HitTarget},
        keymap::KeyMap,
        overlays::{
            confirm::ConfirmOverlay, context_menu::ContextMenuOverlay,
            file_picker::FilePickerOverlay, image_preview::ImagePreviewOverlay,
            links::LinkExtractorOverlay, note_form::NoteFormOverlay,
            quick_switcher::QuickSwitcherOverlay, settings::SettingsOverlay,
            vault_switcher::VaultSwitcherOverlay, Overlay, OverlayKind, OverlayOutcome,
        },
        sidebar::{build_sidebar_entries, SidebarEntry},
        theme::Theme,
        toast::ToastQueue,
    },
    vault::{
        fs::{
            append_to_note, create_note, ensure_daily_note, move_note_to_trash, safe_vault_path,
            save_vault_attachment, toggle_task_at_line,
        },
        merged_vaults,
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

#[derive(Debug, Clone)]
pub struct PendingPixelGraphic {
    pub col: u16,
    pub row: u16,
    pub protocol: TerminalImageProtocol,
    pub payload: String,
}

pub struct AppState {
    pub cfg: AppConfig,
    pub vaults: Vec<VaultProfile>,
    pub active_vault: VaultProfile,
    pub storage: Storage,
    pub sync_ctrl: SyncController,
    pub notes: Vec<NoteSummary>,
    pub tags: Vec<TagCount>,
    pub tasks: Vec<TaskEntry>,
    pub active_note: Option<NoteDetail>,
    pub graph: GraphData,
    pub history: Vec<String>,
    pub preview_cache: HashMap<String, RenderedTerminalImage>,
    pub graphics_caps: TerminalGraphicsCaps,
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
    pub graph_global: bool,
    pub graph_depth: usize,
    pub graph_selected: usize,
    pub inspector_selected: usize,
    pub overlay: Option<Overlay>,
    pub toasts: ToastQueue,
    pub keymap: KeyMap,
    pub pending_pixel: Cell<Option<PendingPixelGraphic>>,
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
            graph_global: false,
            graph_depth: 2,
            graph_selected: 0,
            inspector_selected: 0,
            overlay: None,
            toasts: ToastQueue::new(),
            keymap: KeyMap,
            pending_pixel: Cell::new(None),
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
            .unwrap_or(false)
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
            tags: Vec::new(),
            tasks: Vec::new(),
            active_note: None,
            graph: GraphData::default(),
            history: Vec::new(),
            preview_cache: HashMap::new(),
            graphics_caps,
            last_index_stats: None,
            pending_external_editor: None,
        }
    }

    /// Refreshes in-memory vault note list, tags, tasks, active note detail, and graph from SQLite.
    pub fn reload_from_storage(&mut self, ui: &mut UiState) {
        let vid = self.active_vault.id.clone();
        self.notes = self.storage.list_notes(&vid, None).unwrap_or_default();
        self.tags = self.storage.list_tags(&vid).unwrap_or_default();
        self.tasks = self
            .storage
            .list_tasks(&vid, TaskFilter::All, None)
            .unwrap_or_default();

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
    }

    pub fn refresh_graph(&mut self, ui: &UiState) {
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
    }

    fn cache_inline_images(&mut self, detail: &NoteDetail) {
        if !self.cfg.inline_images {
            return;
        }
        for img in &detail.images {
            let key = format!("inline:{}", img.abs_path.to_string_lossy());
            if self.preview_cache.contains_key(&key) {
                continue;
            }
            if let Ok(bytes) = fs::read(&img.abs_path) {
                if let Ok(rendered) =
                    decode_and_render_halfblocks(&bytes, 48, 10, (19, 19, 24))
                {
                    self.preview_cache.insert(key, rendered);
                }
            }
        }
    }

    pub fn ensure_modal_image_cached(
        &mut self,
        ov: &mut ImagePreviewOverlay,
        term_w: u16,
        term_h: u16,
    ) {
        let Some(target) = ov.current().cloned() else {
            return;
        };
        let key = format!("{}@{}", target.abs_path.to_string_lossy(), ov.zoom_pct);
        if self.preview_cache.contains_key(&key) {
            return;
        }
        let (cols, rows) = ov.target_dimensions(term_w, term_h);
        match fs::read(&target.abs_path) {
            Ok(bytes) => {
                match decode_and_render_for_protocol(
                    &bytes,
                    cols,
                    rows,
                    (19, 19, 24),
                    self.graphics_caps,
                ) {
                    Ok(rendered) => {
                        self.preview_cache.insert(key, rendered);
                        ov.error = None;
                    }
                    Err(e) => ov.error = Some(format!("Failed to render image: {e}")),
                }
            }
            Err(e) => ov.error = Some(format!("Failed to read image file: {e}")),
        }
    }

    pub fn open_note(&mut self, ui: &mut UiState, note_ref: &str, push_history: bool) {
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

    pub fn handle_core_event(&mut self, ev: CoreEvent, ui: &mut UiState) {
        match ev {
            CoreEvent::VaultIndexed { stats } => {
                let changed = stats.updated > 0 || stats.removed > 0 || self.notes.is_empty();
                self.last_index_stats = Some(stats);
                if changed {
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
                        Some(action.clone())
                    }
                }
            } else {
                None
            };
            if let Some(next_act) = outcome {
                if next_act != action || matches!(next_act, Action::Quit | Action::QuitConfirm) {
                    self.dispatch_core(next_act, ui);
                }
            }
            return;
        }

        self.dispatch_core(action, ui);
    }

    fn dispatch_core(&mut self, action: Action, ui: &mut UiState) {
        match action {
            Action::Quit => ui.should_quit = true,
            Action::QuitConfirm => {
                ui.should_quit = true;
            }
            Action::RefreshAll => {
                if let Ok(stats) = self.storage.sync_vault(&self.active_vault) {
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
                        ui.canvas_scroll = 0;
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
                        ui.canvas_scroll = 0;
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
                ui.canvas_scroll = ui.canvas_scroll.saturating_sub(10);
                ui.canvas_selected_block = ui.canvas_selected_block.saturating_sub(5);
            }
            Action::PageDown => {
                ui.canvas_scroll = ui.canvas_scroll.saturating_add(10);
                if let Some(d) = &self.active_note {
                    let max_b = d.parsed.blocks.len().saturating_sub(1);
                    ui.canvas_selected_block = (ui.canvas_selected_block + 5).min(max_b);
                }
            }
            Action::JumpTop => {
                ui.canvas_selected_block = 0;
                ui.canvas_scroll = 0;
            }
            Action::JumpBottom => {
                if let Some(d) = &self.active_note {
                    ui.canvas_selected_block = d.parsed.blocks.len().saturating_sub(1);
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
                    match toggle_task_at_line(&abs, line_no) {
                        Ok(checked) => {
                            let _ = self.storage.sync_vault(&self.active_vault);
                            self.reload_from_storage(ui);
                            ui.toasts.success(if checked {
                                "Marked task [x] complete"
                            } else {
                                "Reopened task [ ]"
                            });
                        }
                        Err(e) => ui.toasts.error(format!("Task toggle failed: {e}")),
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
            Action::PasteClipboard => match read_clipboard_image_png() {
                Ok(Some((filename, bytes))) => {
                    let name = if filename.trim().is_empty() {
                        format!("pasted-{}.png", Local::now().format("%Y%m%d-%H%M%S"))
                    } else {
                        filename
                    };
                    match save_vault_attachment(&self.active_vault, &name, &bytes) {
                        Ok((saved_name, _)) => {
                            if let Some(d) = &self.active_note {
                                if let Ok(abs) =
                                    safe_vault_path(&self.active_vault.path, &d.summary.rel_path)
                                {
                                    let _ = append_to_note(&abs, &format!("\n![[{saved_name}]]"));
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
            },

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
                        ui.open_overlay(Overlay::ImagePreview(ov));
                    } else {
                        ui.toasts.info("No embedded images in this note");
                    }
                }
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
            Action::MouseScrollUp { .. } => self.dispatch_core(Action::MoveUp, ui),
            Action::MouseScrollDown { .. } => self.dispatch_core(Action::MoveDown, ui),

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

    fn handle_mouse_click(&mut self, col: u16, row: u16, ui: &mut UiState) {
        let Some(target) = ui.last_hitmap.hit(row, col) else {
            if ui.overlay.is_some() {
                ui.close_overlay();
            }
            return;
        };
        match target {
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
            HitTarget::GraphNode(idx) => {
                ui.focus = Focus::Canvas;
                if ui.graph_selected == idx {
                    if let Some(node) = self.graph.nodes.get(idx).cloned() {
                        self.open_note(ui, &node.id, true);
                    }
                } else {
                    ui.graph_selected = idx;
                }
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
                ButtonId::EditExternal => self.dispatch_core(Action::RequestExternalEditor, ui),
                ButtonId::LinkExtractor => self.dispatch_core(Action::OpenLinkExtractor, ui),
                ButtonId::ToggleHrefs => self.dispatch_core(Action::ToggleShowLinkUrls, ui),
                ButtonId::ImagePreview => self.dispatch_core(Action::OpenImagePreview, ui),
                ButtonId::ReindexNow => self.dispatch_core(Action::RefreshAll, ui),
                ButtonId::GraphToggleScope => self.dispatch_core(Action::ToggleGraphScope, ui),
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
