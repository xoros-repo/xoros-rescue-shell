use std::{env, io};
use std::io::Write;
use std::os::fd::AsRawFd;
use cursive::{CursiveExt, menu};
use cursive::menu::Tree;
use cursive::views::Dialog;
use subprocess::Exec;
use cursive::traits::*;
use termios::{TCSANOW, tcsetattr, Termios};

pub fn recovery_menu() -> Tree {
    let tree = menu::Tree::new()
        .leaf("Flash ROOT partitions", move |s| {
            s.add_layer(Dialog::info("Config was backed up!"));
        })
        .leaf("Verify ROOT partitions", move |s| {
            s.add_layer(Dialog::info("Config was restored!"));
        })
        .leaf("Subshell", move |s| {
            // Save current terminal state to avoid messing up the terminal after shell -> reset
            let stdin_fd = io::stdin().as_raw_fd();
            let original_termios = Termios::from_fd(stdin_fd).unwrap();
            let state = s.dump();

            let shell_exec = env::var("SHELL").unwrap_or("/bin/sh".to_string());

            // tcsetattr(stdin_fd, TCSANOW, &original_termios).expect("TODO: panic message");
            io::stdout().flush().expect("TODO: panic message");

            let shell_exit_status = Exec::shell(format!("{0} -c 'export PS1=\"rescue-shell# \"; reset; {0}'", "/bin/sh"))
                .detached()  // This detaches the shell from the parent process
                .join()      // Wait for the shell to finish
                .expect("Failed to launch shell");

            tcsetattr(stdin_fd, TCSANOW, &original_termios).expect("TODO: panic message");
            s.restore(state);
            s.clear();
        })
        .delimiter()
        .leaf("Complete Reset", move |s| {
            s.add_layer(Dialog::info("Config was reset!"));
        });
    return tree;
}
