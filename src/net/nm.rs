//! NetworkManager keyfiles of the normal system (section 4.6): writing the rescue network
//! configuration into the overlay upper directory (NET-6) and reading existing connections
//! back as rescue configurations (NET-8). The keyfile format itself is
//! `xoros_core::net::nm_keyfile`.
//!
//! Overlayfs rules (section 4.5): a whiteout (character device 0:0) in the upper directory
//! hides the lower file; an opaque directory (xattr `trusted.overlay.opaque=y`) hides the whole
//! lower directory.

use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::Serialize;
use xoros_core::net::nm_keyfile::{self, NetConfig};

use crate::backend::{BootTarget, PartRole, Slot, SystemBackend};
use crate::constants::{NM_CONNECTIONS_DIR, OVERLAY_UPPER_DIR};

const OPAQUE_XATTR: &str = "trusted.overlay.opaque";

/// `xoros-<if>.nmconnection`.
pub fn keyfile_name(ifname: &str) -> String {
    format!("xoros-{ifname}.nmconnection")
}

/// Keyfile directory relative to the overlay partition root.
pub fn upper_connections_rel() -> PathBuf {
    Path::new(OVERLAY_UPPER_DIR).join(NM_CONNECTIONS_DIR)
}

/// A whiteout: character device 0:0 (section 4.5).
pub fn is_whiteout(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.file_type().is_char_device() && m.rdev() == 0)
        .unwrap_or(false)
}

fn cpath(p: &Path) -> Option<CString> {
    CString::new(p.as_os_str().as_bytes()).ok()
}

/// Directory with `trusted.overlay.opaque=y`.
pub fn is_opaque(p: &Path) -> bool {
    let (Some(path), Ok(name)) = (cpath(p), CString::new(OPAQUE_XATTR)) else {
        return false;
    };
    let mut buf = [0u8; 4];
    // SAFETY: valid C strings and a buffer of the given length.
    let n = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    n == 1 && buf[0] == b'y'
}

fn set_opaque(p: &Path) -> io::Result<()> {
    let path = cpath(p).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let name =
        CString::new(OPAQUE_XATTR).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: valid C strings and a one-byte value.
    let r = unsafe { libc::lsetxattr(path.as_ptr(), name.as_ptr(), b"y".as_ptr().cast(), 1, 0) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

fn chown_root(p: &Path) -> io::Result<()> {
    if is_root() {
        std::os::unix::fs::chown(p, Some(0), Some(0))
    } else {
        Ok(())
    }
}

/// Create `rel` below `upper` directory by directory: missing ones with `mode`, the last one
/// (`system-connections`) with `last_mode`. A whiteout on the way is replaced by an opaque
/// directory, so the deleted lower content stays deleted (NET-6).
fn ensure_dirs(upper: &Path, rel: &Path, mode: u32, last_mode: u32) -> io::Result<()> {
    let comps: Vec<_> = rel.components().collect();
    let mut cur = upper.to_path_buf();
    for (i, c) in comps.iter().enumerate() {
        cur.push(c);
        let m = if i + 1 == comps.len() {
            last_mode
        } else {
            mode
        };
        let mut opaque = false;
        if is_whiteout(&cur) {
            log::info!("NET-6: removing whiteout {}", cur.display());
            fs::remove_file(&cur)?;
            opaque = true;
        }
        match fs::symlink_metadata(&cur) {
            Ok(md) if md.is_dir() => continue,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a directory", cur.display()),
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fs::create_dir(&cur)?;
        fs::set_permissions(&cur, fs::Permissions::from_mode(m))?;
        chown_root(&cur)?;
        if opaque {
            if let Err(e) = set_opaque(&cur) {
                log::warn!("NET-6: cannot mark {} opaque: {e}", cur.display());
            }
        }
    }
    Ok(())
}

/// UUID of an existing keyfile (kept when it is rewritten, NET-6).
fn existing_uuid(p: &Path) -> Option<String> {
    let t = fs::read_to_string(p).ok()?;
    if let Ok(c) = nm_keyfile::parse(&t) {
        return Some(c.uuid);
    }
    t.lines()
        .find_map(|l| l.trim().strip_prefix("uuid="))
        .map(|u| u.trim().to_string())
        .filter(|u| u.len() == 36)
}

/// NET-6: write the keyfile of `c` into `<overlay root>/overlays/etc/upper/NetworkManager/
/// system-connections/xoros-<if>.nmconnection` (directories 0755, `system-connections` 0700,
/// file root:root 0600, whiteout removed first). Returns the path.
pub fn write_keyfile(overlay_root: &Path, c: &NetConfig) -> io::Result<PathBuf> {
    let ifname = c.interface.clone().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "connection without interface")
    })?;
    let upper = overlay_root.join(OVERLAY_UPPER_DIR);
    if is_whiteout(&upper) || !upper.is_dir() {
        // The overlay partition is empty (e.g. after a reset): create the upper dir.
        ensure_dirs(overlay_root, Path::new(OVERLAY_UPPER_DIR), 0o755, 0o755)?;
    }
    ensure_dirs(&upper, Path::new(NM_CONNECTIONS_DIR), 0o755, 0o700)?;
    let dir = upper.join(NM_CONNECTIONS_DIR);
    let path = dir.join(keyfile_name(&ifname));
    let mut c = c.clone();
    c.id = format!("xoros-{ifname}");
    if let Some(u) = existing_uuid(&path) {
        c.uuid = u;
    }
    c.validate()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    if is_whiteout(&path) {
        log::info!("NET-6: removing whiteout {}", path.display());
        fs::remove_file(&path)?;
    }
    let tmp = dir.join(format!(".{}.tmp", keyfile_name(&ifname)));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(nm_keyfile::render(&c).as_bytes())?;
        f.sync_all()?;
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    chown_root(&tmp)?;
    fs::rename(&tmp, &path)?;
    if let Ok(d) = fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    log::info!("NET-6: wrote {} (uuid {})", path.display(), c.uuid);
    Ok(path)
}

