//! Theme readable with the 8 basic ANSI colors of the Linux VT (RT-3). ratatui's `Gray` is
//! ANSI white (37/47); the bright colors (`White`, `LightBlue`, ...) need 16-color support
//! and are not used.

use ratatui::style::Color;

use crate::ui::tk::Theme;

pub fn theme() -> Theme {
    Theme {
        background: Color::Blue,
        view: Color::Gray,
        primary: Color::Black,
        secondary: Color::Blue,
        title: Color::Red,
        highlight: Color::Blue,
        highlight_inactive: Color::Cyan,
        highlight_text: Color::Gray,
    }
}

/// Theme by its settings name (CFG-1 `theme`, see [`crate::settings::THEMES`]); unknown names
/// get the default theme.
pub fn named(name: &str) -> Theme {
    match name {
        "dark" => Theme {
            background: Color::Black,
            view: Color::Black,
            primary: Color::Gray,
            secondary: Color::Cyan,
            title: Color::Cyan,
            highlight: Color::Cyan,
            highlight_inactive: Color::Blue,
            highlight_text: Color::Black,
        },
        "mono" => Theme {
            background: Color::Black,
            view: Color::Black,
            primary: Color::Gray,
            secondary: Color::Gray,
            title: Color::Gray,
            highlight: Color::Gray,
            highlight_inactive: Color::Gray,
            highlight_text: Color::Black,
        },
        // Norton Commander: cyan on blue panels, black on cyan selection bar, yellow titles.
        "norton" => Theme {
            background: Color::Blue,
            view: Color::Blue,
            primary: Color::Cyan,
            secondary: Color::Gray,
            title: Color::Yellow,
            highlight: Color::Cyan,
            highlight_inactive: Color::Gray,
            highlight_text: Color::Black,
        },
        _ => theme(),
    }
}
