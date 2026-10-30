//! Progress dialog for background tasks: bar, bytes, speed, ETA and Cancel (UI-3).

use crate::ui::tk::traits::*;
use crate::ui::tk::views::Dialog;
use crate::ui::tk::{Printer, Ui, Vec2};

use crate::tasks::{format_bytes, format_duration, TaskHandle, TaskSnapshot, TaskState};
use crate::texts;
use crate::ui::dialogs::closable;

const WIDTH: usize = 60;

/// Draws the live state of a task (redrawn by the UI ticker).
pub struct TaskView {
    handle: TaskHandle,
}

impl TaskView {
    pub fn new(handle: TaskHandle) -> Self {
        TaskView { handle }
    }
}

/// Text lines describing a task snapshot (also used by the task list).
pub fn describe(snap: &TaskSnapshot) -> (String, String) {
    let amounts = if snap.total > 0 {
        format!("{} / {}", format_bytes(snap.done), format_bytes(snap.total))
    } else if snap.done > 0 {
        format_bytes(snap.done)
    } else {
        String::new()
    };
    let speed = snap
        .speed()
        .map(|v| format!("  {}/s", format_bytes(v as u64)))
        .unwrap_or_default();
    let eta = match (&snap.state, snap.eta()) {
        (TaskState::Running, Some(e)) => format!("  ETA {}", format_duration(e)),
        _ => String::new(),
    };
    let detail = format!("{amounts}{speed}{eta}");
    let state = match &snap.state {
        TaskState::Running => format!("Running ({})", format_duration(snap.elapsed)),
        TaskState::Succeeded(s) => format!("Done: {s}"),
        TaskState::Failed(e) => format!("FAILED: {e}"),
        TaskState::Cancelled => "Cancelled.".to_string(),
    };
    (detail, state)
}

impl View for TaskView {
    fn draw(&self, printer: &Printer) {
        let snap = self.handle.snapshot();
        let w = printer.size.x.max(10);
        printer.print((0, 0), &crate::report::ellipsize(&snap.message, w));
        // Bar: [#####.....] 45%
        let pct = snap.fraction().map(|f| (f * 100.0).round() as usize);
        let label = pct.map(|p| format!(" {p:>3}%")).unwrap_or_default();
        let bar_w = w.saturating_sub(2 + label.len());
        let filled = match (snap.fraction(), &snap.state) {
            (_, TaskState::Succeeded(_)) => bar_w,
            (Some(f), _) => ((f * bar_w as f64) as usize).min(bar_w),
            (None, _) => 0,
        };
        printer.print((0, 1), "[");
        printer.with_style(printer.theme.highlight(), |p| {
            p.print_hline((1, 1), filled, " ");
        });
        printer.print_hline((1 + filled, 1), bar_w - filled, ".");
        printer.print((1 + bar_w, 1), "]");
        printer.print((2 + bar_w, 1), &label);
        let (detail, state) = describe(&snap);
        printer.print((0, 2), &crate::report::ellipsize(&detail, w));
        for (i, line) in crate::report::wrap(&state, w).iter().take(3).enumerate() {
            printer.print((0, 3 + i), line);
        }
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        Vec2::new(WIDTH.min(constraint.x), 6.min(constraint.y))
    }
}

/// Show a progress dialog for `handle`. "Close" hides the dialog; the task keeps running
/// and is listed in Tools -> Background tasks.
pub fn show_task(s: &mut Ui, title: &str, handle: TaskHandle) {
    s.add_layer(task_dialog(title, handle));
}

/// Like [`show_task`], with a name so the dialog can be closed when the task finishes
/// ([`crate::ui::dialogs::remove_named_layer`]).
pub fn show_task_named(s: &mut Ui, title: &str, handle: TaskHandle, name: &str) {
    s.add_layer(task_dialog(title, handle).with_name(name));
}

fn task_dialog(title: &str, handle: TaskHandle) -> impl View {
    let cancel_handle = handle.clone();
    closable(
        Dialog::around(TaskView::new(handle))
            .title(title)
            .button(texts::BTN_CANCEL, move |_| cancel_handle.cancel())
            .dismiss_button(texts::BTN_CLOSE),
    )
}
