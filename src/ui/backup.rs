//! Backup menu: export to stick, import from stick, factory reset (BAK-1..5, RST-1..3).
//!
//! The screens only collect choices and show results; the work runs in
//! [`crate::backup_jobs`] inside background tasks (UI-3).

use std::sync::{Arc, Mutex};

use cursive::menu::Tree;
use cursive::traits::*;
use cursive::views::{Checkbox, Dialog, LinearLayout, ScrollView, SelectView, TextView};
use cursive::Cursive;

use crate::app::App;
use crate::backend::{PartRole, Stick};
use crate::backup::{ImportMode, PartitionRole, RESET_CONFIRM_WORD};
use crate::backup_jobs::{
    self, describe_backup, device_identity, export_summary, import_summary, reset_summary,
    ExportRequest, FoundBackup, ImportRequest, ResetRequest,
};
use crate::tasks::{format_bytes, lock, TaskCtx, TaskFailure, TaskKind, TaskState};
use crate::texts;
use crate::ui::dialogs::{self, closable, MAX_H, MAX_W};
use crate::ui::progress::show_task_named;
use crate::ui::{app, pop_to_base};

/// Name of the progress dialog of the running backup job.
const JOB_DIALOG: &str = "backup_job_progress";
const MODE_SELECT: &str = "backup_mode_select";
const RESET_RESCUE_BOX: &str = "reset_rescue_settings";

pub fn menu() -> Tree {
    Tree::new()
        .leaf("Export to stick", export_start)
        .leaf("Import from stick", |s| {
            crate::ui::pin::require(s, "backup import", import_start)
        })
        .delimiter()
        .leaf("Reset configuration", |s| {
            crate::ui::pin::require(s, "configuration reset", |s| reset_start(s, false))
        })
        .leaf("Full factory reset", |s| {
            crate::ui::pin::require(s, "full factory reset", |s| reset_start(s, true))
        })
        .delimiter()
        .leaf("Repair file system...", crate::ui::maint::repair_start)
        .leaf("Raw partition image...", crate::ui::maint::image_start)
        .leaf("Safely remove stick", crate::ui::maint::safe_remove_start)
}

fn checkbox_name(role: PartitionRole) -> String {
    format!("backup_part_{}", role.as_str())
}

/// Checkboxes for the given partitions (all checked), with a used-space hint.
fn partition_checkboxes(app: &App, roles: &[PartitionRole]) -> LinearLayout {
    let mut l = LinearLayout::vertical();
    for r in roles {
        let usage = app
            .backend
            .partition_usage(PartRole::from(*r))
            .ok()
            .flatten()
            .map(|u| format!("  ({} used)", format_bytes(u.used)))
            .unwrap_or_default();
        l.add_child(
            LinearLayout::horizontal()
                .child(Checkbox::new().checked().with_name(checkbox_name(*r)))
                .child(TextView::new(format!(" {}{usage}", r.as_str()))),
        );
    }
    l
}

fn checked_roles(s: &mut Cursive, roles: &[PartitionRole]) -> Vec<PartitionRole> {
    roles
        .iter()
        .copied()
        .filter(|r| {
            s.call_on_name(&checkbox_name(*r), |c: &mut Checkbox| c.is_checked())
                .unwrap_or(false)
        })
        .collect()
}

fn role_list(roles: &[PartitionRole]) -> String {
    roles
        .iter()
        .map(|r| r.as_str())
        .collect::<Vec<_>>()
        .join(" and ")
}

/// Refuse to start while another write task runs (UI-3).
fn write_busy(s: &mut Cursive, app: &App) -> bool {
    match app.tasks.write_running() {
        Some(t) => {
            dialogs::error(
                s,
                "Cannot start now.",
                &crate::tasks::SpawnError::WriteBusy(t.name().to_string()),
            );
            true
        }
        None => false,
    }
}

