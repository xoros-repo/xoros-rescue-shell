//! Backup, restore and factory reset of the persistent partitions (BAK-*, RST-*).
//!
//! UI independent (no ratatui, ARCH-1). All functions are blocking; run them in a worker thread
//! and pass a progress callback and a [`CancelToken`] (UI-3). Mounting, `e2fsck` and `mke2fs`
//! are done by the caller ([`tools`] has injectable helpers).
//!
//! # Archive format (BAK-1, BAK-2, BAK-3)
//!
//! `xoros-backup-<hostname>-<YYYYMMDD-HHMMSS>.tar.gz` (UTC time), or `<name>.000`, `.001`, ...
//! when the size exceeds `split_size`; plus `<name>.sha256` (`sha256sum` format, one line per
//! file, file names without directory). The archive is a gzip compressed POSIX tar:
//!
//! - first entry `xoros-backup.json` ([`BackupMetadata`]),
//! - then `overlay/` (the partition root itself, then its content depth-first in byte order of
//!   names), then `application/` likewise. Excluded: `overlay/lost+found`,
//!   `overlay/overlays/etc/work`, `application/lost+found`, sockets, other file systems.
//! - ustar headers; pax extended headers (`x`) carry `path`/`linkpath` (names >= 100 bytes or
//!   non-ASCII), `mtime` (when it has nanoseconds or is negative), `uid`/`gid` (> 2097151),
//!   `size` (>= 8 GiB) and every extended attribute as `SCHILY.xattr.<name>` with the raw value
//!   (`trusted.overlay.*`, `security.*`, `user.*`, `system.posix_acl_*`).
//! - numeric uid/gid only (uname/gname empty), mode incl. setuid/setgid/sticky, hard links as
//!   type `1` pointing to the first archived name, devices `3`/`4` with major/minor (overlayfs
//!   whiteouts are char 0:0), FIFOs `6`, symlinks `2`. Sparse files are stored expanded.
//!
//! The format can be read by GNU tar (`tar --xattrs --xattrs-include='*' --numeric-owner -xpzf`)
//! and bsdtar.

mod error;
mod export;
mod fsops;
mod import;
mod meta;
mod progress;
mod reset;
mod split;
pub mod tools;

pub use error::{BackupError, ChecksumProblem, Result};
pub use export::{check_space, export, split_size_for, ExportOptions, ExportReport, SpaceCheck};
pub use fsops::{fs_space, FsSpace};
pub use import::{
    backup_info, import, list_backups, read_metadata, verify, BackupInfo, FileError, ImportMode,
    ImportOptions, ImportReport, PartitionImport, MAX_REPORTED_ERRORS,
};
pub use meta::{
    archive_name, part_name, BackupMetadata, PartitionRole, PartitionStats, SlotMeta,
    FAT_MAX_PART_SIZE, METADATA_NAME, OVERLAY_UPPER_DIR, OVERLAY_WORK_DIR, RESCUE_DIR,
    STICK_BACKUP_DIR,
};
pub use progress::{CancelToken, Phase, Progress, ProgressFn};
pub use reset::{factory_reset, reset_confirmed, ResetReport, RESET_CONFIRM_WORD};
pub use split::PartInfo;
pub(crate) use split::SplitWriter;

#[cfg(test)]
mod tests;
