//! Identity of the running rescue system: `/etc/os-release`, hostname (SYS-1, OTA-1).

use std::collections::BTreeMap;
use std::path::Path;

use crate::backend::RescueSystemInfo;

/// Parse an os-release file (`KEY=value`, optional single/double quotes, `#` comments).
pub fn parse_os_release(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(v);
        map.insert(k.trim().to_string(), v.replace("\\\"", "\""));
    }
    map
}

/// Hostname of the running system.
pub fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: buffer and length are valid; gethostname NUL-terminates on success
    // (we also reserve the last byte).
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len() - 1) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let s = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Machine name from a rescue hostname `xoros-rescue-<machine>` (OTA-1).
pub fn machine_from_hostname(hostname: &str) -> Option<&str> {
    hostname
        .strip_prefix("xoros-rescue-")
        .filter(|m| !m.is_empty())
}

/// Read the identity of the running system.
pub fn rescue_system_info(os_release: &Path) -> RescueSystemInfo {
    let map = std::fs::read_to_string(os_release)
        .map(|t| parse_os_release(&t))
        .unwrap_or_default();
    RescueSystemInfo {
        version_id: map.get("VERSION_ID").cloned(),
        name: map.get("NAME").cloned(),
        hostname: hostname(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release() {
        let m = parse_os_release(
            "# comment\nNAME=\"Xoros Rescue\"\nVERSION_ID=1.0\nID='xoros'\nbroken line\n",
        );
        assert_eq!(m.get("NAME").map(String::as_str), Some("Xoros Rescue"));
        assert_eq!(m.get("VERSION_ID").map(String::as_str), Some("1.0"));
        assert_eq!(m.get("ID").map(String::as_str), Some("xoros"));
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn machine_from_host() {
        assert_eq!(
            machine_from_hostname("xoros-rescue-raspberrypi4-64"),
            Some("raspberrypi4-64")
        );
        assert_eq!(machine_from_hostname("xoros-rescue-"), None);
        assert_eq!(machine_from_hostname("other"), None);
    }
}
