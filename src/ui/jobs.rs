//! Running a job in a background task from a screen: progress dialog, then a result callback
//! on the UI thread (UI-3, UI-5).

use std::sync::{Arc, Mutex};

use crate::ui::tk::Ui;

use crate::app::App;
use crate::tasks::{lock, TaskCtx, TaskFailure, TaskKind, TaskSnapshot, TaskState};
use crate::ui::dialogs;
use crate::ui::progress::show_task_named;

/// How a job ended, for the callback.
pub enum JobEnd<T> {
    Ok(T),
    Cancelled,
    Failed(String),
}

/// Run `body` in a task named `title` with a progress dialog called `dialog`; when it ends the
/// dialog closes and `on_end` runs on the UI thread.
pub fn run<T, B, K>(
    s: &mut Ui,
    title: &str,
    kind: TaskKind,
    dialog: &'static str,
    body: B,
    on_end: K,
) where
    T: Send + 'static,
    B: FnOnce(&App, &TaskCtx) -> Result<T, TaskFailure> + Send + 'static,
    K: FnOnce(&mut Ui, JobEnd<T>) + Send + 'static,
{
    let Some(app) = crate::ui::app(s) else { return };
    let result: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
    let r_body = Arc::clone(&result);
    let app_body = Arc::clone(&app);
    let sink = s.cb_sink().clone();
    let on_finish = Box::new(move |snap: TaskSnapshot| {
        let value = lock(&result).take();
        let _ = sink.send(Box::new(move |s: &mut Ui| {
            dialogs::remove_named_layer(s, dialog);
            let end = match (snap.state, value) {
                (TaskState::Succeeded(_), Some(v)) => JobEnd::Ok(v),
                (TaskState::Cancelled, _) => JobEnd::Cancelled,
                (TaskState::Failed(e), _) => JobEnd::Failed(e),
                _ => JobEnd::Failed("The task ended without a result.".into()),
            };
            on_end(s, end);
            crate::ui::refresh_status(s);
        }));
    });
    let spawned = app.tasks.spawn(
        title,
        kind,
        move |ctx| {
            let v = body(&app_body, ctx)?;
            *lock(&r_body) = Some(v);
            Ok("done".into())
        },
        Some(on_finish),
    );
    match spawned {
        Ok(h) => show_task_named(s, title, h, dialog),
        Err(e) => dialogs::error(s, "Cannot start the task.", &e),
    }
}
