//! Export of persistent partitions into a pax tar.gz archive (BAK-1, BAK-2, BAK-3).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use flate2::write::GzEncoder;
use flate2::Compression;
use tar::{EntryType, Header};

use super::error::{BackupError, IoContext, Result};
use super::fsops;
use super::meta::{
    archive_name, format_created, BackupMetadata, PartitionRole, PartitionStats, SlotMeta,
    CHECKSUM_SUFFIX, FAT_MAX_PART_SIZE, FORMAT_VERSION, METADATA_NAME,
};
use super::progress::{check_cancel, CancelToken, Phase, ProgressFn, Reporter};
use super::split::{sync_dir, PartInfo, SplitWriter};

/// Options of [`export`].
#[derive(Clone, Debug)]
pub struct ExportOptions {
    /// Device host name, used in the archive name.
    pub hostname: String,
    /// `MACHINE` of the device (metadata).
    pub machine: String,
    /// Slot build info for the metadata, keys `ROOTFS_A`/`ROOTFS_B`.
    pub slots: BTreeMap<String, Option<SlotMeta>>,
    /// Rescue shell version (metadata `app_version`).
    pub app_version: String,
    /// Split the archive into parts of at most this many bytes (BAK-3). Use
    /// [`FAT_MAX_PART_SIZE`] on vfat, see [`split_size_for`].
    pub split_size: Option<u64>,
    /// gzip level 0..=9 (default 6).
    pub compression_level: u32,
    /// Creation time; `None` = now. Determines the archive name.
    pub time: Option<DateTime<Utc>>,
}

impl ExportOptions {
    pub fn new(hostname: &str, machine: &str) -> Self {
        ExportOptions {
            hostname: hostname.to_string(),
            machine: machine.to_string(),
            slots: BTreeMap::new(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            split_size: None,
            compression_level: 6,
            time: None,
        }
    }
}

/// Result of a successful [`export`].
#[derive(Clone, Debug)]
pub struct ExportReport {
    /// `xoros-backup-<host>-<time>.tar.gz`.
    pub archive_name: String,
    /// The written archive file(s) in order with size and SHA-256.
    pub parts: Vec<PartInfo>,
    /// The `.sha256` sidecar file.
    pub checksum_file: PathBuf,
    /// The metadata stored as first entry.
    pub metadata: BackupMetadata,
    /// Non fatal problems (unreadable files, sockets, mount points skipped, ...).
    pub warnings: Vec<String>,
}

impl ExportReport {
    pub fn compressed_bytes(&self) -> u64 {
        self.parts.iter().map(|p| p.size).sum()
    }
}

/// Result of [`check_space`] (BAK-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpaceCheck {
    /// Used space of the selected partitions (upper bound of the uncompressed size).
    pub needed: u64,
    /// Free space on the destination.
    pub available: u64,
    /// Destination is vfat: the archive must be split ([`FAT_MAX_PART_SIZE`]).
    pub needs_split: bool,
}

impl SpaceCheck {
    /// Warn the user, but allow continuing (the archive is compressed).
    pub fn may_not_fit(&self) -> bool {
        self.needed > self.available
    }
}

const MSDOS_SUPER_MAGIC: i64 = 0x4d44;

/// Compares the used space of the mounted source partitions with the free space of `dest_dir`.
pub fn check_space(sources: &[(PartitionRole, &Path)], dest_dir: &Path) -> Result<SpaceCheck> {
    let mut needed = 0;
    for (_, root) in sources {
        needed += fsops::fs_space(root)
            .ctx(|| format!("statvfs {}", root.display()))?
            .used;
    }
    let available = fsops::fs_space(dest_dir)
        .ctx(|| format!("statvfs {}", dest_dir.display()))?
        .available;
    Ok(SpaceCheck {
        needed,
        available,
        needs_split: split_size_for(dest_dir)?.is_some(),
    })
}

