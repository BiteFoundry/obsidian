//! Semantic multi-depth color & style tokens for the Obsidian Terminal UI.
//!
//! Includes 5 curated truecolor palettes:
//! - `Obsidian Dark` (default: iconic `#a78bfa` / `#7c3aed` purple accent on `#131318` deep obsidian)
//! - `Catppuccin Mocha`
//! - `Tokyo Night`
//! - `Nord`
//! - `Warm Paper`
//!
//! Plus `ANSI 256`, `ANSI 16`, and `NO_COLOR`.

use ratatui::style::{Color, Modifier, Style};

use crate::{config::ThemePreset, models::CalloutKind};

#[derive(Debug, Clone)]
pub struct Theme {
    pub no_color: bool,
    pub ansi16_only: bool,
    pub is_light: bool,
    // Surfaces
    pub bg_canvas: Color,
    pub bg_sidebar: Color,
    pub bg_header: Color,
    pub bg_composer: Color,
    pub bg_elevated: Color,
    pub bg_selected: Color,
    pub bg_code: Color,
    pub bg_card: Color,
    // Foregrounds
    pub fg_primary: Color,
    pub fg_secondary: Color,
    pub fg_muted: Color,
    pub fg_subtle: Color,
    // Semantic
    pub accent: Color,
    pub accent_deep: Color,
    pub accent_soft: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,
    pub code_fg: Color,
    pub tag_fg: Color,
    pub tag_bg: Color,
    // Shared aliases
    pub fg: Color,
    pub muted: Color,
    pub border: Color,
    pub border_focus: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub divider: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::obsidian_dark()
    }
}

impl Theme {
    pub fn detect(preset: ThemePreset) -> Self {
        if preset == ThemePreset::NoColor
            || std::env::var("NO_COLOR").is_ok_and(|v| !v.trim().is_empty())
        {
            return Self::no_color();
        }
        match preset {
            ThemePreset::ObsidianDark => Self::obsidian_dark(),
            ThemePreset::Catppuccin => Self::catppuccin(),
            ThemePreset::TokyoNight => Self::tokyo_night(),
            ThemePreset::Nord => Self::nord(),
            ThemePreset::WarmPaper => Self::warm_paper(),
            ThemePreset::Ansi256 => Self::ansi256(),
            ThemePreset::Ansi16 => Self::ansi16(),
            ThemePreset::NoColor => Self::no_color(),
            ThemePreset::Auto => {
                let colorterm = std::env::var("COLORTERM")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if colorterm.contains("truecolor") || colorterm.contains("24bit") {
                    return Self::obsidian_dark();
                }
                let term = std::env::var("TERM")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if term == "dumb" {
                    return Self::no_color();
                }
                if term.contains("256color") {
                    return Self::ansi256();
                }
                if cfg!(windows) || std::env::var("WT_SESSION").is_ok() {
                    return Self::obsidian_dark();
                }
                if term.is_empty() {
                    Self::ansi256()
                } else {
                    Self::ansi16()
                }
            }
        }
    }

