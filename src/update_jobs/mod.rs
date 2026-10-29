//! Slot updates from the network (OTA-1..11) and from a USB stick (USB-2..5) on top of the
//! crate's image sources and flash engine (`xoros_core::source`, `xoros_core::flash`).
//! No cursive (ARCH-1): the UI and the CLI run these functions inside a
//! [`TaskRunner`](crate::tasks::TaskRunner) worker.
//!
//! Flow: [`device_target`] (OTA-1) -> [`check`] (fetch and check the manifest, list images,
//! OTA-2/3/9) -> the caller picks an image and [`Targets`] (OTA-4) -> [`run_update`]
//! (`flash::update_slot`, plus `flash::copy_slot` for A+B, OTA-5..7, USB-3..5).
//!
//! Rescue images are listed but never written (OTA-9): only `rootfs` images reach
//! `update_slot`, and `update_rescue` is not used.

pub mod example;

use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Serialize;
use xoros_core::env::{BuildTimestamp, Suffix};
use xoros_core::flash::{self, FlashOptions, FlashReport, UpdateOptions};
use xoros_core::manifest::{Image, ImageType, Manifest, Relation, Target};
use xoros_core::source::{
    load_manifest, DirSource, FileMeta, FileSource, HttpSource, ImageSource, SignaturePolicy,
    SignatureStatus, Sidecars,
};
use xoros_core::squashfs::{Superblock, SUPERBLOCK_SIZE};
use xoros_core::{Phase, ProgressEvent};

use crate::backend::{BackendError, BuildInfo, MountGuard, Slot, Stick, SystemBackend};
use crate::clock::format_build_timestamp;
use crate::constants::{STICK_OTA_DEPTH, STICK_OTA_DIRS};
use crate::net::config::dns_servers;
use crate::net::http::{check_base_url, http_options};
use crate::net::{time, NetEnv};
use crate::tasks::{format_bytes, TaskCtx, TaskFailure};

// --------------------------------------------------------------------------------------------
// targets and device identity (OTA-1, OTA-4)

/// Which slots to write (OTA-4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Targets {
    A,
    B,
    /// Write A, verify, copy A to B and verify B (OTA-5).
    Both,
}

impl Targets {
    pub const ALL: [Targets; 3] = [Targets::A, Targets::B, Targets::Both];

    pub fn slots(self) -> Vec<Slot> {
        match self {
            Targets::A => vec![Slot::A],
            Targets::B => vec![Slot::B],
            Targets::Both => vec![Slot::A, Slot::B],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Targets::A => "slot A",
            Targets::B => "slot B",
            Targets::Both => "slots A+B",
        }
    }
}

impl FromStr for Targets {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_uppercase().as_str() {
            "A" => Ok(Targets::A),
            "B" => Ok(Targets::B),
            "AB" | "A+B" | "BOTH" => Ok(Targets::Both),
            _ => Err(format!("Unknown target '{s}' (A, B or AB).")),
        }
    }
}

/// What is known about the device (OTA-1).
#[derive(Clone, Debug, Default)]
pub struct DeviceTarget {
    /// From the newest valid slot's `xoros.env`.
    pub target: Option<Target>,
    pub source_slot: Option<Slot>,
    /// `MACHINE` of a valid slot, else from the rescue host name `xoros-rescue-<machine>`.
    pub machine: Option<String>,
    /// Build timestamps of the slots (for newer/same/older, OTA-3).
    pub timestamps: [Option<BuildTimestamp>; 2],
    pub infos: [Option<BuildInfo>; 2],
}

