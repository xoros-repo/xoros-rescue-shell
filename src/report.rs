//! Plain-text rendering of status, hardware and storage information. Used by the TUI
//! screens and the CLI; later also by the system report (SYS-5, LOG-2).
//! Lines are kept within 76 columns so they fit an 80x24 dialog (RT-4).

use std::fmt::Write as _;

use crate::backend::{BlockDevice, BootState, Layout, Result as BackendResult};
use crate::clock::format_build_timestamp;
use crate::constants::GRUBENV_KNOWN_VARS;
use crate::hw::{format_uptime, HardwareInfo};
use crate::slots::{RescueCheck, SlotStatus};
use crate::status::SystemStatus;
use crate::tasks::format_bytes;

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn opt(o: &Option<String>) -> &str {
    o.as_deref().unwrap_or("-")
}

/// Shorten long text with an ellipsis so it fits `max` columns.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
        out.push_str("...");
        out
    }
}

fn slot_lines(out: &mut String, s: &SlotStatus) {
    let head = format!("Slot {}:", s.slot.letter());
    let state = match (s.valid, &s.error) {
        (Some(true), _) => "valid",
        (Some(false), _) => "EMPTY/INVALID",
        (None, _) => "unknown",
    };
    match &s.info {
        Some(i) => {
            let _ = writeln!(
                out,
                "{head} {state}  {} {}  built {}  {}",
                i.version,
                i.suffix,
                format_build_timestamp(&i.timestamp),
                ellipsize(&i.machine, 18)
            );
        }
        None => {
            let detail = s
                .error
                .as_deref()
                .map(|e| format!(" ({})", ellipsize(e, 50)))
                .unwrap_or_default();
            let _ = writeln!(out, "{head} {state}{detail}");
        }
    }
    let tries = match (s.tries, s.max_tries) {
        (Some(t), Some(m)) => {
            format!("{t}/{m}{}", if t >= m { " (exceeded)" } else { "" })
        }
        _ => "?".into(),
    };
    let order = s
        .order_position
        .map(|p| p.to_string())
        .unwrap_or_else(|| "?".into());
    let rescue = match s.rescue_match {
        Some(true) => "  rescue: match",
        Some(false) => "  rescue: differs",
        None => "",
    };
    let _ = writeln!(
        out,
        "        tries {tries}  order {order}  last boot: {}{rescue}",
        yes_no(s.last_boot)
    );
}

/// Status screen text (SYS-1).
pub fn status_text(st: &SystemStatus) -> String {
    let mut out = String::new();
    if st.simulation {
        let _ = writeln!(out, "Backend: {}", ellipsize(&st.backend, 66));
    }
    let _ = writeln!(out, "Why rescue: {}", st.reason.text);
    if st.grubenv_broken {
        let _ = writeln!(out, "  grubenv is broken: use Boot -> Recreate grubenv.");
    }
    let next = st
        .predicted_next_boot
        .map(|t| t.name().to_string())
        .unwrap_or_else(|| "unknown".into());
    let cur = st
        .current_boot
        .map(|t| t.name().to_string())
        .unwrap_or_else(|| "unknown".into());
    let _ = writeln!(out, "Running: {cur}   Next boot (predicted): {next}");
    if let Some(e) = &st.boot_error {
        if !st.grubenv_broken {
            let _ = writeln!(out, "  Boot state: {}", ellipsize(e, 62));
        }
    }
    out.push('\n');
    for s in &st.slots {
        slot_lines(&mut out, s);
    }
    out.push('\n');

    let sys = st.rescue.system.clone().unwrap_or_default();
    let _ = writeln!(
        out,
        "Rescue: {} {}  app {}  host {}",
        sys.name.as_deref().unwrap_or("rescue"),
        opt(&sys.version_id),
        st.rescue.app_version,
        ellipsize(opt(&sys.hostname), 24)
    );
    let check = match &st.rescue.image_check {
        RescueCheck::NotChecked => "not checked".to_string(),
        RescueCheck::Checking => "checking...".to_string(),
        RescueCheck::Done(sha) => {
            let matches: Vec<String> = st
                .slots
                .iter()
                .filter_map(|s| {
                    s.rescue_match
                        .map(|m| format!("{}: {}", s.slot.letter(), yes_no(m)))
                })
                .collect();
            format!(
                "{}  matches slot {}",
                ellipsize(sha, 19),
                if matches.is_empty() {
                    "-".to_string()
                } else {
                    matches.join(", ")
                }
            )
        }
        RescueCheck::Failed(e) => format!("failed: {}", ellipsize(e, 50)),
    };
    let _ = writeln!(out, "  Image SHA-256: {check}");
    out.push('\n');

    let _ = writeln!(out, "Persistent partitions:");
    for p in &st.persistent {
        let usage = match (&p.usage, &p.error) {
            (Some(u), _) => format!(
                "{} used of {} ({} free)",
                format_bytes(u.used),
                format_bytes(u.total),
                format_bytes(u.free())
            ),
            (None, Some(e)) => ellipsize(e, 46),
            (None, None) => "usage unknown".into(),
        };
        let _ = writeln!(
            out,
            "  {:<12} {:<12} {}",
            p.label,
            p.device.as_deref().unwrap_or("missing"),
            usage
        );
    }
    out.push('\n');

    let _ = writeln!(out, "Network:");
    if st.network.interfaces.is_empty() {
        let _ = writeln!(out, "  no interfaces");
    }
    for i in &st.network.interfaces {
        let addrs: Vec<&str> = i
            .addresses
            .iter()
            .filter(|a| !a.starts_with("fe80"))
            .map(String::as_str)
            .collect();
        let _ = writeln!(
            out,
            "  {:<10} {:<8} {}",
            ellipsize(&i.name, 10),
            i.state.as_deref().unwrap_or("?"),
            ellipsize(&addrs.join(" "), 52)
        );
    }
    let internet = match st.network.internet_reachable {
        Some(b) => yes_no(b),
        None => "not checked (Network -> Test)",
    };
    let _ = writeln!(out, "  Internet reachable: {internet}");
    let sync = match st.clock.synchronized {
        Some(b) => yes_no(b),
        None => "unknown",
    };
    let _ = writeln!(out, "\nTime: {}  synchronized: {sync}", st.clock.utc);
    out
}

