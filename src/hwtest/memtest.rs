//! TST-1: userspace memory test. Allocates and `mlock`s a share of `MemAvailable` (default
//! 80 %, leaving at least 64 MiB) and runs, per pass: solid bits (0x00/0xFF), checkerboard,
//! walking ones, walking zeros, address-in-address, seeded random data and a bit-flip test.
//! Failing words are reported with their offset in the test buffer and the expected/actual
//! values. A userspace test cannot reach memory used by the kernel and the app itself.

use std::fmt::Write as _;
use std::path::Path;

use super::{Outcome, TestReport, MAX_DETAIL_LINES};
use crate::tasks::{format_bytes, TaskCtx, TaskFailure};

const MIB: u64 = 1024 * 1024;

/// Default share of `MemAvailable` (TST-1).
pub const DEFAULT_PERCENT: u8 = 80;
/// Memory always left free (TST-1).
pub const RESERVE_BYTES: u64 = 64 * MIB;
/// Upper bound in simulation mode (the host's memory is not the device's).
pub const SIM_MAX_BYTES: u64 = 256 * MIB;
/// Accepted share and passes.
pub const PERCENT_RANGE: std::ops::RangeInclusive<u8> = 1..=95;
pub const PASSES_RANGE: std::ops::RangeInclusive<u32> = 1..=100;

/// Words between progress/cancel checks (1 MiB).
const CHUNK_WORDS: usize = (MIB / 8) as usize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemOptions {
    /// Share of `MemAvailable` in percent.
    pub percent: u8,
    pub passes: u32,
    /// Cap of the test size (simulation).
    pub max_bytes: Option<u64>,
}

impl Default for MemOptions {
    fn default() -> Self {
        MemOptions {
            percent: DEFAULT_PERCENT,
            passes: 1,
            max_bytes: None,
        }
    }
}

/// From `/proc/meminfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
}

/// Parse `/proc/meminfo` text (`MemTotal:  1234 kB`).
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let field = |name: &str| {
        text.lines().find_map(|l| {
            let rest = l.strip_prefix(name)?.strip_prefix(':')?;
            let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            Some(kb * 1024)
        })
    };
    Some(MemInfo {
        total: field("MemTotal")?,
        available: field("MemAvailable")?,
    })
}

pub fn mem_info(proc_root: &Path) -> Result<MemInfo, String> {
    let path = proc_root.join("meminfo");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_meminfo(&text).ok_or_else(|| format!("{}: no MemTotal/MemAvailable", path.display()))
}

/// Bytes to test: `percent` of `available`, at most `available - 64 MiB`, a multiple of 1 MiB.
pub fn test_size(available: u64, opts: &MemOptions) -> u64 {
    let want = available / 100 * u64::from(opts.percent);
    let mut size = want.min(available.saturating_sub(RESERVE_BYTES));
    if let Some(cap) = opts.max_bytes {
        size = size.min(cap);
    }
    size / MIB * MIB
}

/// A failing word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemError {
    pub pattern: String,
    /// Byte offset in the test buffer.
    pub offset: u64,
    pub expected: u64,
    pub actual: u64,
}

impl MemError {
    pub fn line(&self) -> String {
        format!(
            "{}: offset 0x{:010x} expected 0x{:016x} actual 0x{:016x} (bits 0x{:016x})",
            self.pattern,
            self.offset,
            self.expected,
            self.actual,
            self.expected ^ self.actual
        )
    }
}

/// splitmix64: index-addressable pseudo-random data from a seed.
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Number of buffer sweeps (one write or one read of the whole buffer) per pass.
pub const SWEEPS_PER_PASS: u64 = 2 * 2 // solid 0x00, 0xFF
    + 2 * 2 // checkerboard and inverse
    + 64 * 2 // walking ones
    + 64 * 2 // walking zeros
    + 2 * 2 // address and inverse
    + 2 // random
    + 1 + BITFLIPS + 1; // bit flip: fill, flips, final check

const BITFLIPS: u64 = 8;

/// Runs the patterns over a buffer. `tick(bytes)` is called every MiB and returns `false` to
/// cancel.
pub struct Tester<'a> {
    buf: &'a mut [u64],
    /// Address of the buffer (address-in-address pattern).
    base: u64,
    pub errors: Vec<MemError>,
    pub error_count: u64,
    tick: &'a mut dyn FnMut(u64) -> bool,
}

