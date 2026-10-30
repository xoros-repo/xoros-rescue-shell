//! Persistent partition maintenance (BAK-6) and raw partition images (BAK-7). Run in worker
//! threads (UI-3); no ratatui here (ARCH-1).
//!
//! - BAK-6: `e2fsck -y`, then a mount test; if the file system still cannot be mounted the UI
//!   offers "Re-create file system" (`mke2fs -t ext4 -L <label> -U <uuid>`, typed confirmation).
//! - BAK-7: a gzip-compressed raw copy of any partition of the boot disk on a stick
//!   (`xoros-image-<label>-<host>-<YYYYMMDD-HHMMSS>.img.gz` + `.sha256`, split on FAT sticks
//!   like backups, BAK-3). The partition is only read.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

use crate::backend::{PartRole, Stick, SystemBackend};
use crate::backup::tools::{e2fsck_force, mke2fs_ext4_uuid, sync_all, FsckOutcome, ToolError};
use crate::backup::{PartInfo, PartitionRole, SplitWriter};
use crate::backup_jobs::{device_identity, split_size};
use crate::constants::{OVERLAY_UPPER_DIR, OVERLAY_WORK_DIR};
use crate::tasks::{format_bytes, TaskCtx, TaskFailure};

/// Word to type before `mke2fs` (UI-4).
pub const MKFS_CONFIRM_WORD: &str = "MKFS";

fn err(e: impl std::fmt::Display) -> TaskFailure {
    TaskFailure::Error(e.to_string())
}

fn persistent(role: PartRole) -> Result<PartitionRole, TaskFailure> {
    match role {
        PartRole::Overlay => Ok(PartitionRole::Overlay),
        PartRole::Application => Ok(PartitionRole::Application),
        other => Err(err(format!(
            "{} is not a persistent partition.",
            other.label()
        ))),
    }
}

fn device_of(backend: &dyn SystemBackend, role: PartRole) -> Result<String, TaskFailure> {
    let layout = backend.layout()?;
    layout
        .get(role)
        .map(|e| e.device.clone())
        .ok_or_else(|| err(format!("No {} partition on the boot disk.", role.label())))
}

// --------------------------------------------------------------------------------------------
// BAK-6

/// Result of [`repair`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairOutcome {
    pub role: PartRole,
    pub device: String,
    /// e2fsck result in one line.
    pub fsck: String,
    /// `None`: the partition mounts; otherwise the mount error (offer re-creating it).
    pub mount_error: Option<String>,
}

impl RepairOutcome {
    pub fn mountable(&self) -> bool {
        self.mount_error.is_none()
    }

    pub fn text(&self) -> String {
        let mut t = format!(
            "{} ({}):\n  e2fsck -y: {}\n",
            self.role.label(),
            self.device,
            self.fsck
        );
        match &self.mount_error {
            None => t.push_str("  The file system mounts again.\n"),
            Some(e) => t.push_str(&format!(
                "  It still cannot be mounted: {e}\n\n\
                 Re-creating the file system erases everything on it. Restore a backup \
                 afterwards (Backup -> Import).\n"
            )),
        }
        t
    }
}

fn fsck_text(r: Result<FsckOutcome, ToolError>) -> Result<String, TaskFailure> {
    Ok(match r {
        Ok(FsckOutcome::Clean) => "clean".into(),
        Ok(FsckOutcome::Corrected) => "errors corrected".into(),
        Ok(FsckOutcome::Uncorrected { code, output }) => format!(
            "errors left (exit code {code}) {}",
            crate::report::ellipsize(output.stderr.trim(), 200)
        ),
        Err(e @ ToolError::Missing(_)) => return Err(err(e)),
        Err(e) => format!("failed: {e}"),
    })
}

fn mount_test(backend: &dyn SystemBackend, role: PartRole) -> Option<String> {
    match backend.mount_partition(role, false) {
        Ok(g) => {
            if let Err(e) = g.unmount() {
                log::warn!("unmount {}: {e}", role.label());
            }
            None
        }
        Err(e) => Some(e.to_string()),
    }
}

/// BAK-6 step 1: `e2fsck -y` on the (released) partition, then try to mount it.
pub fn repair(
    backend: &dyn SystemBackend,
    role: PartRole,
    ctx: &TaskCtx,
) -> Result<RepairOutcome, TaskFailure> {
    persistent(role)?;
    let device = device_of(backend, role)?;
    ctx.set_message(format!("Releasing {}", role.label()));
    backend.release_partition(role)?;
    ctx.set_message(format!("e2fsck -y {device}"));
    log::info!("action: e2fsck -y {device} ({})", role.label());
    let fsck = fsck_text(e2fsck_force(backend.command_runner().as_ref(), &device))?;
    log::info!("action: e2fsck -y {device}: {fsck}");
    ctx.set_message("Mount test");
    let mount_error = mount_test(backend, role);
    Ok(RepairOutcome {
        role,
        device,
        fsck,
        mount_error,
    })
}

