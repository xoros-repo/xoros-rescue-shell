//! Admin session in the TUI (SEC-1..7): the unlock dialog, the gate for admin-only menu entries,
//! lock/unlock and the admin password dialog. The policy is in [`crate::access`].

use std::sync::Arc;

use crate::ui::tk::traits::*;
use crate::ui::tk::views::{Dialog, EditView, LinearLayout, TextView};
use crate::ui::tk::Ui;

use crate::access::{self, UnlockError};
use crate::texts;
use crate::ui::dialogs::{self, closable, remove_named_layer, MAX_W};
use crate::ui::{app, update_status_line};

const PW_INPUT: &str = "admin_pw_input";
const PW_NEW1: &str = "admin_pw_new1";
const PW_NEW2: &str = "admin_pw_new2";
const CHECKING: &str = "admin_pw_checking";
const UNLOCK_DIALOG: &str = "admin_unlock_dialog";

/// Run `then` if admin actions are allowed (no protection or an open session), otherwise after
/// a successful unlock. `what` names the action for the log (SEC-1, SEC-3).
pub fn require<F>(s: &mut Ui, what: &str, then: F)
where
    F: Fn(&mut Ui) + Send + Sync + 'static,
{
    if access::is_admin() {
        access::touch();
        then(s);
        return;
    }
    if access::protection().is_locked() {
        log::warn!("SEC: refused (no valid admin password): {what}");
        dialogs::info(s, texts::TITLE_ADMIN, &UnlockError::Locked.to_string());
        return;
    }
    unlock_dialog(s, what, Arc::new(then));
}

/// A menu callback that runs `f` behind [`require`].
pub fn gate<F>(what: &'static str, f: F) -> impl Fn(&mut Ui) + 'static
where
    F: Fn(&mut Ui) + Clone + Send + Sync + 'static,
{
    move |s: &mut Ui| require(s, what, f.clone())
}

/// `[ADMIN]` / `[LOCKED]` for the status line, nothing when protection is off (SEC-2).
pub fn status_tag() -> Option<&'static str> {
    if !access::protection().is_on() {
        None
    } else if access::is_admin() {
        Some("[ADMIN]")
    } else {
        Some("[LOCKED]")
    }
}

fn read_field(s: &mut Ui, name: &str) -> String {
    s.call_on_name(name, |v: &mut EditView| v.get_content().to_string())
        .unwrap_or_default()
}

fn secret_field(label: &str, name: &str) -> impl View {
    LinearLayout::horizontal()
        .child(TextView::new(format!("{label:<12}")))
        .child(
            EditView::new()
                .secret()
                .max_content_width(*xoros_core::admin::PASSWORD_LEN.end())
                .with_name(name)
                .fixed_width(30),
        )
}

/// Run slow work (PBKDF2) in a thread with a "Checking..." note; `done` runs on the UI thread.
fn run_slow<T, W, D>(s: &mut Ui, note: &str, work: W, done: D)
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
    D: FnOnce(&mut Ui, T) + Send + 'static,
{
    s.add_layer(Dialog::around(TextView::new(note)).with_name(CHECKING));
    let sink = s.cb_sink().clone();
    let spawned = std::thread::Builder::new()
        .name("admin-password".into())
        .spawn(move || {
            let r = work();
            let _ = sink.send(Box::new(move |s: &mut Ui| {
                remove_named_layer(s, CHECKING);
                done(s, r);
            }));
        });
    if let Err(e) = spawned {
        remove_named_layer(s, CHECKING);
        dialogs::error(s, "Cannot start the password check.", &e);
    }
}

