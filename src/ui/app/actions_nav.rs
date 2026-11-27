//! Navigation, focus, sidebar/inspector, and graph view action handlers.

use crate::{
    models::{NoteBlock, SidebarTab},
    ui::{action::Action, sidebar::build_sidebar_entries},
};

use super::{AppState, CanvasMode, Focus, UiState};

impl AppState {
    pub(super) fn dispatch_nav_action(&mut self, action: Action, ui: &mut UiState) {
        match action {
            Action::NextFocus => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                if ui.canvas_mode == CanvasMode::Graph && ui.focus == Focus::Canvas {
                    let len = self.graph.nodes.len();
                    if len > 0 {
                        ui.graph_selected = (ui.graph_selected + 1) % len;
                    }
                } else {
                    ui.focus = match ui.focus {
                        Focus::Sidebar => Focus::Canvas,
                        Focus::Canvas => {
                            if ui.show_inspector {
                                Focus::Inspector
                            } else if ui.show_sidebar {
                                Focus::Sidebar
                            } else {
                                Focus::Canvas
                            }
                        }
                        Focus::Inspector => {
                            if ui.show_sidebar {
                                Focus::Sidebar
                            } else {
                                Focus::Canvas
                            }
                        }
                        Focus::Overlay => Focus::Canvas,
                    };
                }
            }
            Action::PrevFocus => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                ui.focus = match ui.focus {
                    Focus::Sidebar => {
                        if ui.show_inspector {
                            Focus::Inspector
                        } else {
                            Focus::Canvas
                        }
                    }
                    Focus::Canvas => {
                        if ui.show_sidebar {
                            Focus::Sidebar
                        } else if ui.show_inspector {
                            Focus::Inspector
                        } else {
                            Focus::Canvas
                        }
                    }
                    Focus::Inspector => Focus::Canvas,
                    Focus::Overlay => Focus::Canvas,
                };
            }

            Action::MoveUp => match ui.focus {
                Focus::Sidebar => {
                    ui.sidebar_selected = ui.sidebar_selected.saturating_sub(1);
                }
                Focus::Canvas => {
                    if ui.canvas_mode == CanvasMode::Graph {
                        let len = self.graph.nodes.len();
                        if len > 0 {
                            ui.graph_selected = (ui.graph_selected + len - 1) % len;
                        }
                    } else {
                        ui.canvas_selected_block = ui.canvas_selected_block.saturating_sub(1);
                        ui.canvas_follow_selected = true;
                    }
                }
                Focus::Inspector => {
                    ui.inspector_selected = ui.inspector_selected.saturating_sub(1);
                }
                Focus::Overlay => {}
            },

            Action::MoveDown => match ui.focus {
                Focus::Sidebar => {
                    let len = build_sidebar_entries(self, ui).len();
                    if len > 0 {
                        ui.sidebar_selected = (ui.sidebar_selected + 1).min(len - 1);
                    }
                }
                Focus::Canvas => {
                    if ui.canvas_mode == CanvasMode::Graph {
                        let len = self.graph.nodes.len();
                        if len > 0 {
                            ui.graph_selected = (ui.graph_selected + 1) % len;
                        }
                    } else if let Some(d) = &self.active_note {
                        let max_b = d.parsed.blocks.len().saturating_sub(1);
                        ui.canvas_selected_block = (ui.canvas_selected_block + 1).min(max_b);
                        ui.canvas_follow_selected = true;
                    }
                }
                Focus::Inspector => {
                    if let Some(d) = &self.active_note {
                        let total = Self::inspector_item_count(d, ui);
                        if total > 0 {
                            ui.inspector_selected = (ui.inspector_selected + 1).min(total - 1);
                        }
                    }
                }
                Focus::Overlay => {}
            },

            Action::PageUp => match ui.focus {
                Focus::Sidebar => {
                    ui.sidebar_selected = ui.sidebar_selected.saturating_sub(10);
                }
                Focus::Inspector => {
                    ui.inspector_selected = ui.inspector_selected.saturating_sub(10);
                }
                Focus::Canvas | Focus::Overlay => {
                    ui.canvas_follow_selected = false;
                    ui.canvas_scroll = ui.canvas_scroll.saturating_sub(10);
                    ui.canvas_selected_block = ui.canvas_selected_block.saturating_sub(5);
                }
            },
            Action::PageDown => match ui.focus {
                Focus::Sidebar => {
                    let len = build_sidebar_entries(self, ui).len();
                    if len > 0 {
                        ui.sidebar_selected = (ui.sidebar_selected + 10).min(len - 1);
                    }
                }
                Focus::Inspector => {
                    if let Some(d) = &self.active_note {
                        let total = Self::inspector_item_count(d, ui);
                        if total > 0 {
                            ui.inspector_selected = (ui.inspector_selected + 10).min(total - 1);
                        }
                    }
                }
                Focus::Canvas | Focus::Overlay => {
                    ui.canvas_follow_selected = false;
                    ui.canvas_scroll = ui.canvas_scroll.saturating_add(10);
                    if let Some(d) = &self.active_note {
                        let max_b = d.parsed.blocks.len().saturating_sub(1);
                        ui.canvas_selected_block = (ui.canvas_selected_block + 5).min(max_b);
                    }
                }
            },
            Action::JumpTop => match ui.focus {
                Focus::Sidebar => {
                    ui.sidebar_selected = 0;
                }
                Focus::Inspector => {
                    ui.inspector_selected = 0;
                }
                Focus::Canvas | Focus::Overlay => {
                    ui.canvas_selected_block = 0;
                    ui.canvas_scroll = 0;
                    ui.canvas_follow_selected = true;
                }
            },
            Action::JumpBottom => match ui.focus {
                Focus::Sidebar => {
                    let len = build_sidebar_entries(self, ui).len();
                    ui.sidebar_selected = len.saturating_sub(1);
                }
                Focus::Inspector => {
                    if let Some(d) = &self.active_note {
                        ui.inspector_selected = Self::inspector_item_count(d, ui).saturating_sub(1);
                    }
                }
                Focus::Canvas | Focus::Overlay => {
                    if let Some(d) = &self.active_note {
                        ui.canvas_selected_block = d.parsed.blocks.len().saturating_sub(1);
                        ui.canvas_follow_selected = true;
                    }
                }
            },

