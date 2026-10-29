//! Finding, verifying and importing backups (BAK-4, BAK-5).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;
use sha2::{Digest, Sha256};
use tar::EntryType;

use super::error::{BackupError, ChecksumProblem, IoContext, Result};
use super::fsops;
use super::meta::{
    part_name, BackupMetadata, PartitionRole, ARCHIVE_PREFIX, ARCHIVE_SUFFIX, CHECKSUM_SUFFIX,
    FORMAT_VERSION, LOST_FOUND, METADATA_NAME, OVERLAY_WORK_DIR, RESCUE_DIR, STICK_BACKUP_DIR,
};
use super::progress::{CancelToken, Phase, ProgressFn, Reporter};
use super::split::{hex, MultiPartReader};

/// A backup found on a stick.
#[derive(Clone, Debug)]
pub struct BackupInfo {
    /// `xoros-backup-<host>-<time>.tar.gz` (also for split backups).
    pub name: String,
    /// Directory containing the files.
    pub dir: PathBuf,
    /// Archive files in order (one file, or `.000`, `.001`, ... without gaps).
    pub parts: Vec<PathBuf>,
    /// Sum of the part sizes.
    pub total_size: u64,
    /// `<name>.sha256` if present.
    pub checksum_file: Option<PathBuf>,
    /// Metadata from the first archive entry, or why it could not be read.
    pub metadata: std::result::Result<BackupMetadata, String>,
}

/// How to restore a partition (BAK-4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ImportMode {
    /// Delete everything on the target (except `lost+found`), then extract.
    #[default]
    Replace,
    /// Extract over existing files, archive wins on conflicts.
    Merge,
}

/// Options of [`import`].
#[derive(Clone, Copy, Debug)]
pub struct ImportOptions {
    pub mode: ImportMode,
    /// Verify the `.sha256` file before touching the targets (default true, BAK-4). A missing
    /// checksum file is an error unless this is false.
    pub verify: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            mode: ImportMode::Replace,
            verify: true,
        }
    }
}

/// A problem with one file during import (BAK-5). The import continues.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileError {
    /// Archive path (e.g. `overlay/overlays/etc/upper/passwd`) or target path.
    pub path: String,
    pub error: String,
}

/// Per partition import statistics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartitionImport {
    /// Entries removed in Replace mode.
    pub removed: u64,
    /// Entries extracted (all types, incl. directories).
    pub entries: u64,
    /// Regular file bytes written.
    pub bytes: u64,
}

/// Result of [`import`] (BAK-5 summary).
#[derive(Clone, Debug, Default)]
pub struct ImportReport {
    pub partitions: BTreeMap<PartitionRole, PartitionImport>,
    /// Per file errors, at most [`MAX_REPORTED_ERRORS`]; `error_count` has the total.
    pub errors: Vec<FileError>,
    pub error_count: u64,
}

/// Errors beyond this number are only counted.
pub const MAX_REPORTED_ERRORS: usize = 1000;

impl ImportReport {
    fn error(&mut self, path: impl Into<String>, error: impl ToString) {
        self.error_count += 1;
        if self.errors.len() < MAX_REPORTED_ERRORS {
            self.errors.push(FileError {
                path: path.into(),
                error: error.to_string(),
            });
        }
    }
}

/// Lists backups in `stick_root` and `stick_root/xoros/backups`, newest name first. Reads the
/// metadata entry of each backup (cheap: first few KiB); does not verify checksums.
pub fn list_backups(stick_root: &Path) -> Vec<BackupInfo> {
    let mut out = Vec::new();
    for dir in [stick_root.to_path_buf(), stick_root.join(STICK_BACKUP_DIR)] {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        for n in &names {
            if !n.starts_with(ARCHIVE_PREFIX) {
                continue;
            }
            let base = if n.ends_with(ARCHIVE_SUFFIX) {
                n.clone()
            } else if let Some(b) = n.strip_suffix(".000") {
                if !b.ends_with(ARCHIVE_SUFFIX) || names.iter().any(|x| x == b) {
                    continue; // a plain file of the same name wins
                }
                b.to_string()
            } else {
                continue;
            };
            if let Ok(info) = backup_info(&dir, &base) {
                out.push(info);
            }
        }
    }
    out.sort_by(|a, b| b.name.cmp(&a.name).then(a.dir.cmp(&b.dir)));
    out
}

