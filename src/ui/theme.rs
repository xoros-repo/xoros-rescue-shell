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
