//! Tools menu: logs (LOG-1), background tasks, diagnostics bundle (LOG-2), subshell (SH-1),
//! remote access (SSH-1), about.

use std::sync::Arc;

use cursive::menu::Tree;
use cursive::traits::*;
use cursive::views::{Dialog, EditView, ScrollView, SelectView, TextView};
use cursive::Cursive;

use crate::constants::{APP_NAME, APP_VERSION};
use crate::logs::{read_log, search, LogSource};
use crate::tasks::TaskState;
use crate::texts;
use crate::ui::dialogs::{self, closable, MAX_H, MAX_W};
use crate::ui::jobs::{self, JobEnd};
use crate::ui::progress::{describe, show_task};
use crate::ui::{app, pop_to_base, show_screen};

const LOG_TEXT: &str = "log_text";

pub fn menu() -> Tree {
    let mut logs = Tree::new();
    for src in LogSource::ALL {
        logs.add_leaf(src.title(), move |s| show_log(s, src));
    }
    let t = Tree::new()
        .subtree("Logs", logs)
        .leaf(texts::TITLE_TASKS, show_tasks);
    t.leaf("Diagnostics bundle", diagnostics_bundle)
        .leaf("Subshell", subshell)
        .leaf("Remote access (SSH)", remote_access)
        .leaf(texts::TITLE_SETTINGS, crate::ui::settings::show)
        .delimiter()
        .leaf(texts::TITLE_ABOUT, about)
}

fn show_log(s: &mut Cursive, src: LogSource) {
    let text = read_log(src).unwrap_or_else(|e| e);
    let text = if text.trim().is_empty() {
        "(empty)".to_string()
    } else {
        text
    };
    let view = ScrollView::new(TextView::new(text.clone()).with_name(LOG_TEXT))
        .scroll_strategy(cursive::view::ScrollStrategy::StickToBottom)
        .max_width(MAX_W)
        .max_height(MAX_H);
    let shared = Arc::new(text);
    show_screen(
        s,
        closable(
            Dialog::around(view)
                .title(src.title())
                .button(texts::BTN_SEARCH, move |s| {
                    ask_search(s, src.title(), Arc::clone(&shared))
                })
                .button(texts::BTN_REFRESH, move |s| show_log(s, src))
                .dismiss_button(texts::BTN_CLOSE),
        ),
    );
}

