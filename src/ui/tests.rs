//! Tests menu (TST-*). M1: rescue image check as a background task with progress (UI-3).

use cursive::menu::Tree;
use cursive::Cursive;

use crate::texts;
use crate::ui::progress::show_task;
use crate::ui::{app, dialogs, refresh_status, todo_leaf};

pub fn menu() -> Tree {
    let t = Tree::new().leaf("Rescue integrity (SHA-256)", rescue_check);
    let t = todo_leaf(t, "Slot integrity", "The slot integrity test (TST-4)");
    let t = todo_leaf(t, "Memory test", "The memory test (TST-1)");
    let t = todo_leaf(t, "Disk read test", "The disk read test (TST-3)");
    todo_leaf(t, "File system check", "The file system check (TST-6)")
}

/// TST-5 (partial): hash the rescue image in the background and compare with the slots.
fn rescue_check(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let sink = s.cb_sink().clone();
    match app.start_rescue_check(Some(Box::new(move |_| {
        let _ = sink.send(Box::new(refresh_status));
    }))) {
        Ok(h) => show_task(s, texts::TASK_RESCUE_HASH, h),
        Err(e) => dialogs::error(s, "Cannot start the check.", &e),
    }
}
