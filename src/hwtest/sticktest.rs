//! TST-8: destructive stick test. Writes a seeded pseudo-random pattern over a whole external
//! disk and reads it back (both with `O_DIRECT`). Every 4 KiB unit starts with its own offset
//! (scrambled with the seed), so data that shows up at the wrong place is recognised: a "fake
//! capacity" stick maps the area beyond its real size onto the start (or drops it), which the
//! read-back reports as wrapped data from a lower offset or as lost data.
//!
//! Safety (UI-4, NF-4): typed confirmation [`CONFIRM_WORD`]; the boot disk is refused by the
//! backend ([`SystemBackend::stick_test_target`]), disks with mounted partitions here; the
//! device is opened with `O_EXCL`, which the kernel refuses while any of its partitions is
//! mounted or in use.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use super::{Outcome, TestReport, MAX_DETAIL_LINES};
use crate::backend::SystemBackend;
use crate::tasks::{format_bytes, format_duration, TaskCtx, TaskFailure};

/// Word to type before the stick is overwritten (UI-4).
pub const CONFIRM_WORD: &str = "ERASE";

/// Write/read block (one block in memory per direction, NF-5).
const BLOCK: usize = 1024 * 1024;
/// Unit with its own offset stamp; also the `O_DIRECT` alignment.
const UNIT: usize = 4096;
const WORDS_PER_UNIT: usize = UNIT / 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StickTestRequest {
    /// Kernel name of the whole disk, e.g. `sdb`.
    pub disk: String,
    /// What the user typed; must be [`CONFIRM_WORD`].
    pub confirmation: String,
}

/// Block I/O of the device under test (a file in tests and simulation).
pub trait StickIo {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
}

impl StickIo for File {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        FileExt::write_all_at(self, buf, offset)
    }
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        FileExt::read_exact_at(self, buf, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.sync_all()
    }
}

/// splitmix64 (same generator as the memory test).
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The pattern: word `i` of the unit at `unit_off`.
fn word(seed: u64, unit_off: u64, i: usize) -> u64 {
    if i == 0 {
        unit_off ^ splitmix(seed)
    } else {
        splitmix(seed ^ unit_off.wrapping_add(i as u64))
    }
}

fn fill_unit(buf: &mut [u8], seed: u64, unit_off: u64) {
    for (i, w) in buf.chunks_exact_mut(8).enumerate() {
        w.copy_from_slice(&word(seed, unit_off, i).to_le_bytes());
    }
}

fn read_word(buf: &[u8], i: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[i * 8..i * 8 + 8]);
    u64::from_le_bytes(b)
}

/// What the read-back found in one 4 KiB unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitState {
    Good,
    /// The unit holds the (intact) data written to this other offset: the stick maps two
    /// addresses onto the same flash (fake capacity).
    Wrapped(u64),
    /// Anything else: lost or corrupted data.
    Bad,
    WriteError,
    ReadError,
}

pub fn check_unit(buf: &[u8], seed: u64, unit_off: u64, size: u64) -> UnitState {
    let ok = |off: u64| (0..WORDS_PER_UNIT).all(|i| read_word(buf, i) == word(seed, off, i));
    if ok(unit_off) {
        return UnitState::Good;
    }
    let claimed = read_word(buf, 0) ^ splitmix(seed);
    if claimed != unit_off && claimed % UNIT as u64 == 0 && claimed < size && ok(claimed) {
        UnitState::Wrapped(claimed)
    } else {
        UnitState::Bad
    }
}

/// A run of units with the same problem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadArea {
    pub offset: u64,
    pub len: u64,
    pub state: UnitState,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StickStats {
    pub tested: u64,
    pub bad_bytes: u64,
    pub areas: Vec<BadArea>,
    /// Number of areas beyond `areas` (only the first [`MAX_DETAIL_LINES`] are kept).
    pub more_areas: u64,
    pub write_time: Duration,
    pub read_time: Duration,
}

