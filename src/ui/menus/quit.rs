use cursive::menu;
use cursive::menu::Tree;
use cursive::views::{Dialog, TextView};
use crate::sys::info::disks_info;

pub fn help_menu(v: &str) -> Tree {
    let version = v.to_string();

    let menu_tree = menu::Tree::new()
        .leaf("Quit", move |s| {
            s.add_layer(Dialog::text(format!("Xoros Rescue Shell v{}", version))
                .title("Quit")
                .button("Quit", |s| std::process::exit(0))
                .button("Reboot", |s| std::process::exit(0))
                .button("Cancel", |s| s.quit()))
        });


    return menu_tree;
}
