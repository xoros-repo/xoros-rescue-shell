//! Network test (NET-4): one step after the other, each with pass/fail, details and duration.
//! Read-only: nothing is configured here (in simulation mode it tests the host's network).

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::config::{dns_servers, refresh_resolv_conf};
use super::http::{http_options, url_host_port};
use super::info::{host_addresses, interfaces_from_sysfs};
use super::time::{build_time, clock_plausible, format_utc};
use super::{dns, NetEnv};
use crate::cmd;
use crate::tasks::CancelToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pass,
    Fail,
    Skip,
}

impl StepStatus {
    pub fn label(self) -> &'static str {
        match self {
            StepStatus::Pass => "PASS",
            StepStatus::Fail => "FAIL",
            StepStatus::Skip => "skip",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Step {
    pub name: &'static str,
    pub status: StepStatus,
    pub detail: String,
    #[serde(serialize_with = "millis")]
    pub duration: Duration,
}

fn millis<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(d.as_millis() as u64)
}

/// What to test against.
#[derive(Clone, Debug)]
pub struct TestOptions {
    /// Manifest URL for the HTTPS step (its host is used for DNS and TCP).
    pub url: String,
    pub proxy: Option<String>,
}

/// Default gateways from `/proc/net/route` (IPv4).
pub fn parse_default_gateways(route: &str) -> Vec<(String, Ipv4Addr)> {
    route
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 3 || f[1] != "00000000" {
                return None;
            }
            let gw = u32::from_str_radix(f[2], 16).ok()?;
            if gw == 0 {
                return None;
            }
            // Little-endian hex of the address in network order.
            Some((f[0].to_string(), Ipv4Addr::from(gw.swap_bytes())))
        })
        .collect()
}

/// `true` if `/proc/net/arp` has a complete entry for `ip`.
pub fn arp_complete(arp: &str, ip: Ipv4Addr) -> bool {
    let ip = ip.to_string();
    arp.lines().skip(1).any(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        f.len() >= 4 && f[0] == ip && f[2] == "0x2" && f[3] != "00:00:00:00:00:00"
    })
}

fn timed(name: &'static str, f: impl FnOnce() -> (StepStatus, String)) -> Step {
    let start = Instant::now();
    let (status, detail) = f();
    Step {
        name,
        status,
        detail,
        duration: start.elapsed(),
    }
}

fn gateway_reachable(env: &NetEnv, gw: Ipv4Addr) -> (StepStatus, String) {
    if cmd::find_tool("ping").is_some() {
        let ip = gw.to_string();
        return match cmd::run_capture("ping", &["-c", "1", "-W", "2", &ip]) {
            Ok(_) => (StepStatus::Pass, format!("{gw} answers ping")),
            Err(e) => {
                // ICMP may be filtered: fall through to ARP.
                if arp_after_probe(env, gw) {
                    (StepStatus::Pass, format!("{gw} in ARP table (no ping answer)"))
                } else {
                    (StepStatus::Fail, format!("{gw}: {e}"))
                }
            }
        };
    }
    if arp_after_probe(env, gw) {
        (StepStatus::Pass, format!("{gw} in ARP table"))
    } else {
        (StepStatus::Fail, format!("{gw} does not answer ARP"))
    }
}

