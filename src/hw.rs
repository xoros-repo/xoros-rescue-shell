//! Hardware details from procfs/sysfs (SYS-2). Paths are injectable for tests and simulation.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Roots of the pseudo file systems (real: `/proc`, `/sys`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPaths {
    pub proc_root: PathBuf,
    pub sys_root: PathBuf,
}

impl Default for HostPaths {
    fn default() -> Self {
        HostPaths {
            proc_root: PathBuf::from("/proc"),
            sys_root: PathBuf::from("/sys"),
        }
    }
}

impl HostPaths {
    pub fn proc(&self, rel: &str) -> PathBuf {
        self.proc_root.join(rel)
    }

    pub fn sys(&self, rel: &str) -> PathBuf {
        self.sys_root.join(rel)
    }
}

/// Read a small text file, trimmed (also of NUL bytes from device-tree files). `None` if
/// missing, unreadable or empty.
pub fn read_trimmed(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let s = String::from_utf8_lossy(&bytes);
    let t = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DmiInfo {
    pub sys_vendor: Option<String>,
    pub product_name: Option<String>,
    pub board_name: Option<String>,
    pub bios_version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Temperature {
    pub zone: String,
    pub kind: Option<String>,
    pub celsius: f64,
}

/// Raspberry Pi firmware throttling state (`get_throttled`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Throttled {
    pub raw: u32,
    pub flags: Vec<&'static str>,
}

const THROTTLE_BITS: &[(u32, &str)] = &[
    (0, "under-voltage now"),
    (1, "ARM frequency capped now"),
    (2, "throttled now"),
    (3, "soft temperature limit now"),
    (16, "under-voltage occurred"),
    (17, "ARM frequency capping occurred"),
    (18, "throttling occurred"),
    (19, "soft temperature limit occurred"),
];

impl Throttled {
    /// Decode the hex value from `get_throttled` (e.g. `50005` or `0x50005`).
    pub fn parse(s: &str) -> Option<Throttled> {
        let t = s.trim();
        let t = t.strip_prefix("0x").unwrap_or(t);
        let raw = u32::from_str_radix(t, 16).ok()?;
        let flags = THROTTLE_BITS
            .iter()
            .filter(|(bit, _)| raw & (1 << bit) != 0)
            .map(|(_, name)| *name)
            .collect();
        Some(Throttled { raw, flags })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct HardwareInfo {
    /// `/proc/device-tree/model` (arm64).
    pub board_model: Option<String>,
    pub dmi: DmiInfo,
    pub cpu_model: Option<String>,
    pub cpu_cores: usize,
    pub mem_total_kib: Option<u64>,
    pub mem_available_kib: Option<u64>,
    pub temperatures: Vec<Temperature>,
    pub throttled: Option<Throttled>,
    pub kernel_release: Option<String>,
    pub kernel_version: Option<String>,
    pub cmdline: Option<String>,
    pub uptime_secs: Option<u64>,
    pub serial_number: Option<String>,
}

impl HardwareInfo {
    /// A one-line description of the machine.
    pub fn board_summary(&self) -> String {
        if let Some(m) = &self.board_model {
            return m.clone();
        }
        let parts: Vec<&str> = [&self.dmi.sys_vendor, &self.dmi.product_name]
            .iter()
            .filter_map(|o| o.as_deref())
            .collect();
        if parts.is_empty() {
            "unknown".to_string()
        } else {
            parts.join(" ")
        }
    }
}

/// `(model, cores)` from `/proc/cpuinfo`. Handles x86 (`model name`) and arm64 (`Model`,
/// `Hardware`, `CPU part`).
pub fn parse_cpuinfo(text: &str) -> (Option<String>, usize) {
    let mut model = None;
    let mut board_model = None;
    let mut hardware = None;
    let mut cores = 0;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "processor" => cores += 1,
            "model name" if model.is_none() && !v.is_empty() => model = Some(v.to_string()),
            "Model" if !v.is_empty() => board_model = Some(v.to_string()),
            "Hardware" if !v.is_empty() => hardware = Some(v.to_string()),
            _ => {}
        }
    }
    (model.or(hardware).or(board_model), cores)
}

/// `(MemTotal, MemAvailable)` in KiB from `/proc/meminfo`.
pub fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>) {
    let mut total = None;
    let mut avail = None;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let key = it.next();
        let val = it.next().and_then(|v| v.parse::<u64>().ok());
        match key {
            Some("MemTotal:") => total = val,
            Some("MemAvailable:") => avail = val,
            _ => {}
        }
    }
    (total, avail)
}

