//! Minimal DNS stub resolver with an explicit server list (NET-3).
//!
//! The rescue image has no `systemd-resolved` and `/etc/resolv.conf` is read-only, so the app
//! does not rely on libc's resolver for its own requests: the nameservers come from
//! `/run/xoros-rescue/resolv.conf` (written by [`super::config::refresh_resolv_conf`]) and
//! names are resolved here with plain UDP queries for `A` and `AAAA` records (RFC 1035).

use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::path::Path;
use std::time::{Duration, Instant};

/// Record types queried.
pub const TYPE_A: u16 = 1;
pub const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

/// Nameservers from a `resolv.conf` (`nameserver <ip>` lines; others ignored).
pub fn parse_resolv_conf(text: &str) -> Vec<IpAddr> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            match (w.next(), w.next()) {
                (Some("nameserver"), Some(ip)) => ip.split('%').next()?.parse().ok(),
                _ => None,
            }
        })
        .collect()
}

/// Nameservers from the file at `path`; empty if it is missing.
pub fn read_resolv_conf(path: &Path) -> Vec<IpAddr> {
    fs::read_to_string(path)
        .map(|t| parse_resolv_conf(&t))
        .unwrap_or_default()
}

/// Encode a query for `name` (one question, recursion desired).
pub fn build_query(id: u16, name: &str, qtype: u16) -> io::Result<Vec<u8>> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00]); // RD
    q.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // QD=1, AN=NS=AR=0
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid host name '{name}'"),
            ));
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&CLASS_IN.to_be_bytes());
    Ok(q)
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("DNS: {msg}"))
}

/// Skip a (possibly compressed) name starting at `pos`; returns the position after it.
fn skip_name(buf: &[u8], mut pos: usize) -> io::Result<usize> {
    loop {
        let len = *buf.get(pos).ok_or_else(|| bad("truncated name"))?;
        match len & 0xC0 {
            0x00 if len == 0 => return Ok(pos + 1),
            0x00 => pos += 1 + len as usize,
            0xC0 => return Ok(pos + 2),
            _ => return Err(bad("bad label")),
        }
    }
}

fn be16(buf: &[u8], pos: usize) -> io::Result<u16> {
    buf.get(pos..pos + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .ok_or_else(|| bad("truncated"))
}

/// Decode a response to query `id`: the `A`/`AAAA` addresses of the answer section (CNAME
/// chains are followed by the server; their records are skipped). `Ok(vec![])` for NXDOMAIN or
/// an empty answer.
pub fn parse_response(buf: &[u8], id: u16) -> io::Result<Vec<IpAddr>> {
    if buf.len() < 12 {
        return Err(bad("short response"));
    }
    if be16(buf, 0)? != id {
        return Err(bad("unexpected id"));
    }
    let flags = be16(buf, 2)?;
    if flags & 0x8000 == 0 {
        return Err(bad("not a response"));
    }
    match flags & 0x000F {
        0 => {}
        3 => return Ok(Vec::new()), // NXDOMAIN
        rcode => return Err(bad(&format!("server error (rcode {rcode})"))),
    }
    let qd = be16(buf, 4)?;
    let an = be16(buf, 6)?;
    let mut pos = 12;
    for _ in 0..qd {
        pos = skip_name(buf, pos)? + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        pos = skip_name(buf, pos)?;
        let rtype = be16(buf, pos)?;
        let rdlen = be16(buf, pos + 8)? as usize;
        let data = buf
            .get(pos + 10..pos + 10 + rdlen)
            .ok_or_else(|| bad("truncated record"))?;
        match (rtype, rdlen) {
            (TYPE_A, 4) => out.push(IpAddr::V4(Ipv4Addr::new(
                data[0], data[1], data[2], data[3],
            ))),
            (TYPE_AAAA, 16) => {
                let mut a = [0u8; 16];
                a.copy_from_slice(data);
                out.push(IpAddr::V6(Ipv6Addr::from(a)));
            }
            _ => {}
        }
        pos += 10 + rdlen;
    }
    Ok(out)
}

fn query_id() -> u16 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos ^ (std::process::id() << 3)) as u16
}

/// One query to one server.
fn query(server: SocketAddr, name: &str, qtype: u16, timeout: Duration) -> io::Result<Vec<IpAddr>> {
    let bind: SocketAddr = match server {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let sock = UdpSocket::bind(bind)?;
    sock.connect(server)?;
    let id = query_id().wrapping_add(qtype);
    sock.send(&build_query(id, name, qtype)?)?;
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 1500];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("DNS server {server} did not answer"),
            ));
        }
        sock.set_read_timeout(Some(left))?;
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("DNS server {server} did not answer"),
                ))
            }
            Err(e) => return Err(e),
        };
        match parse_response(&buf[..n], id) {
            Err(e) if e.to_string().contains("unexpected id") => continue,
            r => return r,
        }
    }
}

/// Resolve `host` with the given servers (tried in order until one answers). IP literals and
/// `localhost` are returned without a query.
pub fn resolve(host: &str, servers: &[IpAddr], timeout: Duration) -> io::Result<Vec<IpAddr>> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    }
    let servers: Vec<SocketAddr> = servers.iter().map(|ip| SocketAddr::new(*ip, 53)).collect();
    resolve_at(host, &servers, timeout)
}

