use ratatui::style::{Color, Modifier, Style};

use orca_core::config::ThemeName;

use crate::syntax_highlight::SyntaxTheme;
use crate::terminal_capabilities::{
    TerminalBackground, TerminalColorLevel, TerminalProfile, resolve_base_theme,
    syntax_style_revision,
};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub user: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub approval: Color,
    pub plan_mode: Color,
    pub markdown_h1: Color,
    pub markdown_h2: Color,
    pub markdown_h3: Color,
    pub markdown_inline_code: Color,
    pub diff_add: Color,
    pub diff_remove: Color,
    pub diff_add_bg: Color,
    pub diff_remove_bg: Color,
    pub diff_add_emphasis_bg: Color,
    pub diff_remove_emphasis_bg: Color,
    /// Background for the mouse text selection in the transcript.
    pub selection_bg: Color,
    pub search_match_bg: Color,
    pub search_match_active_bg: Color,
    pub(crate) syntax_theme: SyntaxTheme,
    pub(crate) color_level: TerminalColorLevel,
    pub(crate) syntax_theme_revision: u64,
}

impl Theme {
    fn base(name: ThemeName) -> Self {
        if name == ThemeName::Auto {
            return Self::base(ThemeName::Dark);
        }

        let syntax_theme = match name {
            ThemeName::Dark => SyntaxTheme::OneHalfDark,
            ThemeName::Light => SyntaxTheme::OneHalfLight,
            ThemeName::Solarized => SyntaxTheme::SolarizedDark,
            ThemeName::Catppuccin => SyntaxTheme::CatppuccinMocha,
            ThemeName::Auto => unreachable!(),
        };

        match name {
            // DeepSeek-blue truecolor palette. Brand accent #4D6BFE drives
            // borders, selection, and the user prompt.
            ThemeName::Dark => Self {
                border: Color::Rgb(77, 107, 254),
                text: Color::Rgb(232, 236, 246),
                muted: Color::Rgb(139, 147, 167),
                user: Color::Rgb(77, 107, 254),
                success: Color::Rgb(47, 177, 112),
                warning: Color::Rgb(217, 164, 65),
                error: Color::Rgb(214, 81, 81),
                approval: Color::Rgb(169, 139, 245),
                plan_mode: Color::Rgb(64, 170, 170),
                markdown_h1: Color::Rgb(77, 107, 254),
                markdown_h2: Color::Rgb(169, 139, 245),
                markdown_h3: Color::Rgb(217, 164, 65),
                markdown_inline_code: Color::Rgb(64, 170, 170),
                diff_add: Color::Rgb(47, 177, 112),
                diff_remove: Color::Rgb(214, 81, 81),
                diff_add_bg: Color::Rgb(0x21, 0x3a, 0x2b),
                diff_remove_bg: Color::Rgb(0x4a, 0x22, 0x1d),
                diff_add_emphasis_bg: Color::Rgb(0x31, 0x5c, 0x40),
                diff_remove_emphasis_bg: Color::Rgb(0x71, 0x35, 0x2a),
                // Muted brand blue: keeps every foreground legible.
                selection_bg: Color::Rgb(46, 62, 132),
                search_match_bg: Color::Rgb(78, 67, 31),
                search_match_active_bg: Color::Rgb(77, 107, 254),
                syntax_theme,
                color_level: TerminalColorLevel::TrueColor,
                syntax_theme_revision: syntax_theme.revision(),
            },
            ThemeName::Light => Self {
                border: Color::Rgb(58, 86, 230),
                text: Color::Rgb(28, 32, 44),
                muted: Color::Rgb(110, 118, 138),
                user: Color::Rgb(58, 86, 230),
                success: Color::Rgb(31, 142, 86),
                warning: Color::Rgb(176, 122, 20),
                error: Color::Rgb(196, 52, 52),
                approval: Color::Rgb(138, 92, 230),
                plan_mode: Color::Rgb(0, 102, 102),
                markdown_h1: Color::Rgb(58, 86, 230),
                markdown_h2: Color::Rgb(138, 92, 230),
                markdown_h3: Color::Rgb(176, 122, 20),
                markdown_inline_code: Color::Rgb(0, 102, 102),
                diff_add: Color::Rgb(31, 142, 86),
                diff_remove: Color::Rgb(196, 52, 52),
                diff_add_bg: Color::Rgb(0xdc, 0xfc, 0xe7),
                diff_remove_bg: Color::Rgb(0xfe, 0xe2, 0xe2),
                diff_add_emphasis_bg: Color::Rgb(0x86, 0xef, 0xac),
                diff_remove_emphasis_bg: Color::Rgb(0xfc, 0xa5, 0xa5),
                selection_bg: Color::Rgb(198, 210, 250),
                search_match_bg: Color::Rgb(255, 235, 153),
                search_match_active_bg: Color::Rgb(166, 188, 255),
                syntax_theme,
                color_level: TerminalColorLevel::TrueColor,
                syntax_theme_revision: syntax_theme.revision(),
            },
            ThemeName::Solarized => Self {
                border: Color::Rgb(38, 139, 210),
                text: Color::Rgb(147, 161, 161),
                muted: Color::Rgb(88, 110, 117),
                user: Color::Rgb(38, 139, 210),
                success: Color::Rgb(133, 153, 0),
                warning: Color::Rgb(181, 137, 0),
                error: Color::Rgb(220, 50, 47),
                approval: Color::Rgb(108, 113, 196),
                plan_mode: Color::Rgb(42, 161, 152),
                markdown_h1: Color::Rgb(38, 139, 210),
                markdown_h2: Color::Rgb(42, 161, 152),
                markdown_h3: Color::Rgb(181, 137, 0),
                markdown_inline_code: Color::Rgb(211, 54, 130),
                diff_add: Color::Rgb(133, 153, 0),
                diff_remove: Color::Rgb(220, 50, 47),
                diff_add_bg: Color::Rgb(0x16, 0x3c, 0x3a),
                diff_remove_bg: Color::Rgb(0x4c, 0x2a, 0x2a),
                diff_add_emphasis_bg: Color::Rgb(0x24, 0x5b, 0x52),
                diff_remove_emphasis_bg: Color::Rgb(0x71, 0x3c, 0x35),
                // base02, Solarized's canonical selection background.
                selection_bg: Color::Rgb(7, 54, 66),
                search_match_bg: Color::Rgb(88, 73, 0),
                search_match_active_bg: Color::Rgb(38, 139, 210),
                syntax_theme,
                color_level: TerminalColorLevel::TrueColor,
                syntax_theme_revision: syntax_theme.revision(),
            },
            ThemeName::Catppuccin => Self {
                border: Color::Rgb(203, 166, 247),
                text: Color::Rgb(205, 214, 244),
                muted: Color::Rgb(147, 153, 178),
                user: Color::Rgb(137, 220, 235),
                success: Color::Rgb(166, 227, 161),
                warning: Color::Rgb(249, 226, 175),
                error: Color::Rgb(243, 139, 168),
                approval: Color::Rgb(203, 166, 247),
                plan_mode: Color::Rgb(148, 226, 213),
                markdown_h1: Color::Rgb(203, 166, 247),
                markdown_h2: Color::Rgb(116, 199, 236),
                markdown_h3: Color::Rgb(249, 226, 175),
                markdown_inline_code: Color::Rgb(245, 194, 231),
                diff_add: Color::Rgb(166, 227, 161),
                diff_remove: Color::Rgb(243, 139, 168),
                diff_add_bg: Color::Rgb(0x29, 0x44, 0x36),
                diff_remove_bg: Color::Rgb(0x4a, 0x30, 0x3a),
                diff_add_emphasis_bg: Color::Rgb(0x3d, 0x65, 0x4d),
                diff_remove_emphasis_bg: Color::Rgb(0x70, 0x45, 0x55),
                // surface2 from the Mocha palette.
                selection_bg: Color::Rgb(88, 91, 112),
                search_match_bg: Color::Rgb(88, 91, 112),
                search_match_active_bg: Color::Rgb(137, 180, 250),
                syntax_theme,
                color_level: TerminalColorLevel::TrueColor,
                syntax_theme_revision: syntax_theme.revision(),
            },
            ThemeName::Auto => unreachable!(),
        }
    }

