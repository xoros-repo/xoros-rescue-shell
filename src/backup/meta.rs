//! Partition roles, archive naming and the `xoros-backup.json` metadata (BAK-1).

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Name of the metadata entry; always the first entry of the archive.
pub const METADATA_NAME: &str = "xoros-backup.json";
/// Value of the `format` field written by this version.
pub const FORMAT_VERSION: u32 = 1;
/// Archive file name prefix.
pub const ARCHIVE_PREFIX: &str = "xoros-backup-";
/// Archive file name suffix (before the split suffix `.000`, ...).
pub const ARCHIVE_SUFFIX: &str = ".tar.gz";
/// Suffix of the checksum sidecar file, appended to the `.tar.gz` name.
pub const CHECKSUM_SUFFIX: &str = ".sha256";
// Paths on the stick and the overlay partition live in the app constants module (NF-3).
pub use crate::constants::{
    FAT_MAX_PART_SIZE, LOST_FOUND, OVERLAY_UPPER_DIR, OVERLAY_WORK_DIR, RESCUE_DIR,
    STICK_BACKUP_DIR,
};

/// A persistent partition that can be backed up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PartitionRole {
    Overlay,
    Application,
}

impl PartitionRole {
    pub const ALL: [PartitionRole; 2] = [PartitionRole::Overlay, PartitionRole::Application];

    /// Name used as archive prefix, JSON key, GPT PARTLABEL and ext4 label.
    pub fn as_str(self) -> &'static str {
        match self {
            PartitionRole::Overlay => "overlay",
            PartitionRole::Application => "application",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        PartitionRole::ALL.iter().copied().find(|r| r.as_str() == s)
    }

    /// ext4 file system label (for `mke2fs -L`, BAK-6).
    pub fn fs_label(self) -> &'static str {
        self.as_str()
    }

    /// Fixed ext4 file system UUID, if the distro uses one (for `mke2fs -U`, BAK-6).
    pub fn fs_uuid(self) -> Option<&'static str> {
        match self {
            PartitionRole::Overlay => None,
            PartitionRole::Application => Some(crate::constants::FSUUID_APPLICATION),
        }
    }

    /// Paths relative to the partition root that are not archived and not restored.
    pub fn excluded(self) -> &'static [&'static str] {
        match self {
            PartitionRole::Overlay => &[LOST_FOUND, OVERLAY_WORK_DIR],
            PartitionRole::Application => &[LOST_FOUND],
        }
    }

    /// `rel` is a `/`-separated path relative to the partition root ("" = root).
    pub(crate) fn is_excluded(self, rel: &[u8]) -> bool {
        self.excluded().iter().any(|ex| {
            let ex = ex.as_bytes();
            rel == ex || (rel.len() > ex.len() && rel.starts_with(ex) && rel[ex.len()] == b'/')
        })
    }
}

impl fmt::Display for PartitionRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Build information of one slot as stored in the metadata (from the slot's `/xoros.env`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotMeta {
    pub version: String,
    pub suffix: String,
    pub timestamp: String,
}

/// Per partition statistics in the metadata.
///
/// `files` counts all archived entries of the partition except directories (regular files,
/// symlinks, hard links, devices, FIFOs); `bytes` is the sum of regular file sizes, each inode
/// counted once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionStats {
    pub files: u64,
    pub bytes: u64,
}

/// Content of `xoros-backup.json` (BAK-1). Unknown fields are ignored when reading.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupMetadata {
    pub format: u32,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub created: String,
    pub machine: String,
    /// Keys `ROOTFS_A`, `ROOTFS_B`; `null` for an empty/invalid slot.
    #[serde(default)]
    pub slots: BTreeMap<String, Option<SlotMeta>>,
    /// Keys `overlay`, `application`; only the partitions in the archive.
    #[serde(default)]
    pub partitions: BTreeMap<String, PartitionStats>,
    #[serde(default)]
    pub app_version: String,
}

impl BackupMetadata {
    /// Partitions contained in the archive, in canonical order.
    pub fn roles(&self) -> Vec<PartitionRole> {
        PartitionRole::ALL
            .iter()
            .copied()
            .filter(|r| self.partitions.contains_key(r.as_str()))
            .collect()
    }

    /// True if `machine` differs from the archive's machine (BAK-4: warning, not an error).
    pub fn machine_mismatch(&self, machine: &str) -> bool {
        self.machine != machine
    }
}

/// Formats the `created` field.
pub(crate) fn format_created(t: &DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Replaces characters that are unsafe in file names (vfat, shell) with `_`.
pub(crate) fn sanitize_hostname(h: &str) -> String {
    let s: String = h
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "xoros".into()
    } else {
        s
    }
}

/// `xoros-backup-<hostname>-<YYYYMMDD-HHMMSS>.tar.gz` (UTC).
pub fn archive_name(hostname: &str, t: &DateTime<Utc>) -> String {
    format!(
        "{ARCHIVE_PREFIX}{}-{}{ARCHIVE_SUFFIX}",
        sanitize_hostname(hostname),
        t.format("%Y%m%d-%H%M%S")
    )
}

/// Name of split part `index` (`.000`, `.001`, ...).
pub fn part_name(archive_name: &str, index: usize) -> String {
    format!("{archive_name}.{index:03}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn names() {
        let t = Utc.with_ymd_and_hms(2026, 10, 27, 12, 0, 5).unwrap();
        assert_eq!(
            archive_name("dev 1/x", &t),
            "xoros-backup-dev_1_x-20261027-120005.tar.gz"
        );
        assert_eq!(part_name("a.tar.gz", 2), "a.tar.gz.002");
        assert_eq!(format_created(&t), "2026-10-27T12:00:05Z");
    }

    #[test]
    fn exclusions() {
        let o = PartitionRole::Overlay;
        assert!(o.is_excluded(b"lost+found"));
        assert!(o.is_excluded(b"lost+found/x"));
        assert!(o.is_excluded(b"overlays/etc/work"));
        assert!(o.is_excluded(b"overlays/etc/work/work"));
        assert!(!o.is_excluded(b"overlays/etc/workx"));
        assert!(!o.is_excluded(b"overlays/etc/upper"));
        assert!(!o.is_excluded(b""));
        assert!(!PartitionRole::Application.is_excluded(b"overlays/etc/work"));
    }

    #[test]
    fn metadata_json_shape() {
        let mut m = BackupMetadata {
            format: 1,
            created: "2026-10-27T12:00:00Z".into(),
            machine: "raspberrypi4-64".into(),
            slots: BTreeMap::new(),
            partitions: BTreeMap::new(),
            app_version: "0.2.0".into(),
        };
        m.slots.insert(
            "ROOTFS_A".into(),
            Some(SlotMeta {
                version: "1.0".into(),
                suffix: "stable".into(),
                timestamp: "20261026214512".into(),
            }),
        );
        m.slots.insert("ROOTFS_B".into(), None);
        m.partitions.insert(
            "overlay".into(),
            PartitionStats {
                files: 120,
                bytes: 81234,
            },
        );
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        let expected = serde_json::json!({ "format": 1, "created": "2026-10-27T12:00:00Z", "machine": "raspberrypi4-64",
            "slots": { "ROOTFS_A": {"version":"1.0","suffix":"stable","timestamp":"20261026214512"}, "ROOTFS_B": null },
            "partitions": { "overlay": {"files": 120, "bytes": 81234} },
            "app_version": "0.2.0" });
        assert_eq!(v, expected);
        let back: BackupMetadata = serde_json::from_value(expected).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.roles(), vec![PartitionRole::Overlay]);
    }
}
