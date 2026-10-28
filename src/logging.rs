//! Application log (LOG-3): `<run dir>/rescue-shell.log`, stderr (journal) when stderr is not
//! the terminal the TUI draws on, and an in-memory ring buffer for the log viewer (LOG-1).
//! When stderr is not used (the TUI on tty1, CLI runs), records go to the journal through its
//! native socket instead, on the device only (not in simulation mode).

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::constants::{LOG_FILE_NAME, LOG_RING_LINES};
use crate::tasks::lock;

struct AppLogger {
    file: Mutex<Option<File>>,
    path: Option<PathBuf>,
    ring: Mutex<VecDeque<String>>,
    to_stderr: bool,
    /// The journal's native socket, if records go there.
    journal: Option<std::os::unix::net::UnixDatagram>,
    level: log::LevelFilter,
}

/// systemd-journald's native protocol socket.
const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";

/// One journal entry in the native protocol: `KEY=value` lines; a value with a newline is sent
/// as `KEY\n<64-bit little-endian length><value>\n`.
pub fn journal_datagram(level: log::Level, message: &str) -> Vec<u8> {
    let priority = match level {
        log::Level::Error => 3,
        log::Level::Warn => 4,
        log::Level::Info => 6,
        log::Level::Debug | log::Level::Trace => 7,
    };
    let mut d = format!("PRIORITY={priority}\nSYSLOG_IDENTIFIER=xoros-rescue-shell\n").into_bytes();
    if message.contains('\n') {
        d.extend_from_slice(b"MESSAGE\n");
        d.extend_from_slice(&(message.len() as u64).to_le_bytes());
        d.extend_from_slice(message.as_bytes());
        d.push(b'\n');
    } else {
        d.extend_from_slice(format!("MESSAGE={message}\n").as_bytes());
    }
    d
}

static LOGGER: OnceLock<AppLogger> = OnceLock::new();

fn stderr_is_tty() -> bool {
    // SAFETY: isatty only inspects the file descriptor.
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}

impl log::Log for AppLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ts = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S");
        let line = format!(
            "{ts} {:<5} {}{}",
            record.level(),
            origin(record.target()),
            record.args()
        );
        if let Some(f) = lock(&self.file).as_mut() {
            let _ = writeln!(f, "{line}");
        }
        if let Some(j) = &self.journal {
            let msg = format!("{}{}", origin(record.target()), record.args());
            let _ = j.send_to(&journal_datagram(record.level(), &msg), JOURNAL_SOCKET);
        }
        if self.to_stderr {
            let _ = writeln!(
                std::io::stderr(),
                "{:<5} {}{}",
                record.level(),
                origin(record.target()),
                record.args()
            );
        }
        let mut ring = lock(&self.ring);
        if ring.len() >= LOG_RING_LINES {
            ring.pop_front();
        }
        ring.push_back(line);
    }

    fn flush(&self) {
        if let Some(f) = lock(&self.file).as_mut() {
            let _ = f.flush();
        }
    }
}

/// Prefix for records from other crates (CORE-5: `xoros-core` logs go into the app log too,
/// marked `[xoros-core]`); empty for the app's own records.
fn origin(target: &str) -> String {
    let krate = target.split("::").next().unwrap_or(target);
    if krate == env!("CARGO_CRATE_NAME") {
        String::new()
    } else {
        format!("[{}] ", krate.replace('_', "-"))
    }
}

/// Install the logger. `run_dir` is created if needed; when the file cannot be opened,
/// logging continues in memory only. Returns the log file path if it is in use.
pub fn init(run_dir: &Path, level: log::LevelFilter, allow_stderr: bool) -> Option<PathBuf> {
    let path = run_dir.join(LOG_FILE_NAME);
    let file = fs::create_dir_all(run_dir)
        .and_then(|_| OpenOptions::new().create(true).append(true).open(&path))
        .ok();
    let to_stderr = allow_stderr && !stderr_is_tty();
    // On the device only (the run directory is the real one, not a simulation's).
    let journal = (!to_stderr
        && run_dir == Path::new(crate::constants::RUN_DIR)
        && Path::new(JOURNAL_SOCKET).exists())
    .then(std::os::unix::net::UnixDatagram::unbound)
    .and_then(Result::ok);
    let logger = AppLogger {
        path: file.as_ref().map(|_| path.clone()),
        file: Mutex::new(file),
        ring: Mutex::new(VecDeque::new()),
        to_stderr,
        journal,
        level,
    };
    if LOGGER.set(logger).is_err() {
        return log_file_path();
    }
    if let Some(l) = LOGGER.get() {
        if log::set_logger(l).is_ok() {
            log::set_max_level(level);
        }
    }
    log_file_path()
}

/// Path of the log file, if logging to a file.
pub fn log_file_path() -> Option<PathBuf> {
    LOGGER.get().and_then(|l| l.path.clone())
}

/// Recent log lines from memory (newest last).
pub fn recent_lines() -> Vec<String> {
    LOGGER
        .get()
        .map(|l| lock(&l.ring).iter().cloned().collect())
        .unwrap_or_default()
}

/// The last log line, for the status line (UI-7).
pub fn last_line() -> Option<String> {
    LOGGER.get().and_then(|l| lock(&l.ring).back().cloned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn journal_entries() {
        let d = super::journal_datagram(log::Level::Warn, "TST-8: overwriting /dev/sdb");
        assert_eq!(
            String::from_utf8(d).unwrap(),
            "PRIORITY=4\nSYSLOG_IDENTIFIER=xoros-rescue-shell\nMESSAGE=TST-8: overwriting /dev/sdb\n"
        );
        let d = super::journal_datagram(log::Level::Info, "a\nb");
        let head = b"PRIORITY=6\nSYSLOG_IDENTIFIER=xoros-rescue-shell\nMESSAGE\n";
        assert_eq!(&d[..head.len()], head);
        assert_eq!(&d[head.len()..head.len() + 8], &3u64.to_le_bytes());
        assert_eq!(&d[head.len() + 8..], b"a\nb\n");
    }

    #[test]
    fn origin_marks_other_crates() {
        assert_eq!(super::origin("xoros_rescue_shell::backup_jobs"), "");
        assert_eq!(super::origin("xoros_core::device::mount"), "[xoros-core] ");
    }
}
