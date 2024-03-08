use cursive::{Cursive, event::Key, traits::*, views::Dialog};

use xoros_rescue_shell::ui::menus::configs::config_menu;
use xoros_rescue_shell::ui::menus::help::help_menu;
use xoros_rescue_shell::ui::menus::recovery::recovery_menu;
use xoros_rescue_shell::ui::menus::tests::tests_menu;
use xoros_rescue_shell::ui::status_bar::StatusBarExt;

mod theme;

fn main() {
    let backend_init = || -> std::io::Result<Box<dyn cursive::backend::Backend>> {
        let backend = cursive::backends::crossterm::Backend::init()?;
        let buffered_backend = cursive_buffered_backend::BufferedBackend::new(backend);
        Ok(Box::new(buffered_backend))
    };

    const VERSION: &str = env!("CARGO_PKG_VERSION");

    let mut app = Cursive::new();
    let theme = theme::theme();
    app.set_theme(theme);

    app.menubar()
        .add_subtree(
            "Configs",
            config_menu(),
        )
        .add_subtree(
            "Tests",
            tests_menu(),
        )
        .add_subtree(
            "Recovery",
            recovery_menu(),
        )
        .add_subtree(
            "Help",
            help_menu(VERSION),
        )
        .add_delimiter()
        .add_leaf("Quit", |s| s.quit());

    // When `autohide` is on (default), the menu only appears when active.
    // Turning it off will leave the menu always visible.
    // Try uncommenting this line!

//    siv.set_autohide_menu(false);

    app.add_global_callback(Key::Esc, |s| s.select_menubar());

    app.add_layer(Dialog::text("Press ESC to continue..."));
    app.status_bar(format!("Xoros Rescue Shell v{}", VERSION));
    app.set_fps(30);
    app.try_run_with(backend_init).ok();
}