impl<'a> Tester<'a> {
    pub fn new(buf: &'a mut [u64], tick: &'a mut dyn FnMut(u64) -> bool) -> Self {
        let base = buf.as_ptr() as u64;
        Tester {
            buf,
            base,
            errors: Vec::new(),
            error_count: 0,
            tick,
        }
    }

    fn report(&mut self, pattern: &str, i: usize, expected: u64, actual: u64) {
        self.error_count += 1;
        if self.errors.len() < MAX_DETAIL_LINES {
            self.errors.push(MemError {
                pattern: pattern.to_string(),
                offset: i as u64 * 8,
                expected,
                actual,
            });
        }
    }

    /// Calls `f(self, start, end)` per chunk; `false` if cancelled.
    fn chunks(&mut self, mut f: impl FnMut(&mut Self, usize, usize)) -> bool {
        let n = self.buf.len();
        let mut start = 0;
        while start < n {
            let end = (start + CHUNK_WORDS).min(n);
            f(self, start, end);
            if !(self.tick)(((end - start) * 8) as u64) {
                return false;
            }
            start = end;
        }
        true
    }

    /// Write `f(i)` to every word.
    pub fn fill(&mut self, f: impl Fn(usize) -> u64) -> bool {
        self.chunks(|t, s, e| {
            let p = t.buf.as_mut_ptr();
            for i in s..e {
                // SAFETY: i < buf.len(); volatile so the compiler keeps every access.
                unsafe { std::ptr::write_volatile(p.add(i), f(i)) };
            }
        })
    }

    /// Compare every word with `f(i)`.
    pub fn check(&mut self, pattern: &str, f: impl Fn(usize) -> u64) -> bool {
        self.chunks(|t, s, e| {
            let p = t.buf.as_ptr();
            for i in s..e {
                // SAFETY: as in `fill`.
                let v = unsafe { std::ptr::read_volatile(p.add(i)) };
                let want = f(i);
                if v != want {
                    t.report(pattern, i, want, v);
                }
            }
        })
    }

    /// Compare every word with `f(i)` and write its complement back.
    fn flip(&mut self, pattern: &str, f: impl Fn(usize) -> u64) -> bool {
        self.chunks(|t, s, e| {
            let p = t.buf.as_mut_ptr();
            for i in s..e {
                // SAFETY: as in `fill`.
                let v = unsafe { std::ptr::read_volatile(p.add(i)) };
                let want = f(i);
                if v != want {
                    t.report(pattern, i, want, v);
                }
                unsafe { std::ptr::write_volatile(p.add(i), !v) };
            }
        })
    }

    fn fill_check(&mut self, pattern: &str, f: impl Fn(usize) -> u64 + Copy) -> bool {
        self.fill(f) && self.check(pattern, f)
    }

    /// One pass of all patterns; `status(name)` names the pattern being run. `false` if
    /// cancelled.
    pub fn pass(&mut self, pass: u32, status: &mut dyn FnMut(&str)) -> bool {
        status("solid bits");
        if !self.fill_check("solid 0x00", |_| 0) || !self.fill_check("solid 0xff", |_| !0) {
            return false;
        }
        status("checkerboard");
        const CB: u64 = 0x5555_5555_5555_5555;
        if !self.fill_check("checkerboard", |i| if i % 2 == 0 { CB } else { !CB })
            || !self.fill_check("checkerboard inv", |i| if i % 2 == 0 { !CB } else { CB })
        {
            return false;
        }
        status("walking ones");
        for k in 0..64 {
            let f = move |i: usize| 1u64.rotate_left(((i + k) % 64) as u32);
            if !self.fill_check("walking ones", f) {
                return false;
            }
        }
        status("walking zeros");
        for k in 0..64 {
            let f = move |i: usize| !1u64.rotate_left(((i + k) % 64) as u32);
            if !self.fill_check("walking zeros", f) {
                return false;
            }
        }
        status("address in address");
        let base = self.base;
        if !self.fill_check("address", move |i| base.wrapping_add(i as u64 * 8))
            || !self.fill_check("address inv", move |i| !base.wrapping_add(i as u64 * 8))
        {
            return false;
        }
        status("random data");
        let seed = 0x5EED_0000_0000_0000 ^ u64::from(pass);
        if !self.fill_check("random", move |i| splitmix(seed.wrapping_add(i as u64))) {
            return false;
        }
        status("bit flip");
        let p = splitmix(seed ^ 0xF11F);
        let start = move |i: usize| if i.is_multiple_of(2) { p } else { !p };
        if !self.fill(start) {
            return false;
        }
        for k in 0..BITFLIPS {
            let f = move |i: usize| if k % 2 == 0 { start(i) } else { !start(i) };
            if !self.flip("bit flip", f) {
                return false;
            }
        }
        // After an even number of flips the words hold the start pattern again.
        self.check("bit flip", start)
    }
}

