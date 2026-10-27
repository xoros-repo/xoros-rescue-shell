//! Simulation backend (SIM-1): system state described by `<dir>/sim.json`.
//!
//! Every read loads the file again (so it can be edited while the app runs), so the UI is fully
//! usable on a development PC. grubenv is a real 1024-byte file
//! `<dir>/boot/EFI/BOOT/grubenv` handled by the crate (`BootPartition::from_directory`), the
//! same code the device uses and the flash engine writes. An old `sim.json` with a `grubenv`
//! object is migrated to that file when it is opened. Slot writes (OTA-5, USB-3) go to
//! `<dir>/rootfs_a.img` / `rootfs_b.img` through `PartitionWriter::for_file` (created sparse
//! with the partition size when missing).
//! Image hashing reads `<dir>/<label>.img` (or the file named in `images`) completely.
//! External sticks are directories `<dir>/sticks/<name>/`, the persistent partitions are
//! directories `<dir>/parts/overlay/` and `<dir>/parts/application/`; "mounting" them only
//! returns the directory, and external tools (`e2fsck`) are logged, not run.
//!

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use xoros_core::boot as cboot;
use xoros_core::SystemPaths;

use super::core::{boot_state_from_core, core_err, reason_from_core};
use super::{
    check_persistent, BackendError, BlockDevice, BootPartition, BootState, BootTarget, BuildInfo,
    ErrorKind, FsUsage, Layout, LayoutEntry, MountGuard, PartRole, Partition, PartitionWriter,
    ProgressFn, RescueReason, RescueSystemInfo, Result, Slot, Stick, SystemBackend,
};
use crate::backup::tools::{CommandOutput, CommandRunner, ToolError};
use crate::constants::SIM_FILE_NAME;
use crate::hw::HostPaths;
use crate::tasks::{lock, CancelToken};

/// Content of `sim.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimState {
    #[serde(default)]
    pub description: Option<String>,
    /// Initial grubenv variables: written to `<dir>/boot/EFI/BOOT/grubenv` (and removed from
    /// here) when that file does not exist yet. Delete the file to simulate a missing grubenv
    /// (ENV-2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grubenv: Option<BTreeMap<String, String>>,
    /// `boot.slot=` of the running system.
    #[serde(default = "default_current_boot")]
    pub current_boot: String,
    /// `/xoros.env` per slot; `null` = empty/invalid slot.
    #[serde(default)]
    pub slots: SimSlots,
    #[serde(default)]
    pub rescue: RescueSystemInfo,
    #[serde(default)]
    pub disks: Vec<BlockDevice>,
    /// Usage of persistent partitions by label (`overlay`, `application`).
    #[serde(default)]
    pub usage: BTreeMap<String, FsUsage>,
    /// Image file per partition label, relative to the sim dir (default `<label>.img`).
    #[serde(default)]
    pub images: BTreeMap<String, String>,
    /// Artificial delay per MiB hashed, to make progress visible.
    #[serde(default)]
    pub hash_delay_ms_per_mib: u64,
    /// Override procfs/sysfs roots (default: the host's).
    #[serde(default)]
    pub proc_root: Option<PathBuf>,
    #[serde(default)]
    pub sys_root: Option<PathBuf>,
    /// File system type per stick directory name (e.g. `{"usb1": "vfat"}`), for BAK-3 splitting.
    #[serde(default)]
    pub stick_fs: BTreeMap<String, String>,
    /// Fault injection: operation name (e.g. `slot_info.B`, `layout`, `reset_tries`,
    /// `mount_stick`, `mount_partition.overlay`, `e2fsck`) -> error text. For `e2fsck` the
    /// simulated tool exits with code 4 and prints the text.
    #[serde(default)]
    pub fail: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimSlots {
    #[serde(default)]
    pub rootfs_a: Option<BuildInfo>,
    #[serde(default)]
    pub rootfs_b: Option<BuildInfo>,
}

fn default_current_boot() -> String {
    "RESCUE".to_string()
}

/// Simulation backend.
pub struct SimBackend {
    dir: PathBuf,
    write_lock: Mutex<()>,
}

const MIB: u64 = 1024 * 1024;

/// `bytes_used` of the example rescue image (rounded up to 4096 it is 12 MiB + 4 KiB).
const EXAMPLE_IMAGE_BYTES_USED: u64 = 12 * MIB + 1000;

impl SimBackend {
    /// Open a simulation directory; fails if `sim.json` is missing or invalid.
    pub fn open(dir: impl Into<PathBuf>) -> Result<SimBackend> {
        let b = SimBackend {
            dir: dir.into(),
            write_lock: Mutex::new(()),
        };
        b.load()?;
        b.migrate_grubenv()?;
        Ok(b)
    }

