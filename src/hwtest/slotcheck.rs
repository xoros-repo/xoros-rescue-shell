//! TST-4 slot integrity and TST-5 rescue integrity.
//!
//! TST-4, per slot: check the squashfs superblock (magic, version, block size, compression,
//! table offsets, `bytes_used` within the partition), mount the slot read-only and read every
//! file (I/O and decompression errors show up as read errors), and, if an OTA manifest for the
//! slot's build is found (stick or OTA server), compare the SHA-256 of the first `size` bytes
//! of the partition with the manifest.
//!
//! TST-5: SHA-256 of the rescue partition (`round_up(bytes_used, 4096)` bytes) against each
//! slot's `RESCUE_IMAGE_SHA256`; a mismatch with both slots is only a warning.

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use xoros_core::env::Suffix;
use xoros_core::manifest::{Image, Target};

use super::{worst, Outcome, TestReport, MAX_DETAIL_LINES};
use crate::backend::{BuildInfo, ErrorKind, PartRole, Slot, SystemBackend};
use crate::constants::{DEFAULT_HTTP_PROXY, DEFAULT_OTA_BASE_URL};
use crate::slots::RescueCheck;
use crate::tasks::{format_bytes, lock, TaskCtx, TaskFailure};
use crate::update_jobs::{self, Origin, StickItem};

/// Squashfs superblock size and fields (little endian).
const SB_SIZE: usize = 96;
const SQUASHFS_MAGIC: u32 = 0x7371_7368; // "hsqs"

/// What the superblock says about an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SquashfsInfo {
    pub bytes_used: u64,
    /// `round_up(bytes_used, 4096)`.
    pub image_len: u64,
    pub block_size: u32,
    pub compression: &'static str,
    pub inodes: u32,
}

fn le16(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn le32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn le64(d: &[u8], o: usize) -> u64 {
    u64::from(le32(d, o)) | u64::from(le32(d, o + 4)) << 32
}

fn compression_name(id: u16) -> Option<&'static str> {
    Some(match id {
        1 => "gzip",
        2 => "lzma",
        3 => "lzo",
        4 => "xz",
        5 => "lz4",
        6 => "zstd",
        _ => return None,
    })
}

/// Check a squashfs superblock against a partition of `part_size` bytes. `Err` lists the
/// problems; `Err` with an empty list means there is no squashfs at all (empty slot).
pub fn check_superblock(d: &[u8], part_size: u64) -> Result<SquashfsInfo, Vec<String>> {
    if d.len() < SB_SIZE || le32(d, 0) != SQUASHFS_MAGIC {
        return Err(Vec::new());
    }
    let mut problems = Vec::new();
    let bytes_used = le64(d, 40);
    let image_len = bytes_used.div_ceil(4096) * 4096;
    let block_size = le32(d, 12);
    let block_log = le16(d, 22);
    let (major, minor) = (le16(d, 28), le16(d, 30));
    if (major, minor) != (4, 0) {
        problems.push(format!(
            "unsupported squashfs version {major}.{minor} (expected 4.0)"
        ));
    }
    if !block_size.is_power_of_two()
        || !(4096..=1 << 20).contains(&block_size)
        || u32::from(block_log) != block_size.trailing_zeros()
    {
        problems.push(format!("bad block size {block_size} (log {block_log})"));
    }
    let compression = compression_name(le16(d, 20));
    if compression.is_none() {
        problems.push(format!("unknown compression id {}", le16(d, 20)));
    }
    let inodes = le32(d, 4);
    if inodes == 0 {
        problems.push("no inodes".into());
    }
    if bytes_used < SB_SIZE as u64 {
        problems.push(format!("bytes_used {bytes_used} is too small"));
    }
    if image_len > part_size {
        problems.push(format!(
            "bytes_used {bytes_used} is beyond the partition size {part_size}"
        ));
    }
    for (name, off) in [
        ("inode table", 64),
        ("directory table", 72),
        ("id table", 48),
    ] {
        let v = le64(d, off);
        if v >= bytes_used {
            problems.push(format!(
                "{name} offset {v} is beyond bytes_used {bytes_used}"
            ));
        }
    }
    if problems.is_empty() {
        Ok(SquashfsInfo {
            bytes_used,
            image_len,
            block_size,
            compression: compression.unwrap_or("?"),
            inodes,
        })
    } else {
        Err(problems)
    }
}

