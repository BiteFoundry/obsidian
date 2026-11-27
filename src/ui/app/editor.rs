//! In-place inline Markdown editor lifecycle, live re-parsing, and conflict-safe autosave.

use std::{collections::HashMap, fs};

use crate::{
    config::write_atomic,
    models::{LinkStatus, OutgoingLinkEntry, TaskFilter},
    ui::inline_editor::{ExternalConflict, InlineNoteEditor},
    vault::{
        fs::{
            content_sha256_bytes, extract_local_markdown_note_link, file_mtime_and_size,
            hydrate_parsed_note_links, resolve_markdown_link_from_source,
            resolve_wikilink_from_source, safe_vault_path,
        },
        parser::parse_markdown,
    },
};

use super::{state::EditorDiskState, AppState, CanvasMode, Focus, UiState};

impl AppState {
    /// Toggles in-place inline Markdown editing directly inside `note_view` (zero popup window).
    pub fn toggle_inline_edit(&mut self, ui: &mut UiState) {
        if ui.inline_editor.is_some() {
            self.finish_inline_edit(ui);
            return;
        }
        self.open_inline_edit_at(ui, None, None);
    }

    /// Opens the in-place Live Preview inline editor directly inside `note_view`, optionally
    /// positioning the cursor at a 0-based logical line and display column.
    pub fn open_inline_edit_at(
        &mut self,
        ui: &mut UiState,
        target_line: Option<usize>,
        target_col: Option<usize>,
    ) {
        let Some(d) = self.active_note.as_ref() else {
            ui.toasts.info("Open a note to edit");
            return;
        };
        let rel_path = d.summary.rel_path.clone();
        let title = d.summary.title.clone();
        let (raw, base_hash) = safe_vault_path(&self.active_vault.path, &rel_path)
            .ok()
            .and_then(|abs| fs::read(&abs).ok())
            .map(|bytes| {
                let hash = content_sha256_bytes(&bytes);
                let text = String::from_utf8(bytes)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                (text, hash)
            })
            .unwrap_or_else(|| {
                let text = d.raw_markdown.clone();
                let hash = content_sha256_bytes(text.as_bytes());
                (text, hash)
            });
        let body_first_line = d.parsed.body_start_line.saturating_sub(1);
        let initial_scroll = ui.canvas_scroll.min(u16::MAX as usize) as u16;
        let mut editor =
            InlineNoteEditor::with_base_hash(rel_path, title, &raw, base_hash, initial_scroll);
        if ui.last_note_content_area.width >= 10 {
            editor.last_content_width = ui.last_note_content_area.width;
        }
        if let Some(line_idx) = target_line {
            editor.place_cursor_at_logical_line_col(line_idx, target_col.unwrap_or(0));
        } else if body_first_line > 0 && initial_scroll == 0 {
            editor.place_cursor_at_logical_line_col(body_first_line, 0);
        }
        ui.canvas_mode = CanvasMode::Note;
        ui.focus = Focus::Canvas;
        ui.inline_editor = Some(editor);
    }

