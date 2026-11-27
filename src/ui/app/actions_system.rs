//! Lifecycle (quit/refresh), vault switching, settings, and pointer drag/scroll actions.

use crate::{
    img_preview::detect_graphics_caps,
    models::GraphData,
    ui::{
        action::Action,
        hitmap::HitTarget,
        overlays::{confirm::ConfirmOverlay, Overlay},
        sidebar::build_sidebar_entries,
        theme::Theme,
    },
};

use super::{AppState, CanvasMode, Focus, UiState};

impl AppState {
    pub(super) fn dispatch_system_action(&mut self, action: Action, ui: &mut UiState) {
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
                self.bump_data_gen();
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
                self.bump_data_gen();
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
                                let total = Self::inspector_item_count(d, ui);
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
}