/// Run a backup job in the background; when it ends the progress dialog closes and `on_ok`
/// shows the result, a failure shows `fail_title` with the full reason (UI-5, BAK-4a).
fn run_job<T, B, K>(
    s: &mut Cursive,
    title: &'static str,
    fail_title: &'static str,
    body: B,
    on_ok: K,
) where
    T: Send + 'static,
    B: FnOnce(&App, &TaskCtx) -> Result<T, TaskFailure> + Send + 'static,
    K: FnOnce(&mut Cursive, T) + Send + 'static,
{
    let Some(app) = app(s) else { return };
    let result: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
    let r_body = Arc::clone(&result);
    let app_body = Arc::clone(&app);
    let sink = s.cb_sink().clone();
    let on_finish = Box::new(move |snap: crate::tasks::TaskSnapshot| {
        let value = lock(&result).take();
        let _ = sink.send(Box::new(move |s: &mut Cursive| {
            dialogs::remove_named_layer(s, JOB_DIALOG);
            match (snap.state, value) {
                (TaskState::Succeeded(_), Some(v)) => on_ok(s, v),
                (TaskState::Cancelled, _) => dialogs::info(s, title, "Cancelled."),
                (TaskState::Failed(e), _) => s.add_layer(dialogs::text_screen(fail_title, &e)),
                _ => {}
            }
            crate::ui::refresh_status(s);
        }));
    });
    let spawned = app.tasks.spawn(
        title,
        TaskKind::Write,
        move |ctx| {
            let v = body(&app_body, ctx)?;
            *lock(&r_body) = Some(v);
            Ok("done".into())
        },
        Some(on_finish),
    );
    match spawned {
        Ok(h) => show_task_named(s, title, h, JOB_DIALOG),
        Err(e) => dialogs::error(s, "Cannot start the task.", &e),
    }
}