/// OTA-1: version, machine and suffix of the newest valid slot.
pub fn device_target(backend: &dyn SystemBackend) -> DeviceTarget {
    let mut d = DeviceTarget::default();
    let mut best: Option<(BuildTimestamp, Slot, Target)> = None;
    for slot in Slot::ALL {
        let Some(info) = backend.slot_info(slot).ok().flatten() else {
            continue;
        };
        let ts = BuildTimestamp::parse(&info.timestamp).ok();
        let suffix = Suffix::from_str(&info.suffix).ok();
        if let (Some(ts), Some(suffix)) = (ts.clone(), suffix) {
            let t = Target::new(info.version.clone(), info.machine.clone(), suffix);
            if best.as_ref().is_none_or(|(b, _, _)| ts > *b) {
                best = Some((ts, slot, t));
            }
        }
        d.timestamps[slot.index()] = ts;
        if d.machine.is_none() && !info.machine.is_empty() {
            d.machine = Some(info.machine.clone());
        }
        d.infos[slot.index()] = Some(info);
    }
    if let Some((_, slot, t)) = best {
        d.machine = Some(t.machine.clone());
        d.source_slot = Some(slot);
        d.target = Some(t);
    }
    if d.machine.is_none() {
        d.machine = backend
            .rescue_system()
            .ok()
            .and_then(|r| r.hostname)
            .and_then(|h| h.strip_prefix("xoros-rescue-").map(str::to_string));
    }
    d
}

// --------------------------------------------------------------------------------------------
// origins

/// Where an update comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// OTA server (OTA-2): `<base>/<version>/<machine>/<suffix>/`.
    Network {
        base_url: String,
        target: Target,
        /// OTA-11.
        proxy: Option<String>,
    },
    /// An OTA tree on a stick (USB-2.1), `rel` relative to the stick root.
    StickTree { stick: Stick, rel: PathBuf },
    /// A single `*.squashfs` image on a stick (USB-2.2).
    StickImage { stick: Stick, rel: PathBuf },
}

impl Origin {
    pub fn describe(&self) -> String {
        match self {
            Origin::Network {
                base_url, target, ..
            } => crate::net::http::ota_dir_url(base_url, target),
            Origin::StickTree { stick, rel } | Origin::StickImage { stick, rel } => {
                format!("{}:/{}", stick.id, rel.display())
            }
        }
    }

    fn stick(&self) -> Option<&Stick> {
        match self {
            Origin::Network { .. } => None,
            Origin::StickTree { stick, .. } | Origin::StickImage { stick, .. } => Some(stick),
        }
    }
}

/// An opened image source; a stick stays mounted while this lives.
pub struct Opened {
    pub source: Box<dyn ImageSource + Send>,
    /// `FileSource` without a sidecar hash (USB-2.2: needs the "unverified" confirmation).
    pub unverified: bool,
    mount: Option<MountGuard>,
}

impl Opened {
    /// USB-5: unmount the stick (if this job mounted it) and sync.
    pub fn release(self) -> Result<Option<String>, BackendError> {
        drop(self.source);
        let note = match self.mount {
            Some(g) => {
                let id = g.path().display().to_string();
                g.unmount()?;
                Some(id)
            }
            None => None,
        };
        // SAFETY: sync(2) has no arguments and cannot fail.
        unsafe { libc::sync() };
        Ok(note)
    }
}

fn fail(e: impl fmt::Display) -> TaskFailure {
    TaskFailure::Error(e.to_string())
}

fn core_fail(e: xoros_core::Error) -> TaskFailure {
    match e {
        xoros_core::Error::Cancelled => TaskFailure::Cancelled,
        other => TaskFailure::Error(other.to_string()),
    }
}

/// Short text of a flash phase.
pub fn phase_text(p: Phase) -> &'static str {
    match p {
        Phase::Download => "Reading",
        Phase::Write => "Writing",
        Phase::Verify => "Verifying",
        Phase::Skip => "Already on the slot",
        Phase::Retry => "Retrying",
        _ => "Working",
    }
}

/// Forwards the crate's progress events to the task (progress bar + message, UI-3).
pub fn task_progress<'a>(ctx: &'a TaskCtx, label: &'a str) -> impl Fn(&ProgressEvent) + 'a {
    move |e: &ProgressEvent| {
        ctx.set_progress(e.bytes_done, e.bytes_total);
        let chunk = match (e.chunk_index, e.chunk_count) {
            (Some(i), Some(n)) if e.phase != Phase::Verify => format!(", chunk {}/{n}", i + 1),
            _ => String::new(),
        };
        ctx.set_message(format!("{label}: {}{chunk}", phase_text(e.phase)));
    }
}

