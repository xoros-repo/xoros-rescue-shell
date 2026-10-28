//! TST-9: CPU stress / thermal test. One worker thread per CPU runs a fixed integer and
//! floating point computation for the chosen time and compares every result with the value
//! computed before the test (a wrong result means an unstable CPU, e.g. under-voltage).
//! Meanwhile the temperatures, the CPU frequency and, on the Raspberry Pi, the firmware's
//! throttling flags (`get_throttled`) are sampled.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Outcome, TestReport};
use crate::hw::{read_temperatures, read_trimmed, HostPaths, Throttled};
use crate::tasks::{format_duration, TaskCtx, TaskFailure};

/// Default and accepted test length in minutes.
pub const DEFAULT_MINUTES: u32 = 5;
pub const MINUTES_RANGE: std::ops::RangeInclusive<u32> = 1..=120;

/// Temperature from which the result is a warning (the Pi throttles at 80-85 C).
pub const HOT_CELSIUS: f64 = 85.0;

const SAMPLE_EVERY: Duration = Duration::from_secs(2);
/// Sample lines kept in the report.
const MAX_SAMPLE_LINES: usize = 40;

const THROTTLED_REL: &str = "devices/platform/soc/soc:firmware/get_throttled";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StressOptions {
    pub duration: Duration,
}

impl Default for StressOptions {
    fn default() -> Self {
        StressOptions {
            duration: Duration::from_secs(u64::from(DEFAULT_MINUTES) * 60),
        }
    }
}

/// One unit of work: integer mixing and dependent floating point operations. Deterministic,
/// so every run must give the same value.
pub fn work_unit(seed: u64) -> u64 {
    let mut x = seed;
    let mut f = 1.000_000_1f64;
    for i in 0..20_000u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x = x.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(i);
        f = (f * 1.000_000_3 + (x >> 40) as f64 * 1e-9).sqrt() + 0.5;
    }
    x ^ f.to_bits()
}

/// Bits of `get_throttled` that are "now" states, and the sticky "occurred" bits.
const NOW_MASK: u32 = 0xF;
const UNDER_VOLTAGE: u32 = (1 << 0) | (1 << 16);

/// What the sampler saw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Samples {
    pub count: u32,
    /// Highest temperature per zone label.
    pub max: Vec<(String, f64)>,
    pub first_max: Option<f64>,
    pub last_max: Option<f64>,
    pub throttled_start: Option<u32>,
    /// OR of all throttling values seen during the test, without the bits already set at
    /// the start.
    pub throttled_new: u32,
    pub freq_khz: Option<(u64, u64)>,
    pub lines: Vec<String>,
}

impl Samples {
    pub fn hottest(&self) -> Option<f64> {
        self.max.iter().map(|(_, t)| *t).reduce(f64::max)
    }

    /// Record one sample.
    pub fn add(
        &mut self,
        at: Duration,
        temps: &[(String, f64)],
        throttled: Option<u32>,
        freq: Option<u64>,
    ) {
        self.count += 1;
        for (zone, t) in temps {
            match self.max.iter_mut().find(|(z, _)| z == zone) {
                Some((_, m)) => *m = m.max(*t),
                None => self.max.push((zone.clone(), *t)),
            }
        }
        let now_max = temps.iter().map(|(_, t)| *t).reduce(f64::max);
        if self.first_max.is_none() {
            self.first_max = now_max;
        }
        self.last_max = now_max.or(self.last_max);
        if let Some(v) = throttled {
            match self.throttled_start {
                None => self.throttled_start = Some(v & !NOW_MASK),
                Some(start) => self.throttled_new |= v & !start,
            }
            self.throttled_new |= v & NOW_MASK;
        }
        if let Some(f) = freq {
            self.freq_khz = Some(match self.freq_khz {
                Some((lo, hi)) => (lo.min(f), hi.max(f)),
                None => (f, f),
            });
        }
        let mut line = format!("  {:>6}", format_duration(at));
        match now_max {
            Some(t) => {
                let _ = write!(line, "  {t:5.1} C");
            }
            None => line.push_str("      - C"),
        }
        if let Some(f) = freq {
            let _ = write!(line, "  {:>5} MHz", f / 1000);
        }
        if let Some(v) = throttled {
            let _ = write!(line, "  throttled 0x{v:x}");
        }
        self.lines.push(line);
    }
}