    /// Re-parses the active note's Markdown in memory (< 0.2ms) on every keystroke so
    /// the right-side Inspector (Outline, Tags, Word Count, Links) and Sidebar Outline update live at 0ms.
    pub fn apply_live_inline_parse(&mut self, ui: &UiState) {
        let Some(ed) = ui.inline_editor.as_ref() else {
            return;
        };
        let Some(detail) = self.active_note.as_mut() else {
            return;
        };
        if detail.summary.rel_path != ed.note_path {
            return;
        }
        let markdown = ed.composer.text().to_string();
        let rel_path = detail.summary.rel_path.clone();
        let title = detail.summary.title.clone();
        let mut parsed = parse_markdown(&rel_path, &title, &markdown);

        let mut all_paths: Vec<String> = Vec::with_capacity(self.notes.len());
        let mut aliases_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut title_by_path: HashMap<String, String> = HashMap::with_capacity(self.notes.len());
        for n in &self.notes {
            all_paths.push(n.rel_path.clone());
            title_by_path.insert(n.rel_path.clone(), n.title.clone());
            if n.rel_path == rel_path {
                if !parsed.aliases.is_empty() {
                    aliases_map.insert(n.rel_path.clone(), parsed.aliases.clone());
                }
            } else if !n.aliases.is_empty() {
                aliases_map.insert(n.rel_path.clone(), n.aliases.clone());
            }
        }

        let images = hydrate_parsed_note_links(
            &self.active_vault.path,
            &rel_path,
            &mut parsed,
            &all_paths,
            &aliases_map,
        );

        let mut outgoing_links: Vec<OutgoingLinkEntry> = parsed
            .wikilinks
            .iter()
            .map(|wl| {
                let resolved_path = resolve_wikilink_from_source(
                    &wl.target_raw,
                    &rel_path,
                    &all_paths,
                    &aliases_map,
                );
                let resolved = resolved_path.is_some();
                let target_title = resolved_path
                    .as_ref()
                    .and_then(|rp| title_by_path.get(rp).cloned())
                    .unwrap_or_else(|| wl.target_raw.clone());
                OutgoingLinkEntry {
                    target_raw: wl.target_raw.clone(),
                    target_resolved_path: resolved_path,
                    target_title,
                    alias: wl.alias.clone(),
                    heading: wl.heading.clone(),
                    is_embed: wl.is_embed,
                    resolved,
                    status: if resolved {
                        LinkStatus::Resolved
                    } else {
                        LinkStatus::Missing
                    },
                    candidates: Vec::new(),
                }
            })
            .collect();
        for ml in &parsed.md_links {
            if ml.is_image {
                continue;
            }
            if let Some((target_raw, heading)) = extract_local_markdown_note_link(&ml.url) {
                let resolved_path =
                    resolve_markdown_link_from_source(&target_raw, &rel_path, &all_paths);
                let resolved = resolved_path.is_some();
                let target_title = resolved_path
                    .as_ref()
                    .and_then(|rp| title_by_path.get(rp).cloned())
                    .unwrap_or_else(|| target_raw.clone());
                let alias = (!ml.label.trim().is_empty()).then(|| ml.label.trim().to_string());
                outgoing_links.push(OutgoingLinkEntry {
                    target_raw,
                    target_resolved_path: resolved_path,
                    target_title,
                    alias,
                    heading,
                    is_embed: false,
                    resolved,
                    status: if resolved {
                        LinkStatus::Resolved
                    } else {
                        LinkStatus::Missing
                    },
                    candidates: Vec::new(),
                });
            }
        }

        detail.raw_markdown = markdown;
        detail.summary.frontmatter = parsed.frontmatter.clone();
        detail.summary.aliases = parsed.aliases.clone();
        detail.summary.tags = parsed.tags.clone();
        detail.summary.word_count = parsed.word_count;
        detail.summary.outgoing_count = outgoing_links.len();
        detail.parsed = parsed;
        detail.outgoing_links = outgoing_links;
        detail.images = images;

        let updated_summary = detail.summary.clone();
        if let Some(slot) = self
            .notes
            .iter_mut()
            .find(|n| n.rel_path == updated_summary.rel_path)
        {
            *slot = updated_summary;
        }
    }

    /// Atomically writes the inline editor buffer to disk (preserving CRLF) after verifying that
    /// the file on disk still matches the baseline recorded on open or last save, and updates ONLY
    /// that single note in SQLite via `Storage::upsert_single_note` (< 5ms, zero full vault scan).
    pub fn flush_inline_save(&mut self, ui: &mut UiState) -> bool {
        let Some((note_path, disk_md, base_hash, is_dirty)) = ui.inline_editor.as_ref().map(|ed| {
            (
                ed.note_path.clone(),
                ed.disk_markdown(),
                ed.base_content_hash,
                ed.dirty,
            )
        }) else {
            return false;
        };

        let abs = match safe_vault_path(&self.active_vault.path, &note_path) {
            Ok(p) => p,
            Err(e) => {
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.mark_error(e.to_string());
                }
                ui.toasts.error(format!("Invalid note path: {e}"));
                return false;
            }
        };

