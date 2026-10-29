//! Slot summary for the UI (SYS-1) and the rescue image check (SYS-1, TST-5).

use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::backend::{BootState, BuildInfo, PartRole, Slot, SystemBackend};
use crate::tasks::{lock, OnFinish, SpawnError, TaskHandle, TaskKind, TaskRunner};

/// Summary of one slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SlotStatus {
    pub slot: Slot,
    /// `Some(true)` valid, `Some(false)` empty/invalid, `None` could not be checked.
    pub valid: Option<bool>,
    pub error: Option<String>,
    pub info: Option<BuildInfo>,
    pub tries: Option<u8>,
    pub max_tries: Option<u8>,
    /// 1-based position in ORDER.
    pub order_position: Option<usize>,
    pub last_boot: bool,
    /// Whether this slot's RESCUE_IMAGE_SHA256 matches the running rescue image.
    pub rescue_match: Option<bool>,
}

impl SlotStatus {
    pub fn exhausted(&self) -> bool {
        matches!((self.tries, self.max_tries), (Some(t), Some(m)) if t >= m)
    }
}

/// State of the background SHA-256 check of the rescue partition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum RescueCheck {
    NotChecked,
    Checking,
    Done(String),
    Failed(String),
}

impl RescueCheck {
    pub fn sha256(&self) -> Option<&str> {
        match self {
            RescueCheck::Done(s) => Some(s),
            _ => None,
        }
    }
}

/// Build the slot summaries from the backend.
pub fn slot_statuses(
    backend: &dyn SystemBackend,
    boot: Option<&BootState>,
    rescue: &RescueCheck,
) -> Vec<SlotStatus> {
    Slot::ALL
        .iter()
        .map(|&slot| {
            let (valid, info, error) = match backend.slot_info(slot) {
                Ok(Some(i)) => (Some(true), Some(i), None),
                Ok(None) => (Some(false), None, None),
                Err(e) => (None, None, Some(e.to_string())),
            };
            let rescue_match = match (rescue.sha256(), info.as_ref()) {
                (Some(sha), Some(i)) => Some(
                    i.rescue_image_sha256
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(sha)),
                ),
                _ => None,
            };
            SlotStatus {
                slot,
                valid,
                error,
                info,
                tries: boot.map(|b| b.try_count(slot)),
                max_tries: boot.map(|b| b.max_tries),
                order_position: boot.map(|b| b.order_position(slot)),
                last_boot: boot
                    .and_then(|b| b.last_boot)
                    .is_some_and(|t| t == crate::backend::BootTarget::Slot(slot)),
                rescue_match,
            }
        })
        .collect()
}

/// Start the SHA-256 check of the rescue image in the background. The result is stored in
/// `state`; `on_finish` runs afterwards (e.g. to refresh the UI).
pub fn start_rescue_check(
    backend: Arc<dyn SystemBackend>,
    runner: &TaskRunner,
    state: Arc<Mutex<RescueCheck>>,
    on_finish: Option<OnFinish>,
) -> Result<TaskHandle, SpawnError> {
    *lock(&state) = RescueCheck::Checking;
    let st = Arc::clone(&state);
    let result = runner.spawn(
        crate::texts::TASK_RESCUE_HASH,
        TaskKind::ReadOnly,
        move |ctx| {
            ctx.set_message("SHA-256 of the rescue partition");
            let r = backend.hash_image(
                PartRole::Rescue,
                &mut |done, total| ctx.set_progress(done, total),
                ctx.cancel_token(),
            );
            match r {
                Ok(sha) => {
                    *lock(&st) = RescueCheck::Done(sha.clone());
                    Ok(format!("sha256 {sha}"))
                }
                Err(e) => {
                    *lock(&st) = if e.kind == crate::backend::ErrorKind::Cancelled {
                        RescueCheck::NotChecked
                    } else {
                        RescueCheck::Failed(e.to_string())
                    };
                    Err(e.into())
                }
            }
        },
        on_finish,
    );
    if let Err(e) = &result {
        *lock(&state) = RescueCheck::Failed(e.to_string());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::hw::testutil::TempDir;
    use crate::tasks::TaskState;

    #[test]
    fn rescue_check_against_sim() {
        let d = TempDir::new("slots");
        SimBackend::create_example(d.path()).expect("example");
        let sim = SimBackend::open(d.path()).expect("open");
        let mut s = sim.load().expect("load");
        s.hash_delay_ms_per_mib = 0;
        if let Some(i) = s.slots.rootfs_b.as_mut() {
            i.rescue_image_sha256 = Some("00".into());
        }
        let json = serde_json::to_string(&s).expect("json");
        d.write("sim.json", json.as_bytes());
        let backend: Arc<dyn SystemBackend> = Arc::new(sim);
        let runner = TaskRunner::new();
        let state = Arc::new(Mutex::new(RescueCheck::NotChecked));
        let h = start_rescue_check(Arc::clone(&backend), &runner, Arc::clone(&state), None)
            .expect("spawn");
        assert!(matches!(h.wait().state, TaskState::Succeeded(_)));
        let check = lock(&state).clone();
        assert!(check.sha256().is_some());
        let boot = backend.boot_state().ok();
        let slots = slot_statuses(backend.as_ref(), boot.as_ref(), &check);
        assert_eq!(slots[0].rescue_match, Some(true));
        assert_eq!(slots[1].rescue_match, Some(false));
        assert!(slots[0].exhausted());
        assert_eq!(slots[1].order_position, Some(2));
    }
}
