//! Terminal keyboard & mouse event -> `Action` translation.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::{
    models::SidebarTab,
    ui::{
        action::Action,
        app::{CanvasMode, Focus},
        keymap::{KeyCommand, KeyMap},
        overlays::OverlayKind,
    },
};

pub fn key_to_action(
    key: KeyEvent,
    focus: Focus,
    canvas_mode: CanvasMode,
    keymap: &KeyMap,
    text_input: bool,
) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // When an overlay is focused (and not editing text), `q` closes the overlay
    // while `Ctrl+C` still quits the app globally.
    if focus == Focus::Overlay && !text_input && !ctrl && !alt && key.code == KeyCode::Char('q') {
        return Some(Action::Input('q'));
    }

    if let Some(cmd) = keymap.resolve_global(&key, text_input) {
        return Some(match cmd {
            KeyCommand::Quit => Action::QuitConfirm,
            KeyCommand::QuickSwitcher => Action::OpenOverlay(OverlayKind::QuickSwitcher),
            KeyCommand::VaultSwitcher => Action::OpenOverlay(OverlayKind::VaultSwitcher),
            KeyCommand::Settings => Action::OpenOverlay(OverlayKind::Settings),
            KeyCommand::Help => Action::OpenOverlay(OverlayKind::Help),
            KeyCommand::Refresh => Action::RefreshAll,
            KeyCommand::ToggleSidebar => Action::ToggleSidebar,
            KeyCommand::ToggleInspector => Action::ToggleInspector,
            KeyCommand::ToggleGraph => Action::ToggleGraphView,
            KeyCommand::OpenDaily => Action::OpenDailyNote,
            KeyCommand::ToggleWidth => Action::TogglePageWidth,
            KeyCommand::ToggleBookmark => Action::ToggleBookmark,
            KeyCommand::QuickAppend => Action::OpenQuickAppend,
            KeyCommand::EditExternal => Action::RequestExternalEditor,
            KeyCommand::NewNote => Action::OpenNewNoteModal,
            KeyCommand::DeleteNote => Action::ConfirmDeleteNote,
            KeyCommand::OpenFilePicker => Action::OpenFilePicker,
            KeyCommand::OpenContextMenu => Action::OpenContextMenu { col: 14, row: 4 },
            KeyCommand::OpenLinkExtractor => Action::OpenLinkExtractor,
            KeyCommand::ToggleShowLinkUrls => Action::ToggleShowLinkUrls,
            KeyCommand::OpenImagePreview => Action::OpenImagePreview,
            KeyCommand::OpenHtmlPreview => Action::OpenHtmlPreview,
            KeyCommand::CopySelection => Action::CopySelectionOrMarkdown,
            KeyCommand::PasteClipboard => Action::PasteClipboard,
            KeyCommand::NavigateBack => Action::NavigateBack,
            KeyCommand::SmoothScrollDown => Action::PageDown,
            KeyCommand::SmoothScrollUp => Action::PageUp,
        });
    }

    if focus == Focus::Overlay {
        if ctrl && key.code == KeyCode::Enter
            || alt && key.code == KeyCode::Enter
            || ctrl && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            return Some(Action::OverlaySubmit);
        }
        return match key.code {
            KeyCode::Esc => Some(Action::CloseOverlay),
            KeyCode::Enter if shift => Some(Action::Newline),
            KeyCode::Enter => Some(Action::Activate),
            KeyCode::Tab => Some(Action::Tab),
            KeyCode::BackTab => Some(Action::PrevFocus),
            KeyCode::Backspace => Some(Action::Backspace),
            KeyCode::Delete => Some(Action::Delete),
            KeyCode::Up => Some(Action::MoveUp),
            KeyCode::Down => Some(Action::MoveDown),
            KeyCode::Left => Some(Action::CursorLeft),
            KeyCode::Right => Some(Action::CursorRight),
            KeyCode::Home => Some(Action::CursorHome),
            KeyCode::End => Some(Action::CursorEnd),
            KeyCode::PageUp => Some(Action::PageUp),
            KeyCode::PageDown => Some(Action::PageDown),
            KeyCode::Char(ch) if !ctrl && !alt => Some(Action::Input(ch)),
            _ => None,
        };
    }

    // Graph View specific shortcuts when viewing Knowledge Graph in Canvas
    if canvas_mode == CanvasMode::Graph && focus == Focus::Canvas {
        match key.code {
            KeyCode::Char('l') | KeyCode::Char('L') => return Some(Action::ToggleGraphScope),
            KeyCode::Char('+') | KeyCode::Char('=') => return Some(Action::GraphDepthInc),
            KeyCode::Char('-') | KeyCode::Char('_') => return Some(Action::GraphDepthDec),
            KeyCode::Char('r') | KeyCode::Char('R') | KeyCode::Char('0') => {
                return Some(Action::GraphResetView);
            }
            KeyCode::Esc => return Some(Action::ToggleGraphView),
            _ => {}
        }
    }

    // Sidebar tab number keys `1`..`5`
    if !ctrl && !alt {
        match key.code {
            KeyCode::Char('1') => return Some(Action::SetSidebarTab(SidebarTab::Files)),
            KeyCode::Char('2') => return Some(Action::SetSidebarTab(SidebarTab::Tags)),
            KeyCode::Char('3') => return Some(Action::SetSidebarTab(SidebarTab::Bookmarks)),
            KeyCode::Char('4') => return Some(Action::SetSidebarTab(SidebarTab::Outline)),
            KeyCode::Char('5') => return Some(Action::SetSidebarTab(SidebarTab::Tasks)),
            _ => {}
        }
    }

    match key.code {
        KeyCode::Tab => Some(Action::NextFocus),
        KeyCode::BackTab => Some(Action::PrevFocus),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::MoveDown),
        KeyCode::Left => Some(Action::MoveLeft),
        KeyCode::Right => Some(Action::MoveRight),
        KeyCode::PageUp => Some(Action::PageUp),
        KeyCode::PageDown => Some(Action::PageDown),
        KeyCode::Home | KeyCode::Char('g') => Some(Action::JumpTop),
        KeyCode::End => Some(Action::JumpBottom),
        KeyCode::Enter => Some(Action::Activate),
        KeyCode::Char(' ') | KeyCode::Char('x') => Some(Action::ToggleSelectedTask),
        KeyCode::Backspace => Some(Action::NavigateBack),
        KeyCode::Esc => Some(Action::ClearTagFilter),
        _ => None,
    }
}

pub fn mouse_to_action(mouse: MouseEvent) -> Option<Action> {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => Some(Action::MouseClick {
            col: mouse.column,
            row: mouse.row,
        }),
        MouseEventKind::Drag(MouseButton::Left) => Some(Action::MouseDrag {
            col: mouse.column,
            row: mouse.row,
        }),
        MouseEventKind::Up(MouseButton::Left) => Some(Action::MouseUp {
            col: mouse.column,
            row: mouse.row,
        }),
        MouseEventKind::Moved => Some(Action::MouseMove {
            col: mouse.column,
            row: mouse.row,
        }),
        MouseEventKind::Down(MouseButton::Right) => Some(Action::OpenContextMenu {
            col: mouse.column,
            row: mouse.row,
        }),
        MouseEventKind::ScrollUp => Some(Action::MouseScrollUp {
            col: mouse.column,
            row: mouse.row,
        }),
        MouseEventKind::ScrollDown => Some(Action::MouseScrollDown {
            col: mouse.column,
            row: mouse.row,
        }),
        _ => None,
    }
}
