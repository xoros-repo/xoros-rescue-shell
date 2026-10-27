//! Rescue network configuration per interface (NET-2) and remembering it for the next rescue
//! boot (NET-5).
//!
//! A configuration is a list of [`NetConfig`] (the connection model of `xoros_core::net::
//! nm_keyfile`, so the same values can be written as NetworkManager keyfiles for the normal
//! system, NET-6) plus the Wi-Fi country code. The current one lives in
//! `<run dir>/network.json`; "remember" copies it to `overlay:/rescue/network.json`. Both files
//! have mode 0600 and WPA2 passphrases are stored as hex PSK only ([`wifi::hash_secret`]).
//!
//! Applying a connection writes the interface's `.network` file for systemd-networkd (DHCP or
//! static address, gateway, DNS) and starts `wpa_supplicant` for Wi-Fi; without networkd the
//! address is set with `ip` or `udhcpc` (NET-1 fallback). Static DNS servers go to
//! `<run dir>/resolv.conf.<if>`, which [`config::refresh_resolv_conf`] merges (NET-3).

use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use xoros_core::net::nm_keyfile::{Ipv4Config, Medium, NetConfig, Wifi, WifiSecurity};

use super::{config, wifi, NetEnv};
use crate::backend::{PartRole, SystemBackend};
use crate::constants::{NETWORK_JSON_NAME, RESCUE_DIR, RESOLV_CONF_NAME};

pub const FORMAT_VERSION: u32 = 1;

fn format_version() -> u32 {
    FORMAT_VERSION
}

/// The rescue network configuration (`network.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescueNetConfig {
    #[serde(default = "format_version")]
    pub version: u32,
    /// Wi-Fi regulatory country (`iw reg set`), e.g. `DE`.
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub connections: Vec<NetConfig>,
}

impl Default for RescueNetConfig {
    fn default() -> Self {
        RescueNetConfig {
            version: FORMAT_VERSION,
            country: None,
            connections: Vec::new(),
        }
    }
}

impl RescueNetConfig {
    pub fn get(&self, ifname: &str) -> Option<&NetConfig> {
        self.connections
            .iter()
            .find(|c| c.interface.as_deref() == Some(ifname))
    }

    /// Add or replace the connection of the same interface (keeps its UUID, NET-6).
    pub fn upsert(&mut self, mut c: NetConfig) {
        match self
            .connections
            .iter_mut()
            .find(|o| o.interface.is_some() && o.interface == c.interface)
        {
            Some(o) => {
                c.uuid = o.uuid.clone();
                *o = c;
            }
            None => self.connections.push(c),
        }
        self.connections
            .sort_by(|a, b| a.interface.cmp(&b.interface));
    }

    pub fn remove(&mut self, ifname: &str) -> bool {
        let n = self.connections.len();
        self.connections
            .retain(|c| c.interface.as_deref() != Some(ifname));
        n != self.connections.len()
    }

    pub fn interfaces(&self) -> Vec<String> {
        self.connections
            .iter()
            .filter_map(|c| c.interface.clone())
            .collect()
    }

    /// A copy without secrets, for reports and the diagnostics bundle (LOG-2).
    pub fn redacted(&self) -> RescueNetConfig {
        let mut r = self.clone();
        for c in &mut r.connections {
            if let Medium::Wifi(w) = &mut c.medium {
                w.security = match &w.security {
                    WifiSecurity::WpaPsk(_) => WifiSecurity::WpaPsk("<hidden>".into()),
                    WifiSecurity::Sae(_) => WifiSecurity::Sae("<hidden>".into()),
                    other => other.clone(),
                };
            }
        }
        r
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into()) + "\n"
    }

    pub fn from_json(text: &str) -> Result<RescueNetConfig, String> {
        serde_json::from_str(text).map_err(|e| format!("network.json: {e}"))
    }
}

