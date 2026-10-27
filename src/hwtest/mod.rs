//! Hardware and integrity tests (TST-*): memory test (TST-1), disk read test (TST-3), slot
//! integrity (TST-4), rescue integrity (TST-5) and file system check (TST-6).
//!
//! Every test runs inside a [`TaskRunner`](crate::tasks::TaskRunner) worker (progress and
//! cancel, UI-3) through [`run_recorded`], which turns the outcome into a [`TestResult`]
//! (pass/warning/fail/aborted/error, duration, details), logs it (LOG-3) and keeps it for the
//! session in [`ResultStore`]: in memory and in `<run dir>/test-results/`, so later CLI
//! calls, a restarted TUI and the diagnostics bundle (LOG-2) see it too. No cursive (ARCH-1).

pub mod diskread;
pub mod fsck;
pub mod memtest;
pub mod slotcheck;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::backend::{PartRole, SystemBackend};
use crate::slots::RescueCheck;
use crate::tasks::{lock, TaskCtx, TaskFailure, TaskKind};

/// Directory below the run directory with the results of this boot; the diagnostics bundle
/// (LOG-2) includes every file in it.
pub use crate::constants::TEST_RESULTS_SUBDIR as RESULTS_SUBDIR;
/// All results of this boot (JSON), in [`RESULTS_SUBDIR`].
pub const RESULTS_FILE: &str = "results.json";

/// Which test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestKind {
    /// TST-1.
    Memory,
    /// TST-3.
    DiskRead,
    /// TST-4.
    SlotIntegrity,
    /// TST-5.
    RescueIntegrity,
    /// TST-6, `e2fsck -f -n`.
    FsCheck,
    /// TST-6, `e2fsck -f -y`.
    FsRepair,
}

impl TestKind {
    pub fn title(self) -> &'static str {
        match self {
            TestKind::Memory => "Memory test",
            TestKind::DiskRead => "Disk read test",
            TestKind::SlotIntegrity => "Slot integrity",
            TestKind::RescueIntegrity => "Rescue integrity",
            TestKind::FsCheck => "File system check",
            TestKind::FsRepair => "File system repair",
        }
    }

    /// Requirement ID.
    pub fn id(self) -> &'static str {
        match self {
            TestKind::Memory => "TST-1",
            TestKind::DiskRead => "TST-3",
            TestKind::SlotIntegrity => "TST-4",
            TestKind::RescueIntegrity => "TST-5",
            TestKind::FsCheck | TestKind::FsRepair => "TST-6",
        }
    }

    /// Text report in [`RESULTS_SUBDIR`] with every result of this kind of this boot.
    pub fn file_name(self) -> &'static str {
        match self {
            TestKind::Memory => "memory.txt",
            TestKind::DiskRead => "disk-read.txt",
            TestKind::SlotIntegrity => "slots.txt",
            TestKind::RescueIntegrity => "rescue.txt",
            TestKind::FsCheck => "fsck.txt",
            TestKind::FsRepair => "fsck-repair.txt",
        }
    }
}