/// Describes the backup `name` (`*.tar.gz`) in `dir`: finds its parts and checksum file and
/// reads the metadata. Fails only if no archive file exists.
pub fn backup_info(dir: &Path, name: &str) -> Result<BackupInfo> {
    let plain = dir.join(name);
    let mut parts = Vec::new();
    if plain.is_file() {
        parts.push(plain);
    } else {
        for i in 0.. {
            let p = dir.join(part_name(name, i));
            if !p.is_file() {
                break;
            }
            parts.push(p);
        }
    }
    if parts.is_empty() {
        return Err(BackupError::InvalidArgument(format!(
            "no archive {name} in {}",
            dir.display()
        )));
    }
    let total_size = parts
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let cs = dir.join(format!("{name}{CHECKSUM_SUFFIX}"));
    let checksum_file = if cs.is_file() { Some(cs) } else { None };
    let metadata = read_metadata(&parts).map_err(|e| e.to_string());
    Ok(BackupInfo {
        name: name.to_string(),
        dir: dir.to_path_buf(),
        parts,
        total_size,
        checksum_file,
        metadata,
    })
}

/// Opens the archive stream of `parts`.
fn open_archive(
    parts: &[PathBuf],
) -> (
    tar::Archive<MultiGzDecoder<BufReader<MultiPartReader>>>,
    std::rc::Rc<std::cell::Cell<u64>>,
) {
    let r = MultiPartReader::new(parts);
    let counter = r.counter.clone();
    let gz = MultiGzDecoder::new(BufReader::with_capacity(1 << 20, r));
    (tar::Archive::new(gz), counter)
}

const MAX_METADATA_SIZE: u64 = 1 << 20;

fn parse_metadata<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<BackupMetadata> {
    let found = entry.path_bytes();
    if found.as_ref() != METADATA_NAME.as_bytes() {
        return Err(BackupError::Metadata(format!(
            "missing (the first archive entry is `{}`)",
            String::from_utf8_lossy(&found)
        )));
    }
    if entry.size() > MAX_METADATA_SIZE {
        return Err(BackupError::Metadata(format!(
            "unreadable (too large: {} bytes)",
            entry.size()
        )));
    }
    let mut buf = Vec::new();
    entry
        .read_to_end(&mut buf)
        .map_err(|e| BackupError::Metadata(format!("unreadable ({e})")))?;
    let m: BackupMetadata = serde_json::from_slice(&buf)
        .map_err(|e| BackupError::Metadata(format!("unreadable (invalid JSON: {e})")))?;
    if m.format != FORMAT_VERSION {
        return Err(BackupError::Metadata(format!(
            "unsupported format {} (this version reads format {FORMAT_VERSION})",
            m.format
        )));
    }
    Ok(m)
}

/// Reads `xoros-backup.json` (the first entry) of an archive.
pub fn read_metadata(parts: &[PathBuf]) -> Result<BackupMetadata> {
    let (mut ar, _) = open_archive(parts);
    let mut entries = ar
        .entries()
        .map_err(|e| BackupError::Metadata(format!("unreadable ({e})")))?;
    match entries.next() {
        Some(Ok(mut e)) => parse_metadata(&mut e),
        Some(Err(e)) => Err(BackupError::Metadata(format!("unreadable ({e})"))),
        None => Err(BackupError::Metadata(
            "missing (the archive is empty)".into(),
        )),
    }
}

/// Parses a `sha256sum` style file: `<hex>  <name>` or `<hex> *<name>` per line.
/// `Err((line number, line))` for the first malformed line.
fn parse_checksum_file(text: &str) -> std::result::Result<Vec<(String, String)>, (usize, String)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let bad = || (i + 1, line.to_string());
        let (hash, rest) = line.split_at(line.find(' ').ok_or_else(bad)?);
        let name = rest
            .strip_prefix("  ")
            .or_else(|| rest.strip_prefix(" *"))
            .or_else(|| rest.strip_prefix(' '))
            .ok_or_else(bad)?;
        if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) || name.is_empty() {
            return Err(bad());
        }
        // Only plain file names in the same directory.
        let name = name.rsplit('/').next().unwrap_or(name);
        out.push((hash.to_ascii_lowercase(), name.to_string()));
    }
    Ok(out)
}