fn resolve_at(host: &str, servers: &[SocketAddr], timeout: Duration) -> io::Result<Vec<IpAddr>> {
    if servers.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no DNS servers configured",
        ));
    }
    let mut last = None;
    for server in servers {
        match query(*server, host, TYPE_A, timeout) {
            Ok(mut v4) => {
                // AAAA is optional (IPv4-only networks are the common case here).
                if let Ok(v6) = query(*server, host, TYPE_AAAA, timeout) {
                    v4.extend(v6);
                }
                if v4.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{host}: no such host (server {server})"),
                    ));
                }
                return Ok(v4);
            }
            Err(e) => {
                log::debug!("DNS {server}: {e}");
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no answer")))
}

/// Split `host:port` / `[v6]:port` (ureq passes the `netloc` in this form).
pub fn split_netloc(netloc: &str) -> io::Result<(&str, u16)> {
    let (host, port) = netloc
        .rsplit_once(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing port"))?;
    let port = port
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad port"))?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolv_conf() {
        let t = "# comment\nnameserver 192.168.1.1\nsearch lan\nnameserver fe80::1%eth0\n\
                 nameserver bogus\noptions edns0\n";
        assert_eq!(
            parse_resolv_conf(t),
            vec![
                "192.168.1.1".parse::<IpAddr>().unwrap(),
                "fe80::1".parse().unwrap()
            ]
        );
    }

    #[test]
    fn query_encoding() {
        let q = build_query(0x1234, "ota.xoros.io", TYPE_A).unwrap();
        assert_eq!(&q[..4], &[0x12, 0x34, 0x01, 0x00]);
        assert_eq!(&q[12..], b"\x03ota\x05xoros\x02io\x00\x00\x01\x00\x01");
        assert!(build_query(1, "a..b", TYPE_A).is_err());
        assert!(build_query(1, &"x".repeat(64), TYPE_A).is_err());
    }

    /// A response with a compressed CNAME and one A and one AAAA record.
    fn response(id: u16, rcode: u8) -> Vec<u8> {
        let mut r = build_query(id, "ota.xoros.io", TYPE_A).unwrap();
        r[2] = 0x81;
        r[3] = 0x80 | rcode;
        r[7] = 3; // ANCOUNT
        // CNAME ota.xoros.io -> cdn.xoros.io
        r.extend_from_slice(&[0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 6]);
        r.extend_from_slice(b"\x03cdn\xC0\x10");
        // A cdn -> 203.0.113.7 (name: pointer to the CNAME data)
        let cdn = 12 + 18 + 12;
        r.extend_from_slice(&[0xC0, cdn as u8, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 203, 0, 113, 7]);
        r.extend_from_slice(&[0xC0, cdn as u8, 0, 28, 0, 1, 0, 0, 0, 60, 0, 16]);
        r.extend_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        r
    }

    #[test]
    fn response_decoding() {
        let ips = parse_response(&response(7, 0), 7).unwrap();
        assert_eq!(
            ips,
            vec![
                "203.0.113.7".parse::<IpAddr>().unwrap(),
                "2001:db8::1".parse().unwrap()
            ]
        );
        assert!(parse_response(&response(7, 0), 8).is_err());
        assert!(parse_response(&response(7, 3), 7).unwrap().is_empty());
        assert!(parse_response(&response(7, 2), 7).is_err());
        let r = response(7, 0);
        assert!(parse_response(&r[..r.len() - 3], 7).is_err());
        assert!(parse_response(&[0; 5], 7).is_err());
    }

    #[test]
    fn literals_and_netloc() {
        assert_eq!(
            resolve("127.0.0.1", &[], Duration::from_secs(1)).unwrap(),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        );
        assert_eq!(
            resolve("[::1]", &[], Duration::from_secs(1)).unwrap(),
            vec!["::1".parse::<IpAddr>().unwrap()]
        );
        assert!(resolve("example.org", &[], Duration::from_secs(1)).is_err());
        assert_eq!(split_netloc("a.b:443").unwrap(), ("a.b", 443));
        assert_eq!(split_netloc("[::1]:80").unwrap(), ("[::1]", 80));
        assert!(split_netloc("a.b").is_err());
    }

    #[test]
    fn query_against_local_server() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            for _ in 0..2 {
                let (n, from) = server.recv_from(&mut buf).unwrap();
                let id = u16::from_be_bytes([buf[0], buf[1]]);
                let qtype = u16::from_be_bytes([buf[n - 4], buf[n - 3]]);
                let mut r = response(id, 0);
                if qtype == TYPE_AAAA {
                    r[7] = 0; // no AAAA records
                    r.truncate(n);
                }
                server.send_to(&r, from).unwrap();
            }
        });
        let ips = resolve_at("ota.xoros.io", &[addr], Duration::from_secs(5)).unwrap();
        t.join().unwrap();
        // The A answer of the fake server carries both records; the AAAA answer is empty.
        assert_eq!(
            ips,
            vec![
                "203.0.113.7".parse::<IpAddr>().unwrap(),
                "2001:db8::1".parse().unwrap()
            ]
        );
        // Nothing answers: a timeout, not a hang.
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let e = resolve_at(
            "x.y",
            &[silent.local_addr().unwrap()],
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    }
}