/// Send a UDP datagram to the gateway (triggers ARP) and look into the ARP table.
fn arp_after_probe(env: &NetEnv, gw: Ipv4Addr) -> bool {
    if let Ok(s) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        let _ = s.send_to(b"x", (gw, 9));
    }
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(100));
        if fs::read_to_string(env.host.proc("net/arp"))
            .map(|t| arp_complete(&t, gw))
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Run all steps; `on_step` sees each step when it is done. Later steps that depend on a
/// failed one are skipped. Stops early when cancelled.
pub fn run(
    env: &NetEnv,
    opts: &TestOptions,
    cancel: &CancelToken,
    on_step: &mut dyn FnMut(&Step),
) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut push = |s: Step, steps: &mut Vec<Step>| {
        on_step(&s);
        steps.push(s);
    };

    // 1. link / carrier
    let addrs = host_addresses();
    let ifs = interfaces_from_sysfs(&env.host, &addrs);
    let s = timed("Link / carrier", || {
        let up: Vec<&str> = ifs
            .iter()
            .filter(|i| i.carrier == Some(true))
            .map(|i| i.name.as_str())
            .collect();
        if up.is_empty() {
            (StepStatus::Fail, "no interface has a carrier (cable / Wi-Fi?)".into())
        } else {
            (StepStatus::Pass, format!("carrier on {}", up.join(", ")))
        }
    });
    push(s, &mut steps);

    // 2. IP address
    let s = timed("IP address", || {
        let mut list = Vec::new();
        for i in &ifs {
            for a in &i.addresses {
                let ip = a.split('/').next().unwrap_or("");
                if ip.parse::<IpAddr>().is_ok_and(|ip| match ip {
                    IpAddr::V4(v) => !v.is_loopback() && !v.is_link_local(),
                    IpAddr::V6(v) => !v.is_loopback() && (v.segments()[0] & 0xffc0) != 0xfe80,
                }) {
                    list.push(format!("{} {a}", i.name));
                }
            }
        }
        if list.is_empty() {
            (StepStatus::Fail, "no usable address (DHCP not answered?)".into())
        } else {
            (StepStatus::Pass, list.join(", "))
        }
    });
    push(s, &mut steps);
    if cancel.is_cancelled() {
        return steps;
    }

    // 3. gateway
    let route = fs::read_to_string(env.host.proc("net/route")).unwrap_or_default();
    let gws = parse_default_gateways(&route);
    let s = timed("Gateway", || match gws.first() {
        None => (StepStatus::Fail, "no default route".into()),
        Some((dev, gw)) => {
            let (st, d) = gateway_reachable(env, *gw);
            (st, format!("{d} via {dev}"))
        }
    });
    push(s, &mut steps);
    if cancel.is_cancelled() {
        return steps;
    }

    // 4. DNS
    let (host, port) = url_host_port(&opts.url).unwrap_or_else(|| (String::new(), 443));
    let mut resolved: Vec<IpAddr> = Vec::new();
    let s = timed("DNS", || {
        let servers = refresh_resolv_conf(env).unwrap_or_else(|_| dns_servers(env));
        if host.parse::<IpAddr>().is_ok() {
            resolved = vec![host.parse().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))];
            return (StepStatus::Skip, format!("{host} is an IP address"));
        }
        if servers.is_empty() {
            return (StepStatus::Fail, "no DNS servers (DHCP lease?)".into());
        }
        let list: Vec<String> = servers.iter().map(|s| s.to_string()).collect();
        match dns::resolve(&host, &servers, Duration::from_secs(3)) {
            Ok(ips) => {
                resolved = ips;
                let txt: Vec<String> = resolved.iter().map(|i| i.to_string()).collect();
                (
                    StepStatus::Pass,
                    format!("{host} -> {} (servers {})", txt.join(", "), list.join(", ")),
                )
            }
            Err(e) => (
                StepStatus::Fail,
                format!("{host}: {e} (servers {})", list.join(", ")),
            ),
        }
    });
    push(s, &mut steps);
    if cancel.is_cancelled() {
        return steps;
    }

    // 5. TCP connect
    let s = timed("TCP connect", || {
        if resolved.is_empty() {
            return (StepStatus::Skip, "no address".into());
        }
        let mut last = String::new();
        for ip in resolved.iter().take(4) {
            let addr = SocketAddr::new(*ip, port);
            match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
                Ok(_) => return (StepStatus::Pass, format!("{addr} connected")),
                Err(e) => last = format!("{addr}: {e}"),
            }
        }
        (StepStatus::Fail, last)
    });
    let tcp_ok = s.status == StepStatus::Pass;
    push(s, &mut steps);

    // 6. clock (NET-7), before HTTPS
    let s = timed("Clock", || {
        let now = format_utc(std::time::SystemTime::now());
        if clock_plausible() {
            (StepStatus::Pass, now)
        } else {
            (
                StepStatus::Fail,
                format!(
                    "{now} is before the app's build date {}: sync the time",
                    format_utc(build_time())
                ),
            )
        }
    });
    push(s, &mut steps);
    if cancel.is_cancelled() {
        return steps;
    }

    // 7. HTTP(S) GET of the manifest
    let s = timed("HTTPS manifest", || {
        if !tcp_ok && opts.proxy.is_none() {
            return (StepStatus::Skip, "no TCP connection".into());
        }
        let mut o = http_options(opts.proxy.clone(), dns_servers(env));
        o.connect_timeout = Duration::from_secs(10);
        o.read_timeout = Duration::from_secs(15);
        let r = xoros_core::http::HttpClient::new(&o).and_then(|c| c.get_optional(&opts.url));
        match r {
            Ok(Some(_)) => (StepStatus::Pass, format!("HTTP 2xx {}", opts.url)),
            Ok(None) => (StepStatus::Fail, format!("HTTP 404 {}", opts.url)),
            Err(e) => (StepStatus::Fail, e.to_string()),
        }
    });
    push(s, &mut steps);
    steps
}