/// Verifies all parts of `backup` against its `.sha256` file (BAK-4). Every part must be listed
/// and every listed file must be a part of the backup.
pub fn verify(
    backup: &BackupInfo,
    progress: &mut ProgressFn<'_>,
    cancel: &CancelToken,
) -> Result<()> {
    let mut rep = Reporter::new(progress, cancel);
    verify_inner(backup, &mut rep)
}

fn verify_inner(backup: &BackupInfo, rep: &mut Reporter<'_, '_>) -> Result<()> {
    let cs = backup.checksum_file.clone().ok_or_else(|| {
        BackupError::MissingChecksumFile(
            backup.dir.join(format!("{}{CHECKSUM_SUFFIX}", backup.name)),
        )
    })?;
    let text = fs::read_to_string(&cs).ctx(|| format!("reading {}", cs.display()))?;
    let list = parse_checksum_file(&text).map_err(|(line, content)| BackupError::Checksum {
        file: cs.clone(),
        problem: ChecksumProblem::Malformed { line, content },
    })?;
    let part_names: Vec<String> = backup
        .parts
        .iter()
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();
    for (_, name) in &list {
        if !part_names.contains(name) {
            return Err(BackupError::Checksum {
                file: backup.dir.join(name),
                problem: ChecksumProblem::ListedButMissing,
            });
        }
    }
    rep.phase(Phase::Verifying, backup.total_size);
    let mut done = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    for (path, name) in backup.parts.iter().zip(&part_names) {
        let expected = list
            .iter()
            .find(|(_, n)| n == name)
            .map(|(h, _)| h.clone())
            .ok_or_else(|| BackupError::Checksum {
                file: path.clone(),
                problem: ChecksumProblem::NotListed,
            })?;
        rep.entry(name);
        let mut f = File::open(path).ctx(|| format!("opening {}", path.display()))?;
        let mut h = Sha256::new();
        loop {
            rep.check()?;
            let n = match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(BackupError::io_path("reading", path, e)),
            };
            h.update(&buf[..n]);
            done += n as u64;
            rep.set_bytes(done);
        }
        let actual = hex(&h.finalize());
        if actual != expected {
            return Err(BackupError::Checksum {
                file: path.clone(),
                problem: ChecksumProblem::Mismatch { expected, actual },
            });
        }
    }
    rep.flush();
    Ok(())
}

/// Splits an archive path into role and normalized relative path. `Err` for unsafe paths.
fn split_archive_path(p: &[u8]) -> std::result::Result<Option<(PartitionRole, Vec<u8>)>, String> {
    if p.starts_with(b"/") {
        return Err("absolute path in archive".into());
    }
    let mut comps = p
        .split(|c| *c == b'/')
        .filter(|c| !c.is_empty() && *c != b".");
    let first = match comps.next() {
        Some(f) => f,
        None => return Err("empty path".into()),
    };
    let role = match std::str::from_utf8(first)
        .ok()
        .and_then(PartitionRole::from_name)
    {
        Some(r) => r,
        None => return Ok(None),
    };
    let mut rel = Vec::new();
    for c in comps {
        if c == b".." {
            return Err("path contains '..'".into());
        }
        if !rel.is_empty() {
            rel.push(b'/');
        }
        rel.extend_from_slice(c);
    }
    Ok(Some((role, rel)))
}

fn rel_path(rel: &[u8]) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(rel))
}