/// Meta data for a single image without a `.json` sidecar: the device's target and the file
/// time as build timestamp.
fn fallback_meta(dev: &DeviceTarget, path: &Path) -> Result<FileMeta, TaskFailure> {
    let target = dev.target.clone().or_else(|| {
        dev.machine
            .clone()
            .map(|m| Target::new("unknown", m, Suffix::Stable))
    });
    let target = target.ok_or_else(|| {
        fail(format!(
            "{} has no .json sidecar and the device's machine is unknown.",
            path.display()
        ))
    })?;
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH);
    let secs = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let ts = chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.format("%Y%m%d%H%M%S").to_string())
        .unwrap_or_else(|| "20000101000000".into());
    let ts = BuildTimestamp::parse(&ts).map_err(core_fail)?;
    Ok(FileMeta::new(&target, ts))
}

/// USB-4: refuse data without a squashfs superblock before anything else.
fn check_squashfs_file(path: &Path) -> Result<(), TaskFailure> {
    let mut f = std::fs::File::open(path).map_err(|e| fail(format!("{}: {e}", path.display())))?;
    let mut head = vec![0u8; SUPERBLOCK_SIZE];
    f.read_exact(&mut head)
        .map_err(|_| fail(format!("{}: not a squashfs image.", path.display())))?;
    Superblock::parse(&head)
        .map(|_| ())
        .map_err(|e| fail(format!("{}: {e}", path.display())))
}

/// OTA-8: trusted keys and `require_signature` from the settings (CFG-1) plus the keys shipped
/// with the rescue image (DIST-5).
pub fn signature_policy() -> SignaturePolicy {
    let (policy, warnings) = crate::settings::state().signature_policy();
    for w in warnings {
        log::warn!("OTA-8: {w}");
    }
    policy
}

/// Open the source of `origin` (mounts a stick read-only; OTA-10 clock check for the network).
pub fn open_source(
    backend: &dyn SystemBackend,
    origin: &Origin,
    ctx: &TaskCtx,
) -> Result<Opened, TaskFailure> {
    match origin {
        Origin::Network {
            base_url,
            target,
            proxy,
        } => {
            check_base_url(base_url).map_err(fail)?;
            let env = NetEnv::for_backend(backend);
            ctx.set_message("Checking the clock");
            if let Some(r) = time::ensure_clock(&env).map_err(fail)? {
                log::info!("clock synced before OTA: {}", r.text());
            }
            let options = http_options(proxy.clone(), dns_servers(&env));
            let source = HttpSource::new(base_url, target, options).map_err(core_fail)?;
            Ok(Opened {
                source: Box::new(source),
                unverified: false,
                mount: None,
            })
        }
        Origin::StickTree { stick, rel } => {
            let g = backend.mount_stick(stick, false)?;
            let dir = g.path().join(rel);
            Ok(Opened {
                source: Box::new(DirSource::new(dir)),
                unverified: false,
                mount: Some(g),
            })
        }
        Origin::StickImage { stick, rel } => {
            let g = backend.mount_stick(stick, false)?;
            let path = g.path().join(rel);
            check_squashfs_file(&path)?;
            let sc = Sidecars::read(&path).map_err(core_fail)?;
            let meta = match sc.meta {
                Some(m) => m,
                None => fallback_meta(&device_target(backend), &path)?,
            };
            ctx.set_message(format!("Hashing {}", rel.display()));
            let progress = task_progress(ctx, "Image on the stick");
            let fs = FileSource::new(&path, sc.sha256, meta, &progress, ctx.cancel_token())
                .map_err(core_fail)?;
            Ok(Opened {
                unverified: !fs.is_verified(),
                source: Box::new(fs),
                mount: Some(g),
            })
        }
    }
}