    /// `<dir>/boot`: the simulated boot partition content.
    pub fn boot_dir(&self) -> PathBuf {
        self.dir.join("boot")
    }

    /// `<dir>/boot/EFI/BOOT/grubenv`.
    pub fn grubenv_path(&self) -> PathBuf {
        self.boot_dir().join(xoros_core::consts::GRUBENV_REL_PATH)
    }

    /// Move `grubenv` from `sim.json` into the grubenv file (only if the file is missing).
    fn migrate_grubenv(&self) -> Result<()> {
        let _guard = lock(&self.write_lock);
        let mut state = self.load()?;
        let Some(vars) = state.grubenv.take() else {
            return Ok(());
        };
        let path = self.grubenv_path();
        if !path.exists() {
            let mut env = xoros_core::grubenv::GrubEnv::new();
            for (k, v) in &vars {
                env.set(k, v).map_err(core_err)?;
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    BackendError::io(format!("Cannot create {}", parent.display()), e)
                })?;
            }
            env.create(&path).map_err(core_err)?;
            log::info!("SIM: grubenv written to {}", path.display());
        }
        self.store(&state)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self) -> PathBuf {
        self.dir.join(SIM_FILE_NAME)
    }

    pub fn load(&self) -> Result<SimState> {
        let path = self.file();
        let text = fs::read_to_string(&path)
            .map_err(|e| BackendError::io(format!("Cannot read {}", path.display()), e))?;
        serde_json::from_str(&text).map_err(|e| {
            BackendError::new(
                ErrorKind::InvalidValue,
                format!("Invalid {}: {e}", path.display()),
            )
        })
    }

    fn store(&self, state: &SimState) -> Result<()> {
        let path = self.file();
        let tmp = self.dir.join(format!("{SIM_FILE_NAME}.tmp"));
        let text = serde_json::to_string_pretty(state).map_err(|e| {
            BackendError::new(ErrorKind::Other, format!("Cannot encode state: {e}"))
        })?;
        fs::write(&tmp, text + "\n")
            .map_err(|e| BackendError::io(format!("Cannot write {}", tmp.display()), e))?;
        fs::rename(&tmp, &path)
            .map_err(|e| BackendError::io(format!("Cannot replace {}", path.display()), e))
    }

    fn check_fail(state: &SimState, op: &str) -> Result<()> {
        match state.fail.get(op) {
            Some(msg) => Err(BackendError::new(ErrorKind::Io, msg.clone())),
            None => Ok(()),
        }
    }

    fn read_state(&self, op: &str) -> Result<cboot::BootState> {
        let state = self.load()?;
        Self::check_fail(&state, op)?;
        BootPartition::from_directory(self.boot_dir())
            .read()
            .map_err(core_err)
    }

    /// Read-modify-write of grubenv (same calls as the device backend).
    fn update_env(
        &self,
        op: &str,
        f: impl FnOnce(&mut cboot::BootState) -> xoros_core::Result<()>,
    ) -> Result<()> {
        let _guard = lock(&self.write_lock);
        let state = self.load()?;
        Self::check_fail(&state, op)?;
        BootPartition::from_directory(self.boot_dir())
            .update(f)
            .map_err(core_err)?;
        log::info!("SIM: grubenv: {op}");
        Ok(())
    }

    fn slot_image(&self, state: &SimState, slot: Slot) -> PathBuf {
        self.image_path(state, PartRole::slot(slot))
    }

    fn image_path(&self, state: &SimState, role: PartRole) -> PathBuf {
        let name = state
            .images
            .get(role.label())
            .cloned()
            .unwrap_or_else(|| format!("{}.img", role.label()));
        self.dir.join(name)
    }

    /// Write an example simulation directory: both slots exhausted, a boot disk and a USB
    /// stick, a 16 MiB `rescue.img` with deterministic content.
    pub fn create_example(dir: &Path) -> Result<()> {
        fs::create_dir_all(dir.join("sticks/usb1"))
            .map_err(|e| BackendError::io(format!("Cannot create {}", dir.display()), e))?;
        write_example_partitions(dir)
            .map_err(|e| BackendError::io(format!("Cannot fill {}/parts", dir.display()), e))?;
        fs::create_dir_all(dir.join("run"))
            .map_err(|e| BackendError::io(format!("Cannot create {}/run", dir.display()), e))?;
        let state = example_state();
        let b = SimBackend {
            dir: dir.to_path_buf(),
            write_lock: Mutex::new(()),
        };
        b.store(&state)?;
        b.migrate_grubenv()?;
        // Deterministic pseudo-random rescue "partition" (xorshift) with a squashfs superblock:
        // the image is the first `image_len()` bytes, the rest is unused partition space that
        // must not be hashed (section 4.1).
        let mut data = vec![0u8; (16 * MIB) as usize];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for chunk in data.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let bytes = x.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        data[..xoros_core::squashfs::SUPERBLOCK_SIZE].copy_from_slice(
            &xoros_core::squashfs::fake_superblock(EXAMPLE_IMAGE_BYTES_USED),
        );
        let image_len = EXAMPLE_IMAGE_BYTES_USED.div_ceil(4096) * 4096;
        let img = dir.join("rescue.img");
        fs::write(&img, &data)
            .map_err(|e| BackendError::io(format!("Cannot write {}", img.display()), e))?;
        // Make both slots claim this rescue image.
        let sha = hex(&Sha256::digest(&data[..image_len as usize]));
        let mut state = b.load()?;
        for info in [state.slots.rootfs_a.as_mut(), state.slots.rootfs_b.as_mut()]
            .into_iter()
            .flatten()
        {
            info.rescue_image_sha256 = Some(sha.clone());
        }
        b.store(&state)?;
        write_example_updates(dir, &data[..image_len as usize])
            .map_err(|e| BackendError::io(format!("Cannot write the example OTA tree in {}", dir.display()), e))
    }
}