/// One line describing a connection (no secrets).
pub fn describe(c: &NetConfig) -> String {
    let ifname = c.interface.as_deref().unwrap_or("?");
    let medium = match &c.medium {
        Medium::Ethernet => "ethernet".to_string(),
        Medium::Wifi(w) => {
            let sec = match &w.security {
                WifiSecurity::Open => "open",
                WifiSecurity::WpaPsk(_) => "WPA2",
                WifiSecurity::Sae(_) => "WPA3",
                #[allow(unreachable_patterns)]
                _ => "?",
            };
            format!(
                "Wi-Fi '{}' {sec}{}",
                w.ssid,
                if w.hidden { " hidden" } else { "" }
            )
        }
        #[allow(unreachable_patterns)]
        _ => "?".to_string(),
    };
    let ip = match &c.ipv4 {
        Ipv4Config::Dhcp => "DHCP".to_string(),
        Ipv4Config::Static {
            address,
            prefix,
            gateway,
        } => format!(
            "static {address}/{prefix}{}",
            gateway.map(|g| format!(" via {g}")).unwrap_or_default()
        ),
        #[allow(unreachable_patterns)]
        _ => "?".to_string(),
    };
    let dns = if c.dns.is_empty() {
        String::new()
    } else {
        format!(
            "  DNS {}",
            c.dns
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    format!("{ifname}: {medium}, {ip}{dns}")
}

// --- building a connection from user input -------------------------------------------------

/// Parse `a.b.c.d/nn`.
pub fn parse_cidr(s: &str) -> Result<(Ipv4Addr, u8), String> {
    let (a, p) = s
        .trim()
        .split_once('/')
        .ok_or_else(|| format!("'{s}': use address/prefix, e.g. 192.168.1.50/24."))?;
    let addr: Ipv4Addr = a
        .parse()
        .map_err(|_| format!("'{a}' is not an IPv4 address."))?;
    let prefix: u8 = p
        .parse()
        .ok()
        .filter(|p| (1..=32).contains(p))
        .ok_or_else(|| format!("'{p}' is not a prefix length (1..32)."))?;
    Ok((addr, prefix))
}

pub fn parse_ipv4(s: &str, what: &str) -> Result<Ipv4Addr, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("{what}: '{s}' is not an IPv4 address."))
}

/// DNS servers separated by commas, semicolons or spaces.
pub fn parse_dns_list(s: &str) -> Result<Vec<Ipv4Addr>, String> {
    s.split([',', ';', ' '])
        .filter(|x| !x.trim().is_empty())
        .map(|x| parse_ipv4(x, "DNS"))
        .collect()
}

/// User input for one interface (TUI form, CLI options).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigInput {
    pub interface: String,
    /// `None`: ethernet.
    pub wifi: Option<WifiInput>,
    /// `None`: DHCP; else (`addr/prefix`, gateway).
    pub static_ip: Option<(String, String)>,
    pub dns: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WifiInput {
    pub ssid: String,
    pub hidden: bool,
    /// `open`, `wpa2` (WPA-PSK) or `wpa3` (SAE).
    pub security: String,
    pub passphrase: String,
}

/// An empty passphrase means "keep the stored secret" when the network (SSID and security)
/// is the same as in `existing` (the stored WPA2 secret is a hex PSK, never shown).
pub fn keep_secret(input: &mut ConfigInput, existing: Option<&NetConfig>) {
    let (Some(w), Some(old)) = (input.wifi.as_mut(), existing) else {
        return;
    };
    let Medium::Wifi(ow) = &old.medium else {
        return;
    };
    if !w.passphrase.is_empty() || w.ssid != ow.ssid {
        return;
    }
    match (w.security.as_str(), &ow.security) {
        ("wpa2", WifiSecurity::WpaPsk(p)) | ("wpa3", WifiSecurity::Sae(p)) => {
            w.passphrase = p.clone()
        }
        _ => {}
    }
}

