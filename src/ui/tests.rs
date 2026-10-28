//! Tests menu (TST-*): slot integrity (TST-4), rescue integrity (TST-5), memory test (TST-1),
//! disk read test (TST-3), file system check (TST-6) and the results of this session.
//!
//! The screens only collect choices; the tests run in [`crate::hwtest`] inside background
//! tasks with progress and Cancel (UI-3). Every result is shown in a dialog and logged.

use cursive::menu::Tree;
use cursive::traits::*;
use cursive::views::{Checkbox, Dialog, EditView, LinearLayout, TextView};
use cursive::Cursive;

use crate::backend::{PartRole, Slot};
use crate::hwtest::fsck::{needs_repair, FsckRequest};
use crate::hwtest::memtest::{self, MemOptions};
use crate::hwtest::slotcheck::{ManifestMode, SlotTestOptions};
use crate::hwtest::{run_request, Outcome, TestEnv, TestKind, TestRequest, TestResult};
use crate::tasks::format_bytes;
use crate::texts;
use crate::ui::dialogs::{self, choose, closable, MAX_W};
use crate::ui::jobs::{self, JobEnd};
use crate::ui::{app, pop_to_base};

/// Name of the progress dialog of the running test.
const JOB_DIALOG: &str = "test_job_progress";
const READ_FILES_BOX: &str = "test_read_files";
const MANIFEST_BOX: &str = "test_manifest";
const MEM_PERCENT: &str = "test_mem_percent";
const MEM_PASSES: &str = "test_mem_passes";

pub fn menu() -> Tree {
    Tree::new()
        .leaf("Slot integrity...", slot_start)
        .leaf("Rescue integrity (SHA-256)", |s| {
            pop_to_base(s);
            run_test(s, TestRequest::Rescue);
        })
        .leaf("Memory test...", memory_start)
        .leaf("Reboot into memtest86+...", memtest86_start)
        .leaf("Disk read test...", disk_start)
        .leaf("File system check...", fsck_start)
        .leaf("Disk health...", health_start)
        .leaf("Stick write test (destructive)...", stick_start)
        .leaf("CPU stress test...", stress_start)
        .delimiter()
        .leaf(texts::TITLE_TEST_RESULTS, show_results)
}

/// Run a test in the background (progress dialog with Cancel), then show its result.
pub fn run_test(s: &mut Cursive, req: TestRequest) {
    let kind = req.kind();
    jobs::run(
        s,
        kind.title(),
        req.task_kind(),
        JOB_DIALOG,
        move |app, ctx| Ok(run_request(&TestEnv::of(app), &req, ctx)),
        move |s, end| match end {
            JobEnd::Ok(r) => show_result(s, r),
            JobEnd::Cancelled => dialogs::info(s, kind.title(), "Cancelled."),
            JobEnd::Failed(e) => dialogs::error(s, &format!("{} failed.", kind.title()), &e),
        },
    );
}

/// The result dialog; a file system check with errors offers the repair (TST-6).
fn show_result(s: &mut Cursive, r: TestResult) {
    let title = format!("{}: {}", r.kind.title(), r.outcome);
    let mut d = Dialog::around(dialogs::scroll_text(r.text())).title(title);
    let repair = needs_repair(&r.details);
    if r.kind == TestKind::FsCheck && r.outcome == Outcome::Fail && !repair.is_empty() {
        d = d.button(texts::BTN_REPAIR, move |s| {
            s.pop_layer();
            fsck_repair_confirm(s, repair.clone());
        });
    }
    s.add_layer(closable(d.dismiss_button(texts::BTN_CLOSE)));
}

fn show_results(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    pop_to_base(s);
    let text = app.test_results.text();
    s.add_layer(dialogs::text_screen(texts::TITLE_TEST_RESULTS, &text));
}

fn checkbox(name: &str, label: &str, checked: bool) -> LinearLayout {
    let mut c = Checkbox::new();
    c.set_checked(checked);
    LinearLayout::horizontal()
        .child(c.with_name(name))
        .child(TextView::new(format!(" {label}")))
}

