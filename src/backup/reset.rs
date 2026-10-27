//! Factory reset of the persistent partitions (RST-1, RST-2).

use std::path::Path;

use super::error::{IoContext, Result};
use super::fsops;
use super::import::{recreate_empty_dir, FileError};
use super::meta::{LOST_FOUND, OVERLAY_UPPER_DIR, OVERLAY_WORK_DIR, RESCUE_DIR};
use super::progress::{CancelToken, Phase, ProgressFn, Reporter};

/// Word the user must type to confirm a factory reset (RST-3).
pub const RESET_CONFIRM_WORD: &str = "RESET";

/// True if `input` is the confirmation word (surrounding white space ignored, case sensitive).
pub fn reset_confirmed(input: &str) -> bool {
    input.trim() == RESET_CONFIRM_WORD
}

/// Result of [`factory_reset`].
#[derive(Clone, Debug, Default)]
pub struct ResetReport {
    pub overlay_removed: u64,
    pub application_removed: u64,
    /// Entries that could not be deleted; the reset continues with the others.
    pub errors: Vec<FileError>,
}

/// Factory reset on mounted (read-write) partition roots.
///
/// RST-1 (`application_root = None`): deletes everything on `overlay` except `lost+found` and,
/// if `keep_rescue_settings`, the rescue shell's `rescue/` directory; then recreates empty
/// `overlays/etc/upper` and `overlays/etc/work` so the normal system starts with the `/etc` of
/// the slot. RST-2 (`application_root = Some`): additionally deletes everything on
/// `application` except `lost+found`. Ends with `syncfs`.
///
/// Typed confirmation (RST-3, [`reset_confirmed`]) and the "export a backup first" offer are
/// the caller's job. Cancelling stops between entries and leaves a partially reset partition.
pub fn factory_reset(
    overlay_root: &Path,
    application_root: Option<&Path>,
    keep_rescue_settings: bool,
    progress: &mut ProgressFn<'_>,
    cancel: &CancelToken,
) -> Result<ResetReport> {
    let mut rep = Reporter::new(progress, cancel);
    let mut report = ResetReport::default();
    let mut errs = Vec::new();
    rep.phase(Phase::Clearing, 0);

    rep.entry("overlay");
    let keep: &[&str] = if keep_rescue_settings {
        &[LOST_FOUND, RESCUE_DIR]
    } else {
        &[LOST_FOUND]
    };
    report.overlay_removed = fsops::clear_dir(overlay_root, keep, &mut errs, cancel)
        .ctx(|| format!("clearing {}", overlay_root.display()))?;
    rep.check()?;
    for d in [OVERLAY_UPPER_DIR, OVERLAY_WORK_DIR] {
        if let Err(e) = recreate_empty_dir(overlay_root, d) {
            errs.push((overlay_root.join(d).display().to_string(), e.to_string()));
        }
    }

    if let Some(app) = application_root {
        rep.entry("application");
        report.application_removed = fsops::clear_dir(app, &[LOST_FOUND], &mut errs, cancel)
            .ctx(|| format!("clearing {}", app.display()))?;
        rep.check()?;
    }

    rep.phase(Phase::Syncing, 0);
    fsops::syncfs(overlay_root).ctx(|| format!("sync {}", overlay_root.display()))?;
    if let Some(app) = application_root {
        fsops::syncfs(app).ctx(|| format!("sync {}", app.display()))?;
    }
    report.errors = errs
        .into_iter()
        .map(|(path, error)| FileError { path, error })
        .collect();
    rep.flush();
    Ok(report)
}