/// Input that reproduces `c` (the form is pre-filled with it; secrets stay empty).
pub fn input_from(c: &NetConfig) -> ConfigInput {
    let wifi = match &c.medium {
        Medium::Wifi(w) => Some(WifiInput {
            ssid: w.ssid.clone(),
            hidden: w.hidden,
            security: match &w.security {
                WifiSecurity::WpaPsk(_) => "wpa2",
                WifiSecurity::Sae(_) => "wpa3",
                _ => "open",
            }
            .to_string(),
            passphrase: String::new(),
        }),
        _ => None,
    };
    let static_ip = match &c.ipv4 {
        Ipv4Config::Static {
            address,
            prefix,
            gateway,
        } => Some((
            format!("{address}/{prefix}"),
            gateway.map(|g| g.to_string()).unwrap_or_default(),
        )),
        _ => None,
    };
    ConfigInput {
        interface: c.interface.clone().unwrap_or_default(),
        wifi,
        static_ip,
        dns: c
            .dns
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Build and check a connection from user input (NET-2). WPA2 passphrases become hex PSKs.
pub fn build(input: &ConfigInput) -> Result<NetConfig, String> {
    let ifname = input.interface.trim();
    if ifname.is_empty() {
        return Err("No interface given.".into());
    }
    let mut c = match &input.wifi {
        None => NetConfig::ethernet(ifname),
        Some(w) => {
            let security = match w.security.to_ascii_lowercase().as_str() {
                "open" | "none" => WifiSecurity::Open,
                "wpa2" | "wpa" | "wpa-psk" | "psk" => WifiSecurity::WpaPsk(w.passphrase.clone()),
                "wpa3" | "sae" => WifiSecurity::Sae(w.passphrase.clone()),
                other => {
                    return Err(format!(
                        "Unknown Wi-Fi security '{other}' (open, wpa2, wpa3)."
                    ))
                }
            };
            let mut wifi = Wifi::new(w.ssid.clone(), security);
            wifi.hidden = w.hidden;
            NetConfig::wifi(ifname, wifi)
        }
    };
    if let Some((cidr, gw)) = &input.static_ip {
        let (addr, prefix) = parse_cidr(cidr)?;
        let gateway = if gw.trim().is_empty() {
            None
        } else {
            Some(parse_ipv4(gw, "Gateway")?)
        };
        c = c.with_static(addr, prefix, gateway);
    }
    c = c.with_dns(parse_dns_list(&input.dns)?);
    c.validate().map_err(|e| e.to_string())?;
    if let Medium::Wifi(w) = &mut c.medium {
        wifi::hash_secret(w);
    }
    Ok(c)
}

// --- applying ----------------------------------------------------------------------------

/// systemd-networkd unit for a connection (NET-2): DHCP or static address, gateway, DNS.
pub fn network_file(c: &NetConfig) -> String {
    let ifname = c.interface.as_deref().unwrap_or("");
    let mut t = format!(
        "# Written by the XorOS rescue shell (NET-2); lives in /run only.\n\
         [Match]\n\
         Name={ifname}\n\
         \n\
         [Network]\n"
    );
    match &c.ipv4 {
        Ipv4Config::Static {
            address,
            prefix,
            gateway,
        } => {
            t.push_str(&format!("Address={address}/{prefix}\n"));
            if let Some(g) = gateway {
                t.push_str(&format!("Gateway={g}\n"));
            }
        }
        _ => t.push_str("DHCP=yes\n"),
    }
    for d in &c.dns {
        t.push_str(&format!("DNS={d}\n"));
    }
    if matches!(c.ipv4, Ipv4Config::Dhcp) {
        t.push_str(&format!(
            "\n[DHCPv4]\nUseDNS={}\n",
            if c.dns.is_empty() { "yes" } else { "no" }
        ));
    }
    t
}

fn static_resolv(env: &NetEnv, ifname: &str) -> PathBuf {
    env.run_dir.join(format!("{RESOLV_CONF_NAME}.{ifname}"))
}

/// What [`apply`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub lines: Vec<String>,
}

impl ApplyReport {
    pub fn text(&self) -> String {
        self.lines.iter().map(|l| format!("{l}\n")).collect()
    }
}

fn io_err(msg: String) -> io::Error {
    io::Error::other(msg)
}

/// NET-2: configure the interface of `c` now. Errors of auxiliary steps are notes.
pub fn apply(env: &NetEnv, c: &NetConfig, country: Option<&str>) -> io::Result<ApplyReport> {
    let ifname = c
        .interface
        .clone()
        .ok_or_else(|| io_err("The connection has no interface.".into()))?;
    let mut rep = ApplyReport::default();
    rep.lines.push(format!("Configuring {}", describe(c)));
    match &c.medium {
        Medium::Wifi(w) => {
            for n in wifi::start_supplicant(env, &ifname, w, country)? {
                rep.lines.push(n);
            }
            rep.lines
                .push(format!("wpa_supplicant started on {ifname}"));
        }
        _ => wifi::stop_supplicant(env, &ifname),
    }
    // Static DNS servers for the app's resolver (NET-3).
    let rf = static_resolv(env, &ifname);
    if c.dns.is_empty() {
        let _ = fs::remove_file(&rf);
    } else {
        let servers: Vec<std::net::IpAddr> = c.dns.iter().map(|d| (*d).into()).collect();
        config::write_file(&rf, &config::resolv_conf_text(&servers), 0o644)?;
    }
    if config::networkd_running(env) {
        let p = env.networkd_dir.join(config::network_file_name(&ifname));
        config::write_file(&p, &network_file(c), 0o644)?;
        rep.lines.push(format!("wrote {}", p.display()));
        if let Err(e) = env.run_tool("networkctl", &["reload"]) {
            rep.lines.push(format!("networkctl reload: {e}"));
        }
        if let Err(e) = env.run_tool("networkctl", &["reconfigure", &ifname]) {
            rep.lines.push(format!("networkctl reconfigure: {e}"));
        }
    } else {
        rep.lines
            .push("systemd-networkd is not running: using ip/udhcpc".into());
        config::stop_udhcpc(env, &ifname);
        match &c.ipv4 {
            Ipv4Config::Static {
                address,
                prefix,
                gateway,
            } => {
                let cidr = format!("{address}/{prefix}");
                env.run_tool("ip", &["-4", "addr", "flush", "dev", &ifname])
                    .map_err(wifi::tool_io)?;
                env.run_tool("ip", &["link", "set", &ifname, "up"])
                    .map_err(wifi::tool_io)?;
                env.run_tool("ip", &["-4", "addr", "add", &cidr, "dev", &ifname])
                    .map_err(wifi::tool_io)?;
                if let Some(g) = gateway {
                    let g = g.to_string();
                    env.run_tool(
                        "ip",
                        &[
                            "-4", "route", "replace", "default", "via", &g, "dev", &ifname,
                        ],
                    )
                    .map_err(wifi::tool_io)?;
                }
            }
            _ => config::start_udhcpc(env, &ifname)?,
        }
    }
    Ok(rep)
}

/// Remove the app's configuration of `ifname` (`.network` file, wpa_supplicant, udhcpc).
pub fn unapply(env: &NetEnv, ifname: &str) -> io::Result<()> {
    wifi::stop_supplicant(env, ifname);
    config::stop_udhcpc(env, ifname);
    let _ = fs::remove_file(static_resolv(env, ifname));
    let p = env.networkd_dir.join(config::network_file_name(ifname));
    match fs::remove_file(&p) {
        Ok(()) => {
            log::info!("NET-2: removed {}", p.display());
            if let Err(e) = env.run_tool("networkctl", &["reload"]) {
                log::warn!("networkctl reload: {e}");
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

// --- current configuration (run directory) -------------------------------------------------

pub fn state_path(env: &NetEnv) -> PathBuf {
    env.run_dir.join(NETWORK_JSON_NAME)
}

/// The current rescue configuration (empty if none was made in this boot).
pub fn load_state(env: &NetEnv) -> RescueNetConfig {
    match fs::read_to_string(state_path(env)) {
        Ok(t) => RescueNetConfig::from_json(&t).unwrap_or_else(|e| {
            log::warn!("{e}");
            RescueNetConfig::default()
        }),
        Err(_) => RescueNetConfig::default(),
    }
}

pub fn save_state(env: &NetEnv, cfg: &RescueNetConfig) -> io::Result<()> {
    config::write_file(&state_path(env), &cfg.to_json(), 0o600)
}

/// NET-2: apply `c` (unless `apply_now` is false), then record it in the current
/// configuration. A new country code replaces the old one.
pub fn configure(
    env: &NetEnv,
    c: NetConfig,
    country: Option<String>,
    apply_now: bool,
) -> io::Result<ApplyReport> {
    let mut state = load_state(env);
    if country.is_some() {
        state.country = country;
    }
    let mut rep = if apply_now {
        let r = apply(env, &c, state.country.as_deref())?;
        if let Err(e) = config::refresh_resolv_conf(env) {
            log::warn!("NET-3: {e}");
        }
        r
    } else {
        ApplyReport {
            lines: vec![format!("Saved (not applied): {}", describe(&c))],
        }
    };
    log::info!("NET-2: {}", describe(&c));
    state.upsert(c);
    save_state(env, &state)?;
    rep.lines.push(format!(
        "Current configuration: {}",
        state_path(env).display()
    ));
    Ok(rep)
}

/// Undo [`configure`] for `ifname`.
pub fn unconfigure(env: &NetEnv, ifname: &str) -> io::Result<bool> {
    let mut state = load_state(env);
    let found = state.remove(ifname);
    unapply(env, ifname)?;
    save_state(env, &state)?;
    let _ = config::refresh_resolv_conf(env);
    Ok(found)
}

// --- remembered configuration (overlay partition, NET-5) -----------------------------------

/// `rescue/network.json` relative to the overlay partition root.
pub fn remembered_rel() -> PathBuf {
    Path::new(RESCUE_DIR).join(NETWORK_JSON_NAME)
}

/// Store `cfg` on the overlay partition (NET-5): secrets hashed, mode 0600.
pub fn remember(backend: &dyn SystemBackend, cfg: &RescueNetConfig) -> Result<PathBuf, String> {
    let mut cfg = cfg.clone();
    for c in &mut cfg.connections {
        if let Medium::Wifi(w) = &mut c.medium {
            wifi::hash_secret(w);
        }
    }
    let guard = backend
        .mount_partition(PartRole::Overlay, true)
        .map_err(|e| e.to_string())?;
    let path = guard.path().join(remembered_rel());
    config::write_file(&path, &cfg.to_json(), 0o600)
        .and_then(|()| fs::File::open(&path)?.sync_all())
        .map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
    guard.unmount().map_err(|e| e.to_string())?;
    log::info!(
        "NET-5: remembered {} connection(s) in overlay:/{}",
        cfg.connections.len(),
        remembered_rel().display()
    );
    Ok(remembered_rel())
}

/// Delete the remembered configuration; `Ok(false)` if there was none.
pub fn forget(backend: &dyn SystemBackend) -> Result<bool, String> {
    let guard = backend
        .mount_partition(PartRole::Overlay, true)
        .map_err(|e| e.to_string())?;
    let path = guard.path().join(remembered_rel());
    let r = match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("Cannot delete {}: {e}", path.display())),
    };
    guard.unmount().map_err(|e| e.to_string())?;
    if r == Ok(true) {
        log::info!("NET-5: remembered network configuration deleted");
    }
    r
}

/// The remembered configuration, `Ok(None)` if there is none (the overlay is mounted
/// read-only for this).
pub fn load_remembered(backend: &dyn SystemBackend) -> Result<Option<RescueNetConfig>, String> {
    let guard = backend
        .mount_partition(PartRole::Overlay, false)
        .map_err(|e| e.to_string())?;
    let path = guard.path().join(remembered_rel());
    let r = match fs::read_to_string(&path) {
        Ok(t) => RescueNetConfig::from_json(&t).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Cannot read {}: {e}", path.display())),
    };
    drop(guard);
    r
}

/// Start-up network setup (NET-1, NET-5): apply the remembered configuration if there is one,
/// then DHCP on the other Ethernet interfaces. Returns log lines.
pub fn startup(env: &NetEnv, backend: &dyn SystemBackend) -> Vec<String> {
    let mut lines = Vec::new();
    let mut skip = Vec::new();
    match load_remembered(backend) {
        Ok(Some(cfg)) => {
            lines.push(format!(
                "NET-5: applying the remembered configuration ({} connection(s))",
                cfg.connections.len()
            ));
            for c in &cfg.connections {
                match apply(env, c, cfg.country.as_deref()) {
                    Ok(r) => lines.extend(r.lines),
                    Err(e) => lines.push(format!("NET-5: {}: {e}", describe(c))),
                }
            }
            skip = cfg.interfaces();
            if let Err(e) = save_state(env, &cfg) {
                lines.push(format!("NET-5: {e}"));
            }
        }
        Ok(None) => {}
        Err(e) => lines.push(format!("NET-5: no remembered configuration ({e})")),
    }
    match config::bring_up_ethernet_except(env, &skip) {
        Ok(r) if !r.interfaces.is_empty() => lines.push(format!("NET-1: {}", r.text().trim())),
        Ok(_) => {}
        Err(e) => lines.push(format!("NET-1: {e}")),
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
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

    fn static_input() -> ConfigInput {
        ConfigInput {
            interface: "eth0".into(),
            wifi: None,
            static_ip: Some(("10.0.2.50/24".into(), "10.0.2.2".into())),
            dns: "10.0.2.3, 1.1.1.1".into(),
        }
    }

    #[test]
    fn build_and_network_file() {
        let c = build(&static_input()).unwrap();
        let f = network_file(&c);
        assert!(f.contains("Name=eth0\n"));
        assert!(f.contains("Address=10.0.2.50/24\nGateway=10.0.2.2\nDNS=10.0.2.3\nDNS=1.1.1.1\n"));
        assert!(!f.contains("DHCP"));
        assert_eq!(
            describe(&c),
            "eth0: ethernet, static 10.0.2.50/24 via 10.0.2.2  DNS 10.0.2.3,1.1.1.1"
        );
        let d = build(&ConfigInput {
            interface: "eth1".into(),
            ..ConfigInput::default()
        })
        .unwrap();
        let f = network_file(&d);
        assert!(f.contains("DHCP=yes\n") && f.contains("UseDNS=yes\n"));
        assert!(build(&ConfigInput::default()).is_err());
        let mut bad = static_input();
        bad.static_ip = Some(("10.0.2.50".into(), String::new()));
        assert!(build(&bad).is_err());
        bad.static_ip = Some(("10.0.2.50/33".into(), String::new()));
        assert!(build(&bad).is_err());
        bad.static_ip = None;
        bad.dns = "1.2.3".into();
        assert!(build(&bad).is_err());
    }

    #[test]
    fn wifi_build_hashes_psk() {
        let mut i = ConfigInput {
            interface: "wlan0".into(),
            wifi: Some(WifiInput {
                ssid: "IEEE".into(),
                hidden: true,
                security: "wpa2".into(),
                passphrase: "password".into(),
            }),
            ..ConfigInput::default()
        };
        let c = build(&i).unwrap();
        match &c.medium {
            Medium::Wifi(w) => {
                assert!(w.hidden);
                assert_eq!(
                    w.security,
                    WifiSecurity::WpaPsk(
                        "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e".into()
                    )
                );
            }
            _ => panic!("not wifi"),
        }
        let red = RescueNetConfig {
            connections: vec![c],
            ..Default::default()
        }
        .redacted();
        assert!(!red.to_json().contains("f42c6f"));
        i.wifi.as_mut().unwrap().passphrase = "short".into();
        assert!(build(&i).is_err());
        i.wifi.as_mut().unwrap().security = "wep".into();
        assert!(build(&i).is_err());
    }

    #[test]
    fn form_round_trip_keeps_secret() {
        let i = ConfigInput {
            interface: "wlan0".into(),
            wifi: Some(WifiInput {
                ssid: "Lab".into(),
                hidden: false,
                security: "wpa2".into(),
                passphrase: "password".into(),
            }),
            static_ip: Some(("192.168.1.9/24".into(), String::new())),
            dns: "9.9.9.9".into(),
        };
        let c = build(&i).unwrap();
        let mut again = input_from(&c);
        assert_eq!(again.static_ip, i.static_ip);
        assert_eq!(again.dns, "9.9.9.9");
        assert_eq!(again.wifi.as_ref().unwrap().passphrase, "");
        keep_secret(&mut again, Some(&c));
        assert_eq!(build(&again).unwrap().medium, c.medium);
        // Another network: the secret is not reused.
        let mut other = input_from(&c);
        other.wifi.as_mut().unwrap().ssid = "Other".into();
        keep_secret(&mut other, Some(&c));
        assert!(build(&other).is_err());
    }

    #[test]
    fn upsert_keeps_uuid() {
        let mut s = RescueNetConfig::default();
        let a = build(&static_input()).unwrap();
        let uuid = a.uuid.clone();
        s.upsert(a);
        let mut i = static_input();
        i.static_ip = None;
        s.upsert(build(&i).unwrap());
        assert_eq!(s.connections.len(), 1);
        assert_eq!(s.connections[0].uuid, uuid);
        assert_eq!(s.connections[0].ipv4, Ipv4Config::Dhcp);
        let back = RescueNetConfig::from_json(&s.to_json()).unwrap();
        assert_eq!(back, s);
        assert!(s.remove("eth0"));
        assert!(!s.remove("eth0"));
    }

    #[test]
    fn configure_in_simulation() {
        let d = TempDir::new("net-profile");
        let e = env(&d);
        let rep = configure(&e, build(&static_input()).unwrap(), None, true).unwrap();
        assert!(rep.text().contains("50-xoros-rescue-eth0.network"));
        let f = fs::read_to_string(e.networkd_dir.join("50-xoros-rescue-eth0.network")).unwrap();
        assert!(f.contains("Address=10.0.2.50/24"));
        let r = fs::read_to_string(e.run_dir.join("resolv.conf")).unwrap();
        assert!(r.contains("nameserver 10.0.2.3\nnameserver 1.1.1.1\n"));
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(state_path(&e)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(load_state(&e).interfaces(), vec!["eth0"]);
        assert!(unconfigure(&e, "eth0").unwrap());
        assert!(load_state(&e).connections.is_empty());
        assert!(!e.networkd_dir.join("50-xoros-rescue-eth0.network").exists());
    }

    #[test]
    fn remember_and_startup() {
        let d = TempDir::new("net-remember");
        let sim = d.path().join("sim");
        SimBackend::create_example(&sim).unwrap();
        let b = SimBackend::open(sim.clone()).unwrap();
        assert_eq!(load_remembered(&b).unwrap(), None);
        let mut cfg = RescueNetConfig {
            country: Some("DE".into()),
            ..Default::default()
        };
        let mut w = Wifi::new("Lab", WifiSecurity::WpaPsk("password".into()));
        w.hidden = false;
        cfg.upsert(NetConfig::wifi("wlan0", w));
        remember(&b, &cfg).unwrap();
        let file = sim.join("parts/overlay/rescue/network.json");
        let text = fs::read_to_string(&file).unwrap();
        assert!(!text.contains("\"password\""), "{text}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let back = load_remembered(&b).unwrap().unwrap();
        assert_eq!(back.country.as_deref(), Some("DE"));

        let e = env(&d);
        d.write("sys/class/net/eth0/operstate", b"up\n");
        let lines = startup(&e, &b);
        assert!(lines.iter().any(|l| l.contains("remembered")), "{lines:?}");
        assert!(e
            .networkd_dir
            .join("50-xoros-rescue-wlan0.network")
            .exists());
        assert!(e.networkd_dir.join("50-xoros-rescue-eth0.network").exists());
        let conf = fs::read_to_string(e.run_dir.join("wpa_supplicant-wlan0.conf")).unwrap();
        assert!(conf.contains("country=DE") && conf.contains("psk="));
        assert_eq!(load_state(&e).interfaces(), vec!["wlan0"]);
        assert!(forget(&b).unwrap());
        assert!(!forget(&b).unwrap());
    }
}