fn is_checked(s: &mut Cursive, name: &str) -> bool {
    s.call_on_name(name, |c: &mut Checkbox| c.is_checked())
        .unwrap_or(false)
}

// --- TST-4 ---

fn slot_start(s: &mut Cursive) {
    pop_to_base(s);
    let items = vec![
        ("Both slots".to_string(), Slot::ALL.to_vec()),
        ("Slot A".to_string(), vec![Slot::A]),
        ("Slot B".to_string(), vec![Slot::B]),
    ];
    choose(
        s,
        texts::TITLE_SLOTTEST,
        texts::SLOTTEST_INTRO,
        items,
        |s, slots| slot_options(s, slots.clone()),
    );
}

fn slot_options(s: &mut Cursive, slots: Vec<Slot>) {
    let layout = LinearLayout::vertical()
        .child(checkbox(
            READ_FILES_BOX,
            "Mount the slot and read every file",
            true,
        ))
        .child(checkbox(
            MANIFEST_BOX,
            "Compare with the OTA manifest (stick, then OTA server)",
            true,
        ));
    s.add_layer(closable(
        Dialog::around(layout)
            .title(texts::TITLE_SLOTTEST)
            .button(texts::BTN_START, move |s| {
                let opts = SlotTestOptions {
                    slots: slots.clone(),
                    read_files: is_checked(s, READ_FILES_BOX),
                    manifest: if is_checked(s, MANIFEST_BOX) {
                        ManifestMode::Auto
                    } else {
                        ManifestMode::Off
                    },
                };
                s.pop_layer();
                run_test(s, TestRequest::Slots(opts));
            })
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

// --- TST-1 ---

fn number_field(label: &str, name: &str, value: u32) -> LinearLayout {
    LinearLayout::horizontal()
        .child(TextView::new(format!("{label:<30}")))
        .child(
            EditView::new()
                .content(value.to_string())
                .with_name(name)
                .fixed_width(6),
        )
}

fn read_number(s: &mut Cursive, name: &str) -> Option<u32> {
    s.call_on_name(name, |v: &mut EditView| v.get_content().trim().parse().ok())
        .flatten()
}

fn memory_start(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    pop_to_base(s);
    let mem = match memtest::mem_info(&app.backend.host_paths().proc_root) {
        Ok(i) => format!(
            "Memory: {} total, {} available. At least {} stay free{}.",
            format_bytes(i.total),
            format_bytes(i.available),
            format_bytes(memtest::RESERVE_BYTES),
            if app.backend.is_simulation() {
                format!(
                    " (simulation: at most {})",
                    format_bytes(memtest::SIM_MAX_BYTES)
                )
            } else {
                String::new()
            }
        ),
        Err(e) => format!("Memory information not available: {e}"),
    };
    let layout = LinearLayout::vertical()
        .child(TextView::new(texts::MEMTEST_INTRO).max_width(MAX_W))
        .child(TextView::new(" "))
        .child(TextView::new(mem).max_width(MAX_W))
        .child(TextView::new(" "))
        .child(number_field(
            "Share of available memory (%)",
            MEM_PERCENT,
            u32::from(memtest::DEFAULT_PERCENT),
        ))
        .child(number_field("Passes", MEM_PASSES, 1))
        .child(TextView::new(" "))
        .child(TextView::new(texts::MEMTEST_NOTE).max_width(MAX_W));
    s.add_layer(closable(
        Dialog::around(layout)
            .title(texts::TITLE_MEMTEST)
            .button(texts::BTN_START, memory_clicked)
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

fn memory_clicked(s: &mut Cursive) {
    let (lo, hi) = memtest::PERCENT_RANGE.into_inner();
    let percent =
        read_number(s, MEM_PERCENT).filter(|p| (u32::from(lo)..=u32::from(hi)).contains(p));
    let passes = read_number(s, MEM_PASSES).filter(|p| memtest::PASSES_RANGE.contains(p));
    let (Some(percent), Some(passes)) = (percent, passes) else {
        dialogs::info(
            s,
            texts::TITLE_MEMTEST,
            "Enter a share of 1..95 % and 1..100 passes.",
        );
        return;
    };
    s.pop_layer();
    run_test(
        s,
        TestRequest::Memory(MemOptions {
            percent: percent as u8,
            passes,
            max_bytes: None,
        }),
    );
}

// --- TST-3 ---

fn disk_start(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    pop_to_base(s);
    let mut items = vec![(
        match app.backend.boot_disk() {
            Ok(d) => format!("Whole boot disk  {}  {}", d.name, format_bytes(d.size)),
            Err(_) => "Whole boot disk".to_string(),
        },
        None,
    )];
    if let Ok(l) = app.backend.layout() {
        for e in &l.entries {
            items.push((
                format!(
                    "{:<12} {:<16} {}",
                    e.role.label(),
                    e.device,
                    format_bytes(e.size)
                ),
                Some(e.role),
            ));
        }
    }
    choose(
        s,
        texts::TITLE_DISKTEST,
        texts::DISKTEST_INTRO,
        items,
        |s, role| run_test(s, TestRequest::Disk(*role)),
    );
}

// --- TST-6 ---

fn fsck_start(s: &mut Cursive) {
    pop_to_base(s);
    let items = vec![
        (
            "overlay and application".to_string(),
            vec![PartRole::Overlay, PartRole::Application],
        ),
        ("overlay".to_string(), vec![PartRole::Overlay]),
        ("application".to_string(), vec![PartRole::Application]),
    ];
    choose(
        s,
        texts::TITLE_FSCK,
        texts::FSCK_INTRO,
        items,
        |s, roles| {
            run_test(
                s,
                TestRequest::Fsck(FsckRequest {
                    roles: roles.clone(),
                    repair: false,
                    confirmation: String::new(),
                }),
            )
        },
    );
}

fn fsck_repair_confirm(s: &mut Cursive, roles: Vec<PartRole>) {
    let Some(app) = app(s) else { return };
    let layout = app.backend.layout().ok();
    let list = roles
        .iter()
        .map(|r| {
            let dev = layout
                .as_ref()
                .and_then(|l| l.get(*r))
                .map(|e| e.device.clone())
                .unwrap_or_else(|| "?".into());
            format!("{} ({dev})", r.label())
        })
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!("Partitions: {list}\n\n{}", texts::FSCK_REPAIR_CONFIRM);
    dialogs::confirm_typed(
        s,
        texts::TITLE_FSCK,
        &text,
        texts::FSCK_REPAIR_WORD,
        move |s, typed| {
            run_test(
                s,
                TestRequest::Fsck(FsckRequest {
                    roles: roles.clone(),
                    repair: true,
                    confirmation: typed,
                }),
            )
        },
    );
}

// --- TST-2 ---

fn memtest86_start(s: &mut Cursive) {
    use crate::hwtest::memtest86;
    let Some(a) = app(s) else { return };
    pop_to_base(s);
    match memtest86::available(a.backend.as_ref()) {
        Ok(true) => dialogs::confirm(
            s,
            texts::TITLE_MEMTEST86,
            texts::MEMTEST86_CONFIRM,
            "Reboot",
            |s| {
                let Some(app) = app(s) else { return };
                if let Some(t) = app.tasks.write_running() {
                    let e = format!("'{}' is still running.", t.name());
                    dialogs::error(s, "Cannot reboot now.", &e);
                    return;
                }
                match memtest86::set_next_boot(app.backend.as_ref()) {
                    Ok(()) => crate::ui::power::run_plan(s, crate::power::PowerPlan::reboot()),
                    Err(e) => dialogs::error(s, "Cannot set the next boot entry.", &e),
                }
            },
        ),
        Ok(false) => dialogs::info(s, texts::TITLE_MEMTEST86, texts::MEMTEST86_MISSING),
        Err(e) => dialogs::error(s, "Cannot read the boot configuration.", &e),
    }
}

// --- TST-7 ---

fn health_start(s: &mut Cursive) {
    let Some(a) = app(s) else { return };
    pop_to_base(s);
    let mut items = vec![("All disks".to_string(), Vec::new())];
    if let Ok(disks) = a.backend.block_devices() {
        for d in disks.iter().filter(|d| crate::hwtest::health::testable(d)) {
            items.push((
                format!(
                    "{:<10} {:<10} {:<6} {}{}",
                    d.name,
                    format_bytes(d.size),
                    d.transport,
                    crate::report::ellipsize(d.model.as_deref().unwrap_or("-"), 24),
                    if d.boot_disk { " [boot]" } else { "" }
                ),
                vec![d.name.clone()],
            ));
        }
    }
    choose(
        s,
        texts::TITLE_HEALTH,
        texts::HEALTH_INTRO,
        items,
        |s, disks| run_test(s, TestRequest::Health(disks.clone())),
    );
}

// --- TST-8 ---

fn stick_start(s: &mut Cursive) {
    use crate::hwtest::sticktest::{candidates, label};
    let Some(a) = app(s) else { return };
    pop_to_base(s);
    let disks = match candidates(a.backend.as_ref()) {
        Ok(d) if d.is_empty() => {
            dialogs::info(s, texts::TITLE_STICKTEST, "No stick found (the boot disk is never offered).");
            return;
        }
        Ok(d) => d,
        Err(e) => {
            dialogs::error(s, "Cannot list the disks.", &e);
            return;
        }
    };
    let items = disks.into_iter().map(|d| (label(&d), d)).collect();
    choose(
        s,
        texts::TITLE_STICKTEST,
        texts::STICKTEST_INTRO,
        items,
        |s, d| stick_confirm(s, d.clone()),
    );
}

fn stick_confirm(s: &mut Cursive, d: crate::backend::BlockDevice) {
    use crate::hwtest::sticktest::{StickTestRequest, CONFIRM_WORD};
    let parts = if d.partitions.is_empty() {
        "no partitions".to_string()
    } else {
        d.partitions
            .iter()
            .map(|p| {
                format!(
                    "{} {} {}",
                    p.name,
                    p.fstype.as_deref().unwrap_or("?"),
                    format_bytes(p.size)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    // About 10 MB/s write + 20 MB/s read on a typical stick.
    let minutes = d.size / 10_000_000 / 60 + d.size / 20_000_000 / 60 + 1;
    let text = format!(
        "ALL DATA on {} ({}, {}) will be destroyed:\n{parts}\n\nThis takes roughly {minutes} \
         minute(s) or more.",
        d.name,
        d.model.as_deref().unwrap_or("unknown model"),
        format_bytes(d.size)
    );
    let disk = d.name.clone();
    dialogs::confirm_typed(s, texts::TITLE_STICKTEST, &text, CONFIRM_WORD, move |s, typed| {
        run_test(
            s,
            TestRequest::Stick(StickTestRequest {
                disk: disk.clone(),
                confirmation: typed,
            }),
        )
    });
}

// --- TST-9 ---

const STRESS_MINUTES: &str = "test_stress_minutes";

fn stress_start(s: &mut Cursive) {
    use crate::hwtest::stress;
    pop_to_base(s);
    let layout = LinearLayout::vertical()
        .child(TextView::new(texts::STRESS_INTRO).max_width(MAX_W))
        .child(TextView::new(" "))
        .child(number_field("Minutes (1..120)", STRESS_MINUTES, stress::DEFAULT_MINUTES));
    s.add_layer(closable(
        Dialog::around(layout)
            .title(texts::TITLE_STRESS)
            .button(texts::BTN_START, |s| {
                let Some(m) = read_number(s, STRESS_MINUTES).filter(|m| stress::MINUTES_RANGE.contains(m))
                else {
                    dialogs::info(s, texts::TITLE_STRESS, "Enter 1..120 minutes.");
                    return;
                };
                s.pop_layer();
                run_test(
                    s,
                    TestRequest::Stress(stress::StressOptions {
                        duration: std::time::Duration::from_secs(u64::from(m) * 60),
                    }),
                );
            })
            .dismiss_button(texts::BTN_CANCEL),
    ));
}
