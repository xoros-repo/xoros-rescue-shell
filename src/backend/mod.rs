//! Narrow interface between the rescue shell and the system (boot state, build info, devices).
//!
//! Implementations:
//! - [`core::CoreBackend`]: the real system through the shared crate `xoros-core`
//!   (`../xor/REQUIREMENTS.md`, XC-21..45); see its module docs for the method mapping;
//! - [`sim::SimBackend`]: simulation mode (SIM-1), state in `<dir>/sim.json`;
//! - [`unavailable::UnavailableBackend`]: fallback when the boot disk/layout cannot be
//!   detected; every device operation fails with that reason.
//!
//! The app keeps its own small serializable types (used by the simulation file, the status
//! JSON and the UI) and converts from/to the crate's types (`From` impls below and in
//! [`core`]). Names and numbers ([`Slot::name`], [`PartRole::label`], ...) come from
//! `xoros_core`.
//!
//! Nothing in this module depends on cursive (ARCH-1).

pub mod core;
pub mod sim;
pub mod unavailable;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub use xoros_core::boot::BootPartition;
pub use xoros_core::device::PartitionWriter;

use crate::hw::HostPaths;
use crate::tasks::CancelToken;

/// Root filesystem slot (XC-34).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub const ALL: [Slot; 2] = [Slot::A, Slot::B];

    /// grubenv / kernel command line name: `ROOTFS_A`.
    pub fn name(self) -> &'static str {
        xoros_core::boot::Slot::from(self).name()
    }

    /// GPT PARTLABEL: `rootfs_a`.
    pub fn label(self) -> &'static str {
        xoros_core::boot::Slot::from(self).label()
    }

    /// GRUB menu entry id: `MENU_ROOTFS_A`.
    pub fn menu_id(self) -> &'static str {
        xoros_core::boot::Slot::from(self).menu_id()
    }

    /// Short letter for the UI.
    pub fn letter(self) -> &'static str {
        match self {
            Slot::A => "A",
            Slot::B => "B",
        }
    }

    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }

    pub fn index(self) -> usize {
        match self {
            Slot::A => 0,
            Slot::B => 1,
        }
    }
}

impl From<Slot> for xoros_core::boot::Slot {
    fn from(s: Slot) -> Self {
        match s {
            Slot::A => xoros_core::boot::Slot::A,
            Slot::B => xoros_core::boot::Slot::B,
        }
    }
}

impl From<xoros_core::boot::Slot> for Slot {
    fn from(s: xoros_core::boot::Slot) -> Self {
        match s {
            xoros_core::boot::Slot::A => Slot::A,
            xoros_core::boot::Slot::B => Slot::B,
        }
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Slot {
    type Err = BackendError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "A" | "ROOTFS_A" | "MENU_ROOTFS_A" => Ok(Slot::A),
            "B" | "ROOTFS_B" | "MENU_ROOTFS_B" => Ok(Slot::B),
            _ => Err(BackendError::invalid("slot", s)),
        }
    }
}

/// A GRUB boot entry (XC-34).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BootTarget {
    Slot(Slot),
    Rescue,
}

impl BootTarget {
    /// `ROOTFS_A`, `ROOTFS_B` or `RESCUE` (values of `LAST_BOOT` and `boot.slot=`).
    pub fn name(self) -> &'static str {
        match self {
            BootTarget::Slot(s) => s.name(),
            BootTarget::Rescue => xoros_core::boot::BootTarget::Rescue.name(),
        }
    }

    /// `MENU_ROOTFS_A`, `MENU_ROOTFS_B` or `MENU_RESCUE` (values of `NEXT_BOOT`).
    pub fn menu_id(self) -> &'static str {
        xoros_core::boot::BootTarget::from(self).menu_id()
    }
}

impl From<BootTarget> for xoros_core::boot::BootTarget {
    fn from(t: BootTarget) -> Self {
        match t {
            BootTarget::Slot(s) => xoros_core::boot::BootTarget::Slot(s.into()),
            BootTarget::Rescue => xoros_core::boot::BootTarget::Rescue,
        }
    }
}

impl From<xoros_core::boot::BootTarget> for BootTarget {
    fn from(t: xoros_core::boot::BootTarget) -> Self {
        match t {
            xoros_core::boot::BootTarget::Slot(s) => BootTarget::Slot(s.into()),
            xoros_core::boot::BootTarget::Rescue => BootTarget::Rescue,
        }
    }
}