/// Judge the samples and the computation errors.
pub fn verdict(s: &Samples, errors: u64) -> (Outcome, Vec<String>) {
    let mut out = Outcome::Pass;
    let mut why = Vec::new();
    if errors > 0 {
        out = Outcome::Fail;
        why.push(format!(
            "{errors} wrong computation result(s): the CPU is not stable"
        ));
    }
    if s.throttled_new & UNDER_VOLTAGE != 0 {
        out = Outcome::Fail;
        why.push("under-voltage during the test: use a stronger power supply".into());
    }
    let other = s.throttled_new & !UNDER_VOLTAGE;
    if other != 0 {
        out = out.max(Outcome::Warning);
        let flags = Throttled::parse(&format!("{other:x}"))
            .map(|t| t.flags.join(", "))
            .unwrap_or_default();
        why.push(format!("throttling during the test: {flags}"));
    }
    if let Some(t) = s.hottest() {
        if t >= HOT_CELSIUS {
            out = out.max(Outcome::Warning);
            why.push(format!("temperature reached {t:.1} C: check the cooling"));
        }
    }
    (out, why)
}

fn cpu_freq_khz(paths: &HostPaths) -> Option<u64> {
    read_trimmed(&paths.sys("devices/system/cpu/cpu0/cpufreq/scaling_cur_freq"))?
        .parse()
        .ok()
}

fn sample_now(paths: &HostPaths) -> (Vec<(String, f64)>, Option<u32>, Option<u64>) {
    let temps = read_temperatures(paths)
        .into_iter()
        .map(|t| (t.kind.unwrap_or(t.zone), t.celsius))
        .collect();
    let throttled = read_trimmed(&paths.sys(THROTTLED_REL))
        .and_then(|v| Throttled::parse(&v))
        .map(|t| t.raw);
    (temps, throttled, cpu_freq_khz(paths))
}