/// `Some(FAT_MAX_PART_SIZE)` if `dest_dir` is on vfat, else `None`.
pub fn split_size_for(dest_dir: &Path) -> Result<Option<u64>> {
    let t = fsops::fs_type(dest_dir).ctx(|| format!("statfs {}", dest_dir.display()))?;
    Ok(if t == MSDOS_SUPER_MAGIC {
        Some(FAT_MAX_PART_SIZE)
    } else {
        None
    })
}

/// Writes a backup of the given mounted partition roots into `dest_dir` (BAK-1..3).
///
/// The sources should be mounted read-only by the caller. The archive is streamed (NF-5): two
/// passes over the source tree (count, then archive), only the hard link table and one directory
/// listing are held in memory. On error or cancellation, all files created so far are deleted.
pub fn export(
    sources: &[(PartitionRole, &Path)],
    dest_dir: &Path,
    opts: &ExportOptions,
    progress: &mut ProgressFn<'_>,
    cancel: &CancelToken,
) -> Result<ExportReport> {
    if sources.is_empty() {
        return Err(BackupError::InvalidArgument("no partition selected".into()));
    }
    let mut seen = HashSet::new();
    let mut sources: Vec<(PartitionRole, &Path)> = sources.to_vec();
    sources.sort_by_key(|(r, _)| *r);
    for (r, _) in &sources {
        if !seen.insert(*r) {
            return Err(BackupError::InvalidArgument(format!(
                "partition {r} given twice"
            )));
        }
    }
    if opts.compression_level > 9 {
        return Err(BackupError::InvalidArgument(
            "compression level must be 0..9".into(),
        ));
    }
    let mut rep = Reporter::new(progress, cancel);
    let time = opts.time.unwrap_or_else(Utc::now);
    let name = archive_name(&opts.hostname, &time);

    // Pass 1: statistics for the metadata entry and the progress total.
    rep.phase(Phase::Scanning, 0);
    let mut partitions = BTreeMap::new();
    let mut total_bytes = 0;
    for (role, root) in &sources {
        let stats = scan(*role, root, &mut rep)?;
        total_bytes += stats.bytes;
        partitions.insert(role.as_str().to_string(), stats);
    }
    let metadata = BackupMetadata {
        format: FORMAT_VERSION,
        created: format_created(&time),
        machine: opts.machine.clone(),
        slots: opts.slots.clone(),
        partitions,
        app_version: opts.app_version.clone(),
    };

    // Pass 2: the archive.
    let writer = SplitWriter::new(dest_dir, &name, opts.split_size)
        .ctx(|| format!("creating archive in {}", dest_dir.display()))?;
    let created = writer.created.clone();
    let checksum_file = dest_dir.join(format!("{name}{CHECKSUM_SUFFIX}"));
    let mut warnings = Vec::new();

    let result = (|| -> Result<Vec<PartInfo>> {
        let gz = GzEncoder::new(writer, Compression::new(opts.compression_level));
        let mut builder = tar::Builder::new(gz);
        rep.phase(Phase::Exporting, total_bytes);
        append_metadata(&mut builder, &metadata, &time).map_err(|e| write_err(e, cancel))?;
        for (role, root) in &sources {
            let mut links = HashMap::new();
            walk(
                *role,
                root,
                cancel,
                &mut warnings,
                &mut |abs, rel, md, warnings| {
                    append_entry(
                        &mut builder,
                        *role,
                        abs,
                        rel,
                        md,
                        &mut links,
                        &mut rep,
                        warnings,
                    )
                },
            )?;
        }
        let gz = builder.into_inner().map_err(|e| write_err(e, cancel))?;
        let writer = gz.finish().map_err(|e| write_err(e, cancel))?;
        let parts = writer.finish().map_err(|e| write_err(e, cancel))?;
        check_cancel(cancel)?;
        write_checksum_file(&checksum_file, &parts)?;
        Ok(parts)
    })();

    match result {
        Ok(parts) => {
            rep.flush();
            Ok(ExportReport {
                archive_name: name,
                parts,
                checksum_file,
                metadata,
                warnings,
            })
        }
        Err(e) => {
            for p in created.borrow().iter() {
                let _ = fs::remove_file(p);
            }
            if !matches!(&e, BackupError::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists)
            {
                let _ = fs::remove_file(&checksum_file);
            }
            Err(e)
        }
    }
}

