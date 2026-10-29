//! TST-3: non-destructive disk read test. Reads the whole boot disk (or one partition)
//! sequentially with `O_DIRECT` in 1 MiB blocks and reports read errors as sector ranges
//! (failing blocks are re-read in 4 KiB steps), slow blocks (> 500 ms), the throughput and
//! kernel I/O error messages from `/dev/kmsg` logged during the test. Nothing is written.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use super::{Outcome, TestReport, MAX_DETAIL_LINES};
use crate::tasks::{format_bytes, format_duration, TaskCtx, TaskFailure};

/// Read block size (TST-3).
pub const BLOCK: usize = 1024 * 1024;
/// Re-read size inside a failing block.
const SUB_BLOCK: usize = 4096;
/// A block that takes longer is "slow" (TST-3).
pub const SLOW_BLOCK: Duration = Duration::from_millis(500);
const SECTOR: u64 = 512;

/// A range of unreadable 512-byte sectors (inclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadRange {
    pub first: u64,
    pub last: u64,
}

/// Add the sectors of `[offset, offset + len)` to `ranges`, merging with the last range.
fn add_bad(ranges: &mut Vec<BadRange>, offset: u64, len: u64) {
    let first = offset / SECTOR;
    let last = (offset + len).div_ceil(SECTOR) - 1;
    match ranges.last_mut() {
        Some(r) if r.last + 1 >= first => r.last = r.last.max(last),
        _ => ranges.push(BadRange { first, last }),
    }
}

/// Buffer aligned for `O_DIRECT`.
struct Aligned {
    data: Vec<u8>,
    start: usize,
}

impl Aligned {
    fn new(len: usize) -> Self {
        let data = vec![0u8; len + SUB_BLOCK];
        let start = data.as_ptr().align_offset(SUB_BLOCK);
        Aligned { data, start }
    }

    fn get(&mut self, len: usize) -> &mut [u8] {
        &mut self.data[self.start..self.start + len]
    }
}

/// Open for reading with `O_DIRECT`; falls back to buffered reads where it is not supported
/// (tmpfs image files in simulation). The bool says whether `O_DIRECT` is used.
fn open(path: &Path) -> std::io::Result<(File, bool)> {
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
    {
        Ok(f) => Ok((f, true)),
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok((File::open(path)?, false)),
        Err(e) => Err(e),
    }
}

/// Kernel log reader positioned at the end, so only messages from the test are read.
struct Kmsg(File);

impl Kmsg {
    fn open(path: &Path) -> Option<Kmsg> {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .ok()?;
        f.seek(SeekFrom::End(0)).ok()?;
        Some(Kmsg(f))
    }

    /// New records (`prio,seq,ts,flags;message`) as messages.
    fn read_new(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 8192];
        loop {
            match self.0.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let rec = String::from_utf8_lossy(&buf[..n]);
                    let first = rec.lines().next().unwrap_or("");
                    out.push(first.split_once(';').map_or(first, |(_, m)| m).to_string());
                }
                // EPIPE: records were overwritten; the next read continues.
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue,
                Err(_) => break, // EAGAIN: no more records
            }
        }
        out
    }
}

/// Kernel messages that look like I/O errors of `device` (kernel name, e.g. `sda`).
pub fn io_error_messages(messages: &[String], device: &str) -> Vec<String> {
    let disk: String = device
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end_matches('p')
        .to_string();
    messages
        .iter()
        .filter(|m| {
            let l = m.to_ascii_lowercase();
            let error = l.contains("i/o error")
                || l.contains("medium error")
                || l.contains("blk_update_request")
                || l.contains("unrecovered read error")
                || l.contains("failed command")
                || l.contains("exception emask")
                || l.contains("timeout");
            error && (disk.is_empty() || l.contains(&disk) || l.starts_with("ata"))
        })
        .cloned()
        .collect()
}

