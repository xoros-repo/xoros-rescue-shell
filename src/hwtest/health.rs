//! TST-7: disk health. Per disk, whatever is available:
//!
//! - `smartctl -H -A /dev/<disk>` (DIST-2): the exit status bits give the verdict, the output
//!   is shown, and reallocated/pending/uncorrectable sector counts are flagged;
//! - NVMe: `nvme smart-log /dev/<disk>`, or without nvme-cli the SMART / health log read
//!   directly with the NVMe admin ioctl (critical warning, spare, wear, media errors);
//! - sysfs: model, firmware and state, SCSI I/O error counters, eMMC `life_time` and
//!   `pre_eol_info`, an NVMe `critical_warning` attribute where the kernel provides one.
//!
//! When neither tool nor a health attribute is available the report says that SMART is
//! unavailable. Nothing is written to the disks.

use std::fmt::Write as _;
use std::path::Path;

use super::{worst, Outcome, TestReport};
use crate::backend::{BlockDevice, SystemBackend};
use crate::backup::tools::{CommandRunner, ToolError};
use crate::hw::read_trimmed;
use crate::tasks::{format_bytes, TaskCtx, TaskFailure};

/// Output lines of a tool kept per disk.
const MAX_TOOL_LINES: usize = 60;

/// Health of one disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskHealth {
    pub outcome: Option<Outcome>,
    /// A health source (SMART, NVMe log, eMMC life time) gave a verdict.
    pub known: bool,
    /// Findings for the summary (`sda: SMART FAILED`).
    pub findings: Vec<String>,
    pub details: String,
}

impl DiskHealth {
    fn flag(&mut self, o: Outcome, finding: impl Into<String>) {
        self.outcome = Some(self.outcome.map_or(o, |x| worst(x, o)));
        let f = finding.into();
        let _ = writeln!(self.details, "  -> {o}: {f}");
        self.findings.push(f);
    }

    fn line(&mut self, text: impl AsRef<str>) {
        let _ = writeln!(self.details, "  {}", text.as_ref());
    }
}

// --- smartctl ------------------------------------------------------------------------------

/// Meaning of the smartctl exit status (bit mask, smartctl(8)): the overall outcome and each
/// set bit with its outcome. Bits 0-2 (no SMART data) are `Error`.
pub fn smartctl_status(code: i32) -> (Outcome, Vec<(Outcome, &'static str)>) {
    const BITS: &[(i32, Outcome, &str)] = &[
        (0, Outcome::Error, "command line not accepted"),
        (1, Outcome::Error, "device could not be opened (no SMART over this bridge?)"),
        (2, Outcome::Error, "a SMART command failed or the data checksum is wrong"),
        (3, Outcome::Fail, "SMART status: DISK FAILING"),
        (4, Outcome::Fail, "prefail attributes at or below threshold"),
        (5, Outcome::Warning, "usage attributes were at or below threshold in the past"),
        (6, Outcome::Warning, "the device error log has errors"),
        (7, Outcome::Warning, "the self-test log has errors"),
    ];
    let mut out = Outcome::Pass;
    let mut msgs = Vec::new();
    for (bit, o, m) in BITS {
        if code & (1 << bit) != 0 {
            out = worst(out, *o);
            msgs.push((*o, *m));
        }
    }
    (out, msgs)
}

/// ATA attributes whose raw value should be 0 on a healthy disk.
const BAD_SECTOR_ATTRS: &[&str] = &[
    "Reallocated_Sector_Ct",
    "Current_Pending_Sector",
    "Offline_Uncorrectable",
    "Reported_Uncorrect",
];

/// Non-zero bad-sector attributes in a `smartctl -A` table (`name`, raw value).
pub fn bad_sector_attributes(output: &str) -> Vec<(String, u64)> {
    output
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // ID# ATTRIBUTE_NAME FLAG VALUE WORST THRESH TYPE UPDATED WHEN_FAILED RAW_VALUE
            if f.len() < 10 || f[0].parse::<u32>().is_err() || !BAD_SECTOR_ATTRS.contains(&f[1]) {
                return None;
            }
            let raw = leading_number(f[9])?;
            (raw > 0).then(|| (f[1].to_string(), raw))
        })
        .collect()
}

