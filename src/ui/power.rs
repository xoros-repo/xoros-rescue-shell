//! Power menu (PWR-1, PWR-2).

use crate::ui::tk::menu::Tree;
use crate::ui::tk::views::Dialog;
use crate::ui::tk::Ui;

use crate::backend::{BootTarget, Slot};
use crate::power::{execute, PowerError, PowerOutcome, PowerPlan};
use crate::texts;
use crate::ui::{app, dialogs};

pub fn menu() -> Tree {
    Tree::new()
        .leaf("Reboot", |s| ask(s, PowerPlan::reboot()))
        .leaf("Reboot into slot A", |s| ask_slot(s, Slot::A))
        .leaf("Reboot into slot B", |s| ask_slot(s, Slot::B))
        .leaf("Reboot into rescue", |s| {
            ask(s, PowerPlan::reboot_into(BootTarget::Rescue, false))
        })
        .delimiter()
        .leaf("Power off", |s| ask(s, PowerPlan::power_off()))
        .delimiter()
        .leaf("Restart rescue shell", |s| {
            dialogs::confirm(
                s,
                texts::TITLE_CONFIRM,
                texts::RESTART_APP,
                texts::BTN_YES,
                |s| {
                    log::info!("user requested restart of the rescue shell");
                    s.quit();
                },
            )
        })
}

fn ask(s: &mut Ui, plan: PowerPlan) {
    let text = format!("{}?", capitalize(&plan.describe()));
    dialogs::confirm(s, texts::TITLE_CONFIRM, &text, texts::BTN_YES, move |s| {
        run_plan(s, plan)
    });
}

fn capitalize(t: &str) -> String {
    let mut c = t.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Reboot into a slot: warn if invalid (BOOT-6), offer resetting its counter (PWR-1).
fn ask_slot(s: &mut Ui, slot: Slot) {
    let Some(app) = app(s) else { return };
    let warning = crate::boot::slot_boot_warning(app.backend.as_ref(), slot)
        .map(|w| format!("WARNING: {w}\n\n"))
        .unwrap_or_default();
    let text = format!(
        "{warning}Reboot into slot {} ({}) once via NEXT_BOOT.\n\n\
         Also reset its try counter? Without a reset, GRUB skips the slot again \
         on later boots if the counter is at MAX_TRIES.",
        slot.letter(),
        slot.name()
    );
    let target = BootTarget::Slot(slot);
    s.add_layer(dialogs::closable(
        Dialog::around(dialogs::scroll_text(text))
            .title(texts::TITLE_CONFIRM)
            .button("Reset + reboot", move |s| {
                s.pop_layer();
                run_plan(s, PowerPlan::reboot_into(target, true));
            })
            .button("Reboot", move |s| {
                s.pop_layer();
                run_plan(s, PowerPlan::reboot_into(target, false));
            })
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

/// Execute a power plan and show the outcome.
pub fn run_plan(s: &mut Ui, plan: PowerPlan) {
    let Some(app) = app(s) else { return };
    match execute(plan, app.backend.as_ref(), &app.tasks) {
        Ok(PowerOutcome::Simulated(msg)) => {
            crate::ui::refresh_status(s);
            dialogs::info(s, "Simulation", &format!("{msg}\n\nNothing was executed."));
        }
        Ok(PowerOutcome::Started) => {
            s.add_layer(Dialog::text(format!("{}...", capitalize(&plan.describe()))));
        }
        Err(e @ PowerError::WriteRunning(_)) => {
            dialogs::error(s, "Cannot reboot or power off now.", &e);
        }
        Err(e) => {
            crate::ui::refresh_status(s);
            dialogs::error(s, &format!("{} failed.", capitalize(&plan.describe())), &e);
        }
    }
}
