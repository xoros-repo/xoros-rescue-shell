use cursive::{Cursive, event::Key, View, views, views::Dialog};
use cursive::traits::*;
use cursive::views::{Layer, ScrollView, TextView};
use xoros_rescue_shell::sys::actions::{beep, quit_handler};

use xoros_rescue_shell::ui::menus::configs::config_menu;
use xoros_rescue_shell::ui::menus::help::help_menu;
use xoros_rescue_shell::ui::menus::recovery::recovery_menu;
use xoros_rescue_shell::ui::menus::tests::tests_menu;
use xoros_rescue_shell::ui::status_bar::StatusBarExt;

mod theme;

fn main() {
    beep::simple();

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
        .add_leaf("Reboot", |s| quit_handler());

    // Show application wide log window at the bottom of the screen:
    let log_view = Layer::new(ScrollView::new(TextView::new("")))
        .full_width()
        .fixed_height(5)
        .with_name("log_view")
        .full_screen();

    app.add_global_callback(Key::Esc, |s| s.select_menubar());

    app.add_layer(Dialog::text("Press ESC for menu..."));
    app.status_bar(format!("Xoros Rescue Shell v{} - [Ctrl-C: Exit]", VERSION));
    app.set_fps(15);
    app.try_run_with(backend_init).ok();
}
