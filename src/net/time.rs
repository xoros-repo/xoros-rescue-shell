//! Time: SNTP sync, manual setting and the plausibility check before HTTPS (NET-7, OTA-10).

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{NaiveDateTime, TimeZone, Utc};

use super::config::dns_servers;
use super::{dns, sntp, NetEnv};
use std::path::Path;

use crate::constants::{BUILD_EPOCH, RTC_DEVICE};

/// Build time of the app; the clock must not be earlier (NET-7).
pub fn build_time() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(BUILD_EPOCH.parse().unwrap_or(0))
}

/// `true` if `now` is not before the app's build date (one day of slack for time zones of the
/// build host).
pub fn clock_plausible_at(now: SystemTime, build: SystemTime) -> bool {
    now + Duration::from_secs(86_400) >= build
}

pub fn clock_plausible() -> bool {
    clock_plausible_at(SystemTime::now(), build_time())
}

pub fn format_utc(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    Utc.timestamp_opt(d.as_secs() as i64, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "?".into())
}

/// Parse a manual entry `YYYY-MM-DD HH:MM[:SS]` (UTC).
pub fn parse_manual(text: &str) -> Result<SystemTime, String> {
    let t = text.trim().trim_end_matches("UTC").trim();
    let dt = NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M"))
        .map_err(|_| format!("Invalid date/time '{text}'. Use YYYY-MM-DD HH:MM[:SS] (UTC)."))?;
    let secs = dt.and_utc().timestamp();
    if secs < 0 {
        return Err(format!("Date/time '{text}' is before 1970."));
    }
    Ok(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Set the system clock (`clock_settime(CLOCK_REALTIME)`); only logged in simulation mode.
pub fn set_system_time(env: &NetEnv, t: SystemTime) -> Result<(), String> {
    if env.simulation {
        log::info!("SIM: would set the clock to {}", format_utc(t));
        return Ok(());
    }
    let d = t
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "time before 1970".to_string())?;
    let ts = libc::timespec {
        tv_sec: d.as_secs() as libc::time_t,
        tv_nsec: d.subsec_nanos() as _,
    };
    // SAFETY: valid pointer to an initialized timespec.
    let rc = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        return Err(format!("Cannot set the clock: {e}"));
    }
    log::warn!("clock set to {}", format_utc(t));
    Ok(())
}

/// Manual date/time entry (NET-7): set the system clock and the RTC. Returns the text for the
/// user.
pub fn set_manual(env: &NetEnv, t: SystemTime) -> Result<String, String> {
    set_system_time(env, t)?;
    let mut msg = format!("Clock set to {}.", format_utc(t));
    if let Some(n) = crate::rtc::store(env.simulation, Path::new(RTC_DEVICE), t) {
        msg = format!("{msg} {n}");
    }
    Ok(msg)
}

/// The hardware clock for display: `None` when there is none.
pub fn rtc_text() -> Option<String> {
    let dev = Path::new(RTC_DEVICE);
    if !dev.exists() {
        return None;
    }
    Some(match crate::rtc::read(dev) {
        Ok(t) => format!("{} ({})", format_utc(t), dev.display()),
        Err(e) => e,
    })
}

/// Result of [`sync`].
#[derive(Clone, Debug, PartialEq)]
pub struct SyncReport {
    pub server: String,
    pub address: SocketAddr,
    pub offset: f64,
    pub new_time: SystemTime,
    /// The clock was changed (offset above 0.5 s).
    pub changed: bool,
    /// What happened to the hardware clock (`None`: no RTC).
    pub rtc_note: Option<String>,
}

impl SyncReport {
    pub fn text(&self) -> String {
        format!(
            "{} ({}): offset {:+.3} s. Time is now {}{}.",
            self.server,
            self.address.ip(),
            self.offset,
            format_utc(self.new_time),
            if self.changed { "" } else { " (unchanged)" }
        ) + &self.rtc_note.as_ref().map(|n| format!(" {n}")).unwrap_or_default()
    }
}

/// `host` or `host:port`.
fn server_addrs(env: &NetEnv, server: &str) -> Result<Vec<SocketAddr>, String> {
    let (host, port) = match dns::split_netloc(server) {
        Ok((h, p)) if !h.contains(':') || h.starts_with('[') => (h, p),
        _ => (server, sntp::NTP_PORT),
    };
    let ips = dns::resolve(host, &dns_servers(env), Duration::from_secs(3))
        .map_err(|e| format!("{host}: {e}"))?;
    Ok(ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect())
}

/// NET-7: query the servers in order (default: `ntp_servers` of the settings, CFG-1) and set
/// the clock from the first answer.
pub fn sync(env: &NetEnv, servers: &[String]) -> Result<SyncReport, String> {
    let default = crate::settings::current().ntp_servers();
    let servers = if servers.is_empty() { &default } else { servers };
    let mut errors = Vec::new();
    for server in servers {
        let addrs = match server_addrs(env, server) {
            Ok(a) => a,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        for addr in addrs.into_iter().take(3) {
            match sntp::query(addr, Duration::from_secs(3)) {
                Ok(r) => {
                    let new_time = r.corrected(SystemTime::now());
                    let changed = r.offset.abs() > 0.5;
                    if changed {
                        set_system_time(env, new_time)?;
                    }
                    log::info!("SNTP {server} ({addr}): offset {:+.3} s", r.offset);
                    return Ok(SyncReport {
                        server: server.clone(),
                        address: addr,
                        offset: r.offset,
                        new_time,
                        changed,
                        rtc_note: crate::rtc::store(env.simulation, Path::new(RTC_DEVICE), new_time),
                    });
                }
                Err(e) => errors.push(format!("{server} ({addr}): {e}")),
            }
        }
    }
    Err(format!("Time sync failed: {}", errors.join("; ")))
}

/// OTA-10 / NET-7: before HTTPS, make sure the clock is plausible; sync automatically if not.
/// `Err` tells the user to set the time.
pub fn ensure_clock(env: &NetEnv) -> Result<Option<SyncReport>, String> {
    if clock_plausible() {
        return Ok(None);
    }
    log::warn!(
        "clock {} is before the build date {}; syncing",
        format_utc(SystemTime::now()),
        format_utc(build_time())
    );
    match sync(env, &[]) {
        Ok(r) if env.simulation || clock_plausible() => Ok(Some(r)),
        Ok(r) => Err(format!(
            "The clock is still wrong after the sync ({}). Set it in Network -> Date and time.",
            r.text()
        )),
        Err(e) => Err(format!(
            "The system clock ({}) is before the build date of this app, so HTTPS certificates \
             cannot be checked. {e}\nSet the time in Network -> Date and time.",
            format_utc(SystemTime::now())
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_entry() {
        let t = parse_manual("2026-10-27 12:34:56").unwrap();
        assert_eq!(format_utc(t), "2026-10-27 12:34:56 UTC");
        let t = parse_manual(" 2026-10-27 12:34 UTC ").unwrap();
        assert_eq!(format_utc(t), "2026-10-27 12:34:00 UTC");
        assert!(parse_manual("27.10.2026").is_err());
        assert!(parse_manual("1960-01-01 00:00").is_err());
    }

    #[test]
    fn plausibility() {
        let build = UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        assert!(!clock_plausible_at(UNIX_EPOCH, build));
        assert!(clock_plausible_at(build, build));
        assert!(clock_plausible_at(build - Duration::from_secs(3600), build));
        assert!(build_time() > UNIX_EPOCH);
        // The test host's clock is fine.
        assert!(clock_plausible());
    }
}