impl fmt::Display for BootTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for BootTarget {
    type Err = BackendError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "RESCUE" | "MENU_RESCUE" => Ok(BootTarget::Rescue),
            other => Slot::from_str(other)
                .map(BootTarget::Slot)
                .map_err(|_| BackendError::invalid("boot target", s)),
        }
    }
}

/// Why GRUB booted rescue (XC-33). The rescue shell adds "slot invalid" on top (SYS-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RescueReason {
    /// `NEXT_BOOT=MENU_RESCUE` or picked in the GRUB menu (both counters below MAX_TRIES).
    Requested,
    /// Both slot counters reached MAX_TRIES.
    SlotsExhausted,
    /// Cannot tell (e.g. grubenv unreadable).
    Unknown,
}

/// Typed view of the grubenv boot variables (XC-32). Missing variables carry grub.cfg defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootState {
    /// `ROOTFS_A_TRY`, `ROOTFS_B_TRY` (index by [`Slot::index`]).
    pub tries: [u8; 2],
    pub max_tries: u8,
    pub order: [Slot; 2],
    pub next_boot: Option<BootTarget>,
    pub last_boot: Option<BootTarget>,
    pub rotation: u8,
    /// Raw grubenv variables in file order (BOOT-1 raw view), including unknown ones.
    pub raw: Vec<(String, String)>,
}

impl BootState {
    pub fn try_count(&self, slot: Slot) -> u8 {
        self.tries[slot.index()]
    }

    pub fn max_tries(&self) -> u8 {
        self.max_tries
    }

    pub fn order(&self) -> [Slot; 2] {
        self.order
    }

    /// 1-based position of `slot` in ORDER.
    pub fn order_position(&self, slot: Slot) -> usize {
        if self.order[0] == slot {
            1
        } else {
            2
        }
    }

    pub fn exhausted(&self, slot: Slot) -> bool {
        self.try_count(slot) >= self.max_tries
    }
}

/// Build information from a slot's `/xoros.env` (XC-21).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub distro: String,
    pub version: String,
    pub machine: String,
    /// `stable`, `devel`, `audit` or `nightly` (XC-22, kept as text here).
    pub suffix: String,
    /// `YYYYMMDDHHMMSS` (XC-23, kept as text here).
    pub timestamp: String,
    #[serde(default)]
    pub rescue_image_sha256: Option<String>,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

/// Identity of the running rescue system (SYS-1).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescueSystemInfo {
    /// `VERSION_ID` from `/etc/os-release`.
    pub version_id: Option<String>,
    /// `NAME` from `/etc/os-release`.
    pub name: Option<String>,
    pub hostname: Option<String>,
}

/// Role of a partition in the XorOS layout (RS 4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PartRole {
    Boot,
    RootfsA,
    RootfsB,
    Rescue,
    Overlay,
    Application,
}

impl PartRole {
    pub const ALL: [PartRole; 6] = [
        PartRole::Boot,
        PartRole::RootfsA,
        PartRole::RootfsB,
        PartRole::Rescue,
        PartRole::Overlay,
        PartRole::Application,
    ];

    /// GPT PARTLABEL (`xoros_core::consts`).
    pub fn label(self) -> &'static str {
        xoros_core::consts::PartitionRole::from(self).label()
    }

    /// Partition number on the boot disk (`xoros_core::consts`).
    pub fn number(self) -> u32 {
        xoros_core::consts::PartitionRole::from(self).number()
    }

    pub fn from_label(label: &str) -> Option<PartRole> {
        PartRole::ALL.iter().copied().find(|r| r.label() == label)
    }

    pub fn slot(slot: Slot) -> PartRole {
        match slot {
            Slot::A => PartRole::RootfsA,
            Slot::B => PartRole::RootfsB,
        }
    }
}

impl From<PartRole> for xoros_core::consts::PartitionRole {
    fn from(r: PartRole) -> Self {
        use xoros_core::consts::PartitionRole as C;
        match r {
            PartRole::Boot => C::Boot,
            PartRole::RootfsA => C::RootfsA,
            PartRole::RootfsB => C::RootfsB,
            PartRole::Rescue => C::Rescue,
            PartRole::Overlay => C::Overlay,
            PartRole::Application => C::Application,
        }
    }
}

impl From<xoros_core::consts::PartitionRole> for PartRole {
    fn from(r: xoros_core::consts::PartitionRole) -> Self {
        use xoros_core::consts::PartitionRole as C;
        match r {
            C::Boot => PartRole::Boot,
            C::RootfsA => PartRole::RootfsA,
            C::RootfsB => PartRole::RootfsB,
            C::Rescue => PartRole::Rescue,
            C::Overlay => PartRole::Overlay,
            C::Application => PartRole::Application,
        }
    }
}

