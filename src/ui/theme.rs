//! Theme readable with the 8 basic ANSI colors of the Linux VT (RT-3). Only `Dark(..)`
//! colors are used (bright colors need bold/16-color support).

use cursive::theme::{BaseColor, BorderStyle, Color, Palette, PaletteColor, Theme};

pub fn theme() -> Theme {
    let mut palette = Palette::default();
    let dark = Color::Dark;
    palette[PaletteColor::Background] = dark(BaseColor::Blue);
    palette[PaletteColor::Shadow] = dark(BaseColor::Black);
    palette[PaletteColor::View] = dark(BaseColor::White);
    palette[PaletteColor::Primary] = dark(BaseColor::Black);
    palette[PaletteColor::Secondary] = dark(BaseColor::Blue);
    palette[PaletteColor::Tertiary] = dark(BaseColor::Black);
    palette[PaletteColor::TitlePrimary] = dark(BaseColor::Red);
    palette[PaletteColor::TitleSecondary] = dark(BaseColor::Blue);
    palette[PaletteColor::Highlight] = dark(BaseColor::Blue);
    palette[PaletteColor::HighlightInactive] = dark(BaseColor::Cyan);
    palette[PaletteColor::HighlightText] = dark(BaseColor::White);
    Theme {
        // No shadows: they cost a row and a column on an 80x24 screen (RT-4).
        shadow: false,
        borders: BorderStyle::Simple,
        palette,
    }
}

/// Theme by its settings name (CFG-1 `theme`, see [`crate::settings::THEMES`]); unknown names
/// get the default theme.
pub fn named(name: &str) -> Theme {
    let dark = Color::Dark;
    let mut t = theme();
    let p = &mut t.palette;
    match name {
        "dark" => {
            p[PaletteColor::Background] = dark(BaseColor::Black);
            p[PaletteColor::View] = dark(BaseColor::Black);
            p[PaletteColor::Primary] = dark(BaseColor::White);
            p[PaletteColor::Secondary] = dark(BaseColor::Cyan);
            p[PaletteColor::Tertiary] = dark(BaseColor::White);
            p[PaletteColor::TitlePrimary] = dark(BaseColor::Cyan);
            p[PaletteColor::TitleSecondary] = dark(BaseColor::Cyan);
            p[PaletteColor::Highlight] = dark(BaseColor::Cyan);
            p[PaletteColor::HighlightInactive] = dark(BaseColor::Blue);
            p[PaletteColor::HighlightText] = dark(BaseColor::Black);
        }
        "mono" => {
            for c in [
                PaletteColor::Background,
                PaletteColor::Shadow,
                PaletteColor::View,
                PaletteColor::HighlightText,
            ] {
                p[c] = dark(BaseColor::Black);
            }
            for c in [
                PaletteColor::Primary,
                PaletteColor::Secondary,
                PaletteColor::Tertiary,
                PaletteColor::TitlePrimary,
                PaletteColor::TitleSecondary,
                PaletteColor::Highlight,
                PaletteColor::HighlightInactive,
            ] {
                p[c] = dark(BaseColor::White);
            }
        }
        _ => {}
    }
    t
}
