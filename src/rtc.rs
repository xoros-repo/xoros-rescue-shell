//! Hardware real-time clock (NET-7 P2): read and write `/dev/rtc0` with the `RTC_RD_TIME` /
//! `RTC_SET_TIME` ioctls (the RTC keeps UTC). The Raspberry Pi has no RTC (RT-7); a missing
//! device is not an error.

use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Datelike, NaiveDate, TimeZone, Timelike, Utc};

/// `struct rtc_time` from `<linux/rtc.h>` (same layout as the first nine fields of `struct tm`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtcTime {
    pub tm_sec: libc::c_int,
    pub tm_min: libc::c_int,
    pub tm_hour: libc::c_int,
    pub tm_mday: libc::c_int,
    /// 0..11
    pub tm_mon: libc::c_int,
    /// Years since 1900.
    pub tm_year: libc::c_int,
    pub tm_wday: libc::c_int,
    pub tm_yday: libc::c_int,
    pub tm_isdst: libc::c_int,
}

/// `_IOR('p', 0x09, struct rtc_time)`
const RTC_RD_TIME: u32 = 0x8024_7009;
/// `_IOW('p', 0x0a, struct rtc_time)`
const RTC_SET_TIME: u32 = 0x4024_700a;

/// The RTC fields of a system time (UTC, seconds precision).
pub fn to_rtc(t: SystemTime) -> Option<RtcTime> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let dt = Utc.timestamp_opt(i64::try_from(secs).ok()?, 0).single()?;
    Some(RtcTime {
        tm_sec: dt.second() as libc::c_int,
        tm_min: dt.minute() as libc::c_int,
        tm_hour: dt.hour() as libc::c_int,
        tm_mday: dt.day() as libc::c_int,
        tm_mon: dt.month0() as libc::c_int,
        tm_year: dt.year() - 1900,
        tm_wday: dt.weekday().num_days_from_sunday() as libc::c_int,
        tm_yday: dt.ordinal0() as libc::c_int,
        tm_isdst: 0,
    })
}

/// The system time of RTC fields; `None` for invalid values (an RTC that was never set).
pub fn from_rtc(r: &RtcTime) -> Option<SystemTime> {
    let u = |v: libc::c_int| u32::try_from(v).ok();
    let date = NaiveDate::from_ymd_opt(r.tm_year + 1900, u(r.tm_mon)? + 1, u(r.tm_mday)?)?;
    let dt = date.and_hms_opt(u(r.tm_hour)?, u(r.tm_min)?, u(r.tm_sec)?)?;
    let secs = u64::try_from(dt.and_utc().timestamp()).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

fn open(dev: &Path, write: bool) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .open(dev)
        .map_err(|e| format!("{}: {e}", dev.display()))
}

/// Read the RTC.
pub fn read(dev: &Path) -> Result<SystemTime, String> {
    let f = open(dev, false)?;
    let mut r = RtcTime::default();
    // SAFETY: valid fd and a pointer to a properly sized, writable rtc_time.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), RTC_RD_TIME as _, &mut r as *mut RtcTime) };
    if rc != 0 {
        return Err(format!(
            "{}: RTC_RD_TIME: {}",
            dev.display(),
            std::io::Error::last_os_error()
        ));
    }
    from_rtc(&r).ok_or_else(|| format!("{}: the RTC holds an invalid time", dev.display()))
}

/// Write `t` to the RTC.
pub fn write(dev: &Path, t: SystemTime) -> Result<(), String> {
    let r = to_rtc(t).ok_or("time out of the RTC range")?;
    let f = open(dev, true)?;
    // SAFETY: valid fd and a pointer to an initialised rtc_time.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), RTC_SET_TIME as _, &r as *const RtcTime) };
    if rc != 0 {
        return Err(format!(
            "{}: RTC_SET_TIME: {}",
            dev.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// After the clock was set: write it to the RTC if there is one. Returns a note for the user,
/// `None` when the device has no RTC. Only logged in simulation mode.
pub fn store(simulation: bool, dev: &Path, t: SystemTime) -> Option<String> {
    if simulation {
        log::info!(
            "SIM: would write {} to {}",
            crate::net::time::format_utc(t),
            dev.display()
        );
        return Some(format!("Simulation: {} not written.", dev.display()));
    }
    if !dev.exists() {
        return None;
    }
    Some(match write(dev, t) {
        Ok(()) => {
            log::info!(
                "action: RTC {} set to {}",
                dev.display(),
                crate::net::time::format_utc(t)
            );
            format!("Hardware clock {} set.", dev.display())
        }
        Err(e) => {
            log::warn!("RTC: {e}");
            format!("Hardware clock not set: {e}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtc_fields_round_trip() {
        let t = UNIX_EPOCH + Duration::from_secs(1_793_190_896); // 2026-10-28 12:34:56 UTC
        let r = to_rtc(t).unwrap();
        assert_eq!(
            (r.tm_year, r.tm_mon, r.tm_mday, r.tm_hour, r.tm_min, r.tm_sec),
            (126, 9, 28, 12, 34, 56)
        );
        assert_eq!(r.tm_wday, 3);
        assert_eq!(r.tm_yday, 300);
        assert_eq!(from_rtc(&r), Some(t));
        assert_eq!(from_rtc(&RtcTime::default()), None, "day 0 is invalid");
        assert_eq!(
            std::mem::size_of::<RtcTime>(),
            36,
            "matches the ioctl number"
        );
    }

    #[test]
    fn missing_device_is_no_rtc() {
        let p = Path::new("/nonexistent/rtc9");
        assert_eq!(store(false, p, SystemTime::now()), None);
        assert!(store(true, p, SystemTime::now()).is_some());
        assert!(read(p).is_err());
    }
}