// --------------------------------------------------------------------------------------------
// check (OTA-2, OTA-3, OTA-8, OTA-9)

/// A rootfs image with its relation to each slot (OTA-3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRow {
    pub image: Image,
    /// Relation to slot A and slot B (`Newer` for an empty slot).
    pub relation: [Relation; 2],
}

impl ImageRow {
    /// Writing this image to `targets` is the same or an older build: "downgrade" (OTA-3).
    pub fn is_downgrade(&self, targets: Targets) -> bool {
        targets
            .slots()
            .iter()
            .any(|s| self.relation[s.index()] != Relation::Newer)
    }

    pub fn label(&self) -> String {
        format!(
            "{}  {:>10}  A: {}  B: {}",
            format_build_timestamp(self.image.build_timestamp.as_str()),
            format_bytes(self.image.size),
            relation_text(self.relation[0]),
            relation_text(self.relation[1])
        )
    }
}

pub fn relation_text(r: Relation) -> &'static str {
    match r {
        Relation::Newer => "newer",
        Relation::Same => "same",
        Relation::Older => "older",
    }
}

/// A rescue image: shown, never written (OTA-9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RescueRow {
    pub image: Image,
    /// Compared with the running rescue image's SHA-256, if known.
    pub matches_current: Option<bool>,
}

/// Result of [`check`].
#[derive(Clone, Debug)]
pub struct CheckResult {
    pub origin: Origin,
    pub source: String,
    pub manifest: Manifest,
    pub signature: SignatureStatus,
    /// Newest first; the default selection is the first (OTA-3).
    pub rootfs: Vec<ImageRow>,
    pub rescue: Vec<RescueRow>,
    pub boot_images: usize,
    /// Single image without a sidecar hash (USB-2.2).
    pub unverified: bool,
    pub warnings: Vec<String>,
}

pub fn signature_text(s: &SignatureStatus) -> String {
    match s {
        SignatureStatus::Verified { key } => format!("verified (trusted key #{})", key.index),
        SignatureStatus::Unsigned => "unsigned (no signature.json)".into(),
        SignatureStatus::NotChecked => {
            "signed, not checked (no trusted keys configured)".into()
        }
        _ => "unknown".into(),
    }
}

impl CheckResult {
    pub fn text(&self) -> String {
        let m = &self.manifest;
        let mut t = format!(
            "Source:    {}\nManifest:  {} {} {} {}\nSignature: {}\n",
            self.source,
            m.distro,
            m.version,
            m.machine,
            m.suffix,
            signature_text(&self.signature)
        );
        if self.unverified {
            t.push_str("WARNING: no .sha256 sidecar, the image cannot be verified.\n");
        }
        for w in &self.warnings {
            t.push_str(&format!("WARNING: {w}\n"));
        }
        t.push_str(&format!("\nRootfs images ({}):\n", self.rootfs.len()));
        if self.rootfs.is_empty() {
            t.push_str("  none\n");
        }
        for r in &self.rootfs {
            t.push_str(&format!("  {}\n", r.label()));
        }
        if !self.rescue.is_empty() {
            t.push_str("\nRescue images (not written from the rescue system):\n");
            for r in &self.rescue {
                let m = match r.matches_current {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "unknown",
                };
                t.push_str(&format!(
                    "  {}  {:>10}  matches current rescue: {m}\n",
                    format_build_timestamp(r.image.build_timestamp.as_str()),
                    format_bytes(r.image.size)
                ));
            }
        }
        if self.boot_images > 0 {
            t.push_str(&format!(
                "\n{} boot image(s): not supported in this version.\n",
                self.boot_images
            ));
        }
        t
    }
}

