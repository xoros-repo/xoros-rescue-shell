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

/// Run an interactive login shell on the terminal (SH-1). The caller (UI) must redraw
/// afterwards. Returns the shell's exit status text.
pub fn run_subshell(run_dir: &Path) -> Result<String, String> {
    if crate::cmd::find_tool(SUBSHELL).is_none() {
        return Err(crate::cmd::ToolError::Missing(SUBSHELL.into()).to_string());
    }
    let tui_termios = get_termios();
    restore_terminal();
    // ash/dash read $ENV after /etc/profile for interactive shells: set the prompt there so
    // the profile cannot override it.
    let rc = run_dir.join("subshell.rc");
    let rc_ok = std::fs::create_dir_all(run_dir)
        .and_then(|_| std::fs::write(&rc, format!("PS1='{SUBSHELL_PROMPT}'\nexport PS1\n")))
        .is_ok();
    println!("\nXorOS rescue subshell. Type 'exit' to return to the rescue shell.\n");
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
