use cursive::{CursiveExt, menu};
use cursive::menu::Tree;
use cursive::views::Dialog;
use subprocess::Exec;

pub fn recovery_menu() -> Tree {
    let tree = menu::Tree::new()
        .leaf("Flash ROOT partitions", move |s| {
            s.add_layer(Dialog::info("Config was backed up!"));
        })
        .leaf("Verify ROOT partitions", move |s| {
            s.add_layer(Dialog::info("Config was restored!"));
        })
        .leaf("Subshell", move |s| {
            let state = s.dump();

            let exit_status = Exec::shell("/bin/sh")
                .detached()  // This detaches the shell from the parent process
                .join()      // Wait for the shell to finish
                .expect("Failed to launch shell");

            s.restore(state);

            s.add_layer(Dialog::info(format!("Shell exited with status: {:?}", exit_status)));
        })
        .delimiter()
        .leaf("Complete Reset", move |s| {
            s.add_layer(Dialog::info("Config was reset!"));
        });
    return tree;
}
