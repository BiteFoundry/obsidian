//! Note creation, append, delete, daily notes, tasks, bookmarks, and attachment actions.

use std::fs;

use chrono::Local;

use crate::{
    models::{NoteBlock, SidebarTab},
    ui::{
        action::Action,
        composer::read_clipboard_image_png,
        overlays::{
            confirm::ConfirmOverlay, file_picker::FilePickerOverlay, note_form::NoteFormOverlay,
            Overlay,
        },
        sidebar::{build_sidebar_entries, SidebarEntry},
    },
    vault::fs::{
        append_to_note, create_note, ensure_daily_note, move_note_to_trash, safe_vault_path,
        save_vault_attachment, toggle_task_at_line_verified,
    },
};

use super::{AppState, CanvasMode, Focus, OpenIntent, UiState};

impl AppState {
    pub(super) fn dispatch_note_action(&mut self, action: Action, ui: &mut UiState) {
        match action {
            Action::ToggleBookmark => {
                if let Some(rp) = self.active_note.as_ref().map(|d| d.summary.rel_path.clone()) {
                    if let Ok(bm) = self.storage.toggle_bookmark(&self.active_vault.id, &rp) {
                        if let Some(detail) = self.active_note.as_mut() {
                            if detail.summary.rel_path == rp {
                                detail.summary.is_bookmarked = bm;
                            }
                        }
                        if let Some(slot) = self.notes.iter_mut().find(|n| n.rel_path == rp) {
                            slot.is_bookmarked = bm;
                        }
                        self.clamp_selections(ui);
                        ui.toasts.success(if bm {
                            "Bookmarked note"
                        } else {
                            "Removed bookmark"
                        });
                    }
                }
            }

            Action::OpenDailyNote => match ensure_daily_note(&self.active_vault, None) {
                Ok((rel_path, _, created)) => {
                    if created
                        || !self
                            .notes
                            .iter()
                            .any(|n| n.rel_path.eq_ignore_ascii_case(&rel_path))
                    {
                        let _ = self.storage.sync_paths(
                            &self.active_vault,
                            std::slice::from_ref(&rel_path),
                            &[],
                        );
                        self.refresh_vault_lists();
                    }
                    self.open_note(ui, &rel_path, true);
                    if created {
                        ui.toasts.success(format!("Created daily note: {rel_path}"));
                    }
                }
                Err(e) => ui.toasts.error(format!("Daily note failed: {e}")),
            },

            // `Space` / `x` only writes a checkbox the focused pane shows: a sidebar row never
            // falls through to the open note's (unseen) canvas selection.
            Action::ToggleSelectedTask => match ui.focus {
                Focus::Sidebar => {
                    let entries = build_sidebar_entries(self, ui);
                    match (ui.sidebar_tab, entries.get(ui.sidebar_selected).cloned()) {
                        (
                            SidebarTab::Tasks,
                            Some(SidebarEntry::Task {
                                note_path, line_no, ..
                            }),
                        ) => {
                            self.dispatch_core(Action::ToggleTaskAtLine { note_path, line_no }, ui);
                        }
                        (SidebarTab::Files, Some(SidebarEntry::Folder { path, .. })) => {
                            self.toggle_folder(ui, path);
                        }
                        _ => {}
                    }
                }
                Focus::Canvas if ui.canvas_mode == CanvasMode::Note => {
                    if let Some(d) = &self.active_note {
                        if let Some(NoteBlock::Task { line_no, .. }) =
                            d.parsed.blocks.get(ui.canvas_selected_block)
                        {
                            let note_path = d.summary.rel_path.clone();
                            let ln = *line_no;
                            self.dispatch_core(
                                Action::ToggleTaskAtLine {
                                    note_path,
                                    line_no: ln,
                                },
                                ui,
                            );
                        }
                    }
                }
                _ => {}
            },

            Action::ToggleTaskAtLine { note_path, line_no } => {
                if let Ok(abs) = safe_vault_path(&self.active_vault.path, &note_path) {
                    let expected = self
                        .active_note
                        .as_ref()
                        .filter(|d| d.summary.rel_path.eq_ignore_ascii_case(&note_path))
                        .and_then(|d| {
                            d.parsed
                                .tasks
                                .iter()
                                .find(|t| t.line_no == line_no)
                                .map(|t| (t.text.clone(), t.checked))
                        })
                        .or_else(|| {
                            self.tasks
                                .iter()
                                .find(|t| {
                                    t.note_path.eq_ignore_ascii_case(&note_path)
                                        && t.line_no == line_no
                                })
                                .map(|t| (t.text.clone(), t.checked))
                        });
                    let expected_text = expected.as_ref().map(|(txt, _)| txt.as_str());
                    let expected_checked = expected.as_ref().map(|(_, chk)| *chk);

                    match toggle_task_at_line_verified(
                        &abs,
                        line_no,
                        expected_text,
                        expected_checked,
                    ) {
                        Ok(checked) => {
                            let _ = self.storage.sync_paths(
                                &self.active_vault,
                                std::slice::from_ref(&note_path),
                                &[],
                            );
                            self.reload_from_storage_without_html_scan(ui);
                            ui.toasts.success(if checked {
                                "Marked task [x] complete"
                            } else {
                                "Reopened task [ ]"
                            });
                        }
                        Err(e) => {
                            let _ = self.storage.sync_paths(
                                &self.active_vault,
                                std::slice::from_ref(&note_path),
                                &[],
                            );
                            self.reload_from_storage_without_html_scan(ui);
                            ui.toasts.warning(format!("Task toggle skipped: {e}"));
                        }
                    }
                }
            }

            Action::JumpToHeadingLine(line_no) => {
                if let Some(d) = &self.active_note {
                    if let Some(idx) = d.parsed.blocks.iter().position(|b| b.line_no() >= line_no) {
                        ui.canvas_mode = CanvasMode::Note;
                        ui.canvas_selected_block = idx;
                        ui.canvas_scroll = 0;
                        ui.canvas_follow_selected = true;
                        ui.focus = Focus::Canvas;
                    }
                }
            }

            Action::FollowSelectedWikilink => {
                if let Some(d) = &self.active_note {
                    if let Some(block) = d.parsed.blocks.get(ui.canvas_selected_block) {
                        if let Some((target, resolved)) = block.first_wikilink() {
                            let dest = resolved.unwrap_or(target);
                            self.open_note_with(ui, &dest, true, OpenIntent::FollowLink);
                        } else if let NoteBlock::Task { line_no, .. } = block {
                            let note_path = d.summary.rel_path.clone();
                            let ln = *line_no;
                            self.dispatch_core(
                                Action::ToggleTaskAtLine {
                                    note_path,
                                    line_no: ln,
                                },
                                ui,
                            );
                        }
                    }
                }
            }

            Action::OpenQuickAppend => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                let title = self
                    .active_note
                    .as_ref()
                    .map(|d| d.summary.title.clone())
                    .unwrap_or_else(|| "Note".into());
                ui.open_overlay(Overlay::NoteForm(NoteFormOverlay::quick_append(title)));
            }
            Action::OpenNewNoteModal => {
                let default_folder = self
                    .active_note
                    .as_ref()
                    .map(|d| d.summary.folder.clone())
                    .unwrap_or_default();
                ui.open_overlay(Overlay::NoteForm(NoteFormOverlay::new_note(default_folder)));
            }
            Action::OpenNoteEditor => {
                self.toggle_inline_edit(ui);
            }
            Action::ToggleEditorMode => {
                if let Some(ref mut ed) = ui.inline_editor {
                    ed.toggle_mode();
                }
            }
            Action::SaveInlineEdit => {
                self.flush_inline_save(ui);
            }
            Action::FinishInlineEdit => {
                self.finish_inline_edit(ui);
            }
            Action::InlineEditTick => {
                self.tick_inline_autosave(ui);
            }
            Action::CreateNoteSubmit {
                title,
                folder,
                tags,
            } => {
                let tag_vec: Vec<String> = tags
                    .split([',', ' '])
                    .map(|s| s.trim().trim_start_matches('#').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let folder_opt = if folder.trim().is_empty() {
                    None
                } else {
                    Some(folder.as_str())
                };
                match create_note(&self.active_vault.path, &title, folder_opt, None, &tag_vec) {
                    Ok((rel_path, _)) => {
                        let _ = self.storage.sync_paths(
                            &self.active_vault,
                            std::slice::from_ref(&rel_path),
                            &[],
                        );
                        self.refresh_vault_lists();
                        self.open_note(ui, &rel_path, true);
                        ui.toasts.success(format!("Created {rel_path}"));
                    }
                    Err(e) => ui.toasts.error(format!("Create failed: {e}")),
                }
            }
            Action::QuickAppendSubmit { text } => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                if let Some(d) = &self.active_note {
                    let rp = d.summary.rel_path.clone();
                    if let Ok(abs) = safe_vault_path(&self.active_vault.path, &rp) {
                        match append_to_note(&abs, &text) {
                            Ok(()) => {
                                let _ = self.storage.sync_paths(
                                    &self.active_vault,
                                    std::slice::from_ref(&rp),
                                    &[],
                                );
                                self.reload_from_storage_without_html_scan(ui);
                                ui.toasts.success("Appended to note");
                            }
                            Err(e) => ui.toasts.error(format!("Append failed: {e}")),
                        }
                    }
                }
            }

            Action::RequestExternalEditor => {
                if let Some(d) = &self.active_note {
                    if let Ok(abs) = safe_vault_path(&self.active_vault.path, &d.summary.rel_path) {
                        self.pending_external_editor = Some(abs);
                    }
                }
            }

            Action::ConfirmDeleteNote => {
                if let Some(d) = &self.active_note {
                    ui.open_overlay(Overlay::Confirm(ConfirmOverlay::new(
                        "Move Note to .trash/",
                        format!("Move '{}' to .trash/?", d.summary.rel_path),
                        "Move to Trash",
                        Action::DeleteActiveNoteConfirmed,
                    )));
                }
            }
            Action::DeleteActiveNoteConfirmed => {
                if let Some(d) = &self.active_note {
                    ui.inline_editor = None;
                    let rp = d.summary.rel_path.clone();
                    match move_note_to_trash(&self.active_vault.path, &rp) {
                        Ok(_) => {
                            self.active_note = None;
                            ui.canvas_scroll = 0;
                            ui.canvas_selected_block = 0;
                            ui.canvas_follow_selected = true;
                            ui.inspector_selected = 0;
                            let _ = self.storage.sync_paths(
                                &self.active_vault,
                                std::slice::from_ref(&rp),
                                &[],
                            );
                            self.reload_from_storage_without_html_scan(ui);
                            ui.toasts.success(format!("Moved {rp} to .trash/"));
                        }
                        Err(e) => ui.toasts.error(format!("Delete failed: {e}")),
                    }
                }
            }

            Action::OpenFilePicker => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                ui.open_overlay(Overlay::FilePicker(FilePickerOverlay::new_in_dir(
                    self.active_vault.path.clone(),
                )));
            }
            Action::AttachLocalFile(src_path) => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                if let Ok(bytes) = fs::read(&src_path) {
                    let fname = src_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("attachment.bin");
                    match save_vault_attachment(&self.active_vault, fname, &bytes) {
                        Ok((saved_name, _)) => {
                            if let Some(rp) =
                                self.active_note.as_ref().map(|d| d.summary.rel_path.clone())
                            {
                                if let Ok(abs) = safe_vault_path(&self.active_vault.path, &rp) {
                                    let _ = append_to_note(&abs, &format!("\n![[{saved_name}]]"));
                                    let _ = self.storage.sync_paths(
                                        &self.active_vault,
                                        std::slice::from_ref(&rp),
                                        &[],
                                    );
                                    self.reload_from_storage_without_html_scan(ui);
                                    ui.toasts.success(format!("Embedded ![[{saved_name}]]"));
                                }
                            }
                        }
                        Err(e) => ui.toasts.error(format!("Attachment save failed: {e}")),
                    }
                }
            }
            Action::PasteClipboard => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                match read_clipboard_image_png() {
                    Ok(Some((filename, bytes))) => {
                        let name = if filename.trim().is_empty() {
                            format!("pasted-{}.png", Local::now().format("%Y%m%d-%H%M%S"))
                        } else {
                            filename
                        };
                        match save_vault_attachment(&self.active_vault, &name, &bytes) {
                            Ok((saved_name, _)) => {
                                if let Some(rp) =
                                    self.active_note.as_ref().map(|d| d.summary.rel_path.clone())
                                {
                                    if let Ok(abs) = safe_vault_path(&self.active_vault.path, &rp) {
                                        let _ =
                                            append_to_note(&abs, &format!("\n![[{saved_name}]]"));
                                        let _ = self.storage.sync_paths(
                                            &self.active_vault,
                                            std::slice::from_ref(&rp),
                                            &[],
                                        );
                                        self.reload_from_storage_without_html_scan(ui);
                                        ui.toasts.success(format!("Pasted ![[{saved_name}]]"));
                                    }
                                }
                            }
                            Err(e) => ui.toasts.error(format!("Clipboard save failed: {e}")),
                        }
                    }
                    Ok(None) => ui.toasts.info("No image found in clipboard"),
                    Err(e) => ui.toasts.warning(format!("Clipboard error: {e}")),
                }
            }

            _ => {}
        }
    }
}