fn ask_search(s: &mut Cursive, title: &'static str, text: Arc<String>) {
    let run = move |s: &mut Cursive, q: &str| {
        s.pop_layer();
        if q.is_empty() {
            return;
        }
        let hits = search(&text, q);
        let body = if hits.is_empty() {
            format!("No lines contain '{q}'.")
        } else {
            hits.iter()
                .map(|(n, l)| format!("{n:>5}: {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        s.add_layer(dialogs::text_screen(
            &format!("{title}: '{q}' ({} lines)", hits.len()),
            &body,
        ));
    };
    s.add_layer(closable(
        Dialog::around(EditView::new().on_submit(run).fixed_width(40))
            .title("Search (Enter)")
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

/// List of background tasks with progress; Enter opens the progress dialog (UI-3).
fn show_tasks(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let running = app.tasks.running();
    let all = app.tasks.all();
    if all.is_empty() {
        dialogs::info(
            s,
            texts::TITLE_TASKS,
            "No background tasks in this session.",
        );
        return;
    }
    let mut select = SelectView::<u64>::new();
    for snap in all.iter().rev() {
        let (detail, state) = describe(snap);
        let marker = if snap.state == TaskState::Running {
            "*"
        } else {
            " "
        };
        let label = crate::report::ellipsize(
            &format!("{marker} {}: {state} {detail}", snap.name),
            MAX_W - 2,
        );
        select.add_item(label, snap.id);
    }
    let handles = running;
    let select =
        select.on_submit(
            move |s, id: &u64| match handles.iter().find(|h| h.id() == *id) {
                Some(h) => show_task(s, h.name().to_string().as_str(), h.clone()),
                None => dialogs::info(s, texts::TITLE_TASKS, "This task has finished."),
            },
        );
    show_screen(
        s,
        closable(
            Dialog::around(ScrollView::new(select).max_height(MAX_H))
                .title(texts::TITLE_TASKS)
                .button(texts::BTN_REFRESH, show_tasks)
                .dismiss_button(texts::BTN_CLOSE),
        ),
    );
}

fn subshell(s: &mut Cursive) {
    dialogs::confirm(s, "Subshell", texts::SUBSHELL_INFO, "Start", |s| {
        crate::ui::pin::require(s, "subshell", start_subshell);
    });
}

fn start_subshell(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let state = s.dump();
    let r = crate::terminal::run_subshell(&app.backend.run_dir());
    s.restore(state);
    s.clear();
    if let Err(e) = r {
        dialogs::error(s, "Cannot start the subshell.", &e);
    }
}

fn about(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let text = format!(
        "{APP_NAME} {APP_VERSION}\n\nBackend: {}\nLog file: {}\n\n\
         Keys: Esc menu, Tab/arrows move, Enter select, q/Esc close,\n\
         r refresh the Status screen.",
        app.backend.describe(),
        crate::logging::log_file_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(memory only)".into())
    );
    dialogs::info(s, texts::TITLE_ABOUT, &text);
}

const JOB_DIALOG: &str = "tools_job_progress";

/// Result of a job as a text screen or error dialog.
pub fn show_job_end(s: &mut Cursive, title: &str, end: JobEnd<String>) {
    match end {
        JobEnd::Ok(t) => s.add_layer(dialogs::text_screen(title, &t)),
        JobEnd::Cancelled => dialogs::info(s, title, "Cancelled."),
        JobEnd::Failed(e) => dialogs::error(s, &format!("{title} failed."), &e),
    }
}

/// LOG-2: diagnostics bundle to a stick.
fn diagnostics_bundle(s: &mut Cursive) {
    pop_to_base(s);
    crate::ui::backup::choose_stick(s, "Diagnostics bundle", |s, stick| {
        jobs::run(
            s,
            "Diagnostics bundle",
            crate::tasks::TaskKind::ReadOnly,
            JOB_DIALOG,
            move |app, ctx| {
                crate::diagnostics::write_bundle(app, &stick, Some(ctx)).map(|name| {
                    format!(
                        "Written to {}:\n  {name}\n\nIt contains the system report, boot \
                         variables, logs, test and network results; no Wi-Fi passphrases.",
                        stick.id
                    )
                })
            },
            |s, end| show_job_end(s, "Diagnostics bundle", end),
        );
    });
}

/// SSH-1: status with Start/Stop and loading authorized keys from a stick.
fn remote_access(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let env = crate::net::NetEnv::for_backend(app.backend.as_ref());
    let st = crate::net::ssh::status(&env);
    let text = format!("{}\n\n{}", texts::SSH_INFO, st.text());
    let start_label = if st.running {
        "Restart (new password)"
    } else {
        "Start"
    };
    show_screen(
        s,
        closable(
            Dialog::around(dialogs::scroll_text(text))
                .title("Remote access (SSH)")
                .button(start_label, |s| ssh_action(s, true))
                .button("Stop", |s| ssh_action(s, false))
                .button("Keys from stick", ssh_keys_from_stick)
                .dismiss_button(texts::BTN_CLOSE),
        ),
    );
}

fn ssh_action(s: &mut Cursive, start: bool) {
    let title = if start {
        "Start remote access"
    } else {
        "Stop remote access"
    };
    jobs::run(
        s,
        title,
        crate::tasks::TaskKind::ReadOnly,
        JOB_DIALOG,
        move |app, ctx| {
            let env = crate::net::NetEnv::for_backend(app.backend.as_ref());
            if start {
                ctx.set_message("Starting sshd");
                let r = crate::net::ssh::start(&env)?;
                crate::ui::mark_ssh_started();
                Ok(r.text())
            } else {
                ctx.set_message("Stopping sshd");
                Ok(match crate::net::ssh::stop(&env)? {
                    true => "sshd stopped; the one-time password is no longer valid.".to_string(),
                    false => "sshd was not running.".to_string(),
                })
            }
        },
        move |s, end| {
            pop_to_base(s);
            show_job_end(s, title, end)
        },
    );
}

fn ssh_keys_from_stick(s: &mut Cursive) {
    crate::ui::backup::choose_stick(s, "SSH keys from stick", |s, stick| {
        let Some(app) = app(s) else { return };
        let env = crate::net::NetEnv::for_backend(app.backend.as_ref());
        let r = crate::cli_net::keys_from_stick(&app, &stick).and_then(|t| {
            crate::net::ssh::add_authorized_keys(&env, &t).map_err(|e| e.to_string())
        });
        match r {
            Ok(n) => dialogs::info(
                s,
                "SSH keys",
                &format!("{n} key(s) added. They are valid until reboot."),
            ),
            Err(e) => dialogs::error(s, "Cannot load SSH keys.", &e),
        }
    });
}
