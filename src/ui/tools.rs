//! Tools menu: logs (LOG-1), background tasks, subshell (SH-1), about.

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
use crate::ui::progress::{describe, show_task};
use crate::ui::{app, show_screen, todo_leaf};

const LOG_TEXT: &str = "log_text";

pub fn menu() -> Tree {
    let mut logs = Tree::new();
    for src in LogSource::ALL {
        logs.add_leaf(src.title(), move |s| show_log(s, src));
    }
    let t = Tree::new()
        .subtree("Logs", logs)
        .leaf(texts::TITLE_TASKS, show_tasks);
    let t = todo_leaf(t, "Diagnostics bundle", "The diagnostics bundle (LOG-2)");
    let t = t.leaf("Subshell", subshell);
    let t = todo_leaf(t, "Remote access (SSH)", "Remote access via SSH (SSH-1)");
    t.delimiter().leaf(texts::TITLE_ABOUT, about)
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
        let Some(app) = app(s) else { return };
        let state = s.dump();
        let r = crate::terminal::run_subshell(&app.backend.run_dir());
        s.restore(state);
        s.clear();
        if let Err(e) = r {
            dialogs::error(s, "Cannot start the subshell.", &e);
        }
    });
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