/// Everything about one archive entry, with pax records and GNU long names applied.
struct EntryMeta {
    path: Vec<u8>,
    link: Option<Vec<u8>>,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: (i64, u32),
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

type PaxRecords = Vec<(Vec<u8>, Vec<u8>)>;

/// Parses pax extended header data (`"<len> <key>=<value>\n"` records). Values may contain any
/// byte incl. newlines (xattrs); the tar crate's own parser splits at newlines, so it is not used.
fn parse_pax(data: &[u8]) -> std::result::Result<PaxRecords, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        if data[pos..].iter().all(|b| *b == 0) {
            break; // padding
        }
        let sp = data[pos..]
            .iter()
            .position(|b| *b == b' ')
            .ok_or("malformed pax record")?;
        let len: usize = std::str::from_utf8(&data[pos..pos + sp])
            .ok()
            .and_then(|l| l.parse().ok())
            .ok_or("malformed pax record length")?;
        if len <= sp + 1 || pos + len > data.len() || data[pos + len - 1] != b'\n' {
            return Err("malformed pax record".into());
        }
        let rec = &data[pos + sp + 1..pos + len - 1];
        let eq = rec
            .iter()
            .position(|b| *b == b'=')
            .ok_or("malformed pax record")?;
        out.push((rec[..eq].to_vec(), rec[eq + 1..].to_vec()));
        pos += len;
    }
    Ok(out)
}

fn strip_nul(mut v: Vec<u8>) -> Vec<u8> {
    while v.last() == Some(&0) {
        v.pop();
    }
    v
}

fn entry_meta(
    h: &tar::Header,
    pax: PaxRecords,
    long_name: Option<Vec<u8>>,
    long_link: Option<Vec<u8>>,
) -> std::result::Result<EntryMeta, String> {
    let e = |e: io::Error| e.to_string();
    let mut m = EntryMeta {
        path: long_name.unwrap_or_else(|| h.path_bytes().into_owned()),
        link: long_link.or_else(|| h.link_name_bytes().map(|l| l.into_owned())),
        mode: h.mode().map_err(e)? & 0o7777,
        uid: h.uid().map_err(e)? as u32,
        gid: h.gid().map_err(e)? as u32,
        mtime: (h.mtime().map_err(e)? as i64, 0),
        xattrs: Vec::new(),
    };
    let num = |v: &[u8]| {
        std::str::from_utf8(v)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
    };
    for (key, value) in pax {
        match key.as_slice() {
            b"path" => m.path = value,
            b"linkpath" => m.link = Some(value),
            b"uid" => m.uid = num(&value).ok_or("bad pax uid")? as u32,
            b"gid" => m.gid = num(&value).ok_or("bad pax gid")? as u32,
            b"mtime" => m.mtime = parse_pax_time(&value).ok_or("bad pax mtime")?,
            b"size" => {
                if num(&value) != Some(h.entry_size().map_err(e)?) {
                    return Err("pax size differs from header size (unsupported)".into());
                }
            }
            _ => {
                if let Some(name) = key.strip_prefix(b"SCHILY.xattr.") {
                    m.xattrs.push((name.to_vec(), value));
                }
            }
        }
    }
    Ok(m)
}

fn parse_pax_time(s: &[u8]) -> Option<(i64, u32)> {
    let s = std::str::from_utf8(s).ok()?;
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    let sec: i64 = int.parse().ok()?;
    let mut nsec: u32 = 0;
    if !frac.is_empty() {
        if !frac.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let digits: String = frac.chars().chain(std::iter::repeat('0')).take(9).collect();
        nsec = digits.parse().ok()?;
    }
    if !neg {
        Some((sec, nsec))
    } else if nsec == 0 {
        Some((-sec, 0))
    } else {
        Some((-sec - 1, 1_000_000_000 - nsec))
    }
}

struct Target<'p> {
    role: PartitionRole,
    root: &'p Path,
}

struct DirFixup {
    path: PathBuf,
    mode: u32,
    mtime: (i64, u32),
}

