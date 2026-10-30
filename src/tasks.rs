//! Background job runner with progress and cancel (UI-3, CORE-4).
//!
//! Long operations (hashing, downloads, writes, tests) run in worker threads. The UI polls
//! [`TaskHandle::snapshot`] to draw a progress bar and calls [`TaskHandle::cancel`]. Only one
//! [`TaskKind::Write`] task may run at a time (UI-3); power actions check
//! [`TaskRunner::write_running`] (PWR-2). No ratatui dependency (ARCH-1).

use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::backend::{BackendError, ErrorKind};

/// Lock a mutex, recovering the data if another thread panicked while holding it.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Shared cancel flag: the crate's token (XC-13), so the task runner, the backup module and
/// the flash engine share one type.
pub use xoros_core::CancelToken;

/// Whether a task writes to the boot disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    ReadOnly,
    Write,
}

/// Life cycle of a task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskState {
    Running,
    /// Finished; the text is a short result summary.
    Succeeded(String),
    Failed(String),
    Cancelled,
}

/// Why a task did not succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskFailure {
    Cancelled,
    Error(String),
}

impl From<BackendError> for TaskFailure {
    fn from(e: BackendError) -> Self {
        if e.kind == ErrorKind::Cancelled {
            TaskFailure::Cancelled
        } else {
            TaskFailure::Error(e.to_string())
        }
    }
}

impl From<std::io::Error> for TaskFailure {
    fn from(e: std::io::Error) -> Self {
        TaskFailure::Error(e.to_string())
    }
}

pub type TaskResult = Result<String, TaskFailure>;

/// Error starting a task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// Another write task is running (UI-3).
    WriteBusy(String),
    Thread(String),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpawnError::WriteBusy(name) => write!(
                f,
                "Another write operation is running ({name}). Wait for it or cancel it first."
            ),
            SpawnError::Thread(e) => write!(f, "Cannot start a worker thread: {e}"),
        }
    }
}

impl std::error::Error for SpawnError {}

#[derive(Debug)]
struct Shared {
    state: TaskState,
    done: u64,
    total: u64,
    message: String,
    finished: Option<Instant>,
}

/// Point-in-time view of a task, for drawing.
#[derive(Clone, Debug)]
pub struct TaskSnapshot {
    pub id: u64,
    pub name: String,
    pub kind: TaskKind,
    pub state: TaskState,
    pub done: u64,
    pub total: u64,
    pub message: String,
    pub elapsed: Duration,
}

impl TaskSnapshot {
    pub fn is_finished(&self) -> bool {
        self.state != TaskState::Running
    }

    /// 0.0..=1.0 when the total is known.
    pub fn fraction(&self) -> Option<f64> {
        if self.total == 0 {
            None
        } else {
            Some((self.done as f64 / self.total as f64).min(1.0))
        }
    }

    /// Bytes (or units) per second.
    pub fn speed(&self) -> Option<f64> {
        let secs = self.elapsed.as_secs_f64();
        if secs < 0.2 || self.done == 0 {
            None
        } else {
            Some(self.done as f64 / secs)
        }
    }

    pub fn eta(&self) -> Option<Duration> {
        let speed = self.speed()?;
        if self.total <= self.done || speed <= 0.0 {
            return None;
        }
        Some(Duration::from_secs_f64(
            (self.total - self.done) as f64 / speed,
        ))
    }
}

/// Handle to a running or finished task.
#[derive(Clone)]
pub struct TaskHandle {
    id: u64,
    name: Arc<str>,
    kind: TaskKind,
    started: Instant,
    shared: Arc<Mutex<Shared>>,
    cancel: CancelToken,
}

impl TaskHandle {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn is_finished(&self) -> bool {
        lock(&self.shared).state != TaskState::Running
    }

    pub fn snapshot(&self) -> TaskSnapshot {
        let s = lock(&self.shared);
        let end = s.finished.unwrap_or_else(Instant::now);
        TaskSnapshot {
            id: self.id,
            name: self.name.to_string(),
            kind: self.kind,
            state: s.state.clone(),
            done: s.done,
            total: s.total,
            message: s.message.clone(),
            elapsed: end.saturating_duration_since(self.started),
        }
    }

