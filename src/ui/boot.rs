//! Boot menu: counters, next boot, order, MAX_TRIES, rotation, grubenv (BOOT-1..7, ROT-1).

use cursive::menu::Tree;
use cursive::traits::*;
use cursive::views::{Dialog, LinearLayout, ScrollView, SelectView, TextView};
use cursive::Cursive;

use crate::backend::{BootTarget, ErrorKind, Result as BResult, Slot, SystemBackend};
use crate::boot as ctl;
use crate::constants::MAX_TRIES_RANGE;
use crate::power::PowerPlan;
use crate::texts;
use crate::ui::dialogs::{self, choose, closable, confirm, text_screen, MAX_W};
use crate::ui::{app, refresh_status, show_screen};

const BOOT_SUMMARY: &str = "boot_summary";

type Action = fn(&mut Cursive);

const ACTIONS: &[(&str, Action)] = &[
    ("Reset try counter...", reset_counter),
    ("Set next boot (one-shot)...", next_boot),
    ("Reset counter and reboot into slot...", reboot_into_slot),
    ("Slot order...", slot_order),
    ("MAX_TRIES...", max_tries),
    ("Disable slot...", disable_slot),
    ("Console rotation...", rotation),
    ("Raw grubenv view", raw_view),
    ("Recreate grubenv...", recreate_grubenv),
];

pub fn menu() -> Tree {
    let mut t = Tree::new().leaf("Boot control", show_boot).delimiter();
    for (label, action) in ACTIONS {
        t.add_leaf(*label, *action);
    }
    t
}

fn summary_text(b: &dyn SystemBackend) -> String {
    let st = match b.boot_state() {
        Ok(st) => st,
        Err(e) if e.kind == ErrorKind::GrubenvBroken => {
            return format!("{e}\nUse 'Recreate grubenv' to fix it.");
        }
        Err(e) => return format!("Boot state not available:\n{e}"),
    };
    let tries = |slot: Slot| {
        let t = st.try_count(slot);
        format!(
            "{}: {t}/{}{}",
            slot.letter(),
            st.max_tries,
            if t >= st.max_tries { " (exceeded)" } else { "" }
        )
    };
    let next = st.next_boot.map(|t| t.menu_id()).unwrap_or("(none)");
    let last = st.last_boot.map(|t| t.name()).unwrap_or("-");
    let predicted = b
        .predict()
        .map(|t| t.name().to_string())
        .unwrap_or_else(|_| "unknown".into());
    format!(
        "Try counters   {}   {}   MAX_TRIES: {}\n\
         ORDER: {}, {}   NEXT_BOOT: {next}   LAST_BOOT: {last}\n\
         ROTATION: {} ({} deg)   Predicted next boot: {predicted}\n\n\
         grubenv:\n{}",
        tries(Slot::A),
        tries(Slot::B),
        st.max_tries,
        st.order[0].letter(),
        st.order[1].letter(),
        st.rotation,
        u32::from(st.rotation) * 90,
        crate::report::boot_vars_text(&st)
    )
}

/// The Boot control screen (BOOT-1).
pub fn show_boot(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let summary = summary_text(app.backend.as_ref());
    let mut select = SelectView::<Action>::new();
    for (label, action) in ACTIONS {
        select.add_item(*label, *action);
    }
    let select = select.on_submit(|s, a: &Action| a(s));
    let layout = LinearLayout::vertical()
        .child(
            ScrollView::new(TextView::new(summary).with_name(BOOT_SUMMARY))
                .max_height(8)
                .max_width(MAX_W),
        )
        .child(TextView::new(" "))
        .child(ScrollView::new(select).max_height(7));
    show_screen(
        s,
        closable(
            Dialog::around(layout)
                .title(texts::TITLE_BOOT)
                .dismiss_button(texts::BTN_CLOSE),
        ),
    );
}

fn refresh_boot(s: &mut Cursive) {
    if let Some(app) = app(s) {
        let text = summary_text(app.backend.as_ref());
        s.call_on_name(BOOT_SUMMARY, |v: &mut TextView| v.set_content(text));
    }
    refresh_status(s);
}

/// Run a boot-control action and report the result (UI-5). Offers to recreate grubenv
/// when it is broken (ENV-2).
fn apply<F>(s: &mut Cursive, what: &str, f: F)
where
    F: FnOnce(&dyn SystemBackend) -> BResult<String>,
{
    let Some(app) = app(s) else { return };
    let r = f(app.backend.as_ref());
    refresh_boot(s);
    match r {
        Ok(msg) => {
            if !msg.is_empty() {
                dialogs::info(s, what, &msg);
            }
        }
        Err(e) if e.kind == ErrorKind::GrubenvBroken => offer_recreate(s),
        Err(e) => dialogs::error(s, &format!("{what} failed."), &e),
    }
}

fn slot_items() -> Vec<(String, Slot)> {
    Slot::ALL
        .iter()
        .map(|s| (format!("Slot {} ({})", s.letter(), s.name()), *s))
        .collect()
}

fn reset_counter(s: &mut Cursive) {
    let mut items: Vec<(String, Vec<Slot>)> = Slot::ALL
        .iter()
        .map(|s| (format!("Slot {}", s.letter()), vec![*s]))
        .collect();
    items.push(("Both slots".into(), vec![Slot::A, Slot::B]));
    choose(
        s,
        "Reset try counter",
        texts::RESET_EXPLANATION,
        items,
        |s, slots| {
            let slots = slots.clone();
            apply(s, "Reset try counter", move |b| {
                ctl::reset_tries(b, &slots)?;
                let names: Vec<&str> = slots.iter().map(|s| s.letter()).collect();
                Ok(format!(
                    "Try counter of slot {} reset to 0. GRUB will try it again on the next boot.",
                    names.join(" and ")
                ))
            });
        },
    );
}