/// File system UUID to keep: the fixed one of the distro (application), else the one in the
/// old superblock if it can still be read.
fn uuid_to_keep(role: PartitionRole, device: &str) -> Option<String> {
    if let Some(u) = role.fs_uuid() {
        return Some(u.to_string());
    }
    xoros_core::ext4::Superblock::read_path(device)
        .ok()
        .map(|sb| sb.uuid)
        .filter(|u| !u.is_empty() && u.chars().any(|c| c != '0' && c != '-'))
}

/// BAK-6 step 2: `mke2fs -t ext4 -L <label> [-U <uuid>]` on the persistent partition, after
/// typed confirmation. For overlay the overlayfs directories are created again.
pub fn recreate_fs(
    backend: &dyn SystemBackend,
    role: PartRole,
    confirmation: &str,
    ctx: &TaskCtx,
) -> Result<String, TaskFailure> {
    let prole = persistent(role)?;
    if confirmation != MKFS_CONFIRM_WORD {
        return Err(err(format!(
            "Not confirmed: type {MKFS_CONFIRM_WORD} to re-create the file system."
        )));
    }
    let device = device_of(backend, role)?;
    ctx.set_message(format!("Releasing {}", role.label()));
    backend.release_partition(role)?;
    let uuid = uuid_to_keep(prole, &device);
    ctx.set_message(format!("mke2fs {device}"));
    log::warn!(
        "action: mke2fs -t ext4 -L {} -U {} {device}",
        prole.fs_label(),
        uuid.as_deref().unwrap_or("(random)")
    );
    mke2fs_ext4_uuid(
        backend.command_runner().as_ref(),
        &device,
        prole,
        uuid.as_deref(),
    )
    .map_err(|e| {
        log::error!("action: mke2fs {device}: {e}");
        err(e)
    })?;
    let g = backend.mount_partition(role, true)?;
    if role == PartRole::Overlay {
        for d in [OVERLAY_UPPER_DIR, OVERLAY_WORK_DIR] {
            std::fs::create_dir_all(g.path().join(d))?;
        }
    }
    sync_all();
    g.unmount()?;
    log::info!("action: mke2fs {device}: ok");
    Ok(format!(
        "New ext4 file system on {} ({device}), label '{}'{}.\n\
         It is empty: restore a backup with Backup -> Import.",
        role.label(),
        prole.fs_label(),
        uuid.map(|u| format!(", UUID {u}")).unwrap_or_default()
    ))
}

// --------------------------------------------------------------------------------------------
// BAK-7

/// Partitions offered for a raw image (all of the boot disk; reading is harmless).
pub const IMAGE_ROLES: &[PartRole] = &[
    PartRole::Overlay,
    PartRole::Application,
    PartRole::RootfsA,
    PartRole::RootfsB,
    PartRole::Rescue,
    PartRole::Boot,
];

/// File name of a raw image (without part suffix).
pub fn image_name(role: PartRole, host: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let host: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "xoros-image-{}-{host}-{}.img.gz",
        role.label(),
        now.format("%Y%m%d-%H%M%S")
    )
}

/// Result of [`export_image`].
#[derive(Clone, Debug)]
pub struct ImageReport {
    pub name: String,
    pub source: String,
    pub raw_bytes: u64,
    pub raw_sha256: String,
    pub parts: Vec<PartInfo>,
}

impl ImageReport {
    pub fn text(&self, stick: &Stick) -> String {
        let gz: u64 = self.parts.iter().map(|p| p.size).sum();
        format!(
            "Written to {}:\n  {}{}\n  {}.sha256\n\n{} read from {} ({} compressed).\n\
             SHA-256 of the raw data: {}",
            stick.id,
            self.name,
            if self.parts.len() > 1 {
                format!(" (.000 .. .{:03})", self.parts.len() - 1)
            } else {
                String::new()
            },
            self.name,
            format_bytes(self.raw_bytes),
            self.source,
            format_bytes(gz),
            self.raw_sha256
        )
    }
}

const BLOCK: usize = 1 << 20;

