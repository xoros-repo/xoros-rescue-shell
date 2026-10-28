//! Update menu: from the network (OTA-1..11) and from a USB stick (USB-2..5).
//!
//! The screens collect choices and show results; checking and writing run in
//! [`crate::update_jobs`] inside background tasks (UI-3).

use std::str::FromStr;

use cursive::menu::Tree;
use cursive::traits::*;
use cursive::views::{Dialog, EditView, LinearLayout, ScrollView, SelectView, TextView};
use cursive::Cursive;
use xoros_core::env::Suffix;
use xoros_core::flash::FlashOptions;
use xoros_core::manifest::Target;

use crate::backend::Slot;
use crate::clock::format_build_timestamp;
use crate::constants::WRITE_BOTH_CONFIRM_WORD;
use crate::tasks::{lock, TaskKind};
use crate::texts;
use crate::ui::dialogs::{self, closable, MAX_W};
use crate::ui::jobs::{self, JobEnd};
use crate::ui::{app, pop_to_base};
use crate::update_jobs::{
    self, device_target, disabled_note, update_summary, CheckResult, ImageRow, Origin, Targets,
    UpdateRequest,
};

const CHECK_DIALOG: &str = "update_check_progress";
const WRITE_DIALOG: &str = "update_write_progress";
const F_BASE: &str = "ota_base_url";
const F_VERSION: &str = "ota_version";
const F_MACHINE: &str = "ota_machine";
const F_SUFFIX: &str = "ota_suffix";
const F_PROXY: &str = "ota_proxy";

pub fn menu() -> Tree {
    Tree::new()
        .leaf("From network (OTA)", ota_start)
        .leaf("From USB stick", usb_start)
}

fn field(label: &str, name: &str, value: &str) -> LinearLayout {
    LinearLayout::horizontal()
        .child(TextView::new(format!("{label:<10}")))
        .child(
            EditView::new()
                .content(value)
                .with_name(name)
                .fixed_width(52),
        )
}

fn read_field(s: &mut Cursive, name: &str) -> String {
    s.call_on_name(name, |v: &mut EditView| v.get_content().trim().to_string())
        .unwrap_or_default()
}

// --------------------------------------------------------------------------------------------
// OTA (OTA-1, OTA-10, OTA-11)