/// Build timestamp of the example update (newer than both example slots).
pub const EXAMPLE_UPDATE_TIMESTAMP: &str = "20261020120000";

/// Example updates (TEST-2, USB-2): an OTA tree `<dir>/ota/1.0/qemux86-64/devel/` to serve
/// over HTTP (`python3 -m http.server -d <dir>/ota`), the same tree on the stick
/// `sticks/usb1/xoros/ota/...` and a single image `sticks/usb1/images/rootfs.squashfs` with
/// sidecars. The rescue image of the manifest is the simulated running one.
fn write_example_updates(dir: &Path, rescue: &[u8]) -> std::io::Result<()> {
    use crate::update_jobs::example::{fake_image, write_ota_tree, write_single_image};
    use xoros_core::env::Suffix;
    use xoros_core::manifest::{ImageType, Target};
    let target = Target::new("1.0", "qemux86-64", Suffix::Devel);
    let image = fake_image(8 * MIB as usize, 42);
    let images = [
        (ImageType::Rootfs, image.as_slice(), EXAMPLE_UPDATE_TIMESTAMP),
        (ImageType::Rescue, rescue, EXAMPLE_UPDATE_TIMESTAMP),
    ];
    let rel = target.rel_path();
    write_ota_tree(&dir.join("ota").join(&rel), &target, &images, 2 * MIB)?;
    write_ota_tree(
        &dir.join("sticks/usb1/xoros/ota").join(&rel),
        &target,
        &images[..1],
        2 * MIB,
    )?;
    write_single_image(
        &dir.join("sticks/usb1/images"),
        "rootfs.squashfs",
        &fake_image(4 * MIB as usize, 43),
        &target,
        Some("20261025080000"),
    )
}

/// Example content of the persistent partitions: a few `/etc` changes, rescue settings and
/// application data.
fn write_example_partitions(dir: &Path) -> std::io::Result<()> {
    let files: &[(&str, &str)] = &[
        (
            "parts/overlay/overlays/etc/upper/hostname",
            "xoros-device\n",
        ),
        (
            "parts/overlay/overlays/etc/upper/NetworkManager/system-connections/eth0.nmconnection",
            "[connection]\nid=eth0\ntype=ethernet\n",
        ),
        (
            "parts/overlay/rescue/rescue-shell.toml",
            "theme = \"default\"\n",
        ),
        (
            "parts/application/docker/volumes/db/_data/data.bin",
            "example data\n",
        ),
        (
            "parts/application/app/config.json",
            "{\"name\": \"demo\"}\n",
        ),
    ];
    for (rel, content) in files {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(p, content)?;
    }
    fs::create_dir_all(dir.join("parts/overlay/overlays/etc/work"))?;
    fs::create_dir_all(dir.join("parts/overlay/lost+found"))?;
    fs::create_dir_all(dir.join("parts/application/lost+found"))
}

/// `e2fsck`/`mke2fs` stand-in for simulation mode: logs the command line and succeeds, or exits
/// with code 4 when `fail.e2fsck` / `fail.mke2fs` is set in `sim.json`.
pub struct SimRunner {
    fail: BTreeMap<String, String>,
}