        // Guard against overwriting concurrent external edits or resurrecting deleted/moved files.
        match self.inspect_editor_disk_state(&note_path, base_hash) {
            EditorDiskState::Unchanged => {}
            EditorDiskState::Modified { text, hash } => {
                if !is_dirty {
                    if let Some(ref mut ed) = ui.inline_editor {
                        ed.reload_from_disk(&text, hash);
                    }
                    let _ = self.storage.sync_paths(
                        &self.active_vault,
                        std::slice::from_ref(&note_path),
                        &[],
                    );
                    if let Ok(detail) = self
                        .storage
                        .load_note_detail(&self.active_vault, &note_path)
                    {
                        self.cache_inline_images(&detail, ui);
                        self.active_note = Some(detail);
                    } else {
                        self.apply_live_inline_parse(ui);
                    }
                    ui.toasts.info("Reloaded externally modified note");
                    return false;
                }
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.mark_conflict(ExternalConflict::Modified);
                }
                ui.toasts.warning(
                    "Save blocked: file changed externally on disk — external text preserved and local edits kept",
                );
                return false;
            }
            EditorDiskState::Deleted => {
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.mark_conflict(ExternalConflict::Deleted);
                }
                ui.toasts.warning(
                    "Save blocked: file was deleted or moved externally — old path not recreated",
                );
                return false;
            }
            EditorDiskState::ReadError(err) => {
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.mark_error(err.clone());
                }
                ui.toasts.error(format!("Save check failed: {err}"));
                return false;
            }
        }

        if let Err(e) = write_atomic(&abs, disk_md.as_bytes()) {
            if let Some(ref mut ed) = ui.inline_editor {
                ed.mark_error(e.to_string());
            }
            ui.toasts.error(format!("Save failed: {e}"));
            return false;
        }

        let new_hash = content_sha256_bytes(disk_md.as_bytes());
        let (mtime_ms, size_bytes) = file_mtime_and_size(&abs);
        if let Err(e) = self.storage.upsert_single_note(
            &self.active_vault,
            &note_path,
            &disk_md,
            mtime_ms,
            size_bytes,
        ) {
            if let Some(ref mut ed) = ui.inline_editor {
                ed.mark_error(e.to_string());
            }
            ui.toasts.error(format!("Index update failed: {e}"));
            return false;
        }

        if let Some(ref mut ed) = ui.inline_editor {
            ed.mark_saved_with_hash(new_hash);
        }

        // Sync live parsed AST + lightweight SQLite aggregates (NO disk scan!)
        self.apply_live_inline_parse(ui);
        let vid = self.active_vault.id.clone();
        self.tags = self.storage.list_tags(&vid).unwrap_or_default();
        self.tasks = self
            .storage
            .list_tasks(&vid, TaskFilter::All, None)
            .unwrap_or_default();
        if let Some(detail) = self.active_note.as_mut() {
            if detail.summary.rel_path == note_path {
                detail.summary.mtime_ms = mtime_ms;
                detail.summary.size_bytes = size_bytes;
                if let Ok(bl) = self.storage.get_backlinks(&vid, &note_path) {
                    detail.summary.backlink_count = bl.len();
                    detail.backlinks = bl;
                }
                if let Ok(ol) = self
                    .storage
                    .get_outgoing_links(&self.active_vault, &note_path)
                {
                    detail.summary.outgoing_count = ol.len();
                    detail.outgoing_links = ol;
                }
                let updated_summary = detail.summary.clone();
                if let Some(slot) = self.notes.iter_mut().find(|n| n.rel_path == note_path) {
                    *slot = updated_summary;
                }
            }
        }
        if let Some(images) = self.active_note.as_ref().map(|d| d.images.clone()) {
            self.cache_inline_images_list(&images, ui);
        }
        if ui.canvas_mode == CanvasMode::Graph {
            self.refresh_graph(ui);
        } else {
            self.clamp_selections(ui);
        }
        true
    }

    /// Flushes debounced auto-save when due and reports whether the editor or toast changed.
    pub fn tick_inline_autosave(&mut self, ui: &mut UiState) -> bool {
        let Some(editor) = ui
            .inline_editor
            .as_ref()
            .filter(|editor| editor.is_autosave_due())
        else {
            return false;
        };
        let save_status = editor.save_status.clone();
        let toast = ui.toasts.current().map(|t| (t.kind, t.message.clone()));
        if self.flush_inline_save(ui) {
            return true;
        }
        ui.inline_editor
            .as_ref()
            .is_some_and(|editor| editor.save_status != save_status)
            || ui.toasts.current().map(|t| (t.kind, t.message.as_str()))
                != toast
                    .as_ref()
                    .map(|(kind, message)| (*kind, message.as_str()))
    }

    /// Flushes any pending inline auto-save and exits cleanly back to the rendered reader view
    /// at the exact same scroll offset on frame 0. Returns `false` if save was blocked (e.g.
    /// external conflict) so the editor stays open and local edits remain on screen.
    pub fn finish_inline_edit(&mut self, ui: &mut UiState) -> bool {
        if ui.inline_editor.as_ref().is_some_and(|e| e.dirty) && !self.flush_inline_save(ui) {
            return false;
        }
        if let Some(ed) = ui.inline_editor.take() {
            let cursor_line_1based = ed.cursor_line_col().0;
            let edited_in_session = ed.has_edited;
            let disk_changed = !matches!(
                self.inspect_editor_disk_state(&ed.note_path, ed.base_content_hash),
                EditorDiskState::Unchanged
            );
            ui.focus = Focus::Canvas;
            if disk_changed {
                let _ = self.storage.sync_paths(
                    &self.active_vault,
                    std::slice::from_ref(&ed.note_path),
                    &[],
                );
                self.reload_from_storage_without_html_scan(ui);
            }
            if let Some(detail) = self.active_note.as_ref() {
                if !detail.parsed.blocks.is_empty() {
                    let target_block = detail
                        .parsed
                        .blocks
                        .iter()
                        .rposition(|b| b.line_no() <= cursor_line_1based)
                        .unwrap_or(0);
                    ui.canvas_selected_block = target_block;
                }
            }
            if edited_in_session {
                ui.canvas_follow_selected = true;
            } else {
                ui.canvas_scroll = ed.scroll_y as usize;
                ui.canvas_follow_selected = false;
            }
        }
        true
    }

}