fn write_err(e: io::Error, cancel: &CancelToken) -> BackupError {
    if cancel.is_cancelled() {
        BackupError::Cancelled
    } else {
        BackupError::io("writing archive", e)
    }
}

fn write_checksum_file(path: &Path, parts: &[PartInfo]) -> Result<()> {
    let mut s = String::new();
    for p in parts {
        let fname = p
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        s.push_str(&format!("{}  {}\n", p.sha256, fname));
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .ctx(|| format!("creating {}", path.display()))?;
    f.write_all(s.as_bytes())
        .and_then(|_| f.sync_all())
        .ctx(|| format!("writing {}", path.display()))?;
    if let Some(d) = path.parent() {
        sync_dir(d).ctx(|| format!("sync {}", d.display()))?;
    }
    Ok(())
}

type Visit<'v> = dyn FnMut(&Path, &[u8], &fs::Metadata, &mut Vec<String>) -> Result<()> + 'v;

/// Depth-first walk in sorted order; `rel` is the `/`-separated path relative to `root` (empty
/// for the root itself). Excluded paths and other file systems are skipped.
fn walk(
    role: PartitionRole,
    root: &Path,
    cancel: &CancelToken,
    warnings: &mut Vec<String>,
    visit: &mut Visit<'_>,
) -> Result<()> {
    let md = fs::symlink_metadata(root).ctx(|| format!("reading {}", root.display()))?;
    if !md.is_dir() {
        return Err(BackupError::InvalidArgument(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    visit(root, b"", &md, warnings)?;
    walk_dir(role, root, b"", md.dev(), cancel, warnings, visit)
}

fn walk_dir(
    role: PartitionRole,
    dir: &Path,
    rel: &[u8],
    dev: u64,
    cancel: &CancelToken,
    warnings: &mut Vec<String>,
    visit: &mut Visit<'_>,
) -> Result<()> {
    check_cancel(cancel)?;
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            warnings.push(format!("{}: cannot list directory: {e}", dir.display()));
            return Ok(());
        }
    };
    let mut names: Vec<OsString> = Vec::new();
    for ent in rd {
        match ent {
            Ok(e) => names.push(e.file_name()),
            Err(e) => warnings.push(format!("{}: cannot list directory: {e}", dir.display())),
        }
    }
    names.sort();
    for name in names {
        check_cancel(cancel)?;
        let mut child_rel = Vec::with_capacity(rel.len() + 1 + name.len());
        if !rel.is_empty() {
            child_rel.extend_from_slice(rel);
            child_rel.push(b'/');
        }
        child_rel.extend_from_slice(name.as_bytes());
        if role.is_excluded(&child_rel) {
            continue;
        }
        let abs = dir.join(&name);
        let md = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) => {
                warnings.push(format!("{}: {e}", abs.display()));
                continue;
            }
        };
        if md.is_dir() && md.dev() != dev {
            warnings.push(format!("{}: mount point, skipped", abs.display()));
            continue;
        }
        visit(&abs, &child_rel, &md, warnings)?;
        if md.is_dir() {
            walk_dir(role, &abs, &child_rel, dev, cancel, warnings, visit)?;
        }
    }
    Ok(())
}

fn scan(role: PartitionRole, root: &Path, rep: &mut Reporter<'_, '_>) -> Result<PartitionStats> {
    let mut stats = PartitionStats::default();
    let mut inodes = HashSet::new();
    let mut ignored = Vec::new();
    let cancel = rep.cancel.clone();
    walk(role, root, &cancel, &mut ignored, &mut |_, rel, md, _| {
        let ft = md.file_type();
        if ft.is_dir() {
            return Ok(());
        }
        if ft.is_socket() {
            return Ok(()); // not archived
        }
        stats.files += 1;
        if ft.is_file() && (md.nlink() <= 1 || inodes.insert((md.dev(), md.ino()))) {
            stats.bytes += md.size();
        }
        if stats.files % 256 == 0 {
            rep.entry(&String::from_utf8_lossy(rel));
        }
        Ok(())
    })?;
    Ok(stats)
}