/// NET-6: write keyfiles for `conns` into the overlay partition (mounted read-write for it).
/// Returns the written paths relative to the overlay root.
pub fn export(backend: &dyn SystemBackend, conns: &[NetConfig]) -> Result<Vec<PathBuf>, String> {
    if conns.is_empty() {
        return Err("No network configuration to copy (configure an interface first).".into());
    }
    let guard = backend
        .mount_partition(PartRole::Overlay, true)
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for c in conns {
        let p = write_keyfile(guard.path(), c).map_err(|e| {
            format!(
                "{}: {e}",
                keyfile_name(c.interface.as_deref().unwrap_or("?"))
            )
        })?;
        out.push(p.strip_prefix(guard.path()).unwrap_or(&p).to_path_buf());
    }
    guard.unmount().map_err(|e| e.to_string())?;
    Ok(out)
}

// --- NET-8 ---------------------------------------------------------------------------------

/// A connection found in the normal system.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NmProfile {
    /// File name (`home.nmconnection`).
    pub file: String,
    /// `overlay` or `slot A`/`slot B`.
    pub source: String,
    /// The connection, or why it cannot be used (shown read-only).
    pub config: Result<NetConfig, String>,
}

impl NmProfile {
    pub fn usable(&self) -> bool {
        self.config.is_ok()
    }

    pub fn label(&self) -> String {
        match &self.config {
            Ok(c) => {
                let d = super::profile::describe(c);
                let d = match c.interface {
                    Some(_) => d,
                    None => d.replacen("?:", "any interface:", 1),
                };
                format!("{} [{}] {d}", self.file, self.source)
            }
            Err(e) => format!("{} [{}] read-only: {e}", self.file, self.source),
        }
    }
}

/// Keyfiles in `dir` (sorted): `(name, Some(text))`, whiteouts as `(name, None)`.
fn read_dir_keyfiles(dir: &Path) -> Vec<(String, Option<String>)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<(String, Option<String>)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let p = e.path();
            if is_whiteout(&p) {
                return Some((name, None));
            }
            if !p.is_file() || !name.ends_with(".nmconnection") {
                return None;
            }
            Some((name, fs::read_to_string(&p).ok()))
        })
        .collect();
    v.sort();
    v
}

fn profile(file: String, source: &str, text: &str) -> NmProfile {
    NmProfile {
        file,
        source: source.to_string(),
        config: nm_keyfile::parse(text).map_err(|e| e.to_string()),
    }
}

/// Merge the overlay upper directory over the slot's `/etc` (NET-8): upper files win,
/// whiteouts and opaque directories hide slot files.
pub fn merge_profiles(upper_etc: Option<&Path>, slot: Option<(&Path, &str)>) -> Vec<NmProfile> {
    let mut out = Vec::new();
    let mut hidden: Vec<String> = Vec::new();
    let mut lower_hidden = false;
    if let Some(upper) = upper_etc {
        let nm = upper.join("NetworkManager");
        let dir = upper.join(NM_CONNECTIONS_DIR);
        lower_hidden = is_whiteout(&nm) || is_whiteout(&dir) || is_opaque(&nm) || is_opaque(&dir);
        for (name, text) in read_dir_keyfiles(&dir) {
            hidden.push(name.clone());
            if let Some(t) = text {
                out.push(profile(name, "overlay", &t));
            }
        }
    }
    if let (Some((root, label)), false) = (slot, lower_hidden) {
        for (name, text) in read_dir_keyfiles(&root.join("etc").join(NM_CONNECTIONS_DIR)) {
            if hidden.contains(&name) {
                continue;
            }
            if let Some(t) = text {
                out.push(profile(name, label, &t));
            }
        }
    }
    out
}

/// The slot the normal system ran from last (else the first in ORDER).
fn normal_slot(backend: &dyn SystemBackend) -> Slot {
    match backend.boot_state() {
        Ok(b) => match b.last_boot {
            Some(BootTarget::Slot(s)) => s,
            _ => b.order()[0],
        },
        Err(_) => Slot::A,
    }
}

