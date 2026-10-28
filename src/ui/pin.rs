//! PIN prompt before destructive actions and the subshell (CFG-2).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cursive::traits::*;
use cursive::views::{Dialog, EditView, LinearLayout, TextView};
use cursive::Cursive;

use crate::texts;
use crate::ui::dialogs::{self, closable};

const PIN_INPUT: &str = "pin_input";
/// Wrong entries before a lockout, and its length.
const MAX_FAILURES: u32 = 3;
const LOCKOUT: Duration = Duration::from_secs(60);

/// Wrong entries in a row and the end of the current lockout.
static FAILURES: Mutex<(u32, Option<Instant>)> = Mutex::new((0, None));

/// Run `then` if no PIN is set or after the user entered the right PIN. `what` names the
/// action for the log.
pub fn require<F>(s: &mut Cursive, what: &str, then: F)
where
    F: Fn(&mut Cursive) + Send + Sync + 'static,
{
    let settings = crate::settings::current();
    if !settings.pin_enabled() {
        then(s);
        return;
    }
    let then = Arc::new(then);
    let what = what.to_string();
    let check = move |s: &mut Cursive| {
        let pin = s
            .call_on_name(PIN_INPUT, |v: &mut EditView| v.get_content().to_string())
            .unwrap_or_default();
        let mut f = crate::tasks::lock(&FAILURES);
        if let Some(until) = f.1 {
            if Instant::now() < until {
                drop(f);
                dialogs::info(s, texts::TITLE_PIN, texts::PIN_LOCKED);
                return;
            }
            *f = (0, None);
        }
        if crate::settings::current().pin_ok(pin.trim()) {
            *f = (0, None);
            drop(f);
            log::info!("PIN accepted for: {what}");
            s.pop_layer();
            then(s);
        } else {
            f.0 += 1;
            if f.0 >= MAX_FAILURES {
                f.1 = Some(Instant::now() + LOCKOUT);
            }
            drop(f);
            log::warn!("wrong PIN for: {what}");
            s.call_on_name(PIN_INPUT, |v: &mut EditView| v.set_content(""));
            dialogs::info(s, texts::TITLE_PIN, "Wrong PIN.");
        }
    };
    let check2 = check.clone();
    s.add_layer(closable(
        Dialog::around(
            LinearLayout::vertical()
                .child(TextView::new(texts::PIN_PROMPT))
                .child(
                    EditView::new()
                        .secret()
                        .max_content_width(*crate::settings::PIN_LEN.end())
                        .on_submit(move |s, _| check2(s))
                        .with_name(PIN_INPUT)
                        .fixed_width(16),
                ),
        )
        .title(texts::TITLE_PIN)
        .button(texts::BTN_OK, check)
        .dismiss_button(texts::BTN_CANCEL),
    ));
}
