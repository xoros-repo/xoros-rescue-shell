//! Backup menu additions: repair / re-create a persistent file system (BAK-6), raw partition
//! image (BAK-7), and safe removal of a stick (USB-5, also in the Update menu).

use cursive::Cursive;

use crate::backend::PartRole;
use crate::partition_jobs::{self, RepairOutcome, IMAGE_ROLES, MKFS_CONFIRM_WORD};
use crate::tasks::TaskKind;
use crate::texts;
use crate::ui::dialogs;
use crate::ui::jobs::{self, JobEnd};
use crate::ui::pop_to_base;

const JOB_DIALOG: &str = "maint_job_progress";

/// BAK-6: choose overlay/application, then e2fsck -y + mount test (behind the PIN).
pub fn repair_start(s: &mut Cursive) {
    pop_to_base(s);
    crate::ui::pin::require(s, "repair file system", |s| {
        let items = [PartRole::Overlay, PartRole::Application]
            .iter()
            .map(|r| (r.label().to_string(), *r))
            .collect();
        dialogs::choose(
            s,
            "Repair file system",
            texts::MKFS_EXPLANATION,
            items,
            |s, role| run_repair(s, *role),
        );
    });
}

fn run_repair(s: &mut Cursive, role: PartRole) {
    jobs::run(
        s,
        "File system repair",
        TaskKind::Write,
        JOB_DIALOG,
        move |app, ctx| partition_jobs::repair(app.backend.as_ref(), role, ctx),
        move |s, end| match end {
            JobEnd::Ok(r) if r.mountable() => {
                s.add_layer(dialogs::text_screen("File system repair", &r.text()))
            }
            JobEnd::Ok(r) => offer_mkfs(s, r),
            JobEnd::Cancelled => dialogs::info(s, "File system repair", "Cancelled."),
            JobEnd::Failed(e) => dialogs::error(s, "File system repair failed.", &e),
        },
    );
}

fn offer_mkfs(s: &mut Cursive, r: RepairOutcome) {
    let role = r.role;
    let text = format!(
        "{}\nThis ERASES all data on {} ({}).",
        r.text(),
        role.label(),
        r.device
    );
    dialogs::confirm_typed(
        s,
        "Re-create file system",
        &text,
        MKFS_CONFIRM_WORD,
        move |s, typed| {
            jobs::run(
                s,
                "Re-create file system",
                TaskKind::Write,
                JOB_DIALOG,
                move |app, ctx| {
                    partition_jobs::recreate_fs(app.backend.as_ref(), role, &typed, ctx)
                },
                |s, end| crate::ui::tools::show_job_end(s, "Re-create file system", end),
            );
        },
    );
}

/// BAK-7: choose the partition and the stick.
pub fn image_start(s: &mut Cursive) {
    pop_to_base(s);
    let items = IMAGE_ROLES
        .iter()
        .map(|r| (r.label().to_string(), *r))
        .collect();
    dialogs::choose(
        s,
        "Raw partition image",
        texts::IMAGE_EXPLANATION,
        items,
        |s, role| {
            let role = *role;
            crate::ui::backup::choose_stick(s, "Raw partition image", move |s, stick| {
                jobs::run(
                    s,
                    "Raw partition image",
                    TaskKind::ReadOnly,
                    JOB_DIALOG,
                    move |app, ctx| {
                        partition_jobs::export_image(app.backend.as_ref(), role, &stick, ctx)
                            .map(|r| r.text(&stick))
                    },
                    |s, end| crate::ui::tools::show_job_end(s, "Raw partition image", end),
                );
            });
        },
    );
}

/// USB-5: unmount + sync, then tell the user the stick can be removed.
pub fn safe_remove_start(s: &mut Cursive) {
    pop_to_base(s);
    crate::ui::backup::choose_stick(s, "Safely remove stick", |s, stick| {
        let Some(app) = crate::ui::app(s) else { return };
        if let Some(t) = app.tasks.write_running() {
            dialogs::error(
                s,
                "Do not remove the stick now.",
                &format!(
                    "A task is running ({}). Wait for it or cancel it first.",
                    t.name()
                ),
            );
            return;
        }
        match crate::sticks::safe_remove(app.backend.as_ref(), &stick) {
            Ok(t) => dialogs::info(s, "Safely remove stick", &t),
            Err(e) => dialogs::error(s, "The stick cannot be removed yet.", &e),
        }
    });
}
