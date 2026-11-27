//! Top-level UI action dispatcher: overlay/editor routing and thin core action router.

use crate::ui::{
    action::Action,
    inline_editor::InlineEditEvent,
    overlays::{context_menu::ContextMenuOverlay, Overlay, OverlayOutcome},
};

use super::{AppState, Focus, UiState};

impl AppState {
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

    pub(super) fn dispatch_core(&mut self, action: Action, ui: &mut UiState) {
        match action {
            Action::NextFocus
            | Action::PrevFocus
            | Action::MoveUp
            | Action::MoveDown
            | Action::PageUp
            | Action::PageDown
            | Action::JumpTop
            | Action::JumpBottom
            | Action::Activate
            | Action::MoveRight
            | Action::MoveLeft
            | Action::NavigateBack
            | Action::ToggleSidebar
            | Action::ToggleInspector
            | Action::TogglePageWidth
            | Action::SetSidebarTab(_)
            | Action::ClearTagFilter
            | Action::OpenNote(_)
            | Action::ToggleGraphView
            | Action::ToggleGraphScope
            | Action::GraphDepthInc
            | Action::GraphDepthDec
            | Action::GraphZoomIn
            | Action::GraphZoomOut
            | Action::GraphResetView
            | Action::GraphTick => self.dispatch_nav_action(action, ui),

            Action::ToggleBookmark
            | Action::OpenDailyNote
            | Action::ToggleSelectedTask
            | Action::ToggleTaskAtLine { .. }
            | Action::JumpToHeadingLine(_)
            | Action::FollowSelectedWikilink
            | Action::OpenQuickAppend
            | Action::OpenNewNoteModal
            | Action::OpenNoteEditor
            | Action::ToggleEditorMode
            | Action::SaveInlineEdit
            | Action::FinishInlineEdit
            | Action::InlineEditTick
            | Action::CreateNoteSubmit { .. }
            | Action::QuickAppendSubmit { .. }
            | Action::RequestExternalEditor
            | Action::ConfirmDeleteNote
            | Action::DeleteActiveNoteConfirmed
            | Action::OpenFilePicker
            | Action::AttachLocalFile(_)
            | Action::PasteClipboard => self.dispatch_note_action(action, ui),

            Action::OpenLinkExtractor
            | Action::ToggleShowLinkUrls
            | Action::OpenImagePreview
            | Action::OpenImagePreviewAt(_)
            | Action::OpenHtmlPreview
            | Action::OpenHtmlBlock(_)
            | Action::OpenHtmlFile(_)
            | Action::OpenHtmlContent { .. }
            | Action::ServeHtmlInBrowser { .. }
            | Action::ShowInfoToast(_)
            | Action::OpenUrl(_)
            | Action::CopyUrlToClipboard(_)
            | Action::CopySelectionOrMarkdown
            | Action::OpenOverlay(_)
            | Action::OpenContextMenu { .. }
            | Action::OpenContextMenuKeyboard
            | Action::CloseOverlay => self.dispatch_media_action(action, ui),

            Action::Quit
            | Action::QuitConfirm
            | Action::ForceQuitDiscard
            | Action::RefreshAll
            | Action::SwitchVault(_)
            | Action::SetTheme(_)
            | Action::SetPageWidth(_)
            | Action::SetImageProtocol(_)
            | Action::ToggleInlineImages
            | Action::MouseClick { .. }
            | Action::MouseDrag { .. }
            | Action::MouseUp { .. }
            | Action::MouseMove { .. }
            | Action::MouseScrollUp { .. }
            | Action::MouseScrollDown { .. } => self.dispatch_system_action(action, ui),

            _ => {}
        }
    }
}
