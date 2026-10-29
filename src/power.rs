//! Reboot and power off (PWR-1, PWR-2). In simulation mode nothing is executed, only logged.

use std::fmt;
use std::path::Path;

use crate::backend::{BackendError, BootTarget, SystemBackend};
use crate::cmd::{run_logged, ToolError};
use crate::tasks::TaskRunner;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    Reboot,
    PowerOff,
}

impl PowerAction {
    fn systemctl_verb(self) -> &'static str {
        match self {
            PowerAction::Reboot => "reboot",
            PowerAction::PowerOff => "poweroff",
        }
    }
}

/// What to do: optionally set the one-shot boot target (and reset its counter) first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerPlan {
    pub action: PowerAction,
    pub next_boot: Option<BootTarget>,
    pub reset_counter: bool,
}

impl PowerPlan {
    pub fn reboot() -> Self {
        PowerPlan {
            action: PowerAction::Reboot,
            next_boot: None,
            reset_counter: false,
        }
    }

    pub fn power_off() -> Self {
        PowerPlan {
            action: PowerAction::PowerOff,
            next_boot: None,
            reset_counter: false,
        }
    }

    pub fn reboot_into(target: BootTarget, reset_counter: bool) -> Self {
        PowerPlan {
            action: PowerAction::Reboot,
            next_boot: Some(target),
            reset_counter,
        }
    }

    pub fn describe(&self) -> String {
        let base = match self.action {
            PowerAction::Reboot => "reboot",
            PowerAction::PowerOff => "power off",
        };
        match (self.next_boot, self.reset_counter) {
            (Some(t), true) => format!("{base} into {t} (counter reset)"),
            (Some(t), false) => format!("{base} into {t}"),
            (None, _) => base.to_string(),
        }
    }
}

#[derive(Debug)]
pub enum PowerError {
    /// A write operation is running (PWR-2); the user must cancel it first.
    WriteRunning(String),
    Backend(BackendError),
    Tool(ToolError),
}

impl fmt::Display for PowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PowerError::WriteRunning(n) => write!(
                f,
                "A write operation is running ({n}). Cancel it first (Tools -> Tasks)."
            ),
            PowerError::Backend(e) => write!(f, "{e}"),
            PowerError::Tool(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PowerError {}

#[derive(Debug, PartialEq, Eq)]
pub enum PowerOutcome {
    /// Simulation: the text says what would have happened.
    Simulated(String),
    /// The system is going down.
    Started,
}

/// Decode `\040`-style octal escapes of /proc/self/mountinfo.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = oct
                    .iter()
                    .fold(0u32, |acc, c| acc * 8 + u32::from(c - b'0'));
                if let Ok(v) = u8::try_from(v) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mount points at or below `dir` from mountinfo text, deepest first.
pub fn mounts_under(mountinfo: &str, dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = mountinfo
        .lines()
        .filter_map(|l| l.split_whitespace().nth(4))
        .map(unescape_mount)
        .filter(|mp| Path::new(mp).starts_with(dir))
        .collect();
    v.sort_by_key(|m| std::cmp::Reverse(m.len()));
    v.dedup();
    v
}

/// Unmount everything under the app's mount directory (PWR-2). Errors are logged and
/// returned as a list; they do not stop the reboot.
pub fn unmount_all(mnt_dir: &Path, simulate: bool) -> Vec<String> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut errors = Vec::new();
    for mp in mounts_under(&info, mnt_dir) {
        if simulate {
            log::info!("SIM: would unmount {mp}");
            continue;
        }
        if let Err(e) = run_logged("umount", &[&mp]) {
            errors.push(format!("{mp}: {e}"));
        }
    }
    errors
}

/// Flush file system buffers.
pub fn sync() {
    // SAFETY: sync(2) has no arguments and cannot fail.
    unsafe { libc::sync() };
}