/// Archive path of `rel` inside `role`: `overlay/` for the root, `overlay/<rel>` otherwise.
fn archive_path(role: PartitionRole, rel: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(role.as_str().len() + 1 + rel.len());
    p.extend_from_slice(role.as_str().as_bytes());
    p.push(b'/');
    p.extend_from_slice(rel);
    p
}

type Pax = Vec<(String, Vec<u8>)>;

fn bytes_path(b: &[u8]) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(b))
}

/// ustar name if it fits, else a pax `path` record plus a truncated ustar name.
fn set_path(h: &mut Header, path: &[u8], pax: &mut Pax) -> io::Result<()> {
    if path.len() < 100 && path.is_ascii() && h.set_path(bytes_path(path)).is_ok() {
        return Ok(());
    }
    pax.push(("path".into(), path.to_vec()));
    let tail: Vec<u8> = path
        .iter()
        .rev()
        .take(99)
        .rev()
        .copied()
        .filter(|c| *c != b'/')
        .collect();
    let tail = if tail.is_empty() {
        b"long-name".to_vec()
    } else {
        tail
    };
    h.set_path(bytes_path(&tail))
}

fn set_link_name(h: &mut Header, target: &[u8], pax: &mut Pax) -> io::Result<()> {
    if target.len() < 100 && target.is_ascii() && h.set_link_name_literal(target).is_ok() {
        return Ok(());
    }
    pax.push(("linkpath".into(), target.to_vec()));
    h.set_link_name_literal(b"long-link")
}

/// pax time value `sec[.nnnnnnnnn]`, also for times before 1970.
pub(crate) fn format_pax_time(sec: i64, nsec: u32) -> String {
    if nsec == 0 {
        sec.to_string()
    } else if sec < 0 {
        // sec + nsec/1e9 is negative: print -(|sec| - 1).(1e9 - nsec)
        format!("-{}.{:09}", -(sec + 1), 1_000_000_000 - nsec)
    } else {
        format!("{sec}.{nsec:09}")
    }
}

const USTAR_ID_MAX: u64 = 0o7777777;

fn fill_common(h: &mut Header, md: &fs::Metadata, pax: &mut Pax) {
    h.set_mode(md.mode() & 0o7777);
    let (uid, gid) = (md.uid() as u64, md.gid() as u64);
    h.set_uid(uid);
    h.set_gid(gid);
    if uid > USTAR_ID_MAX {
        pax.push(("uid".into(), uid.to_string().into_bytes()));
    }
    if gid > USTAR_ID_MAX {
        pax.push(("gid".into(), gid.to_string().into_bytes()));
    }
    let (sec, nsec) = (md.mtime(), md.mtime_nsec() as u32);
    h.set_mtime(sec.max(0) as u64);
    if sec < 0 || nsec != 0 {
        pax.push(("mtime".into(), format_pax_time(sec, nsec).into_bytes()));
    }
}

fn add_xattrs(abs: &Path, pax: &mut Pax, warnings: &mut Vec<String>) {
    let names = match fsops::list_xattrs(abs) {
        Ok(n) => n,
        Err(e) => {
            warnings.push(format!("{}: cannot list xattrs: {e}", abs.display()));
            return;
        }
    };
    for name in names {
        let key = match String::from_utf8(name.clone()) {
            Ok(k) => format!("SCHILY.xattr.{k}"),
            Err(_) => {
                warnings.push(format!(
                    "{}: xattr with non UTF-8 name skipped",
                    abs.display()
                ));
                continue;
            }
        };
        match fsops::get_xattr(abs, &name) {
            Ok(Some(v)) => pax.push((key, v)),
            Ok(None) => {}
            Err(e) => warnings.push(format!(
                "{}: cannot read xattr {}: {e}",
                abs.display(),
                String::from_utf8_lossy(&name)
            )),
        }
    }
}

