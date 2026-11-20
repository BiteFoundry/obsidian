//! Obsidian-native keyboard shortcuts table (`KeyMap`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCommand {
    Quit,
    QuickSwitcher,
    VaultSwitcher,
    Settings,
    Help,
    Refresh,
    ToggleSidebar,
    ToggleInspector,
    ToggleGraph,
    OpenDaily,
    ToggleWidth,
    ToggleBookmark,
    QuickAppend,
    EditNote,
    EditExternal,
    NewNote,
    DeleteNote,
    OpenFilePicker,
    OpenContextMenu,
    OpenLinkExtractor,
    ToggleShowLinkUrls,
    OpenImagePreview,
    OpenHtmlPreview,
    CopySelection,
    PasteClipboard,
    NavigateBack,
    SmoothScrollDown,
    SmoothScrollUp,
}

#[derive(Debug, Clone, Default)]
pub struct KeyMap;

impl KeyMap {
    pub fn resolve_global(&self, key: &KeyEvent, text_input: bool) -> Option<KeyCommand> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            return Some(KeyCommand::Quit);
        }
        if ctrl && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O')) {
            return Some(KeyCommand::QuickSwitcher);
        }
        if ctrl && matches!(key.code, KeyCode::Char('k') | KeyCode::Char('K')) {
            return Some(KeyCommand::QuickSwitcher);
        }
        if ctrl && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P')) {
            return Some(KeyCommand::QuickSwitcher);
        }
        if ctrl && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S')) && !text_input {
            return Some(KeyCommand::VaultSwitcher);
        }
        if ctrl && matches!(key.code, KeyCode::Char(',')) {
            return Some(KeyCommand::Settings);
        }
        if ctrl && matches!(key.code, KeyCode::Char('b') | KeyCode::Char('B')) {
            return Some(KeyCommand::ToggleSidebar);
        }
        if ctrl && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F')) {
            return Some(KeyCommand::OpenFilePicker);
        }
        if ctrl && matches!(key.code, KeyCode::Char('e') | KeyCode::Char('E')) && !text_input {
            return Some(KeyCommand::EditExternal);
        }
        if ctrl && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V')) && !text_input {
            return Some(KeyCommand::PasteClipboard);
        }
        if ctrl && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) {
            return Some(KeyCommand::Refresh);
        }
        if ctrl && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D')) && !text_input {
            return Some(KeyCommand::SmoothScrollDown);
        }
        if ctrl && matches!(key.code, KeyCode::Char('u') | KeyCode::Char('U')) && !text_input {
            return Some(KeyCommand::SmoothScrollUp);
        }
        if alt && matches!(key.code, KeyCode::Left) {
            return Some(KeyCommand::NavigateBack);
        }
        if alt && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V')) {
            return Some(KeyCommand::OpenImagePreview);
        }

        if text_input || ctrl || alt {
            return None;
        }

        match key.code {
            KeyCode::Char('q') => Some(KeyCommand::Quit),
            KeyCode::Char('?') => Some(KeyCommand::Help),
            KeyCode::Char('[') => Some(KeyCommand::ToggleSidebar),
            KeyCode::Char('G') => Some(KeyCommand::ToggleGraph),
            KeyCode::Char('D') => Some(KeyCommand::OpenDaily),
            KeyCode::Char('B') => Some(KeyCommand::ToggleInspector),
            KeyCode::Char('w') => Some(KeyCommand::ToggleWidth),
            KeyCode::Char('b') => Some(KeyCommand::ToggleBookmark),
            KeyCode::Char('a') => Some(KeyCommand::QuickAppend),
            KeyCode::Char('n') => Some(KeyCommand::NewNote),
            KeyCode::Char('e') => Some(KeyCommand::EditNote),
            KeyCode::Char('X') if shift => Some(KeyCommand::DeleteNote),
            KeyCode::Char('u') => Some(KeyCommand::OpenLinkExtractor),
            KeyCode::Char('h') => Some(KeyCommand::ToggleShowLinkUrls),
            KeyCode::Char('H') => Some(KeyCommand::OpenHtmlPreview),
            KeyCode::Char('v') => Some(KeyCommand::OpenImagePreview),
            KeyCode::Char('y') => Some(KeyCommand::CopySelection),
            KeyCode::Char('.') => Some(KeyCommand::OpenContextMenu),
            KeyCode::Char('r') => Some(KeyCommand::Refresh),
            _ => None,
        }
    }
}

pub const HELP_SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "Workspace & Navigation",
        &[
            ("Ctrl+O / Ctrl+K / Ctrl+P", "Quick Switcher & Command Palette"),
            ("Ctrl+S", "Vault Switcher (auto-discovered & registered vaults)"),
            ("[ / Ctrl+B", "Toggle Left Sidebar"),
            ("1 2 3 4 5", "Sidebar tabs: Files · Tags · Bookmarks · Outline · Tasks"),
            ("B", "Toggle Right Inspector (Backlinks & Outgoing Links)"),
            ("Tab / Shift+Tab", "Cycle focus across Sidebar, Canvas, and Inspector"),
            ("Backspace / Alt+Left", "Navigate back in note history"),
        ],
    ),
    (
        "Obsidian Note Canvas & Knowledge Graph",
        &[
            ("G", "Toggle Interactive Knowledge Graph View (Braille Canvas)"),
            ("l / + / -", "In Graph View: toggle Local/Global scope & hop depth"),
            ("D", "Open or create Today's Daily Note"),
            ("Space / x", "Toggle live Markdown checkbox right in the .md file"),
            ("Enter", "Follow [[wikilink]], open note, or jump to heading"),
            ("a / n", "Quick Append / New Note"),
            ("e / Ctrl+E", "Edit note in terminal / Open in external $EDITOR"),
            ("b / w / h", "Toggle Bookmark / Centered-Full Width / Inline Hrefs"),
        ],
    ),
    (
        "Media, Links & System",
        &[
            ("u", "Link Extractor overlay (wikilinks & external URLs)"),
            ("v / Alt+V", "HD Image Preview modal (Kitty/iTerm2/Sixel/Halfblock)"),
            ("H", "Interactive HTML Preview modal (QuickJS, m Raw, o Browser)"),
            ("Ctrl+V / Ctrl+F", "Paste clipboard PNG / File Picker (z zoxide jump)"),
            ("y / X", "Copy code or note Markdown / Move note to .trash/"),
            ("Ctrl+, / . / ?", "Settings (5 palettes) / Context Menu / Keybinding Help"),
            ("r / q", "Reindex Vault / Quit"),
        ],
    ),
];