/// Restores the selected partitions from `backup` into the mounted (read-write) target roots.
///
/// The caller runs `e2fsck -p` before and shows the summary after (BAK-5, see
/// [`super::tools`]). Order: verify checksums (if enabled) -> clear targets (Replace) -> extract
/// -> recreate empty `overlays/etc/work` on overlay -> `syncfs`. Errors on single files are
/// collected in the report; archive corruption, cancellation and target root errors abort.
/// After an abort in the extract phase the target is partially restored.
pub fn import(
    backup: &BackupInfo,
    targets: &[(PartitionRole, &Path)],
    opts: &ImportOptions,
    progress: &mut ProgressFn<'_>,
    cancel: &CancelToken,
) -> Result<ImportReport> {
    if targets.is_empty() {
        return Err(BackupError::InvalidArgument("no partition selected".into()));
    }
    // `BackupInfo::metadata` holds the full message of a `BackupError::Metadata`.
    let metadata = backup
        .metadata
        .clone()
        .map_err(BackupError::InvalidArgument)?;
    let mut tg: Vec<Target<'_>> = Vec::new();
    for (role, root) in targets {
        if !metadata.partitions.contains_key(role.as_str()) {
            return Err(BackupError::InvalidArgument(format!(
                "the backup does not contain {role}"
            )));
        }
        if tg.iter().any(|t| t.role == *role) {
            return Err(BackupError::InvalidArgument(format!(
                "partition {role} given twice"
            )));
        }
        let md = fs::symlink_metadata(root).ctx(|| format!("target {}", root.display()))?;
        if !md.is_dir() {
            return Err(BackupError::InvalidArgument(format!(
                "{} is not a directory",
                root.display()
            )));
        }
        tg.push(Target { role: *role, root });
    }
    let mut rep = Reporter::new(progress, cancel);
    if opts.verify {
        verify_inner(backup, &mut rep)?;
    }
    let mut report = ImportReport::default();
    for t in &tg {
        report.partitions.insert(t.role, PartitionImport::default());
    }

    if opts.mode == ImportMode::Replace {
        rep.phase(Phase::Clearing, 0);
        for t in &tg {
            rep.check()?;
            rep.entry(t.role.as_str());
            let mut errs = Vec::new();
            let removed = fsops::clear_dir(t.root, replace_keep(t.role), &mut errs, cancel)
                .ctx(|| format!("clearing {}", t.root.display()))?;
            rep.check()?;
            for (p, e) in errs {
                report.error(p, e);
            }
            report
                .partitions
                .get_mut(&t.role)
                .expect("inserted")
                .removed = removed;
        }
    }

    rep.phase(Phase::Importing, backup.total_size);
    let (mut ar, counter) = open_archive(&backup.parts);
    let mut fixups: Vec<DirFixup> = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    // Raw iteration: pax headers and GNU long names are applied here (see `parse_pax`).
    let entries = ar
        .entries()
        .map_err(|e| BackupError::Format(e.to_string()))?
        .raw(true);
    let fmt = |e: io::Error| BackupError::Format(e.to_string());
    let mut first = true;
    let mut pax: Option<PaxRecords> = None;
    let mut long_name: Option<Vec<u8>> = None;
    let mut long_link: Option<Vec<u8>> = None;
    for entry in entries {
        rep.check()?;
        let mut entry = entry.map_err(fmt)?;
        let etype = entry.header().entry_type();
        match etype {
            EntryType::XHeader => {
                let mut d = Vec::new();
                entry.read_to_end(&mut d).map_err(fmt)?;
                pax = Some(parse_pax(&d).map_err(BackupError::Format)?);
                continue;
            }
            EntryType::XGlobalHeader => continue,
            EntryType::GNULongName | EntryType::GNULongLink => {
                let mut d = Vec::new();
                entry.read_to_end(&mut d).map_err(fmt)?;
                if etype == EntryType::GNULongName {
                    long_name = Some(strip_nul(d));
                } else {
                    long_link = Some(strip_nul(d));
                }
                continue;
            }
            _ => {}
        }
        if first {
            first = false;
            parse_metadata(&mut entry)?;
            pax = None;
            continue;
        }
        let meta = entry_meta(
            entry.header(),
            pax.take().unwrap_or_default(),
            long_name.take(),
            long_link.take(),
        );
        let meta = match meta {
            Ok(m) => m,
            Err(e) => {
                report.error(
                    String::from_utf8_lossy(&entry.header().path_bytes()).into_owned(),
                    e,
                );
                continue;
            }
        };
        let apath_s = String::from_utf8_lossy(&meta.path).into_owned();
        rep.entry(&apath_s);
        rep.set_bytes(counter.get());
        let (role, rel) = match split_archive_path(&meta.path) {
            Ok(Some(x)) => x,
            Ok(None) => continue, // unknown top-level entry, ignored
            Err(e) => {
                report.error(apath_s, e);
                continue;
            }
        };
        let target = match tg.iter().find(|t| t.role == role) {
            Some(t) => t,
            None => continue,
        };
        if role.is_excluded(&rel) {
            continue;
        }
        if keeps_current_rescue_dir(role, &rel) {
            continue;
        }
        match extract_entry(
            &mut entry,
            meta,
            target,
            &tg,
            &rel,
            &mut fixups,
            &mut buf,
            &mut rep,
        ) {
            Ok(bytes) => {
                let s = report.partitions.get_mut(&role).expect("inserted");
                s.entries += 1;
                s.bytes += bytes;
            }
            Err(ExtractError::Fatal(e)) => return Err(e),
            Err(ExtractError::File(e)) => report.error(apath_s, e),
        }
    }
    rep.set_bytes(counter.get());

    // Empty overlayfs work dir (BAK-4); before the directory fix-ups so overlays/etc keeps its
    // archived mtime.
    for t in &tg {
        if t.role == PartitionRole::Overlay {
            if let Err(e) = recreate_empty_dir(t.root, OVERLAY_WORK_DIR) {
                report.error(t.root.join(OVERLAY_WORK_DIR).display().to_string(), e);
            }
        }
    }

    // Directory permissions and times last: children changed the mtimes, and a read-only
    // directory mode would have blocked creating its children.
    for f in fixups.iter().rev() {
        if let Err(e) = fsops::chmod(&f.path, f.mode) {
            report.error(f.path.display().to_string(), format!("chmod: {e}"));
        }
        if let Err(e) = fsops::set_mtime_nofollow(&f.path, f.mtime.0, f.mtime.1) {
            report.error(f.path.display().to_string(), format!("set mtime: {e}"));
        }
    }

    rep.phase(Phase::Syncing, 0);
    for t in &tg {
        fsops::syncfs(t.root).ctx(|| format!("sync {}", t.root.display()))?;
    }
    rep.flush();
    Ok(report)
}

