//! cursive screens and dialogs, one module per menu (UI-*). The only place that uses cursive.

pub mod backup;
pub mod boot;
pub mod dialogs;
pub mod jobs;
pub mod network;
pub mod power;
pub mod progress;
pub mod status;
pub mod tests;
pub mod theme;
pub mod tools;
pub mod update;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cursive::event::{Event, Key};
use cursive::menu;
use cursive::traits::*;
use cursive::views::{LinearLayout, Panel, ScrollView, TextView};
use cursive::Cursive;

use crate::app::App;
use crate::constants::{APP_NAME, APP_VERSION};
use crate::texts;

/// Name of the status text view on the base layer.
pub const STATUS_TEXT: &str = "status_text";
/// Name of the bottom status line.
pub const STATUS_LINE: &str = "status_line";

/// Number of layers that form the base screen (the Status screen with the status line).
const BASE_LAYERS: usize = 1;

/// The shared application state stored as cursive user data.
pub fn app(s: &mut Cursive) -> Option<Arc<App>> {
    s.user_data::<Arc<App>>().cloned()
}

/// Close all screens/dialogs above the base Status screen.
pub fn pop_to_base(s: &mut Cursive) {
    while s.screen().len() > BASE_LAYERS {
        s.pop_layer();
    }
}

/// Show a screen opened from the menu: replaces other open screens.
pub fn show_screen<V: View>(s: &mut Cursive, view: V) {
    pop_to_base(s);
    s.add_layer(view);
}

fn status_line_text(app: &App, width: usize) -> String {
    let mut parts = Vec::new();
    if app.backend.is_simulation() {
        parts.push(format!("[{}]", texts::SIMULATION_NOTE));
    }
    let running = app.tasks.running();
    if !running.is_empty() {
        parts.push(format!("[{} task(s)]", running.len()));
    }
    parts.push(texts::STATUS_BAR_KEYS.to_string());
    parts.push(format!("v{APP_VERSION}"));
    let mut line = parts.join("  ");
    if let Some(last) = crate::logging::last_line() {
        // UI-7: last log message (without the date) when there is room.
        let msg = last
            .splitn(3, ' ')
            .nth(2)
            .unwrap_or(&last)
            .trim()
            .to_string();
        let room = width.saturating_sub(line.chars().count() + 3);
        if room >= 20 {
            line = format!("{line} | {}", crate::report::ellipsize(&msg, room));
        }
    }
    line
}

/// Refresh the bottom status line.
pub fn update_status_line(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let width = s.screen_size().x;
    let text = status_line_text(&app, width);
    s.call_on_name(STATUS_LINE, |v: &mut TextView| v.set_content(text));
}

/// Recompute the Status screen text.
pub fn refresh_status(s: &mut Cursive) {
    let Some(app) = app(s) else { return };
    let text = crate::report::status_text(&app.status());
    s.call_on_name(STATUS_TEXT, |v: &mut TextView| v.set_content(text));
    update_status_line(s);
}

fn build_menubar(s: &mut Cursive) {
    // Rebuilt on every Esc: cursive keeps the last focused menu otherwise, so the bar would
    // reopen on the menu of the previous action instead of the first one.
    s.menubar().clear();
    s.menubar()
        .add_subtree(texts::MENU_STATUS, status::menu())
        .add_subtree(texts::MENU_BOOT, boot::menu())
        .add_subtree(texts::MENU_UPDATE, update::menu())
        .add_subtree(texts::MENU_BACKUP, backup::menu())
        .add_subtree(texts::MENU_NETWORK, network::menu())
        .add_subtree(texts::MENU_TESTS, tests::menu())
        .add_subtree(texts::MENU_TOOLS, tools::menu())
        .add_subtree(texts::MENU_POWER, power::menu());
    s.set_autohide_menu(false);
}

fn base_layer() -> impl View {
    let status = Panel::new(
        ScrollView::new(TextView::new("Loading...").with_name(STATUS_TEXT)).full_height(),
    )
    .title(format!("{APP_NAME} - {}", texts::TITLE_STATUS));
    LinearLayout::vertical().child(status.full_width()).child(
        TextView::new("")
            .no_wrap()
            .with_name(STATUS_LINE)
            .full_width(),
    )
}

