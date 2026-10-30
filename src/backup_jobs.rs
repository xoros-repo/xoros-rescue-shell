//! Backup, restore and factory reset jobs on top of [`crate::backup`] and the backend
//! (BAK-1..5, RST-1..3). No ratatui (ARCH-1): the UI and the CLI call these functions inside a
//! [`TaskRunner`](crate::tasks::TaskRunner) worker.
//!
//! The backup module uses `Rc` internally, so every job function here runs one backup call
//! entirely inside the calling (worker) thread. Mount guards are dropped (unmounted) before the
//! function returns.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::backend::{BackendError, PartRole, Slot, Stick, SystemBackend};
use crate::backup::tools::{e2fsck_preen, sync_all, FsckOutcome, ToolError};
use crate::backup::{
    self, BackupError, BackupInfo, BackupMetadata, ExportOptions, ExportReport, ImportMode,
    ImportOptions, ImportReport, PartitionRole, Phase, Progress, ResetReport, SlotMeta, SpaceCheck,
    FAT_MAX_PART_SIZE, OVERLAY_UPPER_DIR,
};
use crate::tasks::{format_bytes, TaskCtx, TaskFailure};

impl From<BackupError> for TaskFailure {
    fn from(e: BackupError) -> Self {
        match e {
            BackupError::Cancelled => TaskFailure::Cancelled,
            other => TaskFailure::Error(other.to_string()),
        }
    }
}

impl From<PartitionRole> for PartRole {
    fn from(r: PartitionRole) -> Self {
        match r {
            PartitionRole::Overlay => PartRole::Overlay,
            PartitionRole::Application => PartRole::Application,
        }
    }
}

/// Short text of a progress phase.
pub fn phase_text(p: Phase) -> &'static str {
    match p {
        Phase::Scanning => "Scanning",
        Phase::Exporting => "Writing archive",
        Phase::Verifying => "Verifying checksums",
        Phase::Clearing => "Deleting old content",
        Phase::Importing => "Extracting",
        Phase::Syncing => "Syncing",
    }
}

/// Forwards backup progress to the task (progress bar + message line, UI-3).
pub fn task_progress(ctx: &TaskCtx) -> impl FnMut(&Progress) + '_ {
    move |p: &Progress| {
        ctx.set_progress(p.done_bytes, p.total_bytes);
        let entries = if p.entries > 0 {
            format!(" ({} entries)", p.entries)
        } else {
            String::new()
        };
        let current = if p.current.is_empty() {
            String::new()
        } else {
            format!(": {}", p.current)
        };
        ctx.set_message(format!("{}{entries}{current}", phase_text(p.phase)));
    }
}

fn backend_err(e: BackendError) -> TaskFailure {
    e.into()
}

/// What the backup metadata needs to know about the device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// `MACHINE` from a valid slot, or from the rescue host name `xoros-rescue-<machine>`.
    pub machine: Option<String>,
    /// Build info per slot (`ROOTFS_A`, `ROOTFS_B`), `None` for an empty/unreadable slot.
    pub slots: BTreeMap<String, Option<SlotMeta>>,
    /// Rescue system host name.
    pub rescue_hostname: Option<String>,
}

pub fn device_identity(backend: &dyn SystemBackend) -> DeviceIdentity {
    let mut id = DeviceIdentity::default();
    for slot in Slot::ALL {
        let info = backend.slot_info(slot).ok().flatten();
        if id.machine.is_none() {
            id.machine = info.as_ref().map(|i| i.machine.clone());
        }
        id.slots.insert(
            slot.name().to_string(),
            info.map(|i| SlotMeta {
                version: i.version,
                suffix: i.suffix,
                timestamp: i.timestamp,
            }),
        );
    }
    id.rescue_hostname = backend.rescue_system().ok().and_then(|r| r.hostname);
    if id.machine.is_none() {
        id.machine = id
            .rescue_hostname
            .as_deref()
            .and_then(|h| h.strip_prefix("xoros-rescue-"))
            .map(str::to_string);
    }
    id
}

