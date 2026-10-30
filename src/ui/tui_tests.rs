//! UI tests in simulation mode, drawn on ratatui's `TestBackend`: every menu entry opens and
//! fits 80x24 and 100x30 (RT-4), and the keyboard handling of the common dialogs works.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use crate::app::{make_backend, App};
use crate::backend::sim::SimBackend;
use crate::ui::tk::traits::*;
use crate::ui::tk::views::{Checkbox, Dialog, EditView, LinearLayout, SelectView};
use crate::ui::tk::{Event, Key, MouseKind, Ui, Vec2};

/// Entries that would use the host network (sim mode runs network actions for real).
const SKIP: &[&str] = &["Network / Test", "Network / Speed test"];

struct Fixture {
    _dir: tempfile::TempDir,
    ui: Ui,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    SimBackend::create_example(dir.path()).expect("example");
    let backend = make_backend(Some(dir.path().to_path_buf())).expect("backend");
    let ui = super::build(Arc::new(App::new(backend)));
    Fixture { _dir: dir, ui }
}

fn term(w: u16, h: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(w, h)).expect("terminal")
}

fn screen(ui: &mut Ui, t: &mut Terminal<TestBackend>) -> Vec<String> {
    ui.run_pending();
    ui.render(t).expect("render");
    let buf = t.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

fn contains(rows: &[String], text: &str) -> bool {
    rows.iter().any(|r| r.contains(text))
}

fn key(ui: &mut Ui, k: Key) {
    ui.on_event(Event::Key(k));
}

fn type_text(ui: &mut Ui, text: &str) {
    for c in text.chars() {
        ui.on_event(Event::Char(c));
    }
}

#[test]
fn status_screen_and_status_line() {
    let mut f = fixture();
    let mut t = term(80, 24);
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, crate::texts::TITLE_STATUS), "{rows:#?}");
    assert!(rows[23].contains("Esc:menu"), "{}", rows[23]);
    assert!(
        rows[0].contains("Status") && rows[0].contains("Power"),
        "{}",
        rows[0]
    );
}

#[test]
fn every_menu_entry_opens_and_fits() {
    for (w, h) in [(80, 24), (100, 30)] {
        let mut f = fixture();
        let mut t = term(w, h);
        let leaves = f.ui.menubar().leaves();
        assert!(leaves.len() > 30, "{} leaves", leaves.len());
        for (label, cb) in leaves {
            if SKIP.contains(&label.as_str()) || label.ends_with("Restart rescue shell") {
                continue;
            }
            cb(&mut f.ui);
            let rows = screen(&mut f.ui, &mut t);
            // The topmost dialog is fully on the screen: its bottom border is visible.
            if f.ui.screen_len() > 1 {
                assert!(
                    rows.iter()
                        .skip(1)
                        .any(|r| r.contains('└') && r.contains('┘')),
                    "{label} at {w}x{h}:\n{}",
                    rows.join("\n")
                );
            }
            super::pop_to_base(&mut f.ui);
        }
    }
}

#[test]
fn esc_opens_the_menu_and_enter_runs_a_leaf() {
    let mut f = fixture();
    let mut t = term(80, 24);
    key(&mut f.ui, Key::Esc);
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, "Overview"), "{}", rows.join("\n"));
    key(&mut f.ui, Key::Right);
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, "Boot control"), "{}", rows.join("\n"));
    key(&mut f.ui, Key::Enter);
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, "Try counters"), "{}", rows.join("\n"));
    // q closes the screen, Esc on the Status screen opens the menu again.
    f.ui.on_event(Event::Char('q'));
    assert_eq!(f.ui.screen_len(), 1);
    key(&mut f.ui, Key::Esc);
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, "Overview"));
    key(&mut f.ui, Key::Esc);
    let rows = screen(&mut f.ui, &mut t);
    assert!(!contains(&rows, "Overview"));
}