/// Anonymous memory, locked with `mlock` when possible, unmapped on drop.
struct TestBuffer {
    ptr: *mut u64,
    bytes: usize,
    locked: bool,
}

// SAFETY: the mapping is owned by this value and only accessed through `&mut`.
unsafe impl Send for TestBuffer {}

impl TestBuffer {
    fn new(bytes: usize) -> Result<TestBuffer, String> {
        // SAFETY: anonymous private mapping, checked for MAP_FAILED.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!(
                "Cannot allocate {}: {}",
                format_bytes(bytes as u64),
                std::io::Error::last_os_error()
            ));
        }
        let mut b = TestBuffer {
            ptr: ptr.cast(),
            bytes,
            locked: false,
        };
        // SAFETY: the range is the mapping above.
        if unsafe { libc::mlock(ptr, bytes) } == 0 {
            b.locked = true;
        } else {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOMEM) && is_root() {
                // Not enough memory to lock it all: writing it would invoke the OOM killer.
                return Err(format!(
                    "Cannot lock {} in memory ({e}). Choose a smaller share.",
                    format_bytes(bytes as u64)
                ));
            }
            log::warn!("memtest: mlock failed ({e}), testing unlocked memory");
        }
        Ok(b)
    }

    fn words(&mut self) -> &mut [u64] {
        // SAFETY: the mapping is `bytes` long, page aligned and zero-filled.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.bytes / 8) }
    }
}

impl Drop for TestBuffer {
    fn drop(&mut self) {
        // SAFETY: the mapping created in `new`.
        unsafe {
            if self.locked {
                libc::munlock(self.ptr.cast(), self.bytes);
            }
            libc::munmap(self.ptr.cast(), self.bytes);
        }
    }
}

fn is_root() -> bool {
    // SAFETY: no arguments.
    unsafe { libc::geteuid() == 0 }
}