/// Copy `input` (`size` bytes) gzip-compressed into `dir/<name>[.NNN]` and write
/// `dir/<name>.sha256`. Created files are deleted on error or cancel.
pub fn write_image(
    input: &mut dyn Read,
    size: u64,
    dir: &Path,
    name: &str,
    split: Option<u64>,
    ctx: &TaskCtx,
) -> Result<(u64, String, Vec<PartInfo>), TaskFailure> {
    let sink = SplitWriter::new(dir, name, split)?;
    let created = std::rc::Rc::clone(&sink.created);
    let sha_path = dir.join(format!("{name}.sha256"));
    let cleanup = || {
        for p in created.borrow().iter() {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_file(&sha_path);
    };
    let result = (|| {
        let mut gz = GzEncoder::new(sink, Compression::new(3));
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; BLOCK];
        let mut done = 0u64;
        ctx.set_progress(0, size);
        while done < size {
            ctx.check_cancelled()?;
            let want = BLOCK.min((size - done) as usize);
            let n = input.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            gz.write_all(&buf[..n])?;
            done += n as u64;
            ctx.set_progress(done, size);
        }
        let parts = gz.finish()?.finish()?;
        let mut lines = String::new();
        for p in &parts {
            let f = p
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            lines.push_str(&format!("{}  {f}\n", p.sha256));
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&sha_path)?;
        f.write_all(lines.as_bytes())?;
        f.sync_all()?;
        let raw: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok::<_, TaskFailure>((done, raw, parts))
    })();
    if result.is_err() {
        cleanup();
    }
    result
}