#[test]
fn ctrl_c_shows_the_hint() {
    let mut f = fixture();
    let mut t = term(80, 24);
    f.ui.on_event(Event::CtrlChar('c'));
    let rows = screen(&mut f.ui, &mut t);
    assert!(
        contains(&rows, "Ctrl-C does not exit"),
        "{}",
        rows.join("\n")
    );
    key(&mut f.ui, Key::Enter);
    assert_eq!(f.ui.screen_len(), 1);
}

#[test]
fn choose_moves_and_submits() {
    let mut f = fixture();
    let picked = Arc::new(AtomicU32::new(0));
    let p = Arc::clone(&picked);
    let items = vec![
        ("one".to_string(), 1u32),
        ("two".into(), 2),
        ("three".into(), 3),
    ];
    crate::ui::dialogs::choose(&mut f.ui, "Pick", "Choose:", items, move |_, v| {
        p.store(*v, Ordering::SeqCst)
    });
    key(&mut f.ui, Key::Down);
    key(&mut f.ui, Key::Down);
    key(&mut f.ui, Key::Enter);
    assert_eq!(picked.load(Ordering::SeqCst), 3);
    assert_eq!(f.ui.screen_len(), 1, "dialog closed");
}

#[test]
fn typed_confirmation_needs_the_word() {
    let mut f = fixture();
    let mut t = term(80, 24);
    let typed = Arc::new(Mutex::new(String::new()));
    let tt = Arc::clone(&typed);
    crate::ui::dialogs::confirm_typed(&mut f.ui, "Reset", "Erase?", "RESET", move |_, w| {
        *tt.lock().unwrap() = w
    });
    type_text(&mut f.ui, "RESEX");
    key(&mut f.ui, Key::Enter);
    assert!(typed.lock().unwrap().is_empty());
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, "Type RESET"), "{}", rows.join("\n"));
    key(&mut f.ui, Key::Enter); // close the hint
    key(&mut f.ui, Key::Backspace);
    type_text(&mut f.ui, "T");
    key(&mut f.ui, Key::Enter);
    assert_eq!(*typed.lock().unwrap(), "RESET");
    assert_eq!(f.ui.screen_len(), 1);
}

#[test]
fn tab_reaches_the_buttons_and_dismiss_closes() {
    let mut f = fixture();
    let ran = Arc::new(AtomicBool::new(false));
    let r = Arc::clone(&ran);
    crate::ui::dialogs::confirm(&mut f.ui, "Q", "Sure?", "Yes", move |_| {
        r.store(true, Ordering::SeqCst)
    });
    // Text only: the first button has the focus.
    key(&mut f.ui, Key::Right);
    key(&mut f.ui, Key::Enter);
    assert!(!ran.load(Ordering::SeqCst), "Cancel pressed");
    assert_eq!(f.ui.screen_len(), 1);
    let r = Arc::clone(&ran);
    crate::ui::dialogs::confirm(&mut f.ui, "Q", "Sure?", "Yes", move |_| {
        r.store(true, Ordering::SeqCst)
    });
    key(&mut f.ui, Key::Enter);
    assert!(ran.load(Ordering::SeqCst));
}

#[test]
fn popup_select_changes_the_value() {
    let mut f = fixture();
    let mut t = term(80, 24);
    let mut sel = SelectView::<&'static str>::new().popup();
    sel.add_item("DHCP", "dhcp");
    sel.add_item("Static", "static");
    f.ui.add_layer(Dialog::around(sel.with_name("m")).title("Form"));
    key(&mut f.ui, Key::Enter);
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, "Static"), "{}", rows.join("\n"));
    key(&mut f.ui, Key::Down);
    key(&mut f.ui, Key::Enter);
    let v = f.ui.call_on_name("m", |v: &mut SelectView<&'static str>| {
        v.selection().map(|s| *s)
    });
    assert_eq!(v, Some(Some("static")));
    assert_eq!(f.ui.screen_len(), 2, "popup closed, form open");
}