/// OTA-2/3/9 (USB-3): fetch and check the manifest and list the images. `rescue_sha256` is the
/// running rescue image's hash, if known.
pub fn check(
    backend: &dyn SystemBackend,
    origin: &Origin,
    rescue_sha256: Option<&str>,
    ctx: &TaskCtx,
) -> Result<CheckResult, TaskFailure> {
    let opened = open_source(backend, origin, ctx)?;
    ctx.set_message("Reading the manifest");
    let r = check_opened(backend, origin, &opened, rescue_sha256);
    drop(opened);
    r
}

fn check_opened(
    backend: &dyn SystemBackend,
    origin: &Origin,
    opened: &Opened,
    rescue_sha256: Option<&str>,
) -> Result<CheckResult, TaskFailure> {
    let dev = device_target(backend);
    let net_target = match origin {
        Origin::Network { target, .. } => Some(target),
        _ => None,
    };
    let policy = signature_policy();
    let loaded =
        load_manifest(opened.source.as_ref(), net_target, &policy).map_err(core_fail)?;
    let m = loaded.manifest;
    let mut warnings = Vec::new();
    if net_target.is_none() {
        // Stick: the machine must match the device (OTA-2 / USB-3).
        if let Some(machine) = &dev.machine {
            if &m.machine != machine {
                return Err(fail(format!(
                    "Manifest machine mismatch: the device is '{machine}', the update is for '{}'.",
                    m.machine
                )));
            }
        }
    }
    if let Some(t) = &dev.target {
        if t.suffix != m.suffix {
            warnings.push(format!(
                "suffix changes from '{}' to '{}'.",
                t.suffix, m.suffix
            ));
        }
        if t.version != m.version {
            warnings.push(format!(
                "version changes from '{}' to '{}'.",
                t.version, m.version
            ));
        }
    }
    let rel = |ts: &BuildTimestamp, slot: Slot| match &dev.timestamps[slot.index()] {
        None => Relation::Newer,
        Some(cur) => match ts.cmp(cur) {
            std::cmp::Ordering::Greater => Relation::Newer,
            std::cmp::Ordering::Equal => Relation::Same,
            std::cmp::Ordering::Less => Relation::Older,
        },
    };
    let rootfs = m
        .candidates(ImageType::Rootfs, None)
        .into_iter()
        .map(|c| ImageRow {
            relation: [
                rel(&c.image.build_timestamp, Slot::A),
                rel(&c.image.build_timestamp, Slot::B),
            ],
            image: c.image,
        })
        .collect();
    let rescue = m
        .candidates(ImageType::Rescue, None)
        .into_iter()
        .map(|c| RescueRow {
            matches_current: rescue_sha256.map(|s| s.eq_ignore_ascii_case(&c.image.sha256)),
            image: c.image,
        })
        .collect();
    let boot_images = m
        .images
        .iter()
        .filter(|i| i.image_type == ImageType::Boot)
        .count();
    Ok(CheckResult {
        origin: origin.clone(),
        source: opened.source.describe(),
        signature: loaded.signature,
        rootfs,
        rescue,
        boot_images,
        unverified: opened.unverified,
        warnings,
        manifest: m,
    })
}

// --------------------------------------------------------------------------------------------
// stick scanning (USB-2)

/// An update source found on a stick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StickItem {
    Tree {
        rel: PathBuf,
        target: Target,
        rootfs_images: usize,
    },
    Image {
        rel: PathBuf,
        size: u64,
        has_sha256: bool,
        meta: Option<String>,
    },
}

impl StickItem {
    pub fn label(&self) -> String {
        match self {
            StickItem::Tree {
                rel,
                target,
                rootfs_images,
            } => format!(
                "OTA tree {}  ({} {} {}, {rootfs_images} rootfs image(s))",
                rel.display(),
                target.version,
                target.machine,
                target.suffix
            ),
            StickItem::Image {
                rel,
                size,
                has_sha256,
                meta,
            } => format!(
                "Image {}  {}{}{}",
                rel.display(),
                format_bytes(*size),
                meta.as_deref()
                    .map(|m| format!("  {m}"))
                    .unwrap_or_default(),
                if *has_sha256 { "" } else { "  (no .sha256)" }
            ),
        }
    }

