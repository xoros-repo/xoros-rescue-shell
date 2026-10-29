//! Date/time information for the status screen (SYS-1). Time sync (NET-7) comes later.

use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClockInfo {
    /// Current UTC time, `YYYY-MM-DD HH:MM:SS UTC`.
    pub utc: String,
    /// Kernel NTP state: `Some(true)` synchronized, `Some(false)` not, `None` unknown.
    pub synchronized: Option<bool>,
}

/// Whether the kernel clock is marked synchronized (`adjtimex`, STA_UNSYNC clear).
#[cfg(target_os = "linux")]
pub fn kernel_clock_synchronized() -> Option<bool> {
    // SAFETY: timex is a plain C struct; zeroed means modes = 0 (read only).
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer to a properly sized struct.
    let rc = unsafe { libc::adjtimex(&mut tx) };
    if rc < 0 {
        return None;
    }
    Some(rc != libc::TIME_ERROR && (tx.status & libc::STA_UNSYNC) == 0)
}

#[cfg(not(target_os = "linux"))]
pub fn kernel_clock_synchronized() -> Option<bool> {
    None
}

pub fn clock_info() -> ClockInfo {
    ClockInfo {
        utc: chrono::Utc::now()
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string(),
        synchronized: kernel_clock_synchronized(),
    }
}

/// Format a 14-digit build timestamp `YYYYMMDDHHMMSS` as `YYYY-MM-DD HH:MM:SS`.
/// Other input is returned unchanged (display only; validation is `xoros_core`'s, XC-23).
pub fn format_build_timestamp(ts: &str) -> String {
    if ts.len() == 14 && ts.bytes().all(|b| b.is_ascii_digit()) {
        format!(
            "{}-{}-{} {}:{}:{}",
            &ts[0..4],
            &ts[4..6],
            &ts[6..8],
            &ts[8..10],
            &ts[10..12],
            &ts[12..14]
        )
    } else {
        ts.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_timestamp() {
        assert_eq!(
            format_build_timestamp("20261026214512"),
            "2026-10-26 21:45:12"
        );
        assert_eq!(format_build_timestamp("bad"), "bad");
    }

    #[test]
    fn clock_works() {
        let c = clock_info();
        assert!(c.utc.ends_with("UTC"));
    }
}