/// BAK-7: raw image of a partition of the boot disk to a stick.
pub fn export_image(
    backend: &dyn SystemBackend,
    role: PartRole,
    stick: &Stick,
    ctx: &TaskCtx,
) -> Result<ImageReport, TaskFailure> {
    let src = backend.read_test_path(Some(role))?;
    let mut f = File::open(&src).map_err(|e| err(format!("{}: {e}", src.display())))?;
    let size = f.seek(SeekFrom::End(0))?;
    f.seek(SeekFrom::Start(0))?;
    ctx.set_message("Mounting the stick");
    let g = backend.mount_stick(stick, true)?;
    let split = split_size(stick, g.path()).map_err(err)?;
    if let Ok(space) = crate::backup::fs_space(g.path()) {
        if space.available < size / 2 {
            log::warn!(
                "BAK-7: only {} free on {} for a {} partition",
                format_bytes(space.available),
                stick.id,
                format_bytes(size)
            );
        }
    }
    let id = device_identity(backend);
    let host = id
        .rescue_hostname
        .clone()
        .or(id.machine.clone())
        .unwrap_or_else(|| "xoros".into());
    let name = image_name(role, &host, chrono::Utc::now());
    log::info!(
        "action: raw image of {} ({}) to {}:{name}",
        role.label(),
        src.display(),
        stick.id
    );
    ctx.set_message(format!("Reading {}", role.label()));
    let (raw_bytes, raw_sha256, parts) = write_image(&mut f, size, g.path(), &name, split, ctx)
        .map_err(|e| {
            log::error!("action: raw image of {}: {e:?}", role.label());
            e
        })?;
    sync_all();
    g.unmount()?;
    log::info!("action: raw image {name}: {raw_bytes} bytes, sha256 {raw_sha256}");
    Ok(ImageReport {
        name,
        source: src.display().to_string(),
        raw_bytes,
        raw_sha256,
        parts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::hw::testutil::TempDir;
    use crate::tasks::{lock, TaskKind, TaskRunner, TaskState};
    use std::sync::{Arc, Mutex};

    fn in_task<T: Send + 'static>(
        f: impl FnOnce(&TaskCtx) -> Result<T, TaskFailure> + Send + 'static,
    ) -> Result<T, String> {
        let runner = TaskRunner::new();
        let out = Arc::new(Mutex::new(None));
        let o = Arc::clone(&out);
        let h = runner
            .spawn(
                "t",
                TaskKind::Write,
                move |ctx| {
                    *lock(&o) = Some(f(ctx)?);
                    Ok(String::new())
                },
                None,
            )
            .expect("spawn");
        match h.wait().state {
            TaskState::Succeeded(_) => Ok(lock(&out).take().expect("value")),
            TaskState::Failed(e) => Err(e),
            other => Err(format!("{other:?}")),
        }
    }

    fn sim() -> (TempDir, Arc<SimBackend>) {
        let d = TempDir::new("partjobs");
        SimBackend::create_example(d.path()).expect("example");
        let b = Arc::new(SimBackend::open(d.path()).expect("open"));
        (d, b)
    }

    #[test]
    fn repair_and_recreate_in_simulation() {
        let (d, b) = sim();
        let b2 = Arc::clone(&b);
        let r = in_task(move |ctx| repair(b2.as_ref(), PartRole::Application, ctx)).unwrap();
        assert!(r.mountable(), "{r:?}");
        assert_eq!(r.fsck, "clean");
        let b2 = Arc::clone(&b);
        assert!(in_task(move |ctx| repair(b2.as_ref(), PartRole::RootfsA, ctx)).is_err());
        let b2 = Arc::clone(&b);
        let e = in_task(move |ctx| recreate_fs(b2.as_ref(), PartRole::Overlay, "mkfs", ctx));
        assert!(e.unwrap_err().contains(MKFS_CONFIRM_WORD));
        let b2 = Arc::clone(&b);
        let t =
            in_task(move |ctx| recreate_fs(b2.as_ref(), PartRole::Overlay, MKFS_CONFIRM_WORD, ctx))
                .unwrap();
        assert!(t.contains("label 'overlay'"), "{t}");
        assert!(d
            .path()
            .join("parts/overlay")
            .join(OVERLAY_WORK_DIR)
            .is_dir());
        let t = in_task({
            let b = Arc::clone(&b);
            move |ctx| recreate_fs(b.as_ref(), PartRole::Application, MKFS_CONFIRM_WORD, ctx)
        })
        .unwrap();
        assert!(t.contains(crate::constants::FSUUID_APPLICATION), "{t}");
    }

    #[test]
    fn mkfs_failure_is_reported() {
        let (d, b) = sim();
        let mut s = b.load().expect("load");
        s.fail.insert("mke2fs".into(), "device busy".into());
        s.fail.insert("e2fsck".into(), "bad superblock".into());
        s.fail
            .insert("mount_partition.overlay".into(), "wrong fs type".into());
        d.write(
            "sim.json",
            serde_json::to_string(&s).expect("json").as_bytes(),
        );
        let b2 = Arc::clone(&b);
        let r = in_task(move |ctx| repair(b2.as_ref(), PartRole::Overlay, ctx)).unwrap();
        assert!(!r.mountable());
        assert!(r.fsck.contains("exit code 4"), "{}", r.fsck);
        assert!(r.text().contains("wrong fs type"));
        let b2 = Arc::clone(&b);
        let e =
            in_task(move |ctx| recreate_fs(b2.as_ref(), PartRole::Overlay, MKFS_CONFIRM_WORD, ctx))
                .unwrap_err();
        assert!(e.contains("device busy"), "{e}");
    }

    #[test]
    fn raw_image_round_trip_and_split() {
        let (d, b) = sim();
        let data: Vec<u8> = (0..3 * BLOCK + 123).map(|i| (i * 7 % 251) as u8).collect();
        d.write("overlay.img", &data);
        let stick = b.external_sticks().expect("sticks").remove(0);
        let st = stick.clone();
        let b2 = Arc::clone(&b);
        let r = in_task(move |ctx| export_image(b2.as_ref(), PartRole::Overlay, &st, ctx)).unwrap();
        assert_eq!(r.raw_bytes, data.len() as u64);
        assert_eq!(r.parts.len(), 1);
        assert!(r.name.starts_with("xoros-image-overlay-") && r.name.ends_with(".img.gz"));
        let gz = std::fs::read(&r.parts[0].path).unwrap();
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, data);
        let sums =
            std::fs::read_to_string(r.parts[0].path.with_file_name(format!("{}.sha256", r.name)))
                .unwrap();
        assert!(sums.starts_with(&r.parts[0].sha256));
        assert!(r.text(&stick).contains(&r.raw_sha256));

        // split into small parts, and cleanup on cancel
        let out = TempDir::new("partjobs-split");
        let dir = out.path().to_path_buf();
        let data2 = data.clone();
        let (n, _, parts) = in_task(move |ctx| {
            write_image(
                &mut &data2[..],
                data2.len() as u64,
                &dir,
                "x.img.gz",
                Some(1000),
                ctx,
            )
        })
        .unwrap();
        assert_eq!(n, data.len() as u64);
        assert!(parts.len() > 1);
        let dir = out.path().to_path_buf();
        let e = in_task(move |ctx| {
            ctx.cancel_token().cancel();
            write_image(
                &mut &data[..],
                data.len() as u64,
                &dir,
                "y.img.gz",
                None,
                ctx,
            )
        });
        assert!(e.is_err());
        assert!(!out.path().join("y.img.gz").exists());
        assert!(!out.path().join("y.img.gz.sha256").exists());
    }

    #[test]
    fn names() {
        let t = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 10, 28, 1, 2, 3).unwrap();
        assert_eq!(
            image_name(PartRole::Application, "xoros rescue/x", t),
            "xoros-image-application-xoros_rescue_x-20261028-010203.img.gz"
        );
    }
}