    pub fn origin(&self, stick: &Stick) -> Origin {
        match self {
            StickItem::Tree { rel, .. } => Origin::StickTree {
                stick: stick.clone(),
                rel: rel.clone(),
            },
            StickItem::Image { rel, .. } => Origin::StickImage {
                stick: stick.clone(),
                rel: rel.clone(),
            },
        }
    }
}

fn find_images(root: &Path, dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() && depth > 0 {
            find_images(root, &p, depth - 1, out);
        } else if ft.is_file() && p.extension().is_some_and(|x| x == "squashfs") {
            if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
    }
}

/// USB-2: OTA trees (`manifest.json` up to 4 levels below the stick root, `xoros/`, `ota/` and
/// `xoros/ota/`) and `*.squashfs` images (up to 2 levels below those directories).
pub fn scan_stick(backend: &dyn SystemBackend, stick: &Stick) -> Result<Vec<StickItem>, String> {
    let g = backend.mount_stick(stick, false).map_err(|e| e.to_string())?;
    let root = g.path().to_path_buf();
    let mut trees: Vec<(PathBuf, Manifest)> = Vec::new();
    let mut images: Vec<PathBuf> = Vec::new();
    for d in STICK_OTA_DIRS {
        let base = root.join(d);
        if !base.is_dir() {
            continue;
        }
        if let Ok(found) = DirSource::discover(&base, STICK_OTA_DEPTH) {
            for (p, m) in found {
                if !trees.iter().any(|(q, _)| *q == p) {
                    trees.push((p, m));
                }
            }
        }
        find_images(&root, &base, 2, &mut images);
    }
    images.sort();
    images.dedup();
    let mut out: Vec<StickItem> = trees
        .into_iter()
        .filter_map(|(p, m)| {
            Some(StickItem::Tree {
                rel: p.strip_prefix(&root).ok()?.to_path_buf(),
                target: m.target(),
                rootfs_images: m
                    .images
                    .iter()
                    .filter(|i| i.image_type == ImageType::Rootfs)
                    .count(),
            })
        })
        .collect();
    out.sort_by_key(|a| a.label());
    for rel in images {
        let p = root.join(&rel);
        let sc = Sidecars::read(&p).ok();
        out.push(StickItem::Image {
            size: std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0),
            has_sha256: sc.as_ref().is_some_and(|s| s.sha256.is_some()),
            meta: sc.and_then(|s| s.meta).map(|m| {
                format!(
                    "{} {} {} {}",
                    m.version,
                    m.machine,
                    m.suffix,
                    format_build_timestamp(m.build_timestamp.as_str())
                )
            }),
            rel,
        });
    }
    drop(g);
    Ok(out)
}

// --------------------------------------------------------------------------------------------
// write (OTA-4..7, USB-3..5)

/// What to write.
#[derive(Clone, Debug)]
pub struct UpdateRequest {
    pub origin: Origin,
    /// The selected image (from [`CheckResult::rootfs`]); found again in the manifest by
    /// index and SHA-256.
    pub image: Image,
    pub targets: Targets,
    /// Retries, back-off and resume (OTA-6, default on).
    pub options: FlashOptions,
}

/// Result of [`run_update`].
#[derive(Clone, Debug)]
pub struct UpdateOutcome {
    pub targets: Targets,
    pub reports: Vec<(Slot, FlashReport)>,
    pub new_build: BuildInfo,
    pub notes: Vec<String>,
}

pub fn update_summary(o: &UpdateOutcome) -> String {
    let mut t = format!(
        "{} written and verified: {} {} {} {}.\n",
        o.targets.label(),
        o.new_build.version,
        o.new_build.machine,
        o.new_build.suffix,
        format_build_timestamp(&o.new_build.timestamp)
    );
    for (slot, r) in &o.reports {
        t.push_str(&format!(
            "  {}: {} written, {} chunk(s) skipped (already there), {} retries, {:.1} s\n",
            slot.name(),
            format_bytes(r.bytes_written),
            r.chunks_skipped,
            r.retries,
            r.duration.as_secs_f64()
        ));
    }
    for n in &o.notes {
        t.push_str(n);
        t.push('\n');
    }
    t
}

