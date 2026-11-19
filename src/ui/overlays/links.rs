//! Link Extractor Overlay (`u`): extracts all [[wikilinks]] and external URLs from the active note.

use std::collections::HashSet;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::NoteDetail,
    ui::{
        action::Action,
        app::UiState,
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
        sidebar::selection_offset,
        truncate_width,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractedLinkKind {
    Wikilink { resolved_path: Option<String> },
    ExternalUrl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLink {
    pub label: String,
    pub target: String,
    pub kind: ExtractedLinkKind,
}

#[derive(Debug, Clone)]
pub struct LinkExtractorOverlay {
    pub context_title: String,
    pub links: Vec<ExtractedLink>,
    pub selected: usize,
}

impl LinkExtractorOverlay {
    pub fn from_note(detail: &NoteDetail) -> Self {
        let mut links = Vec::new();
        let mut seen = HashSet::new();

        for ol in &detail.outgoing_links {
            let key = format!("wiki:{}", ol.target_raw.to_ascii_lowercase());
            if seen.insert(key) {
                let label = ol
                    .alias
                    .clone()
                    .unwrap_or_else(|| ol.target_title.clone());
                links.push(ExtractedLink {
                    label,
                    target: ol
                        .target_resolved_path
                        .clone()
                        .unwrap_or_else(|| ol.target_raw.clone()),
                    kind: ExtractedLinkKind::Wikilink {
                        resolved_path: ol.target_resolved_path.clone(),
                    },
                });
            }
        }

        for ml in &detail.parsed.md_links {
            if ml.url.starts_with("http://") || ml.url.starts_with("https://") {
                if seen.insert(ml.url.clone()) {
                    links.push(ExtractedLink {
                        label: if ml.label.trim().is_empty() {
                            ml.url.clone()
                        } else {
                            ml.label.clone()
                        },
                        target: ml.url.clone(),
                        kind: ExtractedLinkKind::ExternalUrl,
                    });
                }
            }
        }

        Self {
            context_title: detail.summary.title.clone(),
            links,
            selected: 0,
        }
    }

    pub fn current(&self) -> Option<&ExtractedLink> {
        let idx = self.selected.min(self.links.len().saturating_sub(1));
        self.links.get(idx)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay
            | Action::OpenLinkExtractor
            | Action::Input('q')
            | Action::Input('u') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::CursorUp | Action::Input('k') => {
                if !self.links.is_empty() {
                    let cur = self.selected.min(self.links.len() - 1);
                    self.selected = (cur + self.links.len() - 1) % self.links.len();
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown | Action::Input('j') => {
                if !self.links.is_empty() {
                    let cur = self.selected.min(self.links.len() - 1);
                    self.selected = (cur + 1) % self.links.len();
                }
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                let Some(link) = self.current() else {
                    return OverlayOutcome::Close;
                };
                match &link.kind {
                    ExtractedLinkKind::Wikilink {
                        resolved_path: Some(rp),
                    } => OverlayOutcome::CloseAndDispatch(Action::OpenNote(rp.clone())),
                    ExtractedLinkKind::Wikilink { resolved_path: None } => {
                        OverlayOutcome::CloseAndDispatch(Action::OpenNote(link.target.clone()))
                    }
                    ExtractedLinkKind::ExternalUrl => {
                        OverlayOutcome::CloseAndDispatch(Action::OpenUrl(link.target.clone()))
                    }
                }
            }
            Action::Input('y') => {
                if let Some(link) = self.current() {
                    OverlayOutcome::Dispatch(Action::CopyUrlToClipboard(link.target.clone()))
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::Input(ch) if ch.is_ascii_digit() && *ch != '0' => {
                let idx = (*ch as usize) - ('1' as usize);
                if idx < self.links.len() {
                    self.selected = idx;
                }
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let max_w = area.width.saturating_sub(6).min(92).max(40);
        let max_h = area.height.saturating_sub(4).min(20).max(9);
        let rect = centered_rect(area, max_w, max_h);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let title = format!(
            " Link Extractor ({}) · {} · Enter Open · y Copy · Esc Close ",
            self.links.len(),
            truncate_width(&self.context_title, 26)
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                truncate_width(&title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if self.links.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "  No wikilinks or URLs found in this note.",
                    theme.muted_style(),
                )),
                inner,
            );
            return;
        }

        let visible_rows = inner.height as usize;
        let selected = self.selected.min(self.links.len() - 1);
        let offset = selection_offset(selected, visible_rows, self.links.len());

        for (i, link) in self.links.iter().skip(offset).take(visible_rows).enumerate() {
            let idx = offset + i;
            let row_y = inner.y + i as u16;
            let row_rect = Rect::new(inner.x, row_y, inner.width, 1);
            let is_sel = idx == selected;

            let (badge, badge_style) = match &link.kind {
                ExtractedLinkKind::Wikilink {
                    resolved_path: Some(_),
                } => ("WIKI", theme.tag_pill_style()),
                ExtractedLinkKind::Wikilink {
                    resolved_path: None,
                } => ("NEW ", Style::default().fg(theme.warning).bg(theme.bg_card)),
                ExtractedLinkKind::ExternalUrl => {
                    ("URL ", Style::default().fg(theme.info).bg(theme.bg_card))
                }
            };

            let line = Line::from(vec![
                Span::styled(
                    if is_sel { " ▸ " } else { "   " },
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {badge} "), badge_style),
                Span::raw(" "),
                Span::styled(
                    truncate_width(&link.label, 28),
                    Style::default()
                        .fg(theme.fg_primary)
                        .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() }),
                ),
                Span::styled("  →  ", theme.muted_style()),
                Span::styled(
                    truncate_width(
                        &link.target,
                        (inner.width as usize).saturating_sub(44),
                    ),
                    theme.muted_style(),
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
    }
}
