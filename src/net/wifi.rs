//! Wi-Fi in the rescue system (NET-2): scan, `wpa_supplicant` configuration, the WPA PSK
//! (NET-5 stores passphrases as `wpa_passphrase` would print them), rfkill and the regulatory
//! domain.
//!
//! `wpa_supplicant` runs with a configuration in the run directory
//! (`wpa_supplicant-<if>.conf`, mode 0600) because the stock `wpa_supplicant@.service` reads
//! `/etc`, which is read-only. Addressing is done by the `.network` file of the interface
//! ([`super::profile`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use xoros_core::net::nm_keyfile::{Wifi, WifiSecurity};

use super::NetEnv;
use crate::cmd::ToolError;

/// One network found by a scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScanEntry {
    pub ssid: String,
    pub bssid: String,
    /// Signal in dBm (e.g. -55).
    pub signal: Option<i32>,
    pub frequency: Option<u32>,
    pub security: ScanSecurity,
}

/// Security offered by a scanned network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScanSecurity {
    Open,
    /// WPA/WPA2 personal.
    WpaPsk,
    /// WPA3 personal.
    Sae,
    /// WPA2/WPA3 transition mode: both work.
    WpaPskSae,
    /// Enterprise (802.1X) or WEP: not supported by the rescue shell.
    Unsupported,
}

impl ScanSecurity {
    pub fn label(self) -> &'static str {
        match self {
            ScanSecurity::Open => "open",
            ScanSecurity::WpaPsk => "WPA2",
            ScanSecurity::Sae => "WPA3",
            ScanSecurity::WpaPskSae => "WPA2/3",
            ScanSecurity::Unsupported => "unsupported",
        }
    }
}

impl ScanEntry {
    /// One line for a selection list.
    pub fn label(&self) -> String {
        let sig = self
            .signal
            .map(|s| format!("{s:>4} dBm"))
            .unwrap_or_else(|| "    ?   ".into());
        let ssid = if self.ssid.is_empty() {
            "(hidden)".to_string()
        } else {
            crate::report::ellipsize(&self.ssid, 32)
        };
        format!("{ssid:<32} {sig}  {}", self.security.label())
    }
}

/// Security from `wpa_cli scan_results` flags, e.g. `[WPA2-PSK-CCMP][ESS]`.
fn security_from_flags(flags: &str) -> ScanSecurity {
    let psk = flags.contains("-PSK");
    let sae = flags.contains("SAE");
    if flags.contains("EAP") || flags.contains("WEP") {
        ScanSecurity::Unsupported
    } else if psk && sae {
        ScanSecurity::WpaPskSae
    } else if sae {
        ScanSecurity::Sae
    } else if psk {
        ScanSecurity::WpaPsk
    } else {
        ScanSecurity::Open
    }
}

/// Decode the `\xNN` escapes `iw` and `wpa_cli` use for non-printable SSID bytes.
fn unescape_ssid(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
            let hex = std::str::from_utf8(&b[i + 2..i + 4]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Sort by signal (strongest first) and keep the strongest entry per SSID.
fn dedup(mut v: Vec<ScanEntry>) -> Vec<ScanEntry> {
    v.sort_by_key(|e| std::cmp::Reverse(e.signal.unwrap_or(-200)));
    let mut seen: Vec<String> = Vec::new();
    v.retain(|e| {
        if e.ssid.is_empty() {
            return true;
        }
        let new = !seen.contains(&e.ssid);
        seen.push(e.ssid.clone());
        new
    });
    v
}

/// Parse `wpa_cli scan_results`: `bssid / frequency / signal level / flags / ssid`.
pub fn parse_wpa_cli_scan(text: &str) -> Vec<ScanEntry> {
    let v = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(5, '\t').collect();
            if f.len() < 4 || f[0].len() != 17 || f[0].matches(':').count() != 5 {
                return None;
            }
            Some(ScanEntry {
                bssid: f[0].to_string(),
                frequency: f[1].trim().parse().ok(),
                signal: f[2].trim().parse().ok(),
                security: security_from_flags(f[3]),
                ssid: unescape_ssid(f.get(4).copied().unwrap_or("")),
            })
        })
        .collect();
    dedup(v)
}

