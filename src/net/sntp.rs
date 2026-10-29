//! SNTP client (RFC 4330) for the time sync (NET-7, RT-7). The rescue image has no
//! `systemd-timesyncd`; the Raspberry Pi has no RTC, so the clock may be far off after power-on
//! and TLS certificate checks fail until it is set.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds between 1900-01-01 (NTP era 0) and 1970-01-01.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
/// Size of an NTP packet without extensions.
pub const PACKET_LEN: usize = 48;
pub const NTP_PORT: u16 = 123;

/// 64-bit NTP timestamp of a system time (era 0; valid until 2036, later times wrap like real
/// NTP clients expect).
pub fn to_ntp(t: SystemTime) -> u64 {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = (d.as_secs() + NTP_UNIX_OFFSET) & 0xFFFF_FFFF;
    let frac = (u64::from(d.subsec_nanos()) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

/// System time of an NTP timestamp. Era 1 (seconds value below 2^31, i.e. after 2036) is
/// assumed for small values (RFC 4330 section 3).
pub fn from_ntp(ts: u64) -> SystemTime {
    let secs = ts >> 32;
    let frac = ts & 0xFFFF_FFFF;
    let secs = if secs & 0x8000_0000 == 0 {
        secs + (1u64 << 32)
    } else {
        secs
    };
    let nanos = (frac * 1_000_000_000) >> 32;
    UNIX_EPOCH + Duration::new(secs.saturating_sub(NTP_UNIX_OFFSET), nanos as u32)
}

/// Client request: LI 0, version 4, mode 3 (client), transmit timestamp `t1`.
pub fn request_packet(t1: SystemTime) -> [u8; PACKET_LEN] {
    let mut p = [0u8; PACKET_LEN];
    p[0] = (4 << 3) | 3;
    p[40..48].copy_from_slice(&to_ntp(t1).to_be_bytes());
    p
}

/// A valid server reply.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub stratum: u8,
    /// Clock offset in seconds (server - local), `((t2 - t1) + (t3 - t4)) / 2`.
    pub offset: f64,
    /// Round trip delay in seconds.
    pub delay: f64,
}

impl Reply {
    /// The corrected current time, given the local time now.
    pub fn corrected(&self, now: SystemTime) -> SystemTime {
        shift(now, self.offset)
    }
}

/// `t` moved by `secs` (may be negative).
pub fn shift(t: SystemTime, secs: f64) -> SystemTime {
    let d = Duration::from_secs_f64(secs.abs());
    if secs >= 0.0 {
        t + d
    } else {
        t.checked_sub(d).unwrap_or(UNIX_EPOCH)
    }
}

fn secs_between(a: SystemTime, b: SystemTime) -> f64 {
    match b.duration_since(a) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Decode and check a reply to the request sent at `t1` and received at `t4` (local times).
pub fn parse_reply(buf: &[u8], t1: SystemTime, t4: SystemTime) -> io::Result<Reply> {
    if buf.len() < PACKET_LEN {
        return Err(bad("short NTP reply"));
    }
    let mode = buf[0] & 0x07;
    if mode != 4 {
        return Err(bad(format!("unexpected NTP mode {mode}")));
    }
    let li = buf[0] >> 6;
    let stratum = buf[1];
    if stratum == 0 {
        let code = String::from_utf8_lossy(&buf[12..16]).into_owned();
        return Err(bad(format!("server refused (kiss code {code})")));
    }
    if li == 3 || stratum > 15 {
        return Err(bad("server clock is not synchronized"));
    }
    let ts = |o: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[o..o + 8]);
        u64::from_be_bytes(b)
    };
    if ts(24) != to_ntp(t1) {
        return Err(bad("NTP reply does not match the request"));
    }
    let (t2, t3) = (from_ntp(ts(32)), from_ntp(ts(40)));
    if ts(40) == 0 {
        return Err(bad("NTP reply without transmit time"));
    }
    let offset = (secs_between(t1, t2) + secs_between(t4, t3)) / 2.0;
    let delay = secs_between(t1, t4) - secs_between(t2, t3);
    Ok(Reply {
        stratum,
        offset,
        delay: delay.max(0.0),
    })
}