impl CommandRunner for SimRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::result::Result<CommandOutput, ToolError> {
        log::info!("SIM: would run '{program} {}'", args.join(" "));
        Ok(match self.fail.get(program) {
            Some(msg) => CommandOutput {
                code: Some(4),
                stdout: String::new(),
                stderr: msg.clone(),
            },
            None => CommandOutput {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            },
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl SystemBackend for SimBackend {
    fn describe(&self) -> String {
        format!("simulation: {}", self.dir.display())
    }

    fn is_simulation(&self) -> bool {
        true
    }

    fn run_dir(&self) -> PathBuf {
        self.dir.join("run")
    }

    fn host_paths(&self) -> HostPaths {
        let mut p = HostPaths::default();
        if let Ok(s) = self.load() {
            if let Some(r) = s.proc_root {
                p.proc_root = self.dir.join(r);
            }
            if let Some(r) = s.sys_root {
                p.sys_root = self.dir.join(r);
            }
        }
        p
    }

    fn boot_state(&self) -> Result<BootState> {
        Ok(boot_state_from_core(&self.read_state("boot_state")?))
    }

    fn current_boot(&self) -> Result<BootTarget> {
        let s = self.load()?;
        BootTarget::from_str(&s.current_boot)
    }

    fn predict(&self) -> Result<BootTarget> {
        Ok(self.read_state("boot_state")?.predict().into())
    }

    fn rescue_reason(&self) -> Result<RescueReason> {
        Ok(reason_from_core(
            self.read_state("boot_state")?.rescue_reason(),
        ))
    }

    fn reset_tries(&self, slot: Slot) -> Result<()> {
        self.update_env("reset_tries", |s| s.reset_tries(slot.into()))
    }

    fn disable(&self, slot: Slot) -> Result<()> {
        self.update_env("disable", |s| s.disable(slot.into()))
    }

    fn set_next_boot(&self, target: Option<BootTarget>) -> Result<()> {
        self.update_env("set_next_boot", |s| {
            s.set_next_boot(target.map(Into::into))
        })
    }

    fn set_order(&self, first: Slot) -> Result<()> {
        self.update_env("set_order", |s| s.set_order(first.into()))
    }

    fn set_max_tries(&self, max_tries: u8) -> Result<()> {
        self.update_env("set_max_tries", |s| {
            s.set_max_tries(u32::from(max_tries))
        })
    }

    fn set_rotation(&self, rotation: u8) -> Result<()> {
        self.update_env("set_rotation", |s| s.set_rotation(rotation))
    }

    fn recreate_grubenv(&self) -> Result<()> {
        let _guard = lock(&self.write_lock);
        let state = self.load()?;
        Self::check_fail(&state, "recreate_grubenv")?;
        BootPartition::from_directory(self.boot_dir())
            .recreate(&cboot::BootState::default())
            .map_err(core_err)
    }

    fn slot_info(&self, slot: Slot) -> Result<Option<BuildInfo>> {
        let s = self.load()?;
        Self::check_fail(&s, &format!("slot_info.{}", slot.letter()))?;
        Ok(match slot {
            Slot::A => s.slots.rootfs_a,
            Slot::B => s.slots.rootfs_b,
        })
    }

    fn rescue_system(&self) -> Result<RescueSystemInfo> {
        let s = self.load()?;
        Self::check_fail(&s, "rescue_system")?;
        Ok(s.rescue)
    }

    fn block_devices(&self) -> Result<Vec<BlockDevice>> {
        let s = self.load()?;
        Self::check_fail(&s, "block_devices")?;
        Ok(s.disks)
    }

    fn boot_disk(&self) -> Result<BlockDevice> {
        self.block_devices()?
            .into_iter()
            .find(|d| d.boot_disk)
            .ok_or_else(|| BackendError::new(ErrorKind::Other, "Boot disk not found."))
    }

    fn layout(&self) -> Result<Layout> {
        let s = self.load()?;
        Self::check_fail(&s, "layout")?;
        let disk = self.boot_disk()?;
        let mut entries = Vec::new();
        for role in PartRole::ALL {
            let part: &Partition = disk
                .partitions
                .iter()
                .find(|p| p.number == role.number())
                .ok_or_else(|| {
                    BackendError::new(
                        ErrorKind::Other,
                        format!(
                            "Partition {} ({}) missing on {}.",
                            role.number(),
                            role.label(),
                            disk.name
                        ),
                    )
                })?;
            if part.partlabel.as_deref() != Some(role.label()) {
                return Err(BackendError::new(
                    ErrorKind::Other,
                    format!(
                        "Partition {} on {} has label {:?}, expected '{}'.",
                        role.number(),
                        disk.name,
                        part.partlabel.as_deref().unwrap_or(""),
                        role.label()
                    ),
                ));
            }
            entries.push(LayoutEntry {
                role,
                device: format!("/dev/{}", part.name),
                size: part.size,
            });
        }
        Ok(Layout {
            disk: disk.name,
            entries,
        })
    }

    fn partition_usage(&self, role: PartRole) -> Result<Option<FsUsage>> {
        let s = self.load()?;
        Self::check_fail(&s, &format!("partition_usage.{}", role.label()))?;
        Ok(s.usage.get(role.label()).copied())
    }

    fn hash_image(
        &self,
        role: PartRole,
        progress: ProgressFn<'_>,
        cancel: &CancelToken,
    ) -> Result<String> {
        let s = self.load()?;
        Self::check_fail(&s, &format!("hash_image.{}", role.label()))?;
        let path = self.image_path(&s, role);
        let f = fs::File::open(&path)
            .map_err(|e| BackendError::io(format!("Cannot open {}", path.display()), e))?;
        let delay =
            (s.hash_delay_ms_per_mib > 0).then(|| Duration::from_millis(s.hash_delay_ms_per_mib));
        super::hash_squashfs(f, &path.display().to_string(), progress, cancel, delay)
    }

    fn external_sticks(&self) -> Result<Vec<Stick>> {
        let s = self.load()?;
        Self::check_fail(&s, "external_sticks")?;
        let dir = self.dir.join("sticks");
        let mut out = Vec::new();
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => {
                return Err(BackendError::io(
                    format!("Cannot list {}", dir.display()),
                    e,
                ))
            }
        };
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let id = e.file_name().to_string_lossy().into_owned();
            let fs_type = s.stick_fs.get(&id).cloned();
            out.push(Stick {
                description: format!(
                    "{id}  (simulated stick{})",
                    fs_type
                        .as_deref()
                        .map(|t| format!(", {t}"))
                        .unwrap_or_default()
                ),
                id,
                device: None,
                fs_type,
                size: 0,
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn mount_stick(&self, stick: &Stick, writable: bool) -> Result<MountGuard> {
        let s = self.load()?;
        Self::check_fail(&s, "mount_stick")?;
        if stick.id.is_empty() || stick.id.contains('/') || stick.id.starts_with('.') {
            return Err(BackendError::invalid("stick", &stick.id));
        }
        let dir = self.dir.join("sticks").join(&stick.id);
        if !dir.is_dir() {
            return Err(BackendError::new(
                ErrorKind::Io,
                format!("Stick {} is no longer present.", stick.id),
            ));
        }
        log::info!(
            "SIM: stick {} at {} ({})",
            stick.id,
            dir.display(),
            if writable { "rw" } else { "ro" }
        );
        Ok(MountGuard::directory(dir))
    }

    fn mount_partition(&self, role: PartRole, writable: bool) -> Result<MountGuard> {
        check_persistent(role)?;
        let s = self.load()?;
        Self::check_fail(&s, &format!("mount_partition.{}", role.label()))?;
        let dir = self.dir.join("parts").join(role.label());
        fs::create_dir_all(&dir)
            .map_err(|e| BackendError::io(format!("Cannot create {}", dir.display()), e))?;
        log::info!(
            "SIM: partition {} at {} ({})",
            role.label(),
            dir.display(),
            if writable { "rw" } else { "ro" }
        );
        Ok(MountGuard::directory(dir))
    }

    fn command_runner(&self) -> Box<dyn CommandRunner> {
        let fail = self.load().map(|s| s.fail).unwrap_or_default();
        Box::new(SimRunner { fail })
    }

    fn boot_partition(&self) -> Result<BootPartition> {
        let s = self.load()?;
        Self::check_fail(&s, "boot_partition")?;
        Ok(BootPartition::from_directory(self.boot_dir()))
    }

    fn open_slot_writer(&self, slot: Slot) -> Result<PartitionWriter> {
        let s = self.load()?;
        Self::check_fail(&s, &format!("open_slot_writer.{}", slot.letter()))?;
        if BootTarget::from_str(&s.current_boot).ok() == Some(BootTarget::Slot(slot)) {
            return Err(BackendError::new(
                ErrorKind::InvalidValue,
                format!("Write refused: {slot} is the running system."),
            ));
        }
        let path = self.slot_image(&s, slot);
        if !path.exists() {
            // A sparse file of the partition size from sim.json (default 1 GiB).
            let size = self
                .layout()
                .ok()
                .and_then(|l| l.get(PartRole::slot(slot)).map(|e| e.size))
                .filter(|s| *s > 0)
                .unwrap_or(1024 * MIB);
            let f = fs::File::create(&path)
                .and_then(|f| f.set_len(size).map(|_| f))
                .map_err(|e| BackendError::io(format!("Cannot create {}", path.display()), e))?;
            drop(f);
            log::info!("SIM: created {} ({size} bytes)", path.display());
        }
        PartitionWriter::for_file(&path, &SystemPaths::default().with_simulation(true))
            .map_err(core_err)
    }

    fn open_slot_reader(&self, slot: Slot) -> Result<fs::File> {
        let s = self.load()?;
        Self::check_fail(&s, &format!("open_slot_reader.{}", slot.letter()))?;
        let path = self.slot_image(&s, slot);
        fs::File::open(&path)
            .map_err(|e| BackendError::io(format!("Cannot open {}", path.display()), e))
    }

    fn slot_written(&self, slot: Slot, info: &BuildInfo) -> Result<()> {
        let _guard = lock(&self.write_lock);
        let mut s = self.load()?;
        let info = Some(info.clone());
        match slot {
            Slot::A => s.slots.rootfs_a = info,
            Slot::B => s.slots.rootfs_b = info,
        }
        log::info!("SIM: {slot} build info recorded");
        self.store(&s)
    }

    /// `<sim>/slots/<label>/`: a directory standing for the slot's root file system (NET-8).
    fn mount_slot(&self, slot: Slot) -> Result<MountGuard> {
        let s = self.load()?;
        Self::check_fail(&s, &format!("mount_slot.{}", slot.letter()))?;
        let dir = self.dir.join("slots").join(slot.label());
        if !dir.is_dir() {
            return Err(BackendError::new(
                ErrorKind::Io,
                format!("{} has no simulated root file system.", slot.label()),
            ));
        }
        Ok(MountGuard::directory(dir))
    }
}

fn example_build(suffix: &str, ts: &str) -> BuildInfo {
    BuildInfo {
        distro: "xoros".into(),
        version: "1.0".into(),
        machine: "qemux86-64".into(),
        suffix: suffix.into(),
        timestamp: ts.into(),
        rescue_image_sha256: None,
        extra: BTreeMap::new(),
    }
}

fn part(number: u32, disk: &str, label: &str, size_mib: u64, fstype: &str) -> Partition {
    Partition {
        number,
        name: format!("{disk}{number}"),
        partlabel: Some(label.into()),
        partuuid: None,
        size: size_mib * MIB,
        fstype: Some(fstype.into()),
        mountpoint: None,
    }
}

/// The example state written by [`SimBackend::create_example`].
pub fn example_state() -> SimState {
    let mut env = BTreeMap::new();
    env.insert("ROOTFS_A_TRY".into(), "3".into());
    env.insert("ROOTFS_B_TRY".into(), "3".into());
    env.insert("LAST_BOOT".into(), "RESCUE".into());
    let mut boot_disk = BlockDevice {
        name: "sda".into(),
        model: Some("QEMU HARDDISK".into()),
        size: 8 * 1024 * MIB,
        removable: false,
        transport: "sata".into(),
        boot_disk: true,
        partitions: vec![
            part(1, "sda", "boot", 64, "vfat"),
            part(2, "sda", "rootfs_a", 1024, "squashfs"),
            part(3, "sda", "rootfs_b", 1024, "squashfs"),
            part(4, "sda", "rescue", 704, "squashfs"),
            part(5, "sda", "overlay", 256, "ext4"),
            part(6, "sda", "application", 4096, "ext4"),
        ],
    };
    boot_disk.partitions[0].partuuid = Some("0bce0f6a-9fb1-4cc7-9d57-6278af875e19".into());
    boot_disk.partitions[1].partuuid = Some("aa508ba4-56d0-4f98-b098-21f51b7b6407".into());
    boot_disk.partitions[2].partuuid = Some("bb91087f-3fce-4b60-a48f-9635e9f43a94".into());
    boot_disk.partitions[3].partuuid = Some("c1e8de08-5724-494f-bb12-634765e37cfb".into());
    boot_disk.partitions[3].mountpoint = Some("/".into());
    let stick = BlockDevice {
        name: "sdb".into(),
        model: Some("SanDisk Ultra".into()),
        size: 32 * 1024 * MIB,
        removable: true,
        transport: "usb".into(),
        boot_disk: false,
        partitions: vec![Partition {
            number: 1,
            name: "sdb1".into(),
            partlabel: None,
            partuuid: None,
            size: 32 * 1024 * MIB - MIB,
            fstype: Some("vfat".into()),
            mountpoint: None,
        }],
    };
    let mut usage = BTreeMap::new();
    usage.insert(
        "overlay".into(),
        FsUsage {
            total: 240 * MIB,
            used: 3 * MIB,
        },
    );
    usage.insert(
        "application".into(),
        FsUsage {
            total: 3900 * MIB,
            used: 1200 * MIB,
        },
    );
    SimState {
        description: Some("Example: both slots exceeded MAX_TRIES (TEST-3 case 1)".into()),
        grubenv: Some(env),
        current_boot: "RESCUE".into(),
        slots: SimSlots {
            rootfs_a: Some(example_build("devel", "20261001120000")),
            rootfs_b: Some(example_build("devel", "20261015080000")),
        },
        rescue: RescueSystemInfo {
            version_id: Some("1.0".into()),
            name: Some("Xoros Rescue".into()),
            hostname: Some("xoros-rescue-qemux86-64".into()),
        },
        disks: vec![boot_disk, stick],
        usage,
        images: BTreeMap::new(),
        hash_delay_ms_per_mib: 100,
        proc_root: None,
        sys_root: None,
        stick_fs: [("usb1".to_string(), "vfat".to_string())].into(),
        fail: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::testutil::TempDir;

    fn sim() -> (TempDir, SimBackend) {
        let d = TempDir::new("sim");
        SimBackend::create_example(d.path()).expect("create example");
        let b = SimBackend::open(d.path()).expect("open");
        (d, b)
    }

    #[test]
    fn example_reads_as_exhausted() {
        let (_d, b) = sim();
        let st = b.boot_state().expect("state");
        assert_eq!(st.tries, [3, 3]);
        assert_eq!(st.max_tries, 3);
        assert_eq!(st.order, [Slot::A, Slot::B]);
        assert_eq!(st.last_boot, Some(BootTarget::Rescue));
        assert_eq!(b.rescue_reason().ok(), Some(RescueReason::SlotsExhausted));
        assert_eq!(b.predict().ok(), Some(BootTarget::Rescue));
        assert_eq!(b.current_boot().ok(), Some(BootTarget::Rescue));
        assert!(b.is_simulation());
    }

    #[test]
    fn mutations_persist() {
        let (_d, b) = sim();
        b.reset_tries(Slot::A).expect("reset");
        assert_eq!(b.predict().ok(), Some(BootTarget::Slot(Slot::A)));
        assert_eq!(b.rescue_reason().ok(), Some(RescueReason::Requested));
        b.set_order(Slot::B).expect("order");
        b.reset_tries(Slot::B).expect("reset");
        assert_eq!(b.predict().ok(), Some(BootTarget::Slot(Slot::B)));
        b.set_next_boot(Some(BootTarget::Rescue)).expect("next");
        let st = b.boot_state().expect("state");
        assert_eq!(st.next_boot, Some(BootTarget::Rescue));
        assert_eq!(b.predict().ok(), Some(BootTarget::Rescue));
        assert!(st
            .raw
            .iter()
            .any(|(k, v)| k == "NEXT_BOOT" && v == "MENU_RESCUE"));
        b.set_next_boot(None).expect("clear");
        assert_eq!(b.boot_state().map(|s| s.next_boot).ok(), Some(None));
        b.set_max_tries(5).expect("max");
        b.disable(Slot::A).expect("disable");
        assert_eq!(b.boot_state().map(|s| s.try_count(Slot::A)).ok(), Some(5));
        assert!(b.set_max_tries(0).is_err());
        assert!(b.set_max_tries(21).is_err());
        b.set_rotation(2).expect("rot");
        assert!(b.set_rotation(4).is_err());
        assert_eq!(b.boot_state().map(|s| s.rotation).ok(), Some(2));
    }

    #[test]
    fn unknown_vars_preserved() {
        let (_d, b) = sim();
        let mut env = xoros_core::grubenv::GrubEnv::read(b.grubenv_path()).expect("read");
        env.set("CUSTOM", "keep me").expect("set");
        env.write_in_place(b.grubenv_path()).expect("write");
        b.reset_tries(Slot::B).expect("reset");
        let st = b.boot_state().expect("state");
        assert!(st.raw.iter().any(|(k, v)| k == "CUSTOM" && v == "keep me"));
        assert_eq!(fs::metadata(b.grubenv_path()).map(|m| m.len()).ok(), Some(1024));
    }

    #[test]
    fn old_sim_json_grubenv_is_migrated() {
        let d = TempDir::new("sim-migrate");
        d.write(
            "sim.json",
            br#"{"grubenv": {"ROOTFS_A_TRY": "2", "ORDER": "ROOTFS_B ROOTFS_A"}}"#,
        );
        let b = SimBackend::open(d.path()).expect("open");
        let st = b.boot_state().expect("state");
        assert_eq!(st.tries, [2, 0]);
        assert_eq!(st.order, [Slot::B, Slot::A]);
        assert_eq!(b.load().expect("load").grubenv, None);
    }

    #[test]
    fn broken_grubenv_and_recreate() {
        let (_d, b) = sim();
        fs::remove_file(b.grubenv_path()).expect("rm");
        assert_eq!(
            b.boot_state().err().map(|e| e.kind),
            Some(ErrorKind::GrubenvBroken)
        );
        assert_eq!(
            b.reset_tries(Slot::A).err().map(|e| e.kind),
            Some(ErrorKind::GrubenvBroken)
        );
        b.recreate_grubenv().expect("recreate");
        let st = b.boot_state().expect("state");
        assert_eq!(st.tries, [0, 0]);
        assert_eq!(st.max_tries, crate::constants::DEFAULT_MAX_TRIES);
    }

    #[test]
    fn layout_and_devices() {
        let (_d, b) = sim();
        let l = b.layout().expect("layout");
        assert_eq!(l.disk, "sda");
        assert_eq!(
            l.get(PartRole::Rescue).map(|e| e.device.as_str()),
            Some("/dev/sda4")
        );
        assert_eq!(b.block_devices().map(|d| d.len()).ok(), Some(2));
        assert_eq!(b.boot_disk().map(|d| d.name).ok(), Some("sda".to_string()));
        // Wrong label -> error.
        let mut s = b.load().expect("load");
        s.disks[0].partitions[2].partlabel = Some("other".into());
        b.store(&s).expect("store");
        assert!(b.layout().is_err());
    }

    #[test]
    fn slot_info_and_fault_injection() {
        let (_d, b) = sim();
        assert_eq!(
            b.slot_info(Slot::B).ok().flatten().map(|i| i.timestamp),
            Some("20261015080000".into())
        );
        let mut s = b.load().expect("load");
        s.slots.rootfs_a = None;
        s.fail.insert("slot_info.B".into(), "I/O error".into());
        b.store(&s).expect("store");
        assert_eq!(b.slot_info(Slot::A).ok(), Some(None));
        assert_eq!(
            b.slot_info(Slot::B).err().map(|e| e.message),
            Some("I/O error".into())
        );
    }

    #[test]
    fn hash_image_matches_and_cancels() {
        let (d, b) = sim();
        let mut s = b.load().expect("load");
        s.hash_delay_ms_per_mib = 0;
        b.store(&s).expect("store");
        let mut last = (0, 0);
        let sha = b
            .hash_image(
                PartRole::Rescue,
                &mut |d, t| last = (d, t),
                &CancelToken::new(),
            )
            .expect("hash");
        // Only the squashfs image (round_up(bytes_used, 4096)) is hashed, not the whole file.
        assert_eq!(last, (12 * MIB + 4096, 12 * MIB + 4096));
        let expected = b
            .slot_info(Slot::A)
            .ok()
            .flatten()
            .and_then(|i| i.rescue_image_sha256);
        assert_eq!(Some(sha), expected);
        let c = CancelToken::new();
        c.cancel();
        let r = b.hash_image(PartRole::Rescue, &mut |_, _| {}, &c);
        assert_eq!(r.err().map(|e| e.kind), Some(ErrorKind::Cancelled));
        // Missing image -> I/O error.
        let _ = fs::remove_file(d.path().join("rescue.img"));
        let r = b.hash_image(PartRole::Rescue, &mut |_, _| {}, &CancelToken::new());
        assert_eq!(r.err().map(|e| e.kind), Some(ErrorKind::Io));
    }

    #[test]
    fn invalid_sim_file() {
        let d = TempDir::new("sim-bad");
        assert!(SimBackend::open(d.path()).is_err());
        d.write("sim.json", b"{ \"unknown_field\": 1 }");
        assert!(SimBackend::open(d.path()).is_err());
        d.write("sim.json", b"{}");
        let b = SimBackend::open(d.path()).expect("minimal");
        assert_eq!(
            b.boot_state().err().map(|e| e.kind),
            Some(ErrorKind::GrubenvBroken)
        );
    }
}