/// OTA-7: which of `targets` are disabled now (after a cancel or an error).
pub fn disabled_note(backend: &dyn SystemBackend, slots: &[Slot]) -> String {
    let Ok(st) = backend.boot_state() else {
        return String::new();
    };
    let off: Vec<String> = slots
        .iter()
        .filter(|s| st.exhausted(**s))
        .map(|s| format!("{} ({}_TRY={})", s.letter(), s.name(), st.try_count(*s)))
        .collect();
    if off.is_empty() {
        String::new()
    } else {
        format!(
            "Slot {} stays disabled (try counter at MAX_TRIES): GRUB skips it until it is \
             written successfully.",
            off.join(", ")
        )
    }
}

fn build_info_of(m: &Manifest, image: &Image) -> BuildInfo {
    let rescue = m
        .images
        .iter()
        .find(|i| i.image_type == ImageType::Rescue && i.build_timestamp == image.build_timestamp)
        .map(|i| i.sha256.clone());
    BuildInfo {
        distro: m.distro.clone(),
        version: m.version.clone(),
        machine: m.machine.clone(),
        suffix: m.suffix.as_str().to_string(),
        timestamp: image.build_timestamp.as_str().to_string(),
        rescue_image_sha256: rescue,
        extra: Default::default(),
    }
}

fn slot_err(backend: &dyn SystemBackend, slot: Slot, e: xoros_core::Error) -> TaskFailure {
    match e {
        xoros_core::Error::Cancelled => TaskFailure::Cancelled,
        other => {
            let note = disabled_note(backend, &[slot]);
            TaskFailure::Error(format!("{}: {other}\n{note}", slot.name()).trim().to_string())
        }
    }
}

/// USB-4: after writing, `/xoros.env` of the slot must name the device's machine.
fn verify_written(
    backend: &dyn SystemBackend,
    slot: Slot,
    expected_machine: &str,
    notes: &mut Vec<String>,
) -> Result<(), TaskFailure> {
    match backend.slot_info(slot) {
        Ok(Some(info)) if info.machine == expected_machine => {
            notes.push(format!(
                "{}: xoros.env says {} {} {} {}.",
                slot.name(),
                info.version,
                info.machine,
                info.suffix,
                format_build_timestamp(&info.timestamp)
            ));
            Ok(())
        }
        Ok(found) => {
            let what = found
                .map(|i| format!("MACHINE={}", i.machine))
                .unwrap_or_else(|| "no valid xoros.env".into());
            let _ = backend.disable(slot);
            Err(fail(format!(
                "{} was written but has {what}, expected MACHINE={expected_machine}. \
                 The slot was disabled.",
                slot.name()
            )))
        }
        Err(e) => {
            notes.push(format!("{}: cannot read xoros.env to check it: {e}", slot.name()));
            Ok(())
        }
    }
}