/// Hardware details text (SYS-2).
pub fn hardware_text(hw: &HardwareInfo) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Board:      {}", ellipsize(&hw.board_summary(), 64));
    if hw.board_model.is_none() {
        if let Some(b) = &hw.dmi.board_name {
            let _ = writeln!(out, "Mainboard:  {}", ellipsize(b, 64));
        }
        if let Some(b) = &hw.dmi.bios_version {
            let _ = writeln!(out, "BIOS:       {}", ellipsize(b, 64));
        }
    }
    let _ = writeln!(out, "Serial:     {}", opt(&hw.serial_number));
    let _ = writeln!(
        out,
        "CPU:        {} ({} cores)",
        ellipsize(hw.cpu_model.as_deref().unwrap_or("unknown"), 50),
        hw.cpu_cores
    );
    let mem = match (hw.mem_total_kib, hw.mem_available_kib) {
        (Some(t), Some(a)) => format!(
            "{} total, {} available",
            format_bytes(t * 1024),
            format_bytes(a * 1024)
        ),
        (Some(t), None) => format!("{} total", format_bytes(t * 1024)),
        _ => "unknown".into(),
    };
    let _ = writeln!(out, "Memory:     {mem}");
    if hw.temperatures.is_empty() {
        let _ = writeln!(out, "Temp:       unknown");
    }
    for t in &hw.temperatures {
        let _ = writeln!(
            out,
            "Temp:       {:.1} C ({})",
            t.celsius,
            ellipsize(t.kind.as_deref().unwrap_or(&t.zone), 40)
        );
    }
    if let Some(th) = &hw.throttled {
        let flags = if th.flags.is_empty() {
            "ok".to_string()
        } else {
            th.flags.join(", ")
        };
        let _ = writeln!(out, "Throttling: 0x{:x}", th.raw);
        for line in wrap(&flags, 62) {
            let _ = writeln!(out, "            {line}");
        }
    }
    let _ = writeln!(out, "Kernel:     {}", opt(&hw.kernel_release));
    let _ = writeln!(
        out,
        "Uptime:     {}",
        hw.uptime_secs
            .map(format_uptime)
            .unwrap_or_else(|| "-".into())
    );
    let _ = writeln!(out, "Command line:");
    for line in wrap(hw.cmdline.as_deref().unwrap_or("-"), 72) {
        let _ = writeln!(out, "  {line}");
    }
    out
}

