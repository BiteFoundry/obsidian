//! Semantic UI actions dispatched from keyboard, mouse, overlays, and commands.

use std::path::PathBuf;

use crate::{
    config::{DensityMode, ImageProtocolPreference, PageWidthMode, ThemePreset},
    models::SidebarTab,
    ui::overlays::OverlayKind,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    // Lifecycle
    Quit,
    QuitConfirm,
    ForceQuitDiscard,
    RefreshAll,

    // Navigation & Focus
    NextFocus,
    PrevFocus,
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    PageUp,
    PageDown,
    JumpTop,
    JumpBottom,
    Activate,
    Tab,
    NavigateBack,

    // Sidebar & Layout
    ToggleSidebar,
    ToggleInspector,
    TogglePageWidth,
    SetSidebarTab(SidebarTab),
    ClearTagFilter,

    // Note Canvas & Graph View
    OpenNote(String),
    ToggleGraphView,
    ToggleGraphScope,
    GraphDepthInc,
    GraphDepthDec,
    GraphZoomIn,
    GraphZoomOut,
    GraphResetView,
    GraphTick,
    ToggleBookmark,
    OpenDailyNote,
    ToggleSelectedTask,
    ToggleTaskAtLine {
        note_path: String,
        line_no: usize,
    },
    JumpToHeadingLine(usize),
    FollowSelectedWikilink,

    // Note Authoring & Mutations
    OpenQuickAppend,
    OpenNewNoteModal,
    OpenNoteEditor,
    SaveInlineEdit,
    FinishInlineEdit,
    InlineEditTick,
    CreateNoteSubmit {
        title: String,
        folder: String,
        tags: String,
    },
    QuickAppendSubmit {
        text: String,
    },
    RequestExternalEditor,
    ConfirmDeleteNote,
    DeleteActiveNoteConfirmed,

    // Media, Links & Clipboard
    OpenFilePicker,
    AttachLocalFile(PathBuf),
    PasteClipboard,
    OpenLinkExtractor,
    ToggleShowLinkUrls,
    OpenImagePreview,
    OpenImagePreviewAt(usize),
    OpenHtmlPreview,
    OpenHtmlBlock(usize),
    OpenHtmlFile(String),
    OpenHtmlContent {
        title: String,
        source_label: String,
        html: String,
    },
    HtmlToggleRawMode,
    ServeHtmlInBrowser {
        title: String,
        html: String,
    },
    ShowInfoToast(String),
    OpenUrl(String),
    CopyUrlToClipboard(String),
    CopySelectionOrMarkdown,

    // Vault & Overlays
    OpenOverlay(OverlayKind),
    OpenContextMenu {
        col: u16,
        row: u16,
    },
    CloseOverlay,
    OverlaySubmit,
    SwitchVault(String),

    // Settings
    SetTheme(ThemePreset),
    SetDensity(DensityMode),
    SetPageWidth(PageWidthMode),
    SetImageProtocol(ImageProtocolPreference),
    ToggleInlineImages,
    ToggleNotify,

    // Text input inside overlays
    Input(char),
    Paste(String),
    Newline,
    Backspace,
    Delete,
    Undo,
    Redo,
    CursorLeft,
    CursorRight,
    CursorUp,
    CursorDown,
    CursorHome,
    CursorEnd,

    // Mouse
    MouseClick {
        col: u16,
        row: u16,
    },
    MouseDrag {
        col: u16,
        row: u16,
    },
    MouseUp {
        col: u16,
        row: u16,
    },
    MouseMove {
        col: u16,
        row: u16,
    },
    MouseScrollUp {
        col: u16,
        row: u16,
    },
    MouseScrollDown {
        col: u16,
        row: u16,
    },
}
