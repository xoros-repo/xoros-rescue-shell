//! Non-interactive CLI (ARCH-2) for boot control (BOOT-*, ROT-1), power (PWR-*), settings
//! (CFG-*), the hardware clock (NET-7), sticks (USB-5) and partition maintenance (BAK-6/7).

use std::sync::Arc;

use crate::app::App;
use crate::backend::{BootTarget, PartRole, Slot, Stick};
use crate::boot as ctl;
use crate::power::{self, PowerOutcome, PowerPlan};
use crate::settings::{self, Settings};
use crate::tasks::{TaskCtx, TaskFailure, TaskKind, TaskState};

pub const USAGE: &str =
    "  xoros-rescue-shell [--sim <dir>] --cli boot show [--json] | reset-counter A|B|both
                     | next A|B|rescue|clear | order A|B | max-tries N | disable A|B
                     | rotation 0..3 | recreate-grubenv --confirm RECREATE
  xoros-rescue-shell [--sim <dir>] --cli power reboot | poweroff
                     | reboot-into A|B|rescue [--reset-counter] [--force]
  xoros-rescue-shell [--sim <dir>] --cli settings show [--json] | get <key> | set <key> <value>...
                     | reset --confirm RESET | set-pin (PIN on stdin) | clear-pin
  xoros-rescue-shell [--sim <dir>] --cli time rtc [show|write]
  xoros-rescue-shell [--sim <dir>] --cli stick list | remove <stick>
  xoros-rescue-shell [--sim <dir>] --cli backup repair overlay|application
  xoros-rescue-shell [--sim <dir>] --cli backup mkfs overlay|application --confirm MKFS
  xoros-rescue-shell [--sim <dir>] --cli backup image <stick> <partition label>
";

/// Word to type before re-creating grubenv from the CLI (ENV-2).
pub const RECREATE_GRUBENV_WORD: &str = "RECREATE";

