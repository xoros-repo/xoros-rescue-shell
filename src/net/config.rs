//! Automatic Ethernet DHCP (NET-1) and the app's resolver configuration (NET-3).
//!
//! For each `eth*`/`en*` interface a `50-xoros-rescue-<if>.network` file is written to
//! systemd-networkd's runtime directory and `networkctl reload` is run. If networkd is not
//! running, busybox `udhcpc` is started with a script of the app (it configures the address
//! and default route with `ip` and writes the DNS servers to the run directory, because
//! `/etc/resolv.conf` is read-only). Nothing is written to `/etc`.

use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use super::dns::{parse_resolv_conf, read_resolv_conf};
use super::NetEnv;
use crate::constants::{NETWORK_FILE_PREFIX, UDHCPC_SCRIPT_NAME};

/// Wired interfaces in sysfs (`eth*`, `en*`), sorted.
pub fn ethernet_interfaces(env: &NetEnv) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(env.host.sys("class/net"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| is_ethernet_name(n))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn is_ethernet_name(name: &str) -> bool {
    (name.starts_with("eth") || name.starts_with("en"))
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// `50-xoros-rescue-<if>.network`.
pub fn network_file_name(ifname: &str) -> String {
    format!("{NETWORK_FILE_PREFIX}{ifname}.network")
}

/// systemd-networkd unit for DHCP on `ifname` (NET-1).
pub fn dhcp_network_file(ifname: &str) -> String {
    format!(
        "# Written by the XorOS rescue shell (NET-1); lives in /run only.\n\
         [Match]\n\
         Name={ifname}\n\
         \n\
         [Network]\n\
         DHCP=yes\n\
         \n\
         [DHCPv4]\n\
         UseDNS=yes\n"
    )
}

/// udhcpc script (NET-1 fallback): address, default route, DNS servers to
/// `<resolv_conf>.<if>` and the merged `<resolv_conf>` (NET-3).
pub fn udhcpc_script(resolv_conf: &Path) -> String {
    let r = resolv_conf.display();
    format!(
        "#!/bin/sh\n\
         # udhcpc script of the XorOS rescue shell (NET-1, NET-3)\n\
         RESOLV='{r}'\n\
         case \"$1\" in\n\
         \x20 deconfig)\n\
         \x20   ip -4 addr flush dev \"$interface\"\n\
         \x20   ip link set \"$interface\" up\n\
         \x20   ;;\n\
         \x20 bound|renew)\n\
         \x20   ip -4 addr flush dev \"$interface\"\n\
         \x20   ip -4 addr add \"$ip/${{mask:-24}}\" dev \"$interface\"\n\
         \x20   for r in $router; do ip -4 route replace default via \"$r\" dev \"$interface\"; break; done\n\
         \x20   : > \"$RESOLV.$interface\"\n\
         \x20   for d in $dns; do echo \"nameserver $d\" >> \"$RESOLV.$interface\"; done\n\
         \x20   cat \"$RESOLV\".* > \"$RESOLV\" 2>/dev/null\n\
         \x20   ;;\n\
         esac\n\
         exit 0\n"
    )
}

/// What [`bring_up_ethernet`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DhcpReport {
    pub interfaces: Vec<String>,
    pub files: Vec<PathBuf>,
    /// `networkd` or `udhcpc`.
    pub method: String,
    pub notes: Vec<String>,
}

impl DhcpReport {
    pub fn text(&self) -> String {
        if self.interfaces.is_empty() {
            return "No Ethernet interface found.".into();
        }
        let mut t = format!(
            "DHCP on {} via {}.\n",
            self.interfaces.join(", "),
            self.method
        );
        for n in &self.notes {
            t.push_str(n);
            t.push('\n');
        }
        t
    }
}

pub(crate) fn write_file(path: &Path, content: &str, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)
}

pub(crate) fn networkd_running(env: &NetEnv) -> bool {
    match env.query_tool("systemctl", &["is-active", "systemd-networkd"]) {
        None => true, // simulation: pretend networkd runs
        Some(Ok(out)) => out.trim() == "active",
        Some(Err(e)) => {
            log::info!("systemd-networkd state unknown: {e}");
            false
        }
    }
}

/// NET-1: DHCP on every Ethernet interface. Errors of single steps are notes, not failures.
pub fn bring_up_ethernet(env: &NetEnv) -> io::Result<DhcpReport> {
    bring_up_ethernet_except(env, &[])
}