/// Seconds from `/proc/uptime`.
pub fn parse_uptime(text: &str) -> Option<u64> {
    let first = text.split_whitespace().next()?;
    let secs: f64 = first.parse().ok()?;
    if secs.is_finite() && secs >= 0.0 {
        Some(secs as u64)
    } else {
        None
    }
}

fn read_temperatures(paths: &HostPaths) -> Vec<Temperature> {
    let dir = paths.sys("class/thermal");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut zones: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("thermal_zone"))
        })
        .collect();
    zones.sort();
    zones
        .into_iter()
        .filter_map(|z| {
            let milli: i64 = read_trimmed(&z.join("temp"))?.parse().ok()?;
            Some(Temperature {
                zone: z.file_name()?.to_string_lossy().into_owned(),
                kind: read_trimmed(&z.join("type")),
                celsius: milli as f64 / 1000.0,
            })
        })
        .collect()
}

/// Collect hardware details. Missing files simply leave fields empty.
pub fn hardware_info(paths: &HostPaths) -> HardwareInfo {
    let dmi_dir = paths.sys("class/dmi/id");
    let dmi = DmiInfo {
        sys_vendor: read_trimmed(&dmi_dir.join("sys_vendor")),
        product_name: read_trimmed(&dmi_dir.join("product_name")),
        board_name: read_trimmed(&dmi_dir.join("board_name")),
        bios_version: read_trimmed(&dmi_dir.join("bios_version")),
    };
    let (cpu_model, cpu_cores) = fs::read_to_string(paths.proc("cpuinfo"))
        .map(|t| parse_cpuinfo(&t))
        .unwrap_or((None, 0));
    let (mem_total_kib, mem_available_kib) = fs::read_to_string(paths.proc("meminfo"))
        .map(|t| parse_meminfo(&t))
        .unwrap_or((None, None));
    let serial_number = read_trimmed(&paths.proc("device-tree/serial-number"))
        .or_else(|| read_trimmed(&dmi_dir.join("product_serial")));
    HardwareInfo {
        board_model: read_trimmed(&paths.proc("device-tree/model")),
        dmi,
        cpu_model,
        cpu_cores,
        mem_total_kib,
        mem_available_kib,
        temperatures: read_temperatures(paths),
        throttled: read_trimmed(&paths.sys("devices/platform/soc/soc:firmware/get_throttled"))
            .and_then(|t| Throttled::parse(&t)),
        kernel_release: read_trimmed(&paths.proc("sys/kernel/osrelease")),
        kernel_version: read_trimmed(&paths.proc("sys/kernel/version")),
        cmdline: read_trimmed(&paths.proc("cmdline")),
        uptime_secs: fs::read_to_string(paths.proc("uptime"))
            .ok()
            .and_then(|t| parse_uptime(&t)),
        serial_number,
    }
}