/// Execute a power plan (PWR-1/PWR-2).
pub fn execute(
    plan: PowerPlan,
    backend: &dyn SystemBackend,
    tasks: &TaskRunner,
) -> Result<PowerOutcome, PowerError> {
    log::info!("power: {} requested", plan.describe());
    if let Some(t) = tasks.write_running() {
        log::warn!("power: refused, write task running: {}", t.name());
        return Err(PowerError::WriteRunning(t.name().to_string()));
    }
    if let Some(target) = plan.next_boot {
        if let (BootTarget::Slot(slot), true) = (target, plan.reset_counter) {
            backend.reset_tries(slot).map_err(PowerError::Backend)?;
            log::info!("power: reset {slot}_TRY=0");
        }
        backend
            .set_next_boot(Some(target))
            .map_err(PowerError::Backend)?;
        log::info!("power: NEXT_BOOT={}", target.menu_id());
    }
    let simulate = backend.is_simulation();
    let mnt = backend.run_dir().join(crate::constants::MNT_SUBDIR);
    for e in unmount_all(&mnt, simulate) {
        log::error!("power: unmount failed: {e}");
    }
    sync();
    tasks.cancel_all();
    if simulate {
        let msg = format!(
            "SIM: would run 'systemctl {}'",
            plan.action.systemctl_verb()
        );
        log::info!("power: {msg}");
        return Ok(PowerOutcome::Simulated(msg));
    }
    run_logged("systemctl", &[plan.action.systemctl_verb()]).map_err(PowerError::Tool)?;
    Ok(PowerOutcome::Started)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::backend::Slot;
    use crate::hw::testutil::TempDir;
    use crate::tasks::{TaskKind, TaskState};
    use std::sync::mpsc;

    #[test]
    fn mountinfo_parsing() {
        let mi = "22 1 8:4 / / ro - squashfs /dev/sda4 ro\n\
30 22 8:17 / /run/xoros-rescue/mnt/usb/sdb1 rw - vfat /dev/sdb1 rw\n\
31 30 0:5 / /run/xoros-rescue/mnt/usb/sdb1/x\\040y rw - tmpfs t rw\n\
32 22 0:6 / /run/other rw - tmpfs t rw\n";
        let v = mounts_under(mi, Path::new("/run/xoros-rescue/mnt"));
        assert_eq!(
            v,
            vec![
                "/run/xoros-rescue/mnt/usb/sdb1/x y".to_string(),
                "/run/xoros-rescue/mnt/usb/sdb1".to_string()
            ]
        );
        assert_eq!(unescape_mount("a\\134b"), "a\\b");
        assert_eq!(unescape_mount("trail\\"), "trail\\");
    }

    #[test]
    fn sim_reboot_into_slot() {
        let d = TempDir::new("power");
        SimBackend::create_example(d.path()).expect("example");
        let b = SimBackend::open(d.path()).expect("open");
        let tasks = TaskRunner::new();
        let r = execute(
            PowerPlan::reboot_into(BootTarget::Slot(Slot::B), true),
            &b,
            &tasks,
        )
        .expect("execute");
        assert!(matches!(r, PowerOutcome::Simulated(_)));
        let st = b.boot_state().expect("state");
        assert_eq!(st.next_boot, Some(BootTarget::Slot(Slot::B)));
        assert_eq!(st.try_count(Slot::B), 0);
        assert_eq!(st.try_count(Slot::A), 3);
    }

    #[test]
    fn refuses_while_writing() {
        let d = TempDir::new("power-busy");
        SimBackend::create_example(d.path()).expect("example");
        let b = SimBackend::open(d.path()).expect("open");
        let tasks = TaskRunner::new();
        let (tx, rx) = mpsc::channel::<()>();
        let h = tasks
            .spawn(
                "fake write",
                TaskKind::Write,
                move |_| {
                    let _ = rx.recv();
                    Ok(String::new())
                },
                None,
            )
            .expect("spawn");
        let r = execute(PowerPlan::power_off(), &b, &tasks);
        assert!(matches!(r, Err(PowerError::WriteRunning(_))));
        let _ = tx.send(());
        assert!(matches!(h.wait().state, TaskState::Succeeded(_)));
        assert!(execute(PowerPlan::power_off(), &b, &tasks).is_ok());
    }
}
