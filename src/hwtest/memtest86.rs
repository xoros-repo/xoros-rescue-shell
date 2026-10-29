//! TST-2: "Reboot into memtest86+". Offered when the GRUB configuration on the boot partition
//! has a menu entry with the id `MENU_MEMTEST` (DIST-6, x86 only). The reboot sets the one-shot
//! `NEXT_BOOT=MENU_MEMTEST`; grub.cfg boots any menu id in `NEXT_BOOT` and clears it, so the
//! boot after memtest86+ is a normal one. `MENU_MEMTEST` is not a `BootTarget` of xoros-core,
//! so it is written as a raw grubenv value.

use crate::backend::SystemBackend;

/// GRUB menu entry id of memtest86+ (DIST-6).
pub const MENU_MEMTEST: &str = "MENU_MEMTEST";

/// GRUB configuration on the boot partition.
pub const GRUB_CFG_REL: &str = "EFI/BOOT/grub.cfg";

/// `true` if `grub_cfg` has a menu entry `--id=MENU_MEMTEST` or `--id MENU_MEMTEST` (quotes
/// allowed); comment lines are ignored.
pub fn has_memtest_entry(grub_cfg: &str) -> bool {
    grub_cfg.lines().any(|line| {
        let l = line.trim_start();
        if l.starts_with('#') || !l.starts_with("menuentry") {
            return false;
        }
        let mut rest = l;
        while let Some(i) = rest.find("--id") {
            let after = &rest[i + 4..];
            let value = after
                .strip_prefix('=')
                .or_else(|| after.strip_prefix(char::is_whitespace))
                .map(|v| v.trim_start().trim_start_matches(['"', '\'']));
            if let Some(v) = value {
                let end = v
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(v.len());
                if &v[..end] == MENU_MEMTEST {
                    return true;
                }
            }
            rest = after;
        }
        false
    })
}

/// Whether memtest86+ can be booted: `Ok(false)` without the entry or without grub.cfg.
pub fn available(backend: &dyn SystemBackend) -> Result<bool, String> {
    Ok(backend
        .read_boot_file(GRUB_CFG_REL)
        .map_err(|e| e.to_string())?
        .is_some_and(|t| has_memtest_entry(&t)))
}

/// Set `NEXT_BOOT=MENU_MEMTEST` after checking that the entry exists. The caller reboots.
pub fn set_next_boot(backend: &dyn SystemBackend) -> Result<(), String> {
    if !available(backend)? {
        return Err(crate::texts::MEMTEST86_MISSING.into());
    }
    let var = xoros_core::consts::VAR_NEXT_BOOT;
    backend
        .boot_partition()
        .map_err(|e| e.to_string())?
        .update(|st| st.env_mut().set(var, MENU_MEMTEST))
        .map_err(|e| e.to_string())?;
    log::info!("TST-2: grubenv {var}={MENU_MEMTEST} (reboot into memtest86+)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::hw::testutil::TempDir;

    #[test]
    fn menu_entry_detection() {
        assert!(has_memtest_entry(
            "menuentry \"Memory test (memtest86+)\" --id=MENU_MEMTEST {\n  chainloader /x\n}\n"
        ));
        assert!(has_memtest_entry(
            "  menuentry 'memtest86+' --class memtest --id MENU_MEMTEST {"
        ));
        assert!(has_memtest_entry("menuentry m --id=\"MENU_MEMTEST\" {"));
        assert!(!has_memtest_entry("# menuentry m --id=MENU_MEMTEST {"));
        assert!(!has_memtest_entry("menuentry m --id=MENU_MEMTEST2 {"));
        assert!(!has_memtest_entry("menuentry Rescue --id=MENU_RESCUE {"));
        assert!(!has_memtest_entry("set default=MENU_MEMTEST"));
    }

    #[test]
    fn next_boot_in_simulation() {
        let d = TempDir::new("memtest86");
        SimBackend::create_example(d.path()).expect("example");
        let b = SimBackend::open(d.path()).expect("open");
        assert_eq!(available(&b), Ok(false));
        assert!(set_next_boot(&b).is_err());
        d.write(
            &format!("boot/{GRUB_CFG_REL}"),
            b"menuentry \"memtest86+\" --id=MENU_MEMTEST {\n}\n",
        );
        assert_eq!(available(&b), Ok(true));
        set_next_boot(&b).expect("set");
        let st = b.boot_state().expect("state");
        assert!(st
            .raw
            .iter()
            .any(|(k, v)| k == "NEXT_BOOT" && v == MENU_MEMTEST));
    }
}
