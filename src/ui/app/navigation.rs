//! Note opening, navigation history, sidebar/inspector activation, and hit-test helpers.

use crate::{
    models::CodedError,
    ui::{
        action::Action,
        hitmap::{ButtonId, HitTarget},
        sidebar::{build_sidebar_entries, tree_parent_index, SidebarEntry},
    },
    vault::fs::{create_note, is_html_file_path},
};

use super::{AppState, CanvasMode, Focus, OpenIntent, UiState};

impl AppState {
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


    pub(super) fn activate_sidebar_selected(&mut self, ui: &mut UiState) {
        let entries = build_sidebar_entries(self, ui);
        let Some(entry) = entries.get(ui.sidebar_selected).cloned() else {
            return;
        };
        match entry {
            SidebarEntry::Folder { path, .. } => self.toggle_folder(ui, path),
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

    pub(super) fn set_folder_expanded(&self, ui: &mut UiState, path: String, expanded: bool) {
        if expanded {
            ui.collapsed_folders.remove(&path);
        } else {
            ui.collapsed_folders.insert(path);
        }
        // Collapsing hides the whole subtree, so keep the selection inside the list.
        self.clamp_selections(ui);
    }

    pub(super) fn toggle_folder(&self, ui: &mut UiState, path: String) {
        let collapsed = ui.collapsed_folders.contains(&path);
        self.set_folder_expanded(ui, path, collapsed);
    }

    /// `Right` in the Files tree: expand a collapsed folder, step into an expanded folder's first
    /// child, or open the selected file.
    pub(super) fn files_tree_right(&mut self, ui: &mut UiState) {
        let entries = build_sidebar_entries(self, ui);
        let sel = ui.sidebar_selected;
        match entries.get(sel) {
            Some(SidebarEntry::Folder {
                path,
                expanded: false,
                ..
            }) => self.set_folder_expanded(ui, path.clone(), true),
            Some(SidebarEntry::Folder {
                depth,
                expanded: true,
                ..
            }) => {
                let first_child = entries.get(sel + 1).and_then(SidebarEntry::tree_depth);
                if first_child.is_some_and(|child_depth| child_depth > *depth) {
                    ui.sidebar_selected = sel + 1;
                }
            }
            Some(SidebarEntry::Note { .. } | SidebarEntry::HtmlFile { .. }) => {
                self.activate_sidebar_selected(ui);
            }
            _ => {}
        }
    }

    /// `Left` in the Files tree: collapse an expanded folder, otherwise select the parent folder.
    pub(super) fn files_tree_left(&mut self, ui: &mut UiState) {
        let entries = build_sidebar_entries(self, ui);
        let sel = ui.sidebar_selected;
        match entries.get(sel) {
            Some(SidebarEntry::Folder {
                path,
                expanded: true,
                ..
            }) => self.set_folder_expanded(ui, path.clone(), false),
            Some(_) => {
                if let Some(parent) = tree_parent_index(&entries, sel) {
                    ui.sidebar_selected = parent;
                }
            }
            None => {}
        }
    }

    pub(super) fn activate_inspector_selected(&mut self, ui: &mut UiState) {
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

    pub(super) fn is_over_graph_view(&self, col: u16, row: u16, ui: &UiState) -> bool {
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

    pub(super) fn scroll_pane_at(&self, col: u16, row: u16, ui: &UiState) -> Focus {
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

}
