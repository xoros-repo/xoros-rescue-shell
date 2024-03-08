use cursive::menu;
use cursive::menu::Tree;
use cursive::views::Dialog;

pub fn help_menu(v: &str) -> Tree {
    let version = v.to_string();

    let tree = menu::Tree::new()
        .leaf("About", move |s| {
            s.add_layer(Dialog::info(format!("Xoros Rescue Shell v{}", version)))
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