/// NET-8: connections of the normal system: overlay upper first, then the slot's
/// `/etc/NetworkManager/system-connections`. Notes explain missing sources.
pub fn list_normal_system(backend: &dyn SystemBackend) -> (Vec<NmProfile>, Vec<String>) {
    let mut notes = Vec::new();
    let overlay = match backend.mount_partition(PartRole::Overlay, false) {
        Ok(g) => Some(g),
        Err(e) => {
            notes.push(format!("overlay: {e}"));
            None
        }
    };
    let slot = normal_slot(backend);
    let slot_guard = match backend.mount_slot(slot) {
        Ok(g) => Some(g),
        Err(e) => {
            notes.push(format!("{}: {e}", slot.label()));
            None
        }
    };
    let label = format!("slot {}", slot.letter());
    let upper = overlay.as_ref().map(|g| g.path().join(OVERLAY_UPPER_DIR));
    let v = merge_profiles(
        upper.as_deref(),
        slot_guard.as_ref().map(|g| (g.path(), label.as_str())),
    );
    (v, notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::testutil::TempDir;
    use xoros_core::net::nm_keyfile::{Ipv4Config, Medium, Wifi, WifiSecurity};

    #[test]
    fn write_keyfile_layout_and_uuid() {
        let d = TempDir::new("nm-export");
        let mut c = NetConfig::wifi(
            "wlan0",
            Wifi::new("Lab", WifiSecurity::WpaPsk("password1".into())),
        );
        c = c.with_static(
            "192.168.1.50".parse().unwrap(),
            24,
            "192.168.1.1".parse().ok(),
        );
        let p = write_keyfile(d.path(), &c).unwrap();
        assert_eq!(
            p,
            d.path().join(
                "overlays/etc/upper/NetworkManager/system-connections/xoros-wlan0.nmconnection"
            )
        );
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p), 0o600);
        assert_eq!(mode(p.parent().unwrap()), 0o700);
        assert_eq!(
            mode(&d.path().join("overlays/etc/upper/NetworkManager")),
            0o755
        );
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("id=xoros-wlan0\n"));
        assert!(text.contains("type=wifi\n"));
        assert!(text.contains("ssid=Lab\n"));
        assert!(text.contains("key-mgmt=wpa-psk\n"));
        assert!(text.contains("method=manual\n"));
        let back = nm_keyfile::parse(&text).unwrap();
        assert_eq!(back.uuid, c.uuid);
        assert_eq!(
            back.ipv4,
            Ipv4Config::Static {
                address: "192.168.1.50".parse().unwrap(),
                prefix: 24,
                gateway: "192.168.1.1".parse().ok()
            }
        );
        // Rewriting with a new UUID keeps the old one.
        let c2 = NetConfig::ethernet("wlan0").with_uuid(nm_keyfile::new_uuid());
        let p2 = write_keyfile(d.path(), &c2).unwrap();
        let back2 = nm_keyfile::parse(&fs::read_to_string(&p2).unwrap()).unwrap();
        assert_eq!(back2.uuid, c.uuid);
        assert_eq!(back2.medium, Medium::Ethernet);
    }

    #[test]
    fn merge_overlay_over_slot() {
        let d = TempDir::new("nm-load");
        let eth = nm_keyfile::render(&NetConfig::ethernet("eth0"));
        let wifi = nm_keyfile::render(&NetConfig::wifi(
            "wlan0",
            Wifi::new("Home", WifiSecurity::WpaPsk("password1".into())),
        ));
        d.write(
            "upper/NetworkManager/system-connections/home.nmconnection",
            wifi.as_bytes(),
        );
        d.write(
            "upper/NetworkManager/system-connections/vpn.nmconnection",
            b"[connection]\nid=vpn\nuuid=x\ntype=vpn\n",
        );
        d.write(
            "slot/etc/NetworkManager/system-connections/home.nmconnection",
            eth.as_bytes(),
        );
        d.write(
            "slot/etc/NetworkManager/system-connections/wired.nmconnection",
            eth.as_bytes(),
        );
        d.write("slot/etc/NetworkManager/system-connections/notes.txt", b"x");
        let v = merge_profiles(
            Some(&d.path().join("upper")),
            Some((&d.path().join("slot"), "slot A")),
        );
        let names: Vec<(&str, &str)> = v
            .iter()
            .map(|p| (p.file.as_str(), p.source.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("home.nmconnection", "overlay"),
                ("vpn.nmconnection", "overlay"),
                ("wired.nmconnection", "slot A")
            ]
        );
        assert!(v[0].usable());
        assert!(!v[1].usable());
        assert!(v[1].label().contains("read-only"));
        assert!(v[2].label().contains("eth0: ethernet, DHCP"));
        // Without overlay only the slot.
        let v = merge_profiles(None, Some((&d.path().join("slot"), "slot B")));
        assert_eq!(v.len(), 2);
    }
}
