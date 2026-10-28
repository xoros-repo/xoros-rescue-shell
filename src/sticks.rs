//! External sticks: safe removal (USB-5) and change detection for the hot-plug refresh of the
//! stick lists (USB-1).

use std::path::Path;

use crate::backend::{Stick, SystemBackend};
use crate::constants::{MNT_SUBDIR, STICK_MOUNT_SUBDIR};

/// Interval of the hot-plug poll while a stick list is shown (USB-1: "poll sysfs every 2 s").
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Ids of the external sticks now (sorted), for change detection; errors count as "none".
pub fn ids(backend: &dyn SystemBackend) -> Vec<String> {
    let mut v: Vec<String> = backend
        .external_sticks()
        .map(|s| s.into_iter().map(|s| s.id).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Mount points of `device` outside `own_dir`, from mountinfo text.
pub fn foreign_mounts(mountinfo: &str, device: &str, own_dir: &Path) -> Vec<String> {
    mountinfo
        .lines()
        .filter_map(|l| {
            let mp = l.split_whitespace().nth(4)?;
            let (_, after) = l.split_once(" - ")?;
            let source = after.split_whitespace().nth(1)?;
            (source == device && !Path::new(mp).starts_with(own_dir)).then(|| mp.to_string())
        })
        .collect()
}

/// USB-5: unmount the stick (all of the app's mounts of it), `sync`, and say it can be removed.
/// Refused when the device is mounted somewhere else (e.g. by hand in the subshell).
pub fn safe_remove(backend: &dyn SystemBackend, stick: &Stick) -> Result<String, String> {
    let own = backend
        .run_dir()
        .join(MNT_SUBDIR)
        .join(STICK_MOUNT_SUBDIR)
        .join(&stick.id);
    log::info!("action: safe removal of stick {}", stick.id);
    if backend.is_simulation() {
        crate::power::sync();
        return Ok(format!(
            "Simulation: nothing to unmount.\n\n{} can be removed now.",
            stick.description
        ));
    }
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    if let Some(dev) = &stick.device {
        let other = foreign_mounts(&info, dev, &own);
        if !other.is_empty() {
            return Err(format!(
                "{dev} is also mounted at {}. Unmount it there first (Tools -> Subshell).",
                other.join(", ")
            ));
        }
    }
    let mut errors = Vec::new();
    for mp in crate::power::mounts_under(&info, &own) {
        if let Err(e) = crate::cmd::run_logged("umount", &[&mp]) {
            errors.push(format!("{mp}: {e}"));
        }
    }
    crate::power::sync();
    if !errors.is_empty() {
        log::error!(
            "action: safe removal of {}: {}",
            stick.id,
            errors.join("; ")
        );
        return Err(format!(
            "The stick is still in use, do not remove it yet:\n{}",
            errors.join("\n")
        ));
    }
    log::info!("action: safe removal of stick {}: ok", stick.id);
    Ok(format!(
        "All data is written and the stick is unmounted.\n\n{} can be removed now.",
        stick.description
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_mount_detection() {
        let mi = "30 22 8:17 / /run/xoros-rescue/mnt/usb/sdb1 rw - vfat /dev/sdb1 rw\n\
                  31 22 8:17 / /mnt/x rw - vfat /dev/sdb1 rw\n\
                  32 22 8:33 / /mnt/y rw - ext4 /dev/sdc1 rw\n";
        let own = Path::new("/run/xoros-rescue/mnt/usb/sdb1");
        assert_eq!(
            foreign_mounts(mi, "/dev/sdb1", own),
            vec!["/mnt/x".to_string()]
        );
        assert!(foreign_mounts(mi, "/dev/sdd1", own).is_empty());
    }

    #[test]
    fn simulation_removal_and_ids() {
        let d = crate::hw::testutil::TempDir::new("sticks");
        crate::backend::sim::SimBackend::create_example(d.path()).expect("example");
        let b = crate::backend::sim::SimBackend::open(d.path()).expect("open");
        let ids = ids(&b);
        assert!(!ids.is_empty());
        let stick = b.external_sticks().expect("sticks").remove(0);
        assert!(safe_remove(&b, &stick)
            .expect("remove")
            .contains("can be removed"));
    }
}
