use cursive::menu;
use cursive::menu::Tree;
use cursive::views::{Dialog, ScrollView, TextView};
use crate::sys::info::disks_info;

pub fn help_menu(v: &str) -> Tree {
    let version = v.to_string();

    let tree = menu::Tree::new()
        .leaf("About", move |s| {
            s.add_layer(Dialog::text(format!("Xoros Rescue Shell v{}", version))
                .title("About")
                .button("Close", |s| s.quit())
                .button("System Info", |s| {

                    let sys_info = "System info placeholder: CPU, RAM, etc.";
                    s.add_layer(Dialog::around(ScrollView::new(TextView::new(disks_info()))).button("Ok", |s| {
                        s.pop_layer();
                    }));
                }))
        }).subtree(
        "Help",
        menu::Tree::new()
            .leaf("General", |s| {
                s.add_layer(Dialog::info("Help message!"))
            })
            .leaf("Online", |s| {
                s.add_layer(Dialog::info("Google it yourself!\n\
                                        Kids, these days..."))
            }),
    );
    return tree;
}