/// First number of a value like `1,234`, `3%`, `34 C`.
fn leading_number(v: &str) -> Option<u64> {
    let digits: String = v
        .trim()
        .chars()
        .filter(|c| *c != ',')
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn tool_output(out: &str) -> String {
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut t = String::new();
    for l in lines.iter().take(MAX_TOOL_LINES) {
        let _ = writeln!(t, "    {l}");
    }
    if lines.len() > MAX_TOOL_LINES {
        let _ = writeln!(t, "    ... {} more line(s)", lines.len() - MAX_TOOL_LINES);
    }
    t
}

/// Run smartctl; `false` if it is not installed.
fn smartctl(runner: &dyn CommandRunner, dev: &str, h: &mut DiskHealth) -> bool {
    match runner.run("smartctl", &["-H", "-A", dev]) {
        Err(ToolError::Missing(_)) => false,
        Err(e) => {
            h.line(format!("smartctl: {e}"));
            true
        }
        Ok(out) => {
            h.line(format!("$ smartctl -H -A {dev}"));
            h.details.push_str(&tool_output(&out.stdout));
            let code = out.code.unwrap_or(-1);
            if code < 0 {
                h.flag(Outcome::Error, "smartctl was killed");
                return true;
            }
            let (o, msgs) = smartctl_status(code);
            if code & 0b111 == 0 {
                h.known = true;
            }
            for (bit_outcome, m) in msgs {
                if bit_outcome == Outcome::Error {
                    // No SMART data (e.g. a USB bridge): a note, not a disk problem.
                    h.line(format!("smartctl: {m}"));
                } else {
                    h.flag(bit_outcome, format!("smartctl: {m}"));
                }
            }
            for (name, raw) in bad_sector_attributes(&out.stdout) {
                h.flag(Outcome::Warning, format!("{name} = {raw}"));
            }
            if o == Outcome::Pass {
                h.line("SMART overall health: PASSED");
            }
            true
        }
    }
}

// --- NVMe --------------------------------------------------------------------------------

/// Values of the NVMe SMART / health information log (log page 02h).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NvmeHealth {
    pub critical_warning: u8,
    pub temperature_c: Option<i64>,
    pub available_spare: u8,
    pub spare_threshold: u8,
    pub percentage_used: u8,
    pub power_on_hours: u64,
    pub unsafe_shutdowns: u64,
    pub media_errors: u64,
}

/// Decode the 512-byte log page.
pub fn parse_nvme_log(b: &[u8]) -> Option<NvmeHealth> {
    if b.len() < 192 {
        return None;
    }
    let u64_at = |o: usize| {
        let mut x = [0u8; 8];
        x.copy_from_slice(&b[o..o + 8]);
        u64::from_le_bytes(x)
    };
    let kelvin = u16::from_le_bytes([b[1], b[2]]);
    Some(NvmeHealth {
        critical_warning: b[0],
        temperature_c: (kelvin > 0).then(|| i64::from(kelvin) - 273),
        available_spare: b[3],
        spare_threshold: b[4],
        percentage_used: b[5],
        power_on_hours: u64_at(128),
        unsafe_shutdowns: u64_at(144),
        media_errors: u64_at(160),
    })
}