/// Run the stress test (in a worker task).
pub fn run(
    paths: &HostPaths,
    opts: &StressOptions,
    ctx: &TaskCtx,
) -> Result<TestReport, TaskFailure> {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let seed = 0x5EED_0F_C0DE;
    let expected = work_unit(seed);
    let stop = Arc::new(AtomicBool::new(false));
    let units = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let mut workers = Vec::new();
    for i in 0..cpus {
        let (stop, units, errors) = (Arc::clone(&stop), Arc::clone(&units), Arc::clone(&errors));
        let w = std::thread::Builder::new()
            .name(format!("stress-{i}"))
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if work_unit(std::hint::black_box(seed)) != expected {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                    units.fetch_add(1, Ordering::Relaxed);
                }
            });
        match w {
            Ok(h) => workers.push(h),
            Err(e) => log::warn!("TST-9: worker {i} not started: {e}"),
        }
    }
    if workers.is_empty() {
        return Err(TaskFailure::Error(
            "No worker thread could be started.".into(),
        ));
    }
    log::info!(
        "TST-9: {} worker(s) for {}",
        workers.len(),
        format_duration(opts.duration)
    );
    let total_ms = opts.duration.as_millis() as u64;
    let mut samples = Samples::default();
    let every = SAMPLE_EVERY.max(opts.duration / MAX_SAMPLE_LINES as u32);
    let t0 = Instant::now();
    let mut next_sample = Duration::ZERO;
    let mut next_line = Duration::ZERO;
    let mut cancelled = false;
    loop {
        let el = t0.elapsed();
        if el >= next_sample {
            let (temps, throttled, freq) = sample_now(paths);
            let keep = el >= next_line;
            let before = samples.lines.len();
            samples.add(el, &temps, throttled, freq);
            if !keep {
                samples.lines.truncate(before);
            } else {
                next_line = el + every;
            }
            next_sample = el + SAMPLE_EVERY;
            let temp = samples
                .last_max
                .map_or(String::new(), |t| format!(", {t:.1} C"));
            ctx.set_message(format!(
                "Stress on {} CPU(s){temp}, {} left",
                workers.len(),
                format_duration(opts.duration.saturating_sub(el))
            ));
        }
        ctx.set_progress((el.as_millis() as u64).min(total_ms), total_ms);
        if el >= opts.duration {
            break;
        }
        if ctx.is_cancelled() {
            cancelled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let ran = t0.elapsed();
    stop.store(true, Ordering::Relaxed);
    for w in workers.drain(..) {
        let _ = w.join();
    }
    // The final state after the load.
    let (temps, throttled, freq) = sample_now(paths);
    samples.add(ran, &temps, throttled, freq);

    let errors = errors.load(Ordering::Relaxed);
    let units = units.load(Ordering::Relaxed);
    let (mut outcome, why) = verdict(&samples, errors);
    let mut d = String::new();
    let _ = writeln!(
        d,
        "{cpus} worker(s) for {} of {}: {units} work units, {errors} wrong result(s).",
        format_duration(ran),
        format_duration(opts.duration)
    );
    match (samples.first_max, samples.hottest()) {
        (Some(a), Some(m)) => {
            let _ = writeln!(d, "Temperature: {a:.1} C at the start, highest {m:.1} C.");
            for (z, t) in &samples.max {
                let _ = writeln!(d, "  {z}: highest {t:.1} C");
            }
        }
        _ => {
            let _ = writeln!(d, "No temperature sensor found (/sys/class/thermal).");
        }
    }
    if let Some((lo, hi)) = samples.freq_khz {
        let _ = writeln!(d, "CPU0 frequency: {} - {} MHz.", lo / 1000, hi / 1000);
    }
    match samples.throttled_start {
        Some(start) => {
            let _ = writeln!(
                d,
                "Pi throttling flags: 0x{start:x} before the test, new during the test 0x{:x}.",
                samples.throttled_new
            );
        }
        None => {
            let _ = writeln!(
                d,
                "No firmware throttling information (not a Raspberry Pi)."
            );
        }
    }
    for w in &why {
        let _ = writeln!(d, "-> {w}");
    }
    let _ = writeln!(d, "\nSamples (time, hottest zone, CPU0 MHz, throttling):");
    for l in &samples.lines {
        let _ = writeln!(d, "{l}");
    }
    let summary = if cancelled {
        outcome = outcome.max(Outcome::Aborted);
        format!("Cancelled after {}.", format_duration(ran))
    } else if why.is_empty() {
        format!(
            "{} on {cpus} CPU(s) without errors{}.",
            format_duration(ran),
            samples
                .hottest()
                .map_or(String::new(), |t| format!(", highest {t:.1} C"))
        )
    } else {
        why.join("; ")
    };
    Ok(TestReport {
        outcome,
        target: format!("{cpus} CPU(s)"),
        summary,
        details: d,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::testutil::TempDir;
    use crate::tasks::{lock, TaskKind, TaskRunner};
    use std::sync::Mutex;

    #[test]
    fn work_unit_is_deterministic() {
        assert_eq!(work_unit(1), work_unit(1));
        assert_ne!(work_unit(1), work_unit(2));
    }

    #[test]
    fn samples_and_verdict() {
        let mut s = Samples::default();
        let t = |c: f64| vec![("cpu-thermal".to_string(), c)];
        // 0x50000: under-voltage and throttling occurred before the test.
        s.add(Duration::ZERO, &t(45.0), Some(0x50000), Some(1_500_000));
        s.add(
            Duration::from_secs(2),
            &t(70.0),
            Some(0x50000),
            Some(1_500_000),
        );
        assert_eq!(verdict(&s, 0), (Outcome::Pass, vec![]));
        // Soft temperature limit now (bit 3) and the frequency cap occurred (bit 17).
        s.add(
            Duration::from_secs(4),
            &t(86.0),
            Some(0x70008),
            Some(1_000_000),
        );
        let (o, why) = verdict(&s, 0);
        assert_eq!(o, Outcome::Warning);
        assert!(why[0].contains("soft temperature limit now"), "{why:?}");
        assert!(why[0].contains("ARM frequency capping occurred"), "{why:?}");
        assert!(why[1].contains("86.0 C"));
        assert_eq!(s.freq_khz, Some((1_000_000, 1_500_000)));
        assert_eq!(s.first_max, Some(45.0));
        s.add(Duration::from_secs(6), &t(60.0), Some(0x50001), None);
        assert_eq!(verdict(&s, 0).0, Outcome::Fail);
        assert_eq!(verdict(&Samples::default(), 3).0, Outcome::Fail);
        assert!(
            s.lines[2].contains("86.0 C   1000 MHz  throttled 0x70008"),
            "{:?}",
            s.lines
        );
    }

    #[test]
    fn short_run_with_fake_sensors() {
        let d = TempDir::new("stress");
        d.write("sys/class/thermal/thermal_zone0/temp", b"48500\n");
        d.write("sys/class/thermal/thermal_zone0/type", b"cpu-thermal\n");
        d.write(
            "sys/devices/platform/soc/soc:firmware/get_throttled",
            b"0x0\n",
        );
        let paths = HostPaths {
            proc_root: d.path().join("proc"),
            sys_root: d.path().join("sys"),
        };
        let out = Arc::new(Mutex::new(None));
        let o2 = Arc::clone(&out);
        TaskRunner::new()
            .spawn(
                "stress",
                TaskKind::ReadOnly,
                move |ctx| {
                    let opts = StressOptions {
                        duration: Duration::from_millis(500),
                    };
                    *lock(&o2) = Some(run(&paths, &opts, ctx));
                    Ok(String::new())
                },
                None,
            )
            .expect("spawn")
            .wait();
        let r = lock(&out).take().expect("ran").expect("report");
        assert_eq!(r.outcome, Outcome::Pass, "{r:?}");
        assert!(r.summary.contains("highest 48.5 C"), "{}", r.summary);
        assert!(r.details.contains("0 wrong result(s)"));
        assert!(r
            .details
            .contains("Pi throttling flags: 0x0 before the test"));
    }
}