    pub fn named(name: ThemeName) -> Self {
        Self::resolve(
            name,
            TerminalProfile {
                background: TerminalBackground::Unknown,
                color_level: TerminalColorLevel::TrueColor,
            },
        )
    }

    pub(crate) fn resolve(name: ThemeName, profile: TerminalProfile) -> Self {
        let mut theme = Self::base(resolve_base_theme(name, profile.background));
        let adapt = |color| profile.color_level.adapt_color(color);
        theme.border = adapt(theme.border);
        theme.text = adapt(theme.text);
        theme.muted = adapt(theme.muted);
        theme.user = adapt(theme.user);
        theme.success = adapt(theme.success);
        theme.warning = adapt(theme.warning);
        theme.error = adapt(theme.error);
        theme.approval = adapt(theme.approval);
        theme.plan_mode = adapt(theme.plan_mode);
        theme.markdown_h1 = adapt(theme.markdown_h1);
        theme.markdown_h2 = adapt(theme.markdown_h2);
        theme.markdown_h3 = adapt(theme.markdown_h3);
        theme.markdown_inline_code = adapt(theme.markdown_inline_code);
        theme.diff_add = adapt(theme.diff_add);
        theme.diff_remove = adapt(theme.diff_remove);
        theme.diff_add_bg = adapt(theme.diff_add_bg);
        theme.diff_remove_bg = adapt(theme.diff_remove_bg);
        theme.diff_add_emphasis_bg = adapt(theme.diff_add_emphasis_bg);
        theme.diff_remove_emphasis_bg = adapt(theme.diff_remove_emphasis_bg);
        theme.selection_bg = adapt(theme.selection_bg);
        theme.search_match_bg = adapt(theme.search_match_bg);
        theme.search_match_active_bg = adapt(theme.search_match_active_bg);
        theme.color_level = profile.color_level;
        theme.syntax_theme_revision =
            syntax_style_revision(theme.syntax_theme, profile.color_level);
        theme
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn selection_style(self) -> Style {
        match self.color_level {
            TerminalColorLevel::Monochrome => Style::default().add_modifier(Modifier::REVERSED),
            _ => Style::default().bg(self.selection_bg),
        }
    }

    pub(crate) fn accent_style(&self) -> Style {
        Style::default().fg(self.border)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn muted_style(&self) -> Style {
        Style::default().fg(self.muted)
    }

    /// Secondary chrome (rails, rule when unfocused, trailing hints): muted plus
    /// DIM so it recedes even on 16-color terminals.
    pub(crate) fn dim_style(&self) -> Style {
        Style::default().fg(self.muted).add_modifier(Modifier::DIM)
    }

    pub(crate) fn search_match_style(self) -> Style {
        match self.color_level {
            TerminalColorLevel::Monochrome => Style::default().add_modifier(Modifier::UNDERLINED),
            _ => Style::default().bg(self.search_match_bg),
        }
    }

    pub(crate) fn search_match_active_style(self) -> Style {
        match self.color_level {
            TerminalColorLevel::Monochrome => {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            }
            _ => Style::default()
                .bg(self.search_match_active_bg)
                .add_modifier(Modifier::BOLD),
        }
    }
}

#[cfg(test)]
mod tests {
    use orca_core::config::ThemeName;
    use ratatui::style::{Color, Modifier, Style};

    use super::Theme;
    use crate::syntax_highlight::SyntaxTheme;
    use crate::terminal_capabilities::{TerminalBackground, TerminalColorLevel, TerminalProfile};

    fn theme_colors(theme: Theme) -> [Color; 22] {
        [
            theme.border,
            theme.text,
            theme.muted,
            theme.user,
            theme.success,
            theme.warning,
            theme.error,
            theme.approval,
            theme.plan_mode,
            theme.markdown_h1,
            theme.markdown_h2,
            theme.markdown_h3,
            theme.markdown_inline_code,
            theme.diff_add,
            theme.diff_remove,
            theme.diff_add_bg,
            theme.diff_remove_bg,
            theme.diff_add_emphasis_bg,
            theme.diff_remove_emphasis_bg,
            theme.selection_bg,
            theme.search_match_bg,
            theme.search_match_active_bg,
        ]
    }

    fn color_fits_level(level: TerminalColorLevel, color: Color) -> bool {
        match level {
            TerminalColorLevel::TrueColor => true,
            TerminalColorLevel::Ansi256 => !matches!(color, Color::Rgb(..)),
            TerminalColorLevel::Ansi16 => !matches!(color, Color::Rgb(..) | Color::Indexed(_)),
            TerminalColorLevel::Monochrome => color == Color::Reset,
        }
    }

    fn assert_theme_colors_fit_level(theme: Theme) {
        assert!(
            theme_colors(theme)
                .into_iter()
                .all(|color| color_fits_level(theme.color_level, color)),
            "{theme:?}"
        );
    }

    #[test]
    fn named_auto_uses_the_existing_dark_palette_without_terminal_context() {
        let auto = Theme::named(ThemeName::Auto);
        let dark = Theme::named(ThemeName::Dark);
        assert_eq!(theme_colors(auto), theme_colors(dark));
        assert_eq!(auto.syntax_theme, dark.syntax_theme);
        assert_eq!(auto.color_level, TerminalColorLevel::TrueColor);
    }

    #[test]
    fn named_themes_preserve_exact_truecolor_palettes() {
        let cases = [
            (
                ThemeName::Dark,
                [
                    Color::Rgb(77, 107, 254),
                    Color::Rgb(232, 236, 246),
                    Color::Rgb(139, 147, 167),
                    Color::Rgb(77, 107, 254),
                    Color::Rgb(47, 177, 112),
                    Color::Rgb(217, 164, 65),
                    Color::Rgb(214, 81, 81),
                    Color::Rgb(169, 139, 245),
                    Color::Rgb(64, 170, 170),
                    Color::Rgb(77, 107, 254),
                    Color::Rgb(169, 139, 245),
                    Color::Rgb(217, 164, 65),
                    Color::Rgb(64, 170, 170),
                    Color::Rgb(47, 177, 112),
                    Color::Rgb(214, 81, 81),
                    Color::Rgb(0x21, 0x3a, 0x2b),
                    Color::Rgb(0x4a, 0x22, 0x1d),
                    Color::Rgb(0x31, 0x5c, 0x40),
                    Color::Rgb(0x71, 0x35, 0x2a),
                    Color::Rgb(46, 62, 132),
                    Color::Rgb(78, 67, 31),
                    Color::Rgb(77, 107, 254),
                ],
            ),
            (
                ThemeName::Light,
                [
                    Color::Rgb(58, 86, 230),
                    Color::Rgb(28, 32, 44),
                    Color::Rgb(110, 118, 138),
                    Color::Rgb(58, 86, 230),
                    Color::Rgb(31, 142, 86),
                    Color::Rgb(176, 122, 20),
                    Color::Rgb(196, 52, 52),
                    Color::Rgb(138, 92, 230),
                    Color::Rgb(0, 102, 102),
                    Color::Rgb(58, 86, 230),
                    Color::Rgb(138, 92, 230),
                    Color::Rgb(176, 122, 20),
                    Color::Rgb(0, 102, 102),
                    Color::Rgb(31, 142, 86),
                    Color::Rgb(196, 52, 52),
                    Color::Rgb(0xdc, 0xfc, 0xe7),
                    Color::Rgb(0xfe, 0xe2, 0xe2),
                    Color::Rgb(0x86, 0xef, 0xac),
                    Color::Rgb(0xfc, 0xa5, 0xa5),
                    Color::Rgb(198, 210, 250),
                    Color::Rgb(255, 235, 153),
                    Color::Rgb(166, 188, 255),
                ],
            ),
            (
                ThemeName::Solarized,
                [
                    Color::Rgb(38, 139, 210),
                    Color::Rgb(147, 161, 161),
                    Color::Rgb(88, 110, 117),
                    Color::Rgb(38, 139, 210),
                    Color::Rgb(133, 153, 0),
                    Color::Rgb(181, 137, 0),
                    Color::Rgb(220, 50, 47),
                    Color::Rgb(108, 113, 196),
                    Color::Rgb(42, 161, 152),
                    Color::Rgb(38, 139, 210),
                    Color::Rgb(42, 161, 152),
                    Color::Rgb(181, 137, 0),
                    Color::Rgb(211, 54, 130),
                    Color::Rgb(133, 153, 0),
                    Color::Rgb(220, 50, 47),
                    Color::Rgb(0x16, 0x3c, 0x3a),
                    Color::Rgb(0x4c, 0x2a, 0x2a),
                    Color::Rgb(0x24, 0x5b, 0x52),
                    Color::Rgb(0x71, 0x3c, 0x35),
                    Color::Rgb(7, 54, 66),
                    Color::Rgb(88, 73, 0),
                    Color::Rgb(38, 139, 210),
                ],
            ),
            (
                ThemeName::Catppuccin,
                [
                    Color::Rgb(203, 166, 247),
                    Color::Rgb(205, 214, 244),
                    Color::Rgb(147, 153, 178),
                    Color::Rgb(137, 220, 235),
                    Color::Rgb(166, 227, 161),
                    Color::Rgb(249, 226, 175),
                    Color::Rgb(243, 139, 168),
                    Color::Rgb(203, 166, 247),
                    Color::Rgb(148, 226, 213),
                    Color::Rgb(203, 166, 247),
                    Color::Rgb(116, 199, 236),
                    Color::Rgb(249, 226, 175),
                    Color::Rgb(245, 194, 231),
                    Color::Rgb(166, 227, 161),
                    Color::Rgb(243, 139, 168),
                    Color::Rgb(0x29, 0x44, 0x36),
                    Color::Rgb(0x4a, 0x30, 0x3a),
                    Color::Rgb(0x3d, 0x65, 0x4d),
                    Color::Rgb(0x70, 0x45, 0x55),
                    Color::Rgb(88, 91, 112),
                    Color::Rgb(88, 91, 112),
                    Color::Rgb(137, 180, 250),
                ],
            ),
        ];

        for (name, expected) in cases {
            assert_eq!(theme_colors(Theme::named(name)), expected, "{name:?}");
        }
    }

    #[test]
    fn dark_diff_backgrounds_match_the_review_palette() {
        let theme = Theme::named(ThemeName::Dark);
        assert_eq!(theme.diff_add_bg, Color::Rgb(0x21, 0x3a, 0x2b));
        assert_eq!(theme.diff_remove_bg, Color::Rgb(0x4a, 0x22, 0x1d));
        assert_eq!(theme.diff_add_emphasis_bg, Color::Rgb(0x31, 0x5c, 0x40));
        assert_eq!(theme.diff_remove_emphasis_bg, Color::Rgb(0x71, 0x35, 0x2a));
    }

    #[test]
    fn resolved_themes_choose_base_palette_and_obey_color_level() {
        for (requested, background, expected) in [
            (
                ThemeName::Auto,
                TerminalBackground::Light,
                Theme::named(ThemeName::Light),
            ),
            (
                ThemeName::Auto,
                TerminalBackground::Dark,
                Theme::named(ThemeName::Dark),
            ),
            (
                ThemeName::Auto,
                TerminalBackground::Unknown,
                Theme::named(ThemeName::Dark),
            ),
            (
                ThemeName::Solarized,
                TerminalBackground::Light,
                Theme::named(ThemeName::Solarized),
            ),
        ] {
            let resolved = Theme::resolve(
                requested,
                TerminalProfile {
                    background,
                    color_level: TerminalColorLevel::TrueColor,
                },
            );
            assert_eq!(theme_colors(resolved), theme_colors(expected));
            assert_eq!(resolved.syntax_theme, expected.syntax_theme);
        }

        for level in [
            TerminalColorLevel::Ansi256,
            TerminalColorLevel::Ansi16,
            TerminalColorLevel::Monochrome,
        ] {
            for name in [
                ThemeName::Dark,
                ThemeName::Light,
                ThemeName::Solarized,
                ThemeName::Catppuccin,
            ] {
                assert_theme_colors_fit_level(Theme::resolve(
                    name,
                    TerminalProfile {
                        background: TerminalBackground::Unknown,
                        color_level: level,
                    },
                ));
            }
        }
    }

    #[test]
    fn resolved_theme_revisions_preserve_truecolor_and_encode_color_level() {
        for (name, expected_revision) in [
            (ThemeName::Dark, 1),
            (ThemeName::Light, 2),
            (ThemeName::Solarized, 3),
            (ThemeName::Catppuccin, 4),
        ] {
            for (level, offset) in [
                (TerminalColorLevel::TrueColor, 0),
                (TerminalColorLevel::Ansi256, 0x100),
                (TerminalColorLevel::Ansi16, 0x200),
                (TerminalColorLevel::Monochrome, 0x300),
            ] {
                let theme = Theme::resolve(
                    name,
                    TerminalProfile {
                        background: TerminalBackground::Unknown,
                        color_level: level,
                    },
                );
                assert_eq!(theme.syntax_theme_revision, expected_revision + offset);
            }
        }
    }

    #[test]
    fn selection_style_uses_adapted_background_or_monochrome_reversal() {
        let color_theme = Theme::resolve(
            ThemeName::Dark,
            TerminalProfile {
                background: TerminalBackground::Unknown,
                color_level: TerminalColorLevel::Ansi16,
            },
        );
        assert_eq!(
            color_theme.selection_style(),
            Style::default().bg(color_theme.selection_bg)
        );

        let monochrome = Theme::resolve(
            ThemeName::Dark,
            TerminalProfile {
                background: TerminalBackground::Unknown,
                color_level: TerminalColorLevel::Monochrome,
            },
        );
        assert_eq!(
            monochrome.selection_style(),
            Style::default().add_modifier(Modifier::REVERSED)
        );
    }

    #[test]
    fn search_styles_are_distinct_and_capability_safe() {
        for name in [
            ThemeName::Dark,
            ThemeName::Light,
            ThemeName::Solarized,
            ThemeName::Catppuccin,
        ] {
            for level in [
                TerminalColorLevel::TrueColor,
                TerminalColorLevel::Ansi256,
                TerminalColorLevel::Ansi16,
                TerminalColorLevel::Monochrome,
            ] {
                let theme = Theme::resolve(
                    name,
                    TerminalProfile {
                        background: TerminalBackground::Unknown,
                        color_level: level,
                    },
                );
                assert_ne!(
                    theme.search_match_style(),
                    theme.search_match_active_style()
                );
                if level == TerminalColorLevel::Monochrome {
                    assert!(
                        theme
                            .search_match_style()
                            .add_modifier
                            .contains(Modifier::UNDERLINED)
                    );
                    assert!(
                        theme
                            .search_match_active_style()
                            .add_modifier
                            .contains(Modifier::REVERSED)
                    );
                }
            }
        }
    }

    #[test]
    fn named_themes_map_to_matching_syntax_themes_and_revisions() {
        let cases = [
            (ThemeName::Dark, SyntaxTheme::OneHalfDark),
            (ThemeName::Light, SyntaxTheme::OneHalfLight),
            (ThemeName::Solarized, SyntaxTheme::SolarizedDark),
            (ThemeName::Catppuccin, SyntaxTheme::CatppuccinMocha),
        ];

        for (name, syntax_theme) in cases {
            let theme = Theme::named(name);
            assert_eq!(theme.syntax_theme, syntax_theme);
            assert_eq!(theme.syntax_theme_revision, syntax_theme.revision());
        }
    }

    #[test]
    fn named_themes_define_markdown_semantic_colors() {
        let cases = [
            (
                ThemeName::Dark,
                [
                    Color::Rgb(77, 107, 254),
                    Color::Rgb(169, 139, 245),
                    Color::Rgb(217, 164, 65),
                    Color::Rgb(64, 170, 170),
                ],
            ),
            (
                ThemeName::Light,
                [
                    Color::Rgb(58, 86, 230),
                    Color::Rgb(138, 92, 230),
                    Color::Rgb(176, 122, 20),
                    Color::Rgb(0, 102, 102),
                ],
            ),
            (
                ThemeName::Solarized,
                [
                    Color::Rgb(38, 139, 210),
                    Color::Rgb(42, 161, 152),
                    Color::Rgb(181, 137, 0),
                    Color::Rgb(211, 54, 130),
                ],
            ),
            (
                ThemeName::Catppuccin,
                [
                    Color::Rgb(203, 166, 247),
                    Color::Rgb(116, 199, 236),
                    Color::Rgb(249, 226, 175),
                    Color::Rgb(245, 194, 231),
                ],
            ),
        ];

        for (name, expected) in cases {
            let theme = Theme::named(name);
            assert_eq!(
                [
                    theme.markdown_h1,
                    theme.markdown_h2,
                    theme.markdown_h3,
                    theme.markdown_inline_code,
                ],
                expected,
                "{name:?}"
            );
        }
    }

    #[test]
    fn markdown_semantic_colors_do_not_use_fixed_ansi_accents() {
        let forbidden = [Color::Cyan, Color::Green, Color::Yellow, Color::Magenta];

        for name in [
            ThemeName::Dark,
            ThemeName::Light,
            ThemeName::Solarized,
            ThemeName::Catppuccin,
        ] {
            let theme = Theme::named(name);
            for color in [
                theme.markdown_h1,
                theme.markdown_h2,
                theme.markdown_h3,
                theme.markdown_inline_code,
            ] {
                assert!(!forbidden.contains(&color), "{name:?}: {color:?}");
            }
        }
    }
}