    /// Block until the task has finished (used by the CLI and tests).
    pub fn wait(&self) -> TaskSnapshot {
        loop {
            if self.is_finished() {
                return self.snapshot();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Passed to the task body to report progress and check for cancellation.
pub struct TaskCtx {
    shared: Arc<Mutex<Shared>>,
    cancel: CancelToken,
}

impl TaskCtx {
    pub fn set_total(&self, total: u64) {
        lock(&self.shared).total = total;
    }

    pub fn set_progress(&self, done: u64, total: u64) {
        let mut s = lock(&self.shared);
        s.done = done;
        s.total = total;
    }

    pub fn add_done(&self, n: u64) {
        let mut s = lock(&self.shared);
        s.done = s.done.saturating_add(n);
    }

    pub fn set_message(&self, msg: impl Into<String>) {
        lock(&self.shared).message = msg.into();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    /// `Err(TaskFailure::Cancelled)` if cancel was requested.
    pub fn check_cancelled(&self) -> Result<(), TaskFailure> {
        if self.is_cancelled() {
            Err(TaskFailure::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Callback run on the worker thread after the task finished.
pub type OnFinish = Box<dyn FnOnce(TaskSnapshot) + Send + 'static>;

#[derive(Default)]
struct RunnerInner {
    next_id: u64,
    tasks: Vec<TaskHandle>,
}

/// Starts tasks and keeps the list of tasks of this session.
#[derive(Default)]
pub struct TaskRunner {
    inner: Mutex<RunnerInner>,
    /// Number of tasks that have finished (never decreases; the UI refreshes on changes).
    finished: Arc<AtomicU64>,
}

/// Number of finished tasks kept in the list.
const KEEP_FINISHED: usize = 50;

impl TaskRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start `body` in a worker thread.
    pub fn spawn<F>(
        &self,
        name: impl Into<String>,
        kind: TaskKind,
        body: F,
        on_finish: Option<OnFinish>,
    ) -> Result<TaskHandle, SpawnError>
    where
        F: FnOnce(&TaskCtx) -> TaskResult + Send + 'static,
    {
        let name: String = name.into();
        let mut inner = lock(&self.inner);
        if kind == TaskKind::Write {
            if let Some(busy) = inner
                .tasks
                .iter()
                .find(|t| t.kind == TaskKind::Write && !t.is_finished())
            {
                return Err(SpawnError::WriteBusy(busy.name.to_string()));
            }
        }
        inner.next_id += 1;
        let shared = Arc::new(Mutex::new(Shared {
            state: TaskState::Running,
            done: 0,
            total: 0,
            message: String::new(),
            finished: None,
        }));
        let handle = TaskHandle {
            id: inner.next_id,
            name: Arc::from(name.as_str()),
            kind,
            started: Instant::now(),
            shared: Arc::clone(&shared),
            cancel: CancelToken::new(),
        };
        let ctx = TaskCtx {
            shared: Arc::clone(&shared),
            cancel: handle.cancel.clone(),
        };
        let thread_handle = handle.clone();
        let task_name = name.clone();
        let finished = Arc::clone(&self.finished);
        std::thread::Builder::new()
            .name(format!("task-{}", handle.id))
            .spawn(move || {
                log::info!("task started: {task_name}");
                let result = catch_unwind(AssertUnwindSafe(|| body(&ctx)));
                let state = match result {
                    Ok(Ok(summary)) => TaskState::Succeeded(summary),
                    Ok(Err(TaskFailure::Cancelled)) => TaskState::Cancelled,
                    Ok(Err(TaskFailure::Error(e))) => TaskState::Failed(e),
                    Err(_) => TaskState::Failed("Internal error (panic) in the task.".into()),
                };
                match &state {
                    TaskState::Succeeded(s) => log::info!("task finished: {task_name}: {s}"),
                    TaskState::Failed(e) => log::error!("task failed: {task_name}: {e}"),
                    TaskState::Cancelled => log::warn!("task cancelled: {task_name}"),
                    TaskState::Running => {}
                }
                {
                    let mut s = lock(&ctx.shared);
                    s.state = state;
                    s.finished = Some(Instant::now());
                }
                finished.fetch_add(1, Ordering::SeqCst);
                if let Some(cb) = on_finish {
                    cb(thread_handle.snapshot());
                }
            })
            .map_err(|e| SpawnError::Thread(e.to_string()))?;

        inner.tasks.push(handle.clone());
        // Drop the oldest finished tasks.
        let finished = inner.tasks.iter().filter(|t| t.is_finished()).count();
        if finished > KEEP_FINISHED {
            let mut to_drop = finished - KEEP_FINISHED;
            inner.tasks.retain(|t| {
                if to_drop > 0 && t.is_finished() {
                    to_drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
        Ok(handle)
    }

    /// The running write task, if any (PWR-2).
    pub fn write_running(&self) -> Option<TaskHandle> {
        lock(&self.inner)
            .tasks
            .iter()
            .find(|t| t.kind == TaskKind::Write && !t.is_finished())
            .cloned()
    }

    /// Running tasks.
    pub fn running(&self) -> Vec<TaskHandle> {
        lock(&self.inner)
            .tasks
            .iter()
            .filter(|t| !t.is_finished())
            .cloned()
            .collect()
    }

    /// All tasks of this session (newest last).
    pub fn all(&self) -> Vec<TaskSnapshot> {
        lock(&self.inner)
            .tasks
            .iter()
            .map(|t| t.snapshot())
            .collect()
    }

    /// Number of tasks finished so far in this session.
    pub fn finished_count(&self) -> u64 {
        self.finished.load(Ordering::SeqCst)
    }

    /// Request cancel of every running task.
    pub fn cancel_all(&self) {
        for t in lock(&self.inner).tasks.iter() {
            t.cancel();
        }
    }
}

/// Format a byte count with binary units ("1.5 GiB").
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Format a duration as `h:mm:ss` or `m:ss`.
pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn task_succeeds_with_progress() {
        let runner = TaskRunner::new();
        let h = runner
            .spawn(
                "count",
                TaskKind::ReadOnly,
                |ctx| {
                    ctx.set_total(10);
                    for _ in 0..10 {
                        ctx.add_done(1);
                    }
                    ctx.set_message("done");
                    Ok("ten".into())
                },
                None,
            )
            .expect("spawn");
        let snap = h.wait();
        assert_eq!(snap.state, TaskState::Succeeded("ten".into()));
        assert_eq!(snap.done, 10);
        assert_eq!(snap.fraction(), Some(1.0));
        assert_eq!(snap.message, "done");
    }

    #[test]
    fn task_cancel_and_failure_and_panic() {
        let runner = TaskRunner::new();
        let (tx, rx) = mpsc::channel::<()>();
        let h = runner
            .spawn(
                "loop",
                TaskKind::ReadOnly,
                move |ctx| {
                    let _ = tx.send(());
                    loop {
                        ctx.check_cancelled()?;
                        std::thread::sleep(Duration::from_millis(5));
                    }
                },
                None,
            )
            .expect("spawn");
        rx.recv().expect("started");
        h.cancel();
        assert_eq!(h.wait().state, TaskState::Cancelled);

        let h = runner
            .spawn(
                "fail",
                TaskKind::ReadOnly,
                |_| Err(TaskFailure::Error("boom".into())),
                None,
            )
            .expect("spawn");
        assert_eq!(h.wait().state, TaskState::Failed("boom".into()));

        let h = runner
            .spawn("panic", TaskKind::ReadOnly, |_| panic!("test panic"), None)
            .expect("spawn");
        assert!(matches!(h.wait().state, TaskState::Failed(_)));
    }

    #[test]
    fn only_one_write_task() {
        let runner = TaskRunner::new();
        let (tx, rx) = mpsc::channel::<()>();
        let w = runner
            .spawn(
                "write1",
                TaskKind::Write,
                move |ctx| {
                    while rx.recv_timeout(Duration::from_millis(5)).is_err() {
                        ctx.check_cancelled()?;
                    }
                    Ok(String::new())
                },
                None,
            )
            .expect("spawn");
        assert!(runner.write_running().is_some());
        let second = runner.spawn("write2", TaskKind::Write, |_| Ok(String::new()), None);
        assert_eq!(second.err(), Some(SpawnError::WriteBusy("write1".into())));
        // Read-only tasks still run.
        let r = runner
            .spawn("read", TaskKind::ReadOnly, |_| Ok(String::new()), None)
            .expect("spawn read");
        r.wait();
        tx.send(()).expect("send");
        w.wait();
        assert!(runner.write_running().is_none());
        assert_eq!(runner.all().len(), 2);
    }

    #[test]
    fn on_finish_called() {
        let runner = TaskRunner::new();
        let (tx, rx) = mpsc::channel();
        runner
            .spawn(
                "cb",
                TaskKind::ReadOnly,
                |_| Ok("x".into()),
                Some(Box::new(move |snap| {
                    let _ = tx.send(snap.state);
                })),
            )
            .expect("spawn");
        let state = rx.recv_timeout(Duration::from_secs(5)).expect("callback");
        assert_eq!(state, TaskState::Succeeded("x".into()));
        assert_eq!(runner.finished_count(), 1);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1 << 30), "1.0 GiB");
        assert_eq!(format_duration(Duration::from_secs(65)), "1:05");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1:02:05");
    }
}