impl StickStats {
    fn add(&mut self, offset: u64, len: u64, state: UnitState) {
        self.bad_bytes += len;
        if let Some(last) = self.areas.last_mut() {
            let same_kind = match (last.state, state) {
                (UnitState::Wrapped(a), UnitState::Wrapped(b)) => {
                    b.wrapping_sub(a) == offset - last.offset
                }
                (a, b) => a == b,
            };
            if same_kind && last.offset + last.len == offset {
                last.len += len;
                return;
            }
        }
        if self.areas.len() < MAX_DETAIL_LINES {
            self.areas.push(BadArea { offset, len, state });
        } else {
            self.more_areas += 1;
        }
    }

    /// The first bad byte: the usable size of a fake-capacity stick.
    pub fn first_bad(&self) -> Option<u64> {
        self.areas.first().map(|a| a.offset)
    }

    pub fn wrapped(&self) -> bool {
        self.wrap_size().is_some()
    }

    /// Smallest distance between a unit and the offset whose data it holds: the real size of
    /// a stick whose addresses wrap around.
    pub fn wrap_size(&self) -> Option<u64> {
        self.areas
            .iter()
            .filter_map(|a| match a.state {
                UnitState::Wrapped(from) => Some(from.abs_diff(a.offset)),
                _ => None,
            })
            .min()
    }
}

/// Phase for the progress callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Write,
    Read,
}

/// Buffer aligned for `O_DIRECT`.
struct Aligned(Vec<u8>, usize);

impl Aligned {
    fn new(len: usize) -> Aligned {
        let v = vec![0u8; len + UNIT];
        let start = v.as_ptr().align_offset(UNIT);
        Aligned(v, start)
    }
    fn get(&mut self, len: usize) -> &mut [u8] {
        &mut self.0[self.1..self.1 + len]
    }
}

/// Write the pattern over the first `size` bytes (a multiple of 4 KiB), then read it back.
/// `tick(phase, bytes)` is called per block and returns `false` to cancel (`None`).
pub fn write_read(
    dev: &dyn StickIo,
    size: u64,
    seed: u64,
    tick: &mut dyn FnMut(Phase, u64) -> bool,
) -> Option<StickStats> {
    let mut st = StickStats {
        tested: size,
        ..StickStats::default()
    };
    let mut buf = Aligned::new(BLOCK);
    let mut write_errors: Vec<(u64, u64)> = Vec::new();
    let t0 = Instant::now();
    let mut off = 0u64;
    while off < size {
        let len = (size - off).min(BLOCK as u64) as usize;
        let b = buf.get(len);
        for (u, chunk) in b.chunks_exact_mut(UNIT).enumerate() {
            fill_unit(chunk, seed, off + (u * UNIT) as u64);
        }
        if let Err(e) = dev.write_all_at(b, off) {
            log::warn!("TST-8: write error at {off}: {e}");
            write_errors.push((off, len as u64));
        }
        if !tick(Phase::Write, len as u64) {
            return None;
        }
        off += len as u64;
    }
    if let Err(e) = dev.sync() {
        log::warn!("TST-8: sync: {e}");
    }
    st.write_time = t0.elapsed();
    let t1 = Instant::now();
    off = 0;
    while off < size {
        let len = (size - off).min(BLOCK as u64) as usize;
        let b = buf.get(len);
        let read_ok = dev.read_exact_at(b, off).is_ok();
        let werr = write_errors
            .iter()
            .any(|(o, l)| off < o + l && *o < off + len as u64);
        for (u, chunk) in b.chunks_exact(UNIT).enumerate() {
            let uo = off + (u * UNIT) as u64;
            let state = if !read_ok {
                UnitState::ReadError
            } else {
                match check_unit(chunk, seed, uo, size) {
                    UnitState::Good => UnitState::Good,
                    _ if werr => UnitState::WriteError,
                    other => other,
                }
            };
            if state != UnitState::Good {
                st.add(uo, UNIT as u64, state);
            }
        }
        if !tick(Phase::Read, len as u64) {
            return None;
        }
        off += len as u64;
    }
    st.read_time = t1.elapsed();
    Some(st)
}

fn speed(bytes: u64, d: Duration) -> String {
    let s = d.as_secs_f64();
    if s <= 0.0 {
        return "-".into();
    }
    format!("{:.1} MB/s", bytes as f64 / s / 1e6)
}