fn append_metadata<W: Write>(
    b: &mut tar::Builder<W>,
    m: &BackupMetadata,
    time: &DateTime<Utc>,
) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(m).map_err(io::Error::other)?;
    let mut h = Header::new_ustar();
    h.set_path(METADATA_NAME)?;
    h.set_entry_type(EntryType::Regular);
    h.set_size(json.len() as u64);
    h.set_mode(0o644);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(time.timestamp().max(0) as u64);
    h.set_cksum();
    b.append(&h, json.as_slice())
}

#[allow(clippy::too_many_arguments)]
fn append_entry<W: Write>(
    b: &mut tar::Builder<W>,
    role: PartitionRole,
    abs: &Path,
    rel: &[u8],
    md: &fs::Metadata,
    links: &mut HashMap<(u64, u64), Vec<u8>>,
    rep: &mut Reporter<'_, '_>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let cancel = rep.cancel.clone();
    let werr = |e: io::Error| write_err(e, &cancel);
    let path = archive_path(role, rel);
    let mut pax: Pax = Vec::new();
    let mut h = Header::new_ustar();
    set_path(&mut h, &path, &mut pax).map_err(werr)?;
    fill_common(&mut h, md, &mut pax);
    h.set_size(0);
    let ft = md.file_type();
    rep.entry(&String::from_utf8_lossy(&path));

    // Hard links: every further name of an inode becomes a link entry to the first name.
    let link_key = (md.dev(), md.ino());
    if !ft.is_dir() && md.nlink() > 1 {
        if let Some(first) = links.get(&link_key) {
            h.set_entry_type(EntryType::Link);
            set_link_name(&mut h, first, &mut pax).map_err(werr)?;
            return finish(b, h, pax, io::empty()).map_err(werr);
        }
    }

    if ft.is_file() {
        let file = match File::open(abs) {
            Ok(f) => f,
            Err(e) => {
                warnings.push(format!("{}: cannot open, skipped: {e}", abs.display()));
                return Ok(());
            }
        };
        add_xattrs(abs, &mut pax, warnings);
        h.set_entry_type(EntryType::Regular);
        h.set_size(md.size());
        if md.size() > 0o77777777777 {
            pax.push(("size".into(), md.size().to_string().into_bytes()));
        }
        let mut reader = FileReader {
            file,
            remaining: md.size(),
            failed: None,
            rep,
        };
        finish(b, h, pax, &mut reader).map_err(werr)?;
        if let Some(e) = reader.failed {
            warnings.push(format!(
                "{}: read error, stored zero-filled: {e}",
                abs.display()
            ));
        }
    } else if ft.is_dir() {
        add_xattrs(abs, &mut pax, warnings);
        h.set_entry_type(EntryType::Directory);
        finish(b, h, pax, io::empty()).map_err(werr)?;
    } else if ft.is_symlink() {
        let target = match fs::read_link(abs) {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("{}: cannot read link, skipped: {e}", abs.display()));
                return Ok(());
            }
        };
        add_xattrs(abs, &mut pax, warnings);
        h.set_entry_type(EntryType::Symlink);
        set_link_name(&mut h, target.as_os_str().as_bytes(), &mut pax).map_err(werr)?;
        finish(b, h, pax, io::empty()).map_err(werr)?;
    } else if ft.is_char_device() || ft.is_block_device() || ft.is_fifo() {
        add_xattrs(abs, &mut pax, warnings);
        if ft.is_fifo() {
            h.set_entry_type(EntryType::Fifo);
        } else {
            h.set_entry_type(if ft.is_char_device() {
                EntryType::Char
            } else {
                EntryType::Block
            });
            let rdev = md.rdev();
            h.set_device_major(libc::major(rdev)).map_err(werr)?;
            h.set_device_minor(libc::minor(rdev)).map_err(werr)?;
        }
        finish(b, h, pax, io::empty()).map_err(werr)?;
    } else {
        warnings.push(format!(
            "{}: socket or unknown file type, skipped",
            abs.display()
        ));
        return Ok(());
    }
    if !ft.is_dir() && md.nlink() > 1 {
        links.insert(link_key, path);
    }
    Ok(())
}

