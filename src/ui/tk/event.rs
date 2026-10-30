//! Key and mouse events of the UI, converted from crossterm. The Linux VT has no mouse
//! (RT-4); terminal emulators (serial console tools, SSH, development) do.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::Vec2;

/// Keys without a character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Esc,
    Tab,
    Backspace,
    Del,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A printable character.
    Char(char),
    /// Ctrl + a letter (lower case).
    CtrlChar(char),
    Key(Key),
    /// Shift + key (Shift-Tab).
    Shift(Key),
    /// Mouse at `pos`, relative to the view that gets the event.
    Mouse {
        pos: Vec2,
        kind: MouseKind,
    },
    /// Anything else (function keys, Alt combinations).
    Unknown,
}

/// The mouse actions the UI uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    /// Left button pressed.
    Press,
    WheelUp,
    WheelDown,
}

impl From<char> for Event {
    fn from(c: char) -> Self {
        Event::Char(c)
    }
}

impl From<Key> for Event {
    fn from(k: Key) -> Self {
        Event::Key(k)
    }
}

impl Event {
    pub fn from_key(k: &KeyEvent) -> Event {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let key = |key| {
            if k.modifiers.contains(KeyModifiers::SHIFT) {
                Event::Shift(key)
            } else {
                Event::Key(key)
            }
        };
        match k.code {
            KeyCode::Char(c) if ctrl => Event::CtrlChar(c.to_ascii_lowercase()),
            KeyCode::Char(_) if alt => Event::Unknown,
            KeyCode::Char(c) => Event::Char(c),
            KeyCode::Enter => key(Key::Enter),
            KeyCode::Esc => key(Key::Esc),
            KeyCode::Tab => key(Key::Tab),
            KeyCode::BackTab => Event::Shift(Key::Tab),
            KeyCode::Backspace => key(Key::Backspace),
            KeyCode::Delete => key(Key::Del),
            KeyCode::Left => key(Key::Left),
            KeyCode::Right => key(Key::Right),
            KeyCode::Up => key(Key::Up),
            KeyCode::Down => key(Key::Down),
            KeyCode::Home => key(Key::Home),
            KeyCode::End => key(Key::End),
            KeyCode::PageUp => key(Key::PageUp),
            KeyCode::PageDown => key(Key::PageDown),
            _ => Event::Unknown,
        }
    }

    /// A mouse event at an absolute screen position; `None` for what the UI does not use
    /// (moves, releases, other buttons), so these do not cause a redraw.
    pub fn from_mouse(m: &MouseEvent) -> Option<Event> {
        let kind = match m.kind {
            MouseEventKind::Down(MouseButton::Left) => MouseKind::Press,
            MouseEventKind::ScrollUp => MouseKind::WheelUp,
            MouseEventKind::ScrollDown => MouseKind::WheelDown,
            _ => return None,
        };
        let pos = Vec2::new(m.column.into(), m.row.into());
        Some(Event::Mouse { pos, kind })
    }

    /// The mouse position and action, if this is a mouse event.
    pub fn mouse(&self) -> Option<(Vec2, MouseKind)> {
        match self {
            Event::Mouse { pos, kind } => Some((*pos, *kind)),
            _ => None,
        }
    }

    /// The same mouse event relative to a child view at `offset`; `None` when the position
    /// is not inside the child (`size`) or this is not a mouse event.
    pub fn mouse_in(&self, offset: Vec2, size: Vec2) -> Option<Event> {
        let (pos, kind) = self.mouse()?;
        if pos.x < offset.x || pos.y < offset.y {
            return None;
        }
        let rel = pos.saturating_sub(offset);
        (rel.x < size.x && rel.y < size.y).then_some(Event::Mouse { pos: rel, kind })
    }

    /// Enter, also with Shift (some terminals report Shift-Enter for keypad Enter).
    pub fn is_enter(&self) -> bool {
        matches!(self, Event::Key(Key::Enter) | Event::Shift(Key::Enter))
    }
}
