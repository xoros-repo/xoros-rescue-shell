//! Developer menu (DVM-1..9): only in the menu bar while `developer_menu = true`; shown with
//! `Ctrl-D` on the Status screen. Every change is a settings write and needs an admin session
//! (SEC-1). The logic is in [`crate::devmenu`].

use crate::ui::tk::menu::Tree;
use crate::ui::tk::traits::*;
use crate::ui::tk::views::{Dialog, EditView, LinearLayout, TextView};
use crate::ui::tk::Ui;

use crate::constants::DEFAULT_OTA_BASE_URL;
use crate::devmenu::{self, FlavorList, ListSource};
use crate::settings::{self, Settings};
use crate::texts;
use crate::ui::access::{gate, require};
use crate::ui::dialogs::{self, closable, remove_named_layer, MAX_W};
use crate::ui::{app, pop_to_base, update_status_line};

const LOADING: &str = "dev_loading";
const F_SERVER: &str = "dev_server";

pub fn menu() -> Tree {
    let cfg = settings::current();
    let sig = if cfg.skip_signature {
        "Signature check: OFF (turn on)"
    } else {
        "Signature check: on (turn off)"
    };
    Tree::new()
        .leaf("Status", show_status)
        .leaf("Image flavor...", flavors)
        .leaf("OTA server...", server)
        .leaf(sig, gate("developer: signature check", toggle_signature))
        .delimiter()
        .leaf(
            "Reset developer settings",
            gate("developer: reset", |s| {
                dialogs::confirm(
                    s,
                    texts::TITLE_DEVELOPER,
                    "Clear the version/suffix override, turn the signature check on and use \
                     the default OTA server?",
                    "Reset",
                    |s| {
                        store(
                            s,
                            devmenu::reset(&settings::current()),
                            "Developer settings reset.",
                        )
                    },
                )
            }),
        )
        .leaf(
            "Hide developer menu",
            gate("developer: hide", |s| {
                dialogs::confirm(
                    s,
                    texts::TITLE_DEVELOPER,
                    "Hide the developer menu? The overrides (version, suffix, signature check) \
                     are cleared with it. Ctrl-D on the Status screen shows it again.",
                    "Hide",
                    |s| {
                        store(
                            s,
                            devmenu::hidden(&settings::current()),
                            "Developer menu hidden.",
                        )
                    },
                )
            }),
        )
}

/// DVM-1: `Ctrl-D` on the Status screen.
pub fn ctrl_d(s: &mut Ui) {
    if settings::current().developer_menu {
        dialogs::info(
            s,
            texts::TITLE_DEVELOPER,
            "The developer menu is shown (Esc, then Developer).",
        );
        return;
    }
    dialogs::confirm(
        s,
        texts::TITLE_DEVELOPER,
        "Show the developer menu? It lets this device install development builds \
         (other image flavors and versions, another OTA server, no signature check).",
        "Show",
        |s| {
            require(s, "developer: show menu", |s| {
                store(
                    s,
                    devmenu::shown(&settings::current()),
                    "Developer menu shown: Esc, then Developer.",
                )
            })
        },
    );
}

/// Save, refresh the status line, tell the user.
fn store(s: &mut Ui, next: Settings, done: &str) {
    let Some(app) = app(s) else { return };
    match settings::save(app.backend.as_ref(), &next) {
        Ok(()) => {
            log::info!("action: developer menu: {done}");
            pop_to_base(s);
            crate::ui::refresh_status(s);
            update_status_line(s);
            dialogs::info(s, texts::TITLE_DEVELOPER, done);
        }
        Err(e) => dialogs::error(s, "Cannot save the settings.", &e),
    }
}

fn show_status(s: &mut Ui) {
    let cfg = settings::current();
    let mut t = devmenu::status_text(&cfg);
    t.push_str(&format!("OTA server: {}\n", cfg.ota_base_url));
    t.push_str(
        "\nThe overrides apply to Update -> From network and '--cli update'. \
         USB updates are not affected.",
    );
    dialogs::info(s, texts::TITLE_DEVELOPER, &t);
}

// --------------------------------------------------------------------------------------------
// DVM-2, DVM-3: flavor list and choice

fn flavors(s: &mut Ui) {
    let Some(app) = app(s) else { return };
    let base = settings::current().ota_base_url;
    s.add_layer(
        Dialog::around(TextView::new(format!(
            "Loading the flavor list from\n{base} ..."
        )))
        .title(texts::TITLE_DEVELOPER)
        .with_name(LOADING),
    );
    let sink = s.cb_sink().clone();
    let spawned = std::thread::Builder::new()
        .name("dev-flavors".into())
        .spawn(move || {
            let r = devmenu::load_flavors(app.backend.as_ref(), &base);
            let _ = sink.send(Box::new(move |s: &mut Ui| {
                remove_named_layer(s, LOADING);
                match r {
                    Ok(list) => show_list(s, list),
                    Err(e) => dialogs::error(s, "Cannot list the flavors.", &e),
                }
            }));
        });
    if let Err(e) = spawned {
        remove_named_layer(s, LOADING);
        dialogs::error(s, "Cannot start a worker thread.", &e);
    }
}