/// Top-level entries of a target partition that Replace mode keeps.
///
/// Rescue settings policy: on `overlay` the rescue shell's own `rescue/` directory (CFG-1) holds
/// the settings of the running rescue system, not of the normal system. An import therefore keeps
/// the *current* `rescue/` directory on the overlay in both modes: Replace does not delete it and
/// neither mode restores the archived one (see [`keeps_current_rescue_dir`]). Export still
/// archives `rescue/`, so a backup is complete (BAK-4).
fn replace_keep(role: PartitionRole) -> &'static [&'static str] {
    match role {
        PartitionRole::Overlay => &[LOST_FOUND, RESCUE_DIR],
        PartitionRole::Application => &[LOST_FOUND],
    }
}

/// True for archive paths below `overlay/rescue` (including the directory itself), which an
/// import skips so the current rescue settings stay (see [`replace_keep`]).
fn keeps_current_rescue_dir(role: PartitionRole, rel: &[u8]) -> bool {
    let d = RESCUE_DIR.as_bytes();
    role == PartitionRole::Overlay
        && (rel == d || (rel.len() > d.len() && rel.starts_with(d) && rel[d.len()] == b'/'))
}

/// Removes `root/rel` if it exists and creates it as an empty directory (mode 0755, parents
/// 0755). Refuses to go through symlinks.
pub(crate) fn recreate_empty_dir(root: &Path, rel: &str) -> io::Result<()> {
    let path = root.join(rel);
    let dev = fs::symlink_metadata(root)?.dev();
    let mut errs = Vec::new();
    fsops::remove_tree(&path, dev, &mut errs, &CancelToken::new());
    if let Some((p, e)) = errs.into_iter().next() {
        return Err(io::Error::other(format!("{p}: {e}")));
    }
    ensure_parents(root, rel.as_bytes())?;
    fs::create_dir(&path)?;
    fsops::chmod(&path, 0o755)
}

enum ExtractError {
    Fatal(BackupError),
    File(String),
}

impl From<io::Error> for ExtractError {
    fn from(e: io::Error) -> Self {
        ExtractError::File(e.to_string())
    }
}

/// Makes sure every ancestor of `rel` below `root` is a real directory (never a symlink),
/// creating missing ones with mode 0755.
fn ensure_parents(root: &Path, rel: &[u8]) -> io::Result<()> {
    let comps: Vec<&[u8]> = rel.split(|c| *c == b'/').collect();
    let mut p = root.to_path_buf();
    for c in &comps[..comps.len().saturating_sub(1)] {
        p.push(std::ffi::OsStr::from_bytes(c));
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::other(format!(
                    "parent {} is not a directory",
                    p.display()
                )))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&p)?;
                fsops::chmod(&p, 0o755)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Removes whatever is at `path` unless it is a directory and the new entry is one too.
