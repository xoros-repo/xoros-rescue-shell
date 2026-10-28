//! TST-6: file system check of the persistent partitions (`overlay`, `application`):
//! `e2fsck -f -n` (check only) and, after typed confirmation, `e2fsck -f -y` (repair). The
//! partitions must not be mounted: the rescue shell's own mounts are released first
//! ([`SystemBackend::release_partition`]); a mount elsewhere (e.g. from the subshell) refuses
//! the check. The e2fsck output is shown. Cancel stops the running e2fsck.

use std::fmt::Write as _;

use super::{worst, Outcome, TestReport};
use crate::backend::{PartRole, SystemBackend};
use crate::backup::tools::{CommandOutput, ToolError};
use crate::tasks::{TaskCtx, TaskFailure};
use crate::texts::FSCK_REPAIR_WORD;

/// Lines of e2fsck output kept per partition.
const MAX_OUTPUT_LINES: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsckRequest {
    pub roles: Vec<PartRole>,
    /// `e2fsck -f -y` instead of `-f -n`.
    pub repair: bool,
    /// For a repair: what the user typed, must be [`FSCK_REPAIR_WORD`] (UI-4).
    pub confirmation: String,
}

/// e2fsck arguments (TST-6).
pub fn args(repair: bool, device: &str) -> [&str; 3] {
    ["-f", if repair { "-y" } else { "-n" }, device]
}

/// Meaning of an e2fsck exit code (bit mask, e2fsck(8)) for a check or a repair.
pub fn interpret(code: Option<i32>, repair: bool) -> (Outcome, &'static str) {
    match code {
        Some(0) => (Outcome::Pass, "clean"),
        Some(c) if c & 8 != 0 => (Outcome::Error, "e2fsck operational error"),
        Some(c) if c & 4 != 0 && repair => (Outcome::Fail, "errors left uncorrected"),
        Some(c) if c & 4 != 0 => (Outcome::Fail, "errors found (repair offered)"),
        Some(c) if c & 3 != 0 => (Outcome::Warning, "errors corrected"),
        Some(_) => (Outcome::Error, "e2fsck failed"),
        None => (Outcome::Error, "e2fsck was killed"),
    }
}

fn output_text(out: &CommandOutput) -> String {
    let text = format!("{}\n{}", out.stdout.trim_end(), out.stderr.trim_end());
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let skip = lines.len().saturating_sub(MAX_OUTPUT_LINES);
    let mut t = String::new();
    if skip > 0 {
        let _ = writeln!(t, "    ... {skip} line(s) omitted");
    }
    for l in &lines[skip..] {
        let _ = writeln!(t, "    {l}");
    }
    t
}

/// Run the check or repair (in a write task: nothing else may use the partitions).
pub fn run(
    backend: &dyn SystemBackend,
    req: &FsckRequest,
    ctx: &TaskCtx,
) -> Result<TestReport, TaskFailure> {
    if req.roles.is_empty() {
        return Err(TaskFailure::Error("No partition selected.".into()));
    }
    if req.repair && req.confirmation.trim() != FSCK_REPAIR_WORD {
        return Err(TaskFailure::Error(format!(
            "Not confirmed: type {FSCK_REPAIR_WORD} to repair."
        )));
    }
    let layout = backend.layout().map_err(TaskFailure::from)?;
    let runner = backend.command_runner();
    let mut outcome = Outcome::Pass;
    let mut summary = Vec::new();
    let mut details = String::new();
    ctx.set_progress(0, req.roles.len() as u64);
    for (i, &role) in req.roles.iter().enumerate() {
        ctx.check_cancelled()?;
        let label = role.label();
        let Some(device) = layout.get(role).map(|e| e.device.clone()) else {
            return Err(TaskFailure::Error(format!("No {label} partition.")));
        };
        ctx.set_message(format!("Unmounting {label}"));
        if let Err(e) = backend.release_partition(role) {
            outcome = worst(outcome, Outcome::Error);
            summary.push(format!("{label}: not checked"));
            let _ = writeln!(details, "{label} ({device}): not checked: {e}\n");
            continue;
        }
        let a = args(req.repair, &device);
        let cmd = format!("e2fsck {}", a.join(" "));
        ctx.set_message(format!("{cmd} ({label})"));
        if req.repair {
            log::warn!("TST-6: repairing {label}: {cmd}");
        }
        // Cancel kills the running e2fsck (TST-6), not only the next partition.
        let ran = runner.run_cancellable("e2fsck", &a, ctx.cancel_token());
        if ctx.is_cancelled() {
            if req.repair {
                log::warn!("TST-6: repair of {label} interrupted; check the file system again");
            }
            return Err(TaskFailure::Cancelled);
        }
        match ran {
            Ok(out) => {
                let (o, what) = interpret(out.code, req.repair);
                outcome = worst(outcome, o);
                summary.push(format!("{label}: {what}"));
                let _ = writeln!(
                    details,
                    "{label} ({device}): {o}, {what} (exit code {})\n  $ {cmd}\n{}",
                    out.code.map_or("signal".to_string(), |c| c.to_string()),
                    output_text(&out)
                );
                log::info!("TST-6: {cmd}: exit code {:?}", out.code);
            }
            Err(e @ ToolError::Missing(_)) => {
                return Err(TaskFailure::Error(format!("{e} (e2fsprogs).")))
            }
            Err(e) => {
                outcome = worst(outcome, Outcome::Error);
                summary.push(format!("{label}: {e}"));
                let _ = writeln!(details, "{label} ({device}): {e}\n");
            }
        }
        ctx.set_progress(i as u64 + 1, req.roles.len() as u64);
    }
    Ok(TestReport {
        outcome,
        target: req
            .roles
            .iter()
            .map(|r| r.label())
            .collect::<Vec<_>>()
            .join(" + "),
        summary: summary.join("; "),
        details,
    })
}

