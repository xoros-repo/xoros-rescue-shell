//! Progress reporting and cancellation of the backup module (the cancel token is the crate's).

use std::time::{Duration, Instant};

use super::error::{BackupError, Result};

/// Cooperative cancellation flag: the crate's token (shared with the task runner).
pub use xoros_core::CancelToken;

/// `Err(BackupError::Cancelled)` once `cancel` is set.
pub(crate) fn check_cancel(cancel: &CancelToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(BackupError::Cancelled)
    } else {
        Ok(())
    }
}

/// What a long running operation is doing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Walking the source partitions to count files and bytes (export).
    Scanning,
    /// Writing the archive (export).
    Exporting,
    /// Hashing archive parts against the `.sha256` file.
    Verifying,
    /// Deleting the old content of a partition (import in Replace mode, factory reset).
    Clearing,
    /// Extracting the archive (import).
    Importing,
    /// `sync` at the end.
    Syncing,
}

/// One progress update. `total_bytes` is 0 when unknown.
///
/// Byte counts mean: Exporting = uncompressed file data read, Verifying = archive bytes hashed,
/// Importing = compressed archive bytes read. Rate and ETA are left to the UI.
#[derive(Clone, Debug)]
pub struct Progress {
    pub phase: Phase,
    pub done_bytes: u64,
    pub total_bytes: u64,
    /// Entries (files, dirs, ...) processed in this phase.
    pub entries: u64,
    /// Path being processed (archive path or file name), for display only.
    pub current: String,
}

/// Progress callback type. Called from the worker thread, throttled to about 10 calls/s.
pub type ProgressFn<'a> = dyn FnMut(&Progress) + 'a;

/// Internal helper: throttles callbacks and checks the cancel token.
pub(crate) struct Reporter<'a, 'b> {
    cb: &'a mut ProgressFn<'b>,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) state: Progress,
    last: Option<Instant>,
}

const MIN_INTERVAL: Duration = Duration::from_millis(100);

impl<'a, 'b> Reporter<'a, 'b> {
    pub(crate) fn new(cb: &'a mut ProgressFn<'b>, cancel: &'a CancelToken) -> Self {
        Reporter {
            cb,
            cancel,
            state: Progress {
                phase: Phase::Scanning,
                done_bytes: 0,
                total_bytes: 0,
                entries: 0,
                current: String::new(),
            },
            last: None,
        }
    }

    /// Starts a new phase and always reports it.
    pub(crate) fn phase(&mut self, phase: Phase, total_bytes: u64) {
        self.state.phase = phase;
        self.state.total_bytes = total_bytes;
        self.state.done_bytes = 0;
        self.state.entries = 0;
        self.state.current.clear();
        self.emit(true);
    }

    pub(crate) fn add_bytes(&mut self, n: u64) {
        self.state.done_bytes += n;
        self.emit(false);
    }

    pub(crate) fn set_bytes(&mut self, n: u64) {
        self.state.done_bytes = n;
        self.emit(false);
    }

    pub(crate) fn entry(&mut self, current: &str) {
        self.state.entries += 1;
        self.state.current.clear();
        self.state.current.push_str(current);
        self.emit(false);
    }

    /// Reports the current state unconditionally (end of a phase).
    pub(crate) fn flush(&mut self) {
        self.emit(true);
    }

    pub(crate) fn check(&self) -> Result<()> {
        check_cancel(self.cancel)
    }

    fn emit(&mut self, force: bool) {
        let now = Instant::now();
        if force
            || self
                .last
                .is_none_or(|t| now.duration_since(t) >= MIN_INTERVAL)
        {
            self.last = Some(now);
            (self.cb)(&self.state);
        }
    }
}
