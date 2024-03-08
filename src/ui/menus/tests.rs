use std::sync::atomic::{AtomicUsize, Ordering};

use cursive::{menu, With};
use cursive::menu::Tree;
use cursive::views::Dialog;

pub fn tests_menu() -> Tree {
    let counter = AtomicUsize::new(1);

    let tree = menu::Tree::new()
        .leaf("New", move |s| {
            let i = counter.fetch_add(1, Ordering::Relaxed);
            let filename = format!("New {i}");
            s.menubar()
                .find_subtree("File")
                .unwrap()
                .find_subtree("Recent")
                .unwrap()
                .insert_leaf(0, filename, |_| ());

            s.add_layer(Dialog::info("New file!"));
        })
        // ... and of subtrees, which open up when selected.
        .subtree(
            "Recent",
            // The `.with()` method can help when running loops
            // within builder patterns.
            menu::Tree::new().with(|tree| {
                for i in 1..100 {
                    // We don't actually do anything here,
                    // but you could!
                    tree.add_item(menu::Item::leaf(format!("Item {i}"), |_| ()).with(|s| {
                        if i % 5 == 0 { s.disable(); }
                    }))
                }
            }),
        )
        // Delimiter are simple lines between items,
        // and cannot be selected.
        .delimiter()
        .with(|tree| {
            for i in 1..10 {
                tree.add_leaf(format!("Option {i}"), |_| ());
            }
        });
    return tree;
}
