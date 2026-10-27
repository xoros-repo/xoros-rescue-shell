//! System status for the Status screen and `--cli status` (SYS-1).

use serde::Serialize;

use crate::backend::{
    BackendError, BootState, BootTarget, ErrorKind, FsUsage, PartRole, RescueReason,
    RescueSystemInfo, SystemBackend,
};
use crate::clock::{clock_info, ClockInfo};
use crate::constants::{APP_VERSION, PERSISTENT_PARTITIONS};
use crate::net::info::{network_summary, NetworkSummary};
use crate::slots::{slot_statuses, RescueCheck, SlotStatus};

/// Why the device is in rescue, as shown to the user (SYS-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    /// NEXT_BOOT or boot menu choice (both counters below MAX_TRIES).
    Requested,
    /// Both slots exceeded MAX_TRIES.
    SlotsExhausted,
    /// At least one slot is empty/invalid.
    SlotsInvalid,
    /// Not booted from rescue (development) or grubenv unreadable.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reason {
    pub code: ReasonCode,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RescueStatus {
    pub system: Option<RescueSystemInfo>,
    pub app_version: String,
    pub image_check: RescueCheck,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PersistentStatus {
    pub label: String,
    pub device: Option<String>,
    pub present: bool,
    pub usage: Option<FsUsage>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SystemStatus {
    pub app_version: String,
    pub backend: String,
    pub simulation: bool,
    pub reason: Reason,
    pub boot: Option<BootState>,
    pub boot_error: Option<String>,
    /// True when grubenv is missing/broken and "Recreate grubenv" should be offered (ENV-2).
    pub grubenv_broken: bool,
    pub current_boot: Option<BootTarget>,
    pub predicted_next_boot: Option<BootTarget>,
    pub slots: Vec<SlotStatus>,
    pub rescue: RescueStatus,
    pub persistent: Vec<PersistentStatus>,
    pub layout_error: Option<String>,
    pub network: NetworkSummary,
    pub clock: ClockInfo,
}

/// Decide the reason text from boot state and slot validity.
pub fn reason(
    boot: Result<&BootState, &BackendError>,
    backend_reason: Option<RescueReason>,
    slots: &[SlotStatus],
    current_boot: Option<BootTarget>,
) -> Reason {
    let boot = match boot {
        Ok(b) => b,
        Err(e) => {
            return Reason {
                code: ReasonCode::Unknown,
                text: format!("Unknown (boot state unreadable: {e})"),
            }
        }
    };
    if let Some(cur) = current_boot {
        if cur != BootTarget::Rescue {
            return Reason {
                code: ReasonCode::Unknown,
                text: format!("Not running from rescue (booted {cur})."),
            };
        }
    }
    let invalid: Vec<&str> = slots
        .iter()
        .filter(|s| s.valid == Some(false))
        .map(|s| s.slot.letter())
        .collect();
    let counters = format!(
        "A: {}/{}, B: {}/{}",
        boot.tries[0], boot.max_tries, boot.tries[1], boot.max_tries
    );
    match backend_reason {
        Some(RescueReason::SlotsExhausted) => Reason {
            code: ReasonCode::SlotsExhausted,
            text: format!("Both slots exceeded MAX_TRIES ({counters})."),
        },
        _ if !invalid.is_empty() => Reason {
            code: ReasonCode::SlotsInvalid,
            text: format!(
                "Slot(s) invalid or empty: {} ({counters}).",
                invalid.join(", ")
            ),
        },
        Some(RescueReason::Requested) => Reason {
            code: ReasonCode::Requested,
            text: format!("Requested via NEXT_BOOT or the boot menu ({counters})."),
        },
        _ => Reason {
            code: ReasonCode::Unknown,
            text: format!("Unknown ({counters})."),
        },
    }
}

/// Collect everything for the Status screen. Calls the backend several times; cheap in
/// simulation, a few mounts on a real device (run off the UI thread when slow).
pub fn collect_status(backend: &dyn SystemBackend, rescue_check: RescueCheck) -> SystemStatus {
    let boot = backend.boot_state();
    let boot_ok = boot.as_ref().ok();
    let slots = slot_statuses(backend, boot_ok, &rescue_check);
    let current_boot = backend.current_boot().ok();
    let reason = reason(
        boot.as_ref(),
        backend.rescue_reason().ok(),
        &slots,
        current_boot,
    );
    let layout = backend.layout();
    let persistent = PERSISTENT_PARTITIONS
        .iter()
        .map(|label| {
            let role = PartRole::from_label(label);
            let device = layout
                .as_ref()
                .ok()
                .and_then(|l| role.and_then(|r| l.get(r)))
                .map(|e| e.device.clone());
            let (usage, error) = match role.map(|r| backend.partition_usage(r)) {
                Some(Ok(u)) => (u, None),
                Some(Err(e)) => (None, Some(e.to_string())),
                None => (None, None),
            };
            PersistentStatus {
                label: label.to_string(),
                present: device.is_some(),
                device,
                usage,
                error,
            }
        })
        .collect();
    let host = backend.host_paths();
    SystemStatus {
        app_version: APP_VERSION.to_string(),
        backend: backend.describe(),
        simulation: backend.is_simulation(),
        reason,
        grubenv_broken: matches!(&boot, Err(e) if e.kind == ErrorKind::GrubenvBroken),
        boot_error: boot.as_ref().err().map(|e| e.to_string()),
        predicted_next_boot: backend.predict().ok(),
        boot: boot.ok(),
        current_boot,
        slots,
        rescue: RescueStatus {
            system: backend.rescue_system().ok(),
            app_version: APP_VERSION.to_string(),
            image_check: rescue_check,
        },
        persistent,
        layout_error: layout.err().map(|e| e.to_string()),
        network: network_summary(&host),
        clock: clock_info(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::backend::Slot;
    use crate::hw::testutil::TempDir;

    #[test]
    fn status_from_example_sim() {
        let d = TempDir::new("status");
        SimBackend::create_example(d.path()).expect("example");
        let b = SimBackend::open(d.path()).expect("open");
        let st = collect_status(&b, RescueCheck::NotChecked);
        assert_eq!(st.reason.code, ReasonCode::SlotsExhausted);
        assert!(st.reason.text.contains("A: 3/3, B: 3/3"));
        assert_eq!(st.predicted_next_boot, Some(BootTarget::Rescue));
        assert_eq!(st.persistent.len(), 2);
        assert_eq!(st.persistent[0].device.as_deref(), Some("/dev/sda5"));
        assert!(st.persistent[1].usage.is_some());
        assert!(!st.grubenv_broken);

        b.reset_tries(Slot::A).expect("reset");
        let st = collect_status(&b, RescueCheck::NotChecked);
        assert_eq!(st.reason.code, ReasonCode::Requested);

        let mut s = b.load().expect("load");
        s.slots.rootfs_b = None;
        d.write(
            "sim.json",
            serde_json::to_string(&s).expect("json").as_bytes(),
        );
        let st = collect_status(&b, RescueCheck::NotChecked);
        assert_eq!(st.reason.code, ReasonCode::SlotsInvalid);

        std::fs::remove_file(b.grubenv_path()).expect("rm grubenv");
        let st = collect_status(&b, RescueCheck::NotChecked);
        assert_eq!(st.reason.code, ReasonCode::Unknown);
        assert!(st.grubenv_broken);
    }
}
