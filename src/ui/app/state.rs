//! Core `AppState`, `UiState`, focus/canvas mode enums, and constructors.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use bitefoundry_html::NativeHtmlServer;

use crate::{
    config::{AppConfig, PageWidthMode, VaultProfile},
    img_preview::{
        detect_graphics_caps, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol,
    },
    models::{GraphData, IndexStats, NoteDetail, NoteSummary, SidebarTab, TagCount, TaskEntry},
    storage::Storage,
    sync::SyncController,
    ui::{
        graph_view::GraphSim,
        hitmap::HitMap,
        inline_editor::InlineNoteEditor,
        keymap::KeyMap,
        overlays::Overlay,
        theme::Theme,
        toast::ToastQueue,
    },
    vault::{fs::VaultHtmlFile, merged_vaults},
};

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

pub(super) enum EditorDiskState {
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
    /// Whether the last drawn inspector had room for its Outline section. Short terminals hide
    /// it, and its headings then drop out of inspector keyboard navigation.
    pub inspector_outline_visible: bool,
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
            inspector_outline_visible: true,
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

}