fn print_result(r: Result<String, String>) -> i32 {
    match r {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// Words after the subcommand: exactly `n` positional words and only the given flags/values.
fn expect_args(
    args: &[String],
    n: usize,
    values: &[&str],
    flags: &[&str],
) -> Result<Vec<String>, String> {
    let mut pos = Vec::new();
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if values.contains(&a.as_str()) {
            if it.next().is_none() {
                return Err(format!("{a} needs a value."));
            }
        } else if flags.contains(&a.as_str()) {
        } else if a.starts_with("--") {
            return Err(format!("Unknown argument '{a}'."));
        } else {
            pos.push(a.clone());
        }
    }
    if pos.len() != n {
        return Err(format!(
            "{} needs {n} argument(s), got {}.",
            args.first().map(String::as_str).unwrap_or("command"),
            pos.len()
        ));
    }
    Ok(pos)
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn slots_arg(w: &str) -> Result<Vec<Slot>, String> {
    if w.eq_ignore_ascii_case("both") || w.eq_ignore_ascii_case("ab") {
        return Ok(Slot::ALL.to_vec());
    }
    w.parse::<Slot>()
        .map(|s| vec![s])
        .map_err(|e| e.to_string())
}

fn persistent_arg(w: &str) -> Result<PartRole, String> {
    match PartRole::from_label(w) {
        Some(r @ (PartRole::Overlay | PartRole::Application)) => Ok(r),
        _ => Err(format!("'{w}' is not overlay or application.")),
    }
}

// --------------------------------------------------------------------------------------------
// boot

fn boot_text(app: &App) -> Result<String, String> {
    let b = app.backend.as_ref();
    let st = b.boot_state().map_err(|e| e.to_string())?;
    let predicted = b
        .predict()
        .map(|t| t.name().to_string())
        .unwrap_or_else(|_| "unknown".into());
    Ok(format!(
        "Predicted next boot: {predicted}\n{}",
        crate::report::boot_vars_text(&st)
    ))
}

pub fn boot_cmd(app: &App, args: &[String]) -> Result<String, String> {
    let b = app.backend.as_ref();
    let e = |r: crate::backend::Result<()>| r.map_err(|e| e.to_string());
    match args.first().map(String::as_str) {
        Some("show") | None => {
            expect_args(args, 0, &[], &["--json"])?;
            if flag(args, "--json") {
                let st = b.boot_state().map_err(|e| e.to_string())?;
                let v = serde_json::json!({
                    "boot": st,
                    "predicted_next_boot": b.predict().ok().map(|t| t.name()),
                    "current_boot": b.current_boot().ok().map(|t| t.name()),
                });
                return serde_json::to_string_pretty(&v)
                    .map(|s| s + "\n")
                    .map_err(|e| e.to_string());
            }
            boot_text(app)
        }
        Some("reset-counter") => {
            let p = expect_args(args, 1, &[], &[])?;
            let slots = slots_arg(&p[0])?;
            e(ctl::reset_tries(b, &slots))?;
            boot_text(app)
        }
        Some("next") => {
            let p = expect_args(args, 1, &[], &[])?;
            let target = match p[0].to_ascii_lowercase().as_str() {
                "clear" | "none" => None,
                other => Some(other.parse::<BootTarget>().map_err(|e| e.to_string())?),
            };
            if let Some(BootTarget::Slot(s)) = target {
                if let Some(w) = ctl::slot_boot_warning(b, s) {
                    eprintln!("warning: {w}");
                }
            }
            e(ctl::set_next_boot(b, target))?;
            boot_text(app)
        }
        Some("order") => {
            let p = expect_args(args, 1, &[], &[])?;
            let first = p[0].parse::<Slot>().map_err(|e| e.to_string())?;
            e(ctl::set_order(b, first))?;
            boot_text(app)
        }
        Some("max-tries") => {
            let p = expect_args(args, 1, &[], &[])?;
            let n: u8 = p[0]
                .parse()
                .ok()
                .filter(|n| crate::constants::MAX_TRIES_RANGE.contains(n))
                .ok_or_else(|| {
                    let r = crate::constants::MAX_TRIES_RANGE;
                    format!("MAX_TRIES must be {}..{}.", r.start(), r.end())
                })?;
            e(ctl::set_max_tries(b, n))?;
            boot_text(app)
        }
        Some("disable") => {
            let p = expect_args(args, 1, &[], &[])?;
            let s = p[0].parse::<Slot>().map_err(|e| e.to_string())?;
            e(ctl::disable(b, s))?;
            boot_text(app)
        }
        Some("rotation") => {
            let p = expect_args(args, 1, &[], &[])?;
            let r: u8 = p[0]
                .parse()
                .ok()
                .filter(|r| *r <= 3)
                .ok_or("ROTATION must be 0..3 (0/90/180/270 degrees).")?;
            let note = ctl::set_rotation(b, r).map_err(|e| e.to_string())?;
            Ok(format!("ROTATION={r}. {note}\n"))
        }
        Some("recreate-grubenv") => {
            expect_args(args, 0, &["--confirm"], &[])?;
            if opt(args, "--confirm").as_deref() != Some(RECREATE_GRUBENV_WORD) {
                return Err(format!(
                    "Recreating grubenv resets all boot variables: add --confirm {RECREATE_GRUBENV_WORD}."
                ));
            }
            e(ctl::recreate_grubenv(b))?;
            boot_text(app)
        }
        Some(other) => Err(format!(
            "Unknown boot command '{other}' (show, reset-counter, next, order, max-tries, \
             disable, rotation, recreate-grubenv)."
        )),
    }
}

// --------------------------------------------------------------------------------------------
// power

/// The plan of `--cli power ...`.
pub fn power_plan(app: &App, args: &[String]) -> Result<PowerPlan, String> {
    match args.first().map(String::as_str) {
        Some("reboot") => {
            expect_args(args, 0, &[], &[])?;
            Ok(PowerPlan::reboot())
        }
        Some("poweroff" | "power-off") => {
            expect_args(args, 0, &[], &[])?;
            Ok(PowerPlan::power_off())
        }
        Some("reboot-into") => {
            let p = expect_args(args, 1, &[], &["--reset-counter", "--force"])?;
            let target = p[0].parse::<BootTarget>().map_err(|e| e.to_string())?;
            if let BootTarget::Slot(s) = target {
                if let Some(w) = ctl::slot_boot_warning(app.backend.as_ref(), s) {
                    if !flag(args, "--force") {
                        return Err(format!(
                            "{w}\nAdd --force to reboot into it anyway (BOOT-6)."
                        ));
                    }
                    eprintln!("warning: {w}");
                }
            }
            let reset = flag(args, "--reset-counter") && matches!(target, BootTarget::Slot(_));
            Ok(PowerPlan::reboot_into(target, reset))
        }
        _ => Err("power needs: reboot | poweroff | reboot-into A|B|rescue.".into()),
    }
}

pub fn power_cmd(app: &App, args: &[String]) -> Result<String, String> {
    let plan = power_plan(app, args)?;
    match power::execute(plan, app.backend.as_ref(), &app.tasks).map_err(|e| e.to_string())? {
        PowerOutcome::Simulated(m) => Ok(format!("{m}\n")),
        PowerOutcome::Started => Ok(format!("{} started.\n", plan.describe())),
    }
}

// --------------------------------------------------------------------------------------------
// settings

fn settings_text(st: &settings::State) -> String {
    let s = &st.settings;
    let mut t = format!("# {}\n", st.origin_text());
    for k in &settings::KEYS[..6] {
        t.push_str(&format!("{k} = {}\n", s.get(k).unwrap_or_default()));
    }
    t.push_str(&format!("pin = {}\n", s.get("pin").unwrap_or_default()));
    if !st.system_keys.is_empty() {
        t.push_str(&format!(
            "# {} trusted key(s) from {}\n",
            st.system_keys.len(),
            crate::constants::OTA_TRUSTED_KEYS_FILE
        ));
    }
    for w in &st.warnings {
        t.push_str(&format!("# warning: {w}\n"));
    }
    t
}

fn read_stdin_line() -> Result<String, String> {
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("Cannot read the PIN from stdin: {e}"))?;
    Ok(line.trim().to_string())
}

pub fn settings_cmd(app: &App, args: &[String]) -> Result<String, String> {
    let b = app.backend.as_ref();
    let st = settings::state();
    let save = |s: &Settings| settings::save(b, s);
    match args.first().map(String::as_str) {
        Some("show") | None => {
            expect_args(args, 0, &[], &["--json"])?;
            if flag(args, "--json") {
                return serde_json::to_string_pretty(&st)
                    .map(|s| s + "\n")
                    .map_err(|e| e.to_string());
            }
            Ok(settings_text(&st))
        }
        Some("get") => {
            let p = expect_args(args, 1, &[], &[])?;
            st.settings
                .get(&p[0])
                .map(|v| v + "\n")
                .ok_or_else(|| format!("Unknown setting '{}'.", p[0]))
        }
        Some("set") => {
            // Lists may be given as several words: set ntp_servers a.example b.example
            let key = args.get(1).ok_or("set needs a key and a value.")?;
            if args.iter().skip(2).any(|a| a.starts_with("--")) {
                return Err("set: options are not allowed after the key.".into());
            }
            let value = args[2..].join(" ");
            let mut s = st.settings.clone();
            s.set(key, &value)?;
            save(&s)?;
            Ok(format!("{key} = {}\n", s.get(key).unwrap_or_default()))
        }
        Some("reset") => {
            expect_args(args, 0, &["--confirm"], &[])?;
            if opt(args, "--confirm").as_deref() != Some(crate::backup::RESET_CONFIRM_WORD) {
                return Err(format!(
                    "Resetting the settings needs --confirm {}.",
                    crate::backup::RESET_CONFIRM_WORD
                ));
            }
            save(&Settings::default())?;
            Ok("Settings reset to the defaults.\n".into())
        }
        Some("set-pin") => {
            expect_args(args, 0, &[], &[])?;
            let pin = read_stdin_line()?;
            let mut s = st.settings.clone();
            s.set_pin(&pin)?;
            save(&s)?;
            Ok("PIN set: it is asked before destructive actions and the subshell.\n".into())
        }
        Some("clear-pin") => {
            expect_args(args, 0, &[], &[])?;
            let mut s = st.settings.clone();
            s.clear_pin();
            save(&s)?;
            Ok("PIN removed.\n".into())
        }
        Some(other) => Err(format!(
            "Unknown settings command '{other}' (show, get, set, reset, set-pin, clear-pin)."
        )),
    }
}

// --------------------------------------------------------------------------------------------
// time rtc, sticks

/// `time rtc ...`; `None` for other `time` subcommands.
pub fn time_cmd(app: &App, args: &[String]) -> Option<Result<String, String>> {
    if args.first().map(String::as_str) != Some("rtc") {
        return None;
    }
    let dev = std::path::Path::new(crate::constants::RTC_DEVICE);
    Some(match args.get(1).map(String::as_str) {
        Some("show") | None if args.len() <= 2 => {
            if !dev.exists() {
                Err(format!(
                    "No hardware clock ({} does not exist).",
                    dev.display()
                ))
            } else {
                crate::rtc::read(dev)
                    .map(|t| format!("{} ({})\n", crate::net::time::format_utc(t), dev.display()))
            }
        }
        Some("write") if args.len() == 2 => {
            let now = std::time::SystemTime::now();
            if !crate::net::time::clock_plausible() {
                return Some(Err(
                    "The system clock is before the build date: sync or set it first.".into(),
                ));
            }
            match crate::rtc::store(app.backend.is_simulation(), dev, now) {
                Some(n) => Ok(format!("{n}\n")),
                None => Err(format!(
                    "No hardware clock ({} does not exist).",
                    dev.display()
                )),
            }
        }
        _ => Err("time rtc needs: show | write.".into()),
    })
}

fn find_stick(app: &App, id: &str) -> Result<Stick, String> {
    app.backend
        .external_sticks()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("No stick '{id}' (see 'stick list')."))
}