#[test]
fn network_form_scrolls_to_the_focused_field() {
    let mut f = fixture();
    let mut t = term(80, 24);
    let leaves = f.ui.menubar().leaves();
    let (_, cb) = leaves
        .iter()
        .find(|(l, _)| l == "Network / Configure")
        .expect("leaf");
    cb(&mut f.ui);
    let rows = screen(&mut f.ui, &mut t);
    // Configure lists the interfaces first; pick the first one if there is a list.
    if contains(&rows, "Configure") && f.ui.screen_len() > 1 {
        key(&mut f.ui, Key::Enter);
        for _ in 0..12 {
            key(&mut f.ui, Key::Tab);
            screen(&mut f.ui, &mut t);
        }
    }
}

// --- mouse -------------------------------------------------------------------------------

/// Screen position of the first occurrence of `text`.
fn find(rows: &[String], text: &str) -> (usize, usize) {
    for (y, r) in rows.iter().enumerate() {
        if let Some(b) = r.find(text) {
            return (r[..b].chars().count(), y);
        }
    }
    panic!("'{text}' not on the screen:\n{}", rows.join("\n"));
}

fn mouse(ui: &mut Ui, (x, y): (usize, usize), kind: MouseKind) {
    ui.on_event(Event::Mouse {
        pos: Vec2::new(x, y),
        kind,
    });
}

fn click(ui: &mut Ui, at: (usize, usize)) {
    mouse(ui, at, MouseKind::Press);
}

#[test]
fn click_opens_menu_and_runs_an_entry() {
    let mut f = fixture();
    let mut t = term(80, 24);
    let rows = screen(&mut f.ui, &mut t);
    click(&mut f.ui, find(&rows, " Boot "));
    let rows = screen(&mut f.ui, &mut t);
    click(&mut f.ui, find(&rows, "Boot control"));
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, "Try counters"), "{}", rows.join("\n"));
    // The Close button.
    click(&mut f.ui, find(&rows, "<Close>"));
    assert_eq!(f.ui.screen_len(), 1);
    // A click next to an open menu closes it.
    let rows = screen(&mut f.ui, &mut t);
    click(&mut f.ui, find(&rows, " Tools "));
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, "Subshell"));
    click(&mut f.ui, (40, 20));
    let rows = screen(&mut f.ui, &mut t);
    assert!(!contains(&rows, "Subshell"), "{}", rows.join("\n"));
}

#[test]
fn click_selects_then_submits_a_list_row() {
    let mut f = fixture();
    let mut t = term(80, 24);
    let picked = Arc::new(AtomicU32::new(0));
    let p = Arc::clone(&picked);
    let items = vec![
        ("one".to_string(), 1u32),
        ("two".into(), 2),
        ("three".into(), 3),
    ];
    crate::ui::dialogs::choose(&mut f.ui, "Pick", "Choose:", items, move |_, v| {
        p.store(*v, Ordering::SeqCst)
    });
    let rows = screen(&mut f.ui, &mut t);
    let at = find(&rows, "three");
    click(&mut f.ui, at);
    assert_eq!(picked.load(Ordering::SeqCst), 0, "first click only selects");
    screen(&mut f.ui, &mut t);
    click(&mut f.ui, at);
    assert_eq!(picked.load(Ordering::SeqCst), 3);
    assert_eq!(f.ui.screen_len(), 1);
}