/// Statistics of a read run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadStats {
    pub size: u64,
    pub read: u64,
    pub bad: Vec<BadRange>,
    pub bad_bytes: u64,
    /// Offsets of blocks that failed and then read fine in 4 KiB steps.
    pub retried_ok: Vec<u64>,
    /// `(offset, duration)` of slow blocks (first [`MAX_DETAIL_LINES`]).
    pub slow: Vec<(u64, Duration)>,
    pub slow_count: u64,
    pub slowest: Duration,
    pub elapsed: Duration,
    pub completed: bool,
}

/// Read `file` (`size` bytes) sequentially. `tick(done)` reports progress and returns `false`
/// to cancel. Generic over the reader for tests.
pub fn read_all(file: &impl FileExt, size: u64, tick: &mut dyn FnMut(u64) -> bool) -> ReadStats {
    let mut st = ReadStats {
        size,
        ..ReadStats::default()
    };
    let mut buf = Aligned::new(BLOCK);
    let t0 = Instant::now();
    let mut offset = 0u64;
    while offset < size {
        let want = ((size - offset) as usize).min(BLOCK);
        // O_DIRECT needs aligned lengths; a short tail is read in full 4 KiB units.
        let len = want.div_ceil(SUB_BLOCK) * SUB_BLOCK;
        let t = Instant::now();
        let r = file.read_at(buf.get(len), offset);
        let took = t.elapsed();
        match r {
            Ok(n) if n >= want => {}
            Ok(_) | Err(_) => {
                // Narrow the failure down to 4 KiB units.
                let mut all_ok = true;
                let mut sub = 0usize;
                while sub < want {
                    let n = (want - sub).min(SUB_BLOCK);
                    let o = offset + sub as u64;
                    let ok = matches!(file.read_at(buf.get(SUB_BLOCK), o), Ok(m) if m >= n);
                    if !ok {
                        all_ok = false;
                        add_bad(&mut st.bad, o, n as u64);
                        st.bad_bytes += n as u64;
                    }
                    sub += SUB_BLOCK;
                    if !tick(offset + sub as u64) {
                        st.elapsed = t0.elapsed();
                        return st;
                    }
                }
                if all_ok {
                    st.retried_ok.push(offset);
                }
            }
        }
        if took > SLOW_BLOCK {
            st.slow_count += 1;
            if st.slow.len() < MAX_DETAIL_LINES {
                st.slow.push((offset, took));
            }
        }
        st.slowest = st.slowest.max(took);
        offset += want as u64;
        st.read = offset;
        if !tick(offset) {
            st.elapsed = t0.elapsed();
            return st;
        }
    }
    st.elapsed = t0.elapsed();
    st.completed = true;
    st
}