/// Parse `iw dev <if> scan`.
pub fn parse_iw_scan(text: &str) -> Vec<ScanEntry> {
    struct Cur {
        e: ScanEntry,
        rsn_or_wpa: bool,
        psk: bool,
        sae: bool,
        eap: bool,
        privacy: bool,
    }
    fn finish(c: Cur) -> ScanEntry {
        let mut e = c.e;
        e.security = if c.eap {
            ScanSecurity::Unsupported
        } else if c.psk && c.sae {
            ScanSecurity::WpaPskSae
        } else if c.sae {
            ScanSecurity::Sae
        } else if c.psk || c.rsn_or_wpa {
            ScanSecurity::WpaPsk
        } else if c.privacy {
            ScanSecurity::Unsupported // WEP
        } else {
            ScanSecurity::Open
        };
        e
    }
    let mut out = Vec::new();
    let mut cur: Option<Cur> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("BSS ") {
            if let Some(c) = cur.take() {
                out.push(finish(c));
            }
            let bssid: String = rest.chars().take(17).collect();
            cur = Some(Cur {
                e: ScanEntry {
                    ssid: String::new(),
                    bssid,
                    signal: None,
                    frequency: None,
                    security: ScanSecurity::Open,
                },
                rsn_or_wpa: false,
                psk: false,
                sae: false,
                eap: false,
                privacy: false,
            });
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        let t = line.trim();
        if let Some(v) = t.strip_prefix("SSID: ") {
            c.e.ssid = unescape_ssid(v);
        } else if t == "SSID:" {
            c.e.ssid.clear();
        } else if let Some(v) = t.strip_prefix("signal: ") {
            c.e.signal = v
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<f64>().ok())
                .map(|f| f.round() as i32);
        } else if let Some(v) = t.strip_prefix("freq: ") {
            c.e.frequency = v.split(['.', ' ']).next().and_then(|n| n.parse().ok());
        } else if t.starts_with("RSN:") || t.starts_with("WPA:") {
            c.rsn_or_wpa = true;
        } else if t.starts_with("capability:") && t.contains("Privacy") {
            c.privacy = true;
        } else if let Some(v) = t.strip_prefix("* Authentication suites:") {
            c.psk |= v.contains("PSK");
            c.sae |= v.contains("SAE");
            c.eap |= v.contains("IEEE 802.1X") || v.contains("EAP");
        }
    }
    if let Some(c) = cur.take() {
        out.push(finish(c));
    }
    dedup(out)
}

// --- WPA PSK (PBKDF2-HMAC-SHA1, IEEE 802.11i) -------------------------------------------

/// SHA-1 (only used for the WPA PSK derivation, where it is mandated by the standard).
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&sha1(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    inner.extend_from_slice(msg);
    let mut outer: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    outer.extend_from_slice(&sha1(&inner));
    sha1(&outer)
}

