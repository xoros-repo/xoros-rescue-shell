//! System report (SYS-5) and diagnostics bundle (LOG-2), written to an external stick.
//!
//! - the report: everything of the Status, Hardware, Storage and Network details screens plus
//!   the boot variables, as text and JSON (`xoros-report-<hostname>-<YYYYMMDD-HHMMSS>.txt` /
//!   `.json`);
//! - the bundle `xoros-diag-<hostname>-<YYYYMMDD-HHMMSS>.tar.gz`: the report, grubenv, both
//!   slots' `xoros.env`, the partition table, `dmesg`, `journalctl -b`, the app log, test
//!   results (`<run dir>/test-results/`), the last network test and the network
//!   configuration. Wi-Fi secrets are never included: the configuration is redacted and lines
//!   that look like `psk=` / password assignments are masked in all logs.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;

use crate::app::App;
use crate::backend::{BlockDevice, BuildInfo, Layout, Slot, Stick, SystemBackend};
use crate::cmd;
use crate::constants::{APP_NAME, APP_VERSION, NET_TEST_REPORT_NAME, TEST_RESULTS_SUBDIR};
use crate::hw::HardwareInfo;
use crate::net::{self, details::NetworkDetails, NetEnv};
use crate::status::SystemStatus;
use crate::tasks::{TaskCtx, TaskFailure};

/// Everything the system report contains (SYS-5).
#[derive(Clone, Debug, Serialize)]
pub struct SystemReport {
    pub generated_utc: String,
    pub app: String,
    pub hostname: String,
    pub status: SystemStatus,
    pub hardware: HardwareInfo,
    pub block_devices: Result<Vec<BlockDevice>, String>,
    pub layout: Result<Layout, String>,
    pub network: NetworkDetails,
}