/// A plausible squashfs 4.0 superblock (xz, 128 KiB blocks) for simulation and test images.
pub fn example_superblock(bytes_used: u64) -> [u8; SB_SIZE] {
    let mut d = [0u8; SB_SIZE];
    d[..SB_SIZE].copy_from_slice(&xoros_core::squashfs::fake_superblock(bytes_used));
    let put32 =
        |d: &mut [u8; SB_SIZE], o: usize, v: u32| d[o..o + 4].copy_from_slice(&v.to_le_bytes());
    let put16 =
        |d: &mut [u8; SB_SIZE], o: usize, v: u16| d[o..o + 2].copy_from_slice(&v.to_le_bytes());
    let put64 =
        |d: &mut [u8; SB_SIZE], o: usize, v: u64| d[o..o + 8].copy_from_slice(&v.to_le_bytes());
    put32(&mut d, 4, 42); // inodes
    put32(&mut d, 12, 128 * 1024);
    put16(&mut d, 20, 4); // xz
    put16(&mut d, 22, 17);
    put16(&mut d, 28, 4);
    put16(&mut d, 30, 0);
    let t = bytes_used.saturating_sub(64).max(SB_SIZE as u64);
    for off in [48, 64, 72] {
        put64(&mut d, off, t);
    }
    for off in [56, 80, 88] {
        put64(&mut d, off, u64::MAX);
    }
    d
}

/// Where to look for the OTA manifest of a slot's build (TST-4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestMode {
    /// Do not compare.
    Off,
    /// OTA trees on attached sticks first, then the OTA server.
    Auto,
    /// This source only.
    Origin(Origin),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotTestOptions {
    pub slots: Vec<Slot>,
    /// Mount the slot and read every file.
    pub read_files: bool,
    pub manifest: ManifestMode,
}

impl Default for SlotTestOptions {
    fn default() -> Self {
        SlotTestOptions {
            slots: Slot::ALL.to_vec(),
            read_files: true,
            manifest: ManifestMode::Auto,
        }
    }
}

/// SHA-256 of the first `len` bytes of `r`; progress `(done, len)`.
fn hash_prefix(mut r: impl Read, len: u64, ctx: &TaskCtx) -> Result<String, TaskFailure> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    while done < len {
        ctx.check_cancelled()?;
        let want = ((len - done) as usize).min(buf.len());
        r.read_exact(&mut buf[..want])
            .map_err(|e| TaskFailure::Error(format!("read error at byte {done}: {e}")))?;
        h.update(&buf[..want]);
        done += want as u64;
        ctx.set_progress(done, len);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Result of reading every file of a mounted slot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileReadStats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: Vec<String>,
    pub error_count: u64,
}

impl FileReadStats {
    fn error(&mut self, msg: String) {
        self.error_count += 1;
        if self.errors.len() < MAX_DETAIL_LINES {
            self.errors.push(msg);
        }
    }
}

fn collect_files(dir: &Path, files: &mut Vec<(PathBuf, u64)>, st: &mut FileReadStats) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => return st.error(format!("{}: {e}", dir.display())),
    };
    st.dirs += 1;
    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                st.error(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        let path = entry.path();
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => collect_files(&path, files, st),
            Ok(m) if m.is_file() => files.push((path, m.len())),
            Ok(_) => {} // symlinks, devices: nothing to read
            Err(e) => st.error(format!("{}: {e}", path.display())),
        }
    }
}

/// Read every regular file below `root` (TST-4). `tick(done, total)` reports bytes and returns
/// `false` to cancel; `None` is returned when cancelled.
pub fn read_tree(root: &Path, tick: &mut dyn FnMut(u64, u64) -> bool) -> Option<FileReadStats> {
    let mut st = FileReadStats::default();
    let mut files = Vec::new();
    collect_files(root, &mut files, &mut st);
    let total: u64 = files.iter().map(|(_, s)| s).sum();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    for (path, size) in files {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        match std::fs::File::open(&path) {
            Ok(mut f) => loop {
                match f.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        done += n as u64;
                        st.bytes += n as u64;
                        if !tick(done, total) {
                            return None;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        st.error(format!("/{rel}: {e}"));
                        break;
                    }
                }
            },
            Err(e) => st.error(format!("/{rel}: {e}")),
        }
        st.files += 1;
        let _ = size;
        if !tick(done, total) {
            return None;
        }
    }
    Some(st)
}