/// Format seconds as `3d 4h 05m` / `4h 05m` / `5m 07s`.
pub fn format_uptime(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86400,
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h}h {m:02}m")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m {s:02}s")
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// A fresh empty directory under the system temp dir, removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> TempDir {
            let p = std::env::temp_dir().join(format!(
                "xrs-test-{}-{}-{}",
                tag,
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).expect("create temp dir");
            TempDir(p)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }

        pub fn write(&self, rel: &str, content: &[u8]) {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).expect("mkdir");
            }
            fs::write(p, content).expect("write fixture");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::TempDir;
    use super::*;

    const PI_CPUINFO: &str = "processor\t: 0\nBogoMIPS\t: 108.00\nCPU part\t: 0xd08\n\n\
processor\t: 1\n\nprocessor\t: 2\n\nprocessor\t: 3\n\n\
Hardware\t: BCM2835\nRevision\t: c03111\nSerial\t\t: 10000000abcdef\n\
Model\t\t: Raspberry Pi 4 Model B Rev 1.1\n";

    #[test]
    fn cpuinfo_x86_and_arm() {
        let x86 = "processor\t: 0\nmodel name\t: Intel(R) Core(TM) i5\n\nprocessor\t: 1\nmodel name\t: Intel(R) Core(TM) i5\n";
        assert_eq!(
            parse_cpuinfo(x86),
            (Some("Intel(R) Core(TM) i5".to_string()), 2)
        );
        assert_eq!(parse_cpuinfo(PI_CPUINFO), (Some("BCM2835".to_string()), 4));
        assert_eq!(parse_cpuinfo(""), (None, 0));
    }

    #[test]
    fn meminfo_and_uptime() {
        let m = "MemTotal:        3884852 kB\nMemFree:  100 kB\nMemAvailable:    2884852 kB\n";
        assert_eq!(parse_meminfo(m), (Some(3884852), Some(2884852)));
        assert_eq!(parse_meminfo("garbage"), (None, None));
        assert_eq!(parse_uptime("12345.67 4000.00\n"), Some(12345));
        assert_eq!(parse_uptime("x"), None);
        assert_eq!(format_uptime(59), "0m 59s");
        assert_eq!(format_uptime(3 * 3600 + 5 * 60), "3h 05m");
        assert_eq!(format_uptime(2 * 86400 + 3600), "2d 1h 00m");
    }

    #[test]
    fn throttled_decode() {
        let t = Throttled::parse("50005").expect("parse");
        assert_eq!(t.raw, 0x50005);
        assert_eq!(
            t.flags,
            vec![
                "under-voltage now",
                "throttled now",
                "under-voltage occurred",
                "throttling occurred"
            ]
        );
        assert_eq!(Throttled::parse("0x0").map(|t| t.flags.len()), Some(0));
        assert!(Throttled::parse("zz").is_none());
    }

    #[test]
    fn pi_fixture_tree() {
        let d = TempDir::new("hw-pi");
        d.write("proc/cpuinfo", PI_CPUINFO.as_bytes());
        d.write("proc/meminfo", b"MemTotal: 1000 kB\nMemAvailable: 500 kB\n");
        d.write(
            "proc/device-tree/model",
            b"Raspberry Pi 4 Model B Rev 1.1\0",
        );
        d.write("proc/device-tree/serial-number", b"10000000abcdef\0");
        d.write("proc/sys/kernel/osrelease", b"6.6.20-v8\n");
        d.write("proc/cmdline", b"root=PARTLABEL=rescue boot.slot=RESCUE\n");
        d.write("proc/uptime", b"100.5 50.0\n");
        d.write("sys/class/thermal/thermal_zone0/temp", b"48500\n");
        d.write("sys/class/thermal/thermal_zone0/type", b"cpu-thermal\n");
        d.write(
            "sys/devices/platform/soc/soc:firmware/get_throttled",
            b"0x1\n",
        );
        let paths = HostPaths {
            proc_root: d.path().join("proc"),
            sys_root: d.path().join("sys"),
        };
        let hw = hardware_info(&paths);
        assert_eq!(hw.board_summary(), "Raspberry Pi 4 Model B Rev 1.1");
        assert_eq!(hw.serial_number.as_deref(), Some("10000000abcdef"));
        assert_eq!(hw.cpu_cores, 4);
        assert_eq!(hw.mem_total_kib, Some(1000));
        assert_eq!(hw.temperatures.len(), 1);
        assert_eq!(hw.temperatures[0].celsius, 48.5);
        assert_eq!(hw.temperatures[0].kind.as_deref(), Some("cpu-thermal"));
        assert_eq!(
            hw.throttled.map(|t| t.flags),
            Some(vec!["under-voltage now"])
        );
        assert_eq!(hw.kernel_release.as_deref(), Some("6.6.20-v8"));
        assert_eq!(hw.uptime_secs, Some(100));
    }

    #[test]
    fn x86_fixture_tree_and_missing_files() {
        let d = TempDir::new("hw-x86");
        d.write("sys/class/dmi/id/sys_vendor", b"QEMU\n");
        d.write(
            "sys/class/dmi/id/product_name",
            b"Standard PC (Q35 + ICH9, 2009)\n",
        );
        d.write("sys/class/dmi/id/product_serial", b"SER123\n");
        let paths = HostPaths {
            proc_root: d.path().join("proc"),
            sys_root: d.path().join("sys"),
        };
        let hw = hardware_info(&paths);
        assert_eq!(hw.board_summary(), "QEMU Standard PC (Q35 + ICH9, 2009)");
        assert_eq!(hw.serial_number.as_deref(), Some("SER123"));
        assert_eq!(hw.cpu_cores, 0);
        assert!(hw.temperatures.is_empty());
        assert!(hw.throttled.is_none());
    }
}
