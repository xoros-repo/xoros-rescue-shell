//! Status menu: overview (SYS-1), hardware (SYS-2), storage (SYS-3).

use cursive::menu::Tree;
use cursive::Cursive;

use crate::texts;
use crate::ui::dialogs::text_screen;
use crate::ui::{app, pop_to_base, refresh_status, show_screen, todo_leaf};

pub fn menu() -> Tree {
    let t = Tree::new()
        .leaf("Overview", |s| {
            pop_to_base(s);
            refresh_status(s);
        })
        .leaf("Hardware", show_hardware)
        .leaf("Storage", show_storage);
    let t = todo_leaf(t, "Network details", "Network details (SYS-4)");
    todo_leaf(t, "Save system report", "Saving the system report (SYS-5)")
}

pub fn show_hardware(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let hw = crate::hw::hardware_info(&app.backend.host_paths());
    let text = crate::report::hardware_text(&hw);
    show_screen(s, text_screen(texts::TITLE_HARDWARE, &text));
}

pub fn show_storage(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let text = crate::report::storage_text(&app.backend.block_devices(), &app.backend.layout());
    show_screen(s, text_screen(texts::TITLE_STORAGE, &text));
}
