//! Network summary for the status screen (SYS-1): interfaces, link state, addresses.
//! Internet reachability is part of the network test (NET-4, M2) and reported as unknown.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::fs;

use serde::Serialize;

use crate::hw::{read_trimmed, HostPaths};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct InterfaceInfo {
    pub name: String,
    pub mac: Option<String>,
    /// `operstate`: up, down, dormant, unknown, ...
    pub state: Option<String>,
    pub carrier: Option<bool>,
    pub wireless: bool,
    pub addresses: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NetworkSummary {
    pub interfaces: Vec<InterfaceInfo>,
    /// `None` = not checked yet (NET-4).
    pub internet_reachable: Option<bool>,
}

/// List interfaces from `<sys>/class/net` (without `lo`) and attach `addresses`
/// (interface name -> list of "addr/prefix").
pub fn interfaces_from_sysfs(
    paths: &HostPaths,
    addresses: &BTreeMap<String, Vec<String>>,
) -> Vec<InterfaceInfo> {
    let dir = paths.sys("class/net");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<InterfaceInfo> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == "lo" {
                return None;
            }
            let p = e.path();
            Some(InterfaceInfo {
                mac: read_trimmed(&p.join("address")),
                state: read_trimmed(&p.join("operstate")),
                carrier: read_trimmed(&p.join("carrier")).map(|c| c == "1"),
                wireless: p.join("wireless").exists() || p.join("phy80211").exists(),
                addresses: addresses.get(&name).cloned().unwrap_or_default(),
                name,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn prefix_len(netmask: &[u8]) -> u32 {
    netmask.iter().map(|b| b.count_ones()).sum()
}

/// IPv4/IPv6 addresses of the running host via `getifaddrs(3)`.
pub fn host_addresses() -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a linked list we free with freeifaddrs; we only read it.
    unsafe {
        if libc::getifaddrs(&mut ifap) != 0 {
            return map;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() {
                continue;
            }
            let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            let family = i32::from((*ifa.ifa_addr).sa_family);
            let text = if family == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                let prefix = if ifa.ifa_netmask.is_null() {
                    32
                } else {
                    let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
                    prefix_len(&m.sin_addr.s_addr.to_ne_bytes())
                };
                format!("{ip}/{prefix}")
            } else if family == libc::AF_INET6 {
                let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
                let prefix = if ifa.ifa_netmask.is_null() {
                    128
                } else {
                    let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in6);
                    prefix_len(&m.sin6_addr.s6_addr)
                };
                format!("{ip}/{prefix}")
            } else {
                continue;
            };
            map.entry(name).or_default().push(text);
        }
        libc::freeifaddrs(ifap);
    }
    map
}

/// Summary of the network for the status screen.
pub fn network_summary(paths: &HostPaths) -> NetworkSummary {
    NetworkSummary {
        interfaces: interfaces_from_sysfs(paths, &host_addresses()),
        internet_reachable: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::testutil::TempDir;

    #[test]
    fn sysfs_interfaces() {
        let d = TempDir::new("net");
        d.write("sys/class/net/lo/operstate", b"unknown\n");
        d.write("sys/class/net/eth0/address", b"dc:a6:32:00:00:01\n");
        d.write("sys/class/net/eth0/operstate", b"up\n");
        d.write("sys/class/net/eth0/carrier", b"1\n");
        d.write("sys/class/net/wlan0/operstate", b"down\n");
        d.write("sys/class/net/wlan0/wireless/.keep", b"");
        let paths = HostPaths {
            proc_root: d.path().join("proc"),
            sys_root: d.path().join("sys"),
        };
        let mut addrs = BTreeMap::new();
        addrs.insert("eth0".to_string(), vec!["192.168.1.5/24".to_string()]);
        let ifs = interfaces_from_sysfs(&paths, &addrs);
        assert_eq!(ifs.len(), 2);
        assert_eq!(ifs[0].name, "eth0");
        assert_eq!(ifs[0].carrier, Some(true));
        assert_eq!(ifs[0].addresses, vec!["192.168.1.5/24"]);
        assert_eq!(ifs[1].name, "wlan0");
        assert!(ifs[1].wireless);
        assert_eq!(ifs[1].carrier, None);
    }

    #[test]
    fn netmask_prefix() {
        assert_eq!(prefix_len(&[255, 255, 255, 0]), 24);
        assert_eq!(prefix_len(&[0; 16]), 0);
    }

    #[test]
    fn host_addresses_does_not_crash() {
        let _ = host_addresses();
    }
}