/// The rootfs image of `info`'s build in a manifest from `origin`.
fn image_from(
    backend: &dyn SystemBackend,
    origin: &Origin,
    info: &BuildInfo,
    ctx: &TaskCtx,
) -> Result<Option<Image>, TaskFailure> {
    let c = update_jobs::check(backend, origin, None, ctx)?;
    Ok(c.rootfs
        .into_iter()
        .map(|r| r.image)
        .find(|i| i.build_timestamp.as_str() == info.timestamp))
}

/// Look for the manifest entry of the slot's build. Returns the source and the image, or a
/// note why nothing was compared.
fn find_manifest_image(
    backend: &dyn SystemBackend,
    info: &BuildInfo,
    mode: &ManifestMode,
    ctx: &TaskCtx,
) -> Result<Result<(String, Image), String>, TaskFailure> {
    let mut notes = Vec::new();
    let try_origin =
        |origin: Origin, notes: &mut Vec<String>| -> Result<Option<(String, Image)>, TaskFailure> {
            match image_from(backend, &origin, info, ctx) {
                Ok(Some(i)) => Ok(Some((origin.describe(), i))),
                Ok(None) => {
                    notes.push(format!(
                        "{}: build {} not listed",
                        origin.describe(),
                        info.timestamp
                    ));
                    Ok(None)
                }
                Err(TaskFailure::Cancelled) => Err(TaskFailure::Cancelled),
                Err(TaskFailure::Error(e)) => {
                    notes.push(format!("{}: {}", origin.describe(), first_line(&e)));
                    Ok(None)
                }
            }
        };
    match mode {
        ManifestMode::Off => return Ok(Err("not requested".into())),
        ManifestMode::Origin(o) => {
            if let Some(found) = try_origin(o.clone(), &mut notes)? {
                return Ok(Ok(found));
            }
        }
        ManifestMode::Auto => {
            for stick in backend.external_sticks().unwrap_or_default() {
                ctx.set_message(format!("Looking for an OTA manifest on {}", stick.id));
                let Ok(items) = update_jobs::scan_stick(backend, &stick) else {
                    continue;
                };
                for item in items {
                    if let StickItem::Tree { target, .. } = &item {
                        if target.version == info.version
                            && target.machine == info.machine
                            && target.suffix.as_str() == info.suffix
                        {
                            if let Some(found) = try_origin(item.origin(&stick), &mut notes)? {
                                return Ok(Ok(found));
                            }
                        }
                    }
                }
            }
            match Suffix::from_str(&info.suffix) {
                Ok(suffix) => {
                    ctx.set_message("Fetching the manifest from the OTA server");
                    let origin = Origin::Network {
                        base_url: DEFAULT_OTA_BASE_URL.to_string(),
                        target: Target::new(info.version.clone(), info.machine.clone(), suffix),
                        proxy: Some(DEFAULT_HTTP_PROXY.to_string()).filter(|p| !p.is_empty()),
                    };
                    if let Some(found) = try_origin(origin, &mut notes)? {
                        return Ok(Ok(found));
                    }
                }
                Err(e) => notes.push(format!("suffix '{}': {e}", info.suffix)),
            }
        }
    }
    Ok(Err(if notes.is_empty() {
        "no manifest found".into()
    } else {
        notes.join("; ")
    }))
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

/// TST-4 for one slot: outcome and detail lines.
fn check_slot(
    backend: &dyn SystemBackend,
    slot: Slot,
    opts: &SlotTestOptions,
    ctx: &TaskCtx,
) -> Result<(Outcome, String, Vec<String>), TaskFailure> {
    let mut lines = Vec::new();
    let role = PartRole::slot(slot);
    let part_size = backend
        .layout()
        .ok()
        .and_then(|l| l.get(role).map(|e| e.size))
        .filter(|s| *s > 0)
        .unwrap_or(u64::MAX);
    ctx.set_message(format!("Slot {}: superblock", slot.letter()));
    let mut f = backend.open_slot_reader(slot).map_err(TaskFailure::from)?;
    let mut sb = [0u8; SB_SIZE];
    let sb_read = f.read_exact(&mut sb);
    drop(f);
    let info = match sb_read
        .map_err(|e| vec![format!("cannot read the superblock: {e}")])
        .and_then(|_| check_superblock(&sb, part_size))
    {
        Ok(i) => i,
        Err(p) if p.is_empty() => {
            return Ok((
                Outcome::Warning,
                "empty (no squashfs image)".into(),
                vec!["No squashfs superblock: the slot is empty or was overwritten.".into()],
            ))
        }
        Err(p) => {
            lines.extend(p.iter().map(|x| format!("superblock: {x}")));
            return Ok((Outcome::Fail, "bad squashfs superblock".into(), lines));
        }
    };
    lines.push(format!(
        "superblock OK: squashfs 4.0, {}, {} KiB blocks, {} inodes, image {} of {}",
        info.compression,
        info.block_size / 1024,
        info.inodes,
        format_bytes(info.image_len),
        if part_size == u64::MAX {
            "?".to_string()
        } else {
            format_bytes(part_size)
        }
    ));
    let mut outcome = Outcome::Pass;
    let mut summary = Vec::new();

    if opts.read_files {
        ctx.set_message(format!("Slot {}: mounting", slot.letter()));
        match backend.mount_slot(slot) {
            Ok(guard) => {
                ctx.set_message(format!("Slot {}: reading every file", slot.letter()));
                let mut tick = |done: u64, total: u64| {
                    ctx.set_progress(done, total);
                    !ctx.is_cancelled()
                };
                let st = read_tree(guard.path(), &mut tick);
                if let Err(e) = guard.unmount() {
                    lines.push(format!("unmount: {e}"));
                }
                let st = st.ok_or(TaskFailure::Cancelled)?;
                lines.push(format!(
                    "files: {} files in {} directories, {} read, {} error(s)",
                    st.files,
                    st.dirs,
                    format_bytes(st.bytes),
                    st.error_count
                ));
                for e in &st.errors {
                    lines.push(format!("  read error: {e}"));
                }
                if st.error_count > 0 {
                    outcome = Outcome::Fail;
                    summary.push(format!("{} file read error(s)", st.error_count));
                } else {
                    summary.push(format!("{} files readable", st.files));
                }
            }
            Err(e) if e.kind == ErrorKind::NotAvailable => {
                lines.push(format!("files: not read ({e})"));
                outcome = worst(outcome, Outcome::Warning);
                summary.push("files not read".into());
            }
            Err(e) if e.kind == ErrorKind::Cancelled => return Err(TaskFailure::Cancelled),
            Err(e) => {
                lines.push(format!("files: the slot does not mount: {e}"));
                outcome = Outcome::Fail;
                summary.push("does not mount".into());
            }
        }
    }

    if opts.manifest != ManifestMode::Off {
        let build = backend.slot_info(slot).ok().flatten();
        match build {
            None => lines.push("manifest: no xoros.env, the build is unknown".into()),
            Some(b) => match find_manifest_image(backend, &b, &opts.manifest, ctx)? {
                Err(note) => lines.push(format!("manifest: not compared ({note})")),
                Ok((source, image)) => {
                    ctx.set_message(format!(
                        "Slot {}: SHA-256 of {}",
                        slot.letter(),
                        format_bytes(image.size)
                    ));
                    let f = backend.open_slot_reader(slot).map_err(TaskFailure::from)?;
                    match hash_prefix(f, image.size, ctx) {
                        Ok(sha) if sha.eq_ignore_ascii_case(&image.sha256) => {
                            lines.push(format!("manifest: SHA-256 matches ({source})"));
                            summary.push("SHA-256 matches the manifest".into());
                        }
                        Ok(sha) => {
                            lines.push(format!(
                                "manifest: SHA-256 MISMATCH ({source})\n  expected {}\n  actual   {sha}",
                                image.sha256
                            ));
                            outcome = Outcome::Fail;
                            summary.push("SHA-256 does not match the manifest".into());
                        }
                        Err(TaskFailure::Cancelled) => return Err(TaskFailure::Cancelled),
                        Err(TaskFailure::Error(e)) => {
                            lines.push(format!("manifest: hashing failed: {e}"));
                            outcome = Outcome::Fail;
                            summary.push("read error while hashing".into());
                        }
                    }
                }
            },
        }
    }
    if summary.is_empty() {
        summary.push("superblock OK".into());
    }
    Ok((outcome, summary.join(", "), lines))
}

/// TST-4: check the selected slots (in a worker thread).
pub fn run_slots(
    backend: &dyn SystemBackend,
    opts: &SlotTestOptions,
    ctx: &TaskCtx,
) -> Result<TestReport, TaskFailure> {
    if opts.slots.is_empty() {
        return Err(TaskFailure::Error("No slot selected.".into()));
    }
    let mut outcome = Outcome::Pass;
    let mut summary = Vec::new();
    let mut details = String::new();
    for &slot in &opts.slots {
        let (o, s, lines) = match check_slot(backend, slot, opts, ctx) {
            Ok(r) => r,
            Err(TaskFailure::Cancelled) => return Err(TaskFailure::Cancelled),
            Err(TaskFailure::Error(e)) => (Outcome::Error, "not checked".into(), vec![e]),
        };
        outcome = worst(outcome, o);
        summary.push(format!("{}: {} ({s})", slot.letter(), o));
        let _ = writeln!(details, "Slot {} ({}): {o}", slot.letter(), slot.label());
        for l in lines {
            let _ = writeln!(details, "  {l}");
        }
        details.push('\n');
    }
    Ok(TestReport {
        outcome,
        target: format!(
            "slot {}",
            opts.slots
                .iter()
                .map(|s| s.letter())
                .collect::<Vec<_>>()
                .join(" + ")
        ),
        summary: summary.join("; "),
        details,
    })
}

/// TST-5: hash the rescue image, store it in `state` (Status screen) and compare it with the
/// slots' `RESCUE_IMAGE_SHA256`.
pub fn run_rescue(
    backend: &dyn SystemBackend,
    state: &Mutex<RescueCheck>,
    ctx: &TaskCtx,
) -> Result<TestReport, TaskFailure> {
    ctx.set_message("SHA-256 of the rescue partition");
    *lock(state) = RescueCheck::Checking;
    let r = backend.hash_image(
        PartRole::Rescue,
        &mut |done, total| ctx.set_progress(done, total),
        ctx.cancel_token(),
    );
    let sha = match r {
        Ok(sha) => {
            *lock(state) = RescueCheck::Done(sha.clone());
            sha
        }
        Err(e) if e.kind == ErrorKind::Cancelled => {
            *lock(state) = RescueCheck::NotChecked;
            return Err(TaskFailure::Cancelled);
        }
        Err(e) => {
            *lock(state) = RescueCheck::Failed(e.to_string());
            let outcome = if e.kind == ErrorKind::InvalidValue || e.kind == ErrorKind::Io {
                Outcome::Fail
            } else {
                Outcome::Error
            };
            return Ok(TestReport {
                outcome,
                target: "rescue partition".into(),
                summary: "The rescue image cannot be hashed.".into(),
                details: e.to_string(),
            });
        }
    };
    let mut details = format!("Rescue image SHA-256: {sha}\n\n");
    let mut matches = Vec::new();
    let mut known = 0;
    for slot in Slot::ALL {
        let line = match backend.slot_info(slot) {
            Ok(Some(i)) => match i.rescue_image_sha256.as_deref() {
                Some(s) if s.eq_ignore_ascii_case(&sha) => {
                    known += 1;
                    matches.push(slot.letter());
                    "matches".to_string()
                }
                Some(s) => {
                    known += 1;
                    format!("differs (slot expects {s})")
                }
                None => "no RESCUE_IMAGE_SHA256 in xoros.env".into(),
            },
            Ok(None) => "empty/invalid slot".into(),
            Err(e) => format!("not readable: {e}"),
        };
        let _ = writeln!(details, "Slot {}: {line}", slot.letter());
    }
    let (outcome, summary) = if !matches.is_empty() {
        (
            Outcome::Pass,
            format!(
                "Matches the rescue image of slot {}.",
                matches.join(" and ")
            ),
        )
    } else if known > 0 {
        details.push_str(
            "\nThe rescue image differs from the one built with the slots. This is a warning \
             only: the rescue system may simply be older or newer than the slots.\n",
        );
        (
            Outcome::Warning,
            "Does not match the rescue image of any slot.".to_string(),
        )
    } else {
        (
            Outcome::Warning,
            "No slot names a rescue image to compare with.".to_string(),
        )
    };
    Ok(TestReport {
        outcome,
        target: "rescue partition".into(),
        summary,
        details,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::hw::testutil::TempDir;
    use crate::tasks::{TaskKind, TaskRunner};
    use std::sync::Arc;

    #[test]
    fn superblock_checks() {
        let good = example_superblock(10_000_000);
        let i = check_superblock(&good, 1 << 30).expect("good");
        assert_eq!(i.image_len, 10_002_432);
        assert_eq!(i.compression, "xz");
        assert_eq!(i.block_size, 128 * 1024);
        assert_eq!(check_superblock(&[0u8; 96], 1 << 30), Err(vec![]));
        let p = check_superblock(&good, 1 << 20).expect_err("too big");
        assert!(p[0].contains("beyond the partition size"), "{p:?}");
        // the crate's minimal superblock is not a valid squashfs 4.0
        let minimal = xoros_core::squashfs::fake_superblock(10_000);
        let p = check_superblock(&minimal, 1 << 30).expect_err("minimal");
        assert!(p.iter().any(|x| x.contains("version 0.0")));
        assert!(p.iter().any(|x| x.contains("compression")));
        let mut bad = good;
        bad[12..16].copy_from_slice(&3000u32.to_le_bytes());
        assert!(check_superblock(&bad, 1 << 30).is_err());
    }

    #[test]
    fn tree_read_counts_and_errors() {
        let d = TempDir::new("slotfs");
        d.write("a/b/c.bin", &vec![1u8; 3 << 20]);
        d.write("x.txt", b"hello");
        std::os::unix::fs::symlink("/nonexistent", d.path().join("dangling")).expect("symlink");
        let mut last = (0, 0);
        let st = read_tree(d.path(), &mut |a, b| {
            last = (a, b);
            true
        })
        .expect("not cancelled");
        assert_eq!(st.files, 2);
        assert_eq!(st.dirs, 3);
        assert_eq!(st.bytes, (3 << 20) + 5);
        assert_eq!(st.error_count, 0);
        assert_eq!(last, (st.bytes, st.bytes));
        assert!(read_tree(d.path(), &mut |_, _| false).is_none());
    }

    fn run_in_task<T: Send + 'static>(
        f: impl FnOnce(&TaskCtx) -> Result<T, TaskFailure> + Send + 'static,
    ) -> Result<T, TaskFailure> {
        let out = Arc::new(Mutex::new(None));
        let o2 = Arc::clone(&out);
        TaskRunner::new()
            .spawn(
                "t",
                TaskKind::ReadOnly,
                move |ctx| {
                    *lock(&o2) = Some(f(ctx));
                    Ok(String::new())
                },
                None,
            )
            .expect("spawn")
            .wait();
        let r = lock(&out).take().expect("result");
        r
    }

    fn sim() -> (TempDir, Arc<SimBackend>) {
        let d = TempDir::new("slotcheck");
        SimBackend::create_example(d.path()).expect("example");
        let b = SimBackend::open(d.path()).expect("open");
        let mut s = b.load().expect("load");
        s.hash_delay_ms_per_mib = 0;
        d.write(
            "sim.json",
            serde_json::to_string(&s).expect("json").as_bytes(),
        );
        (d, Arc::new(b))
    }

    #[test]
    fn slots_in_simulation() {
        let (d, b) = sim();
        let b2 = Arc::clone(&b);
        let opts = SlotTestOptions {
            manifest: ManifestMode::Off,
            ..SlotTestOptions::default()
        };
        let r = run_in_task(move |ctx| run_slots(b2.as_ref(), &opts, ctx)).expect("run");
        assert_eq!(r.outcome, Outcome::Pass, "{r:?}");
        assert!(r.details.contains("superblock OK"), "{}", r.details);
        assert!(r.details.contains("2 files in"), "{}", r.details);
        // A broken slot B and missing file tree of slot A.
        let mut img = std::fs::read(d.path().join("rootfs_b.img")).expect("img");
        img[28] = 3; // squashfs 3.x
        std::fs::write(d.path().join("rootfs_b.img"), img).expect("write");
        std::fs::remove_dir_all(d.path().join("slotfs/rootfs_a")).expect("rm");
        let b2 = Arc::clone(&b);
        let opts = SlotTestOptions {
            manifest: ManifestMode::Off,
            ..SlotTestOptions::default()
        };
        let r = run_in_task(move |ctx| run_slots(b2.as_ref(), &opts, ctx)).expect("run");
        assert_eq!(r.outcome, Outcome::Fail);
        assert!(
            r.summary.contains("A: WARNING (files not read)"),
            "{}",
            r.summary
        );
        assert!(r.details.contains("version 3.0"), "{}", r.details);
    }

    #[test]
    fn slot_against_manifest_on_stick() {
        let (d, b) = sim();
        // Slot B gets exactly the image of the example OTA tree on the stick.
        let tree = d.path().join("sticks/usb1/xoros/ota");
        let manifest_path = walk_find(&tree, "manifest.json").expect("manifest");
        let m = xoros_core::manifest::Manifest::read(&manifest_path).expect("manifest");
        let image = m.images[0].clone();
        let dir = manifest_path.parent().expect("dir").to_path_buf();
        let mut data = Vec::new();
        for c in &image.chunks {
            data.extend(std::fs::read(dir.join(&c.file)).expect("chunk"));
        }
        let mut s = b.load().expect("load");
        let mut slot = s.slots.rootfs_b.clone().expect("b");
        slot.timestamp = image.build_timestamp.as_str().to_string();
        s.slots.rootfs_b = Some(slot);
        d.write(
            "sim.json",
            serde_json::to_string(&s).expect("json").as_bytes(),
        );
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(d.path().join("rootfs_b.img"))
            .expect("open");
        std::os::unix::fs::FileExt::write_all_at(&f, &data, 0).expect("write");
        drop(f);
        let b2 = Arc::clone(&b);
        let opts = SlotTestOptions {
            slots: vec![Slot::B],
            read_files: false,
            manifest: ManifestMode::Auto,
        };
        let r = run_in_task(move |ctx| run_slots(b2.as_ref(), &opts, ctx)).expect("run");
        assert!(r.details.contains("SHA-256 matches"), "{}", r.details);
        // One changed byte inside the image: mismatch.
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(d.path().join("rootfs_b.img"))
            .expect("open");
        std::os::unix::fs::FileExt::write_all_at(&f, &[0xEE], 5000).expect("write");
        drop(f);
        let b2 = Arc::clone(&b);
        let opts = SlotTestOptions {
            slots: vec![Slot::B],
            read_files: false,
            manifest: ManifestMode::Auto,
        };
        let r = run_in_task(move |ctx| run_slots(b2.as_ref(), &opts, ctx)).expect("run");
        assert_eq!(r.outcome, Outcome::Fail);
        assert!(r.details.contains("MISMATCH"), "{}", r.details);
    }

    fn walk_find(dir: &Path, name: &str) -> Option<PathBuf> {
        for e in std::fs::read_dir(dir).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(f) = walk_find(&p, name) {
                    return Some(f);
                }
            } else if p.file_name().is_some_and(|n| n == name) {
                return Some(p);
            }
        }
        None
    }

    #[test]
    fn rescue_in_simulation() {
        let (d, b) = sim();
        let state = Arc::new(Mutex::new(RescueCheck::NotChecked));
        let (b2, s2) = (Arc::clone(&b), Arc::clone(&state));
        let r = run_in_task(move |ctx| run_rescue(b2.as_ref(), &s2, ctx)).expect("run");
        assert_eq!(r.outcome, Outcome::Pass, "{r:?}");
        assert!(lock(&state).sha256().is_some());
        // Both slots name another rescue image: warning only.
        let mut s = b.load().expect("load");
        for i in [s.slots.rootfs_a.as_mut(), s.slots.rootfs_b.as_mut()]
            .into_iter()
            .flatten()
        {
            i.rescue_image_sha256 = Some("00".into());
        }
        d.write(
            "sim.json",
            serde_json::to_string(&s).expect("json").as_bytes(),
        );
        let (b2, s2) = (Arc::clone(&b), Arc::clone(&state));
        let r = run_in_task(move |ctx| run_rescue(b2.as_ref(), &s2, ctx)).expect("run");
        assert_eq!(r.outcome, Outcome::Warning);
        // Not a squashfs: fail.
        d.write("rescue.img", &[0u8; 8192]);
        let (b2, s2) = (Arc::clone(&b), Arc::clone(&state));
        let r = run_in_task(move |ctx| run_rescue(b2.as_ref(), &s2, ctx)).expect("run");
        assert_eq!(r.outcome, Outcome::Fail);
        assert!(matches!(*lock(&state), RescueCheck::Failed(_)));
    }
}