/// Disks the stick test may overwrite: every disk but the boot disk, with a medium.
pub fn candidates(backend: &dyn SystemBackend) -> Result<Vec<crate::backend::BlockDevice>, String> {
    Ok(backend
        .block_devices()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|d| !d.boot_disk && d.size > 0 && crate::hwtest::health::testable(d))
        .collect())
}

/// One line per candidate disk (CLI and TUI).
pub fn label(d: &crate::backend::BlockDevice) -> String {
    let mounted = d.partitions.iter().any(|p| p.mountpoint.is_some());
    format!(
        "{:<8} {:<10} {:<6} {}{}",
        d.name,
        crate::tasks::format_bytes(d.size),
        d.transport,
        crate::report::ellipsize(d.model.as_deref().unwrap_or("-"), 26),
        if mounted { " [mounted]" } else { "" }
    )
}

/// Text report of the test.
pub fn report(name: &str, nominal: u64, st: &StickStats) -> TestReport {
    let mut d = String::new();
    let _ = writeln!(
        d,
        "Tested {} of {} ({} bytes). Write {} ({}), read {} ({}).",
        format_bytes(st.tested),
        format_bytes(nominal),
        st.tested,
        speed(st.tested, st.write_time),
        format_duration(st.write_time),
        speed(st.tested, st.read_time),
        format_duration(st.read_time),
    );
    for a in &st.areas {
        let what = match a.state {
            UnitState::Wrapped(from) => format!("holds the data written at {from}"),
            UnitState::Bad => "data lost or corrupted".into(),
            UnitState::WriteError => "write error".into(),
            UnitState::ReadError => "read error".into(),
            UnitState::Good => "ok".into(),
        };
        let _ = writeln!(
            d,
            "  bytes {}..{} ({}): {what}",
            a.offset,
            a.offset + a.len,
            format_bytes(a.len)
        );
    }
    if st.more_areas > 0 {
        let _ = writeln!(d, "  ... and {} more area(s)", st.more_areas);
    }
    let _ = writeln!(d, "\n{}", crate::texts::STICKTEST_AFTER);
    let (outcome, summary) = match st.first_bad() {
        Some(_) if st.wrapped() => (
            Outcome::Fail,
            format!(
                "Fake capacity: data wraps around; the stick stores only about {} of {}.",
                format_bytes(st.wrap_size().unwrap_or(0)),
                format_bytes(nominal)
            ),
        ),
        None => (
            Outcome::Pass,
            format!(
                "{} written and read back without errors.",
                format_bytes(st.tested)
            ),
        ),
        Some(first) if st.bad_bytes >= st.tested.saturating_sub(first) / 2 => (
            Outcome::Fail,
            format!(
                "Fake or broken capacity: only the first {} of {} hold data.",
                format_bytes(first),
                format_bytes(nominal)
            ),
        ),
        Some(_) => (
            Outcome::Fail,
            format!(
                "{} in {} area(s) failed.",
                format_bytes(st.bad_bytes),
                st.areas.len() as u64 + st.more_areas
            ),
        ),
    };
    TestReport {
        outcome,
        target: format!("{name} ({})", format_bytes(nominal)),
        summary,
        details: d,
    }
}

/// Open for the test: read/write, `O_EXCL` (refused while mounted), `O_DIRECT` where
/// supported (not on tmpfs image files).
fn open(path: &Path) -> io::Result<(File, bool)> {
    let o = |flags| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(flags)
            .open(path)
    };
    match o(libc::O_EXCL | libc::O_DIRECT) {
        Ok(f) => Ok((f, true)),
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => o(libc::O_EXCL).map(|f| (f, false)),
        Err(e) => Err(e),
    }
}

/// Ask the kernel to re-read the (now overwritten) partition table (`BLKRRPART`).
fn reread_partitions(f: &File) {
    use std::os::unix::io::AsRawFd;
    const BLKRRPART: u64 = 0x125F;
    // SAFETY: argument-less ioctl on an open block device.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), BLKRRPART as _) };
    if rc != 0 {
        log::info!("TST-8: BLKRRPART: {}", io::Error::last_os_error());
    }
}

fn seed_now() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    splitmix(t ^ u64::from(std::process::id()))
}

