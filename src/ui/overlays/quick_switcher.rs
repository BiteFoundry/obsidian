//! Obsidian Quick Switcher & Command Palette (`Ctrl+O` / `Ctrl+K` / `Ctrl+P`).
//!
//! Fuzzy searches notes (by title, path, tags, aliases) plus workspace commands (`>` prefix or inline).

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
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayKind, OverlayOutcome},
    sidebar::selection_offset,
    truncate_width,
};

#[derive(Debug, Clone)]
pub enum SwitcherItem {
    Note {
        rel_path: String,
        title: String,
        folder: String,
        tags: Vec<String>,
        backlinks: usize,
    },
    HtmlFile {
        rel_path: String,
        title: String,
        folder: String,
    },
    Command {
        label: String,
        shortcut: String,
        action: Action,
    },
}

#[derive(Debug, Clone, Default)]
pub struct QuickSwitcherOverlay {
    pub query: String,
    pub selected: usize,
}

impl QuickSwitcherOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self, app: &AppState) -> Vec<SwitcherItem> {
        let raw = self.query.trim();
        let command_only = raw.starts_with('>');
        let needle = raw.trim_start_matches('>').trim().to_ascii_lowercase();

        let mut out = Vec::new();
        if !command_only {
            let mut scored: Vec<(i32, SwitcherItem)> = app
                .notes
                .iter()
                .filter_map(|n| {
                    let score = fuzzy_score(
                        &needle,
                        &n.title,
                        &n.rel_path,
                        &n.tags,
                        &n.aliases,
                    )?;
                    Some((
                        score,
                        SwitcherItem::Note {
                            rel_path: n.rel_path.clone(),
                            title: n.title.clone(),
                            folder: n.folder.clone(),
                            tags: n.tags.clone(),
                            backlinks: n.backlink_count,
                        },
                    ))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            out.extend(scored.into_iter().take(40).map(|(_, item)| item));

            for h in &app.html_files {
                if needle.is_empty()
                    || h.title.to_ascii_lowercase().contains(&needle)
                    || h.rel_path.to_ascii_lowercase().contains(&needle)
                {
                    out.push(SwitcherItem::HtmlFile {
                        rel_path: h.rel_path.clone(),
                        title: h.title.clone(),
                        folder: h.folder.clone(),
                    });
                }
            }
        }

        let commands = vec![
            ("Open Knowledge Graph View", "G", Action::ToggleGraphView),
            ("Open Today's Daily Note", "D", Action::OpenDailyNote),
            ("Interactive HTML Preview", "H", Action::OpenHtmlPreview),
            ("Create New Note", "n", Action::OpenNewNoteModal),
            ("Edit Active Note in Terminal", "e", Action::OpenNoteEditor),
            ("Edit Active Note in External $EDITOR", "Ctrl+E", Action::RequestExternalEditor),
            ("Quick Append to Active Note", "a", Action::OpenQuickAppend),
            ("Toggle Right Inspector (Backlinks)", "B", Action::ToggleInspector),
            ("Toggle Left Sidebar", "[", Action::ToggleSidebar),
            ("Switch Vault", "Ctrl+S", Action::OpenOverlay(OverlayKind::VaultSwitcher)),
            ("Settings & Themes (5 Palettes)", "Ctrl+,", Action::OpenOverlay(OverlayKind::Settings)),
            ("Extract Links from Active Note", "u", Action::OpenLinkExtractor),
            ("Attach Local File (zoxide)", "Ctrl+F", Action::OpenFilePicker),
            ("Reindex Vault Now", "r", Action::RefreshAll),
            ("Keyboard Shortcuts Help", "?", Action::OpenOverlay(OverlayKind::Help)),
        ];

        for (label, shortcut, action) in commands {
            if command_only
                || needle.is_empty()
                || label.to_ascii_lowercase().contains(&needle)
            {
                out.push(SwitcherItem::Command {
                    label: label.to_string(),
                    shortcut: shortcut.to_string(),
                    action,
                });
            }
        }
        out
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        let list = self.items(app);
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp => {
                if !list.is_empty() {
                    let cur = self.selected.min(list.len() - 1);
                    self.selected = (cur + list.len() - 1) % list.len();
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Tab => {
                if !list.is_empty() {
                    let cur = self.selected.min(list.len() - 1);
                    self.selected = (cur + 1) % list.len();
                }
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.query.pop();
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                for ch in text.chars() {
                    if ch != '\r' && ch != '\n' {
                        self.query.push(ch);
                    }
                }
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                if list.is_empty() {
                    return OverlayOutcome::Close;
                }
                let idx = self.selected.min(list.len() - 1);
                match &list[idx] {
                    SwitcherItem::Note { rel_path, .. } => {
                        OverlayOutcome::CloseAndDispatch(Action::OpenNote(rel_path.clone()))
                    }
                    SwitcherItem::HtmlFile { rel_path, .. } => {
                        OverlayOutcome::CloseAndDispatch(Action::OpenHtmlFile(rel_path.clone()))
                    }
                    SwitcherItem::Command { action, .. } => {
                        OverlayOutcome::CloseAndDispatch(action.clone())
                    }
                }
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let max_w = area.width.saturating_sub(8).min(82).max(40);
        let max_h = area.height.saturating_sub(6).min(20).max(10);
        let rect = centered_rect(area, max_w, max_h);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .border_style(theme.border_style(true))
            .title(Line::from(Span::styled(
                " Quick Switcher · Type note name or >command · Enter Open ",
                theme.title(true),
            )));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width < 4 || inner.height < 3 {
            return;
        }

        // Input bar
        let input_rect = Rect::new(inner.x, inner.y, inner.width, 1);
        let prompt_line = Line::from(vec![
            Span::styled(
                " ⌕ ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            if self.query.is_empty() {
                Span::styled("Find a note or run >command...", theme.muted_style())
            } else {
                Span::styled(
                    format!("{}▏", self.query),
                    Style::default().fg(theme.fg_primary),
                )
            },
        ]);
        frame.render_widget(
            Paragraph::new(prompt_line).style(Style::default().bg(theme.bg_composer)),
            input_rect,
        );

        // Divider
        let div_rect = Rect::new(inner.x, inner.y + 1, inner.width, 1);
        frame.render_widget(
            Paragraph::new("─".repeat(inner.width as usize))
                .style(Style::default().fg(theme.divider)),
            div_rect,
        );

        let list = self.items(app);
        let list_h = inner.height.saturating_sub(2) as usize;
        if list.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled("  No matching notes or commands.", theme.muted_style())),
                Rect::new(inner.x, inner.y + 2, inner.width, 1),
            );
            return;
        }

        let sel = self.selected.min(list.len() - 1);
        let offset = selection_offset(sel, list_h, list.len());

        for (i, item) in list.iter().skip(offset).take(list_h).enumerate() {
            let idx = offset + i;
            let row_y = inner.y + 2 + i as u16;
            let row_rect = Rect::new(inner.x, row_y, inner.width, 1);
            let is_sel = idx == sel;
            let row_style = if is_sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated)
            };

            let line = match item {
                SwitcherItem::Note {
                    title,
                    folder,
                    tags,
                    backlinks,
                    ..
                } => {
                    let mut spans = vec![
                        Span::styled(
                            if is_sel { " ▸ " } else { "   " },
                            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("▪ ", Style::default().fg(theme.accent_soft)),
                        Span::styled(
                            truncate_width(title, 34),
                            Style::default()
                                .fg(theme.fg_primary)
                                .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() }),
                        ),
                    ];
                    if !folder.is_empty() {
                        spans.push(Span::styled(
                            format!("  {}", truncate_width(folder, 18)),
                            theme.muted_style(),
                        ));
                    }
                    if let Some(first_tag) = tags.first() {
                        spans.push(Span::styled(format!("  #{first_tag}"), theme.tag_pill_style()));
                    }
                    if *backlinks > 0 {
                        spans.push(Span::styled(format!("  ←{backlinks}"), theme.muted_style()));
                    }
                    Line::from(spans)
                }
                SwitcherItem::HtmlFile { title, folder, .. } => {
                    let mut spans = vec![
                        Span::styled(
                            if is_sel { " ▸ " } else { "   " },
                            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("◈ ", Style::default().fg(theme.info)),
                        Span::styled(
                            truncate_width(title, 34),
                            Style::default()
                                .fg(theme.fg_primary)
                                .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() }),
                        ),
                        Span::styled("  [HTML]", Style::default().fg(theme.info)),
                    ];
                    if !folder.is_empty() {
                        spans.push(Span::styled(
                            format!("  {}", truncate_width(folder, 18)),
                            theme.muted_style(),
                        ));
                    }
                    Line::from(spans)
                }
                SwitcherItem::Command {
                    label, shortcut, ..
                } => Line::from(vec![
                    Span::styled(
                        if is_sel { " ▸ " } else { "   " },
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("⚡ ", Style::default().fg(theme.warning)),
                    Span::styled(
                        truncate_width(label, (inner.width as usize).saturating_sub(18)),
                        Style::default().fg(theme.fg_primary),
                    ),
                    Span::styled(format!("  [{shortcut}]"), theme.muted_style()),
                ]),
            };