/// One line per step, for the UI and the CLI.
pub fn step_line(s: &Step) -> String {
    format!(
        "[{}] {:<15} {:>6} ms  {}",
        s.status.label(),
        s.name,
        s.duration.as_millis(),
        s.detail
    )
}

pub fn report_text(steps: &[Step]) -> String {
    let mut t: String = steps.iter().map(|s| step_line(s) + "\n").collect();
    let failed = steps.iter().filter(|s| s.status == StepStatus::Fail).count();
    t.push_str(&if failed == 0 {
        "\nAll checks passed.\n".to_string()
    } else {
        format!("\n{failed} check(s) failed.\n")
    });
    t
}

/// Keep the last test report in the run directory for the diagnostics bundle (LOG-2).
pub fn record(env: &NetEnv, steps: &[Step]) {
    let p = env.run_dir.join(crate::constants::NET_TEST_REPORT_NAME);
    let text = format!(
        "Network test {}\n\n{}",
        format_utc(std::time::SystemTime::now()),
        report_text(steps)
    );
    if let Err(e) = super::config::write_file(&p, &text, 0o644) {
        log::warn!("cannot save the network test report: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::testutil::TempDir;
    use crate::hw::HostPaths;

    #[test]
    fn routes_and_arp() {
        let route = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
                     eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\n\
                     eth0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\n";
        assert_eq!(
            parse_default_gateways(route),
            vec![("eth0".to_string(), Ipv4Addr::new(192, 168, 1, 1))]
        );
        let arp = "IP address       HW type     Flags       HW address            Mask     Device\n\
                   192.168.1.1      0x1         0x2         aa:bb:cc:dd:ee:ff     *        eth0\n\
                   192.168.1.9      0x1         0x0         00:00:00:00:00:00     *        eth0\n";
        assert!(arp_complete(arp, Ipv4Addr::new(192, 168, 1, 1)));
        assert!(!arp_complete(arp, Ipv4Addr::new(192, 168, 1, 9)));
    }

    #[test]
    fn local_http_server_passes_tcp_and_http() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let t = std::thread::spawn(move || {
            if let Ok(Some(rq)) = server.recv_timeout(Duration::from_secs(10)) {
                let _ = rq.respond(tiny_http::Response::from_string("{}"));
            }
        });
        let d = TempDir::new("net-diag");
        let env = NetEnv {
            run_dir: d.path().join("run"),
            networkd_dir: d.path().join("run/systemd/network"),
            leases_dir: d.path().join("leases"),
            etc_resolv_conf: d.path().join("resolv.conf"),
            host: HostPaths {
                proc_root: d.path().join("proc"),
                sys_root: d.path().join("sys"),
            },
            simulation: true,
        };
        let opts = TestOptions {
            url: format!("http://127.0.0.1:{port}/1.0/m/devel/manifest.json"),
            proxy: None,
        };
        let mut seen = 0;
        let steps = run(&env, &opts, &CancelToken::new(), &mut |_| seen += 1);
        t.join().unwrap();
        assert_eq!(seen, 7);
        let by = |n: &str| steps.iter().find(|s| s.name == n).unwrap().status;
        // Empty fixture sysfs/procfs: no carrier, no route.
        assert_eq!(by("Link / carrier"), StepStatus::Fail);
        assert_eq!(by("Gateway"), StepStatus::Fail);
        assert_eq!(by("DNS"), StepStatus::Skip);
        assert_eq!(by("TCP connect"), StepStatus::Pass);
        assert_eq!(by("HTTPS manifest"), StepStatus::Pass, "{steps:?}");
        let txt = report_text(&steps);
        assert!(txt.contains("check(s) failed"));
    }
}