/// NET-1 for the Ethernet interfaces not in `skip` (those have their own configuration,
/// NET-2/NET-5).
pub fn bring_up_ethernet_except(env: &NetEnv, skip: &[String]) -> io::Result<DhcpReport> {
    let mut rep = DhcpReport {
        interfaces: ethernet_interfaces(env)
            .into_iter()
            .filter(|i| !skip.contains(i))
            .collect(),
        ..DhcpReport::default()
    };
    if rep.interfaces.is_empty() {
        log::info!("NET-1: no Ethernet interface");
        return Ok(rep);
    }
    if networkd_running(env) {
        rep.method = "systemd-networkd".into();
        for ifname in &rep.interfaces {
            let p = env.networkd_dir.join(network_file_name(ifname));
            write_file(&p, &dhcp_network_file(ifname), 0o644)?;
            log::info!("NET-1: wrote {}", p.display());
            rep.files.push(p);
        }
        if let Err(e) = env.run_tool("networkctl", &["reload"]) {
            rep.notes.push(format!("networkctl reload: {e}"));
        }
    } else {
        rep.method = "udhcpc".into();
        rep.files.push(env.run_dir.join(UDHCPC_SCRIPT_NAME));
        for ifname in &rep.interfaces {
            if let Err(e) = start_udhcpc(env, ifname) {
                rep.notes.push(format!("{ifname}: {e}"));
            }
        }
    }
    Ok(rep)
}

fn udhcpc_pid_file(env: &NetEnv, ifname: &str) -> PathBuf {
    env.run_dir.join(format!("udhcpc.{ifname}.pid"))
}

/// NET-1 fallback: (re)start busybox `udhcpc` in the background on `ifname`.
pub fn start_udhcpc(env: &NetEnv, ifname: &str) -> io::Result<()> {
    stop_udhcpc(env, ifname);
    let script = env.run_dir.join(UDHCPC_SCRIPT_NAME);
    write_file(&script, &udhcpc_script(&env.resolv_conf()), 0o755)?;
    let script_s = script.display().to_string();
    let pid_s = udhcpc_pid_file(env, ifname).display().to_string();
    let args = [
        "udhcpc", "-i", ifname, "-s", &script_s, "-p", &pid_s, "-t", "3", "-T", "2", "-b",
    ];
    env.run_tool("busybox", &args)
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Stop the `udhcpc` the app started on `ifname`, if any.
pub fn stop_udhcpc(env: &NetEnv, ifname: &str) {
    let pidf = udhcpc_pid_file(env, ifname);
    let pid = fs::read_to_string(&pidf)
        .ok()
        .and_then(|t| t.trim().parse::<i32>().ok())
        .filter(|p| *p > 1);
    if let Some(pid) = pid {
        if env.simulation {
            log::info!("SIM: would stop udhcpc (pid {pid})");
        } else {
            // SAFETY: plain kill(2) on a pid read from our own pid file.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            log::info!("NET-2: stopped udhcpc on {ifname} (pid {pid})");
        }
    }
    let _ = fs::remove_file(pidf);
}

/// DNS servers from systemd-networkd lease files (`DNS=a b c`).
pub fn lease_dns(leases_dir: &Path) -> Vec<IpAddr> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(leases_dir) else {
        return out;
    };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for f in files {
        if let Ok(text) = fs::read_to_string(&f) {
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("DNS=") {
                    out.extend(v.split_whitespace().filter_map(|a| a.parse::<IpAddr>().ok()));
                }
            }
        }
    }
    out
}

/// `resolv.conf` content for `servers`.
pub fn resolv_conf_text(servers: &[IpAddr]) -> String {
    let mut t = String::from("# Written by the XorOS rescue shell (NET-3).\n");
    for s in servers {
        t.push_str(&format!("nameserver {s}\n"));
    }
    t
}

/// NET-3: collect the DNS servers (networkd leases, udhcpc per-interface files, else the
/// host's `/etc/resolv.conf`) and write the app's `resolv.conf`. Returns the servers.
pub fn refresh_resolv_conf(env: &NetEnv) -> io::Result<Vec<IpAddr>> {
    let mut servers = lease_dns(&env.leases_dir);
    let own = env.resolv_conf();
    if let Ok(rd) = fs::read_dir(&env.run_dir) {
        let prefix = format!(
            "{}.",
            own.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(true, |x| x != "tmp"))
            .collect();
        files.sort();
        for f in files {
            servers.extend(read_resolv_conf(&f));
        }
    }
    if servers.is_empty() {
        // Static setups, or DIST-3 already linking /etc/resolv.conf to our file.
        if let Ok(t) = fs::read_to_string(&env.etc_resolv_conf) {
            servers = parse_resolv_conf(&t);
        }
    }
    let mut seen = Vec::new();
    servers.retain(|s| {
        let new = !seen.contains(s);
        seen.push(*s);
        new
    });
    write_file(&own, &resolv_conf_text(&servers), 0o644)?;
    log::info!(
        "NET-3: {} DNS server(s) in {}",
        servers.len(),
        own.display()
    );
    Ok(servers)
}