    /// Signature Obsidian Dark palette: `#a78bfa` / `#7c3aed` purple accent on `#131318` deep obsidian.
    pub fn obsidian_dark() -> Self {
        let bg_selected = Color::Rgb(42, 36, 66);
        let fg_primary = Color::Rgb(234, 234, 242);
        let fg_muted = Color::Rgb(148, 148, 168);
        let fg_subtle = Color::Rgb(82, 82, 102);
        let accent = Color::Rgb(167, 139, 250); // #a78bfa
        let accent_deep = Color::Rgb(124, 58, 237); // #7c3aed
        let accent_soft = Color::Rgb(196, 181, 253); // #c4b5fd
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: false,
            bg_canvas: Color::Rgb(19, 19, 24),
            bg_sidebar: Color::Rgb(24, 24, 32),
            bg_header: Color::Rgb(22, 22, 30),
            bg_composer: Color::Rgb(30, 30, 42),
            bg_elevated: Color::Rgb(29, 29, 40),
            bg_selected,
            bg_code: Color::Rgb(28, 28, 38),
            bg_card: Color::Rgb(26, 26, 36),
            fg_primary,
            fg_secondary: Color::Rgb(203, 204, 220),
            fg_muted,
            fg_subtle,
            accent,
            accent_deep,
            accent_soft,
            success: Color::Rgb(74, 222, 128),
            warning: Color::Rgb(251, 191, 36),
            error: Color::Rgb(248, 113, 113),
            info: Color::Rgb(96, 165, 250),
            code_fg: Color::Rgb(244, 162, 118),
            tag_fg: Color::Rgb(196, 181, 253),
            tag_bg: Color::Rgb(49, 38, 84),
            fg: fg_primary,
            muted: fg_muted,
            border: Color::Rgb(50, 48, 68),
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: Color::Rgb(42, 42, 58),
        }
    }

    /// Catppuccin Mocha palette with Mauve `#cba6f7` accent.
    pub fn catppuccin() -> Self {
        let bg_selected = Color::Rgb(49, 50, 68);
        let fg_primary = Color::Rgb(205, 214, 244);
        let fg_muted = Color::Rgb(147, 153, 178);
        let fg_subtle = Color::Rgb(88, 91, 112);
        let accent = Color::Rgb(203, 166, 247);
        let accent_deep = Color::Rgb(136, 57, 239);
        let accent_soft = Color::Rgb(180, 190, 254);
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: false,
            bg_canvas: Color::Rgb(30, 30, 46),
            bg_sidebar: Color::Rgb(24, 24, 37),
            bg_header: Color::Rgb(36, 39, 58),
            bg_composer: Color::Rgb(49, 50, 68),
            bg_elevated: Color::Rgb(42, 43, 60),
            bg_selected,
            bg_code: Color::Rgb(24, 24, 37),
            bg_card: Color::Rgb(38, 39, 56),
            fg_primary,
            fg_secondary: Color::Rgb(186, 194, 222),
            fg_muted,
            fg_subtle,
            accent,
            accent_deep,
            accent_soft,
            success: Color::Rgb(166, 227, 161),
            warning: Color::Rgb(249, 226, 175),
            error: Color::Rgb(243, 139, 168),
            info: Color::Rgb(137, 180, 250),
            code_fg: Color::Rgb(250, 179, 135),
            tag_fg: Color::Rgb(203, 166, 247),
            tag_bg: Color::Rgb(56, 48, 82),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: fg_subtle,
        }
    }

    pub fn tokyo_night() -> Self {
        let bg_selected = Color::Rgb(41, 46, 66);
        let fg_primary = Color::Rgb(192, 202, 245);
        let fg_muted = Color::Rgb(115, 122, 162);
        let fg_subtle = Color::Rgb(65, 72, 104);
        let accent = Color::Rgb(187, 154, 247);
        let accent_deep = Color::Rgb(122, 162, 247);
        let accent_soft = Color::Rgb(125, 207, 255);
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: false,
            bg_canvas: Color::Rgb(26, 27, 38),
            bg_sidebar: Color::Rgb(22, 22, 30),
            bg_header: Color::Rgb(31, 35, 53),
            bg_composer: Color::Rgb(36, 40, 59),
            bg_elevated: Color::Rgb(36, 40, 59),
            bg_selected,
            bg_code: Color::Rgb(22, 22, 30),
            bg_card: Color::Rgb(31, 35, 53),
            fg_primary,
            fg_secondary: Color::Rgb(169, 177, 214),
            fg_muted,
            fg_subtle,
            accent,
            accent_deep,
            accent_soft,
            success: Color::Rgb(158, 206, 106),
            warning: Color::Rgb(224, 175, 104),
            error: Color::Rgb(247, 118, 142),
            info: Color::Rgb(122, 162, 247),
            code_fg: Color::Rgb(255, 158, 100),
            tag_fg: Color::Rgb(187, 154, 247),
            tag_bg: Color::Rgb(46, 42, 72),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: fg_subtle,
        }
    }

    pub fn nord() -> Self {
        let bg_selected = Color::Rgb(67, 76, 94);
        let fg_primary = Color::Rgb(236, 239, 244);
        let fg_muted = Color::Rgb(143, 158, 179);
        let fg_subtle = Color::Rgb(76, 86, 106);
        let accent = Color::Rgb(180, 142, 173);
        let accent_deep = Color::Rgb(136, 192, 208);
        let accent_soft = Color::Rgb(129, 161, 193);
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: false,
            bg_canvas: Color::Rgb(46, 52, 64),
            bg_sidebar: Color::Rgb(39, 44, 54),
            bg_header: Color::Rgb(59, 66, 82),
            bg_composer: Color::Rgb(59, 66, 82),
            bg_elevated: Color::Rgb(59, 66, 82),
            bg_selected,
            bg_code: Color::Rgb(39, 44, 54),
            bg_card: Color::Rgb(59, 66, 82),
            fg_primary,
            fg_secondary: Color::Rgb(216, 222, 233),
            fg_muted,
            fg_subtle,
            accent,
            accent_deep,
            accent_soft,
            success: Color::Rgb(163, 190, 140),
            warning: Color::Rgb(235, 203, 139),
            error: Color::Rgb(191, 97, 106),
            info: Color::Rgb(136, 192, 208),
            code_fg: Color::Rgb(208, 135, 112),
            tag_fg: Color::Rgb(216, 222, 233),
            tag_bg: Color::Rgb(76, 86, 106),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: fg_subtle,
        }
    }

    pub fn warm_paper() -> Self {
        let bg_selected = Color::Rgb(237, 233, 254);
        let fg_primary = Color::Rgb(38, 36, 44);
        let fg_muted = Color::Rgb(110, 108, 120);
        let fg_subtle = Color::Rgb(196, 194, 204);
        let accent = Color::Rgb(124, 58, 237);
        let accent_deep = Color::Rgb(91, 33, 182);
        let accent_soft = Color::Rgb(139, 92, 246);
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: true,
            bg_canvas: Color::Rgb(252, 251, 248),
            bg_sidebar: Color::Rgb(245, 243, 239),
            bg_header: Color::Rgb(249, 248, 245),
            bg_composer: Color::Rgb(245, 243, 239),
            bg_elevated: Color::Rgb(244, 242, 238),
            bg_selected,
            bg_code: Color::Rgb(240, 238, 234),
            bg_card: Color::Rgb(246, 244, 240),
            fg_primary,
            fg_secondary: Color::Rgb(70, 68, 78),
            fg_muted,
            fg_subtle,
            accent,
            accent_deep,
            accent_soft,
            success: Color::Rgb(21, 128, 61),
            warning: Color::Rgb(180, 83, 9),
            error: Color::Rgb(185, 28, 28),
            info: Color::Rgb(29, 78, 216),
            code_fg: Color::Rgb(190, 24, 93),
            tag_fg: Color::Rgb(109, 40, 217),
            tag_bg: Color::Rgb(237, 233, 254),
            fg: fg_primary,
            muted: fg_muted,
            border: Color::Rgb(222, 220, 226),
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: Color::Rgb(230, 228, 234),
        }
    }

    pub fn ansi256() -> Self {
        let bg_selected = Color::Indexed(237);
        let fg_primary = Color::Indexed(254);
        let fg_muted = Color::Indexed(244);
        let fg_subtle = Color::Indexed(239);
        let accent = Color::Indexed(141);
        let accent_deep = Color::Indexed(98);
        let accent_soft = Color::Indexed(183);
        Self {
            no_color: false,
            ansi16_only: false,
            is_light: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Indexed(234),
            bg_header: Color::Indexed(235),
            bg_composer: Color::Indexed(235),
            bg_elevated: Color::Indexed(236),
            bg_selected,
            bg_code: Color::Indexed(235),
            bg_card: Color::Indexed(235),
            fg_primary,
            fg_secondary: Color::Indexed(249),
            fg_muted,
            fg_subtle,
            accent,
            accent_deep,
            accent_soft,
            success: Color::Indexed(79),
            warning: Color::Indexed(179),
            error: Color::Indexed(204),
            info: Color::Indexed(75),
            code_fg: Color::Indexed(209),
            tag_fg: Color::Indexed(183),
            tag_bg: Color::Indexed(53),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            divider: fg_subtle,
        }
    }

    pub fn ansi16() -> Self {
        Self {
            no_color: false,
            ansi16_only: true,
            is_light: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Reset,
            bg_header: Color::Reset,
            bg_composer: Color::Reset,
            bg_elevated: Color::Reset,
            bg_selected: Color::DarkGray,
            bg_code: Color::Black,
            bg_card: Color::Reset,
            fg_primary: Color::Reset,
            fg_secondary: Color::White,
            fg_muted: Color::DarkGray,
            fg_subtle: Color::DarkGray,
            accent: Color::Magenta,
            accent_deep: Color::Magenta,
            accent_soft: Color::LightMagenta,
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            info: Color::Cyan,
            code_fg: Color::Yellow,
            tag_fg: Color::LightMagenta,
            tag_bg: Color::Reset,
            fg: Color::Reset,
            muted: Color::DarkGray,
            border: Color::DarkGray,
            border_focus: Color::Magenta,
            selection_bg: Color::DarkGray,
            selection_fg: Color::White,
            divider: Color::DarkGray,
        }
    }

    pub fn no_color() -> Self {
        Self {
            no_color: true,
            ansi16_only: true,
            is_light: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Reset,
            bg_header: Color::Reset,
            bg_composer: Color::Reset,
            bg_elevated: Color::Reset,
            bg_selected: Color::Reset,
            bg_code: Color::Reset,
            bg_card: Color::Reset,
            fg_primary: Color::Reset,
            fg_secondary: Color::Reset,
            fg_muted: Color::Reset,
            fg_subtle: Color::Reset,
            accent: Color::Reset,
            accent_deep: Color::Reset,
            accent_soft: Color::Reset,
            success: Color::Reset,
            warning: Color::Reset,
            error: Color::Reset,
            info: Color::Reset,
            code_fg: Color::Reset,
            tag_fg: Color::Reset,
            tag_bg: Color::Reset,
            fg: Color::Reset,
            muted: Color::Reset,
            border: Color::Reset,
            border_focus: Color::Reset,
            selection_bg: Color::Reset,
            selection_fg: Color::Reset,
            divider: Color::Reset,
        }
    }

    pub fn callout_color(&self, kind: CalloutKind) -> Color {
        if self.no_color {
            return Color::Reset;
        }
        match kind {
            CalloutKind::Note | CalloutKind::Example => self.accent,
            CalloutKind::Tip | CalloutKind::Success => self.success,
            CalloutKind::Info | CalloutKind::Question => self.info,
            CalloutKind::Warning => self.warning,
            CalloutKind::Danger => self.error,
            CalloutKind::Quote => self.fg_muted,
        }
    }

    pub fn tag_pill_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else if self.ansi16_only {
            Style::default()
                .fg(self.tag_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.tag_fg).bg(self.tag_bg)
        }
    }

    pub fn prop_pill_style(&self) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().fg(self.fg_secondary).bg(self.bg_elevated)
        }
    }

    pub fn wikilink_style(&self, resolved: bool) -> Style {
        if self.no_color {
            return Style::default().add_modifier(Modifier::UNDERLINED);
        }
        if resolved {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
                .fg(self.fg_muted)
                .add_modifier(Modifier::UNDERLINED)
        }
    }

    pub fn border_style(&self, focused: bool) -> Style {
        if self.no_color {
            return if focused {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
        }
        Style::default().fg(if focused {
            self.border_focus
        } else {
            self.border
        })
    }

    pub fn selection(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().bg(self.bg_selected)
        }
    }

    pub fn muted_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(self.fg_muted)
        }
    }

    pub fn subtle_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(self.fg_subtle)
        }
    }

    pub fn title(&self, focused: bool) -> Style {
        if focused {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            self.muted_style()
        }
    }

    pub fn accent_style(&self) -> Style {
        Style::default().fg(self.accent)
    }

    pub fn code_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.code_fg).bg(self.bg_code)
        }
    }

    pub fn quote_style(&self) -> Style {
        Style::default()
            .fg(self.fg_secondary)
            .add_modifier(Modifier::ITALIC)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_5_curated_obsidian_palettes_render() {
        for preset in [
            ThemePreset::ObsidianDark,
            ThemePreset::Catppuccin,
            ThemePreset::TokyoNight,
            ThemePreset::Nord,
            ThemePreset::WarmPaper,
            ThemePreset::Ansi256,
            ThemePreset::Ansi16,
            ThemePreset::NoColor,
        ] {
            let t = Theme::detect(preset);
            let _ = t.callout_color(CalloutKind::Tip);
            let _ = t.tag_pill_style();
            let _ = t.wikilink_style(true);
            let _ = t.wikilink_style(false);
        }
    }
}