#[derive(Clone)]
enum Pick {
    Follow,
    Flavor(String, xoros_core::env::Suffix),
}

fn show_list(s: &mut Ui, list: FlavorList) {
    let mut text = format!("Server: {}\nMachine: {}", list.base_url, list.machine);
    if let ListSource::Fallback(why) = &list.source {
        text.push_str(&format!(
            "\nNo flavor index ({}): the check on Update finds out what exists.",
            crate::report::ellipsize(why, MAX_W - 40)
        ));
    }
    let mut items = vec![(
        "Follow the installed build (no override)".to_string(),
        Pick::Follow,
    )];
    for r in &list.rows {
        items.push((
            crate::report::ellipsize(&r.label(), MAX_W - 4),
            Pick::Flavor(r.version.clone(), r.suffix),
        ));
    }
    dialogs::choose(s, "Image flavor", &text, items, |s, pick| {
        let pick = pick.clone();
        require(s, "developer: choose flavor", move |s| {
            choose(s, pick.clone())
        })
    });
}

fn choose(s: &mut Ui, pick: Pick) {
    let Some(app) = app(s) else { return };
    let cfg = settings::current();
    let installed = crate::update_jobs::device_target(app.backend.as_ref()).target;
    match pick {
        Pick::Follow => store(
            s,
            devmenu::following_installed(&cfg),
            "Updates follow the installed build again.",
        ),
        Pick::Flavor(version, suffix) => {
            let next = match devmenu::with_flavor(&cfg, installed.as_ref(), &version, suffix) {
                Ok(n) => n,
                Err(e) => {
                    dialogs::error(s, "Invalid choice.", &e);
                    return;
                }
            };
            let mut text = format!(
                "Use {version} / {suffix} for Update -> From network?\nNothing is downloaded now."
            );
            for w in devmenu::choice_warnings(installed.as_ref(), &version, suffix) {
                text.push_str(&format!("\n\n{w}"));
            }
            let done =
                format!("Selected {version} / {suffix}: install with Update -> From network.");
            dialogs::confirm(s, texts::TITLE_CONFIRM, &text, "Select", move |s| {
                store(s, next.clone(), &done)
            });
        }
    }
}

// --------------------------------------------------------------------------------------------
// DVM-4: OTA server

fn server(s: &mut Ui) {
    let cfg = settings::current();
    let layout = LinearLayout::vertical()
        .child(
            TextView::new(
                "Base URL of the OTA server. https://, or http:// for a local mirror \
                 (e.g. http://192.168.1.10:8000).",
            )
            .max_width(MAX_W),
        )
        .child(TextView::new(" "))
        .child(
            EditView::new()
                .content(cfg.ota_base_url.clone())
                .with_name(F_SERVER)
                .fixed_width(60),
        );
    s.add_layer(closable(
        Dialog::around(layout)
            .title("OTA server")
            .button("Save", gate("developer: OTA server", save_server))
            .button(
                "Default",
                gate("developer: OTA server", |s| {
                    set_server(s, DEFAULT_OTA_BASE_URL)
                }),
            )
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

fn save_server(s: &mut Ui) {
    let url = s
        .call_on_name(F_SERVER, |v: &mut EditView| {
            v.get_content().trim().to_string()
        })
        .unwrap_or_default();
    set_server(s, &url);
}

fn set_server(s: &mut Ui, url: &str) {
    let mut next = settings::current();
    if let Err(e) = next.set("ota_base_url", url) {
        dialogs::error(s, "Invalid server URL.", &e);
        return;
    }
    let done = format!("OTA server: {}", next.ota_base_url);
    store(s, next, &done);
}

// --------------------------------------------------------------------------------------------
// DVM-7: signature check

fn toggle_signature(s: &mut Ui) {
    let cfg = settings::current();
    let mut next = cfg.clone();
    next.skip_signature = !cfg.skip_signature;
    if next.skip_signature {
        dialogs::confirm(
            s,
            texts::TITLE_CONFIRM,
            "Load manifests without checking signature.json, also when signatures are \
             required? The SHA-256 checks of chunks and images stay on. Every update in this \
             mode is logged with a warning.",
            "Turn off",
            move |s| store(s, next.clone(), "Signature check OFF."),
        );
    } else {
        store(s, next, "Signature check on.");
    }
}