pub fn stick_cmd(app: &App, args: &[String]) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("list") | None => {
            expect_args(args, 0, &[], &[])?;
            let sticks = app.backend.external_sticks().map_err(|e| e.to_string())?;
            Ok(sticks
                .iter()
                .map(|s| format!("{}\t{}\n", s.id, s.description))
                .collect())
        }
        Some("remove") => {
            let p = expect_args(args, 1, &[], &[])?;
            let stick = find_stick(app, &p[0])?;
            crate::sticks::safe_remove(app.backend.as_ref(), &stick).map(|t| t + "\n")
        }
        Some(other) => Err(format!("Unknown stick command '{other}' (list, remove).")),
    }
}

// --------------------------------------------------------------------------------------------
// backup repair / mkfs / image

/// Run `body` in a task of `kind`, waiting for it (progress goes to stderr).
fn run_task<T: Send + 'static>(
    app: &App,
    name: &str,
    kind: TaskKind,
    body: impl FnOnce(&TaskCtx) -> Result<T, TaskFailure> + Send + 'static,
) -> Result<T, String> {
    let out = Arc::new(std::sync::Mutex::new(None));
    let o = Arc::clone(&out);
    let h = app
        .tasks
        .spawn(
            name,
            kind,
            move |ctx| {
                *crate::tasks::lock(&o) = Some(body(ctx)?);
                Ok(String::new())
            },
            None,
        )
        .map_err(|e| e.to_string())?;
    let mut last = String::new();
    let snap = loop {
        let snap = h.snapshot();
        if snap.is_finished() {
            break snap;
        }
        let pct = snap
            .fraction()
            .map(|f| format!(" {:>3}%", (f * 100.0) as u32))
            .unwrap_or_default();
        let line = format!("{}{pct}", snap.message);
        if line != last && !snap.message.is_empty() {
            eprintln!("{line}");
            last = line;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    match snap.state {
        TaskState::Succeeded(_) => crate::tasks::lock(&out)
            .take()
            .ok_or_else(|| "no result".into()),
        TaskState::Cancelled => Err("Cancelled.".into()),
        TaskState::Failed(e) => Err(e),
        TaskState::Running => Err("still running".into()),
    }
}

/// `backup repair|mkfs|image ...`; `None` for other `backup` subcommands.
pub fn backup_cmd(app: &App, args: &[String]) -> Option<Result<String, String>> {
    let backend = Arc::clone(&app.backend);
    Some(match args.first().map(String::as_str)? {
        "repair" => (|| {
            let p = expect_args(args, 1, &[], &[])?;
            let role = persistent_arg(&p[0])?;
            let r = run_task(app, "File system repair", TaskKind::Write, move |ctx| {
                crate::partition_jobs::repair(backend.as_ref(), role, ctx)
            })?;
            if r.mountable() {
                Ok(r.text())
            } else {
                Err(format!(
                    "{}Re-create it with: --cli backup mkfs {} --confirm {}",
                    r.text(),
                    role.label(),
                    crate::partition_jobs::MKFS_CONFIRM_WORD
                ))
            }
        })(),
        "mkfs" => (|| {
            let p = expect_args(args, 1, &["--confirm"], &[])?;
            let role = persistent_arg(&p[0])?;
            let confirmation = opt(args, "--confirm").unwrap_or_default();
            run_task(app, "Re-create file system", TaskKind::Write, move |ctx| {
                crate::partition_jobs::recreate_fs(backend.as_ref(), role, &confirmation, ctx)
            })
            .map(|t| t + "\n")
        })(),
        "image" => (|| {
            let p = expect_args(args, 2, &[], &[])?;
            let stick = find_stick(app, &p[0])?;
            let role = PartRole::from_label(&p[1])
                .ok_or_else(|| format!("Unknown partition '{}'.", p[1]))?;
            let st = stick.clone();
            run_task(app, "Raw partition image", TaskKind::ReadOnly, move |ctx| {
                crate::partition_jobs::export_image(backend.as_ref(), role, &st, ctx)
            })
            .map(|r| r.text(&stick) + "\n")
        })(),
        _ => return None,
    })
}

pub fn run_boot(app: &App, args: &[String]) -> i32 {
    print_result(boot_cmd(app, args))
}

pub fn run_power(app: &App, args: &[String]) -> i32 {
    print_result(power_cmd(app, args))
}

pub fn run_settings(app: &App, args: &[String]) -> i32 {
    print_result(settings_cmd(app, args))
}

pub fn run_stick(app: &App, args: &[String]) -> i32 {
    print_result(stick_cmd(app, args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sim::SimBackend;
    use crate::hw::testutil::TempDir;

    fn app() -> (TempDir, App) {
        let d = TempDir::new("cli-admin");
        SimBackend::create_example(d.path()).expect("example");
        let app =
            App::new(crate::app::make_backend(Some(d.path().to_path_buf())).expect("backend"));
        (d, app)
    }

    fn w(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn boot_commands() {
        let (_d, app) = app();
        assert!(boot_cmd(&app, &w(&["show"]))
            .unwrap()
            .contains("ROOTFS_A_TRY"));
        assert!(boot_cmd(&app, &w(&["show", "--json"]))
            .unwrap()
            .contains("predicted_next_boot"));
        boot_cmd(&app, &w(&["reset-counter", "a"])).unwrap();
        assert_eq!(app.backend.boot_state().unwrap().tries, [0, 3]);
        boot_cmd(&app, &w(&["reset-counter", "both"])).unwrap();
        boot_cmd(&app, &w(&["next", "rootfs_b"])).unwrap();
        assert_eq!(
            app.backend.boot_state().unwrap().next_boot,
            Some(BootTarget::Slot(Slot::B))
        );
        boot_cmd(&app, &w(&["next", "clear"])).unwrap();
        assert_eq!(app.backend.boot_state().unwrap().next_boot, None);
        boot_cmd(&app, &w(&["order", "B"])).unwrap();
        boot_cmd(&app, &w(&["max-tries", "5"])).unwrap();
        boot_cmd(&app, &w(&["disable", "A"])).unwrap();
        let st = app.backend.boot_state().unwrap();
        assert_eq!(
            (st.order[0], st.max_tries, st.try_count(Slot::A)),
            (Slot::B, 5, 5)
        );
        assert!(boot_cmd(&app, &w(&["rotation", "2"]))
            .unwrap()
            .contains("Simulation"));
        assert_eq!(app.backend.boot_state().unwrap().rotation, 2);
        for bad in [
            &["max-tries", "0"][..],
            &["max-tries", "21"],
            &["rotation", "4"],
            &["next", "c"],
            &["order"],
            &["reset-counter", "a", "b"],
            &["show", "--bogus"],
            &["recreate-grubenv"],
            &["frobnicate"],
        ] {
            assert!(boot_cmd(&app, &w(bad)).is_err(), "{bad:?}");
        }
        boot_cmd(
            &app,
            &w(&["recreate-grubenv", "--confirm", RECREATE_GRUBENV_WORD]),
        )
        .unwrap();
    }

    #[test]
    fn power_plans() {
        let (d, app) = app();
        assert_eq!(power_plan(&app, &w(&["reboot"])), Ok(PowerPlan::reboot()));
        assert_eq!(
            power_plan(&app, &w(&["poweroff"])),
            Ok(PowerPlan::power_off())
        );
        assert_eq!(
            power_plan(&app, &w(&["reboot-into", "b", "--reset-counter"])),
            Ok(PowerPlan::reboot_into(BootTarget::Slot(Slot::B), true))
        );
        assert_eq!(
            power_plan(&app, &w(&["reboot-into", "rescue", "--reset-counter"])),
            Ok(PowerPlan::reboot_into(BootTarget::Rescue, false))
        );
        assert!(power_plan(&app, &w(&["reboot", "now"])).is_err());
        assert!(power_plan(&app, &w(&["halt"])).is_err());
        // BOOT-6: an empty slot needs --force.
        let b = SimBackend::open(d.path()).expect("open");
        let mut s = b.load().expect("load");
        s.slots.rootfs_b = None;
        d.write(
            "sim.json",
            serde_json::to_string(&s).expect("json").as_bytes(),
        );
        assert!(power_plan(&app, &w(&["reboot-into", "B"])).is_err());
        assert!(power_plan(&app, &w(&["reboot-into", "B", "--force"])).is_ok());
        let out = power_cmd(&app, &w(&["reboot-into", "A", "--reset-counter"])).unwrap();
        assert!(out.contains("SIM"), "{out}");
        assert_eq!(app.backend.boot_state().unwrap().try_count(Slot::A), 0);
    }

    #[test]
    fn stick_and_backup_extras() {
        let (_d, app) = app();
        let list = stick_cmd(&app, &w(&["list"])).unwrap();
        let id = list.split('\t').next().unwrap().to_string();
        assert!(stick_cmd(&app, &w(&["remove", &id]))
            .unwrap()
            .contains("can be removed"));
        assert!(stick_cmd(&app, &w(&["remove", "nope"])).is_err());
        assert!(backup_cmd(&app, &w(&["export"])).is_none());
        assert!(backup_cmd(&app, &w(&["repair", "overlay"]))
            .unwrap()
            .is_ok());
        assert!(backup_cmd(&app, &w(&["repair", "rescue"]))
            .unwrap()
            .is_err());
        assert!(backup_cmd(&app, &w(&["mkfs", "overlay"])).unwrap().is_err());
        assert!(
            backup_cmd(&app, &w(&["mkfs", "overlay", "--confirm", "MKFS"]))
                .unwrap()
                .is_ok()
        );
        let r = backup_cmd(&app, &w(&["image", &id, "rescue"]))
            .unwrap()
            .unwrap();
        assert!(r.contains("xoros-image-rescue-"), "{r}");
        assert!(time_cmd(&app, &w(&["show"])).is_none());
        assert!(time_cmd(&app, &w(&["rtc", "bogus"])).unwrap().is_err());
    }

    #[test]
    fn settings_text_and_args() {
        let st = settings::State::default();
        let t = settings_text(&st);
        assert!(t.contains("ota_base_url = https://"));
        assert!(t.contains("pin = off"));
        let (_d, app) = app();
        assert!(settings_cmd(&app, &w(&["get", "theme"])).is_ok());
        assert!(settings_cmd(&app, &w(&["get", "nope"])).is_err());
        assert!(settings_cmd(&app, &w(&["set", "theme", "neon"])).is_err());
        assert!(settings_cmd(&app, &w(&["reset"])).is_err());
        assert!(settings_cmd(&app, &w(&["show", "--json"]))
            .unwrap()
            .contains("ntp_servers"));
    }
}
