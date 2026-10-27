//! Log sources for the log viewer (LOG-1).

use crate::cmd::run_capture;
use crate::constants::LOG_MAX_LINES;

/// A log the user can view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSource {
    /// The app's own log (LOG-3).
    App,
    /// `journalctl -b` of the current boot.
    Journal,
    /// Kernel log (`dmesg`).
    Kernel,
}

impl LogSource {
    pub const ALL: [LogSource; 3] = [LogSource::App, LogSource::Journal, LogSource::Kernel];

    pub fn title(self) -> &'static str {
        match self {
            LogSource::App => "App log",
            LogSource::Journal => "System journal (this boot)",
            LogSource::Kernel => "Kernel log (dmesg)",
        }
    }
}

/// Keep only the last `max` lines.
pub fn tail(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max);
    lines[start..].join("\n")
}

/// Read a log. Errors (missing tool, no permission) come back as text for the viewer.
pub fn read_log(source: LogSource) -> Result<String, String> {
    match source {
        LogSource::App => {
            if let Some(p) = crate::logging::log_file_path() {
                if let Ok(t) = std::fs::read_to_string(&p) {
                    return Ok(tail(&t, LOG_MAX_LINES));
                }
            }
            Ok(crate::logging::recent_lines().join("\n"))
        }
        LogSource::Journal => run_capture("journalctl", &["-b", "--no-pager", "-o", "short-iso"])
            .map(|t| tail(&t, LOG_MAX_LINES))
            .map_err(|e| e.to_string()),
        LogSource::Kernel => run_capture("dmesg", &[])
            .map(|t| tail(&t, LOG_MAX_LINES))
            .map_err(|e| e.to_string()),
    }
}

/// Lines containing `query` (case-insensitive), with 1-based line numbers.
pub fn search(text: &str, query: &str) -> Vec<(usize, String)> {
    let q = query.to_lowercase();
    text.lines()
        .enumerate()
        .filter(|(_, l)| l.to_lowercase().contains(&q))
        .map(|(i, l)| (i + 1, l.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_and_search() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("a", 5), "a");
        let r = search("Error one\nok\nanother ERROR", "error");
        assert_eq!(
            r,
            vec![(1, "Error one".into()), (3, "another ERROR".into())]
        );
        assert!(search("x", "y").is_empty());
    }

    #[test]
    fn app_log_readable() {
        assert!(read_log(LogSource::App).is_ok());
    }
}