/// One partition of a block device (XC-40).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partition {
    pub number: u32,
    /// Kernel name, e.g. `sda2`, `mmcblk0p2`.
    pub name: String,
    #[serde(default)]
    pub partlabel: Option<String>,
    #[serde(default)]
    pub partuuid: Option<String>,
    pub size: u64,
    #[serde(default)]
    pub fstype: Option<String>,
    #[serde(default)]
    pub mountpoint: Option<String>,
}

/// A whole-disk block device (XC-40).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDevice {
    /// Kernel name, e.g. `sda`.
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    pub size: u64,
    #[serde(default)]
    pub removable: bool,
    /// `usb`, `sata`, `nvme`, `mmc`, `virtio` or `unknown`.
    #[serde(default = "unknown_transport")]
    pub transport: String,
    /// Marked by the backend from boot disk detection (DEV-2).
    #[serde(default)]
    pub boot_disk: bool,
    #[serde(default)]
    pub partitions: Vec<Partition>,
}

fn unknown_transport() -> String {
    "unknown".to_string()
}

/// Partition roles on the boot disk (XC-42).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    pub disk: String,
    pub entries: Vec<LayoutEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutEntry {
    pub role: PartRole,
    /// Device node, e.g. `/dev/sda2`.
    pub device: String,
    pub size: u64,
}

impl Layout {
    pub fn get(&self, role: PartRole) -> Option<&LayoutEntry> {
        self.entries.iter().find(|e| e.role == role)
    }
}

/// File system usage of a mounted partition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsUsage {
    pub total: u64,
    pub used: u64,
}

impl FsUsage {
    pub fn free(&self) -> u64 {
        self.total.saturating_sub(self.used)
    }
}

/// Error categories callers can match on (subset of `xoros_core::Error`, XC-11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The function is not available in this build/mode (e.g. xoros-core not integrated).
    NotAvailable,
    /// grubenv is missing or not 1024 bytes; the UI offers "Recreate grubenv" (ENV-2).
    GrubenvBroken,
    InvalidValue,
    Io,
    Cancelled,
    Other,
}

/// Error returned by every backend operation. `Display` is a short English sentence (UI-5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendError {
    pub kind: ErrorKind,
    pub message: String,
}

impl BackendError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        BackendError {
            kind,
            message: message.into(),
        }
    }

    pub fn invalid(what: &str, value: &str) -> Self {
        Self::new(
            ErrorKind::InvalidValue,
            format!("Invalid {what}: '{value}'."),
        )
    }

    pub fn not_available(what: &str) -> Self {
        Self::new(
            ErrorKind::NotAvailable,
            format!("{what} is not available yet."),
        )
    }

    pub fn io(context: impl fmt::Display, err: std::io::Error) -> Self {
        Self::new(ErrorKind::Io, format!("{context}: {err}"))
    }

    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "Cancelled.")
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BackendError {}

pub type Result<T> = std::result::Result<T, BackendError>;

/// An external stick: a partition (or whole-disk file system) on a disk that is not the boot
/// disk (USB-1). In simulation mode a directory `<sim>/sticks/<name>/`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stick {
    /// Stable id: kernel name (`sdb1`, `sdc`) or the simulation directory name.
    pub id: String,
    /// One line for the UI, e.g. `sdb1  SanDisk Ultra  28.6 GiB  vfat`.
    pub description: String,
    /// Device node (`/dev/sdb1`); `None` in simulation mode.
    pub device: Option<String>,
    /// File system type if known (`vfat`, `exfat`, `ext4`).
    pub fs_type: Option<String>,
    pub size: u64,
}

impl Stick {
    /// FAT file systems need split archives (BAK-3).
    pub fn is_fat(&self) -> bool {
        matches!(self.fs_type.as_deref(), Some("vfat" | "msdos" | "fat"))
    }
}

/// Undoes a mount; implemented by the backends.
pub trait Release: Send {
    fn release(self: Box<Self>) -> Result<()>;
}

/// A mounted file system (stick or persistent partition) that is unmounted when dropped or with
/// [`MountGuard::unmount`]. Simulation guards point at plain directories and unmount nothing.
pub struct MountGuard {
    path: PathBuf,
    release: Option<Box<dyn Release>>,
}

impl MountGuard {
    /// A directory that is not a mount (simulation).
    pub fn directory(path: impl Into<PathBuf>) -> Self {
        MountGuard {
            path: path.into(),
            release: None,
        }
    }