/// The 256-bit WPA PSK of `passphrase` for `ssid` as 64 hex digits, exactly what
/// `wpa_passphrase` prints as `psk=` (NET-5).
pub fn wpa_psk_hex(ssid: &str, passphrase: &str) -> String {
    let mut out = [0u8; 32];
    for (block, chunk) in out.chunks_mut(20).enumerate() {
        let mut salt = ssid.as_bytes().to_vec();
        salt.extend_from_slice(&(block as u32 + 1).to_be_bytes());
        let mut u = hmac_sha1(passphrase.as_bytes(), &salt);
        let mut t = u;
        for _ in 1..4096 {
            u = hmac_sha1(passphrase.as_bytes(), &u);
            for (x, y) in t.iter_mut().zip(u.iter()) {
                *x ^= y;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// `true` for a 64-hex-digit PSK (as opposed to a passphrase).
pub fn is_hex_psk(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// Replace a WPA2 passphrase with its hex PSK (NET-5: nothing in clear text). WPA3-SAE needs
/// the passphrase itself and is returned unchanged.
pub fn hash_secret(w: &mut Wifi) {
    if let WifiSecurity::WpaPsk(p) = &w.security {
        if !is_hex_psk(p) {
            w.security = WifiSecurity::WpaPsk(wpa_psk_hex(&w.ssid, p));
        }
    }
}

// --- wpa_supplicant ---------------------------------------------------------------------

/// Quote a value for wpa_supplicant.conf: `"text"` for plain printable ASCII, otherwise the
/// hex form (`ssid=6869` without quotes) wpa_supplicant also accepts.
fn conf_string(s: &str) -> String {
    if s.bytes().all(|c| (0x20..0x7f).contains(&c) && c != b'"') {
        format!("\"{s}\"")
    } else {
        s.bytes().map(|b| format!("{b:02x}")).collect()
    }
}

/// Control socket directory for `wpa_cli -p` (below the run directory).
pub fn ctrl_dir(env: &NetEnv) -> PathBuf {
    env.run_dir.join("wpa_supplicant")
}

pub fn conf_path(env: &NetEnv, ifname: &str) -> PathBuf {
    env.run_dir.join(format!("wpa_supplicant-{ifname}.conf"))
}

fn pid_path(env: &NetEnv, ifname: &str) -> PathBuf {
    env.run_dir.join(format!("wpa_supplicant-{ifname}.pid"))
}

/// wpa_supplicant configuration for one network (NET-2).
pub fn supplicant_conf(ctrl_dir: &Path, w: &Wifi, country: Option<&str>) -> String {
    let mut t = format!(
        "# Written by the XorOS rescue shell (NET-2); lives in /run only.\n\
         ctrl_interface={}\n\
         update_config=0\n",
        ctrl_dir.display()
    );
    if let Some(cc) = country {
        t.push_str(&format!("country={cc}\n"));
    }
    t.push_str("\nnetwork={\n");
    t.push_str(&format!("\tssid={}\n", conf_string(&w.ssid)));
    if w.hidden {
        t.push_str("\tscan_ssid=1\n");
    }
    match &w.security {
        WifiSecurity::Open => t.push_str("\tkey_mgmt=NONE\n"),
        WifiSecurity::WpaPsk(p) => {
            t.push_str("\tkey_mgmt=WPA-PSK\n");
            if is_hex_psk(p) {
                t.push_str(&format!("\tpsk={}\n", p.to_ascii_lowercase()));
            } else {
                t.push_str(&format!("\tpsk={}\n", conf_string(p)));
            }
        }
        WifiSecurity::Sae(p) => {
            t.push_str("\tkey_mgmt=SAE\n\tieee80211w=2\n");
            t.push_str(&format!("\tsae_password={}\n", conf_string(p)));
        }
        // `WifiSecurity` is non-exhaustive.
        #[allow(unreachable_patterns)]
        _ => t.push_str("\tkey_mgmt=NONE\n"),
    }
    t.push_str("}\n");
    t
}

/// Two upper-case letters (ISO 3166-1 alpha-2) or `00` (world).
pub fn valid_country(cc: &str) -> bool {
    cc.len() == 2
        && cc
            .bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// NET-2: unblock all rfkill switches (`/sys/class/rfkill/*/soft` = 0).
pub fn unblock_rfkill(env: &NetEnv) -> Vec<String> {
    let mut notes = Vec::new();
    let Ok(rd) = fs::read_dir(env.host.sys("class/rfkill")) else {
        return notes;
    };
    for e in rd.flatten() {
        let soft = e.path().join("soft");
        if crate::hw::read_trimmed(&soft).as_deref() != Some("1") {
            continue;
        }
        if env.simulation {
            log::info!("SIM: would unblock {}", soft.display());
            continue;
        }
        match fs::write(&soft, "0") {
            Ok(()) => log::info!("NET-2: unblocked {}", soft.display()),
            Err(e) => notes.push(format!("rfkill {}: {e}", soft.display())),
        }
    }
    notes
}

fn read_pid(p: &Path) -> Option<i32> {
    fs::read_to_string(p)
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|p| *p > 1)
}

/// Stop the wpa_supplicant the app started on `ifname` (if any).
pub fn stop_supplicant(env: &NetEnv, ifname: &str) {
    let pidf = pid_path(env, ifname);
    if let Some(pid) = read_pid(&pidf) {
        if env.simulation {
            log::info!("SIM: would stop wpa_supplicant (pid {pid})");
        } else {
            // SAFETY: plain kill(2) on a pid read from our own pid file.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            log::info!("NET-2: stopped wpa_supplicant on {ifname} (pid {pid})");
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
    let _ = fs::remove_file(pidf);
}

/// NET-2: write the configuration and (re)start `wpa_supplicant -B` on `ifname`.
pub fn start_supplicant(
    env: &NetEnv,
    ifname: &str,
    w: &Wifi,
    country: Option<&str>,
) -> io::Result<Vec<String>> {
    let mut notes = unblock_rfkill(env);
    if let Some(cc) = country {
        if let Err(e) = env.run_tool("iw", &["reg", "set", cc]) {
            notes.push(format!("iw reg set {cc}: {e}"));
        }
    }
    stop_supplicant(env, ifname);
    let conf = conf_path(env, ifname);
    super::config::write_file(&conf, &supplicant_conf(&ctrl_dir(env), w, country), 0o600)?;
    log::info!("NET-2: wrote {} (ssid '{}')", conf.display(), w.ssid);
    let conf_s = conf.display().to_string();
    let pid_s = pid_path(env, ifname).display().to_string();
    let _ = env.run_tool("ip", &["link", "set", ifname, "up"]);
    env.run_tool(
        "wpa_supplicant",
        &["-B", "-i", ifname, "-c", &conf_s, "-P", &pid_s],
    )
    .map_err(tool_io)?;
    Ok(notes)
}

pub fn tool_io(e: ToolError) -> io::Error {
    io::Error::other(e.to_string())
}

/// Example networks shown by a scan in simulation mode (SIM-1).
fn sim_scan() -> Vec<ScanEntry> {
    let e = |ssid: &str, sig, sec| ScanEntry {
        ssid: ssid.into(),
        bssid: "02:00:00:00:00:01".into(),
        signal: Some(sig),
        frequency: Some(2437),
        security: sec,
    };
    vec![
        e("XorOS-Lab", -42, ScanSecurity::WpaPsk),
        e("Guest", -67, ScanSecurity::Open),
        e("Office-WPA3", -71, ScanSecurity::Sae),
    ]
}

/// NET-2: scan for networks on `ifname`: `wpa_cli scan_results` when the app's
/// wpa_supplicant runs there, else `iw dev <if> scan` (brings the interface up first).
pub fn scan(env: &NetEnv, ifname: &str) -> Result<Vec<ScanEntry>, String> {
    if env.simulation {
        log::info!("SIM: example scan results for {ifname}");
        return Ok(sim_scan());
    }
    unblock_rfkill(env);
    let ctrl = ctrl_dir(env).display().to_string();
    if read_pid(&pid_path(env, ifname)).is_some() {
        let base = ["-p", ctrl.as_str(), "-i", ifname];
        let scan: Vec<&str> = base.iter().copied().chain(["scan"]).collect();
        if crate::cmd::run_capture("wpa_cli", &scan).is_ok() {
            std::thread::sleep(std::time::Duration::from_secs(4));
            let res: Vec<&str> = base.iter().copied().chain(["scan_results"]).collect();
            if let Ok(t) = crate::cmd::run_capture("wpa_cli", &res) {
                let v = parse_wpa_cli_scan(&t);
                if !v.is_empty() {
                    return Ok(v);
                }
            }
        }
    }
    let _ = crate::cmd::run_capture("ip", &["link", "set", ifname, "up"]);
    let text =
        crate::cmd::run_capture("iw", &["dev", ifname, "scan"]).map_err(|e| e.to_string())?;
    Ok(parse_iw_scan(&text))
}

/// Current association of `ifname` from `iw dev <if> link`: (SSID, signal dBm).
pub fn link_info(ifname: &str) -> Option<(String, Option<i32>)> {
    let t = crate::cmd::run_capture("iw", &["dev", ifname, "link"]).ok()?;
    parse_iw_link(&t)
}

pub fn parse_iw_link(t: &str) -> Option<(String, Option<i32>)> {
    if t.trim_start().starts_with("Not connected") {
        return None;
    }
    let mut ssid = None;
    let mut sig = None;
    for l in t.lines().map(str::trim) {
        if let Some(v) = l.strip_prefix("SSID: ") {
            ssid = Some(unescape_ssid(v));
        } else if let Some(v) = l.strip_prefix("signal: ") {
            sig = v.split_whitespace().next().and_then(|n| n.parse().ok());
        }
    }
    ssid.map(|s| (s, sig))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_vectors() {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        let long = vec![b'a'; 1000];
        assert_eq!(
            hex(&sha1(&long)),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba"
        );
    }

    #[test]
    fn wpa_passphrase_vectors() {
        // IEEE 802.11i-2004, annex H.4.
        assert_eq!(
            wpa_psk_hex("IEEE", "password"),
            "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"
        );
        assert_eq!(
            wpa_psk_hex("ThisIsASSID", "ThisIsAPassword"),
            "0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af"
        );
        let mut w = Wifi::new("IEEE", WifiSecurity::WpaPsk("password".into()));
        hash_secret(&mut w);
        assert!(matches!(&w.security, WifiSecurity::WpaPsk(p) if is_hex_psk(p)));
        let before = w.clone();
        hash_secret(&mut w);
        assert_eq!(w, before);
    }

    #[test]
    fn supplicant_conf_text() {
        let w = Wifi::new("Lab", WifiSecurity::WpaPsk("secret12".into()));
        let c = supplicant_conf(Path::new("/run/x/wpa"), &w, Some("DE"));
        assert!(c.contains("ctrl_interface=/run/x/wpa\n"));
        assert!(c.contains("country=DE\n"));
        assert!(c.contains("\tssid=\"Lab\"\n\tkey_mgmt=WPA-PSK\n\tpsk=\"secret12\"\n"));
        let mut w = Wifi::new("Café", WifiSecurity::Sae("pw".into()));
        w.hidden = true;
        let c = supplicant_conf(Path::new("/c"), &w, None);
        assert!(c.contains("\tssid=436166c3a9\n\tscan_ssid=1\n\tkey_mgmt=SAE\n"));
        assert!(c.contains("sae_password=\"pw\""));
        let w = Wifi::new("Open", WifiSecurity::Open);
        assert!(supplicant_conf(Path::new("/c"), &w, None).contains("key_mgmt=NONE"));
        assert!(valid_country("DE") && valid_country("00") && !valid_country("de"));
    }

    #[test]
    fn wpa_cli_scan() {
        let t = "bssid / frequency / signal level / flags / ssid\n\
                 aa:bb:cc:dd:ee:01\t2437\t-60\t[WPA2-PSK-CCMP][ESS]\tHome\n\
                 aa:bb:cc:dd:ee:02\t5180\t-40\t[WPA2-PSK+SAE-CCMP][ESS]\tHome\n\
                 aa:bb:cc:dd:ee:03\t2412\t-80\t[ESS]\tCafe\\x20Free\n\
                 aa:bb:cc:dd:ee:04\t2412\t-70\t[WPA2-EAP-CCMP][ESS]\tCorp\n";
        let v = parse_wpa_cli_scan(t);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].ssid, "Home");
        assert_eq!(v[0].signal, Some(-40));
        assert_eq!(v[0].security, ScanSecurity::WpaPskSae);
        assert_eq!(v[1].security, ScanSecurity::Unsupported);
        assert_eq!(v[2].ssid, "Cafe Free");
        assert_eq!(v[2].security, ScanSecurity::Open);
        assert!(v[0].label().contains("WPA2/3"));
    }

    #[test]
    fn iw_scan() {
        let t = "BSS aa:bb:cc:dd:ee:01(on wlan0)\n\
                 \tfreq: 2437\n\
                 \tcapability: ESS Privacy ShortSlotTime (0x0411)\n\
                 \tsignal: -55.00 dBm\n\
                 \tSSID: Home\n\
                 \tRSN:\t * Version: 1\n\
                 \t\t * Authentication suites: PSK\n\
                 BSS aa:bb:cc:dd:ee:02(on wlan0)\n\
                 \tfreq: 5180.0\n\
                 \tsignal: -70.00 dBm\n\
                 \tSSID: Open\n\
                 BSS aa:bb:cc:dd:ee:03(on wlan0) -- associated\n\
                 \tsignal: -65.00 dBm\n\
                 \tSSID: New\n\
                 \tRSN:\t * Version: 1\n\
                 \t\t * Authentication suites: SAE\n";
        let v = parse_iw_scan(t);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].ssid, "Home");
        assert_eq!(v[0].security, ScanSecurity::WpaPsk);
        assert_eq!(v[0].frequency, Some(2437));
        assert_eq!(v[1].ssid, "New");
        assert_eq!(v[1].security, ScanSecurity::Sae);
        assert_eq!(v[2].security, ScanSecurity::Open);
        assert_eq!(v[2].frequency, Some(5180));
    }

    #[test]
    fn iw_link() {
        assert_eq!(parse_iw_link("Not connected.\n"), None);
        let t = "Connected to aa:bb:cc:dd:ee:01 (on wlan0)\n\tSSID: Home\n\tfreq: 2437\n\tsignal: -48 dBm\n";
        assert_eq!(parse_iw_link(t), Some(("Home".into(), Some(-48))));
    }
}
