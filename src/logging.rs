//! Application log (LOG-3): `<run dir>/rescue-shell.log`, stderr (journal) when stderr is not
//! the terminal the TUI draws on, and an in-memory ring buffer for the log viewer (LOG-1).

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
    level: log::LevelFilter,
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
    let logger = AppLogger {
        path: file.as_ref().map(|_| path.clone()),
        file: Mutex::new(file),
        ring: Mutex::new(VecDeque::new()),
        to_stderr: allow_stderr && !stderr_is_tty(),
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
    fn origin_marks_other_crates() {
        assert_eq!(super::origin("xoros_rescue_shell::backup_jobs"), "");
        assert_eq!(super::origin("xoros_core::device::mount"), "[xoros-core] ");
    }
}