            frame.render_widget(Paragraph::new(line).style(row_style), row_rect);
            hitmap.add(row_rect, HitTarget::OverlayItem { index: idx });
        }
    }
}

fn fuzzy_score(
    needle: &str,
    title: &str,
    rel_path: &str,
    tags: &[String],
    aliases: &[String],
) -> Option<i32> {
    if needle.is_empty() {
        return Some(10);
    }
    let tl = title.to_ascii_lowercase();
    let pl = rel_path.to_ascii_lowercase();
    if tl == needle {
        return Some(1000);
    }
    if tl.starts_with(needle) {
        return Some(800);
    }
    if tl.contains(needle) {
        return Some(600);
    }
    if aliases.iter().any(|a| a.to_ascii_lowercase().contains(needle)) {
        return Some(500);
    }
    if tags.iter().any(|t| {
        let t_low = t.to_ascii_lowercase();
        let n_clean = needle.trim_start_matches('#');
        t_low.contains(n_clean)
    }) {
        return Some(450);
    }
    if pl.contains(needle) {
        return Some(350);
    }
    // Subsequence match on title
    let mut chars = needle.chars();
    let mut cur = chars.next()?;
    for c in tl.chars() {
        if c == cur {
            if let Some(next) = chars.next() {
                cur = next;
            } else {
                return Some(200);
            }
        }
    }
    None
}