/// Host name for file names: `[A-Za-z0-9._-]` only, `unknown` if empty.
pub fn hostname(backend: &dyn SystemBackend) -> String {
    let raw = backend
        .rescue_system()
        .ok()
        .and_then(|r| r.hostname)
        .or_else(|| {
            fs::read_to_string(backend.host_paths().proc("sys/kernel/hostname"))
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default();
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '.') {
        "unknown".into()
    } else {
        s
    }
}

/// `YYYYMMDD-HHMMSS` (UTC) for file names.
pub fn file_timestamp() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

pub fn collect(app: &App) -> SystemReport {
    let b = app.backend.as_ref();
    SystemReport {
        generated_utc: chrono::Utc::now()
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string(),
        app: format!("{APP_NAME} {APP_VERSION}"),
        hostname: hostname(b),
        status: app.status(),
        hardware: crate::hw::hardware_info(&b.host_paths()),
        block_devices: b.block_devices().map_err(|e| e.to_string()),
        layout: b.layout().map_err(|e| e.to_string()),
        network: net::details::collect(&NetEnv::for_backend(b)),
    }
}

fn section(out: &mut String, title: &str, body: &str) {
    let _ = writeln!(out, "=== {title} ===\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
}

/// The report as text (SYS-5).
pub fn report_text(r: &SystemReport) -> String {
    let mut out = format!(
        "{} system report\nGenerated: {}\nHost: {}\n\n",
        r.app, r.generated_utc, r.hostname
    );
    section(&mut out, "Status", &crate::report::status_text(&r.status));
    if let Some(b) = &r.status.boot {
        section(&mut out, "Boot variables", &crate::report::boot_raw_text(b));
    }
    section(
        &mut out,
        "Hardware",
        &crate::report::hardware_text(&r.hardware),
    );
    let devices = r
        .block_devices
        .clone()
        .map_err(|e| crate::backend::BackendError::new(crate::backend::ErrorKind::Other, e));
    let layout = r
        .layout
        .clone()
        .map_err(|e| crate::backend::BackendError::new(crate::backend::ErrorKind::Other, e));
    section(
        &mut out,
        "Storage",
        &crate::report::storage_text(&devices, &layout),
    );
    section(&mut out, "Network", &net::details::text(&r.network));
    out
}

pub fn report_json(r: &SystemReport) -> String {
    serde_json::to_string_pretty(r).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")) + "\n"
}

/// Mask values of lines that look like secrets (LOG-2: no Wi-Fi passphrases).
pub fn scrub(text: &str) -> String {
    const KEYS: &[&str] = &[
        "psk=",
        "psk\":",
        "sae_password=",
        "password=",
        "passphrase=",
    ];
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        match KEYS
            .iter()
            .filter_map(|k| lower.find(k).map(|i| i + k.len()))
            .min()
        {
            Some(end) => {
                out.push_str(&line[..end]);
                out.push_str("<hidden>");
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// `xoros.env` text of a slot.
fn env_text(info: &BuildInfo) -> String {
    let mut t = format!(
        "DISTRO={}\nVERSION={}\nMACHINE={}\nSUFFIX={}\nTIMESTAMP={}\n",
        info.distro, info.version, info.machine, info.suffix, info.timestamp
    );
    if let Some(s) = &info.rescue_image_sha256 {
        let _ = writeln!(t, "RESCUE_IMAGE_SHA256={s}");
    }
    for (k, v) in &info.extra {
        let _ = writeln!(t, "{k}={v}");
    }
    t
}

fn tool_text(tool: &str, args: &[&str]) -> String {
    match cmd::run_capture(tool, args) {
        Ok(t) => scrub(&t),
        Err(e) => format!("{e}\n"),
    }
}

/// Files of the bundle as (relative path, content) (LOG-2).
pub fn bundle_files(app: &App, ctx: Option<&TaskCtx>) -> Vec<(String, Vec<u8>)> {
    let msg = |m: &str| {
        if let Some(c) = ctx {
            c.set_message(m);
        }
    };
    let b = app.backend.as_ref();
    let env = NetEnv::for_backend(b);
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut add = |name: &str, text: String| files.push((name.to_string(), text.into_bytes()));

    msg("System report");
    let r = collect(app);
    add("system-report.txt", report_text(&r));
    add("system-report.json", report_json(&r));
    add(
        "grubenv.txt",
        match b.boot_state() {
            Ok(s) => crate::report::boot_raw_text(&s),
            Err(e) => format!("grubenv: {e}\n"),
        },
    );
    for slot in Slot::ALL {
        let text = match b.slot_info(slot) {
            Ok(Some(i)) => env_text(&i),
            Ok(None) => "slot is empty or invalid\n".to_string(),
            Err(e) => format!("{e}\n"),
        };
        add(&format!("{}.xoros.env", slot.label()), text);
    }
    let devices = b.block_devices();
    let layout = b.layout();
    add(
        "partitions.txt",
        crate::report::storage_text(&devices, &layout),
    );
    msg("Kernel log and journal");
    add("dmesg.txt", tool_text("dmesg", &[]));
    add(
        "journal.txt",
        tool_text("journalctl", &["-b", "--no-pager", "-o", "short-iso"]),
    );
    let app_log = crate::logs::read_log(crate::logs::LogSource::App).unwrap_or_else(|e| e);
    add("rescue-shell.log", scrub(&app_log));
    msg("Network");
    add("network.txt", net::details::text(&r.network));
    add(
        "network-config.json",
        net::profile::load_state(&env).redacted().to_json(),
    );
    if let Ok(t) = fs::read_to_string(env.run_dir.join(NET_TEST_REPORT_NAME)) {
        add("network-test.txt", t);
    }
    msg("Test results");
    let tests = b.run_dir().join(TEST_RESULTS_SUBDIR);
    if let Ok(rd) = fs::read_dir(&tests) {
        let mut names: Vec<_> = rd.flatten().filter(|e| e.path().is_file()).collect();
        names.sort_by_key(|e| e.file_name());
        for e in names {
            if let Ok(data) = fs::read(e.path()) {
                let name = e.file_name().to_string_lossy().into_owned();
                files.push((format!("{TEST_RESULTS_SUBDIR}/{name}"), data));
            }
        }
    }
    files
}

/// Write `files` as a gzip-compressed tar below the directory `top`.
pub fn write_tar_gz(out: impl Write, top: &str, files: &[(String, Vec<u8>)]) -> io::Result<()> {
    let gz = flate2::write::GzEncoder::new(out, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    let mtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for (name, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(mtime);
        h.set_cksum();
        tar.append_data(&mut h, format!("{top}/{name}"), data.as_slice())?;
    }
    tar.into_inner()?.finish()?.flush()
}

/// Write `name` on a mounted stick: `<name>.part`, fsync, rename.
fn write_on_stick(
    dir: &Path,
    name: &str,
    fill: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let part = dir.join(format!("{name}.part"));
    let r = (|| {
        let mut f = fs::File::create(&part)?;
        fill(&mut f)?;
        f.sync_all()?;
        fs::rename(&part, dir.join(name))?;
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if r.is_err() {
        let _ = fs::remove_file(&part);
    }
    r
}

fn stick_err(stick: &Stick, e: impl std::fmt::Display) -> TaskFailure {
    TaskFailure::Error(format!("Stick {}: {e}", stick.id))
}

/// SYS-5: write the report as `.txt` and `.json` to the stick root. Returns the file names.
pub fn save_report(app: &App, stick: &Stick) -> Result<Vec<String>, TaskFailure> {
    let r = collect(app);
    let base = format!("xoros-report-{}-{}", r.hostname, file_timestamp());
    let guard = app.backend.mount_stick(stick, true)?;
    let mut names = Vec::new();
    for (ext, text) in [("txt", report_text(&r)), ("json", report_json(&r))] {
        let name = format!("{base}.{ext}");
        write_on_stick(guard.path(), &name, |f| f.write_all(text.as_bytes()))
            .map_err(|e| stick_err(stick, e))?;
        names.push(name);
    }
    guard.unmount()?;
    log::info!(
        "SYS-5: system report written to {}: {}",
        stick.id,
        names.join(", ")
    );
    Ok(names)
}

/// LOG-2: write the diagnostics bundle to the stick root. Returns the file name.
pub fn write_bundle(
    app: &App,
    stick: &Stick,
    ctx: Option<&TaskCtx>,
) -> Result<String, TaskFailure> {
    let files = bundle_files(app, ctx);
    if let Some(c) = ctx {
        c.check_cancelled()?;
        c.set_message("Writing to the stick");
    }
    let top = format!(
        "xoros-diag-{}-{}",
        hostname(app.backend.as_ref()),
        file_timestamp()
    );
    let name = format!("{top}.tar.gz");
    let guard = app.backend.mount_stick(stick, true)?;
    write_on_stick(guard.path(), &name, |f| write_tar_gz(f, &top, &files))
        .map_err(|e| stick_err(stick, e))?;
    guard.unmount()?;
    log::info!(
        "LOG-2: diagnostics bundle {name} ({} files) written to {}",
        files.len(),
        stick.id
    );
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::make_backend;
    use crate::backend::sim::SimBackend;
    use crate::hw::testutil::TempDir;

    #[test]
    fn scrub_secrets() {
        let t = "a\n\tpsk=\"secret\"\nPassword=abc def\n{\"psk\": \"x\"}\nok psk\n";
        let s = scrub(t);
        assert_eq!(
            s,
            "a\n\tpsk=<hidden>\nPassword=<hidden>\n{\"psk\":<hidden>\nok psk\n"
        );
    }

    #[test]
    fn report_and_bundle_on_sim_stick() {
        let d = TempDir::new("diag");
        SimBackend::create_example(d.path()).expect("example");
        let app = App::new(make_backend(Some(d.path().to_path_buf())).expect("backend"));
        let env = NetEnv::for_backend(app.backend.as_ref());
        // A Wi-Fi configuration whose secret must not end up in the bundle.
        let c = net::profile::build(&net::profile::ConfigInput {
            interface: "wlan0".into(),
            wifi: Some(net::profile::WifiInput {
                ssid: "Lab".into(),
                hidden: false,
                security: "wpa3".into(),
                passphrase: "TopSecretPass".into(),
            }),
            ..Default::default()
        })
        .unwrap();
        net::profile::configure(&env, c, None, false).unwrap();
        d.write("run/test-results/memtest.txt", b"memtest: PASS\n");

        let r = collect(&app);
        let text = report_text(&r);
        assert!(text.contains("=== Status ===") && text.contains("=== Network ==="));
        assert!(report_json(&r).contains("\"hardware\""));

        let sticks = app.backend.external_sticks().unwrap();
        let stick = sticks.iter().find(|s| s.id == "usb1").unwrap();
        let names = save_report(&app, stick).unwrap();
        assert_eq!(names.len(), 2);
        assert!(names[0].starts_with("xoros-report-") && names[0].ends_with(".txt"));
        assert!(d.path().join("sticks/usb1").join(&names[1]).is_file());

        let name = write_bundle(&app, stick, None).unwrap();
        assert!(name.starts_with("xoros-diag-") && name.ends_with(".tar.gz"));
        let ts = &name["xoros-diag-".len()..name.len() - ".tar.gz".len()];
        assert_eq!(ts.rsplit('-').next().unwrap().len(), 6, "{name}");
        let f = fs::File::open(d.path().join("sticks/usb1").join(&name)).unwrap();
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
        let mut entries = Vec::new();
        let mut all = String::new();
        for e in ar.entries().unwrap() {
            let mut e = e.unwrap();
            entries.push(e.path().unwrap().to_string_lossy().into_owned());
            let mut s = String::new();
            let _ = io::Read::read_to_string(&mut e, &mut s);
            all.push_str(&s);
        }
        let top = name.trim_end_matches(".tar.gz");
        for f in [
            "system-report.txt",
            "system-report.json",
            "grubenv.txt",
            "rootfs_a.xoros.env",
            "rootfs_b.xoros.env",
            "partitions.txt",
            "dmesg.txt",
            "journal.txt",
            "rescue-shell.log",
            "network.txt",
            "network-config.json",
            "test-results/memtest.txt",
        ] {
            assert!(
                entries.contains(&format!("{top}/{f}")),
                "{f} missing: {entries:?}"
            );
        }
        assert!(!all.contains("TopSecretPass"));
        assert!(all.contains("memtest: PASS"));
        assert!(!d
            .path()
            .join("sticks/usb1")
            .join(format!("{name}.part"))
            .exists());
    }
}
