//! Rescue shell paths and defaults (NF-3).
//!
//! Device and boot facts (partition labels and numbers, PARTUUIDs, grubenv variable names,
//! limits) come from `xoros_core::consts`; the few the UI needs are derived from there below.

use xoros_core::consts as core;

/// Application version (from Cargo.toml).
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Application name shown in the UI.
pub const APP_NAME: &str = "XorOS Rescue Shell";

/// Runtime state directory (RT-2).
pub const RUN_DIR: &str = "/run/xoros-rescue";

/// Mount directory below [`RUN_DIR`] (RT-2).
pub const MNT_SUBDIR: &str = "mnt";

/// App log file name below [`RUN_DIR`] (LOG-3).
pub const LOG_FILE_NAME: &str = "rescue-shell.log";

/// Environment variable that enables simulation mode (SIM-1).
pub const SIM_ENV_VAR: &str = "XOROS_RESCUE_SIM";

/// Simulation description file inside a simulation directory.
pub const SIM_FILE_NAME: &str = "sim.json";

/// Default shell for the subshell (SH-1).
pub const SUBSHELL: &str = "/bin/sh";

/// Prompt of the subshell (SH-1).
pub const SUBSHELL_PROMPT: &str = "rescue-shell# ";

/// os-release file of the running (rescue) system (SYS-1).
pub const OS_RELEASE: &str = "/etc/os-release";

/// fbcon rotation file for the running console (ROT-1), relative to the sysfs root.
pub const FBCON_ROTATE_ALL: &str = "class/graphics/fbcon/rotate_all";

/// Number of lines kept in the in-memory log buffer (LOG-1).
pub const LOG_RING_LINES: usize = 2000;

/// Maximum number of lines read from external log sources (LOG-1).
pub const LOG_MAX_LINES: usize = 5000;

/// Range of valid MAX_TRIES values (BOOT-5), from `xoros_core::consts`.
pub const MAX_TRIES_RANGE: std::ops::RangeInclusive<u8> =
    (core::MIN_MAX_TRIES as u8)..=(core::MAX_TRY_COUNTER as u8);

/// Default MAX_TRIES (grub.cfg default), from `xoros_core::consts`.
pub const DEFAULT_MAX_TRIES: u8 = core::DEFAULT_MAX_TRIES as u8;

/// grubenv variable names shown in the boot screen (BOOT-1), from `xoros_core::consts`.
pub const GRUBENV_KNOWN_VARS: &[&str] = &[
    core::VAR_ROOTFS_A_TRY,
    core::VAR_ROOTFS_B_TRY,
    core::VAR_MAX_TRIES,
    core::VAR_ORDER,
    core::VAR_NEXT_BOOT,
    core::VAR_LAST_BOOT,
    core::VAR_ROTATION,
];

/// Persistent partition roles shown on the status screen.
pub const PERSISTENT_PARTITIONS: &[&str] = &[core::PARTLABEL_OVERLAY, core::PARTLABEL_APPLICATION];

/// Mount directory of the rescue shell for `xoros_core::SystemPaths::mount_dir` (RT-2).
pub const MOUNT_DIR: &str = "/run/xoros-rescue/mnt";

/// Sub directory of [`MOUNT_DIR`] for external sticks (USB-1).
pub const STICK_MOUNT_SUBDIR: &str = "usb";

/// Sub directory on a stick that is searched for backups in addition to the stick root (BAK-4).
pub const STICK_BACKUP_DIR: &str = "xoros/backups";

/// Maximum backup part size on vfat sticks: 4 GiB - 1 byte (BAK-3).
pub const FAT_MAX_PART_SIZE: u64 = 4 * 1024 * 1024 * 1024 - 1;

/// Overlayfs work directory inside the overlay partition (section 4.5; excluded from backups,
/// recreated empty after an import or reset).
pub const OVERLAY_WORK_DIR: &str = "overlays/etc/work";

/// Overlayfs upper directory inside the overlay partition (section 4.5).
pub const OVERLAY_UPPER_DIR: &str = "overlays/etc/upper";

/// The rescue shell's own directory on the overlay partition (CFG-1, RST-1, BAK-4).
pub const RESCUE_DIR: &str = "rescue";

/// ext4 `lost+found`: never archived, never deleted.
pub const LOST_FOUND: &str = "lost+found";

/// ext4 file system UUID of the application partition (section 4.1, BAK-6).
pub const FSUUID_APPLICATION: &str = core::FSUUID_APPLICATION;

/// systemd-networkd runtime configuration directory (NET-1; `/etc` is read-only).
pub const NETWORKD_RUN_DIR: &str = "/run/systemd/network";

/// systemd-networkd DHCP lease files (DNS servers for NET-3).
pub const NETWORKD_LEASES_DIR: &str = "/run/systemd/netif/leases";

/// Name of the `.network` file per interface: `50-xoros-rescue-<if>.network` (NET-1).
pub const NETWORK_FILE_PREFIX: &str = "50-xoros-rescue-";

/// The app's resolver configuration below the run directory (NET-3).
pub const RESOLV_CONF_NAME: &str = "resolv.conf";

/// udhcpc script written below the run directory (NET-1 fallback).
pub const UDHCPC_SCRIPT_NAME: &str = "udhcpc.script";

/// Default OTA server (OTA, CFG-1 `ota_base_url`).
pub const DEFAULT_OTA_BASE_URL: &str = core::DEFAULT_OTA_BASE_URL;

/// Default HTTP proxy (CFG-1 `http_proxy`; empty = direct, OTA-11).
pub const DEFAULT_HTTP_PROXY: &str = "";

/// Default SNTP servers (CFG-1 `ntp_servers`, NET-7).
pub const DEFAULT_NTP_SERVERS: &[&str] = &["pool.ntp.org", "time.cloudflare.com"];

/// Directories on a stick searched for OTA trees and images, besides the root (USB-2).
pub const STICK_OTA_DIRS: &[&str] = &["", "xoros", "ota", "xoros/ota"];

/// How deep below each of [`STICK_OTA_DIRS`] `manifest.json` files are searched (USB-2).
pub const STICK_OTA_DEPTH: usize = 4;

/// Word to type before writing both slots (UI-4).
pub const WRITE_BOTH_CONFIRM_WORD: &str = "WRITE";

/// Build time of the app (seconds since the epoch, from build.rs / `SOURCE_DATE_EPOCH`): a
/// clock before this is certainly wrong (NET-7).
pub const BUILD_EPOCH: &str = env!("XOROS_BUILD_EPOCH");