/// The interesting fields of `nvme smart-log` text output.
pub fn parse_nvme_cli(text: &str) -> Option<NvmeHealth> {
    let mut h = NvmeHealth::default();
    let mut seen = false;
    for l in text.lines() {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase().replace(' ', "_");
        let n = leading_number(v);
        match (key.as_str(), n) {
            ("critical_warning", Some(n)) => {
                h.critical_warning = n as u8;
                seen = true;
            }
            ("temperature", Some(n)) => h.temperature_c = Some(n as i64),
            ("available_spare", Some(n)) => h.available_spare = n as u8,
            ("available_spare_threshold", Some(n)) => h.spare_threshold = n as u8,
            ("percentage_used", Some(n)) => h.percentage_used = n as u8,
            ("power_on_hours", Some(n)) => h.power_on_hours = n,
            ("unsafe_shutdowns", Some(n)) => h.unsafe_shutdowns = n,
            ("media_errors", Some(n)) => h.media_errors = n,
            _ => {}
        }
    }
    seen.then_some(h)
}

const CRITICAL_BITS: &[&str] = &[
    "available spare below threshold",
    "temperature out of range",
    "reliability degraded",
    "media is read-only",
    "volatile memory backup failed",
    "persistent memory region read-only",
];

fn judge_nvme(n: &NvmeHealth, h: &mut DiskHealth) {
    h.known = true;
    h.line(format!(
        "NVMe health: critical warning 0x{:02x}, spare {} % (threshold {} %), used {} %, \
         media errors {}",
        n.critical_warning, n.available_spare, n.spare_threshold, n.percentage_used, n.media_errors
    ));
    h.line(format!(
        "  temperature {}, power-on hours {}, unsafe shutdowns {}",
        n.temperature_c
            .map_or("unknown".to_string(), |t| format!("{t} C")),
        n.power_on_hours,
        n.unsafe_shutdowns
    ));
    for (bit, name) in CRITICAL_BITS.iter().enumerate() {
        if n.critical_warning & (1 << bit) != 0 {
            h.flag(Outcome::Fail, format!("NVMe critical warning: {name}"));
        }
    }
    if n.percentage_used >= 100 {
        h.flag(Outcome::Fail, format!("worn out ({} % used)", n.percentage_used));
    } else if n.percentage_used >= 90 {
        h.flag(Outcome::Warning, format!("{} % of the rated life used", n.percentage_used));
    }
    if n.media_errors > 0 {
        h.flag(Outcome::Warning, format!("{} media error(s)", n.media_errors));
    }
}

/// `NVME_IOCTL_ADMIN_CMD`, `_IOWR('N', 0x41, struct nvme_admin_cmd)` (72 bytes).
const NVME_IOCTL_ADMIN_CMD: u64 = 0xC048_4E41;

/// `struct nvme_admin_cmd` / `nvme_passthru_cmd` (linux/nvme_ioctl.h).
#[repr(C)]
#[derive(Default)]
struct NvmeAdminCmd {
    opcode: u8,
    flags: u8,
    rsvd1: u16,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    metadata: u64,
    addr: u64,
    metadata_len: u32,
    data_len: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
    result: u32,
}

/// Read the SMART / health log with the admin ioctl (root; nvme-cli not needed).
fn nvme_log_ioctl(dev: &Path) -> Result<NvmeHealth, String> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open(dev).map_err(|e| format!("{}: {e}", dev.display()))?;
    let mut buf = [0u8; 512];
    let mut cmd = NvmeAdminCmd {
        opcode: 0x02, // Get Log Page
        nsid: 0xFFFF_FFFF,
        addr: buf.as_mut_ptr() as u64,
        data_len: buf.len() as u32,
        // NUMDL = dwords - 1, log identifier 02h
        cdw10: ((buf.len() as u32 / 4 - 1) << 16) | 0x02,
        timeout_ms: 5000,
        ..NvmeAdminCmd::default()
    };
    // SAFETY: `cmd` matches the kernel's struct (72 bytes) and points at `buf`, which lives
    // until the call returns.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), NVME_IOCTL_ADMIN_CMD as _, &mut cmd) };
    if rc != 0 {
        return Err(format!(
            "NVMe log page ioctl on {}: {}",
            dev.display(),
            if rc < 0 {
                std::io::Error::last_os_error().to_string()
            } else {
                format!("NVMe status 0x{rc:x}")
            }
        ));
    }
    parse_nvme_log(&buf).ok_or_else(|| "short NVMe log page".to_string())
}

