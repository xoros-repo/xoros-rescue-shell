//! Terminal handling outside cursive: saved termios, panic hook (UI-5), subshell (SH-1).

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::thread::ThreadId;

use crate::constants::{SUBSHELL, SUBSHELL_PROMPT};

static STARTUP_TERMIOS: OnceLock<libc::termios> = OnceLock::new();
static MAIN_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// Escape sequences undoing what the crossterm backend sets up (alternate screen, hidden
/// cursor, mouse capture).
const LEAVE_TUI: &str =
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1015l\x1b[?1006l\x1b[0m\x1b[?25h\x1b[?1049l";
const ENTER_TUI: &str = "\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1015h\x1b[?1006h";

fn get_termios() -> Option<libc::termios> {
    // SAFETY: termios is a plain C struct; tcgetattr fills it for a valid fd.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd and pointer.
    let rc = unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) };
    if rc == 0 {
        Some(t)
    } else {
        None
    }
}

fn set_termios(t: &libc::termios) {
    // SAFETY: valid fd and pointer to an initialised termios.
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t);
    }
}

/// Remember the terminal state before the TUI starts and the main thread id.
pub fn save_startup_state() {
    if let Some(t) = get_termios() {
        let _ = STARTUP_TERMIOS.set(t);
    }
    let _ = MAIN_THREAD.set(std::thread::current().id());
}

/// Put the terminal back into the state from before the TUI started.
pub fn restore_terminal() {
    let mut out = std::io::stdout();
    let _ = out.write_all(LEAVE_TUI.as_bytes());
    let _ = out.flush();
    if let Some(t) = STARTUP_TERMIOS.get() {
        set_termios(t);
    }
}

/// Install the panic hook (UI-5): log the panic; if it happened on the UI thread, restore
/// the terminal first so the message is readable. Panics in worker threads are caught by
/// the task runner and must not tear down the running UI.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let on_main = MAIN_THREAD
            .get()
            .is_none_or(|id| *id == std::thread::current().id());
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        log::error!("panic in thread '{thread}': {info}");
        log::logger().flush();
        if on_main {
            restore_terminal();
            previous(info);
        }
    }));
}

/// Bytes a terminal sends for a key (VT/xterm sequences), for handing keys back to the tty.
pub fn key_bytes(key: &crossterm::event::KeyEvent) -> Vec<u8> {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if key.kind == KeyEventKind::Release {
        return Vec::new();
    }
    let csi = |s: &str| [b"\x1b[".as_slice(), s.as_bytes()].concat();
    let mut out = match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) && c.is_ascii() => {
            match c.to_ascii_lowercase() {
                c @ ('a'..='z' | '@' | '[' | '\\' | ']' | '^' | '_') => vec![(c as u8) & 0x1f],
                ' ' => vec![0],
                c => vec![c as u8],
            }
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => csi("Z"),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi("A"),
        KeyCode::Down => csi("B"),
        KeyCode::Right => csi("C"),
        KeyCode::Left => csi("D"),
        KeyCode::Home => csi("H"),
        KeyCode::End => csi("F"),
        KeyCode::Insert => csi("2~"),
        KeyCode::Delete => csi("3~"),
        KeyCode::PageUp => csi("5~"),
        KeyCode::PageDown => csi("6~"),
        _ => Vec::new(),
    };
    if key.modifiers.contains(KeyModifiers::ALT) && !out.is_empty() {
        out.insert(0, 0x1b);
    }
    out
}

/// SH-1: keys typed right after the Enter that started the subshell may already have been
/// read from the tty by crossterm's event reader (it reads everything available at once), so
/// the shell would never see them. Take them out of the reader.
fn take_buffered_keys() -> Vec<u8> {
    use crossterm::event::{poll, read, Event};
    let mut out = Vec::new();
    while let Ok(true) = poll(std::time::Duration::ZERO) {
        match read() {
            Ok(Event::Key(k)) => out.extend(key_bytes(&k)),
            Ok(Event::Paste(p)) => out.extend(p.into_bytes()),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

/// Put `bytes` back into the tty input queue (`TIOCSTI`; needs CAP_SYS_ADMIN on kernels with
/// `dev.tty.legacy_tiocsti=0`, which root in the rescue system has).
fn push_back_input(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    for b in bytes {
        // SAFETY: valid fd and a pointer to one byte.
        let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCSTI as _, b as *const u8) };
        if rc != 0 {
            log::warn!(
                "subshell: {} typed key byte(s) could not be passed on: {}",
                bytes.len(),
                std::io::Error::last_os_error()
            );
            return;
        }
    }
    log::info!("subshell: passed {} typed key byte(s) on to the shell", bytes.len());
}

/// Run an interactive login shell on the terminal (SH-1). The caller (UI) must redraw
/// afterwards. Returns the shell's exit status text.
pub fn run_subshell(run_dir: &Path) -> Result<String, String> {
    if crate::cmd::find_tool(SUBSHELL).is_none() {
        return Err(crate::cmd::ToolError::Missing(SUBSHELL.into()).to_string());
    }
    let tui_termios = get_termios();
    let typed = take_buffered_keys();
    restore_terminal();
    // ash/dash read $ENV after /etc/profile for interactive shells: set the prompt there so
    // the profile cannot override it.
    let rc = run_dir.join("subshell.rc");
    let rc_ok = std::fs::create_dir_all(run_dir)
        .and_then(|_| std::fs::write(&rc, format!("PS1='{SUBSHELL_PROMPT}'\nexport PS1\n")))
        .is_ok();
    println!("\nXorOS rescue subshell. Type 'exit' to return to the rescue shell.\n");
    // In cooked mode now, so the line discipline echoes and edits them like typed input.
    push_back_input(&typed);
    let mut cmd = Command::new(SUBSHELL);
    cmd.arg("-l").env("PS1", SUBSHELL_PROMPT);
    if rc_ok {
        cmd.env("ENV", &rc);
    }
    log::info!("subshell: started {SUBSHELL} -l");
    let status = cmd.status();
    let mut out = std::io::stdout();
    let _ = out.write_all(ENTER_TUI.as_bytes());
    let _ = out.flush();
    if let Some(t) = tui_termios {
        set_termios(&t);
    }
    match status {
        Ok(s) => {
            log::info!("subshell: exited with {s}");
            Ok(s.to_string())
        }
        Err(e) => {
            log::error!("subshell: cannot start: {e}");
            Err(format!("Cannot start {SUBSHELL}: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn keys_to_bytes() {
        let k = |c, m| key_bytes(&KeyEvent::new(c, m));
        let none = KeyModifiers::NONE;
        assert_eq!(k(KeyCode::Char('l'), none), b"l");
        assert_eq!(k(KeyCode::Char('ä'), none), "ä".as_bytes());
        assert_eq!(k(KeyCode::Char('c'), KeyModifiers::CONTROL), vec![3]);
        assert_eq!(k(KeyCode::Char('D'), KeyModifiers::CONTROL), vec![4]);
        assert_eq!(k(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
        assert_eq!(k(KeyCode::Enter, none), b"\r");
        assert_eq!(k(KeyCode::Backspace, none), vec![0x7f]);
        assert_eq!(k(KeyCode::Up, none), b"\x1b[A");
        assert_eq!(k(KeyCode::Delete, none), b"\x1b[3~");
        assert!(k(KeyCode::F(5), none).is_empty());
    }
}
