//! Mouse click hit-target resolution and action dispatch.

use crate::{
    models::NoteBlock,
    ui::{
        action::Action,
        hitmap::{ButtonId, HitTarget},
        overlays::{Overlay, OverlayKind, OverlayOutcome},
        sidebar::{build_sidebar_entries, SidebarEntry},
    },
};

use super::{AppState, Focus, UiState};

impl AppState {
    pub(super) fn handle_mouse_click(&mut self, col: u16, row: u16, ui: &mut UiState) {
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