/// How a test ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    /// Passed with remarks (slow blocks, rescue older/newer than both slots, ...).
    Warning,
    /// Cancelled by the user before a problem was found.
    Aborted,
    /// The test found a problem.
    Fail,
    /// The test could not run (missing tool, device not available, ...).
    Error,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::Warning => "WARNING",
            Outcome::Aborted => "ABORTED",
            Outcome::Fail => "FAIL",
            Outcome::Error => "ERROR",
        }
    }

    /// Exit code of the CLI (ARCH-2): 0 pass/warning, 3 the test found a problem, 1 the test
    /// did not complete (error, cancelled).
    pub fn exit_code(self) -> i32 {
        match self {
            Outcome::Pass | Outcome::Warning => 0,
            Outcome::Fail => 3,
            Outcome::Aborted | Outcome::Error => 1,
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What a test body returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestReport {
    pub outcome: Outcome,
    /// What was tested, e.g. `sda (8.0 GiB)`.
    pub target: String,
    /// One line.
    pub summary: String,
    /// Multi-line details for the result dialog and the log.
    pub details: String,
}

/// A finished test, kept for the session (TST: "keep the result for the session").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestResult {
    pub kind: TestKind,
    pub outcome: Outcome,
    pub target: String,
    /// Start time, UTC (`YYYY-MM-DD HH:MM:SS`).
    pub started: String,
    pub duration_secs: f64,
    pub summary: String,
    pub details: String,
}

impl TestResult {
    /// Header line: `TST-1 Memory test: PASS (1:23)  summary`.
    pub fn headline(&self) -> String {
        format!(
            "{} {}: {} ({})  {}",
            self.kind.id(),
            self.kind.title(),
            self.outcome,
            crate::tasks::format_duration(std::time::Duration::from_secs_f64(
                self.duration_secs.max(0.0)
            )),
            self.summary
        )
    }

    /// Full text for the result dialog, the CLI and the diagnostics bundle.
    pub fn text(&self) -> String {
        let mut t = format!(
            "{}\nTarget:   {}\nStarted:  {} UTC\nResult:   {}\nDuration: {:.1} s\n",
            self.kind.title(),
            self.target,
            self.started,
            self.outcome,
            self.duration_secs
        );
        t.push_str(&format!("Summary:  {}\n", self.summary));
        if !self.details.trim().is_empty() {
            t.push('\n');
            t.push_str(self.details.trim_end());
            t.push('\n');
        }
        t
    }
}

/// Results of this session (boot): in memory and in `<run dir>/test-results/`
/// (`results.json` plus one text report per test kind, e.g. `memory.txt`).
pub struct ResultStore {
    dir: Option<PathBuf>,
    results: Mutex<Vec<TestResult>>,
}

/// Between two results in a text report.
const SEPARATOR: &str = "\n------------------------------------------------------------\n\n";

/// Results kept per boot (oldest are dropped).
const MAX_RESULTS: usize = 100;

impl ResultStore {
    /// Store backed by `<run_dir>/test-results/`; earlier results of this boot are loaded.
    pub fn new(run_dir: &Path) -> Self {
        let dir = run_dir.join(RESULTS_SUBDIR);
        let results = std::fs::read_to_string(dir.join(RESULTS_FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        ResultStore {
            dir: Some(dir),
            results: Mutex::new(results),
        }
    }

    /// Store without a file (tests).
    pub fn in_memory() -> Self {
        ResultStore {
            dir: None,
            results: Mutex::new(Vec::new()),
        }
    }

    /// The results directory (`None` for an in-memory store).
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Keep and log a result (LOG-3), and rewrite the results files.
    pub fn record(&self, r: TestResult) {
        match r.outcome {
            Outcome::Pass | Outcome::Warning => log::info!("test: {}", r.headline()),
            Outcome::Aborted => log::warn!("test: {}", r.headline()),
            Outcome::Fail | Outcome::Error => log::error!("test: {}", r.headline()),
        }
        for line in r.details.lines().filter(|l| !l.trim().is_empty()).take(40) {
            log::info!("test:   {line}");
        }
        let kind = r.kind;
        let mut all = lock(&self.results);
        all.push(r);
        let excess = all.len().saturating_sub(MAX_RESULTS);
        all.drain(..excess);
        let Some(dir) = &self.dir else { return };
        let report = all
            .iter()
            .filter(|x| x.kind == kind)
            .map(|x| x.text())
            .collect::<Vec<_>>()
            .join(SEPARATOR);
        let json = serde_json::to_string_pretty(&*all).map_err(|e| e.to_string());
        drop(all);
        let write = |name: &str, text: String| {
            std::fs::write(dir.join(name), text).map_err(|e| e.to_string())
        };
        let written = std::fs::create_dir_all(dir)
            .map_err(|e| e.to_string())
            .and_then(|_| write(kind.file_name(), report))
            .and_then(|_| write(RESULTS_FILE, json? + "\n"));
        if let Err(e) = written {
            log::warn!("cannot write test results to {}: {e}", dir.display());
        }
    }

    /// All results of this session, oldest first.
    pub fn all(&self) -> Vec<TestResult> {
        lock(&self.results).clone()
    }

    /// Text of all results (Tests -> Results, diagnostics bundle).
    pub fn text(&self) -> String {
        let all = self.all();
        if all.is_empty() {
            return "No tests were run in this session.\n".into();
        }
        all.iter()
            .map(|r| r.text())
            .collect::<Vec<_>>()
            .join(SEPARATOR)
    }
}

/// Current time for [`TestResult::started`].
fn now_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Run a test body in the current task, record the result and return it. Cancellation and
/// errors from the body become `Aborted`/`Error` results (the task itself succeeds, so the UI
/// and the CLI always get a result to show).
pub fn run_recorded(
    store: &ResultStore,
    kind: TestKind,
    target: &str,
    ctx: &TaskCtx,
    body: impl FnOnce(&TaskCtx) -> Result<TestReport, TaskFailure>,
) -> TestResult {
    let started = now_utc();
    let t0 = Instant::now();
    let report = match body(ctx) {
        Ok(r) => r,
        Err(TaskFailure::Cancelled) => TestReport {
            outcome: Outcome::Aborted,
            target: target.to_string(),
            summary: "Cancelled.".into(),
            details: String::new(),
        },
        Err(TaskFailure::Error(e)) => TestReport {
            outcome: Outcome::Error,
            target: target.to_string(),
            summary: "The test could not run.".into(),
            details: e,
        },
    };
    let result = TestResult {
        kind,
        outcome: report.outcome,
        target: if report.target.is_empty() {
            target.to_string()
        } else {
            report.target
        },
        started,
        duration_secs: t0.elapsed().as_secs_f64(),
        summary: report.summary,
        details: report.details,
    };
    store.record(result.clone());
    result
}

/// A test to run, from the UI or the CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestRequest {
    Memory(memtest::MemOptions),
    /// The boot disk (`None`) or one of its partitions.
    Disk(Option<PartRole>),
    Slots(slotcheck::SlotTestOptions),
    Rescue,
    Fsck(fsck::FsckRequest),
}

impl TestRequest {
    pub fn kind(&self) -> TestKind {
        match self {
            TestRequest::Memory(_) => TestKind::Memory,
            TestRequest::Disk(_) => TestKind::DiskRead,
            TestRequest::Slots(_) => TestKind::SlotIntegrity,
            TestRequest::Rescue => TestKind::RescueIntegrity,
            TestRequest::Fsck(r) if r.repair => TestKind::FsRepair,
            TestRequest::Fsck(_) => TestKind::FsCheck,
        }
    }

    /// e2fsck runs as a write task: nothing else may use the partitions meanwhile (UI-3).
    pub fn task_kind(&self) -> TaskKind {
        match self {
            TestRequest::Fsck(_) => TaskKind::Write,
            _ => TaskKind::ReadOnly,
        }
    }
}

/// Kernel log for the disk read test (not read in simulation mode).
const KMSG: &str = "/dev/kmsg";

/// Name of the disk read target: kernel name of the node (image file name in simulation).
fn disk_target_name(backend: &dyn SystemBackend, role: Option<PartRole>, path: &Path) -> String {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    match role {
        Some(r) => format!("{base} ({})", r.label()),
        None if backend.is_simulation() => base,
        None => format!("{base} (boot disk)"),
    }
}

/// What a test needs from the app; owned, so it can move into a task.
#[derive(Clone)]
pub struct TestEnv {
    pub backend: Arc<dyn SystemBackend>,
    pub results: Arc<ResultStore>,
    pub rescue_check: Arc<Mutex<RescueCheck>>,
}

impl TestEnv {
    pub fn of(app: &App) -> TestEnv {
        TestEnv {
            backend: Arc::clone(&app.backend),
            results: Arc::clone(&app.test_results),
            rescue_check: Arc::clone(&app.rescue_check),
        }
    }
}

/// Run a test in the current task and record the result (TST-*).
pub fn run_request(env: &TestEnv, req: &TestRequest, ctx: &TaskCtx) -> TestResult {
    let backend = env.backend.as_ref();
    let store = env.results.as_ref();
    let kind = req.kind();
    match req {
        TestRequest::Memory(opts) => {
            let mut opts = opts.clone();
            if backend.is_simulation() {
                opts.max_bytes = Some(memtest::SIM_MAX_BYTES);
            }
            let proc_root = backend.host_paths().proc_root;
            run_recorded(store, kind, "RAM", ctx, |ctx| {
                memtest::run(&proc_root, &opts, ctx)
            })
        }
        TestRequest::Disk(role) => {
            let path = backend.read_test_path(*role);
            let name = match &path {
                Ok(p) => disk_target_name(backend, *role, p),
                Err(_) => role.map_or("boot disk", |r| r.label()).to_string(),
            };
            run_recorded(store, kind, &name, ctx, |ctx| {
                let path = path.map_err(TaskFailure::from)?;
                let dev = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let kmsg = (!backend.is_simulation()).then_some(Path::new(KMSG));
                diskread::run(&path, &dev, kmsg, ctx).map(|mut r| {
                    r.target = r.target.replacen(&dev, &name, 1);
                    r
                })
            })
        }
        TestRequest::Slots(opts) => run_recorded(store, kind, "slots", ctx, |ctx| {
            slotcheck::run_slots(backend, opts, ctx)
        }),
        TestRequest::Rescue => run_recorded(store, kind, "rescue partition", ctx, |ctx| {
            slotcheck::run_rescue(backend, &env.rescue_check, ctx)
        }),
        TestRequest::Fsck(r) => {
            let target = r
                .roles
                .iter()
                .map(|x| x.label())
                .collect::<Vec<_>>()
                .join(" + ");
            run_recorded(store, kind, &target, ctx, |ctx| fsck::run(backend, r, ctx))
        }
    }
}

/// Resolve the disk read target of the CLI (`--device`): `disk` or the boot disk's kernel
/// name, a partition label (`application`) or a partition's kernel name (`sda6`).
pub fn parse_disk_target(
    backend: &dyn SystemBackend,
    name: &str,
) -> Result<Option<PartRole>, String> {
    if name == "disk" {
        return Ok(None);
    }
    if let Some(r) = PartRole::from_label(name) {
        return Ok(Some(r));
    }
    let layout = backend.layout().map_err(|e| e.to_string())?;
    if layout.disk == name {
        return Ok(None);
    }
    layout
        .entries
        .iter()
        .find(|e| Path::new(&e.device).file_name().is_some_and(|n| n == name))
        .map(|e| Some(e.role))
        .ok_or_else(|| {
            format!(
                "Unknown device '{name}': use disk, {} or a partition label.",
                layout.disk
            )
        })
}

/// The worse of two outcomes (for tests over several targets).
pub fn worst(a: Outcome, b: Outcome) -> Outcome {
    a.max(b)
}

/// Upper bound of detail lines of one kind (errors, slow blocks) kept in a report.
pub const MAX_DETAIL_LINES: usize = 50;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{TaskKind, TaskRunner, TaskState};

    #[test]
    fn outcomes_order_and_codes() {
        assert_eq!(worst(Outcome::Pass, Outcome::Warning), Outcome::Warning);
        assert_eq!(worst(Outcome::Fail, Outcome::Warning), Outcome::Fail);
        assert_eq!(worst(Outcome::Aborted, Outcome::Error), Outcome::Error);
        assert_eq!(Outcome::Warning.exit_code(), 0);
        assert_eq!(Outcome::Fail.exit_code(), 3);
        assert_eq!(Outcome::Error.exit_code(), 1);
    }

    #[test]
    fn results_are_recorded_and_persisted() {
        let d = crate::hw::testutil::TempDir::new("hwtest-store");
        let store = std::sync::Arc::new(ResultStore::new(d.path()));
        let runner = TaskRunner::new();
        let s2 = std::sync::Arc::clone(&store);
        let out: std::sync::Arc<Mutex<Vec<TestResult>>> = Default::default();
        let o2 = std::sync::Arc::clone(&out);
        let h = runner
            .spawn(
                "t",
                TaskKind::ReadOnly,
                move |ctx| {
                    let ok = run_recorded(&s2, TestKind::DiskRead, "sda", ctx, |_| {
                        Ok(TestReport {
                            outcome: Outcome::Pass,
                            target: String::new(),
                            summary: "fine".into(),
                            details: "line".into(),
                        })
                    });
                    let err = run_recorded(&s2, TestKind::FsCheck, "overlay", ctx, |_| {
                        Err(TaskFailure::Error("e2fsck missing".into()))
                    });
                    let cancelled = run_recorded(&s2, TestKind::Memory, "RAM", ctx, |_| {
                        Err(TaskFailure::Cancelled)
                    });
                    lock(&o2).extend([ok, err, cancelled]);
                    Ok(String::new())
                },
                None,
            )
            .expect("spawn");
        assert!(matches!(h.wait().state, TaskState::Succeeded(_)));
        let r = lock(&out).clone();
        assert_eq!(r[0].outcome, Outcome::Pass);
        assert_eq!(r[0].target, "sda");
        assert_eq!(r[1].outcome, Outcome::Error);
        assert_eq!(r[1].details, "e2fsck missing");
        assert_eq!(r[2].outcome, Outcome::Aborted);
        assert!(r[0].headline().starts_with("TST-3 Disk read test: PASS"));
        assert!(r[1].text().contains("Result:   ERROR"));
        // A new store (e.g. the next CLI call) sees the results of this boot.
        let again = ResultStore::new(d.path());
        assert_eq!(again.all(), store.all());
        assert_eq!(again.all().len(), 3);
        assert!(again.text().contains("Memory test"));
        let dir = d.path().join(RESULTS_SUBDIR);
        let disk = std::fs::read_to_string(dir.join("disk-read.txt")).expect("disk-read.txt");
        assert!(disk.contains("Result:   PASS"));
        assert!(std::fs::read_to_string(dir.join("fsck.txt")).is_ok_and(|t| t.contains("ERROR")));
        assert!(dir.join("memory.txt").is_file() && dir.join(RESULTS_FILE).is_file());
        assert!(ResultStore::in_memory().text().contains("No tests"));
    }
}
