//! Network details (SYS-4): per interface MAC, link/carrier, speed, addresses, Wi-Fi SSID and
//! signal, the app's configuration; default routes and DNS servers.

use std::fmt::Write as _;
use std::fs;
use std::net::{IpAddr, Ipv4Addr};

use serde::Serialize;

use super::info::{host_addresses, interfaces_from_sysfs};
use super::{config, diag, profile, wifi, NetEnv};
use crate::hw::read_trimmed;
use crate::report::ellipsize;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct InterfaceDetails {
    pub name: String,
    pub mac: Option<String>,
    pub state: Option<String>,
    pub carrier: Option<bool>,
    /// Link speed in Mbit/s (sysfs `speed`; unknown for Wi-Fi and without link).
    pub speed_mbps: Option<u32>,
    pub wireless: bool,
    pub addresses: Vec<String>,
    pub ssid: Option<String>,
    pub signal_dbm: Option<i32>,
    /// The rescue configuration of the interface (NET-2), or automatic DHCP (NET-1).
    pub config: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DefaultRoute {
    pub interface: String,
    pub gateway: Ipv4Addr,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NetworkDetails {
    pub interfaces: Vec<InterfaceDetails>,
    pub default_routes: Vec<DefaultRoute>,
    /// The app's resolver servers (NET-3).
    pub dns: Vec<IpAddr>,
    pub country: Option<String>,
}

/// Collect the details. Wi-Fi association is read with `iw` (not in simulation mode).
pub fn collect(env: &NetEnv) -> NetworkDetails {
    let state = profile::load_state(env);
    let addrs = host_addresses();
    let interfaces = interfaces_from_sysfs(&env.host, &addrs)
        .into_iter()
        .map(|i| {
            let dir = env.host.sys(&format!("class/net/{}", i.name));
            let speed_mbps = read_trimmed(&dir.join("speed"))
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|s| *s > 0)
                .map(|s| s as u32);
            let (ssid, signal_dbm) = if i.wireless && !env.simulation {
                match wifi::link_info(&i.name) {
                    Some((s, sig)) => (Some(s), sig),
                    None => (None, None),
                }
            } else {
                (None, None)
            };
            let cfg = match state.get(&i.name) {
                Some(c) => Some(profile::describe(c)),
                None if env
                    .networkd_dir
                    .join(config::network_file_name(&i.name))
                    .exists() =>
                {
                    Some(format!("{}: DHCP (automatic)", i.name))
                }
                None => None,
            };
            InterfaceDetails {
                name: i.name,
                mac: i.mac,
                state: i.state,
                carrier: i.carrier,
                speed_mbps,
                wireless: i.wireless,
                addresses: i.addresses,
                ssid,
                signal_dbm,
                config: cfg.map(|c| c.split_once(": ").map(|(_, r)| r.to_string()).unwrap_or(c)),
            }
        })
        .collect();
    let default_routes = fs::read_to_string(env.host.proc("net/route"))
        .map(|t| diag::parse_default_gateways(&t))
        .unwrap_or_default()
        .into_iter()
        .map(|(interface, gateway)| DefaultRoute { interface, gateway })
        .collect();
    NetworkDetails {
        interfaces,
        default_routes,
        dns: config::dns_servers(env),
        country: state.country,
    }
}

fn opt<T: ToString>(o: &Option<T>) -> String {
    o.as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".into())
}

/// Text for the details screen and the system report (fits 76 columns).
pub fn text(d: &NetworkDetails) -> String {
    let mut out = String::new();
    if d.interfaces.is_empty() {
        out.push_str("No network interfaces.\n");
    }
    for i in &d.interfaces {
        let kind = if i.wireless { "Wi-Fi" } else { "wired" };
        let _ = writeln!(out, "{} ({kind})  MAC {}", i.name, opt(&i.mac));
        let carrier = match i.carrier {
            Some(true) => "yes",
            Some(false) => "no",
            None => "-",
        };
        let speed = i
            .speed_mbps
            .map(|s| format!("  speed {s} Mbit/s"))
            .unwrap_or_default();
        let _ = writeln!(out, "  state {}  carrier {carrier}{speed}", opt(&i.state));
        if i.wireless {
            match &i.ssid {
                Some(s) => {
                    let sig = i
                        .signal_dbm
                        .map(|v| format!("  signal {v} dBm"))
                        .unwrap_or_default();
                    let _ = writeln!(out, "  SSID '{}'{sig}", ellipsize(s, 40));
                }
                None => {
                    let _ = writeln!(out, "  not associated");
                }
            }
        }
        if i.addresses.is_empty() {
            let _ = writeln!(out, "  no IP address");
        }
        for a in &i.addresses {
            let _ = writeln!(out, "  {a}");
        }
        if let Some(c) = &i.config {
            let _ = writeln!(out, "  config: {}", ellipsize(c, 64));
        }
    }
    out.push('\n');
    if d.default_routes.is_empty() {
        out.push_str("Default route: none\n");
    }
    for r in &d.default_routes {
        let _ = writeln!(out, "Default route: via {} dev {}", r.gateway, r.interface);
    }
    let dns: Vec<String> = d.dns.iter().map(|s| s.to_string()).collect();
    let _ = writeln!(
        out,
        "DNS: {}",
        if dns.is_empty() {
            "none".to_string()
        } else {
            dns.join(" ")
        }
    );
    if let Some(cc) = &d.country {
        let _ = writeln!(out, "Wi-Fi country: {cc}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::testutil::TempDir;
    use crate::hw::HostPaths;

    #[test]
    fn details_from_fixture() {
        let d = TempDir::new("net-details");
        d.write("sys/class/net/eth0/address", b"52:54:00:12:34:56\n");
        d.write("sys/class/net/eth0/operstate", b"up\n");
        d.write("sys/class/net/eth0/carrier", b"1\n");
        d.write("sys/class/net/eth0/speed", b"1000\n");
        d.write("sys/class/net/wlan0/operstate", b"down\n");
        d.write("sys/class/net/wlan0/speed", b"-1\n");
        d.write("sys/class/net/wlan0/wireless/.keep", b"");
        d.write(
            "proc/net/route",
            b"Iface\tDestination\tGateway \tFlags\nconfigured\t00000000\t0202000A\t0003\n",
        );
        d.write("run/resolv.conf", b"nameserver 10.0.2.3\n");
        let env = NetEnv {
            run_dir: d.path().join("run"),
            networkd_dir: d.path().join("run/systemd/network"),
            leases_dir: d.path().join("leases"),
            etc_resolv_conf: d.path().join("etc/resolv.conf"),
            host: HostPaths {
                proc_root: d.path().join("proc"),
                sys_root: d.path().join("sys"),
            },
            simulation: true,
        };
        d.write("run/systemd/network/50-xoros-rescue-eth0.network", b"");
        let det = collect(&env);
        assert_eq!(det.interfaces.len(), 2);
        assert_eq!(det.interfaces[0].speed_mbps, Some(1000));
        assert_eq!(det.interfaces[1].speed_mbps, None);
        assert_eq!(
            det.interfaces[0].config.as_deref(),
            Some("DHCP (automatic)")
        );
        assert_eq!(det.default_routes[0].gateway, Ipv4Addr::new(10, 0, 2, 2));
        assert_eq!(det.dns, vec!["10.0.2.3".parse::<IpAddr>().unwrap()]);
        let t = text(&det);
        assert!(t.contains("eth0 (wired)  MAC 52:54:00:12:34:56"));
        assert!(t.contains("speed 1000 Mbit/s"));
        assert!(t.contains("wlan0 (Wi-Fi)"));
        assert!(t.contains("not associated"));
        assert!(t.contains("Default route: via 10.0.2.2 dev configured"));
        assert!(t.contains("DNS: 10.0.2.3"));
        assert!(t.lines().all(|l| l.chars().count() <= 76));
    }
}