/// Run the test (in a write task).
pub fn run(
    backend: &dyn SystemBackend,
    req: &StickTestRequest,
    ctx: &TaskCtx,
) -> Result<TestReport, TaskFailure> {
    if req.confirmation.trim() != CONFIRM_WORD {
        return Err(TaskFailure::Error(format!(
            "Not confirmed: type {CONFIRM_WORD} to overwrite {}.",
            req.disk
        )));
    }
    let devices = backend.block_devices().map_err(TaskFailure::from)?;
    let disk =
        crate::backend::check_stick_test_disk(&devices, &req.disk).map_err(TaskFailure::from)?;
    let mounted: Vec<String> = disk
        .partitions
        .iter()
        .filter_map(|p| p.mountpoint.as_ref().map(|m| format!("{} on {m}", p.name)))
        .collect();
    if !mounted.is_empty() {
        return Err(TaskFailure::Error(format!(
            "{} is in use: {}. Unmount it first.",
            req.disk,
            mounted.join(", ")
        )));
    }
    let (path, size) = backend
        .stick_test_target(&req.disk)
        .map_err(TaskFailure::from)?;
    let test_size = size / UNIT as u64 * UNIT as u64;
    if test_size == 0 {
        return Err(TaskFailure::Error(format!(
            "{} has no medium (size 0).",
            req.disk
        )));
    }
    let (f, direct) = open(&path).map_err(|e| {
        TaskFailure::Error(format!(
            "Cannot open {} exclusively: {e} (mounted or in use?)",
            path.display()
        ))
    })?;
    log::warn!(
        "TST-8: overwriting {} ({}, {} bytes, model {}) with a test pattern",
        path.display(),
        format_bytes(size),
        size,
        disk.model.as_deref().unwrap_or("-")
    );
    ctx.set_progress(0, 2 * test_size);
    let mut done = 0u64;
    let mut last = None;
    let mut tick = |phase: Phase, n: u64| {
        if last != Some(phase) {
            ctx.set_message(match phase {
                Phase::Write => format!("Writing the test pattern to {}", req.disk),
                Phase::Read => format!("Reading back and checking {}", req.disk),
            });
            last = Some(phase);
        }
        done += n;
        ctx.set_progress(done, 2 * test_size);
        !ctx.is_cancelled()
    };
    let r = write_read(&f, test_size, seed_now(), &mut tick);
    if !backend.is_simulation() {
        reread_partitions(&f);
    }
    drop(f);
    let st = r.ok_or_else(|| {
        log::warn!("TST-8: cancelled; {} is partly overwritten", req.disk);
        TaskFailure::Cancelled
    })?;
    let mut rep = report(&req.disk, size, &st);
    if !direct {
        rep.details
            .push_str("Note: O_DIRECT is not supported here; the read-back may hit the cache.\n");
    }
    log::warn!("TST-8: {}: {}", req.disk, rep.summary);
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A stick that claims `nominal` bytes but stores only `real`; addresses wrap around.
    struct FakeStick {
        real: Mutex<Vec<u8>>,
        bad_from: Option<u64>,
    }

    impl StickIo for FakeStick {
        fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
            let mut m = self.real.lock().expect("lock");
            let n = m.len() as u64;
            for (i, b) in buf.iter().enumerate() {
                let a = (offset + i as u64) % n;
                m[a as usize] = *b;
            }
            Ok(())
        }
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
            let m = self.real.lock().expect("lock");
            let n = m.len() as u64;
            for (i, b) in buf.iter_mut().enumerate() {
                let a = offset + i as u64;
                *b = match self.bad_from {
                    Some(f) if a >= f => 0xFF,
                    _ => m[(a % n) as usize],
                };
            }
            Ok(())
        }
        fn sync(&self) -> io::Result<()> {
            Ok(())
        }
    }

    fn stick(real: usize, bad_from: Option<u64>) -> FakeStick {
        FakeStick {
            real: Mutex::new(vec![0u8; real]),
            bad_from,
        }
    }

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn good_stick_passes() {
        let s = stick(3 * MIB as usize, None);
        let mut ticks = 0u64;
        let st = write_read(&s, 3 * MIB, 42, &mut |_, n| {
            ticks += n;
            true
        })
        .expect("run");
        assert_eq!(ticks, 6 * MIB);
        assert_eq!(st.first_bad(), None);
        let r = report("sdb", 3 * MIB, &st);
        assert_eq!(r.outcome, Outcome::Pass, "{r:?}");
    }

    #[test]
    fn fake_capacity_is_detected() {
        // Claims 4 MiB, stores 1 MiB: everything above 1 MiB wraps onto the start.
        let s = stick(MIB as usize, None);
        let st = write_read(&s, 4 * MIB, 7, &mut |_, _| true).expect("run");
        // The last pass over [3, 4) MiB overwrote the start, so the start holds that data.
        assert!(st.wrapped());
        assert_eq!(st.first_bad(), Some(0));
        let r = report("sdb", 4 * MIB, &st);
        assert_eq!(r.outcome, Outcome::Fail);
        assert!(
            r.summary.contains("stores only about 1.0 MiB of 4.0 MiB"),
            "{}",
            r.summary
        );
        assert!(
            r.details.contains("holds the data written at 3145728"),
            "{}",
            r.details
        );
        // Wrapped units of one area are merged into one line per MiB.
        assert!(st.areas.len() <= 4, "{:?}", st.areas);
    }

    #[test]
    fn dropped_writes_beyond_real_size() {
        // Reads above 2 MiB return 0xFF (data dropped): first bad byte = usable size.
        let s = stick(4 * MIB as usize, Some(2 * MIB));
        let st = write_read(&s, 4 * MIB, 9, &mut |_, _| true).expect("run");
        assert_eq!(st.first_bad(), Some(2 * MIB));
        assert_eq!(st.bad_bytes, 2 * MIB);
        assert!(!st.wrapped());
        let r = report("sdc", 4 * MIB, &st);
        assert!(
            r.summary.contains("only the first 2.0 MiB"),
            "{}",
            r.summary
        );
    }

    #[test]
    fn cancel_and_unit_check() {
        let s = stick(2 * MIB as usize, None);
        let mut calls = 0;
        assert!(write_read(&s, 2 * MIB, 1, &mut |_, _| {
            calls += 1;
            calls < 2
        })
        .is_none());
        let mut u = vec![0u8; UNIT];
        fill_unit(&mut u, 5, 8192);
        assert_eq!(check_unit(&u, 5, 8192, MIB), UnitState::Good);
        assert_eq!(check_unit(&u, 5, 0, MIB), UnitState::Wrapped(8192));
        assert_eq!(check_unit(&u, 6, 8192, MIB), UnitState::Bad);
        u[100] ^= 1;
        assert_eq!(check_unit(&u, 5, 8192, MIB), UnitState::Bad);
    }

    #[test]
    fn simulation_refuses_boot_disk_and_needs_the_word() {
        use crate::backend::sim::SimBackend;
        use crate::hw::testutil::TempDir;
        use crate::tasks::{lock, TaskKind, TaskRunner};
        use std::sync::Arc;
        let d = TempDir::new("sticktest");
        SimBackend::create_example(d.path()).expect("example");
        let b = Arc::new(SimBackend::open(d.path()).expect("open"));
        let run_req = |disk: &str, word: &str| {
            let b = Arc::clone(&b);
            let req = StickTestRequest {
                disk: disk.into(),
                confirmation: word.into(),
            };
            let out = Arc::new(Mutex::new(None));
            let o2 = Arc::clone(&out);
            TaskRunner::new()
                .spawn(
                    "stick",
                    TaskKind::Write,
                    move |ctx| {
                        *lock(&o2) = Some(run(b.as_ref(), &req, ctx));
                        Ok(String::new())
                    },
                    None,
                )
                .expect("spawn")
                .wait();
            let r = lock(&out).take().expect("ran");
            r
        };
        assert!(matches!(
            run_req("sdb", "erase"),
            Err(TaskFailure::Error(_))
        ));
        match run_req("sda", CONFIRM_WORD) {
            Err(TaskFailure::Error(e)) => assert!(e.contains("boot disk"), "{e}"),
            other => panic!("{other:?}"),
        }
        let r = run_req("sdb", CONFIRM_WORD).expect("sdb");
        assert_eq!(r.outcome, Outcome::Pass, "{r:?}");
        assert!(r.target.starts_with("sdb (32.0 MiB)"), "{}", r.target);
    }
}