/// Host name for the archive name: the normal system's `/etc/hostname` from the overlay upper
/// dir if available, else the machine, else `xoros`.
fn archive_hostname(overlay_root: Option<&Path>, id: &DeviceIdentity) -> String {
    overlay_root
        .and_then(|r| std::fs::read_to_string(r.join(OVERLAY_UPPER_DIR).join("hostname")).ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| id.machine.clone())
        .unwrap_or_else(|| "xoros".to_string())
}

/// Split size for a stick: FAT needs parts of at most 4 GiB - 1 (BAK-3). The backend's file
/// system type wins; otherwise `statfs` of the mounted directory decides.
pub fn split_size(stick: &Stick, dir: &Path) -> Result<Option<u64>, BackupError> {
    if stick.is_fat() {
        return Ok(Some(FAT_MAX_PART_SIZE));
    }
    backup::split_size_for(dir)
}

// --------------------------------------------------------------------------------------------
// export (BAK-1..3)

#[derive(Clone, Debug)]
pub struct ExportRequest {
    pub roles: Vec<PartitionRole>,
    pub stick: Stick,
}

/// Result of the space check before an export (BAK-3).
#[derive(Clone, Debug)]
pub struct ExportPreview {
    pub space: SpaceCheck,
    pub split: Option<u64>,
}

impl ExportPreview {
    pub fn text(&self) -> String {
        let mut t = format!(
            "Used on the selected partitions: {}\nFree on the stick: {}\n",
            format_bytes(self.space.needed),
            format_bytes(self.space.available)
        );
        t.push_str(match self.split {
            Some(_) => "The stick is FAT: the archive is split into parts of at most 4 GiB.\n",
            None => "The archive is written as one file.\n",
        });
        if self.space.may_not_fit() {
            t.push_str(
                "\nWARNING: the backup may not fit on the stick. The archive is compressed, \
                 so it can still fit; you can continue.\n",
            );
        }
        t
    }
}

fn mount_sources(
    backend: &dyn SystemBackend,
    roles: &[PartitionRole],
) -> Result<Vec<(PartitionRole, crate::backend::MountGuard)>, BackendError> {
    roles
        .iter()
        .map(|r| backend.mount_partition((*r).into(), false).map(|g| (*r, g)))
        .collect()
}

/// Mount everything read-only and compare used and free space (BAK-3).
pub fn export_preview(
    backend: &dyn SystemBackend,
    req: &ExportRequest,
) -> Result<ExportPreview, String> {
    let sources = mount_sources(backend, &req.roles).map_err(|e| e.to_string())?;
    let stick = backend
        .mount_stick(&req.stick, false)
        .map_err(|e| e.to_string())?;
    let refs: Vec<(PartitionRole, &Path)> = sources.iter().map(|(r, g)| (*r, g.path())).collect();
    let mut space = backup::check_space(&refs, stick.path()).map_err(|e| e.to_string())?;
    // The backend's view of the used space wins when it has one (statvfs of the mounted
    // partition on a device; the configured values in simulation, where the partitions are
    // directories on the host file system).
    let used: Option<u64> = req
        .roles
        .iter()
        .map(|r| {
            backend
                .partition_usage((*r).into())
                .ok()
                .flatten()
                .map(|u| u.used)
        })
        .sum();
    if let Some(u) = used {
        space.needed = u;
    }
    let split = split_size(&req.stick, stick.path()).map_err(|e| e.to_string())?;
    Ok(ExportPreview { space, split })
}