/// Run the read test on `path` (device node or image). `name` is the kernel name for the
/// kernel log filter and the report; `kmsg` the kernel log (`None` in simulation).
pub fn run(
    path: &Path,
    name: &str,
    kmsg: Option<&Path>,
    ctx: &TaskCtx,
) -> Result<TestReport, TaskFailure> {
    let (mut file, direct) = open(path)
        .map_err(|e| TaskFailure::Error(format!("Cannot open {}: {e}", path.display())))?;
    let size = file.seek(SeekFrom::End(0)).map_err(|e| {
        TaskFailure::Error(format!("Cannot get the size of {}: {e}", path.display()))
    })?;
    if size == 0 {
        return Err(TaskFailure::Error(format!("{} is empty.", path.display())));
    }
    let mut klog = kmsg.and_then(Kmsg::open);
    let target = format!("{name} ({}, {})", path.display(), format_bytes(size));
    ctx.set_progress(0, size);
    let mut tick = |done: u64| {
        ctx.set_progress(done, size);
        ctx.set_message(format!(
            "Reading {name}: {} of {}",
            format_bytes(done),
            format_bytes(size)
        ));
        !ctx.is_cancelled()
    };
    let st = read_all(&file, size, &mut tick);
    let kernel = klog
        .as_mut()
        .map(|k| io_error_messages(&k.read_new(), name))
        .unwrap_or_default();

    let secs = st.elapsed.as_secs_f64().max(0.001);
    let mut d = String::new();
    let _ = writeln!(
        d,
        "Read {} of {} in {} ({}/s){}.",
        format_bytes(st.read),
        format_bytes(size),
        format_duration(st.elapsed),
        format_bytes((st.read as f64 / secs) as u64),
        if direct {
            ", O_DIRECT"
        } else {
            ", buffered (O_DIRECT not supported)"
        }
    );
    let _ = writeln!(
        d,
        "Slowest 1 MiB block: {} ms; slow blocks (> {} ms): {}.",
        st.slowest.as_millis(),
        SLOW_BLOCK.as_millis(),
        st.slow_count
    );
    if !st.bad.is_empty() {
        let _ = writeln!(
            d,
            "\nUnreadable sectors (512 bytes), {} in total:",
            st.bad_bytes / SECTOR
        );
        for r in st.bad.iter().take(MAX_DETAIL_LINES) {
            let _ = writeln!(
                d,
                "  {}-{} ({} sectors)",
                r.first,
                r.last,
                r.last - r.first + 1
            );
        }
        if st.bad.len() > MAX_DETAIL_LINES {
            let _ = writeln!(
                d,
                "  ... and {} more range(s)",
                st.bad.len() - MAX_DETAIL_LINES
            );
        }
    }
    for o in st.retried_ok.iter().take(MAX_DETAIL_LINES) {
        let _ = writeln!(d, "Read error at byte {o}, the retry succeeded.");
    }
    if !st.slow.is_empty() {
        let _ = writeln!(d, "\nSlow blocks:");
        for (o, t) in &st.slow {
            let _ = writeln!(d, "  at {} ({} ms)", format_bytes(*o), t.as_millis());
        }
    }
    match (&klog, kernel.is_empty()) {
        (None, _) => {
            let _ = writeln!(
                d,
                "\nKernel log not available: I/O error messages not checked."
            );
        }
        (Some(_), true) => {
            let _ = writeln!(d, "\nNo kernel I/O error messages during the test.");
        }
        (Some(_), false) => {
            let _ = writeln!(d, "\nKernel messages during the test:");
            for m in kernel.iter().take(MAX_DETAIL_LINES) {
                let _ = writeln!(d, "  {m}");
            }
        }
    }
    let problems = !st.bad.is_empty();
    let remarks = st.slow_count > 0 || !st.retried_ok.is_empty() || !kernel.is_empty();
    let (outcome, summary) = if problems {
        (
            Outcome::Fail,
            format!(
                "{} unreadable in {} range(s){}.",
                format_bytes(st.bad_bytes),
                st.bad.len(),
                if st.completed { "" } else { " (cancelled)" }
            ),
        )
    } else if !st.completed {
        (
            Outcome::Aborted,
            format!(
                "Cancelled after {}, no read errors so far.",
                format_bytes(st.read)
            ),
        )
    } else if remarks {
        (
            Outcome::Warning,
            format!(
                "Readable, but {} slow block(s), {} retried block(s), {} kernel message(s).",
                st.slow_count,
                st.retried_ok.len(),
                kernel.len()
            ),
        )
    } else {
        (
            Outcome::Pass,
            format!(
                "{} read without errors at {}/s.",
                format_bytes(size),
                format_bytes((st.read as f64 / secs) as u64)
            ),
        )
    };
    Ok(TestReport {
        outcome,
        target,
        summary,
        details: d,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A "disk" with unreadable byte ranges.
    struct FakeDisk {
        size: u64,
        bad: Vec<(u64, u64)>,
        reads: Cell<u64>,
    }

    impl FileExt for FakeDisk {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
            self.reads.set(self.reads.get() + 1);
            let end = offset + buf.len() as u64;
            if self.bad.iter().any(|&(s, e)| s < end && offset < e) {
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
            Ok((self.size.saturating_sub(offset) as usize).min(buf.len()))
        }

        fn write_at(&self, _: &[u8], _: u64) -> std::io::Result<usize> {
            unreachable!("the read test never writes")
        }
    }

    #[test]
    fn clean_disk() {
        let d = FakeDisk {
            size: 3 * BLOCK as u64 + 1000,
            bad: vec![],
            reads: Cell::new(0),
        };
        let mut last = 0;
        let st = read_all(&d, d.size, &mut |n| {
            last = n;
            true
        });
        assert!(st.completed);
        assert!(st.bad.is_empty());
        assert_eq!(st.read, d.size);
        assert_eq!(last, d.size);
        assert_eq!(d.reads.get(), 4);
    }

    #[test]
    fn bad_sectors_become_ranges() {
        let b = BLOCK as u64;
        let d = FakeDisk {
            size: 4 * b,
            // 8 KiB in block 1, two adjacent 4 KiB units across blocks 2/3
            bad: vec![(b + 8192, b + 16384), (3 * b - 100, 3 * b + 100)],
            reads: Cell::new(0),
        };
        let st = read_all(&d, d.size, &mut |_| true);
        assert!(st.completed);
        assert_eq!(
            st.bad,
            vec![
                BadRange {
                    first: (b + 8192) / 512,
                    last: (b + 16384) / 512 - 1
                },
                BadRange {
                    first: (3 * b - 4096) / 512,
                    last: (3 * b + 4096) / 512 - 1
                },
            ]
        );
        assert_eq!(st.bad_bytes, 8192 + 8192);
        assert!(st.retried_ok.is_empty());
    }

    #[test]
    fn cancel() {
        let d = FakeDisk {
            size: 10 * BLOCK as u64,
            bad: vec![],
            reads: Cell::new(0),
        };
        let mut n = 0;
        let st = read_all(&d, d.size, &mut |_| {
            n += 1;
            n < 2
        });
        assert!(!st.completed);
        assert_eq!(st.read, 2 * BLOCK as u64);
    }

    #[test]
    fn range_merging_and_kernel_filter() {
        let mut r = Vec::new();
        add_bad(&mut r, 0, 4096);
        add_bad(&mut r, 4096, 4096);
        add_bad(&mut r, 100 * 4096, 4096);
        assert_eq!(
            r,
            vec![
                BadRange { first: 0, last: 15 },
                BadRange {
                    first: 800,
                    last: 807
                }
            ]
        );
        let msgs = vec![
            "blk_update_request: I/O error, dev sda, sector 2048 op 0x0:(READ)".to_string(),
            "usb 1-1: new high-speed USB device".to_string(),
            "Buffer I/O error on dev sdb1, logical block 0".to_string(),
        ];
        assert_eq!(io_error_messages(&msgs, "sda4"), vec![msgs[0].clone()]);
        assert_eq!(io_error_messages(&msgs, "nvme0n1p6").len(), 0);
    }

    #[test]
    fn file_run_reports() {
        let d = crate::hw::testutil::TempDir::new("diskread");
        d.write("disk.img", &vec![7u8; 3 * BLOCK + 512]);
        let runner = crate::tasks::TaskRunner::new();
        let path = d.path().join("disk.img");
        let out = std::sync::Arc::new(std::sync::Mutex::new(None));
        let o2 = std::sync::Arc::clone(&out);
        runner
            .spawn(
                "read",
                crate::tasks::TaskKind::ReadOnly,
                move |ctx| {
                    *crate::tasks::lock(&o2) = Some(run(&path, "disk", None, ctx));
                    Ok(String::new())
                },
                None,
            )
            .expect("spawn")
            .wait();
        let r = crate::tasks::lock(&out).take().expect("ran").expect("ok");
        assert_eq!(r.outcome, Outcome::Pass, "{r:?}");
        assert!(r.details.contains("Kernel log not available"));
        assert!(r.target.starts_with("disk ("));
    }
}
