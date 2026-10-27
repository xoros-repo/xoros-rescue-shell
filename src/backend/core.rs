//! The real backend: a thin adapter over the shared crate `xoros-core` (CORE-1..6).
//!
//! | trait method | xoros-core |
//! |---|---|
//! | `boot_state`, `predict`, `rescue_reason` | `BootPartition::read` -> `boot::BootState` |
//! | setters, `recreate_grubenv` | `BootPartition::update` / `BootPartition::recreate` |
//! | `current_boot` | `boot::current_boot` (`/proc/cmdline`) |
//! | `slot_info` | `device::slot_info` |
//! | `block_devices`, `boot_disk`, `layout` | `device::list_block_devices`, `device::boot_disk`, `device::Layout::of` |
//! | `hash_image` | `squashfs::Superblock::image_len` on the partition node |
//! | `mount_stick`, `mount_partition` | `device::mount::TempMount` (+ `mountinfo` for existing mounts) |
//! | `boot_partition` | `BootPartition::from_layout` |
//! | `open_slot_writer`, `open_slot_reader` | `device::PartitionWriter::open`, `device::Layout::open_read` |
//!
//! Every system path comes from [`SystemPaths`] (tests use a fixture tree via
//! [`SystemPaths::with_root`]). The boot partition is mounted only for one read or write
//! (ENV-3) by the crate.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use xoros_core::boot::{self as cboot, BootPartition};
use xoros_core::device::{self as cdev, mount, Layout as CoreLayout, PartitionWriter};
use xoros_core::mountinfo::{self, MountInfo};
use xoros_core::SystemPaths;

use super::{
    check_persistent, BackendError, BlockDevice, BootState, BootTarget, BuildInfo, ErrorKind,
    FsUsage, Layout, LayoutEntry, MountGuard, PartRole, Partition, ProgressFn, Release,
    RescueReason, RescueSystemInfo, Result, Slot, Stick, SystemBackend,
};
use crate::backup::tools::{CommandRunner, SystemRunner};
use crate::constants::{MOUNT_DIR, OS_RELEASE, RUN_DIR, STICK_MOUNT_SUBDIR};
use crate::hw::HostPaths;
use crate::tasks::{format_bytes, CancelToken};

/// Converts crate errors into the app's error kinds.
pub fn core_err(e: xoros_core::Error) -> BackendError {
    use xoros_core::Error as E;
    let kind = match &e {
        // Typed GrubenvMissing/GrubenvSize (xoros-core 0.3) and the older inferred forms.
        _ if e.is_grubenv_recreatable() => ErrorKind::GrubenvBroken,
        E::GrubenvFormat(_) => ErrorKind::GrubenvBroken,
        E::Io { path, source }
            if source.kind() == std::io::ErrorKind::NotFound
                && path.ends_with(xoros_core::consts::GRUBENV_REL_PATH) =>
        {
            ErrorKind::GrubenvBroken
        }
        E::Cancelled => ErrorKind::Cancelled,
        E::InvalidValue { .. } | E::MissingValue { .. } => ErrorKind::InvalidValue,
        E::Io { .. } | E::Mount { .. } => ErrorKind::Io,
        _ => ErrorKind::Other,
    };
    let message = match kind {
        ErrorKind::GrubenvBroken => format!("grubenv is missing or broken: {e}"),
        _ => e.to_string(),
    };
    BackendError::new(kind, message)
}

/// Real system access through `xoros-core`.
pub struct CoreBackend {
    paths: SystemPaths,
    run_dir: PathBuf,
}

impl CoreBackend {
    /// Backend for the running system: real paths, mounts under `/run/xoros-rescue/mnt`.
    pub fn system() -> CoreBackend {
        CoreBackend::with_paths(SystemPaths::default().mount_dir(MOUNT_DIR), RUN_DIR)
    }

    /// Backend on explicit paths (tests: a fixture tree from `SystemPaths::with_root`).
    pub fn with_paths(paths: SystemPaths, run_dir: impl Into<PathBuf>) -> CoreBackend {
        CoreBackend {
            paths,
            run_dir: run_dir.into(),
        }
    }

    pub fn paths(&self) -> &SystemPaths {
        &self.paths
    }

    /// Detect the boot disk and its layout (DEV-1, DEV-2). Used at start-up to decide whether
    /// this backend can be used.
    pub fn probe(&self) -> Result<Layout> {
        self.layout()
    }