/// Export the selected partitions to the stick root. Runs in a worker thread.
pub fn run_export(
    backend: &dyn SystemBackend,
    req: &ExportRequest,
    ctx: &TaskCtx,
) -> Result<ExportReport, TaskFailure> {
    ctx.set_message("Mounting");
    let id = device_identity(backend);
    let sources = mount_sources(backend, &req.roles).map_err(backend_err)?;
    let stick = backend.mount_stick(&req.stick, true).map_err(backend_err)?;
    let refs: Vec<(PartitionRole, &Path)> = sources.iter().map(|(r, g)| (*r, g.path())).collect();
    let overlay = refs
        .iter()
        .find(|(r, _)| *r == PartitionRole::Overlay)
        .map(|(_, p)| *p);
    let mut opts = ExportOptions::new(
        &archive_hostname(overlay, &id),
        id.machine.as_deref().unwrap_or("unknown"),
    );
    opts.slots = id.slots.clone();
    opts.app_version = crate::constants::APP_VERSION.to_string();
    opts.split_size = split_size(&req.stick, stick.path())?;
    let cancel = ctx.cancel_token().clone();
    let report = backup::export(&refs, stick.path(), &opts, &mut task_progress(ctx), &cancel)?;
    ctx.set_message("Syncing the stick");
    sync_all();
    drop(sources);
    stick.unmount().map_err(backend_err)?;
    log::info!(
        "backup exported: {} ({} part(s), {})",
        report.archive_name,
        report.parts.len(),
        format_bytes(report.compressed_bytes())
    );
    Ok(report)
}