fn finish<W: Write, R: Read>(
    b: &mut tar::Builder<W>,
    mut h: Header,
    pax: Pax,
    data: R,
) -> io::Result<()> {
    if !pax.is_empty() {
        append_pax(b, &h, &pax)?;
    }
    h.set_cksum();
    b.append(&h, data)
}

/// Encodes pax records (`"<len> <key>=<value>\n"`, len counts the whole record).
pub(crate) fn encode_pax(records: &Pax) -> Vec<u8> {
    let mut data = Vec::new();
    for (key, value) in records {
        let rest = 1 + key.len() + 1 + value.len() + 1; // ' ' key '=' value '\n'
        let mut len = rest + rest.to_string().len();
        if rest + len.to_string().len() != len {
            len = rest + len.to_string().len();
        }
        data.extend_from_slice(len.to_string().as_bytes());
        data.push(b' ');
        data.extend_from_slice(key.as_bytes());
        data.push(b'=');
        data.extend_from_slice(value);
        data.push(b'\n');
    }
    data
}

/// Writes a pax extended header (`x`) for the entry `h`. Unlike `Builder::append_pax_extensions`
/// the header has a name (`PaxHeaders/<name>`, as GNU tar does): busybox tar treats a header
/// with an empty name as end of archive.
fn append_pax<W: Write>(b: &mut tar::Builder<W>, h: &Header, records: &Pax) -> io::Result<()> {
    let data = encode_pax(records);
    let mut x = Header::new_ustar();
    let name = h.path_bytes();
    let base = name
        .rsplit(|c| *c == b'/')
        .find(|c| !c.is_empty())
        .unwrap_or(b"entry");
    let mut pax_name = b"PaxHeaders/".to_vec();
    pax_name.extend(base.iter().take(80));
    x.set_path(bytes_path(&pax_name))?;
    x.set_entry_type(EntryType::XHeader);
    x.set_size(data.len() as u64);
    x.set_mode(0o644);
    x.set_uid(0);
    x.set_gid(0);
    x.set_mtime(h.mtime().unwrap_or(0));
    x.set_cksum();
    b.append(&x, data.as_slice())
}

/// Reads exactly `remaining` bytes of a file: on a read error or if the file shrank, the rest is
/// zero-filled and the error is kept in `failed`, so the archive stays consistent.
struct FileReader<'r, 'a, 'b> {
    file: File,
    remaining: u64,
    failed: Option<io::Error>,
    rep: &'r mut Reporter<'a, 'b>,
}

impl Read for FileReader<'_, '_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        if self.rep.cancel.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        let want = (buf.len() as u64).min(self.remaining) as usize;
        let n = if self.failed.is_some() {
            0
        } else {
            loop {
                match self.file.read(&mut buf[..want]) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        self.failed = Some(e);
                        break 0;
                    }
                }
            }
        };
        let n = if n == 0 {
            if self.failed.is_none() {
                self.failed = Some(io::Error::new(io::ErrorKind::UnexpectedEof, "file shrank"));
            }
            buf[..want].fill(0);
            want
        } else {
            n
        };
        self.remaining -= n as u64;
        self.rep.add_bytes(n as u64);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::{encode_pax, format_pax_time};

    #[test]
    fn pax_encoding() {
        let r = |k: &str, v: &[u8]| (k.to_string(), v.to_vec());
        assert_eq!(encode_pax(&vec![r("path", b"a/b")]), b"12 path=a/b\n");
        // length crossing a digit boundary: ' ' + "path" + '=' + 91 + '\n' = 98 -> "101 path=..."
        let v = vec![b'x'; 91];
        let e = encode_pax(&vec![r("path", &v)]);
        assert!(e.starts_with(b"101 path="));
        assert_eq!(e.len(), 101);
        let v = vec![b'x'; 2];
        assert_eq!(encode_pax(&vec![r("k", &v)]), b"7 k=xx\n");
    }

    #[test]
    fn pax_time() {
        assert_eq!(format_pax_time(5, 0), "5");
        assert_eq!(format_pax_time(5, 1), "5.000000001");
        assert_eq!(format_pax_time(-2, 500_000_000), "-1.500000000");
        assert_eq!(format_pax_time(-1, 0), "-1");
    }
}
