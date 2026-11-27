//! Storage synchronization, vault list refresh, selection clamping, and CoreEvent handling.

use std::fs;

use crate::{
    models::{CoreEvent, NoteDetail, TaskFilter},
    ui::{
        inline_editor::ExternalConflict,
        overlays::{
            html_preview::{extract_note_html_candidates, HtmlPreviewSource},
            Overlay,
        },
        sidebar::build_sidebar_entries,
    },
    vault::fs::{content_sha256_bytes, safe_vault_path, scan_vault_html_files},
};

use super::{state::EditorDiskState, AppState, CanvasMode, UiState};

impl AppState {
    pub(super) fn inspect_editor_disk_state(&self, note_path: &str, base_hash: [u8; 32]) -> EditorDiskState {
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

    pub(super) fn refresh_vault_lists(&mut self) {
        let vid = self.active_vault.id.clone();
        self.notes = self.storage.list_notes(&vid, None).unwrap_or_default();
        self.tags = self.storage.list_tags(&vid).unwrap_or_default();
        self.tasks = self
            .storage
            .list_tasks(&vid, TaskFilter::All, None)
            .unwrap_or_default();
    }

    /// Refreshes in-memory vault note list, HTML files, tags, tasks, active note detail, and (when visible) graph.
    pub fn reload_from_storage(&mut self, ui: &mut UiState) {
        self.reload_from_storage_inner(ui, true);
    }

    /// Refreshes in-memory vault state from SQLite after a Markdown-only mutation without walking the
    /// filesystem for `.html` files on the UI thread.
    pub(crate) fn reload_from_storage_without_html_scan(&mut self, ui: &mut UiState) {
        self.reload_from_storage_inner(ui, false);
    }

    fn reload_from_storage_inner(&mut self, ui: &mut UiState, refresh_html: bool) {
        let vid = self.active_vault.id.clone();
        self.refresh_vault_lists();
        if refresh_html {
            self.html_files = scan_vault_html_files(&self.active_vault.path).unwrap_or_default();
        }

        if let Some((ed_path, ed_base_hash, ed_dirty, prev_conflict)) =
            ui.inline_editor.as_ref().map(|e| {
                (
                    e.note_path.clone(),
                    e.base_content_hash,
                    e.dirty,
                    e.external_conflict.clone(),
                )
            })
        {
            let stored_bookmarked = self
                .notes
                .iter()
                .find(|n| n.rel_path == ed_path)
                .map(|n| n.is_bookmarked);
            match self.inspect_editor_disk_state(&ed_path, ed_base_hash) {
                EditorDiskState::Unchanged => {
                    if ed_dirty {
                        self.apply_live_inline_parse(ui);
                    } else if let Ok(detail) =
                        self.storage.load_note_detail(&self.active_vault, &ed_path)
                    {
                        self.cache_inline_images(&detail, ui);
                        self.active_note = Some(detail);
                    }
                }
                EditorDiskState::Modified { text, hash } => {
                    if !ed_dirty {
                        if let Some(ref mut ed) = ui.inline_editor {
                            ed.reload_from_disk(&text, hash);
                        }
                        if let Ok(detail) =
                            self.storage.load_note_detail(&self.active_vault, &ed_path)
                        {
                            self.cache_inline_images(&detail, ui);
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
            // Keep backlinks & bookmark status fresh even when inline_editor is dirty.
            if let Some(detail) = self.active_note.as_mut() {
                if detail.summary.rel_path == ed_path {
                    if let Some(bm) = stored_bookmarked {
                        detail.summary.is_bookmarked = bm;
                    }
                    if let Ok(bl) = self.storage.get_backlinks(&vid, &ed_path) {
                        detail.summary.backlink_count = bl.len();
                        detail.backlinks = bl;
                    }
                    let updated_summary = detail.summary.clone();
                    if let Some(slot) = self.notes.iter_mut().find(|n| n.rel_path == ed_path) {
                        *slot = updated_summary;
                    }
                }
            }
            if ui.canvas_mode == CanvasMode::Graph {
                self.refresh_graph(ui);
            }
            self.refresh_open_html_preview(ui);
            self.clamp_selections(ui);
            return;
        }

        let current_path = self
            .active_note
            .as_ref()
            .map(|d| d.summary.rel_path.clone())
            .or_else(|| self.notes.first().map(|n| n.rel_path.clone()));

        if let Some(rp) = current_path {
            if let Ok(detail) = self.storage.load_note_detail(&self.active_vault, &rp) {
                self.cache_inline_images(&detail, ui);
                self.active_note = Some(detail);
            } else if let Some(first) = self.notes.first() {
                let first_rp = first.rel_path.clone();
                if let Ok(detail) = self.storage.load_note_detail(&self.active_vault, &first_rp) {
                    self.cache_inline_images(&detail, ui);
                    self.active_note = Some(detail);
                } else {
                    self.active_note = None;
                }
                ui.canvas_scroll = 0;
                ui.canvas_selected_block = 0;
                ui.canvas_follow_selected = true;
                ui.inspector_selected = 0;
            } else {
                self.active_note = None;
                ui.canvas_scroll = 0;
                ui.canvas_selected_block = 0;
                ui.canvas_follow_selected = true;
                ui.inspector_selected = 0;
            }
        } else {
            self.active_note = None;
            ui.canvas_scroll = 0;
            ui.canvas_selected_block = 0;
            ui.canvas_follow_selected = true;
            ui.inspector_selected = 0;
        }
        if ui.canvas_mode == CanvasMode::Graph {
            self.refresh_graph(ui);
        }
        self.refresh_open_html_preview(ui);
        self.clamp_selections(ui);
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

    /// Selectable inspector rows: backlinks, outgoing links, then Outline headings while the
    /// Outline section is on screen.
    pub(super) fn inspector_item_count(detail: &NoteDetail, ui: &UiState) -> usize {
        let headings = if ui.inspector_outline_visible {
            detail.parsed.headings.len()
        } else {
            0
        };
        detail.backlinks.len() + detail.outgoing_links.len() + headings
    }

    pub fn clamp_inspector_selection(&self, ui: &mut UiState) {
        let len = self
            .active_note
            .as_ref()
            .map_or(0, |detail| Self::inspector_item_count(detail, ui));
        ui.inspector_selected = ui.inspector_selected.min(len.saturating_sub(1));
    }

    pub fn clamp_selections(&self, ui: &mut UiState) {
        let sidebar_len = build_sidebar_entries(self, ui).len();
        ui.sidebar_selected = ui.sidebar_selected.min(sidebar_len.saturating_sub(1));

        let graph_len = self.graph.nodes.len();
        ui.graph_selected = ui.graph_selected.min(graph_len.saturating_sub(1));

        if let Some(detail) = self.active_note.as_ref() {
            let blocks_len = detail.parsed.blocks.len();
            ui.canvas_selected_block = ui.canvas_selected_block.min(blocks_len.saturating_sub(1));

            self.clamp_inspector_selection(ui);
        } else {
            ui.canvas_selected_block = 0;
            ui.canvas_scroll = 0;
            ui.inspector_selected = 0;
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
            .get_graph(&self.active_vault, center.as_deref(), ui.graph_depth)
            .unwrap_or_default();
        ui.graph_sim.sync_from_graph(&self.graph);
        self.clamp_selections(ui);
    }


    pub fn handle_core_event(&mut self, ev: CoreEvent, ui: &mut UiState) {
        match ev {
            CoreEvent::VaultIndexed { stats } => {
                if stats.vault_id == self.active_vault.id {
                    self.last_index_stats = Some(stats);
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

}