fn ota_start(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    pop_to_base(s);
    let dev = device_target(app.backend.as_ref());
    let intro = match (&dev.target, dev.source_slot) {
        (Some(_), Some(slot)) => format!(
            "Version, machine and suffix are taken from the newest valid slot ({}).",
            slot.name()
        ),
        _ => "No valid slot: enter version and suffix (machine from the rescue host name)."
            .to_string(),
    };
    let t = dev.target.clone();
    let cfg = crate::settings::current();
    let layout = LinearLayout::vertical()
        .child(TextView::new(intro).max_width(MAX_W))
        .child(TextView::new(" "))
        .child(field("Server", F_BASE, &cfg.ota_base_url))
        .child(field(
            "Version",
            F_VERSION,
            t.as_ref().map(|t| t.version.as_str()).unwrap_or(""),
        ))
        .child(field(
            "Machine",
            F_MACHINE,
            dev.machine.as_deref().unwrap_or(""),
        ))
        .child(field(
            "Suffix",
            F_SUFFIX,
            t.as_ref().map(|t| t.suffix.as_str()).unwrap_or("stable"),
        ))
        .child(field("Proxy", F_PROXY, &cfg.http_proxy))
        .child(TextView::new(
            "Suffix: stable, devel, audit or nightly. Proxy: http://host:port or empty.",
        ));
    s.add_layer(closable(
        Dialog::around(layout)
            .title("Update from the network")
            .button("Check", ota_check_clicked)
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

fn ota_check_clicked(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let base = read_field(s, F_BASE);
    let version = read_field(s, F_VERSION);
    let machine = read_field(s, F_MACHINE);
    let suffix = read_field(s, F_SUFFIX);
    let proxy = Some(read_field(s, F_PROXY)).filter(|p| !p.is_empty());
    if let Err(e) = crate::net::http::check_base_url(&base) {
        dialogs::error(s, "Invalid server URL.", &e);
        return;
    }
    let suffix = match Suffix::from_str(&suffix) {
        Ok(v) => v,
        Err(e) => {
            dialogs::error(s, "Invalid suffix.", &e);
            return;
        }
    };
    if version.is_empty() || machine.is_empty() {
        dialogs::info(s, "Update", "Enter version and machine.");
        return;
    }
    let origin = Origin::Network {
        base_url: base,
        target: Target::new(version, machine, suffix),
        proxy,
    };
    let current = device_target(app.backend.as_ref()).target;
    match current.filter(|t| t.suffix != suffix) {
        Some(t) => dialogs::confirm(
            s,
            texts::TITLE_CONFIRM,
            &format!(
                "The installed build is '{}'. Switch to '{suffix}' builds?",
                t.suffix
            ),
            "Switch",
            move |s| {
                s.pop_layer();
                start_check(s, origin.clone());
            },
        ),
        None => {
            s.pop_layer();
            start_check(s, origin);
        }
    }
}

// --------------------------------------------------------------------------------------------
// USB (USB-2)

fn usb_start(s: &mut Cursive) {
    pop_to_base(s);
    crate::ui::backup::choose_stick(s, "Update from a stick", |s, stick| {
        let Some(app) = app(s) else { return };
        let items = match update_jobs::scan_stick(app.backend.as_ref(), &stick) {
            Ok(v) => v,
            Err(e) => {
                dialogs::error(s, "Cannot read the stick.", &e);
                return;
            }
        };
        if items.is_empty() {
            dialogs::info(
                s,
                "Update from a stick",
                "No update found. Expected an OTA tree (<version>/<machine>/<suffix>/manifest.json \
                 in the root, xoros/, ota/ or xoros/ota/) or a *.squashfs image with a .sha256 file.",
            );
            return;
        }
        let list = items
            .iter()
            .map(|i| (crate::report::ellipsize(&i.label(), MAX_W - 4), i.origin(&stick)))
            .collect();
        dialogs::choose(s, "Update from a stick", "Choose the update:", list, |s, o| {
            start_check(s, o.clone())
        });
    });
}

// --------------------------------------------------------------------------------------------
// check -> choose image -> choose target -> confirm -> write

fn start_check(s: &mut Cursive, origin: Origin) {
    let Some(app) = app(s) else { return };
    let rescue = lock(&app.rescue_check).sha256().map(str::to_string);
    let o = origin.clone();
    jobs::run(
        s,
        "Update check",
        TaskKind::ReadOnly,
        CHECK_DIALOG,
        move |app, ctx| update_jobs::check(app.backend.as_ref(), &o, rescue.as_deref(), ctx),
        move |s, end| match end {
            JobEnd::Ok(c) => show_check(s, c),
            JobEnd::Cancelled => dialogs::info(s, "Update check", "Cancelled."),
            JobEnd::Failed(e) => s.add_layer(dialogs::text_screen("Update check failed", &e)),
        },
    );
}

fn show_check(s: &mut Cursive, c: CheckResult) {
    let mut select = SelectView::<ImageRow>::new();
    for r in &c.rootfs {
        select.add_item(r.label(), r.clone());
    }
    let c2 = c.clone();
    let select = select.on_submit(move |s, row: &ImageRow| {
        s.pop_layer();
        choose_targets(s, c2.clone(), row.clone());
    });
    let mut layout = LinearLayout::vertical().child(
        ScrollView::new(TextView::new(c.text()))
            .max_width(MAX_W)
            .max_height(10),
    );
    if c.rootfs.is_empty() {
        layout.add_child(TextView::new("Nothing to install."));
    } else {
        layout.add_child(TextView::new("Choose the rootfs image (newest first):"));
        layout.add_child(ScrollView::new(select).max_height(5));
    }
    s.add_layer(closable(
        Dialog::around(layout)
            .title("Available updates")
            .dismiss_button(texts::BTN_CLOSE),
    ));
}

fn slot_text(app: &crate::app::App, slot: Slot) -> String {
    let device = app
        .backend
        .layout()
        .ok()
        .and_then(|l| {
            l.get(crate::backend::PartRole::slot(slot))
                .map(|e| e.device.clone())
        })
        .unwrap_or_else(|| "?".into());
    let content = match app.backend.slot_info(slot) {
        Ok(Some(i)) => format!(
            "{} {} {}",
            i.version,
            i.suffix,
            format_build_timestamp(&i.timestamp)
        ),
        Ok(None) => "empty/invalid".into(),
        Err(e) => format!("unreadable ({e})"),
    };
    format!("{} ({device}, {}): {content}", slot.name(), slot.label())
}

fn choose_targets(s: &mut Cursive, c: CheckResult, row: ImageRow) {
    let Some(app) = app(s) else { return };
    let text = format!(
        "Install {} ({}).\n\n{}\n{}",
        format_build_timestamp(row.image.build_timestamp.as_str()),
        crate::tasks::format_bytes(row.image.size),
        slot_text(&app, Slot::A),
        slot_text(&app, Slot::B)
    );
    let items = Targets::ALL
        .iter()
        .map(|t| (t.label().to_string(), *t))
        .collect();
    dialogs::choose(s, "Write to", &text, items, move |s, t| {
        confirm_write(s, c.clone(), row.clone(), *t)
    });
}

fn confirm_write(s: &mut Cursive, c: CheckResult, row: ImageRow, targets: Targets) {
    let Some(app) = app(s) else { return };
    let mut text = String::from("This overwrites:\n");
    for slot in targets.slots() {
        text.push_str(&format!("  {}\n", slot_text(&app, slot)));
    }
    text.push_str(&format!(
        "\nwith {} {} {} {} from\n{}\n\nThe slot is disabled while it is written.",
        c.manifest.version,
        c.manifest.machine,
        c.manifest.suffix,
        format_build_timestamp(row.image.build_timestamp.as_str()),
        c.source
    ));
    let mut warnings = Vec::new();
    if row.is_downgrade(targets) {
        warnings.push("This is the SAME or an OLDER build than installed (downgrade).");
    }
    if c.unverified {
        warnings.push("The image has no .sha256 file: it is UNVERIFIED.");
    }
    if !warnings.is_empty() {
        text = format!("WARNING: {}\n\n{text}", warnings.join("\nWARNING: "));
    }
    let req = UpdateRequest {
        origin: c.origin.clone(),
        image: row.image.clone(),
        targets,
        options: FlashOptions::default(),
    };
    if targets == Targets::Both {
        dialogs::confirm_typed(
            s,
            "Write both slots",
            &text,
            WRITE_BOTH_CONFIRM_WORD,
            move |s, _| start_write(s, req.clone()),
        );
    } else {
        dialogs::confirm(s, texts::TITLE_CONFIRM, &text, "Write", move |s| {
            start_write(s, req.clone())
        });
    }
}

fn start_write(s: &mut Cursive, req: UpdateRequest) {
    let Some(app) = app(s) else { return };
    if let Some(t) = app.tasks.write_running() {
        dialogs::error(
            s,
            "Cannot start now.",
            &crate::tasks::SpawnError::WriteBusy(t.name().to_string()),
        );
        return;
    }
    let title = format!("Update {}", req.targets.label());
    let r = req.clone();
    jobs::run(
        s,
        &title,
        TaskKind::Write,
        WRITE_DIALOG,
        move |app, ctx| update_jobs::run_update(app.backend.as_ref(), &r, ctx),
        move |s, end| {
            let Some(app) = crate::ui::app(s) else { return };
            let note = disabled_note(app.backend.as_ref(), &req.targets.slots());
            match end {
                JobEnd::Ok(o) => s.add_layer(dialogs::text_screen(
                    "Update finished",
                    &update_summary(&o),
                )),
                JobEnd::Cancelled => dialogs::info(
                    s,
                    "Update cancelled",
                    &format!("Cancelled.\n{note}\nStart the update again to resume."),
                ),
                JobEnd::Failed(e) => {
                    let text = if e.contains(&note) || note.is_empty() {
                        e
                    } else {
                        format!("{e}\n{note}")
                    };
                    let retry = req.clone();
                    s.add_layer(closable(
                        Dialog::around(dialogs::scroll_text(format!(
                            "{text}\n\nRetry continues where it stopped (chunks already on \
                             the slot are skipped)."
                        )))
                        .title("Update failed")
                        .button("Retry", move |s| {
                            s.pop_layer();
                            start_write(s, retry.clone());
                        })
                        .dismiss_button(texts::BTN_CLOSE),
                    ));
                }
            }
        },
    );
}