/// Confirm booting into a slot that may be invalid (BOOT-6), then continue.
fn with_slot_warning<F>(s: &mut Cursive, slot: Slot, then: F)
where
    F: Fn(&mut Cursive) + Send + Sync + 'static,
{
    let Some(app) = app(s) else { return };
    match ctl::slot_boot_warning(app.backend.as_ref(), slot) {
        None => then(s),
        Some(w) => confirm(
            s,
            "Warning",
            &format!("{w}\n\nContinue anyway?"),
            "Continue",
            then,
        ),
    }
}

fn next_boot(s: &mut Cursive) {
    let items = vec![
        ("Slot A (once)".to_string(), Some(BootTarget::Slot(Slot::A))),
        ("Slot B (once)".to_string(), Some(BootTarget::Slot(Slot::B))),
        ("Rescue (once)".to_string(), Some(BootTarget::Rescue)),
        ("Clear NEXT_BOOT".to_string(), None),
    ];
    choose(
        s,
        "Set next boot",
        texts::NEXT_BOOT_EXPLANATION,
        items,
        |s, target| {
            let target = *target;
            let set = move |s: &mut Cursive| {
                apply(s, "Set next boot", move |b| {
                    ctl::set_next_boot(b, target)?;
                    Ok(match target {
                        Some(t) => format!(
                            "NEXT_BOOT={}. Takes effect on the next reboot.",
                            t.menu_id()
                        ),
                        None => "NEXT_BOOT cleared.".to_string(),
                    })
                })
            };
            match target {
                Some(BootTarget::Slot(slot)) => with_slot_warning(s, slot, set),
                _ => set(s),
            }
        },
    );
}

fn reboot_into_slot(s: &mut Cursive) {
    choose(
        s,
        "Reboot into slot",
        texts::REBOOT_INTO_EXPLANATION,
        slot_items(),
        |s, slot| {
            let slot = *slot;
            with_slot_warning(s, slot, move |s| {
                confirm(
                    s,
                    texts::TITLE_CONFIRM,
                    &format!(
                        "Reset {}_TRY to 0, set NEXT_BOOT={} and reboot now?",
                        slot.name(),
                        slot.menu_id()
                    ),
                    "Reboot",
                    move |s| {
                        crate::ui::power::run_plan(
                            s,
                            PowerPlan::reboot_into(BootTarget::Slot(slot), true),
                        )
                    },
                );
            });
        },
    );
}

fn slot_order(s: &mut Cursive) {
    let items = vec![
        ("A first (ROOTFS_A ROOTFS_B)".to_string(), Slot::A),
        ("B first (ROOTFS_B ROOTFS_A)".to_string(), Slot::B),
    ];
    choose(
        s,
        "Slot order",
        "GRUB tries the slots in this order.",
        items,
        |s, first| {
            let first = *first;
            apply(s, "Set slot order", move |b| {
                ctl::set_order(b, first)?;
                Ok(String::new())
            });
        },
    );
}

fn max_tries(s: &mut Cursive) {
    let items = MAX_TRIES_RANGE.map(|n| (n.to_string(), n)).collect();
    choose(
        s,
        "MAX_TRIES",
        "Boot attempts before GRUB skips a slot (default 3).",
        items,
        |s, n| {
            let n = *n;
            apply(s, "Set MAX_TRIES", move |b| {
                ctl::set_max_tries(b, n)?;
                Ok(String::new())
            });
        },
    );
}

fn disable_slot(s: &mut Cursive) {
    choose(
        s,
        "Disable slot",
        "Sets the slot's try counter to MAX_TRIES so GRUB skips it.",
        slot_items(),
        |s, slot| {
            let slot = *slot;
            confirm(
                s,
                texts::TITLE_CONFIRM,
                &format!("Disable slot {} ({})?", slot.letter(), slot.name()),
                "Disable",
                move |s| {
                    apply(s, "Disable slot", move |b| {
                        ctl::disable(b, slot)?;
                        Ok(format!("Slot {} disabled.", slot.letter()))
                    })
                },
            );
        },
    );
}

fn rotation(s: &mut Cursive) {
    let items = (0u8..=3)
        .map(|r| (format!("{r}: {} degrees", u32::from(r) * 90), r))
        .collect();
    choose(
        s,
        "Console rotation",
        texts::ROTATION_EXPLANATION,
        items,
        |s, r| {
            let r = *r;
            apply(s, "Set rotation", move |b| ctl::set_rotation(b, r));
            // Redraw everything in case the console geometry changed (RT-4).
            s.clear();
        },
    );
}

fn raw_view(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    match app.backend.boot_state() {
        Ok(st) => s.add_layer(text_screen(
            "grubenv (raw)",
            &crate::report::boot_raw_text(&st),
        )),
        Err(e) if e.kind == ErrorKind::GrubenvBroken => offer_recreate(s),
        Err(e) => dialogs::error(s, "Cannot read grubenv.", &e),
    }
}

fn recreate_grubenv(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    match app.backend.boot_state() {
        Err(e) if e.kind == ErrorKind::GrubenvBroken => offer_recreate(s),
        Ok(_) => dialogs::info(s, "Recreate grubenv", "grubenv is valid; nothing to do."),
        Err(e) => dialogs::error(s, "Cannot read grubenv.", &e),
    }
}

fn offer_recreate(s: &mut Cursive) {
    confirm(
        s,
        "Recreate grubenv",
        texts::GRUBENV_BROKEN,
        "Recreate",
        |s| {
            apply(s, "Recreate grubenv", |b| {
                ctl::recreate_grubenv(b)?;
                Ok("grubenv recreated with default values.".into())
            })
        },
    );
}