pub fn export_summary(r: &ExportReport, stick: &Stick) -> String {
    let mut t = format!(
        "Backup written to stick {}:\n\n  {}\n  {} part(s), {} compressed\n  checksums: {}\n\n",
        stick.id,
        r.archive_name,
        r.parts.len(),
        format_bytes(r.compressed_bytes()),
        r.checksum_file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    for (name, s) in &r.metadata.partitions {
        t.push_str(&format!(
            "  {name}: {} files, {}\n",
            s.files,
            format_bytes(s.bytes)
        ));
    }
    if !r.warnings.is_empty() {
        t.push_str(&format!("\n{} warning(s):\n", r.warnings.len()));
        for w in &r.warnings {
            t.push_str(&format!("  {w}\n"));
        }
    }
    t.push_str("\nThe stick can be removed.");
    t
}

// --------------------------------------------------------------------------------------------
// listing and import (BAK-4, BAK-4a, BAK-5)

/// A backup on a stick. `info`'s paths are only valid while the stick is mounted; jobs look the
/// backup up again by `dir_rel` + `info.name`.
#[derive(Clone, Debug)]
pub struct FoundBackup {
    pub info: BackupInfo,
    /// Directory relative to the stick root (`""` or `xoros/backups`).
    pub dir_rel: PathBuf,
}

/// Backups on a stick (root and `xoros/backups/`), newest first.
pub fn list_stick_backups(
    backend: &dyn SystemBackend,
    stick: &Stick,
) -> Result<Vec<FoundBackup>, String> {
    let g = backend
        .mount_stick(stick, false)
        .map_err(|e| e.to_string())?;
    let list = backup::list_backups(g.path())
        .into_iter()
        .map(|info| FoundBackup {
            dir_rel: info
                .dir
                .strip_prefix(g.path())
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            info,
        })
        .collect();
    g.unmount().map_err(|e| e.to_string())?;
    Ok(list)
}

/// Metadata screen text (BAK-4), including the machine mismatch warning.
pub fn describe_backup(b: &FoundBackup, device_machine: Option<&str>) -> String {
    let i = &b.info;
    let loc = if b.dir_rel.as_os_str().is_empty() {
        "stick root".to_string()
    } else {
        b.dir_rel.display().to_string()
    };
    let mut t = format!(
        "{}\nLocation: {loc}\nSize: {} in {} file(s){}\n",
        i.name,
        format_bytes(i.total_size),
        i.parts.len(),
        if i.checksum_file.is_some() {
            ""
        } else {
            "\nChecksum file: MISSING (the backup cannot be imported)"
        }
    );
    match &i.metadata {
        Err(e) => t.push_str(&format!("\nCannot be imported: {e}\n")),
        Ok(m) => {
            t.push_str(&format!(
                "Created: {}\nMachine: {}\nRescue shell: {}\n",
                m.created, m.machine, m.app_version
            ));
            if let Some(dm) = device_machine {
                if m.machine_mismatch(dm) {
                    t.push_str(&format!(
                        "\nWARNING: this backup was made on '{}', this device is '{dm}'.\n",
                        m.machine
                    ));
                }
            }
            for (slot, s) in &m.slots {
                match s {
                    Some(s) => t.push_str(&format!(
                        "{slot}: {} {} ({})\n",
                        s.version, s.suffix, s.timestamp
                    )),
                    None => t.push_str(&format!("{slot}: empty\n")),
                }
            }
            for (name, s) in &m.partitions {
                t.push_str(&format!(
                    "Partition {name}: {} files, {}\n",
                    s.files,
                    format_bytes(s.bytes)
                ));
            }
        }
    }
    t
}

#[derive(Clone, Debug)]
pub struct ImportRequest {
    pub stick: Stick,
    pub dir_rel: PathBuf,
    pub name: String,
    pub roles: Vec<PartitionRole>,
    pub mode: ImportMode,
}

#[derive(Clone, Debug)]
pub struct ImportOutcome {
    pub metadata: BackupMetadata,
    pub report: ImportReport,
    /// e2fsck results and other notes per partition (BAK-5).
    pub notes: Vec<String>,
}

/// Why an import was refused before anything on the target changed (BAK-4a).
fn refuse(reason: impl std::fmt::Display) -> TaskFailure {
    let msg = format!("The backup cannot be imported: {reason}");
    log::error!("{msg}");
    TaskFailure::Error(msg)
}

/// Look the backup up on the mounted stick, check its metadata and verify every part against
/// the `.sha256` file (BAK-4a). Nothing on the device is touched.
fn checked_backup(
    stick_root: &Path,
    req: &ImportRequest,
    ctx: &TaskCtx,
) -> Result<(BackupInfo, BackupMetadata), TaskFailure> {
    let dir = stick_root.join(&req.dir_rel);
    let info = backup::backup_info(&dir, &req.name).map_err(refuse)?;
    let cancel = ctx.cancel_token().clone();
    let verify = |ctx: &TaskCtx| match backup::verify(&info, &mut task_progress(ctx), &cancel) {
        Ok(()) => Ok(()),
        Err(BackupError::Cancelled) => Err(TaskFailure::Cancelled),
        Err(e) => Err(refuse(e)),
    };
    let metadata = match info.metadata.clone() {
        Ok(m) => m,
        Err(e) => {
            // A damaged part also breaks the metadata; the checksum result names the file.
            verify(ctx)?;
            return Err(refuse(e));
        }
    };
    for r in &req.roles {
        if !metadata.partitions.contains_key(r.as_str()) {
            return Err(refuse(format!("it does not contain the {r} partition")));
        }
    }
    verify(ctx)?;
    Ok((info, metadata))
}

/// Verify a backup only (no import).
pub fn run_verify(
    backend: &dyn SystemBackend,
    req: &ImportRequest,
    ctx: &TaskCtx,
) -> Result<String, TaskFailure> {
    let stick = backend
        .mount_stick(&req.stick, false)
        .map_err(backend_err)?;
    let (info, _) = checked_backup(stick.path(), req, ctx)?;
    stick.unmount().map_err(backend_err)?;
    Ok(format!(
        "{}: all {} part(s) match the checksum file.",
        info.name,
        info.parts.len()
    ))
}

/// `e2fsck -p` on the (unmounted) target partition (BAK-5).
fn fsck_target(
    backend: &dyn SystemBackend,
    role: PartRole,
    notes: &mut Vec<String>,
) -> Result<(), TaskFailure> {
    let layout = backend.layout().map_err(backend_err)?;
    let device = layout
        .get(role)
        .map(|e| e.device.clone())
        .ok_or_else(|| TaskFailure::Error(format!("No {} partition.", role.label())))?;
    let runner = backend.command_runner();
    match e2fsck_preen(runner.as_ref(), &device) {
        Ok(FsckOutcome::Clean) => notes.push(format!("{}: e2fsck clean", role.label())),
        Ok(FsckOutcome::Corrected) => {
            notes.push(format!("{}: e2fsck corrected errors", role.label()))
        }
        Ok(FsckOutcome::Uncorrected { code, output }) => {
            return Err(TaskFailure::Error(format!(
                "e2fsck -p {device} left errors uncorrected (exit code {code}): {}\n\
                 Run a file system check before importing. Nothing was imported.",
                output.stderr.trim()
            )))
        }
        Err(e @ ToolError::Missing(_)) => {
            log::warn!("{e}");
            notes.push(format!("{}: {e}, check skipped", role.label()));
        }
        Err(e) => return Err(TaskFailure::Error(e.to_string())),
    }
    Ok(())
}

/// Import a backup (BAK-4, BAK-4a, BAK-5): mount the stick read-only, verify (refuse on any
/// problem before touching the device), `e2fsck -p` the targets, mount them read-write,
/// extract, `sync`. Runs in a worker thread.
pub fn run_import(
    backend: &dyn SystemBackend,
    req: &ImportRequest,
    ctx: &TaskCtx,
) -> Result<ImportOutcome, TaskFailure> {
    if req.roles.is_empty() {
        return Err(TaskFailure::Error("No partition selected.".into()));
    }
    ctx.set_message("Mounting the stick");
    let stick = backend
        .mount_stick(&req.stick, false)
        .map_err(backend_err)?;
    let (info, metadata) = checked_backup(stick.path(), req, ctx)?;
    ctx.check_cancelled()?;

    let mut notes = Vec::new();
    for r in &req.roles {
        ctx.set_message(format!("e2fsck -p {}", r.as_str()));
        fsck_target(backend, (*r).into(), &mut notes)?;
    }
    ctx.check_cancelled()?;
    let targets: Vec<(PartitionRole, crate::backend::MountGuard)> = req
        .roles
        .iter()
        .map(|r| backend.mount_partition((*r).into(), true).map(|g| (*r, g)))
        .collect::<Result<_, _>>()
        .map_err(backend_err)?;
    let refs: Vec<(PartitionRole, &Path)> = targets.iter().map(|(r, g)| (*r, g.path())).collect();
    // The archive was verified above in this task, from the same mount (BAK-4a), so it is not
    // hashed a second time here. The UI never offers importing without verification.
    let opts = ImportOptions {
        mode: req.mode,
        verify: false,
    };
    let cancel = ctx.cancel_token().clone();
    let report = backup::import(&info, &refs, &opts, &mut task_progress(ctx), &cancel)?;
    ctx.set_message("Syncing");
    sync_all();
    for (r, g) in targets {
        if let Err(e) = g.unmount() {
            notes.push(format!("{}: unmount failed: {e}", r.as_str()));
        }
    }
    drop(stick);
    log::info!(
        "backup imported: {} ({:?}, {} error(s))",
        req.name,
        req.mode,
        report.error_count
    );
    for e in &report.errors {
        log::warn!("import: {}: {}", e.path, e.error);
    }
    Ok(ImportOutcome {
        metadata,
        report,
        notes,
    })
}

/// Summary after an import (BAK-5): per partition counts and the collected per-file errors.
pub fn import_summary(name: &str, o: &ImportOutcome) -> String {
    let mut t = format!("Imported {name}\n\n");
    for (role, p) in &o.report.partitions {
        t.push_str(&format!(
            "  {role}: {} entries, {} written{}\n",
            p.entries,
            format_bytes(p.bytes),
            if p.removed > 0 {
                format!(", {} old entries removed", p.removed)
            } else {
                String::new()
            }
        ));
    }
    for n in &o.notes {
        t.push_str(&format!("  {n}\n"));
    }
    if o.report.error_count == 0 {
        t.push_str("\nNo errors.");
    } else {
        t.push_str(&format!(
            "\n{} file(s) could not be restored:\n",
            o.report.error_count
        ));
        for e in &o.report.errors {
            t.push_str(&format!("  {}: {}\n", e.path, e.error));
        }
        if o.report.error_count > o.report.errors.len() as u64 {
            t.push_str(&format!(
                "  ... and {} more (see the log)\n",
                o.report.error_count - o.report.errors.len() as u64
            ));
        }
    }
    t
}

// --------------------------------------------------------------------------------------------
// factory reset (RST-1..3)

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetRequest {
    /// RST-2: also delete all data on `application`.
    pub full: bool,
    /// Also delete the rescue shell's own `rescue/` directory on the overlay (RST-1).
    pub reset_rescue_settings: bool,
    /// What the user typed; must be [`backup::RESET_CONFIRM_WORD`] (RST-3).
    pub confirmation: String,
}

/// Factory reset (RST-1/RST-2). Runs in a worker thread.
pub fn run_reset(
    backend: &dyn SystemBackend,
    req: &ResetRequest,
    ctx: &TaskCtx,
) -> Result<ResetReport, TaskFailure> {
    if !backup::reset_confirmed(&req.confirmation) {
        return Err(TaskFailure::Error(format!(
            "Not confirmed: type {} to reset.",
            backup::RESET_CONFIRM_WORD
        )));
    }
    ctx.set_message("Mounting");
    let overlay = backend
        .mount_partition(PartRole::Overlay, true)
        .map_err(backend_err)?;
    let application = if req.full {
        Some(
            backend
                .mount_partition(PartRole::Application, true)
                .map_err(backend_err)?,
        )
    } else {
        None
    };
    let cancel = ctx.cancel_token().clone();
    let report = backup::factory_reset(
        overlay.path(),
        application.as_ref().map(|g| g.path()),
        !req.reset_rescue_settings,
        &mut task_progress(ctx),
        &cancel,
    )?;
    sync_all();
    overlay.unmount().map_err(backend_err)?;
    if let Some(a) = application {
        a.unmount().map_err(backend_err)?;
    }
    log::info!(
        "factory reset ({}): overlay {} entries, application {} entries removed, {} error(s)",
        if req.full { "full" } else { "configuration" },
        report.overlay_removed,
        report.application_removed,
        report.errors.len()
    );
    Ok(report)
}

pub fn reset_summary(req: &ResetRequest, r: &ResetReport) -> String {
    let mut t = format!(
        "{} done.\n\n  overlay: {} entries removed{}\n",
        if req.full {
            "Full factory reset"
        } else {
            "Configuration reset"
        },
        r.overlay_removed,
        if req.reset_rescue_settings {
            " (rescue settings too)"
        } else {
            " (rescue settings kept)"
        }
    );
    if req.full {
        t.push_str(&format!(
            "  application: {} entries removed\n",
            r.application_removed
        ));
    }
    if r.errors.is_empty() {
        t.push_str("\nNo errors. The normal system starts with the /etc of its slot.");
    } else {
        t.push_str(&format!(
            "\n{} entries could not be deleted:\n",
            r.errors.len()
        ));
        for e in &r.errors {
            t.push_str(&format!("  {}: {}\n", e.path, e.error));
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::hw::testutil::TempDir;
    use crate::tasks::{TaskKind, TaskRunner, TaskState};
    use std::fs;
    use std::sync::{Arc, Mutex};

    fn sim() -> (TempDir, Arc<SimBackend>) {
        let d = TempDir::new("bjobs");
        SimBackend::create_example(d.path()).expect("example");
        let b = SimBackend::open(d.path()).expect("open");
        (d, Arc::new(b))
    }

    /// Run `f` as a task and return its value or failure text.
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
                    let r = f(ctx)?;
                    *crate::tasks::lock(&o) = Some(r);
                    Ok(String::new())
                },
                None,
            )
            .expect("spawn");
        match h.wait().state {
            TaskState::Succeeded(_) => Ok(crate::tasks::lock(&out).take().expect("value")),
            TaskState::Failed(e) => Err(e),
            other => Err(format!("{other:?}")),
        }
    }

    fn stick(b: &SimBackend) -> Stick {
        let s = b.external_sticks().expect("sticks");
        assert_eq!(s.len(), 1);
        s.into_iter().next().expect("stick")
    }

    #[test]
    fn export_import_reset_in_simulation() {
        let (d, b) = sim();
        let st = stick(&b);
        assert!(st.is_fat());
        let roles = vec![PartitionRole::Overlay, PartitionRole::Application];
        let req = ExportRequest {
            roles: roles.clone(),
            stick: st.clone(),
        };
        let preview = export_preview(b.as_ref(), &req).expect("preview");
        assert_eq!(preview.split, Some(FAT_MAX_PART_SIZE));
        assert!(preview.text().contains("split"));

        let bb = Arc::clone(&b);
        let r2 = req.clone();
        let rep = in_task(move |ctx| run_export(bb.as_ref(), &r2, ctx)).expect("export");
        assert!(rep.archive_name.starts_with("xoros-backup-xoros-device-"));
        assert_eq!(rep.metadata.machine, "qemux86-64");
        assert!(export_summary(&rep, &st).contains(&rep.archive_name));

        let found = list_stick_backups(b.as_ref(), &st).expect("list");
        assert_eq!(found.len(), 1);
        let text = describe_backup(&found[0], Some("raspberrypi4-64"));
        assert!(text.contains("WARNING: this backup was made on 'qemux86-64'"));
        assert!(!describe_backup(&found[0], Some("qemux86-64")).contains("WARNING"));

        // change the device, then restore
        let parts = d.path().join("parts");
        fs::write(
            parts.join("overlay/overlays/etc/upper/hostname"),
            "changed\n",
        )
        .expect("w");
        fs::write(
            parts.join("overlay/rescue/rescue-shell.toml"),
            "theme = \"x\"\n",
        )
        .expect("w");
        fs::write(parts.join("application/new-file"), "x").expect("w");
        let ireq = ImportRequest {
            stick: st.clone(),
            dir_rel: found[0].dir_rel.clone(),
            name: found[0].info.name.clone(),
            roles: roles.clone(),
            mode: ImportMode::Replace,
        };
        let bb = Arc::clone(&b);
        let r2 = ireq.clone();
        let out = in_task(move |ctx| run_import(bb.as_ref(), &r2, ctx)).expect("import");
        assert_eq!(out.report.error_count, 0, "{:?}", out.report.errors);
        assert!(out.notes.iter().any(|n| n.contains("e2fsck clean")));
        assert_eq!(
            fs::read_to_string(parts.join("overlay/overlays/etc/upper/hostname")).expect("r"),
            "xoros-device\n"
        );
        // current rescue settings kept
        assert_eq!(
            fs::read_to_string(parts.join("overlay/rescue/rescue-shell.toml")).expect("r"),
            "theme = \"x\"\n"
        );
        assert!(!parts.join("application/new-file").exists());
        assert!(import_summary(&ireq.name, &out).contains("No errors."));

        // RST-3: wrong word refused, nothing deleted
        let bb = Arc::clone(&b);
        let bad = ResetRequest {
            full: true,
            reset_rescue_settings: false,
            confirmation: "reset".into(),
        };
        assert!(in_task(move |ctx| run_reset(bb.as_ref(), &bad, ctx)).is_err());
        assert!(parts.join("application/app/config.json").exists());
        // RST-1 keeps application and rescue settings
        let bb = Arc::clone(&b);
        let req1 = ResetRequest {
            full: false,
            reset_rescue_settings: false,
            confirmation: "RESET".into(),
        };
        let r = in_task(move |ctx| run_reset(bb.as_ref(), &req1, ctx)).expect("reset");
        assert!(r.overlay_removed > 0);
        assert!(!parts.join("overlay/overlays/etc/upper/hostname").exists());
        assert!(parts.join("overlay/overlays/etc/work").is_dir());
        assert!(parts.join("overlay/rescue/rescue-shell.toml").exists());
        assert!(parts.join("application/app/config.json").exists());
        // RST-2
        let bb = Arc::clone(&b);
        let req2 = ResetRequest {
            full: true,
            reset_rescue_settings: true,
            confirmation: " RESET ".into(),
        };
        let r = in_task(move |ctx| run_reset(bb.as_ref(), &req2, ctx)).expect("reset");
        assert!(r.application_removed > 0);
        assert!(!parts.join("application/app").exists());
        assert!(!parts.join("overlay/rescue").exists());
    }

    #[test]
    fn import_refused_before_touching_the_device() {
        let (d, b) = sim();
        let st = stick(&b);
        let req = ExportRequest {
            roles: vec![PartitionRole::Overlay],
            stick: st.clone(),
        };
        let bb = Arc::clone(&b);
        let rep = in_task(move |ctx| run_export(bb.as_ref(), &req, ctx)).expect("export");
        let ireq = ImportRequest {
            stick: st.clone(),
            dir_rel: PathBuf::new(),
            name: rep.archive_name.clone(),
            roles: vec![PartitionRole::Overlay],
            mode: ImportMode::Replace,
        };
        let marker = d.path().join("parts/overlay/marker");
        fs::write(&marker, "m").expect("w");

        // damaged part
        let part = d.path().join("sticks/usb1").join(&rep.archive_name);
        let mut data = fs::read(&part).expect("r");
        let n = data.len() / 2;
        data[n] ^= 0x55;
        fs::write(&part, &data).expect("w");
        let bb = Arc::clone(&b);
        let r2 = ireq.clone();
        let e = in_task(move |ctx| run_import(bb.as_ref(), &r2, ctx)).expect_err("refused");
        assert!(
            e.starts_with(&format!(
                "The backup cannot be imported: Part `{}`: expected SHA-256",
                rep.archive_name
            )),
            "{e}"
        );
        assert!(marker.exists());
        let bb = Arc::clone(&b);
        let r2 = ireq.clone();
        assert!(in_task(move |ctx| run_verify(bb.as_ref(), &r2, ctx)).is_err());
        data[n] ^= 0x55;
        fs::write(&part, &data).expect("w");

        // partition not in the backup
        let bb = Arc::clone(&b);
        let mut r2 = ireq.clone();
        r2.roles = vec![PartitionRole::Application];
        let e = in_task(move |ctx| run_import(bb.as_ref(), &r2, ctx)).expect_err("refused");
        assert!(e.contains("does not contain the application"), "{e}");

        // missing checksum file
        fs::remove_file(&rep.checksum_file).expect("rm");
        let bb = Arc::clone(&b);
        let r2 = ireq.clone();
        let e = in_task(move |ctx| run_import(bb.as_ref(), &r2, ctx)).expect_err("refused");
        assert_eq!(
            e,
            format!(
                "The backup cannot be imported: Checksum file `{}.sha256` not found next to the backup",
                rep.archive_name
            )
        );
        assert!(marker.exists());
    }

    #[test]
    fn fsck_failure_aborts_import() {
        let (_d, b) = sim();
        let st = stick(&b);
        let req = ExportRequest {
            roles: vec![PartitionRole::Overlay],
            stick: st.clone(),
        };
        let bb = Arc::clone(&b);
        let rep = in_task(move |ctx| run_export(bb.as_ref(), &req, ctx)).expect("export");
        let mut s = b.load().expect("load");
        s.fail.insert("e2fsck".into(), "bad superblock".into());
        fs::write(
            b.dir().join("sim.json"),
            serde_json::to_string(&s).expect("json"),
        )
        .expect("w");
        let ireq = ImportRequest {
            stick: st,
            dir_rel: PathBuf::new(),
            name: rep.archive_name,
            roles: vec![PartitionRole::Overlay],
            mode: ImportMode::Merge,
        };
        let bb = Arc::clone(&b);
        let e = in_task(move |ctx| run_import(bb.as_ref(), &ireq, ctx)).expect_err("fsck");
        assert!(
            e.contains("bad superblock") && e.contains("Nothing was imported"),
            "{e}"
        );
    }
}
