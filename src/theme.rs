use cursive::theme::{BaseColor, Color, Palette, PaletteColor, Theme};

pub(crate) fn theme() -> Theme {
    let mut theme = Theme::default();
    let mut palette = Palette::default();

    palette[PaletteColor::Background] = Color::Dark(BaseColor::Red);
    palette[PaletteColor::View] = Color::Light(BaseColor::Red);
    palette[PaletteColor::Shadow] = Color::Dark(BaseColor::Black);
    palette[PaletteColor::Highlight] = Color::Dark(BaseColor::Black);
    palette[PaletteColor::HighlightText] = Color::Light(BaseColor::White);
    theme.palette = palette;
    return theme;
}
