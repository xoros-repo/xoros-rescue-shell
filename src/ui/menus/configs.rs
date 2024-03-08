use cursive::menu;
use cursive::menu::Tree;
use cursive::views::Dialog;

pub fn config_menu() -> Tree {
    let tree = menu::Tree::new()
        .leaf("Backup", move |s| {
            s.add_layer(Dialog::info("Config was backed up!"));
        })
        .leaf("Restore", move |s| {
            s.add_layer(Dialog::info("Config was restored!"));
        })
        .delimiter()
        .leaf("Reset", move |s| {
            s.add_layer(Dialog::info("Config was reset!"));
        });
    return tree;
}
