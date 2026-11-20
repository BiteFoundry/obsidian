//! Modal overlays for the Obsidian Terminal UI.

pub mod confirm;
pub mod context_menu;
pub mod file_picker;
pub mod html_preview;
pub mod image_preview;
pub mod links;
pub mod note_editor;
pub mod note_form;
pub mod quick_switcher;
pub mod settings;
pub mod vault_switcher;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{ButtonId, HitMap, HitTarget},
    keymap::HELP_SECTIONS,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    QuickSwitcher,
    VaultSwitcher,
    Settings,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    Handled,
    NotHandled,
    Close,
    Dispatch(Action),
    CloseAndDispatch(Action),
}

#[derive(Debug)]
pub enum Overlay {
    QuickSwitcher(quick_switcher::QuickSwitcherOverlay),
    VaultSwitcher(vault_switcher::VaultSwitcherOverlay),
    Settings(settings::SettingsOverlay),
    LinkExtractor(links::LinkExtractorOverlay),
    FilePicker(file_picker::FilePickerOverlay),
    ImagePreview(image_preview::ImagePreviewOverlay),
    HtmlPreview(html_preview::HtmlPreviewOverlay),
    NoteEditor(note_editor::NoteEditorOverlay),
    NoteForm(note_form::NoteFormOverlay),
    Confirm(confirm::ConfirmOverlay),
    ContextMenu(context_menu::ContextMenuOverlay),
    Help { scroll: u16 },
}

impl Overlay {
    pub fn is_text_input(&self) -> bool {
        match self {
            Self::QuickSwitcher(_) | Self::NoteForm(_) | Self::FilePicker(_) => true,
            Self::NoteEditor(ov) => ov.accepts_text(),
            Self::HtmlPreview(ov) => ov.is_editing_input(),
            _ => false,
        }
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        match self {
            Self::QuickSwitcher(ov) => ov.handle(action, app),
            Self::VaultSwitcher(ov) => ov.handle(action, app),
            Self::Settings(ov) => ov.handle(action),
            Self::LinkExtractor(ov) => ov.handle(action),
            Self::FilePicker(ov) => ov.handle(action),
            Self::ImagePreview(ov) => ov.handle(action),
            Self::HtmlPreview(ov) => ov.handle(action),
            Self::NoteEditor(ov) => ov.handle(action),
            Self::NoteForm(ov) => ov.handle(action),
            Self::Confirm(ov) => ov.handle(action),
            Self::ContextMenu(ov) => ov.handle(action),
            Self::Help { scroll } => match action {
                Action::CloseOverlay | Action::Input('q') | Action::Input('?') => {
                    OverlayOutcome::Close
                }
                Action::MoveDown | Action::Input('j') | Action::MouseScrollDown { .. } => {
                    *scroll = scroll.saturating_add(1);
                    OverlayOutcome::Handled
                }
                Action::MoveUp | Action::Input('k') | Action::MouseScrollUp { .. } => {
                    *scroll = scroll.saturating_sub(1);
                    OverlayOutcome::Handled
                }
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        match self {
            Self::QuickSwitcher(ov) => ov.draw(frame, area, app, ui, hitmap),
            Self::VaultSwitcher(ov) => ov.draw(frame, area, app, ui, hitmap),
            Self::Settings(ov) => ov.draw(frame, area, app, ui, hitmap),
            Self::LinkExtractor(ov) => ov.draw(frame, area, ui, hitmap),
            Self::FilePicker(ov) => ov.draw(frame, area, ui, hitmap),
            Self::ImagePreview(ov) => ov.draw(frame, area, app, ui, hitmap),
            Self::HtmlPreview(ov) => ov.draw(frame, area, ui, hitmap),
            Self::NoteEditor(ov) => {
                ui.cursor_pos = ov.draw(frame, area, ui, hitmap);
            }
            Self::NoteForm(ov) => ov.draw(frame, area, ui, hitmap),
            Self::Confirm(ov) => ov.draw(frame, area, ui, hitmap),
            Self::ContextMenu(ov) => ov.draw(frame, area, ui, hitmap),
            Self::Help { scroll } => draw_help_overlay(frame, area, ui, hitmap, *scroll),
        }
    }
}

pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

fn draw_help_overlay(
    frame: &mut Frame,
    area: Rect,
    ui: &UiState,
    hitmap: &mut HitMap,
    scroll: u16,
) {
    let theme = &ui.theme;
    let rect = centered_rect(
        area,
        area.width.saturating_sub(6).min(86).max(40),
        area.height.saturating_sub(4).min(28).max(12),
    );
    frame.render_widget(Clear, rect);
    hitmap.add(rect, HitTarget::OverlayBody);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
        .border_style(theme.border_style(true))
        .title(Line::from(Span::styled(
            " Obsidian Keybindings · j/k Scroll · Esc Close ",
            theme.title(true),
        )));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let close_label = "[✕ Esc]";
    let close_w = unicode_width::UnicodeWidthStr::width(close_label) as u16;
    if rect.width > close_w + 4 {
        let close_x = rect.right().saturating_sub(close_w + 1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                close_label,
                Style::default().fg(theme.fg).bg(theme.bg_elevated),
            )),
            Rect::new(close_x, rect.y, close_w, 1),
        );
        hitmap.add_line(
            close_x,
            rect.y,
            close_w,
            HitTarget::Button(ButtonId::ConfirmCancel),
        );
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (section, items) in HELP_SECTIONS {
        lines.push(Line::from(Span::styled(
            format!("  {section}"),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        for &(keys, desc) in *items {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {keys:<26}"),
                    Style::default()
                        .fg(theme.accent_soft)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(desc.to_string(), Style::default().fg(theme.fg_primary)),
            ]));
        }
        lines.push(Line::from(""));
    }

    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
}
