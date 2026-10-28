//! Settings screen (CFG-1) with the PIN (CFG-2). Tools -> Settings.

use cursive::traits::*;
use cursive::views::{Checkbox, Dialog, EditView, LinearLayout, SelectView, TextView};
use cursive::Cursive;

use crate::settings::{self, Origin, Settings, THEMES};
use crate::texts;
use crate::ui::dialogs::{self, closable, MAX_W};
use crate::ui::{app, show_screen};

const F_URL: &str = "cfg_ota_base_url";
const F_PROXY: &str = "cfg_http_proxy";
const F_NTP: &str = "cfg_ntp_servers";
const F_KEYS: &str = "cfg_trusted_keys";
const F_SIG: &str = "cfg_require_signature";
const F_THEME: &str = "cfg_theme";
const F_PIN1: &str = "cfg_pin1";
const F_PIN2: &str = "cfg_pin2";

fn field(label: &str, name: &str, value: &str) -> LinearLayout {
    LinearLayout::horizontal()
        .child(TextView::new(format!("{label:<14}")))
        .child(
            EditView::new()
                .content(value)
                .with_name(name)
                .fixed_width(56),
        )
}

fn text_of(s: &mut Cursive, name: &str) -> String {
    s.call_on_name(name, |v: &mut EditView| v.get_content().trim().to_string())
        .unwrap_or_default()
}

/// Tools -> Settings (behind the PIN).
pub fn show(s: &mut Cursive) {
    crate::ui::pin::require(s, "settings", open);
}

fn open(s: &mut Cursive) {
    let st = settings::state();
    let c = &st.settings;
    let mut theme = SelectView::<String>::new().popup();
    for t in THEMES {
        theme.add_item(*t, t.to_string());
    }
    let idx = THEMES.iter().position(|t| *t == c.theme).unwrap_or(0);
    theme.set_selection(idx);
    let mut info = format!(
        "Source: {}\n",
        crate::report::ellipsize(&st.origin_text(), MAX_W - 8)
    );
    if !st.system_keys.is_empty() {
        info.push_str(&format!(
            "Rescue image keys: {} (always trusted)\n",
            st.system_keys.len()
        ));
    }
    info.push_str(&format!(
        "PIN: {}",
        if c.pin_enabled() { "set" } else { "off" }
    ));
    for w in st.warnings.iter().take(2) {
        info.push_str(&format!(
            "\nWARNING: {}",
            crate::report::ellipsize(w, MAX_W - 9)
        ));
    }
    if let Origin::Unavailable(_) = st.origin {
        info.push_str(&format!("\n{}", texts::SETTINGS_READ_ONLY));
    }
    let layout = LinearLayout::vertical()
        .child(TextView::new(texts::SETTINGS_INTRO).max_width(MAX_W))
        .child(TextView::new(" "))
        .child(field("OTA server", F_URL, &c.ota_base_url))
        .child(field("HTTP proxy", F_PROXY, &c.http_proxy))
        .child(field("Time servers", F_NTP, &c.ntp_servers.join(" ")))
        .child(field("Trusted keys", F_KEYS, &c.trusted_keys.join(" ")))
        .child(
            LinearLayout::horizontal()
                .child(TextView::new(format!("{:<14}", "Signature")))
                .child(
                    Checkbox::new()
                        .with_checked(c.require_signature)
                        .with_name(F_SIG),
                )
                .child(TextView::new(" required for OTA updates")),
        )
        .child(
            LinearLayout::horizontal()
                .child(TextView::new(format!("{:<14}", "Theme")))
                .child(theme.with_name(F_THEME)),
        )
        .child(TextView::new(" "))
        .child(TextView::new(info).max_width(MAX_W));
    show_screen(
        s,
        closable(
            Dialog::around(layout)
                .title(texts::TITLE_SETTINGS)
                .button("Save", save_clicked)
                .button("PIN...", pin_menu)
                .button("Defaults", |s| {
                    dialogs::confirm(
                        s,
                        texts::TITLE_CONFIRM,
                        "Replace all settings (and the PIN) with the built-in defaults?",
                        "Reset",
                        |s| store(s, Settings::default(), "Settings reset to the defaults."),
                    )
                })
                .dismiss_button(texts::BTN_CANCEL),
        ),
    );
}

fn save_clicked(s: &mut Cursive) {
    let mut next = settings::current();
    let mut errors = Vec::new();
    for (key, name) in [
        ("ota_base_url", F_URL),
        ("http_proxy", F_PROXY),
        ("ntp_servers", F_NTP),
        ("trusted_keys", F_KEYS),
    ] {
        let v = text_of(s, name);
        if let Err(e) = next.set(key, &v) {
            errors.push(e);
        }
    }
    let sig = s
        .call_on_name(F_SIG, |c: &mut Checkbox| c.is_checked())
        .unwrap_or(false);
    next.require_signature = sig;
    if let Some(Some(t)) = s.call_on_name(F_THEME, |v: &mut SelectView<String>| {
        v.selection().map(|t| (*t).clone())
    }) {
        if let Err(e) = next.set("theme", &t) {
            errors.push(e);
        }
    }
    if !errors.is_empty() {
        dialogs::error(s, "Settings not saved.", &errors.join("\n"));
        return;
    }
    if next.require_signature
        && next.trusted_keys.is_empty()
        && settings::state().system_keys.is_empty()
    {
        errors.push("Signatures are required but no trusted key is configured: every OTA update would be refused.".to_string());
        dialogs::error(s, "Settings not saved.", &errors.join("\n"));
        return;
    }
    store(s, next, "Settings saved.");
}

/// Save to the overlay partition, apply the theme, close the screen.
fn store(s: &mut Cursive, next: Settings, done: &str) {
    let Some(app) = app(s) else { return };
    match settings::save(app.backend.as_ref(), &next) {
        Ok(()) => {
            s.set_theme(crate::ui::theme::named(&next.theme));
            crate::ui::pop_to_base(s);
            dialogs::info(s, texts::TITLE_SETTINGS, done);
        }
        Err(e) => dialogs::error(s, "Cannot save the settings.", &e),
    }
}

fn pin_menu(s: &mut Cursive) {
    let enabled = settings::current().pin_enabled();
    let layout = LinearLayout::vertical()
        .child(TextView::new(texts::PIN_INFO).max_width(60))
        .child(TextView::new(" "))
        .child(
            LinearLayout::horizontal()
                .child(TextView::new(format!("{:<12}", "New PIN")))
                .child(EditView::new().secret().with_name(F_PIN1).fixed_width(16)),
        )
        .child(
            LinearLayout::horizontal()
                .child(TextView::new(format!("{:<12}", "Repeat")))
                .child(EditView::new().secret().with_name(F_PIN2).fixed_width(16)),
        );
    let mut d = Dialog::around(layout)
        .title(texts::TITLE_PIN)
        .button("Set PIN", |s| {
            let (a, b) = (text_of(s, F_PIN1), text_of(s, F_PIN2));
            if a != b {
                dialogs::error(s, "PIN not set.", &"The two entries differ.");
                return;
            }
            let mut next = settings::current();
            if let Err(e) = next.set_pin(&a) {
                dialogs::error(s, "PIN not set.", &e);
                return;
            }
            store(s, next, "PIN set.");
        });
    if enabled {
        d = d.button("Remove PIN", |s| {
            let mut next = settings::current();
            next.clear_pin();
            store(s, next, "PIN removed.");
        });
    }
    s.add_layer(closable(d.dismiss_button(texts::BTN_CANCEL)));
}
