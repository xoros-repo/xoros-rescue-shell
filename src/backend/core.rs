//! The real backend: a thin adapter over the shared crate `xoros-core` (CORE-1..6).
//!
//! | trait method | xoros-core |
//! |---|---|
//! | `boot_state`, `predict`, `rescue_reason` | `BootPartition::read` -> `boot::BootState` |
//! | setters, `recreate_grubenv` | `BootPartition::update` / `BootPartition::recreate` |
//! | `current_boot` | `boot::current_boot` (`/proc/cmdline`) |
//! | `slot_info` | `device::slot_info` |
//! | `block_devices`, `boot_disk`, `layout` | `device::list_block_devices`, `device::boot_disk`, `device::Layout::of` |
//! | `hash_image` | `device::hash_device` (squashfs image length) on the partition node |
//! | `external_sticks`, `mount_stick` | `device::external_disks`, `Volume::mount_request` (file system probed first) |
//! | `mount_partition`, `mount_slot` | `device::mount::MountRequest` (reuses existing mounts) |
//! | `partition_usage` | `statvfs` of an existing mount, else `ext4::Superblock` + metadata overhead |
//! | `release_partition` | `device::mount_points`, `device::holders`, `mount::umount` |
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
use xoros_core::fsprobe::FsType;
use xoros_core::mountinfo::{self, MountInfo};
use xoros_core::SystemPaths;

use super::{
    check_persistent, BackendError, BlockDevice, BootState, BootTarget, BuildInfo, ErrorKind,
    FsUsage, Layout, LayoutEntry, MountGuard, PartRole, Partition, ProgressFn, Release,
    RescueReason, RescueSystemInfo, Result, Slot, Stick, SystemBackend,
};
use crate::backup::tools::{CommandRunner, SystemRunner};
use crate::constants::{MOUNT_DIR, OS_RELEASE, RUN_DIR};
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

    /// Mount through the crate (USB-1, BAK-*): an existing mount of the device is reused, else
    /// it is mounted at the request's target with the given or probed file system type.
    fn mount_request(&self, req: mount::MountRequest) -> Result<MountGuard> {
        let g = req.mount(&self.paths).map_err(core_err)?;
        if g.is_temporary() {
            log::info!("mounted {} at {} ({})", req.node().display(), g.path().display(), g.fs_type());
        } else {
            log::info!("{} already mounted at {}", req.node().display(), g.path().display());
        }
        Ok(MountGuard::with_release(
            g.path().to_path_buf(),
            Box::new(CoreRelease(g)),
        ))
    }

    /// External volumes with a file system a stick may carry, with the disk's model.
    fn stick_volumes(&self) -> Result<Vec<(cdev::Volume, String)>> {
        let mut out = Vec::new();
        for d in cdev::external_disks(&self.paths).map_err(core_err)? {
            if is_virtual_disk(&d.disk.name) {
                continue;
            }
            let model = d
                .disk
                .model
                .clone()
                .unwrap_or_else(|| d.disk.transport.to_string());
            for v in d.volumes {
                if STICK_FS.contains(&v.fs_type.as_str()) {
                    out.push((v, model.clone()));
                }
            }
        }
        Ok(out)
    }
}

/// Unmounts the crate's mount guard through the app's [`MountGuard`].
struct CoreRelease(mount::MountGuard);