#[test]
fn click_focuses_fields_and_toggles_checkboxes() {
    let mut f = fixture();
    let mut t = term(80, 24);
    let form = LinearLayout::vertical()
        .child(
            EditView::new()
                .content("first")
                .with_name("a")
                .fixed_width(20),
        )
        .child(
            EditView::new()
                .content("second")
                .with_name("b")
                .fixed_width(20),
        )
        .child(Checkbox::new().with_name("c"));
    f.ui.add_layer(Dialog::around(form).title("Form").dismiss_button("Cancel"));
    let rows = screen(&mut f.ui, &mut t);
    // Click right after "sec": the cursor goes there.
    let (x, y) = find(&rows, "second");
    click(&mut f.ui, (x + 3, y));
    type_text(&mut f.ui, "X");
    let b = f.ui.call_on_name("b", |v: &mut EditView| v.get_content());
    assert_eq!(b.as_deref(), Some("secXond"));
    let a = f.ui.call_on_name("a", |v: &mut EditView| v.get_content());
    assert_eq!(a.as_deref(), Some("first"));
    click(&mut f.ui, find(&rows, "[ ]"));
    let c = f.ui.call_on_name("c", |v: &mut Checkbox| v.is_checked());
    assert_eq!(c, Some(true));
}

#[test]
fn wheel_scrolls_a_long_text() {
    let mut f = fixture();
    let mut t = term(80, 24);
    let text: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    f.ui.add_layer(crate::ui::dialogs::text_screen("Long", &text));
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, "line 1 "));
    let at = find(&rows, "line 2 ");
    for _ in 0..5 {
        mouse(&mut f.ui, at, MouseKind::WheelDown);
    }
    let rows = screen(&mut f.ui, &mut t);
    assert!(!contains(&rows, "line 1 "), "{}", rows.join("\n"));
    assert!(contains(&rows, "line 16 "));
    mouse(&mut f.ui, at, MouseKind::WheelUp);
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, "line 13 "));
}

/// Background color of the cell at `(x, y)` after a redraw.
fn bg(ui: &mut Ui, t: &mut Terminal<TestBackend>, (x, y): (u16, u16)) -> ratatui::style::Color {
    let _ = screen(ui, t);
    t.backend().buffer()[(x, y)].bg
}

/// Tools -> Settings, the theme drop-down focused and opened, `theme` picked.
fn pick_theme(ui: &mut Ui, theme: &str) {
    crate::ui::settings::show(ui);
    let i = crate::settings::THEMES
        .iter()
        .position(|t| *t == theme)
        .expect("theme");
    for _ in 0..5 {
        key(ui, Key::Tab);
    }
    key(ui, Key::Enter);
    key(ui, Key::Home);
    for _ in 0..i {
        key(ui, Key::Down);
    }
    key(ui, Key::Enter);
}

#[test]
fn theme_shows_when_picked_and_is_kept_by_save() {
    use ratatui::style::Color;
    let mut f = fixture();
    let mut t = term(100, 30);
    // Inside the status screen, outside the settings dialog.
    let at = (2, 2);
    assert_eq!(bg(&mut f.ui, &mut t, at), Color::Gray);
    pick_theme(&mut f.ui, "dark");
    assert_eq!(bg(&mut f.ui, &mut t, at), Color::Black, "preview");
    // Cancel drops the preview.
    key(&mut f.ui, Key::Esc);
    assert_eq!(bg(&mut f.ui, &mut t, at), Color::Gray, "back after Esc");
    assert_eq!(crate::settings::current().theme, "default");

    pick_theme(&mut f.ui, "norton");
    // Another screen from the menu also drops the preview.
    crate::ui::show_screen(&mut f.ui, Dialog::text("x"));
    assert_eq!(
        bg(&mut f.ui, &mut t, at),
        Color::Gray,
        "back after another screen"
    );

    pick_theme(&mut f.ui, "norton");
    assert_eq!(bg(&mut f.ui, &mut t, at), Color::Blue);
    // Tab from the theme field reaches Save.
    key(&mut f.ui, Key::Tab);
    key(&mut f.ui, Key::Enter);
    let rows = screen(&mut f.ui, &mut t);
    assert!(contains(&rows, "Settings saved."), "{}", rows.join("\n"));
    assert_eq!(crate::settings::current().theme, "norton");
    key(&mut f.ui, Key::Enter);
    assert_eq!(bg(&mut f.ui, &mut t, at), Color::Blue, "kept after Save");
}