// --- eMMC --------------------------------------------------------------------------------

/// eMMC `life_time` (`0x01 0x02`: device life time estimate type A and B, 0x01 = 0-10 % used,
/// ..., 0x0a = 90-100 %, 0x0b = exceeded) and `pre_eol_info` (1 normal, 2 warning: 80 % of
/// the reserved blocks used, 3 urgent).
fn judge_emmc(dir: &Path, h: &mut DiskHealth) {
    let parse_hex = |s: &str| u8::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    if let Some(lt) = read_trimmed(&dir.join("life_time")) {
        h.known = true;
        let v: Vec<u8> = lt.split_whitespace().filter_map(parse_hex).collect();
        let worst_v = v.iter().copied().max().unwrap_or(0);
        let pct = |x: u8| match x {
            0 => "unknown".to_string(),
            1..=10 => format!("{}-{} %", (x - 1) * 10, x * 10),
            _ => "exceeded".to_string(),
        };
        h.line(format!(
            "eMMC life time used: {} ({lt})",
            v.iter().map(|x| pct(*x)).collect::<Vec<_>>().join(" / ")
        ));
        if worst_v >= 0x0b {
            h.flag(Outcome::Fail, "eMMC life time exceeded");
        } else if worst_v >= 0x09 {
            h.flag(Outcome::Warning, format!("eMMC life time {} used", pct(worst_v)));
        }
    }
    if let Some(eol) = read_trimmed(&dir.join("pre_eol_info")) {
        h.known = true;
        match parse_hex(&eol) {
            Some(1) => h.line("eMMC pre-EOL: normal"),
            Some(2) => h.flag(Outcome::Warning, "eMMC pre-EOL warning (80 % of reserved blocks used)"),
            Some(3) => h.flag(Outcome::Fail, "eMMC pre-EOL urgent (reserved blocks nearly used up)"),
            _ => h.line(format!("eMMC pre-EOL: {eol}")),
        }
    }
}

// --- per disk ------------------------------------------------------------------------------

/// Where to look and what may be run.
pub struct HealthEnv<'a> {
    /// `<sys>/block`.
    pub sys_block: &'a Path,
    /// `/dev`.
    pub dev_dir: &'a Path,
    pub runner: &'a dyn CommandRunner,
    /// No ioctls on host devices in simulation mode.
    pub simulation: bool,
}

fn hex_counter(dir: &Path, name: &str) -> Option<u64> {
    let v = read_trimmed(&dir.join(name))?;
    u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()
}