impl Release for CoreRelease {
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

/// Used/total space of an unmounted ext2/3/4 file system as `df` shows it (statfs semantics,
/// like the kernel's `ext4_statfs`): the total leaves out the file system's own metadata
/// (superblock copies, group descriptors, bitmaps, inode tables, journal) and
/// used = total - free, where free includes the blocks reserved for root. A fresh file system
/// therefore shows almost nothing used. `None` if the data is not ext2/3/4.
pub fn ext4_usage(mut r: impl Read + Seek) -> std::io::Result<Option<FsUsage>> {
    let mut raw = [0u8; xoros_core::ext4::SUPERBLOCK_SIZE];
    r.seek(SeekFrom::Start(xoros_core::ext4::SUPERBLOCK_OFFSET))?;
    if r.read_exact(&mut raw).is_err() {
        return Ok(None);
    }
    let Ok(sb) = xoros_core::ext4::Superblock::parse(&raw) else {
        return Ok(None);
    };
    let total = sb
        .blocks_count
        .saturating_sub(ext4_overhead_blocks(&raw, &sb));
    let used = total.saturating_sub(sb.free_blocks);
    Ok(Some(FsUsage {
        total: total.saturating_mul(sb.block_size),
        used: used.saturating_mul(sb.block_size),
    }))
}

/// Blocks used by ext4 metadata: `s_overhead_clusters` when mke2fs recorded it (e2fsprogs
/// 1.47+), else computed like the kernel's `ext4_calculate_overhead`.
fn ext4_overhead_blocks(d: &[u8], sb: &xoros_core::ext4::Superblock) -> u64 {
    let le16 = |o: usize| u64::from(u16::from_le_bytes([d[o], d[o + 1]]));
    let le32 = |o: usize| u64::from(u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]));
    // bigalloc: the counters are in clusters of 2^(log_cluster - log_block) blocks.
    let cluster_ratio = if sb.feature_ro_compat & 0x0200 != 0 {
        1u64 << le32(0x1c).saturating_sub(le32(0x18)).min(30)
    } else {
        1
    };
    let recorded = le32(0x248);
    if recorded > 0 {
        return recorded.saturating_mul(cluster_ratio);
    }
    let bs = sb.block_size;
    let first_data = le32(0x14);
    let per_group = le32(0x20);
    let inodes_per_group = le32(0x28);
    if per_group == 0 || cluster_ratio != 1 {
        return 0;
    }
    let groups = sb.blocks_count.saturating_sub(first_data).div_ceil(per_group);
    if groups > 1 << 24 {
        return 0; // implausible: do not loop forever over a damaged superblock
    }
    let is64 = sb.feature_incompat & xoros_core::ext4::INCOMPAT_64BIT != 0;
    let desc_size = if is64 { le16(0xfe).max(64) } else { 32 };
    let desc_per_block = (bs / desc_size).max(1);
    let gdb_count = groups.div_ceil(desc_per_block);
    let reserved_gdt = le16(0xce);
    let meta_bg = sb.feature_incompat & 0x0010 != 0;
    let first_meta_bg = le32(0x104);
    let sparse_super = sb.feature_ro_compat & 0x0001 != 0;
    let sparse_super2 = sb.feature_compat & 0x0200 != 0;
    let backup_bgs = [le32(0x24c), le32(0x250)];
    let inode_size = if le32(0x4c) == 0 { 128 } else { le16(0x58) };
    let inode_table = (inodes_per_group * inode_size).div_ceil(bs);
    let is_power = |mut n: u64, base: u64| {
        while n > 1 && n.is_multiple_of(base) {
            n /= base;
        }
        n == 1
    };
    let has_super = |g: u64| {
        if g == 0 {
            true
        } else if sparse_super2 {
            backup_bgs.contains(&g)
        } else if g <= 1 || !sparse_super {
            true
        } else {
            g % 2 == 1 && (is_power(g, 3) || is_power(g, 5) || is_power(g, 7))
        }
    };
    let mut overhead = first_data;
    for g in 0..groups {
        let sup = u64::from(has_super(g));
        if !meta_bg || g < first_meta_bg * desc_per_block {
            if sup == 1 {
                overhead += 1 + if meta_bg { first_meta_bg } else { gdb_count } + reserved_gdt;
            }
        } else {
            let first = g / desc_per_block * desc_per_block;
            let gdt = g == first || g == first + 1 || g == first + desc_per_block - 1;
            overhead += sup + u64::from(gdt);
        }
        overhead += 2 + inode_table; // block and inode bitmap, inode table
    }
    // Internal journal: its size is backed up in s_jnl_blocks (i_size_high, i_size).
    if sb.has_journal() && le32(0xe0) != 0 && d[0xfd] == 1 {
        overhead += (le32(0x14c) | le32(0x148) << 32) / bs;
    }
    overhead
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
        super::hash_squashfs(layout.node(role.into()), progress, cancel, None)
    }

    fn external_sticks(&self) -> Result<Vec<Stick>> {
        Ok(self
            .stick_volumes()?
            .into_iter()
            .map(|(v, model)| Stick {
                id: v.name.clone(),
                description: format!(
                    "{}  {model}  {}  {}{}",
                    v.name,
                    format_bytes(v.size),
                    v.fs_type,
                    v.label.as_deref().map(|l| format!("  {l}")).unwrap_or_default()
                ),
                device: Some(v.node.display().to_string()),
                fs_type: Some(v.fs_type.as_str().to_string()),
                size: v.size,
            })
            .collect())
    }

    fn mount_stick(&self, stick: &Stick, writable: bool) -> Result<MountGuard> {
        // Look the stick up again: it must still exist and must not be on the boot disk. The
        // file system type was probed from the superblock, so only that driver is tried (no
        // vfat attempt on an ext4 stick, USB-1).
        let (v, _) = self
            .stick_volumes()?
            .into_iter()
            .find(|(v, _)| v.name == stick.id)
            .ok_or_else(|| {
                BackendError::new(
                    ErrorKind::Io,
                    format!("Stick {} is no longer present.", stick.id),
                )
            })?;
        self.mount_request(v.mount_request(&self.paths, writable))
    }

    fn mount_partition(&self, role: PartRole, writable: bool) -> Result<MountGuard> {
        check_persistent(role)?;
        let layout = self.core_layout()?;
        let part = layout.partition(role.into());
        let target = self.paths.mount_dir.join(role.label());
        self.mount_request(
            mount::MountRequest::new(&part.node, part.dev, target)
                .fs_type(FsType::Ext4)
                .writable(writable),
        )
    }

    fn mount_slot(&self, slot: Slot) -> Result<MountGuard> {
        let layout = self.core_layout()?;
        let part = layout.partition(PartRole::slot(slot).into());
        let target = self.paths.mount_dir.join(slot.label());
        self.mount_request(
            mount::MountRequest::new(&part.node, part.dev, target).fs_type(FsType::Squashfs),
        )
    }

    fn read_test_path(&self, role: Option<PartRole>) -> Result<PathBuf> {
        let layout = self.core_layout()?;
        Ok(match role {
            Some(r) => layout.node(r.into()).to_path_buf(),
            None => layout.disk.node.clone(),
        })
    }

    fn release_partition(&self, role: PartRole) -> Result<()> {
        check_persistent(role)?;
        let layout = self.core_layout()?;
        let part = layout.partition(role.into());
        for m in cdev::mount_points(&self.paths, part.dev).map_err(core_err)? {
            let host = self.paths.host_path(&m);
            if !host.starts_with(&self.paths.mount_dir) {
                return Err(BackendError::new(
                    ErrorKind::Io,
                    format!(
                        "{} is mounted at {}. Unmount it first (Tools -> Subshell: umount {}).",
                        part.node.display(),
                        m.display(),
                        m.display()
                    ),
                ));
            }
            mount::umount(&host).map_err(core_err)?;
            log::info!("unmounted {} from {}", part.node.display(), host.display());
        }
        let holders = cdev::holders(&self.paths, part.dev);
        if !holders.is_empty() {
            return Err(BackendError::new(
                ErrorKind::Io,
                format!("{} is in use by {}.", part.node.display(), holders.join(", ")),
            ));
        }
        Ok(())
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

    fn read_boot_file(&self, rel: &str) -> Result<Option<String>> {
        let layout = self.core_layout()?;
        let part = layout.partition(xoros_core::consts::PartitionRole::Boot);
        let guard = self.mount_request(
            mount::MountRequest::new(&part.node, part.dev, self.paths.mount_dir.join("boot-ro"))
                .fs_type(FsType::Vfat),
        )?;
        let path = guard.path().join(rel.trim_start_matches('/'));
        let r = match fs::read_to_string(&path) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(BackendError::io(path.display(), e)),
        };
        guard.unmount()?;
        r
    }

    fn stick_test_target(&self, disk: &str) -> Result<(PathBuf, u64)> {
        let d = super::check_stick_test_disk(&self.block_devices()?, disk)?;
        Ok((self.paths.dev.join(&d.name), d.size))
    }
}

#[cfg(test)]
mod tests;
