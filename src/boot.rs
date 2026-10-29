//! Boot control actions with logging (BOOT-*, ROT-1, LOG-3). Policy and warnings live here;
//! grubenv access is the backend's.

use crate::backend::{BootTarget, Result, Slot, SystemBackend};
use crate::constants::{FBCON_ROTATE, FBCON_ROTATE_ALL};

/// Run a state-changing action and log it with its result (LOG-3).
pub fn logged<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    log::info!("action: {what}");
    let r = f();
    match &r {
        Ok(_) => log::info!("action: {what}: ok"),
        Err(e) => log::error!("action: {what}: failed: {e}"),
    }
    r
}

/// Reset try counters (BOOT-2).
pub fn reset_tries(b: &dyn SystemBackend, slots: &[Slot]) -> Result<()> {
    for &s in slots {
        logged(&format!("reset {s}_TRY=0"), || b.reset_tries(s))?;
    }
    Ok(())
}

/// Set or clear the one-shot boot target (BOOT-3).
pub fn set_next_boot(b: &dyn SystemBackend, target: Option<BootTarget>) -> Result<()> {
    let what = match target {
        Some(t) => format!("set NEXT_BOOT={}", t.menu_id()),
        None => "clear NEXT_BOOT".to_string(),
    };
    logged(&what, || b.set_next_boot(target))
}

pub fn set_order(b: &dyn SystemBackend, first: Slot) -> Result<()> {
    logged(&format!("set ORDER={} {}", first, first.other()), || {
        b.set_order(first)
    })
}

pub fn set_max_tries(b: &dyn SystemBackend, n: u8) -> Result<()> {
    logged(&format!("set MAX_TRIES={n}"), || b.set_max_tries(n))
}

pub fn disable(b: &dyn SystemBackend, slot: Slot) -> Result<()> {
    logged(&format!("disable {slot} (TRY=MAX_TRIES)"), || {
        b.disable(slot)
    })
}

pub fn recreate_grubenv(b: &dyn SystemBackend) -> Result<()> {
    logged("recreate grubenv", || b.recreate_grubenv())
}

/// Set ROTATION in grubenv and apply it to the running console (ROT-1). Returns a note
/// for the user about the immediate application.
pub fn set_rotation(b: &dyn SystemBackend, rotation: u8) -> Result<String> {
    logged(&format!("set ROTATION={rotation}"), || {
        b.set_rotation(rotation)
    })?;
    let path = b.host_paths().sys_root.join(FBCON_ROTATE_ALL);
    if b.is_simulation() {
        log::info!("SIM: would write {rotation} to {}", path.display());
        return Ok("Simulation: console rotation not changed.".into());
    }
    match std::fs::write(&path, format!("{rotation}\n")) {
        // Without CONFIG_FRAMEBUFFER_CONSOLE_ROTATION the write succeeds but changes nothing:
        // check the foreground console's rotation.
        Ok(()) => match std::fs::read_to_string(b.host_paths().sys_root.join(FBCON_ROTATE)) {
            Ok(now) if now.trim() == rotation.to_string() => {
                log::info!("console rotation set to {rotation}");
                Ok("Applied to the console now.".into())
            }
            Ok(now) => {
                log::warn!("console rotation is still {} after writing {rotation}", now.trim());
                Ok("Applied on next boot (this kernel cannot rotate the running console).".into())
            }
            Err(e) => {
                log::warn!("cannot read back the console rotation: {e}");
                Ok("Applied on next boot.".into())
            }
        },
        Err(e) => {
            log::warn!("cannot apply console rotation: {e}");
            Ok(format!("Applied on next boot (console: {e})."))
        }
    }
}

/// Warning before booting into a slot that is empty/invalid or unknown (BOOT-6).
pub fn slot_boot_warning(b: &dyn SystemBackend, slot: Slot) -> Option<String> {
    match b.slot_info(slot) {
        Ok(Some(_)) => None,
        Ok(None) => Some(format!(
            "Slot {} is empty or invalid. Booting it will most likely fail and end in rescue again.",
            slot.letter()
        )),
        Err(e) => Some(format!(
            "The content of slot {} could not be checked: {e}",
            slot.letter()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::hw::testutil::TempDir;

    #[test]
    fn actions_and_warnings() {
        let d = TempDir::new("boot");
        SimBackend::create_example(d.path()).expect("example");
        let b = SimBackend::open(d.path()).expect("open");
        reset_tries(&b, &[Slot::A, Slot::B]).expect("reset");
        assert_eq!(b.boot_state().map(|s| s.tries).ok(), Some([0, 0]));
        assert!(slot_boot_warning(&b, Slot::A).is_none());
        let mut s = b.load().expect("load");
        s.slots.rootfs_b = None;
        d.write(
            "sim.json",
            serde_json::to_string(&s).expect("json").as_bytes(),
        );
        assert!(slot_boot_warning(&b, Slot::B).is_some());
        let note = set_rotation(&b, 1).expect("rotation");
        assert!(note.contains("Simulation"));
        set_next_boot(&b, Some(BootTarget::Slot(Slot::A))).expect("next");
        set_order(&b, Slot::B).expect("order");
        set_max_tries(&b, 4).expect("max");
        disable(&b, Slot::A).expect("disable");
        let st = b.boot_state().expect("state");
        assert_eq!(st.order, [Slot::B, Slot::A]);
        assert_eq!(st.try_count(Slot::A), 4);
        assert_eq!(st.rotation, 1);
    }
}
