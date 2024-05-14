use std::{env, io};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::thread::sleep;
use cursive::{Cursive, CursiveExt, menu};
use cursive::event::Key;
use cursive::menu::Tree;
use cursive::views::{Dialog, ProgressBar};
use subprocess::Exec;
use cursive::traits::*;
use termios::{TCSANOW, tcsetattr, Termios};

fn partition_flash_confirm_msg(partition: &str) -> String {
    return format!("{} partition will be overwritten by OTA flash.\nPlease confirm.", partition);
}

pub fn recovery_menu() -> Tree {
    let tree = menu::Tree::new()
        .subtree(
            "Force OTA flash",
            menu::Tree::new()
                .leaf("ROOTFS_A", |s| {
                    s.add_layer(Dialog::text(partition_flash_confirm_msg("ROOTFS_A"))
                        .button("Ok", |s| {
                            s.pop_layer();

                            s.add_global_callback(Key::Esc, |s| {

                            });

                            s.add_layer(Dialog::around(ProgressBar::new()
                                .with_label(| value, (min, max)| -> String {
                                    format!("Progress: {} / {}", value, max)
                                })
                                .with_task(|counter| {
                                    // This closure is called in parallel.
                                    for _ in 0..100 {
                                        sleep(std::time::Duration::from_millis(100));
                                        counter.tick(1);
                                    }
                                }).full_width()
                            ));

                            s.add_global_callback(Key::Esc, |s| {
                                 s.select_menubar();
                            });
                        })
                        .button("Cancel", |s| {
                            s.pop_layer();
                        })
                    )
                })
                .leaf("ROOTFS_B", |s| {
                    s.add_layer(Dialog::info("ROOTFS_B").button("Cancel", |s| {
                        s.pop_layer();
                    }))
                })
                .leaf("RESCUE", |s| {
                    s.add_layer(Dialog::info("RESCUE").button("Cancel", |s| {
                        s.pop_layer();
                    }))
                }),
        )
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