/// Returns true if a directory is kept.
fn clear_dest(path: &Path, new_is_dir: bool) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => {
            if new_is_dir {
                Ok(true)
            } else {
                fs::remove_dir_all(path)?;
                Ok(false)
            }
        }
        Ok(_) => {
            fs::remove_file(path)?;
            Ok(false)
        }
    }
}

fn chown_if_needed(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    if m.uid() != uid || m.gid() != gid {
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    }
    Ok(())
}

fn set_xattrs_path(path: &Path, xattrs: &[(Vec<u8>, Vec<u8>)], errs: &mut Vec<String>) {
    for (n, v) in xattrs {
        if let Err(e) = fsops::set_xattr(path, n, v) {
            errs.push(format!("xattr {}: {e}", String::from_utf8_lossy(n)));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_entry<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    meta: EntryMeta,
    target: &Target<'_>,
    all: &[Target<'_>],
    rel: &[u8],
    fixups: &mut Vec<DirFixup>,
    buf: &mut [u8],
    rep: &mut Reporter<'_, '_>,
) -> std::result::Result<u64, ExtractError> {
    let dest = if rel.is_empty() {
        target.root.to_path_buf()
    } else {
        target.root.join(rel_path(rel))
    };
    let etype = entry.header().entry_type();
    let is_dir = etype == EntryType::Directory;
    if rel.is_empty() && !is_dir {
        return Err(ExtractError::File(
            "partition root entry is not a directory".into(),
        ));
    }
    if !rel.is_empty() {
        ensure_parents(target.root, rel)?;
    }
    // Replace mode emptied the target before; in both modes an existing entry is replaced, an
    // existing directory is kept if the archive has a directory there.
    let keep_dir = if rel.is_empty() {
        true
    } else {
        clear_dest(&dest, is_dir)?
    };
    let mut errs: Vec<String> = Vec::new();
    let mut bytes = 0;

    match etype {
        EntryType::Regular | EntryType::Continuous => {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&dest)?;
            loop {
                let n = match entry.read(buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(ExtractError::Fatal(BackupError::Format(e.to_string()))),
                };
                f.write_all(&buf[..n])?;
                bytes += n as u64;
                if rep.cancel.is_cancelled() {
                    return Err(ExtractError::Fatal(BackupError::Cancelled));
                }
            }
            let m = f.metadata()?;
            if m.uid() != meta.uid || m.gid() != meta.gid {
                std::os::unix::fs::fchown(&f, Some(meta.uid), Some(meta.gid))?;
            }
            // chmod after chown (chown clears setuid/setgid), xattrs after chown (chown clears
            // security.capability).
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(fs::Permissions::from_mode(meta.mode))?;
            }
            for (n, v) in &meta.xattrs {
                if let Err(e) = fsops::fset_xattr(&f, n, v) {
                    errs.push(format!("xattr {}: {e}", String::from_utf8_lossy(n)));
                }
            }
            fsops::fset_mtime(&f, meta.mtime.0, meta.mtime.1)?;
        }
        EntryType::Directory => {
            if !keep_dir {
                fs::create_dir(&dest)?;
                fsops::chmod(&dest, 0o700)?;
            }
            chown_if_needed(&dest, meta.uid, meta.gid)?;
            set_xattrs_path(&dest, &meta.xattrs, &mut errs);
            fixups.push(DirFixup {
                path: dest.clone(),
                mode: meta.mode,
                mtime: meta.mtime,
            });
        }
        EntryType::Symlink => {
            let target_name = meta
                .link
                .clone()
                .ok_or_else(|| ExtractError::File("symlink without target".into()))?;
            std::os::unix::fs::symlink(std::ffi::OsString::from_vec(target_name), &dest)?;
            chown_if_needed(&dest, meta.uid, meta.gid)?;
            set_xattrs_path(&dest, &meta.xattrs, &mut errs);
            fsops::set_mtime_nofollow(&dest, meta.mtime.0, meta.mtime.1)?;
        }
        EntryType::Link => {
            let link = meta
                .link
                .clone()
                .ok_or_else(|| ExtractError::File("hard link without target".into()))?;
            let (lrole, lrel) = split_archive_path(&link)
                .map_err(ExtractError::File)?
                .ok_or_else(|| {
                    ExtractError::File("hard link target outside the partitions".into())
                })?;
            if lrole != target.role || lrel.is_empty() {
                return Err(ExtractError::File("hard link across partitions".into()));
            }
            let src_root = all
                .iter()
                .find(|t| t.role == lrole)
                .map(|t| t.root)
                .unwrap_or(target.root);
            let src = src_root.join(rel_path(&lrel));
            ensure_parents(src_root, &lrel)?;
            fs::hard_link(&src, &dest).map_err(|e| {
                ExtractError::File(format!(
                    "hard link to {}: {e}",
                    String::from_utf8_lossy(&link)
                ))
            })?;
            // Metadata belongs to the inode, already set from the first name.
        }
        EntryType::Char | EntryType::Block | EntryType::Fifo => {
            let (kind, major, minor) = match etype {
                EntryType::Char => (
                    libc::S_IFCHR,
                    entry.header().device_major()?.unwrap_or(0),
                    entry.header().device_minor()?.unwrap_or(0),
                ),
                EntryType::Block => (
                    libc::S_IFBLK,
                    entry.header().device_major()?.unwrap_or(0),
                    entry.header().device_minor()?.unwrap_or(0),
                ),
                _ => (libc::S_IFIFO, 0, 0),
            };
            fsops::mknod(&dest, kind | 0o600, major, minor)?;
            chown_if_needed(&dest, meta.uid, meta.gid)?;
            fsops::chmod(&dest, meta.mode)?;
            set_xattrs_path(&dest, &meta.xattrs, &mut errs);
            fsops::set_mtime_nofollow(&dest, meta.mtime.0, meta.mtime.1)?;
        }
        EntryType::XGlobalHeader => return Ok(0),
        other => {
            return Err(ExtractError::File(format!(
                "unsupported entry type {other:?}"
            )))
        }
    }
    if errs.is_empty() {
        Ok(bytes)
    } else {
        Err(ExtractError::File(errs.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_paths() {
        assert_eq!(
            split_archive_path(b"overlay/").unwrap(),
            Some((PartitionRole::Overlay, vec![]))
        );
        assert_eq!(
            split_archive_path(b"./application//a/./b").unwrap(),
            Some((PartitionRole::Application, b"a/b".to_vec()))
        );
        assert_eq!(split_archive_path(b"xoros-backup.json").unwrap(), None);
        assert!(split_archive_path(b"overlay/../x").is_err());
        assert!(split_archive_path(b"/overlay/x").is_err());
    }

    #[test]
    fn pax_records() {
        let data = b"29 SCHILY.xattr.user.a=x\ny\n=\n12 path=a/b\n\0\0";
        let r = parse_pax(data).unwrap();
        assert_eq!(
            r,
            vec![
                (b"SCHILY.xattr.user.a".to_vec(), b"x\ny\n=".to_vec()),
                (b"path".to_vec(), b"a/b".to_vec())
            ]
        );
        assert!(parse_pax(b"99 path=x\n").is_err());
        assert!(parse_pax(b"x path=x\n").is_err());
    }

    #[test]
    fn pax_times() {
        assert_eq!(parse_pax_time(b"12"), Some((12, 0)));
        assert_eq!(parse_pax_time(b"12.5"), Some((12, 500_000_000)));
        assert_eq!(parse_pax_time(b"12.000000001"), Some((12, 1)));
        assert_eq!(parse_pax_time(b"-1.5"), Some((-2, 500_000_000)));
        assert_eq!(parse_pax_time(b"x"), None);
    }

    #[test]
    fn checksum_lines() {
        let h = "a".repeat(64);
        let l = parse_checksum_file(&format!("{h}  x.tar.gz.000\n{h} *y\n\n")).unwrap();
        assert_eq!(
            l,
            vec![(h.clone(), "x.tar.gz.000".into()), (h.clone(), "y".into())]
        );
        assert!(parse_checksum_file("zz  x\n").is_err());
    }
}