/// Health of one disk (kernel name `disk`).
pub fn check_disk(env: &HealthEnv<'_>, disk: &BlockDevice) -> DiskHealth {
    let mut h = DiskHealth::default();
    let name = disk.name.as_str();
    let dev = env.dev_dir.join(name);
    let dev_s = dev.display().to_string();
    let devdir = env.sys_block.join(name).join("device");
    let mut ident = Vec::new();
    for (label, file) in [
        ("vendor", "vendor"),
        ("model", "model"),
        ("firmware", "firmware_rev"),
        ("rev", "rev"),
        ("serial", "serial"),
        ("state", "state"),
    ] {
        if let Some(v) = read_trimmed(&devdir.join(file)) {
            ident.push(format!("{label} {v}"));
        }
    }
    if !ident.is_empty() {
        h.line(format!("sysfs: {}", ident.join(", ")));
    }
    if let Some(st) = read_trimmed(&devdir.join("state")) {
        if !["running", "live"].contains(&st.as_str()) {
            h.flag(Outcome::Warning, format!("device state '{st}'"));
        }
    }
    // SCSI (SATA, USB) error counters since boot. Information only: they also count "no
    // medium" answers of empty card readers; the disk read test (TST-3) finds real errors.
    if let (Some(err), Some(tmo)) = (
        hex_counter(&devdir, "ioerr_cnt"),
        hex_counter(&devdir, "iotmo_cnt"),
    ) {
        h.line(format!(
            "SCSI commands with errors since boot: {err}, timeouts: {tmo} (information only)"
        ));
    }
    if disk.transport == "mmc" || name.starts_with("mmcblk") {
        judge_emmc(&devdir, &mut h);
    }
    let nvme = disk.transport == "nvme" || name.starts_with("nvme");
    let mut have_tool = false;
    if nvme {
        if let Some(cw) = read_trimmed(&devdir.join("critical_warning")) {
            h.line(format!("sysfs critical_warning: {cw}"));
        }
        match env.runner.run("nvme", &["smart-log", &dev_s]) {
            Err(ToolError::Missing(_)) => {}
            Err(e) => {
                have_tool = true;
                h.line(format!("nvme smart-log: {e}"));
            }
            Ok(out) => {
                have_tool = true;
                h.line(format!("$ nvme smart-log {dev_s}"));
                h.details.push_str(&tool_output(&out.stdout));
                match parse_nvme_cli(&out.stdout) {
                    Some(n) if out.code == Some(0) => judge_nvme(&n, &mut h),
                    _ => h.line(format!(
                        "nvme smart-log failed (exit code {:?}): {}",
                        out.code,
                        out.stderr.trim()
                    )),
                }
            }
        }
        if !h.known && !env.simulation {
            match nvme_log_ioctl(&dev) {
                Ok(n) => {
                    h.line("NVMe health log read with the admin ioctl (nvme-cli not used).");
                    judge_nvme(&n, &mut h);
                }
                Err(e) => h.line(e),
            }
        }
    }
    if !h.known {
        have_tool |= smartctl(env.runner, &dev_s, &mut h);
    }
    if !h.known {
        h.line(if have_tool {
            "No SMART data from this device (common for USB sticks and SD cards)."
        } else {
            "SMART unavailable: smartctl/nvme are not in this rescue image (DIST-2); \
             only sysfs information is shown."
        });
    }
    h
}

/// Disks the health test looks at: real disks, not loop/RAM devices.
pub fn testable(d: &BlockDevice) -> bool {
    !["loop", "ram", "zram"].iter().any(|p| d.name.starts_with(p))
}