/// Let the user pick an external stick; "Refresh" re-reads the list (USB-1).
pub(crate) fn choose_stick<F>(s: &mut Cursive, purpose: &'static str, on_pick: F)
where
    F: Fn(&mut Cursive, Stick) + Clone + Send + Sync + 'static,
{
    let Some(app) = app(s) else { return };
    let sticks = match app.backend.external_sticks() {
        Ok(v) => v,
        Err(e) => {
            dialogs::error(s, "Cannot list external sticks.", &e);
            return;
        }
    };
    let refresh = {
        let on_pick = on_pick.clone();
        move |s: &mut Cursive| {
            s.pop_layer();
            choose_stick(s, purpose, on_pick.clone());
        }
    };
    watch_sticks(s, purpose, on_pick.clone(), &sticks);
    if sticks.is_empty() {
        s.add_layer(
            closable(
                Dialog::around(TextView::new(texts::NO_STICK).max_width(MAX_W))
                    .title(purpose)
                    .button(texts::BTN_REFRESH, refresh)
                    .dismiss_button(texts::BTN_CANCEL),
            )
            .with_name(STICK_LIST),
        );
        return;
    }
    let mut select = SelectView::<Stick>::new();
    for st in sticks {
        let label = crate::report::ellipsize(&st.description, MAX_W - 4);
        select.add_item(label, st);
    }
    let select = select.on_submit(move |s, st: &Stick| {
        s.pop_layer();
        on_pick(s, st.clone());
    });
    s.add_layer(
        closable(
            Dialog::around(
                LinearLayout::vertical()
                    .child(TextView::new("Choose the stick:"))
                    .child(ScrollView::new(select).max_height(10)),
            )
            .title(purpose)
            .button(texts::BTN_REFRESH, refresh)
            .dismiss_button(texts::BTN_CANCEL),
        )
        .with_name(STICK_LIST),
    );
}

/// Name of the stick list layer (hot-plug refresh, USB-1).
const STICK_LIST: &str = "stick_list";

/// Generation of the open stick list; a watcher stops when a newer list was opened.
static STICK_LIST_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// USB-1 hot-plug: poll the sticks every 2 s while the list is open and rebuild it when a
/// stick appears or disappears.
fn watch_sticks<F>(s: &mut Cursive, purpose: &'static str, on_pick: F, shown: &[Stick])
where
    F: Fn(&mut Cursive, Stick) + Clone + Send + Sync + 'static,
{
    use std::sync::atomic::Ordering;
    let Some(app) = app(s) else { return };
    let gen = STICK_LIST_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let mut known: Vec<String> = shown.iter().map(|st| st.id.clone()).collect();
    known.sort();
    let sink = s.cb_sink().clone();
    let spawned = std::thread::Builder::new()
        .name("stick-watch".into())
        .spawn(move || {
            // At most 30 minutes per list.
            for _ in 0..900 {
                std::thread::sleep(crate::sticks::POLL_INTERVAL);
                if STICK_LIST_GEN.load(Ordering::SeqCst) != gen {
                    return;
                }
                let now = crate::sticks::ids(app.backend.as_ref());
                if now == known {
                    continue;
                }
                known = now;
                let on_pick = on_pick.clone();
                let _ = sink.send(Box::new(move |s: &mut Cursive| {
                    if STICK_LIST_GEN.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    // Only when the list is the top layer (not behind a dialog opened from it).
                    if s.screen_mut().find_layer_from_name(STICK_LIST)
                        == Some(cursive::views::LayerPosition::FromFront(0))
                    {
                        s.pop_layer();
                        choose_stick(s, purpose, on_pick);
                    }
                }));
            }
        });
    if let Err(e) = spawned {
        log::warn!("stick hot-plug watch not started: {e}");
    }
}

// --------------------------------------------------------------------------------------------
// export (BAK-1..3)

fn export_start(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    pop_to_base(s);
    if write_busy(s, &app) {
        return;
    }
    let roles = PartitionRole::ALL;
    let layout = LinearLayout::vertical()
        .child(TextView::new(texts::EXPORT_INTRO).max_width(MAX_W))
        .child(TextView::new(" "))
        .child(partition_checkboxes(&app, &roles));
    s.add_layer(closable(
        Dialog::around(layout)
            .title(texts::TITLE_EXPORT)
            .button("Next", move |s| {
                let chosen = checked_roles(s, &roles);
                if chosen.is_empty() {
                    dialogs::info(s, texts::TITLE_EXPORT, "Select at least one partition.");
                    return;
                }
                s.pop_layer();
                choose_stick(s, texts::TITLE_EXPORT, move |s, stick| {
                    export_confirm(s, chosen.clone(), stick)
                });
            })
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

fn export_confirm(s: &mut Cursive, roles: Vec<PartitionRole>, stick: Stick) {
    let Some(app) = app(s) else { return };
    let req = ExportRequest { roles, stick };
    // Mounting and statvfs are quick; the space check is done before the task starts (BAK-3).
    let preview = match backup_jobs::export_preview(app.backend.as_ref(), &req) {
        Ok(p) => p,
        Err(e) => {
            dialogs::error(s, "Cannot prepare the export.", &e);
            return;
        }
    };
    let text = format!(
        "Export {} to stick {}.\n\n{}",
        role_list(&req.roles),
        req.stick.description,
        preview.text()
    );
    dialogs::confirm(s, texts::TITLE_EXPORT, &text, "Start export", move |s| {
        let req = req.clone();
        let stick = req.stick.clone();
        run_job(
            s,
            texts::TASK_EXPORT,
            "Export failed",
            move |app, ctx| backup_jobs::run_export(app.backend.as_ref(), &req, ctx),
            move |s, rep| {
                s.add_layer(dialogs::text_screen(
                    texts::TITLE_EXPORT,
                    &export_summary(&rep, &stick),
                ))
            },
        );
    });
}

// --------------------------------------------------------------------------------------------
// import (BAK-4, BAK-4a, BAK-5)

fn import_start(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    pop_to_base(s);
    if write_busy(s, &app) {
        return;
    }
    choose_stick(s, texts::TITLE_IMPORT, import_list);
}

fn import_list(s: &mut Cursive, stick: Stick) {
    let Some(app) = app(s) else { return };
    let found = match backup_jobs::list_stick_backups(app.backend.as_ref(), &stick) {
        Ok(f) => f,
        Err(e) => {
            dialogs::error(s, "Cannot read the stick.", &e);
            return;
        }
    };
    if found.is_empty() {
        dialogs::info(
            s,
            texts::TITLE_IMPORT,
            &format!(
                "No backups found on {} (searched the stick root and xoros/backups/).",
                stick.id
            ),
        );
        return;
    }
    let mut select = SelectView::<FoundBackup>::new();
    for b in found {
        let mark = if b.info.metadata.is_err() || b.info.checksum_file.is_none() {
            "  [cannot import]"
        } else {
            ""
        };
        let label = format!("{}  {}{mark}", b.info.name, format_bytes(b.info.total_size));
        select.add_item(crate::report::ellipsize(&label, MAX_W - 4), b);
    }
    let select = select.on_submit(move |s, b: &FoundBackup| {
        import_details(s, stick.clone(), b.clone());
    });
    s.add_layer(closable(
        Dialog::around(ScrollView::new(select).max_height(MAX_H))
            .title(texts::TITLE_IMPORT)
            .dismiss_button(texts::BTN_CLOSE),
    ));
}

/// Metadata screen with the machine mismatch warning (BAK-4).
fn import_details(s: &mut Cursive, stick: Stick, b: FoundBackup) {
    let Some(app) = app(s) else { return };
    let machine = device_identity(app.backend.as_ref()).machine;
    let text = describe_backup(&b, machine.as_deref());
    let b_verify = b.clone();
    let st_verify = stick.clone();
    s.add_layer(closable(
        Dialog::around(dialogs::scroll_text(text))
            .title(texts::TITLE_IMPORT)
            .button("Import...", move |s| {
                import_options(s, stick.clone(), b.clone())
            })
            .button("Verify", move |s| {
                let req = ImportRequest {
                    stick: st_verify.clone(),
                    dir_rel: b_verify.dir_rel.clone(),
                    name: b_verify.info.name.clone(),
                    roles: Vec::new(),
                    mode: ImportMode::Replace,
                };
                run_job(
                    s,
                    texts::TASK_VERIFY,
                    "Verification failed",
                    move |app, ctx| backup_jobs::run_verify(app.backend.as_ref(), &req, ctx),
                    |s, msg: String| dialogs::info(s, texts::TASK_VERIFY, &msg),
                );
            })
            .dismiss_button(texts::BTN_CLOSE),
    ));
}

/// Show and log why a backup cannot be imported (BAK-4a).
fn refuse(s: &mut Cursive, reason: &str) {
    let msg = format!("The backup cannot be imported: {reason}");
    log::error!("{msg}");
    s.add_layer(dialogs::text_screen("Import refused", &msg));
}

fn import_options(s: &mut Cursive, stick: Stick, b: FoundBackup) {
    let Some(app) = app(s) else { return };
    // BAK-4a: refuse right away when the backup is known to be unusable.
    let metadata = match &b.info.metadata {
        Ok(m) => m.clone(),
        Err(e) => return refuse(s, e),
    };
    if b.info.checksum_file.is_none() {
        let e = crate::backup::BackupError::MissingChecksumFile(
            b.info.dir.join(format!("{}.sha256", b.info.name)),
        );
        return refuse(s, &e.to_string());
    }
    let roles = metadata.roles();
    let mut mode = SelectView::<ImportMode>::new();
    mode.add_item(texts::MODE_REPLACE, ImportMode::Replace);
    mode.add_item(texts::MODE_MERGE, ImportMode::Merge);
    let layout = LinearLayout::vertical()
        .child(TextView::new("Restore these partitions:"))
        .child(partition_checkboxes(&app, &roles))
        .child(TextView::new(" "))
        .child(TextView::new("Mode:"))
        .child(mode.with_name(MODE_SELECT))
        .child(TextView::new(" "))
        .child(TextView::new(texts::RESCUE_KEPT_NOTE).max_width(MAX_W));
    s.add_layer(closable(
        Dialog::around(ScrollView::new(layout).max_height(MAX_H))
            .title(texts::TITLE_IMPORT)
            .button("Next", move |s| {
                let chosen = checked_roles(s, &roles);
                if chosen.is_empty() {
                    dialogs::info(s, texts::TITLE_IMPORT, "Select at least one partition.");
                    return;
                }
                let mode = s
                    .call_on_name(MODE_SELECT, |v: &mut SelectView<ImportMode>| {
                        v.selection().map(|m| *m)
                    })
                    .flatten()
                    .unwrap_or_default();
                s.pop_layer();
                import_confirm(
                    s,
                    ImportRequest {
                        stick: stick.clone(),
                        dir_rel: b.dir_rel.clone(),
                        name: b.info.name.clone(),
                        roles: chosen,
                        mode,
                    },
                );
            })
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

/// UI-4: show what will be overwritten, then import.
fn import_confirm(s: &mut Cursive, req: ImportRequest) {
    let Some(app) = app(s) else { return };
    let layout = app.backend.layout().ok();
    let targets: Vec<String> = req
        .roles
        .iter()
        .map(|r| {
            let dev = layout
                .as_ref()
                .and_then(|l| l.get(PartRole::from(*r)))
                .map(|e| e.device.clone())
                .unwrap_or_else(|| "?".into());
            format!("  {} ({dev})", r.as_str())
        })
        .collect();
    let what = match req.mode {
        ImportMode::Replace => "ALL current content of these partitions is deleted first",
        ImportMode::Merge => "existing files with the same names are overwritten",
    };
    let text = format!(
        "Import {}\ninto:\n{}\n\nMode {:?}: {what} (except lost+found and the rescue \
         settings).\n\nThe backup is verified first; nothing is changed if verification fails.",
        req.name,
        targets.join("\n"),
        req.mode
    );
    dialogs::confirm(s, texts::TITLE_IMPORT, &text, "Import", move |s| {
        let req = req.clone();
        let name = req.name.clone();
        run_job(
            s,
            texts::TASK_IMPORT,
            "Import not done",
            move |app, ctx| backup_jobs::run_import(app.backend.as_ref(), &req, ctx),
            move |s, out| {
                s.add_layer(dialogs::text_screen(
                    "Import summary",
                    &import_summary(&name, &out),
                ))
            },
        );
    });
}

// --------------------------------------------------------------------------------------------
// factory reset (RST-1..3)

fn reset_title(full: bool) -> &'static str {
    if full {
        texts::TITLE_FULL_RESET
    } else {
        texts::TITLE_CONFIG_RESET
    }
}

fn reset_start(s: &mut Cursive, full: bool) {
    let Some(app) = app(s) else { return };
    pop_to_base(s);
    if write_busy(s, &app) {
        return;
    }
    // RST-3: offer an export first when a stick is present.
    let sticks = app.backend.external_sticks().unwrap_or_default();
    if sticks.is_empty() {
        reset_options(s, full);
        return;
    }
    s.add_layer(closable(
        Dialog::around(TextView::new(texts::RESET_OFFER_EXPORT).max_width(MAX_W))
            .title(reset_title(full))
            .button("Export first", |s| {
                s.pop_layer();
                export_start(s);
            })
            .button("Continue without backup", move |s| {
                s.pop_layer();
                reset_options(s, full);
            })
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

fn reset_options(s: &mut Cursive, full: bool) {
    let explanation = if full {
        texts::FULL_RESET_EXPLANATION
    } else {
        texts::CONFIG_RESET_EXPLANATION
    };
    let layout = LinearLayout::vertical()
        .child(TextView::new(explanation).max_width(MAX_W))
        .child(TextView::new(" "))
        .child(
            LinearLayout::horizontal()
                .child(Checkbox::new().with_name(RESET_RESCUE_BOX))
                .child(TextView::new(" Also reset the rescue shell settings")),
        );
    s.add_layer(closable(
        Dialog::around(layout)
            .title(reset_title(full))
            .button("Next", move |s| {
                let reset_rescue = s
                    .call_on_name(RESET_RESCUE_BOX, |c: &mut Checkbox| c.is_checked())
                    .unwrap_or(false);
                s.pop_layer();
                reset_confirm(s, full, reset_rescue);
            })
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

fn reset_confirm(s: &mut Cursive, full: bool, reset_rescue_settings: bool) {
    let Some(app) = app(s) else { return };
    let layout = app.backend.layout().ok();
    let dev = |r: PartRole| {
        layout
            .as_ref()
            .and_then(|l| l.get(r))
            .map(|e| e.device.clone())
            .unwrap_or_else(|| "?".into())
    };
    let mut text = format!(
        "This deletes the content of overlay ({}){}",
        dev(PartRole::Overlay),
        if reset_rescue_settings {
            " including the rescue settings"
        } else {
            " (rescue settings are kept)"
        }
    );
    if full {
        text.push_str(&format!(
            "\nand ALL data on application ({}).",
            dev(PartRole::Application)
        ));
    } else {
        text.push('.');
    }
    text.push_str("\n\nThis cannot be undone.");
    dialogs::confirm_typed(
        s,
        reset_title(full),
        &text,
        RESET_CONFIRM_WORD,
        move |s, typed| {
            let req = ResetRequest {
                full,
                reset_rescue_settings,
                confirmation: typed,
            };
            let req2 = req.clone();
            run_job(
                s,
                reset_title(full),
                "Reset failed",
                move |app, ctx| backup_jobs::run_reset(app.backend.as_ref(), &req, ctx),
                move |s, rep| {
                    s.add_layer(dialogs::text_screen(
                        reset_title(full),
                        &reset_summary(&req2, &rep),
                    ))
                },
            );
        },
    );
}