/// The servers the app resolves with: the app's `resolv.conf`, refreshed first if it is
/// missing or empty.
pub fn dns_servers(env: &NetEnv) -> Vec<IpAddr> {
    let v = read_resolv_conf(&env.resolv_conf());
    if !v.is_empty() {
        return v;
    }
    refresh_resolv_conf(env).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::testutil::TempDir;
    use crate::hw::HostPaths;

    fn env(d: &TempDir) -> NetEnv {
        NetEnv {
            run_dir: d.path().join("run"),
            networkd_dir: d.path().join("run/systemd/network"),
            leases_dir: d.path().join("leases"),
            etc_resolv_conf: d.path().join("etc/resolv.conf"),
            host: HostPaths {
                proc_root: d.path().join("proc"),
                sys_root: d.path().join("sys"),
            },
            simulation: true,
        }
    }

    #[test]
    fn network_file_generation() {
        assert_eq!(network_file_name("eth0"), "50-xoros-rescue-eth0.network");
        let f = dhcp_network_file("enp1s0");
        assert!(f.contains("[Match]\nName=enp1s0\n"));
        assert!(f.contains("[Network]\nDHCP=yes\n"));
        assert!(is_ethernet_name("enx001122334455"));
        assert!(!is_ethernet_name("wlan0"));
        assert!(!is_ethernet_name("eth0/../x"));
    }

    #[test]
    fn bring_up_in_simulation_writes_only_below_run() {
        let d = TempDir::new("net-cfg");
        for n in ["eth0", "enp2s0", "wlan0", "lo"] {
            d.write(&format!("sys/class/net/{n}/operstate"), b"up\n");
        }
        let e = env(&d);
        let rep = bring_up_ethernet(&e).unwrap();
        assert_eq!(rep.interfaces, vec!["enp2s0", "eth0"]);
        assert_eq!(rep.method, "systemd-networkd");
        let f = fs::read_to_string(e.networkd_dir.join("50-xoros-rescue-eth0.network")).unwrap();
        assert!(f.contains("Name=eth0"));
        assert!(rep.text().contains("enp2s0, eth0"));
        assert!(!e.networkd_dir.join("50-xoros-rescue-wlan0.network").exists());
    }

    #[test]
    fn udhcpc_script_text() {
        let s = udhcpc_script(Path::new("/run/xoros-rescue/resolv.conf"));
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains("RESOLV='/run/xoros-rescue/resolv.conf'"));
        assert!(s.contains("ip -4 addr add \"$ip/${mask:-24}\""));
        assert!(s.contains("  bound|renew)\n"));
    }

    #[test]
    fn resolv_conf_sources() {
        let d = TempDir::new("net-resolv");
        let e = env(&d);
        // Fallback: /etc/resolv.conf.
        d.write("etc/resolv.conf", b"nameserver 9.9.9.9\n");
        assert_eq!(
            refresh_resolv_conf(&e).unwrap(),
            vec!["9.9.9.9".parse::<IpAddr>().unwrap()]
        );
        // networkd lease and udhcpc file win, duplicates removed.
        d.write(
            "leases/2",
            b"# This is private data.\nADDRESS=192.168.1.5\nDNS=192.168.1.1 1.1.1.1\n",
        );
        d.write("run/resolv.conf.eth1", b"nameserver 1.1.1.1\nnameserver 8.8.8.8\n");
        let v = refresh_resolv_conf(&e).unwrap();
        let txt: Vec<String> = v.iter().map(|i| i.to_string()).collect();
        assert_eq!(txt, ["192.168.1.1", "1.1.1.1", "8.8.8.8"]);
        let own = fs::read_to_string(e.resolv_conf()).unwrap();
        assert!(own.contains("nameserver 192.168.1.1\nnameserver 1.1.1.1\n"));
        assert_eq!(dns_servers(&e).len(), 3);
    }
}