fn unlock_dialog(s: &mut Ui, what: &str, then: Arc<dyn Fn(&mut Ui) + Send + Sync>) {
    let what = what.to_string();
    let submit = move |s: &mut Ui| {
        let left = access::blocked_for();
        if left > 0 {
            dialogs::info(
                s,
                texts::TITLE_ADMIN,
                &UnlockError::Blocked(left).to_string(),
            );
            return;
        }
        let pw = read_field(s, PW_INPUT);
        s.call_on_name(PW_INPUT, |v: &mut EditView| v.set_content(""));
        let then = Arc::clone(&then);
        let what = what.clone();
        run_slow(
            s,
            "Checking the password...",
            move || access::unlock(&pw),
            move |s, r| {
                update_status_line(s);
                match r {
                    Ok(()) => {
                        remove_named_layer(s, UNLOCK_DIALOG);
                        log::info!("SEC: unlocked for: {what}");
                        then(s);
                    }
                    Err(e) => dialogs::info(s, texts::TITLE_ADMIN, &e.to_string()),
                }
            },
        );
    };
    let submit2 = submit.clone();
    let layout = LinearLayout::vertical()
        .child(TextView::new(texts::ADMIN_PROMPT).max_width(MAX_W))
        .child(TextView::new(" "))
        .child(
            LinearLayout::horizontal()
                .child(TextView::new(format!("{:<12}", "Password")))
                .child(
                    EditView::new()
                        .secret()
                        .max_content_width(*xoros_core::admin::PASSWORD_LEN.end())
                        .on_submit(move |s, _| submit2(s))
                        .with_name(PW_INPUT)
                        .fixed_width(30),
                ),
        );
    s.add_layer(
        closable(
            Dialog::around(layout)
                .title(texts::TITLE_ADMIN)
                .button(texts::BTN_OK, submit)
                .dismiss_button(texts::BTN_CANCEL),
        )
        .with_name(UNLOCK_DIALOG),
    );
}

/// Tools -> Unlock / Lock (SEC-2).
pub fn toggle_session(s: &mut Ui) {
    if !access::protection().is_on() {
        dialogs::info(s, texts::TITLE_ADMIN, texts::ADMIN_OFF_INFO);
    } else if access::is_admin() {
        access::lock();
        update_status_line(s);
        crate::ui::pop_to_base(s);
        dialogs::info(s, texts::TITLE_ADMIN, "Admin session locked.");
    } else {
        require(s, "unlock", |s| {
            dialogs::info(s, texts::TITLE_ADMIN, "Admin session unlocked.");
        });
    }
}

/// Menu label of [`toggle_session`] for the current state.
pub fn toggle_label() -> &'static str {
    if access::protection().is_on() && access::is_admin() {
        "Lock admin session"
    } else {
        "Unlock admin..."
    }
}

/// Tools -> Admin password (SEC-4): set/change or remove the device password.
pub fn password_menu(s: &mut Ui) {
    require(s, "admin password", password_dialog);
}

fn password_dialog(s: &mut Ui) {
    let protection = access::protection();
    let info = format!(
        "{}\n\nNow: {}.",
        texts::ADMIN_PASSWORD_INFO,
        protection.describe()
    );
    let layout = LinearLayout::vertical()
        .child(TextView::new(info).max_width(MAX_W))
        .child(TextView::new(" "))
        .child(secret_field("New", PW_NEW1))
        .child(secret_field("Repeat", PW_NEW2));
    let mut d = Dialog::around(layout)
        .title(texts::TITLE_ADMIN_PASSWORD)
        .button("Set password", set_clicked);
    if protection.name() == "device" {
        d = d.button("Remove device password", |s| {
            dialogs::confirm(
                s,
                texts::TITLE_CONFIRM,
                texts::ADMIN_REMOVE_CONFIRM,
                "Remove",
                |s| store(s, None),
            )
        });
    }
    s.add_layer(closable(d.dismiss_button(texts::BTN_CANCEL)));
}

fn set_clicked(s: &mut Ui) {
    let (a, b) = (read_field(s, PW_NEW1), read_field(s, PW_NEW2));
    if a != b {
        dialogs::error(s, "Password not set.", &"The two entries differ.");
        return;
    }
    if let Err(e) = xoros_core::admin::check_password_format(&a) {
        dialogs::error(s, "Password not set.", &e);
        return;
    }
    store(s, Some(a));
}

fn store(s: &mut Ui, password: Option<String>) {
    let Some(app) = app(s) else { return };
    run_slow(
        s,
        "Writing the admin password...",
        move || access::set_device_password(app.backend.as_ref(), password.as_deref()),
        |s, r| {
            update_status_line(s);
            match r {
                Ok(msg) => {
                    crate::ui::pop_to_base(s);
                    dialogs::info(s, texts::TITLE_ADMIN_PASSWORD, &msg);
                }
                Err(e) => dialogs::error(s, "The admin password was not changed.", &e),
            }
        },
    );
}