/// The partitions of a check result that need a repair (e2fsck found errors).
pub fn needs_repair(details: &str) -> Vec<PartRole> {
    [PartRole::Overlay, PartRole::Application]
        .into_iter()
        .filter(|r| {
            details
                .lines()
                .any(|l| l.starts_with(&format!("{} (", r.label())) && l.contains("errors found"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::hw::testutil::TempDir;
    use crate::tasks::{lock, TaskKind, TaskRunner};
    use std::sync::{Arc, Mutex};

    #[test]
    fn exit_codes() {
        assert_eq!(interpret(Some(0), false).0, Outcome::Pass);
        assert_eq!(interpret(Some(4), false).0, Outcome::Fail);
        assert_eq!(interpret(Some(1), true).0, Outcome::Warning);
        assert_eq!(interpret(Some(5), true).0, Outcome::Fail);
        assert_eq!(interpret(Some(8), false).0, Outcome::Error);
        assert_eq!(interpret(None, false).0, Outcome::Error);
        assert_eq!(args(false, "/dev/sda5"), ["-f", "-n", "/dev/sda5"]);
        assert_eq!(args(true, "/dev/sda5"), ["-f", "-y", "/dev/sda5"]);
    }

    fn run_sim(d: &TempDir, req: FsckRequest) -> Result<TestReport, TaskFailure> {
        let b = Arc::new(SimBackend::open(d.path()).expect("open"));
        let out = Arc::new(Mutex::new(None));
        let o2 = Arc::clone(&out);
        TaskRunner::new()
            .spawn(
                "fsck",
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
    }

    #[test]
    fn check_and_repair_in_simulation() {
        let d = TempDir::new("fsck");
        SimBackend::create_example(d.path()).expect("example");
        let req = FsckRequest {
            roles: vec![PartRole::Overlay, PartRole::Application],
            repair: false,
            confirmation: String::new(),
        };
        let r = run_sim(&d, req.clone()).expect("check");
        assert_eq!(r.outcome, Outcome::Pass, "{r:?}");
        assert!(r.details.contains("$ e2fsck -f -n /dev/sda5"));
        assert!(needs_repair(&r.details).is_empty());

        // e2fsck finds errors (simulated exit code 4).
        let b = SimBackend::open(d.path()).expect("open");
        let mut s = b.load().expect("load");
        s.fail
            .insert("e2fsck".into(), "overlay: Inode 12 has a bad extent".into());
        d.write(
            "sim.json",
            serde_json::to_string(&s).expect("json").as_bytes(),
        );
        let r = run_sim(&d, req.clone()).expect("check");
        assert_eq!(r.outcome, Outcome::Fail);
        assert!(r.details.contains("bad extent"));
        assert_eq!(
            needs_repair(&r.details),
            vec![PartRole::Overlay, PartRole::Application]
        );

        // A repair needs the confirmation word.
        let repair = FsckRequest {
            repair: true,
            ..req.clone()
        };
        assert!(matches!(
            run_sim(&d, repair.clone()),
            Err(TaskFailure::Error(_))
        ));
        let r = run_sim(
            &d,
            FsckRequest {
                confirmation: "REPAIR".into(),
                ..repair
            },
        )
        .expect("repair");
        assert!(r.details.contains("$ e2fsck -f -y /dev/sda5"));

        // A partition that cannot be released is reported, the other one is still checked.
        s.fail.clear();
        s.fail
            .insert("release_partition.overlay".into(), "mounted at /mnt".into());
        d.write(
            "sim.json",
            serde_json::to_string(&s).expect("json").as_bytes(),
        );
        let r = run_sim(&d, req).expect("check");
        assert_eq!(r.outcome, Outcome::Error);
        assert!(r
            .details
            .contains("overlay (/dev/sda5): not checked: mounted at /mnt"));
        assert!(r.details.contains("application (/dev/sda6): PASS"));
    }
}