/// Word-wrap text to `width` columns (long words are split).
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        while word.chars().count() > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let head: String = word.chars().take(width).collect();
            word = word.chars().skip(width).collect();
            lines.push(head);
        }
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&word);
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Storage text (SYS-3): block devices and the boot disk partition table.
pub fn storage_text(
    devices: &BackendResult<Vec<BlockDevice>>,
    layout: &BackendResult<Layout>,
) -> String {
    let mut out = String::new();
    let devices = match devices {
        Ok(d) => d,
        Err(e) => {
            let _ = writeln!(out, "Block devices: {e}");
            return out;
        }
    };
    let _ = writeln!(out, "Disk     Size       Transport Rem  Model");
    for d in devices {
        let _ = writeln!(
            out,
            "{:<8} {:<10} {:<9} {:<4} {}{}",
            ellipsize(&d.name, 8),
            format_bytes(d.size),
            ellipsize(&d.transport, 9),
            yes_no(d.removable),
            ellipsize(d.model.as_deref().unwrap_or("-"), 26),
            if d.boot_disk { "  [boot disk]" } else { "" }
        );
    }
    out.push('\n');
    match devices.iter().find(|d| d.boot_disk) {
        None => {
            let _ = writeln!(out, "Boot disk: not found");
        }
        Some(disk) => {
            let _ = writeln!(out, "Boot disk {} partitions:", disk.name);
            let _ = writeln!(
                out,
                " #  Device      Label        Size       FS       Mounted"
            );
            for p in &disk.partitions {
                let _ = writeln!(
                    out,
                    "{:>2}  {:<11} {:<12} {:<10} {:<8} {}",
                    p.number,
                    ellipsize(&p.name, 11),
                    ellipsize(p.partlabel.as_deref().unwrap_or("-"), 12),
                    format_bytes(p.size),
                    ellipsize(p.fstype.as_deref().unwrap_or("-"), 8),
                    ellipsize(p.mountpoint.as_deref().unwrap_or("-"), 20)
                );
            }
            out.push('\n');
            let _ = writeln!(out, "PARTUUIDs:");
            for p in &disk.partitions {
                let _ = writeln!(
                    out,
                    "{:>2}  {}",
                    p.number,
                    p.partuuid.as_deref().unwrap_or("-")
                );
            }
        }
    }
    out.push('\n');
    match layout {
        Ok(_) => {
            let _ = writeln!(out, "XorOS layout: OK (labels match partition numbers)");
        }
        Err(e) => {
            let _ = writeln!(out, "XorOS layout: {}", ellipsize(&e.to_string(), 60));
        }
    }
    out
}

/// Known grubenv variables with values (BOOT-1).
pub fn boot_vars_text(b: &BootState) -> String {
    let mut out = String::new();
    for name in GRUBENV_KNOWN_VARS {
        let value = b
            .raw
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| format!("\"{v}\""))
            .unwrap_or_else(|| "(not set, default)".into());
        let _ = writeln!(out, "  {name:<14} {}", ellipsize(&value, 56));
    }
    out
}

/// Raw grubenv content (BOOT-1 raw view).
pub fn boot_raw_text(b: &BootState) -> String {
    let mut out = String::from("# GRUB Environment Block\n");
    for (k, v) in &b.raw {
        let _ = writeln!(out, "{k}={v}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::backend::SystemBackend;
    use crate::hw::testutil::TempDir;
    use crate::status::collect_status;

    fn max_width(s: &str) -> usize {
        s.lines().map(|l| l.chars().count()).max().unwrap_or(0)
    }

    #[test]
    fn texts_fit_80_columns() {
        let d = TempDir::new("report");
        SimBackend::create_example(d.path()).expect("example");
        let b = SimBackend::open(d.path()).expect("open");
        let st = collect_status(&b, RescueCheck::Done("ab".repeat(32)));
        let t = status_text(&st);
        assert!(t.contains("Both slots exceeded MAX_TRIES"), "{t}");
        assert!(max_width(&t) <= 76, "{t}");
        let s = storage_text(&b.block_devices(), &b.layout());
        assert!(s.contains("[boot disk]"));
        assert!(max_width(&s) <= 76, "{s}");
        let hw = crate::hw::hardware_info(&crate::hw::HostPaths::default());
        assert!(max_width(&hardware_text(&hw)) <= 76);
        let bs = b.boot_state().expect("state");
        assert!(boot_vars_text(&bs).contains("ROOTFS_A_TRY"));
        assert!(boot_raw_text(&bs).starts_with("# GRUB"));
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("a bb ccc", 4), vec!["a bb", "ccc"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("", 3), vec![""]);
        assert_eq!(ellipsize("abcdef", 5), "ab...");
    }
}