/// Query one server address.
pub fn query(addr: SocketAddr, timeout: Duration) -> io::Result<Reply> {
    let bind: SocketAddr = match addr {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let sock = UdpSocket::bind(bind)?;
    sock.connect(addr)?;
    sock.set_read_timeout(Some(timeout))?;
    let t1 = SystemTime::now();
    sock.send(&request_packet(t1))?;
    let mut buf = [0u8; 128];
    let n = sock.recv(&mut buf).map_err(|e| {
        if matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            io::Error::new(io::ErrorKind::TimedOut, format!("{addr}: no answer"))
        } else {
            e
        }
    })?;
    let t4 = SystemTime::now();
    parse_reply(&buf[..n], t1, t4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64, nanos: u32) -> SystemTime {
        UNIX_EPOCH + Duration::new(secs, nanos)
    }

    #[test]
    fn timestamp_round_trip() {
        let t = at(1_793_000_000, 250_000_000);
        let back = from_ntp(to_ntp(t));
        let diff = secs_between(t, back).abs();
        assert!(diff < 1e-6, "{diff}");
        assert_eq!(to_ntp(at(0, 0)) >> 32, NTP_UNIX_OFFSET);
        // After 2036 (era 1).
        let late = at((1u64 << 32) - NTP_UNIX_OFFSET + 100, 0);
        assert!(secs_between(late, from_ntp(to_ntp(late))).abs() < 1e-6);
    }

    fn reply(t1: SystemTime, t2: SystemTime, t3: SystemTime, stratum: u8) -> [u8; 48] {
        let mut r = [0u8; 48];
        r[0] = (4 << 3) | 4;
        r[1] = stratum;
        r[24..32].copy_from_slice(&to_ntp(t1).to_be_bytes());
        r[32..40].copy_from_slice(&to_ntp(t2).to_be_bytes());
        r[40..48].copy_from_slice(&to_ntp(t3).to_be_bytes());
        r
    }

    #[test]
    fn request_and_reply() {
        let t1 = at(100_000, 0);
        let p = request_packet(t1);
        assert_eq!(p[0], 0x23);
        assert_eq!(&p[40..48], &to_ntp(t1).to_be_bytes());

        // Server is 1000 s ahead; 0.2 s each way, 0.1 s processing.
        let t2 = at(101_000, 200_000_000);
        let t3 = at(101_000, 300_000_000);
        let t4 = at(100_000, 500_000_000);
        let r = parse_reply(&reply(t1, t2, t3, 2), t1, t4).unwrap();
        assert!((r.offset - 1000.0).abs() < 1e-3, "{}", r.offset);
        assert!((r.delay - 0.4).abs() < 1e-3, "{}", r.delay);
        assert_eq!(r.stratum, 2);
        let now = r.corrected(t4);
        assert!(secs_between(at(101_000, 500_000_000), now).abs() < 1e-3);

        // Kiss-o'-death, wrong origin, wrong mode, short.
        let mut kod = reply(t1, t2, t3, 0);
        kod[12..16].copy_from_slice(b"RATE");
        assert!(parse_reply(&kod, t1, t4)
            .unwrap_err()
            .to_string()
            .contains("RATE"));
        assert!(parse_reply(&reply(at(5, 0), t2, t3, 2), t1, t4).is_err());
        let mut m = reply(t1, t2, t3, 2);
        m[0] = (4 << 3) | 3;
        assert!(parse_reply(&m, t1, t4).is_err());
        assert!(parse_reply(&m[..20], t1, t4).is_err());
        let mut unsync = reply(t1, t2, t3, 2);
        unsync[0] |= 0xC0;
        assert!(parse_reply(&unsync, t1, t4).is_err());
        assert!(secs_between(shift(t1, -5.0), t1) > 4.9);
    }

    #[test]
    fn query_local_server() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 48];
            let (_, from) = server.recv_from(&mut buf).unwrap();
            let mut r = [0u8; 48];
            r[0] = (4 << 3) | 4;
            r[1] = 1;
            r[24..32].copy_from_slice(&buf[40..48]);
            let now = to_ntp(SystemTime::now() + Duration::from_secs(3600));
            r[32..40].copy_from_slice(&now.to_be_bytes());
            r[40..48].copy_from_slice(&now.to_be_bytes());
            server.send_to(&r, from).unwrap();
        });
        let r = query(addr, Duration::from_secs(5)).unwrap();
        t.join().unwrap();
        assert!((r.offset - 3600.0).abs() < 5.0, "{}", r.offset);
    }
}