/// What the ticker saw last: number of screen layers and finished tasks at the last refresh.
static TICK_SEEN: Mutex<(usize, u64)> = Mutex::new((BASE_LAYERS, 0));

/// Ticker callback: status line, and the Status screen when it becomes visible again after a
/// dialog/screen was closed or when a task finished while it is visible. Actions change the
/// state (update, boot settings, reset, tests), so the screen is never left stale.
fn on_tick(s: &mut Cursive) {
    update_status_line(s);
    let Some(app) = app(s) else { return };
    let layers = s.screen().len();
    let finished = app.tasks.finished_count();
    let refresh = {
        let mut seen = crate::tasks::lock(&TICK_SEEN);
        let on_base = layers <= BASE_LAYERS;
        let refresh = on_base && (seen.0 > BASE_LAYERS || seen.1 != finished);
        seen.0 = layers;
        if on_base {
            seen.1 = finished;
        }
        refresh
    };
    if refresh {
        refresh_status(s);
    }
}

/// Periodic redraw and status line update while the UI runs (progress bars, UI-3).
fn start_ticker(s: &Cursive) {
    let sink = s.cb_sink().clone();
    let _ = std::thread::Builder::new()
        .name("ui-ticker".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_millis(300));
            if sink.send(Box::new(on_tick)).is_err() {
                break;
            }
        });
}

/// Run the TUI until the user quits (the app normally runs until reboot).
pub fn run(app: App) -> Result<(), String> {
    let app = Arc::new(app);
    let mut siv = Cursive::new();
    siv.set_theme(theme::named(&crate::settings::current().theme));
    siv.set_user_data(Arc::clone(&app));
    build_menubar(&mut siv);
    siv.add_fullscreen_layer(base_layer());

    // UI-2: Esc opens the menu; Ctrl-C does not exit.
    siv.add_global_callback(Key::Esc, |s| {
        build_menubar(s);
        s.select_menubar();
    });
    siv.clear_global_callbacks(Event::CtrlChar('c'));
    siv.set_on_pre_event(Event::CtrlChar('c'), |s| {
        dialogs::info(s, APP_NAME, texts::CTRL_C_HINT);
    });
    siv.add_global_callback('r', |s| {
        if s.screen().len() == BASE_LAYERS {
            refresh_status(s);
        }
    });

    refresh_status(&mut siv);
    // SYS-1: rescue image hash in the background, "checking..." until done.
    let sink = siv.cb_sink().clone();
    if let Err(e) = app.start_rescue_check(Some(Box::new(move |_| {
        let _ = sink.send(Box::new(refresh_status));
    }))) {
        log::warn!("rescue check not started: {e}");
    }
    app.start_network_setup();
    refresh_status(&mut siv);
    start_ticker(&siv);

    let backend_init = || -> std::io::Result<Box<dyn cursive::backend::Backend>> {
        let backend = cursive::backends::crossterm::Backend::init()?;
        Ok(Box::new(cursive_buffered_backend::BufferedBackend::new(
            backend,
        )))
    };
    siv.try_run_with(backend_init).map_err(|e| e.to_string())?;
    app.tasks.cancel_all();
    stop_ssh_if_started(&app);
    Ok(())
}

/// sshd was started from this TUI (SSH-1: stop it when the app exits).
static SSH_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn mark_ssh_started() {
    SSH_STARTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn stop_ssh_if_started(app: &App) {
    if SSH_STARTED.load(std::sync::atomic::Ordering::SeqCst) {
        let env = crate::net::NetEnv::for_backend(app.backend.as_ref());
        if let Err(e) = crate::net::ssh::stop(&env) {
            log::warn!("SSH-1: {e}");
        }
    }
}

/// A menu leaf that shows "not implemented yet" for `feature`.
pub fn todo_leaf(tree: menu::Tree, label: &str, feature: &'static str) -> menu::Tree {
    tree.leaf(label, move |s| dialogs::not_implemented(s, feature))
}
