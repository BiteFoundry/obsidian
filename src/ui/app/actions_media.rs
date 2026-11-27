//! Media preview, link extractor, clipboard/URL, overlay, and context menu actions.

use std::fs;

use bitefoundry_html::{sniff_content_kind, ContentKind};

use crate::{
    models::NoteBlock,
    ui::{
        action::Action,
        hitmap::HitTarget,
        overlays::{
            context_menu::ContextMenuOverlay,
            html_preview::{HtmlPreviewOverlay, HtmlPreviewSource},
            image_preview::ImagePreviewOverlay,
            links::LinkExtractorOverlay,
            quick_switcher::QuickSwitcherOverlay,
            settings::SettingsOverlay,
            vault_switcher::VaultSwitcherOverlay,
            Overlay, OverlayKind,
        },
        sidebar::{build_sidebar_entries, SidebarEntry},
    },
    vault::merged_vaults,
};

use super::{AppState, Focus, UiState};

impl AppState {
    pub(super) fn dispatch_media_action(&mut self, action: Action, ui: &mut UiState) {
        match action {
            Action::OpenLinkExtractor => {
                if let Some(d) = &self.active_note {
                    ui.open_overlay(Overlay::LinkExtractor(LinkExtractorOverlay::from_note(d)));
                }
            }
            Action::ToggleShowLinkUrls => {
                ui.show_inline_hrefs = !ui.show_inline_hrefs;
                self.cfg.show_inline_hrefs = ui.show_inline_hrefs;
            }
            Action::OpenImagePreview => {
                self.dispatch_core(Action::OpenImagePreviewAt(0), ui);
            }
            Action::OpenImagePreviewAt(idx) => {
                if let Some(d) = &self.active_note {
                    if let Some(ov) = ImagePreviewOverlay::from_note(d, Some(idx)) {
                        if let Some(target) = ov.current() {
                            if let Ok(bytes) = fs::read(&target.abs_path) {
                                if sniff_content_kind(
                                    Some(&target.mime_type),
                                    Some(&target.display_name),
                                    Some(&bytes),
                                ) == ContentKind::Html
                                {
                                    let html = String::from_utf8_lossy(&bytes).into_owned();
                                    let src = target.abs_path.display().to_string();
                                    let name = target.display_name.clone();
                                    ui.open_overlay(Overlay::HtmlPreview(
                                        HtmlPreviewOverlay::new(name.clone(), src, html)
                                            .with_source(HtmlPreviewSource::File(
                                                target.abs_path.clone(),
                                            )),
                                    ));
                                    ui.toasts.info(format!(
                                        "Detected HTML in '{name}' — routed to interactive HTML preview"
                                    ));
                                    return;
                                }
                            }
                        }
                        ui.open_overlay(Overlay::ImagePreview(ov));
                    } else {
                        ui.toasts.info("No embedded images in this note");
                    }
                }
            }
            Action::OpenHtmlPreview => {
                self.open_html_preview(ui, None);
            }
            Action::OpenHtmlBlock(block_idx) => {
                self.open_html_preview(ui, Some(block_idx));
            }
            Action::OpenHtmlFile(rel_path) => {
                self.open_html_file(ui, &rel_path);
            }
            Action::OpenHtmlContent {
                title,
                source_label,
                html,
            } => {
                ui.open_overlay(Overlay::HtmlPreview(HtmlPreviewOverlay::new(
                    title,
                    source_label,
                    html,
                )));
            }
            Action::ServeHtmlInBrowser { title, html } => {
                self.pending_html_serve = Some((title, html));
            }
            Action::ShowInfoToast(msg) => {
                ui.toasts.info(msg);
            }
            Action::OpenUrl(url) => {
                if let Err(e) = open::that(&url) {
                    ui.toasts.error(format!("Failed to open URL: {e}"));
                } else {
                    ui.toasts.info("Opened link in browser");
                }
            }
            Action::CopyUrlToClipboard(text) => {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    if cb.set_text(text).is_ok() {
                        ui.toasts.success("Copied to clipboard");
                    }
                }
            }
            Action::CopySelectionOrMarkdown => {
                if let Some(d) = &self.active_note {
                    let payload = match d.parsed.blocks.get(ui.canvas_selected_block) {
                        Some(NoteBlock::CodeBlock { code, .. }) => code.clone(),
                        _ => d.raw_markdown.clone(),
                    };
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        if cb.set_text(payload).is_ok() {
                            ui.toasts.success("Copied to clipboard");
                        }
                    }
                }
            }

