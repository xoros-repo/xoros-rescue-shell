//! Writing an archive as one or more part files while hashing them (BAK-1, BAK-3), and reading
//! the parts back as one stream.

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use sha2::{Digest, Sha256};

use super::meta::part_name;

/// A finished part file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartInfo {
    pub path: PathBuf,
    pub size: u64,
    /// Lower case hex SHA-256.
    pub sha256: String,
}

struct OpenPart {
    path: PathBuf,
    out: BufWriter<File>,
    hasher: Sha256,
    written: u64,
}

/// `Write` sink that creates `<name>` (no split) or `<name>.000`, `<name>.001`, ... (split), each
/// at most `split_size` bytes, hashing every part. Files are created with `create_new`, so an
/// existing backup is never overwritten. Every created path is recorded in `created` so the caller
/// can delete them on failure.
pub(crate) struct SplitWriter {
    dir: PathBuf,
    name: String,
    split_size: Option<u64>,
    current: Option<OpenPart>,
    parts: Vec<PartInfo>,
    pub(crate) created: Rc<std::cell::RefCell<Vec<PathBuf>>>,
}

const BUF_SIZE: usize = 1 << 20;

impl SplitWriter {
    pub(crate) fn new(dir: &Path, name: &str, split_size: Option<u64>) -> io::Result<Self> {
        if split_size == Some(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "split size must be > 0",
            ));
        }
        Ok(SplitWriter {
            dir: dir.to_path_buf(),
            name: name.to_string(),
            split_size,
            current: None,
            parts: Vec::new(),
            created: Rc::new(std::cell::RefCell::new(Vec::new())),
        })
    }

    fn open_next(&mut self) -> io::Result<()> {
        let fname = match self.split_size {
            Some(_) => part_name(&self.name, self.parts.len()),
            None => self.name.clone(),
        };
        let path = self.dir.join(fname);
        let f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        self.created.borrow_mut().push(path.clone());
        self.current = Some(OpenPart {
            path,
            out: BufWriter::with_capacity(BUF_SIZE, f),
            hasher: Sha256::new(),
            written: 0,
        });
        Ok(())
    }

    fn close_current(&mut self) -> io::Result<()> {
        if let Some(mut p) = self.current.take() {
            p.out.flush()?;
            let f = p.out.into_inner().map_err(|e| e.into_error())?;
            f.sync_all()?;
            self.parts.push(PartInfo {
                path: p.path,
                size: p.written,
                sha256: hex(&p.hasher.finalize()),
            });
        }
        Ok(())
    }

    /// Closes the last part and returns all parts. With splitting enabled and a single part, the
    /// part is renamed from `<name>.000` to `<name>` (a split is only visible when needed).
    pub(crate) fn finish(mut self) -> io::Result<Vec<PartInfo>> {
        if self.current.is_none() && self.parts.is_empty() {
            self.open_next()?;
        }
        self.close_current()?;
        if self.split_size.is_some() && self.parts.len() == 1 {
            let plain = self.dir.join(&self.name);
            if fs::symlink_metadata(&plain).is_err() {
                fs::rename(&self.parts[0].path, &plain)?;
                self.created.borrow_mut().push(plain.clone());
                self.parts[0].path = plain;
            }
        }
        sync_dir(&self.dir)?;
        Ok(std::mem::take(&mut self.parts))
    }
}

impl Write for SplitWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let (Some(limit), Some(cur)) = (self.split_size, self.current.as_ref()) {
            if cur.written >= limit {
                self.close_current()?;
            }
        }
        if self.current.is_none() {
            self.open_next()?;
        }
        let limit = self.split_size;
        let cur = self.current.as_mut().expect("part is open");
        let room = limit
            .map_or(buf.len() as u64, |l| l - cur.written)
            .min(buf.len() as u64) as usize;
        let n = cur.out.write(&buf[..room])?;
        cur.hasher.update(&buf[..n]);
        cur.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.current.as_mut() {
            Some(c) => c.out.flush(),
            None => Ok(()),
        }
    }
}

pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    // vfat/exfat may not support fsync on directories; ignore EINVAL there.
    match File::open(dir)?.sync_all() {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        r => r,
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Reads several files as one stream and counts the bytes read.
pub(crate) struct MultiPartReader {
    parts: Vec<PathBuf>,
    next: usize,
    current: Option<File>,
    pub(crate) counter: Rc<Cell<u64>>,
}

impl MultiPartReader {
    pub(crate) fn new(parts: &[PathBuf]) -> Self {
        MultiPartReader {
            parts: parts.to_vec(),
            next: 0,
            current: None,
            counter: Rc::new(Cell::new(0)),
        }
    }
}

impl Read for MultiPartReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.current.is_none() {
                if self.next >= self.parts.len() {
                    return Ok(0);
                }
                let p = &self.parts[self.next];
                let f = File::open(p)
                    .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
                self.current = Some(f);
                self.next += 1;
            }
            let n = self.current.as_mut().expect("open").read(buf)?;
            if n == 0 && !buf.is_empty() {
                self.current = None;
                continue;
            }
            self.counter.set(self.counter.get() + n as u64);
            return Ok(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_rejoin() {
        let d = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7) as u8).collect();
        let mut w = SplitWriter::new(d.path(), "a.tar.gz", Some(3000)).unwrap();
        for c in data.chunks(777) {
            w.write_all(c).unwrap();
        }
        let parts = w.finish().unwrap();
        assert_eq!(parts.len(), 4);
        assert_eq!(
            parts.iter().map(|p| p.size).collect::<Vec<_>>(),
            vec![3000, 3000, 3000, 1000]
        );
        assert!(parts[3].path.ends_with("a.tar.gz.003"));
        assert_eq!(parts[0].sha256, hex(&Sha256::digest(&data[..3000])));
        let paths: Vec<_> = parts.iter().map(|p| p.path.clone()).collect();
        let mut r = MultiPartReader::new(&paths);
        let mut back = Vec::new();
        r.read_to_end(&mut back).unwrap();
        assert_eq!(back, data);
        assert_eq!(r.counter.get(), 10_000);
    }

    #[test]
    fn single_part_is_renamed() {
        let d = tempfile::tempdir().unwrap();
        let mut w = SplitWriter::new(d.path(), "b.tar.gz", Some(3000)).unwrap();
        w.write_all(&[1u8; 3000]).unwrap(); // exactly one full part
        let parts = w.finish().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].path, d.path().join("b.tar.gz"));
        assert!(!d.path().join("b.tar.gz.000").exists());
    }

    #[test]
    fn never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("c.tar.gz"), b"x").unwrap();
        let mut w = SplitWriter::new(d.path(), "c.tar.gz", None).unwrap();
        assert!(w.write_all(b"y").is_err());
    }
}