    fn core_layout(&self) -> Result<CoreLayout> {
        let disk = cdev::boot_disk(&self.paths).map_err(core_err)?;
        CoreLayout::of(&disk).map_err(core_err)
    }

    fn read_state(&self) -> Result<cboot::BootState> {
        self.boot_partition()?.read().map_err(core_err)
    }

    fn update(
        &self,
        what: &str,
        f: impl FnOnce(&mut cboot::BootState) -> xoros_core::Result<()>,
    ) -> Result<()> {
        self.boot_partition()?.update(f).map_err(core_err)?;
        log::info!("grubenv: {what}");
        Ok(())
    }

    fn mounts(&self) -> Result<Vec<MountInfo>> {
        MountInfo::read(&self.paths).map_err(core_err)
    }

    /// Existing mount of the whole file system on `dev`, if any.
    fn existing_mount(&self, dev: cdev::DevNum) -> Result<Option<MountInfo>> {
        // The last matching entry: later mounts stack on top.
        Ok(mountinfo::mounts_of(&self.mounts()?, dev)
            .filter(|m| m.root == Path::new("/"))
            .last()
            .cloned())
    }

    /// Mount `node` at `target`, or reuse an existing mount of `dev` (USB-1, BAK-*).
    fn mount_fs(
        &self,
        node: &Path,
        dev: cdev::DevNum,
        target: &Path,
        fs_types: &[&str],
        writable: bool,
    ) -> Result<MountGuard> {
        if let Some(m) = self.existing_mount(dev)? {
            if writable && m.is_read_only() {
                return Err(BackendError::new(
                    ErrorKind::Io,
                    format!(
                        "{} is mounted read-only at {}.",
                        node.display(),
                        m.mount_point.display()
                    ),
                ));
            }
            log::info!(
                "{} already mounted at {}",
                node.display(),
                m.mount_point.display()
            );
            return Ok(MountGuard::directory(self.paths.host_path(&m.mount_point)));
        }
        let opts = mount::MountOptions {
            read_only: !writable,
            ..mount::MountOptions::default()
        };
        let mut last = None;
        for t in fs_types {
            match mount::TempMount::new(node, target, t, &opts) {
                Ok(g) => {
                    return Ok(MountGuard::with_release(
                        target.to_path_buf(),
                        Box::new(TempRelease(g)),
                    ))
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last
            .map(core_err)
            .unwrap_or_else(|| BackendError::new(ErrorKind::Other, "No file system type.")))
    }
}

/// Unmounts a `TempMount` through the [`MountGuard`].
struct TempRelease(mount::TempMount);

impl Release for TempRelease {
    fn release(self: Box<Self>) -> Result<()> {
        self.0.unmount().map_err(core_err)
    }
}

// --- type conversions ---

fn u8_sat(v: u32) -> u8 {
    v.min(u32::from(u8::MAX)) as u8
}

pub fn boot_state_from_core(s: &cboot::BootState) -> BootState {
    BootState {
        tries: [
            u8_sat(s.try_count(cboot::Slot::A)),
            u8_sat(s.try_count(cboot::Slot::B)),
        ],
        max_tries: u8_sat(s.max_tries()),
        order: s.order().map(Slot::from),
        next_boot: s.next_boot().map(BootTarget::from),
        last_boot: s.last_boot().map(BootTarget::from),
        rotation: s.rotation(),
        raw: s
            .env()
            .vars()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

pub fn reason_from_core(r: cboot::RescueReason) -> RescueReason {
    match r {
        cboot::RescueReason::Requested => RescueReason::Requested,
        cboot::RescueReason::SlotsExhausted => RescueReason::SlotsExhausted,
        cboot::RescueReason::Unknown => RescueReason::Unknown,
    }
}

pub fn build_info_from_core(b: xoros_core::env::BuildInfo) -> BuildInfo {
    BuildInfo {
        distro: b.distro,
        version: b.version,
        machine: b.machine,
        suffix: b.suffix.as_str().to_string(),
        timestamp: b.timestamp.as_str().to_string(),
        rescue_image_sha256: b.rescue_image_sha256,
        extra: b.extra,
    }
}

fn block_device_from_core(
    d: &cdev::BlockDevice,
    boot_disk: bool,
    mounts: &[MountInfo],
) -> BlockDevice {
    BlockDevice {
        name: d.name.clone(),
        model: match (&d.vendor, &d.model) {
            (Some(v), Some(m)) if !m.starts_with(v.as_str()) => Some(format!("{v} {m}")),
            (_, Some(m)) => Some(m.clone()),
            (Some(v), None) => Some(v.clone()),
            (None, None) => None,
        },
        size: d.size,
        removable: d.removable,
        transport: d.transport.as_str().to_string(),
        boot_disk,
        partitions: d
            .partitions
            .iter()
            .map(|p| Partition {
                number: p.number,
                name: p.name.clone(),
                partlabel: p.label.clone(),
                partuuid: p.uuid.clone(),
                size: p.size,
                fstype: p.fs_type.clone(),
                mountpoint: mountinfo::mounts_of(mounts, p.dev)
                    .find(|m| m.root == Path::new("/"))
                    .map(|m| m.mount_point.display().to_string()),
            })
            .collect(),
    }
}

/// Used/total space from an ext4 superblock (offset 1024), without mounting.
/// `None` if the data is not ext2/3/4.
pub fn ext4_usage(mut r: impl Read + Seek) -> std::io::Result<Option<FsUsage>> {
    let mut sb = [0u8; 1024];
    r.seek(SeekFrom::Start(1024))?;
    if r.read_exact(&mut sb).is_err() {
        return Ok(None);
    }
    let u16_at = |o: usize| u16::from_le_bytes([sb[o], sb[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([sb[o], sb[o + 1], sb[o + 2], sb[o + 3]]);
    if u16_at(56) != 0xEF53 {
        return Ok(None);
    }
    let log_bs = u32_at(24);
    if log_bs > 6 {
        return Ok(None);
    }
    let bs = 1024u64 << log_bs;
    let is64 = u32_at(96) & 0x80 != 0;
    let (blocks, free) = if is64 {
        (
            u64::from(u32_at(4)) | (u64::from(u32_at(0x150)) << 32),
            u64::from(u32_at(12)) | (u64::from(u32_at(0x158)) << 32),
        )
    } else {
        (u64::from(u32_at(4)), u64::from(u32_at(12)))
    };
    let total = blocks.saturating_mul(bs);
    let free = free.saturating_mul(bs).min(total);
    Ok(Some(FsUsage {
        total,
        used: total - free,
    }))
}

/// `statvfs` of a mounted file system.
fn statvfs_usage(path: &Path) -> Result<FsUsage> {
    let s = crate::backup::fs_space(path)
        .map_err(|e| BackendError::io(format!("statvfs {}", path.display()), e))?;
    Ok(FsUsage {
        total: s.total,
        used: s.used,
    })
}

/// File systems a stick may carry (USB-1); others (swap, squashfs of XorOS drives) are skipped.
const STICK_FS: &[&str] = &["vfat", "exfat", "ext4", "ext3", "ext2"];

/// Kernel names that are never sticks.
fn is_virtual_disk(name: &str) -> bool {
    ["loop", "sr", "md", "dm-", "zram", "ram", "nbd"]
        .iter()
        .any(|p| name.starts_with(p))
}

impl SystemBackend for CoreBackend {
    fn describe(&self) -> String {
        match self.core_layout() {
            Ok(l) => format!("device: {} via xoros-core", l.disk.name),
            Err(e) => format!("device via xoros-core ({e})"),
        }
    }

    fn is_simulation(&self) -> bool {
        false
    }

    fn run_dir(&self) -> PathBuf {
        self.run_dir.clone()
    }

    fn host_paths(&self) -> HostPaths {
        HostPaths {
            proc_root: self.paths.proc.clone(),
            sys_root: self.paths.sysfs.clone(),
        }
    }

    fn boot_state(&self) -> Result<BootState> {
        Ok(boot_state_from_core(&self.read_state()?))
    }

    fn current_boot(&self) -> Result<BootTarget> {
        match cboot::current_boot(&self.paths).map_err(core_err)? {
            Some(t) => Ok(t.into()),
            None => Err(BackendError::new(
                ErrorKind::Other,
                "Not booted by XorOS GRUB (no boot.slot= on the kernel command line).",
            )),
        }
    }

    fn predict(&self) -> Result<BootTarget> {
        Ok(self.read_state()?.predict().into())
    }

    fn rescue_reason(&self) -> Result<RescueReason> {
        Ok(reason_from_core(self.read_state()?.rescue_reason()))
    }

    fn reset_tries(&self, slot: Slot) -> Result<()> {
        self.update(&format!("{}_TRY=0", slot.name()), |s| {
            s.reset_tries(slot.into())
        })
    }

    fn disable(&self, slot: Slot) -> Result<()> {
        self.update(&format!("disable {}", slot.name()), |s| {
            s.disable(slot.into())
        })
    }

    fn set_next_boot(&self, target: Option<BootTarget>) -> Result<()> {
        let what = format!(
            "NEXT_BOOT={}",
            target.map(|t| t.menu_id()).unwrap_or_default()
        );
        self.update(&what, |s| s.set_next_boot(target.map(Into::into)))
    }

    fn set_order(&self, first: Slot) -> Result<()> {
        self.update(&format!("ORDER {} first", first.name()), |s| {
            s.set_order(first.into())
        })
    }

    fn set_max_tries(&self, max_tries: u8) -> Result<()> {
        self.update(&format!("MAX_TRIES={max_tries}"), |s| {
            s.set_max_tries(u32::from(max_tries))
        })
    }

    fn set_rotation(&self, rotation: u8) -> Result<()> {
        self.update(&format!("ROTATION={rotation}"), |s| {
            s.set_rotation(rotation)
        })
    }

    fn recreate_grubenv(&self) -> Result<()> {
        self.boot_partition()?
            .recreate(&cboot::BootState::default())
            .map_err(core_err)?;
        log::warn!("grubenv recreated with default values");
        Ok(())
    }

    fn slot_info(&self, slot: Slot) -> Result<Option<BuildInfo>> {
        let layout = self.core_layout()?;
        Ok(cdev::slot_info(&layout, slot.into(), &self.paths)
            .map_err(core_err)?
            .map(build_info_from_core))
    }

    fn rescue_system(&self) -> Result<RescueSystemInfo> {
        Ok(crate::sysinfo::rescue_system_info(
            &self.paths.host_path(Path::new(OS_RELEASE)),
        ))
    }

    fn block_devices(&self) -> Result<Vec<BlockDevice>> {
        let boot = cdev::boot_disk(&self.paths).ok().map(|d| d.name);
        let mounts = self.mounts().unwrap_or_default();
        Ok(cdev::list_block_devices(&self.paths)
            .map_err(core_err)?
            .iter()
            .map(|d| block_device_from_core(d, boot.as_deref() == Some(d.name.as_str()), &mounts))
            .collect())
    }

    fn boot_disk(&self) -> Result<BlockDevice> {
        let d = cdev::boot_disk(&self.paths).map_err(core_err)?;
        let mounts = self.mounts().unwrap_or_default();
        Ok(block_device_from_core(&d, true, &mounts))
    }

    fn layout(&self) -> Result<Layout> {
        let l = self.core_layout()?;
        Ok(Layout {
            disk: l.disk.name.clone(),
            entries: l
                .iter()
                .map(|(role, p)| LayoutEntry {
                    role: role.into(),
                    device: p.node.display().to_string(),
                    size: p.size,
                })
                .collect(),
        })
    }

    fn partition_usage(&self, role: PartRole) -> Result<Option<FsUsage>> {
        let layout = self.core_layout()?;
        let part = layout.partition(role.into());
        if let Some(m) = self.existing_mount(part.dev)? {
            return statvfs_usage(&self.paths.host_path(&m.mount_point)).map(Some);
        }
        // Not mounted (normal in rescue): read the ext4 superblock instead of mounting.
        let f = fs::File::open(&part.node)
            .map_err(|e| BackendError::io(format!("Cannot open {}", part.node.display()), e))?;
        ext4_usage(f)
            .map_err(|e| BackendError::io(format!("Cannot read {}", part.node.display()), e))
    }

    fn hash_image(
        &self,
        role: PartRole,
        progress: ProgressFn<'_>,
        cancel: &CancelToken,
    ) -> Result<String> {
        let layout = self.core_layout()?;
        let node = layout.node(role.into());
        let f = fs::File::open(node)
            .map_err(|e| BackendError::io(format!("Cannot open {}", node.display()), e))?;
        super::hash_squashfs(f, &node.display().to_string(), progress, cancel, None)
    }

    fn external_sticks(&self) -> Result<Vec<Stick>> {
        let boot = cdev::boot_disk(&self.paths).map_err(core_err)?;
        let disks = cdev::list_block_devices(&self.paths).map_err(core_err)?;
        let mut out = Vec::new();
        for d in disks
            .iter()
            .filter(|d| d.name != boot.name && !is_virtual_disk(&d.name) && d.size > 0)
        {
            let model = d.model.clone().unwrap_or_else(|| d.transport.to_string());
            if d.partitions.is_empty() {
                // A file system on the whole device: its type is not known to the crate.
                out.push(Stick {
                    id: d.name.clone(),
                    description: format!("{}  {model}  {}", d.name, format_bytes(d.size)),
                    device: Some(d.node.display().to_string()),
                    fs_type: None,
                    size: d.size,
                });
                continue;
            }
            for p in &d.partitions {
                if let Some(t) = &p.fs_type {
                    if !STICK_FS.contains(&t.as_str()) {
                        continue;
                    }
                }
                out.push(Stick {
                    id: p.name.clone(),
                    description: format!(
                        "{}  {model}  {}  {}",
                        p.name,
                        format_bytes(p.size),
                        p.fs_type.as_deref().unwrap_or("unknown fs")
                    ),
                    device: Some(p.node.display().to_string()),
                    fs_type: p.fs_type.clone(),
                    size: p.size,
                });
            }
        }
        Ok(out)
    }

    fn mount_stick(&self, stick: &Stick, writable: bool) -> Result<MountGuard> {
        // Look the stick up again: it must still exist and must not be on the boot disk.
        let current = self.external_sticks()?;
        let st = current.iter().find(|s| s.id == stick.id).ok_or_else(|| {
            BackendError::new(
                ErrorKind::Io,
                format!("Stick {} is no longer present.", stick.id),
            )
        })?;
        let disks = cdev::list_block_devices(&self.paths).map_err(core_err)?;
        let (node, dev) = disks
            .iter()
            .flat_map(|d| {
                std::iter::once((d.name.as_str(), d.node.clone(), d.dev)).chain(
                    d.partitions
                        .iter()
                        .map(|p| (p.name.as_str(), p.node.clone(), p.dev)),
                )
            })
            .find(|(n, _, _)| *n == st.id)
            .map(|(_, node, dev)| (node, dev))
            .ok_or_else(|| BackendError::new(ErrorKind::Io, format!("{} not found.", st.id)))?;
        let target = self.paths.mount_dir.join(STICK_MOUNT_SUBDIR).join(&st.id);
        let types: Vec<&str> = match st.fs_type.as_deref() {
            Some(t) => vec![t],
            None => vec!["vfat", "exfat", "ext4"],
        };
        self.mount_fs(&node, dev, &target, &types, writable)
    }

    fn mount_partition(&self, role: PartRole, writable: bool) -> Result<MountGuard> {
        check_persistent(role)?;
        let layout = self.core_layout()?;
        let part = layout.partition(role.into());
        let target = self.paths.mount_dir.join(role.label());
        self.mount_fs(&part.node, part.dev, &target, &["ext4"], writable)
    }

    fn command_runner(&self) -> Box<dyn CommandRunner> {
        Box::new(SystemRunner::default())
    }

    fn boot_partition(&self) -> Result<BootPartition> {
        Ok(BootPartition::from_layout(
            &self.core_layout()?,
            &self.paths,
        ))
    }

    fn open_slot_writer(&self, slot: Slot) -> Result<PartitionWriter> {
        let layout = self.core_layout()?;
        PartitionWriter::open(&layout, cboot::BootTarget::Slot(slot.into()), &self.paths)
            .map_err(core_err)
    }

    fn open_slot_reader(&self, slot: Slot) -> Result<fs::File> {
        self.core_layout()?
            .open_read(PartRole::slot(slot).into())
            .map_err(core_err)
    }

    fn mount_slot(&self, slot: Slot) -> Result<MountGuard> {
        let layout = self.core_layout()?;
        let part = layout.partition(PartRole::slot(slot).into());
        let target = self.paths.mount_dir.join(slot.label());
        self.mount_fs(&part.node, part.dev, &target, &["squashfs"], false)
    }
}

#[cfg(test)]
mod tests;