            Action::Activate => match ui.focus {
                Focus::Sidebar => self.activate_sidebar_selected(ui),
                Focus::Canvas => {
                    if ui.canvas_mode == CanvasMode::Graph {
                        if let Some(node) = self.graph.nodes.get(ui.graph_selected).cloned() {
                            self.open_note(ui, &node.id, true);
                        }
                    } else if let Some(d) = &self.active_note {
                        match d.parsed.blocks.get(ui.canvas_selected_block) {
                            Some(block) if block.first_wikilink().is_some() => {
                                self.dispatch_core(Action::FollowSelectedWikilink, ui);
                            }
                            Some(NoteBlock::Task { .. }) => {
                                self.dispatch_core(Action::FollowSelectedWikilink, ui);
                            }
                            Some(NoteBlock::ImageEmbed { .. } | NoteBlock::HtmlEmbed { .. }) => {}
                            Some(block) => {
                                let target_line = block.line_no().saturating_sub(1);
                                self.open_inline_edit_at(ui, Some(target_line), None);
                            }
                            None => {
                                self.open_inline_edit_at(ui, Some(0), None);
                            }
                        }
                    }
                }
                Focus::Inspector => self.activate_inspector_selected(ui),
                Focus::Overlay => {}
            },

            Action::MoveRight
                if ui.focus == Focus::Sidebar && ui.sidebar_tab == SidebarTab::Files =>
            {
                self.files_tree_right(ui);
            }
            Action::MoveLeft
                if ui.focus == Focus::Sidebar && ui.sidebar_tab == SidebarTab::Files =>
            {
                self.files_tree_left(ui);
            }

            Action::NavigateBack => {
                if let Some(prev) = self.history.pop() {
                    self.open_note(ui, &prev, false);
                } else {
                    ui.toasts.info("History stack is empty");
                }
            }

            Action::ToggleSidebar => {
                ui.show_sidebar = !ui.show_sidebar;
                if !ui.show_sidebar && ui.focus == Focus::Sidebar {
                    ui.focus = Focus::Canvas;
                }
            }
            Action::ToggleInspector => {
                ui.show_inspector = !ui.show_inspector;
                if !ui.show_inspector && ui.focus == Focus::Inspector {
                    ui.focus = Focus::Canvas;
                }
            }
            Action::TogglePageWidth => {
                ui.page_width = ui.page_width.toggle();
                self.cfg.page_width = ui.page_width;
                let _ = self.cfg.save();
            }
            Action::SetSidebarTab(tab) => {
                if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                    return;
                }
                ui.sidebar_tab = tab;
                ui.sidebar_selected = 0;
                ui.show_sidebar = true;
                ui.focus = Focus::Sidebar;
                self.clamp_selections(ui);
            }
            Action::ClearTagFilter => {
                if ui.active_tag_filter.is_some() {
                    ui.active_tag_filter = None;
                    ui.sidebar_selected = 0;
                    self.clamp_selections(ui);
                }
            }

            Action::OpenNote(path) => self.open_note(ui, &path, true),

            Action::ToggleGraphView => {
                ui.canvas_mode = match ui.canvas_mode {
                    CanvasMode::Note => {
                        if ui.inline_editor.is_some() && !self.finish_inline_edit(ui) {
                            return;
                        }
                        self.refresh_graph(ui);
                        ui.graph_selected = 0;
                        ui.focus = Focus::Canvas;
                        CanvasMode::Graph
                    }
                    CanvasMode::Graph => CanvasMode::Note,
                };
            }
            Action::ToggleGraphScope => {
                ui.graph_global = !ui.graph_global;
                ui.graph_selected = 0;
                self.refresh_graph(ui);
            }
            Action::GraphDepthInc => {
                ui.graph_depth = (ui.graph_depth + 1).min(5);
                self.refresh_graph(ui);
            }
            Action::GraphDepthDec => {
                ui.graph_depth = ui.graph_depth.saturating_sub(1).max(1);
                self.refresh_graph(ui);
            }
            Action::GraphZoomIn => {
                ui.graph_sim
                    .zoom_by(crate::ui::graph_view::GRAPH_WHEEL_FACTOR);
            }
            Action::GraphZoomOut => {
                ui.graph_sim
                    .zoom_by(1.0 / crate::ui::graph_view::GRAPH_WHEEL_FACTOR);
            }
            Action::GraphResetView => {
                ui.graph_sim.reset_view(&self.graph);
                ui.toasts.info("Reset graph view");
            }
            Action::GraphTick if ui.canvas_mode == CanvasMode::Graph => {
                ui.graph_sim.step_physics(&self.graph);
            }

            _ => {}
        }
    }
}
