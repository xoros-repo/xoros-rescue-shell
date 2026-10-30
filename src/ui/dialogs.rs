//! Common dialogs: info, error (UI-5), confirm (UI-4), choice lists, text screens.
//! All sized to fit 80x24 (RT-4); `q` and `Esc` close them (UI-2).

use crate::ui::tk::Key;
use crate::ui::tk::traits::*;
use crate::ui::tk::views::{
    Dialog, EditView, LinearLayout, OnEventView, ScrollView, SelectView, TextView,
};
use crate::ui::tk::Ui;

use crate::texts;

/// Maximum content width/height inside a dialog on an 80x24 screen.
pub const MAX_W: usize = 74;
pub const MAX_H: usize = 16;

/// Make a view close on `q` and `Esc`.
pub fn closable<V: View>(v: V) -> OnEventView<V> {
    OnEventView::new(v)
        .on_event('q', |s| {
            s.pop_layer();
        })
        .on_event(Key::Esc, |s| {
            s.pop_layer();
        })
}

/// Scrollable, size-limited text.
pub fn scroll_text(text: impl Into<String>) -> impl View {
    ScrollView::new(TextView::new(text.into()))
        .max_width(MAX_W)
        .max_height(MAX_H)
}

pub fn info(s: &mut Ui, title: &str, text: &str) {
    s.add_layer(closable(
        Dialog::around(scroll_text(text))
            .title(title)
            .dismiss_button(texts::BTN_OK),
    ));
}

/// Error dialog with a short explanation and the underlying error text (UI-5).
pub fn error(s: &mut Ui, what: &str, err: &dyn std::fmt::Display) {
    log::error!("{what}: {err}");
    let text = format!("{what}\n\n{err}");
    s.add_layer(closable(
        Dialog::around(scroll_text(text))
            .title(texts::TITLE_ERROR)
            .dismiss_button(texts::BTN_OK),
    ));
}

pub fn not_implemented(s: &mut Ui, feature: &str) {
    info(
        s,
        texts::TITLE_NOT_IMPLEMENTED,
        &texts::not_implemented(feature),
    );
}

/// Confirmation dialog; `on_yes` runs after the dialog is closed.
pub fn confirm<F>(s: &mut Ui, title: &str, text: &str, yes_label: &str, on_yes: F)
where
    F: Fn(&mut Ui) + 'static + Send + Sync,
{
    s.add_layer(closable(
        Dialog::around(scroll_text(text))
            .title(title)
            .button(yes_label, move |s| {
                s.pop_layer();
                on_yes(s);
            })
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

/// Let the user pick one of `items`; `on_pick` runs after the dialog is closed.
pub fn choose<T, F>(s: &mut Ui, title: &str, text: &str, items: Vec<(String, T)>, on_pick: F)
where
    T: Send + Sync + 'static,
    F: Fn(&mut Ui, &T) + 'static + Send + Sync,
{
    let mut select = SelectView::<T>::new();
    for (label, value) in items {
        select.add_item(label, value);
    }
    let select = select.on_submit(move |s, v: &T| {
        s.pop_layer();
        on_pick(s, v);
    });
    let mut layout = LinearLayout::vertical();
    if !text.is_empty() {
        layout.add_child(TextView::new(text).max_width(MAX_W));
        layout.add_child(TextView::new(" "));
    }
    layout.add_child(ScrollView::new(select).max_height(10));
    s.add_layer(closable(
        Dialog::around(layout)
            .title(title)
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

/// A text screen with a Close button.
pub fn text_screen(title: &str, text: &str) -> impl View {
    closable(
        Dialog::around(scroll_text(text))
            .title(title)
            .dismiss_button(texts::BTN_CLOSE),
    )
}

/// Name of the input field of [`confirm_typed`].
const TYPED_INPUT: &str = "typed_confirm_input";

/// Confirmation by typing `word` (UI-4, RST-3). `on_ok` runs after the dialog is closed and gets
/// the typed text; a wrong word shows a hint and keeps the dialog open.
pub fn confirm_typed<F>(s: &mut Ui, title: &str, text: &str, word: &'static str, on_ok: F)
where
    F: Fn(&mut Ui, String) + 'static + Send + Sync,
{
    let on_ok = std::sync::Arc::new(on_ok);
    let submit = move |s: &mut Ui| {
        let typed = s
            .call_on_name(TYPED_INPUT, |v: &mut EditView| v.get_content().to_string())
            .unwrap_or_default();
        if typed.trim() == word {
            s.pop_layer();
            on_ok(s, typed);
        } else {
            info(
                s,
                texts::TITLE_CONFIRM,
                &format!("Type {word} (capital letters) to confirm."),
            );
        }
    };
    let submit2 = submit.clone();
    let layout = LinearLayout::vertical()
        .child(TextView::new(text).max_width(MAX_W))
        .child(TextView::new(" "))
        .child(TextView::new(format!("Type {word} to confirm:")))
        .child(
            EditView::new()
                .on_submit(move |s, _| submit2(s))
                .with_name(TYPED_INPUT)
                .fixed_width(20),
        );
    s.add_layer(
        OnEventView::new(
            Dialog::around(ScrollView::new(layout).max_height(MAX_H))
                .title(title)
                .button(texts::BTN_OK, submit)
                .dismiss_button(texts::BTN_CANCEL),
        )
        .on_event(Key::Esc, |s| {
            s.pop_layer();
        }),
    );
}

/// Remove the layer that contains the view named `name`, if it is still open.
pub fn remove_named_layer(s: &mut Ui, name: &str) {
    if let Some(pos) = s.find_layer_from_name(name) {
        s.remove_layer(pos);
    }
}