    pub fn with_release(path: impl Into<PathBuf>, release: Box<dyn Release>) -> Self {
        MountGuard {
            path: path.into(),
            release: Some(release),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Unmount now and report errors (dropping only logs them).
    pub fn unmount(mut self) -> Result<()> {
        match self.release.take() {
            Some(r) => r.release(),
            None => Ok(()),
        }
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        if let Some(r) = self.release.take() {
            if let Err(e) = r.release() {
                log::warn!("unmount {}: {e}", self.path.display());
            }
        }
    }
}

impl fmt::Debug for MountGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountGuard")
            .field("path", &self.path)
            .field("mounted", &self.release.is_some())
            .finish()
    }
}

/// Progress callback for long operations: `(bytes_done, bytes_total)`.
pub type ProgressFn<'a> = &'a mut dyn FnMut(u64, u64);

/// SHA-256 (hex) of the squashfs image in `path` (partition node or image file): the first
/// `Superblock::image_len()` bytes (`round_up(bytes_used, 4096)`, section 4.1), not the whole
/// partition, through `xoros_core::device::hash_device`. Data without a squashfs superblock is
/// an `InvalidValue` error. Reports `(done, image_len)` per MiB; `delay` slows it down per MiB
/// (simulation only).
pub fn hash_squashfs(
    path: &std::path::Path,
    progress: ProgressFn<'_>,
    cancel: &CancelToken,
    delay: Option<std::time::Duration>,
) -> Result<String> {
    let progress = std::cell::RefCell::new(progress);
    let report = |e: &xoros_core::ProgressEvent| {
        (progress.borrow_mut())(e.bytes_done, e.bytes_total);
        if let Some(d) = delay {
            std::thread::sleep(d);
        }
    };
    xoros_core::device::hash_device(path, None, &report, cancel)
        .map(|h| h.sha256)
        .map_err(|e| match e {
            xoros_core::Error::NotSquashfs => BackendError::new(
                ErrorKind::InvalidValue,
                format!("{} does not contain a squashfs image.", path.display()),
            ),
            other => core::core_err(other),
        })
}

/// Everything the rescue shell needs from the system. Implementations must be thread safe;
/// every call is blocking and may be slow (run disk work in a task, UI-3 / CORE-4).
pub trait SystemBackend: Send + Sync {
    /// Short description for the status bar / logs, e.g. "simulation: ./sim".
    fn describe(&self) -> String;

    /// True in simulation mode (SIM-1): power actions and console changes only log.
    fn is_simulation(&self) -> bool;

    /// Directory for runtime state (`/run/xoros-rescue` or `<sim>/run`, RT-2).
    fn run_dir(&self) -> PathBuf;

    /// procfs/sysfs roots for hardware information (SYS-2).
    fn host_paths(&self) -> HostPaths;

    // --- boot state (XC-30..36) ---

    /// Read grubenv. `ErrorKind::GrubenvBroken` when missing or wrong size (ENV-2).
    fn boot_state(&self) -> Result<BootState>;

    /// Entry the system was booted from (`boot.slot=` on the kernel command line, XC-35).
    fn current_boot(&self) -> Result<BootTarget>;

    /// Entry GRUB will boot next, reproducing grub.cfg (XC-33).
    fn predict(&self) -> Result<BootTarget>;

    /// Why rescue was booted (XC-33).
    fn rescue_reason(&self) -> Result<RescueReason>;

    /// `<SLOT>_TRY=0` (BOOT-2).
    fn reset_tries(&self, slot: Slot) -> Result<()>;

    /// `<SLOT>_TRY=MAX_TRIES` (BOOT-7).
    fn disable(&self, slot: Slot) -> Result<()>;

    /// One-shot `NEXT_BOOT`; `None` clears it (BOOT-3, PWR-1).
    fn set_next_boot(&self, target: Option<BootTarget>) -> Result<()>;

    /// `ORDER` with `first` first (BOOT-4).
    fn set_order(&self, first: Slot) -> Result<()>;

    /// `MAX_TRIES` 1..=20 (BOOT-5).
    fn set_max_tries(&self, max_tries: u8) -> Result<()>;

    /// `ROTATION` 0..=3 (ROT-1).
    fn set_rotation(&self, rotation: u8) -> Result<()>;

    /// Create a fresh 1024-byte grubenv (explicit user action, ENV-2 / XC-31 `GrubEnv::create`).
    fn recreate_grubenv(&self) -> Result<()>;

    // --- build info (XC-21, XC-45) ---