/// Run the memory test (in a worker thread).
pub fn run(proc_root: &Path, opts: &MemOptions, ctx: &TaskCtx) -> Result<TestReport, TaskFailure> {
    let info = mem_info(proc_root).map_err(TaskFailure::Error)?;
    let size = test_size(info.available, opts);
    if size < MIB {
        return Err(TaskFailure::Error(format!(
            "Not enough free memory to test (MemAvailable {}).",
            format_bytes(info.available)
        )));
    }
    ctx.set_message(format!("Allocating and locking {}", format_bytes(size)));
    let mut buf = TestBuffer::new(size as usize).map_err(TaskFailure::Error)?;
    let locked = buf.locked;
    let total = size * SWEEPS_PER_PASS * u64::from(opts.passes);
    ctx.set_progress(0, total);
    let mut done = 0u64;
    let mut tick = |n: u64| {
        done += n;
        ctx.set_progress(done, total);
        !ctx.is_cancelled()
    };
    let mut tester = Tester::new(buf.words(), &mut tick);
    let mut completed = 0;
    for pass in 1..=opts.passes {
        let mut status = |name: &str| {
            ctx.set_message(format!("Pass {pass}/{}: {name}", opts.passes));
        };
        if !tester.pass(pass, &mut status) {
            break;
        }
        completed = pass;
    }
    let (errors, error_count) = (std::mem::take(&mut tester.errors), tester.error_count);
    drop(tester);
    drop(buf);

    let mut details = String::new();
    let _ = writeln!(
        details,
        "Tested {} of {} available ({} % requested, {} total RAM){}.",
        format_bytes(size),
        format_bytes(info.available),
        opts.percent,
        format_bytes(info.total),
        if locked {
            ", locked with mlock"
        } else {
            ", NOT locked (mlock failed)"
        }
    );
    let _ = writeln!(details, "Passes completed: {completed} of {}.", opts.passes);
    let _ = writeln!(
        details,
        "Patterns: solid bits, checkerboard, walking ones/zeros, address in address, \
         random data, bit flip."
    );
    if error_count > 0 {
        let _ = writeln!(details, "\n{error_count} failing word(s):");
        for e in &errors {
            let _ = writeln!(details, "  {}", e.line());
        }
        if error_count > errors.len() as u64 {
            let _ = writeln!(
                details,
                "  ... and {} more",
                error_count - errors.len() as u64
            );
        }
    }
    let _ = writeln!(details, "\n{}", crate::texts::MEMTEST_NOTE);
    let target = format!(
        "RAM, {} of {}",
        format_bytes(size),
        format_bytes(info.total)
    );
    let aborted = completed < opts.passes;
    let (outcome, summary) = match (error_count, aborted) {
        (0, false) => (
            Outcome::Pass,
            format!(
                "{completed} pass(es) over {}, no errors.",
                format_bytes(size)
            ),
        ),
        (0, true) => (
            Outcome::Aborted,
            format!("Cancelled after {completed} complete pass(es), no errors so far."),
        ),
        (n, _) => (
            Outcome::Fail,
            format!(
                "{n} memory error(s) in {}{}.",
                format_bytes(size),
                if aborted { " (cancelled)" } else { "" }
            ),
        ),
    };
    Ok(TestReport {
        outcome,
        target,
        summary,
        details,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_and_size() {
        let info = parse_meminfo(
            "MemTotal:        1000000 kB\nMemFree:  10 kB\nMemAvailable:     800000 kB\n",
        )
        .expect("parse");
        assert_eq!(info.total, 1_000_000 * 1024);
        assert_eq!(info.available, 800_000 * 1024);
        assert!(parse_meminfo("MemTotal: 1 kB\n").is_none());
        let o = MemOptions::default();
        // 80 % of 781 MiB, rounded down to MiB
        assert_eq!(test_size(info.available, &o), 625 * MIB);
        // at least 64 MiB stay free
        assert_eq!(
            test_size(
                100 * MIB,
                &MemOptions {
                    percent: 95,
                    ..o.clone()
                }
            ),
            36 * MIB
        );
        assert_eq!(test_size(10 * MIB, &o), 0);
        let sim = MemOptions {
            max_bytes: Some(SIM_MAX_BYTES),
            ..o
        };
        assert_eq!(test_size(64 << 30, &sim), SIM_MAX_BYTES);
    }

    #[test]
    fn clean_buffer_passes() {
        let mut buf = vec![0u64; CHUNK_WORDS * 2 + 3];
        let mut ticks = 0u64;
        let mut tick = |n: u64| {
            ticks += n;
            true
        };
        let mut t = Tester::new(&mut buf, &mut tick);
        let mut names = Vec::new();
        assert!(t.pass(1, &mut |n| names.push(n.to_string())));
        assert_eq!(t.error_count, 0);
        drop(t);
        assert_eq!(ticks, SWEEPS_PER_PASS * (CHUNK_WORDS as u64 * 2 + 3) * 8);
        assert_eq!(names.len(), 7);
    }

    #[test]
    fn corrupted_word_is_reported() {
        let mut buf = vec![0u64; 100];
        let mut tick = |_| true;
        let mut t = Tester::new(&mut buf, &mut tick);
        assert!(t.fill(|_| 0xAAAA));
        // a stuck bit at word 7
        t.buf[7] ^= 1 << 3;
        assert!(t.check("solid", |_| 0xAAAA));
        assert_eq!(t.error_count, 1);
        assert_eq!(
            t.errors[0],
            MemError {
                pattern: "solid".into(),
                offset: 56,
                expected: 0xAAAA,
                actual: 0xAAA2,
            }
        );
        assert!(t.errors[0].line().contains("bits 0x0000000000000008"));
    }

    #[test]
    fn cancel_stops_the_pass() {
        let mut buf = vec![0u64; CHUNK_WORDS * 4];
        let mut calls = 0;
        let mut tick = |_| {
            calls += 1;
            calls < 3
        };
        let mut t = Tester::new(&mut buf, &mut tick);
        assert!(!t.pass(1, &mut |_| {}));
        drop(t);
        assert_eq!(calls, 3);
    }

    #[test]
    fn locked_buffer_round_trip() {
        let mut b = TestBuffer::new(2 * MIB as usize).expect("mmap");
        let w = b.words();
        assert_eq!(w.len(), (2 * MIB / 8) as usize);
        w[0] = 1;
        w[w.len() - 1] = 2;
        assert_eq!(w[0] + w[w.len() - 1], 3);
    }
}
