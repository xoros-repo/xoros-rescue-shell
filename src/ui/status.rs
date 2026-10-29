//! Status menu: overview (SYS-1), hardware (SYS-2), storage (SYS-3), network details (SYS-4),
//! system report to a stick (SYS-5).

use cursive::menu::Tree;
use cursive::Cursive;

use crate::texts;
use crate::ui::dialogs::text_screen;
use crate::ui::{app, pop_to_base, refresh_status, show_screen};

pub fn menu() -> Tree {
    let t = Tree::new()
        .leaf("Overview", |s| {
            pop_to_base(s);
            refresh_status(s);
        })
        .leaf("Hardware", show_hardware)
        .leaf("Storage", show_storage);
    t.leaf("Network details", show_network)
        .leaf("Save system report", save_report)
}

pub fn show_hardware(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let hw = crate::hw::hardware_info(&app.backend.host_paths());
    let text = crate::report::hardware_text(&hw);
    show_screen(s, text_screen(texts::TITLE_HARDWARE, &text));
}

pub fn show_storage(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let text = crate::report::storage_text(&app.backend.block_devices(), &app.backend.layout());
    show_screen(s, text_screen(texts::TITLE_STORAGE, &text));
}

/// SYS-4: network details with a Refresh button.
pub fn show_network(s: &mut Cursive) {
    use cursive::views::Dialog;
    let Some(app) = app(s) else { return };
    let env = crate::net::NetEnv::for_backend(app.backend.as_ref());
    let text = crate::net::details::text(&crate::net::details::collect(&env));
    show_screen(
        s,
        crate::ui::dialogs::closable(
            Dialog::around(crate::ui::dialogs::scroll_text(text))
                .title("Network details")
                .button(texts::BTN_REFRESH, show_network)
                .dismiss_button(texts::BTN_CLOSE),
        ),
    );
}

/// SYS-5: write the system report (text + JSON) to a stick.
fn save_report(s: &mut Cursive) {
    pop_to_base(s);
    crate::ui::backup::choose_stick(s, "Save system report", |s, stick| {
        crate::ui::jobs::run(
            s,
            "Save system report",
            crate::tasks::TaskKind::ReadOnly,
            "status_job_progress",
            move |app, ctx| {
                ctx.set_message("Collecting the system report");
                crate::diagnostics::save_report(app, &stick).map(|names| {
                    format!(
                        "Written to {}:\n{}",
                        stick.id,
                        names.iter().map(|n| format!("  {n}\n")).collect::<String>()
                    )
                })
            },
            |s, end| crate::ui::tools::show_job_end(s, "Save system report", end),
        );
    });
}