/// Run the test over `disks` (kernel names; all disks when empty).
pub fn run(
    backend: &dyn SystemBackend,
    disks: &[String],
    ctx: &TaskCtx,
) -> Result<TestReport, TaskFailure> {
    let all = backend.block_devices().map_err(TaskFailure::from)?;
    let selected: Vec<BlockDevice> = if disks.is_empty() {
        all.into_iter().filter(testable).collect()
    } else {
        let mut v = Vec::new();
        for n in disks {
            v.push(
                all.iter()
                    .find(|d| &d.name == n)
                    .cloned()
                    .ok_or_else(|| TaskFailure::Error(format!("No disk '{n}'.")))?,
            );
        }
        v
    };
    if selected.is_empty() {
        return Err(TaskFailure::Error("No disks found.".into()));
    }
    let sys_block = backend.host_paths().sys("block");
    let runner = backend.command_runner();
    let env = HealthEnv {
        sys_block: &sys_block,
        dev_dir: Path::new("/dev"),
        runner: runner.as_ref(),
        simulation: backend.is_simulation(),
    };
    let mut outcome = Outcome::Pass;
    let mut any_known = false;
    let mut summary = Vec::new();
    let mut details = String::new();
    ctx.set_progress(0, selected.len() as u64);
    for (i, d) in selected.iter().enumerate() {
        ctx.check_cancelled()?;
        ctx.set_message(format!("Checking {}", d.name));
        let h = check_disk(&env, d);
        let o = h.outcome.unwrap_or(Outcome::Pass);
        outcome = worst(outcome, o);
        any_known |= h.known;
        let verdict = if !h.known && h.findings.is_empty() {
            "no health data".to_string()
        } else if h.findings.is_empty() {
            "OK".to_string()
        } else {
            h.findings.join(", ")
        };
        summary.push(format!("{}: {verdict}", d.name));
        let _ = writeln!(
            details,
            "{} ({}, {}, {}){}: {o}\n{}",
            d.name,
            d.model.as_deref().unwrap_or("unknown model"),
            format_bytes(d.size),
            d.transport,
            if d.boot_disk { " [boot disk]" } else { "" },
            h.details
        );
        ctx.set_progress(i as u64 + 1, selected.len() as u64);
    }
    if !any_known && outcome == Outcome::Pass {
        // Nothing could be judged: not a pass.
        outcome = Outcome::Warning;
    }
    Ok(TestReport {
        outcome,
        target: selected
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>()
            .join(" + "),
        summary: summary.join("; "),
        details,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::tools::CommandOutput;
    use crate::hw::testutil::TempDir;
    use std::collections::BTreeMap;

    /// Fake tools: program -> (exit code, stdout); others are missing.
    struct Tools(BTreeMap<&'static str, (i32, String)>);

    impl CommandRunner for Tools {
        fn run(&self, program: &str, _args: &[&str]) -> Result<CommandOutput, ToolError> {
            let (code, out) = self
                .0
                .get(program)
                .ok_or_else(|| ToolError::Missing(program.into()))?;
            Ok(CommandOutput {
                code: Some(*code),
                stdout: out.clone(),
                stderr: String::new(),
            })
        }
    }

    fn disk(name: &str, transport: &str) -> BlockDevice {
        BlockDevice {
            name: name.into(),
            size: 1 << 30,
            transport: transport.into(),
            ..BlockDevice::default()
        }
    }

    fn env<'a>(d: &'a TempDir, runner: &'a Tools) -> HealthEnv<'a> {
        HealthEnv {
            sys_block: Box::leak(Box::new(d.path().join("block"))),
            dev_dir: Path::new("/dev"),
            runner,
            simulation: true,
        }
    }

    const SMART_ATA: &str = "\
SMART overall-health self-assessment test result: PASSED
ID# ATTRIBUTE_NAME          FLAG     VALUE WORST THRESH TYPE      UPDATED  WHEN_FAILED RAW_VALUE
  5 Reallocated_Sector_Ct   0x0033   100   100   010    Pre-fail  Always       -       8
  9 Power_On_Hours          0x0032   099   099   000    Old_age   Always       -       1,234
197 Current_Pending_Sector  0x0012   100   100   000    Old_age   Always       -       0
";

    #[test]
    fn smartctl_bits_and_attributes() {
        assert_eq!(smartctl_status(0), (Outcome::Pass, vec![]));
        assert_eq!(smartctl_status(1 << 3).0, Outcome::Fail);
        assert_eq!(smartctl_status(1 << 6).0, Outcome::Warning);
        assert_eq!(smartctl_status(2).0, Outcome::Error);
        assert_eq!(
            bad_sector_attributes(SMART_ATA),
            vec![("Reallocated_Sector_Ct".to_string(), 8)]
        );
        let d = TempDir::new("health-smart");
        let t = Tools([("smartctl", (0, SMART_ATA.to_string()))].into());
        let h = check_disk(&env(&d, &t), &disk("sda", "sata"));
        assert!(h.known);
        assert_eq!(h.outcome, Some(Outcome::Warning));
        assert_eq!(h.findings, vec!["Reallocated_Sector_Ct = 8"]);
        let t = Tools([("smartctl", (8, "FAILED!\n".to_string()))].into());
        let h = check_disk(&env(&d, &t), &disk("sda", "sata"));
        assert_eq!(h.outcome, Some(Outcome::Fail));
        // USB bridge without SMART: not a disk failure, but no health data either.
        let t = Tools([("smartctl", (2, String::new()))].into());
        let h = check_disk(&env(&d, &t), &disk("sdb", "usb"));
        assert!(!h.known);
        assert!(h.details.contains("No SMART data from this device"));
    }

    #[test]
    fn nvme_log_page_and_cli() {
        let mut b = [0u8; 512];
        b[0] = 0x04; // reliability degraded
        b[1..3].copy_from_slice(&310u16.to_le_bytes());
        b[3] = 100;
        b[4] = 10;
        b[5] = 3;
        b[128..136].copy_from_slice(&500u64.to_le_bytes());
        b[160..168].copy_from_slice(&2u64.to_le_bytes());
        let n = parse_nvme_log(&b).expect("parse");
        assert_eq!(
            (n.critical_warning, n.temperature_c, n.percentage_used, n.power_on_hours),
            (4, Some(37), 3, 500)
        );
        assert_eq!(n.media_errors, 2);
        assert!(parse_nvme_log(&b[..100]).is_none());
        assert_eq!(std::mem::size_of::<NvmeAdminCmd>(), 72);

        let cli = "Smart Log for NVME device:nvme0n1 namespace-id:ffffffff\n\
                   critical_warning                        : 0\n\
                   temperature                             : 34 C (307 Kelvin)\n\
                   available_spare                         : 100%\n\
                   available_spare_threshold               : 10%\n\
                   percentage_used                         : 95%\n\
                   media_errors                            : 0\n\
                   power_on_hours                          : 1,024\n";
        let n = parse_nvme_cli(cli).expect("cli");
        assert_eq!((n.percentage_used, n.power_on_hours, n.temperature_c), (95, 1024, Some(34)));
        let d = TempDir::new("health-nvme");
        let t = Tools([("nvme", (0, cli.to_string()))].into());
        let h = check_disk(&env(&d, &t), &disk("nvme0n1", "nvme"));
        assert!(h.known);
        assert_eq!(h.outcome, Some(Outcome::Warning), "{}", h.details);
        let mut hh = DiskHealth::default();
        judge_nvme(&parse_nvme_log(&b).expect("parse"), &mut hh);
        assert_eq!(hh.outcome, Some(Outcome::Fail));
        assert!(hh.findings[0].contains("reliability degraded"));
    }

    #[test]
    fn emmc_and_sysfs_only() {
        let d = TempDir::new("health-emmc");
        d.write("block/mmcblk0/device/life_time", b"0x02 0x0a\n");
        d.write("block/mmcblk0/device/pre_eol_info", b"0x01\n");
        d.write("block/mmcblk0/device/name", b"SEM16G\n");
        let none = Tools(BTreeMap::new());
        let h = check_disk(&env(&d, &none), &disk("mmcblk0", "mmc"));
        assert!(h.known);
        assert_eq!(h.outcome, Some(Outcome::Warning));
        assert!(h.details.contains("10-20 % / 90-100 %"), "{}", h.details);
        d.write("block/mmcblk0/device/pre_eol_info", b"0x03\n");
        let h = check_disk(&env(&d, &none), &disk("mmcblk0", "mmc"));
        assert_eq!(h.outcome, Some(Outcome::Fail));

        d.write("block/sda/device/model", b"QEMU HARDDISK\n");
        d.write("block/sda/device/state", b"running\n");
        d.write("block/sda/device/ioerr_cnt", b"0x3\n");
        d.write("block/sda/device/iotmo_cnt", b"0x0\n");
        let h = check_disk(&env(&d, &none), &disk("sda", "sata"));
        assert!(!h.known);
        assert_eq!(h.outcome, None);
        assert!(h.details.contains("errors since boot: 3, timeouts: 0"));
        assert!(h.details.contains("model QEMU HARDDISK, state running"));
        assert!(h.details.contains("SMART unavailable"));
    }
}
