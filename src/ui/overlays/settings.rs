//! Settings Overlay (`Ctrl+,`): 5 curated Obsidian palettes, layout width, density, inspector, inline images.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    config::{DensityMode, ImageProtocolPreference, PageWidthMode, ThemePreset},
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

const PALETTES: &[ThemePreset] = &[
    ThemePreset::ObsidianDark,
    ThemePreset::Catppuccin,
    ThemePreset::TokyoNight,
    ThemePreset::Nord,
    ThemePreset::WarmPaper,
    ThemePreset::Auto,
    ThemePreset::Ansi256,
    ThemePreset::NoColor,
];

#[derive(Debug, Clone)]
pub struct SettingsOverlay {
    pub selected: usize,
    pub theme: ThemePreset,
    pub width: PageWidthMode,
    pub density: DensityMode,
    pub show_inspector: bool,
    pub inline_images: bool,
    pub image_protocol: ImageProtocolPreference,
    pub notify: bool,
}

impl SettingsOverlay {
    pub fn from_state(app: &AppState, ui: &UiState) -> Self {
        Self {
            selected: 0,
            theme: app.cfg.theme,
            width: ui.page_width,
            density: ui.density,
            show_inspector: ui.show_inspector,
            inline_images: app.cfg.inline_images,
            image_protocol: app.cfg.image_protocol,
            notify: app.cfg.notify,
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        const ITEMS: usize = 7;
        match action {
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::Input('k') => {
                self.selected = (self.selected + ITEMS - 1) % ITEMS;
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                self.selected = (self.selected + 1) % ITEMS;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::CursorRight | Action::Input(' ') => {
                self.cycle_selected(true)
            }
            Action::CursorLeft => self.cycle_selected(false),
            _ => OverlayOutcome::Handled,
        }
    }

    fn cycle_selected(&mut self, forward: bool) -> OverlayOutcome {
        match self.selected {
            0 => {
                let idx = PALETTES.iter().position(|&p| p == self.theme).unwrap_or(0);
                let next_idx = if forward {
                    (idx + 1) % PALETTES.len()
                } else {
                    (idx + PALETTES.len() - 1) % PALETTES.len()
                };
                self.theme = PALETTES[next_idx];
                OverlayOutcome::Dispatch(Action::SetTheme(self.theme))
            }
            1 => {
                self.width = self.width.toggle();
                OverlayOutcome::Dispatch(Action::SetPageWidth(self.width))
            }
            2 => {
                self.density = self.density.toggle();
                OverlayOutcome::Dispatch(Action::SetDensity(self.density))
            }
            3 => {
                self.show_inspector = !self.show_inspector;
                OverlayOutcome::Dispatch(Action::ToggleInspector)
            }
            4 => {
                self.inline_images = !self.inline_images;
                OverlayOutcome::Dispatch(Action::ToggleInlineImages)
            }
            5 => {
                self.image_protocol = self.image_protocol.next();
                OverlayOutcome::Dispatch(Action::SetImageProtocol(self.image_protocol))
            }
            6 => {
                self.notify = !self.notify;
                OverlayOutcome::Dispatch(Action::ToggleNotify)
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        _app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(area, area.width.saturating_sub(6).clamp(44, 78), 17);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .border_style(theme.border_style(true))
            .title(Line::from(Span::styled(
                " Settings · Enter/←/→ Change · Esc Close ",
                theme.title(true),
            )));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let rows = [
            ("Color Palette", self.theme.label().to_string()),
            ("Canvas Width", self.width.label().to_string()),
            ("List Density", self.density.label().to_string()),
            (
                "Right Inspector (Backlinks)",
                if self.show_inspector {
                    "Visible"
                } else {
                    "Hidden"
                }
                .to_string(),
            ),
            (
                "Inline Image Previews",
                if self.inline_images {
                    "Enabled"
                } else {
                    "Disabled"
                }
                .to_string(),
            ),
            ("Image Protocol", self.image_protocol.label().to_string()),
            (
                "Desktop Notifications",
                if self.notify { "Enabled" } else { "Muted" }.to_string(),
            ),
        ];

        for (idx, (label, value)) in rows.iter().enumerate() {
            let y = inner.y + 1 + idx as u16;
            if y >= inner.y + inner.height.saturating_sub(4) {
                break;
            }
            let row_rect = Rect::new(inner.x, y, inner.width, 1);
            let is_sel = idx == self.selected;
            let line = Line::from(vec![
                Span::styled(
                    if is_sel { " ▸ " } else { "   " },
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{label:<30}"),
                    Style::default()
                        .fg(theme.fg_primary)
                        .add_modifier(if is_sel {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(
                    format!("‹ {value} ›"),
                    Style::default()
                        .fg(theme.accent_soft)
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(if is_sel {
                    theme.selection()
                } else {
                    Style::default().bg(theme.bg_elevated)
                }),
                row_rect,
            );
            hitmap.add(row_rect, HitTarget::OverlayItem { index: idx });
        }

        // Palette swatch strip at bottom
        let swatch_y = inner.y + inner.height.saturating_sub(3);
        if swatch_y > inner.y + 7 {
            let swatches = Line::from(vec![
                Span::styled("  Palettes: ", theme.muted_style()),
                Span::styled(
                    " Obsidian Dark ",
                    swatch_pill(self.theme == ThemePreset::ObsidianDark, theme),
                ),
                Span::raw(" "),
                Span::styled(
                    " Catppuccin ",
                    swatch_pill(self.theme == ThemePreset::Catppuccin, theme),
                ),
                Span::raw(" "),
                Span::styled(
                    " Tokyo Night ",
                    swatch_pill(self.theme == ThemePreset::TokyoNight, theme),
                ),
                Span::raw(" "),
                Span::styled(
                    " Nord ",
                    swatch_pill(self.theme == ThemePreset::Nord, theme),
                ),
                Span::raw(" "),
                Span::styled(
                    " Warm Paper ",
                    swatch_pill(self.theme == ThemePreset::WarmPaper, theme),
                ),
            ]);
            frame.render_widget(
                Paragraph::new(swatches),
                Rect::new(inner.x, swatch_y, inner.width, 1),
            );
        }
    }
}

fn swatch_pill(active: bool, theme: &crate::ui::theme::Theme) -> Style {
    if active {
        Style::default()
            .fg(theme.bg_canvas)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        theme.prop_pill_style()
    }
}
