use cursive::menu;
use cursive::menu::Tree;
use cursive::views::Dialog;

pub fn recovery_menu() -> Tree {
    let tree = menu::Tree::new()
        .leaf("Flash ROOT partitions", move |s| {
            s.add_layer(Dialog::info("Config was backed up!"));
        })
        .leaf("Verify ROOT partitions", move |s| {
            s.add_layer(Dialog::info("Config was restored!"));
        })
        .delimiter()
        .leaf("Complete Reset", move |s| {
            s.add_layer(Dialog::info("Config was reset!"));
        });
    return tree;
}