            Action::OpenOverlay(kind) => match kind {
                OverlayKind::QuickSwitcher => {
                    ui.open_overlay(Overlay::QuickSwitcher(QuickSwitcherOverlay::new()));
                }
                OverlayKind::VaultSwitcher => {
                    let mut vaults = merged_vaults(&self.cfg);
                    if !vaults.iter().any(|v| v.id == self.active_vault.id) {
                        vaults.insert(0, self.active_vault.clone());
                    }
                    self.vaults = vaults;
                    ui.open_overlay(Overlay::VaultSwitcher(VaultSwitcherOverlay::new(self)));
                }
                OverlayKind::Settings => {
                    ui.open_overlay(Overlay::Settings(SettingsOverlay::from_state(self, ui)));
                }
                OverlayKind::Help => {
                    ui.open_overlay(Overlay::Help { scroll: 0 });
                }
            },
            Action::OpenContextMenu { col, row } => {
                // Target the actual item under the right-click cursor so actions like
                // "Move Note to .trash/" or "Edit" operate on the right-clicked note/block.
                match ui.last_hitmap.hit(row, col) {
                    Some(HitTarget::SidebarRow(idx)) | Some(HitTarget::SidebarTaskCheckbox(idx)) => {
                        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                            return;
                        }
                        let entries = build_sidebar_entries(self, ui);
                        if let Some(entry) = entries.get(idx).cloned() {
                            ui.sidebar_selected = idx;
                            match entry {
                                SidebarEntry::Note { rel_path, .. } => {
                                    self.open_note(ui, &rel_path, true);
                                    if ui.inline_editor.is_some() {
                                        return;
                                    }
                                }
                                SidebarEntry::Task { note_path, .. } => {
                                    self.open_note(ui, &note_path, true);
                                    if ui.inline_editor.is_some() {
                                        return;
                                    }
                                }
                                _ => {}
                            }
                            ui.focus = Focus::Sidebar;
                        }
                    }
                    Some(HitTarget::CanvasBlock(idx)) | Some(HitTarget::WikilinkBlock(idx)) => {
                        ui.canvas_selected_block = idx;
                        ui.focus = Focus::Canvas;
                    }
                    Some(HitTarget::GraphNode(idx)) => {
                        ui.graph_selected = idx;
                        ui.focus = Focus::Canvas;
                    }
                    Some(HitTarget::InspectorBacklink(idx)) => {
                        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                            return;
                        }
                        ui.inspector_selected = idx;
                        ui.focus = Focus::Inspector;
                    }
                    Some(HitTarget::InspectorOutgoing(idx)) => {
                        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                            return;
                        }
                        let bl_len = self
                            .active_note
                            .as_ref()
                            .map(|d| d.backlinks.len())
                            .unwrap_or(0);
                        ui.inspector_selected = bl_len + idx;
                        ui.focus = Focus::Inspector;
                    }
                    Some(HitTarget::InspectorOutline(idx)) => {
                        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                            return;
                        }
                        let base = self
                            .active_note
                            .as_ref()
                            .map(|d| d.backlinks.len() + d.outgoing_links.len())
                            .unwrap_or(0);
                        ui.inspector_selected = base + idx;
                        ui.focus = Focus::Inspector;
                    }
                    _ => {}
                }
                ui.open_overlay(Overlay::ContextMenu(ContextMenuOverlay::default_menu(
                    col, row,
                )));
            }
            Action::OpenContextMenuKeyboard => {
                ui.open_overlay(Overlay::ContextMenu(ContextMenuOverlay::default_menu(14, 4)));
            }
            Action::CloseOverlay => ui.close_overlay(),

            _ => {}
        }
    }
}