    /// `/xoros.env` of a slot; `Ok(None)` for an empty/invalid slot.
    fn slot_info(&self, slot: Slot) -> Result<Option<BuildInfo>>;

    /// Identity of the running rescue system.
    fn rescue_system(&self) -> Result<RescueSystemInfo>;

    // --- devices (XC-40..44) ---

    /// All whole-disk block devices; the boot disk has `boot_disk = true`.
    fn block_devices(&self) -> Result<Vec<BlockDevice>>;

    /// The disk the system booted from (DEV-2).
    fn boot_disk(&self) -> Result<BlockDevice>;

    /// XorOS partition roles on the boot disk (DEV-1, XC-42).
    fn layout(&self) -> Result<Layout>;

    /// Used/total space of a persistent partition, `Ok(None)` if unknown.
    fn partition_usage(&self, role: PartRole) -> Result<Option<FsUsage>>;

    /// SHA-256 (hex) of the squashfs image in a partition (first `round_up(bytes_used, 4096)`
    /// bytes). Checks `cancel` at least every MiB and reports progress.
    fn hash_image(
        &self,
        role: PartRole,
        progress: ProgressFn<'_>,
        cancel: &CancelToken,
    ) -> Result<String>;

    // --- sticks and persistent partitions (USB-1, BAK-*, RST-*) ---

    /// External sticks: file systems on disks other than the boot disk (USB-1, DEV-3).
    fn external_sticks(&self) -> Result<Vec<Stick>>;

    /// Mount a stick under `<run dir>/mnt/usb/<id>` (read-only unless `writable`, USB-1).
    fn mount_stick(&self, stick: &Stick, writable: bool) -> Result<MountGuard>;

    /// Mount a persistent partition (`Overlay` or `Application` only, ext4) of the boot disk
    /// under `<run dir>/mnt/<label>`. Other roles are refused (NF-4).
    fn mount_partition(&self, role: PartRole, writable: bool) -> Result<MountGuard>;

    /// Runs external tools (`e2fsck`, `mke2fs`); a logging mock in simulation mode (BAK-5).
    fn command_runner(&self) -> Box<dyn crate::backup::tools::CommandRunner>;

    // --- writing slots (OTA-5, USB-3, NF-4) ---

    /// grubenv access for the crate's flash engine (`flash::update_slot` / `copy_slot` disable
    /// the slot while it is written and enable it afterwards). The boot partition is mounted
    /// only for each single read or write (ENV-3).
    fn boot_partition(&self) -> Result<BootPartition>;

    /// Open a slot partition for writing through the crate's guarded writer (NF-4: refuses
    /// whole disks, boot/rescue, mounted partitions and the running slot).
    fn open_slot_writer(&self, slot: Slot) -> Result<PartitionWriter>;

    /// Open a slot partition for reading (source of the A+B copy, OTA-5).
    fn open_slot_reader(&self, slot: Slot) -> Result<std::fs::File>;

    /// Called after a slot was written and verified, with the build info from the manifest.
    /// The simulation records it as the slot's `xoros.env` (its images are not real
    /// squashfs); real backends read `/xoros.env` from the slot itself, so the default does
    /// nothing.
    fn slot_written(&self, _slot: Slot, _info: &BuildInfo) -> Result<()> {
        Ok(())
    }
}

/// Refuse roles that are not persistent partitions (NF-4: never write boot/rootfs/rescue).
pub fn check_persistent(role: PartRole) -> Result<()> {
    match role {
        PartRole::Overlay | PartRole::Application => Ok(()),
        other => Err(BackendError::new(
            ErrorKind::InvalidValue,
            format!(
                "Partition '{}' is not a persistent partition.",
                other.label()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_and_target_names_round_trip() {
        for s in Slot::ALL {
            assert_eq!(Slot::from_str(s.name()).ok(), Some(s));
            assert_eq!(Slot::from_str(s.menu_id()).ok(), Some(s));
            let t = BootTarget::Slot(s);
            assert_eq!(BootTarget::from_str(t.name()).ok(), Some(t));
            assert_eq!(BootTarget::from_str(t.menu_id()).ok(), Some(t));
        }
        assert_eq!(
            BootTarget::from_str("MENU_RESCUE").ok(),
            Some(BootTarget::Rescue)
        );
        assert!(BootTarget::from_str("nope").is_err());
    }

    #[test]
    fn part_roles() {
        for r in PartRole::ALL {
            assert_eq!(PartRole::from_label(r.label()), Some(r));
        }
        assert_eq!(PartRole::slot(Slot::B).number(), 3);
    }
}