/// OTA-5 (USB-3): write the image to the target slot(s). For A+B, A is written and verified
/// first, then copied to B from the local disk and B is verified against the same hash. Every
/// slot is disabled while it is written and stays disabled on error or cancel (OTA-7).
pub fn run_update(
    backend: &dyn SystemBackend,
    req: &UpdateRequest,
    ctx: &TaskCtx,
) -> Result<UpdateOutcome, TaskFailure> {
    let cancel = ctx.cancel_token();
    let opened = open_source(backend, &req.origin, ctx)?;
    ctx.set_message("Reading the manifest");
    let net_target = match &req.origin {
        Origin::Network { target, .. } => Some(target),
        _ => None,
    };
    let loaded = load_manifest(opened.source.as_ref(), net_target, &signature_policy())
        .map_err(core_fail)?;
    let m = loaded.manifest;
    let image = m
        .images
        .iter()
        .find(|i| i.index == req.image.index && i.sha256.eq_ignore_ascii_case(&req.image.sha256))
        .cloned()
        .ok_or_else(|| fail("The selected image is no longer in the manifest. Check again."))?;
    // OTA-9: never anything but rootfs (the crate refuses too).
    if image.image_type != ImageType::Rootfs {
        return Err(fail(format!(
            "A {} image cannot be written from the rescue system.",
            image.image_type
        )));
    }
    let dev = device_target(backend);
    let machine = dev.machine.clone().unwrap_or_else(|| m.machine.clone());
    if m.machine != machine {
        return Err(fail(format!(
            "Manifest machine mismatch: the device is '{machine}', the update is for '{}'.",
            m.machine
        )));
    }
    let boot = backend.boot_partition()?;
    let slots = req.targets.slots();
    // OTA-4: everything must fit before the first write.
    for s in &slots {
        let w = backend.open_slot_writer(*s)?;
        if image.size > w.capacity() {
            return Err(fail(format!(
                "The image ({}) does not fit into {} ({}).",
                format_bytes(image.size),
                s.name(),
                format_bytes(w.capacity())
            )));
        }
    }
    let info = build_info_of(&m, &image);
    let mut notes = Vec::new();
    let mut reports = Vec::new();

    let first = slots[0];
    let mut opts = UpdateOptions::default();
    opts.flash = req.options.clone();
    opts.make_preferred = true;
    {
        let label = format!("{} from {}", first.name(), opened.source.describe());
        let progress = task_progress(ctx, &label);
        let mut w = backend.open_slot_writer(first)?;
        let r = flash::update_slot(
            opened.source.as_ref(),
            &image,
            first.into(),
            &mut w,
            &boot,
            &opts,
            &progress,
            cancel,
        )
        .map_err(|e| slot_err(backend, first, e))?;
        reports.push((first, r));
    }
    backend.slot_written(first, &info)?;
    verify_written(backend, first, &machine, &mut notes)?;

    if req.targets == Targets::Both {
        // A+B: copy A to B from the local disk (no second download).
        let label = format!("{} copied from {}", Slot::B.name(), Slot::A.name());
        let progress = task_progress(ctx, &label);
        let mut reader = backend.open_slot_reader(Slot::A)?;
        let len = Superblock::read(&mut reader)
            .map(|sb| sb.image_len())
            .map_err(core_fail)?;
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|e| fail(format!("{}: {e}", Slot::A.name())))?;
        // The copy covers round_up(bytes_used, 4096); compare with the manifest hash when that
        // is the whole image (the usual case), else B is compared with what was read from A.
        let expected = (len == image.size).then_some(image.sha256.as_str());
        let mut w = backend.open_slot_writer(Slot::B)?;
        let mut copy_opts = opts.clone();
        copy_opts.make_preferred = false;
        let r = flash::copy_slot(
            Slot::A.into(),
            &mut reader,
            Slot::B.into(),
            &mut w,
            &boot,
            expected,
            &copy_opts,
            &progress,
            cancel,
        )
        .map_err(|e| slot_err(backend, Slot::B, e))?;
        reports.push((Slot::B, r));
        backend.slot_written(Slot::B, &info)?;
        verify_written(backend, Slot::B, &machine, &mut notes)?;
    }

    if let Some(stick) = req.origin.stick() {
        ctx.set_message("Unmounting the stick");
        match opened.release() {
            Ok(_) => notes.push(format!(
                "Stick {} is unmounted and synced: it can be removed now.",
                stick.id
            )),
            Err(e) => notes.push(format!("Stick {}: {e}", stick.id)),
        }
    }
    let first_name = reports
        .first()
        .map(|(s, _)| s.name())
        .unwrap_or_default();
    notes.push(format!(
        "{first_name} is first in the boot order. Reboot into it with Power -> Reboot into slot."
    ));
    Ok(UpdateOutcome {
        targets: req.targets,
        reports,
        new_build: info,
        notes,
    })
}

#[cfg(test)]
mod tests;
