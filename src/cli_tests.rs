//! CLI for the tests added after M3 (ARCH-2): reboot into memtest86+ (TST-2), disk health
//! (TST-7), destructive stick test (TST-8), CPU stress (TST-9) and the network speed test
//! (NET-4 P2); called from [`crate::cli`].

use crate::app::App;
use crate::cli::check_test_args;
use crate::hwtest::{memtest86, sticktest, stress, TestRequest};

pub const USAGE: &str = "\
\x20 xoros-rescue-shell [--sim <dir>] --cli test memtest86 [--reboot]
  xoros-rescue-shell [--sim <dir>] --cli test health [<disk>...]
  xoros-rescue-shell [--sim <dir>] --cli test stick [<disk> --confirm ERASE]
                     (without a disk: list the disks that can be tested)
  xoros-rescue-shell [--sim <dir>] --cli test stress [--minutes N]
  xoros-rescue-shell [--sim <dir>] --cli net speed [--base <url>] [--proxy <url>]
";

/// Words of `args` that are not options.
fn positional(args: &[String]) -> Vec<String> {
    positional_values(args, &[])
}

/// Words of `args` that are neither options nor the values of the options in `values`.
fn positional_values(args: &[String], values: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
        } else if a.starts_with('-') {
            skip = values.contains(&a.as_str());
        } else {
            out.push(a.clone());
        }
    }
    out
}

fn opt_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// `--cli test <word> <args>` for the tests of this module; `None` for other words.
pub fn test_request(app: &App, word: &str, args: &[String]) -> Option<Result<TestRequest, String>> {
    let _ = app;
    Some(match word {
        "health" => {
            check_test_args(args, &[], &[], true).map(|()| TestRequest::Health(positional(args)))
        }
        "stick" => (|| {
            check_test_args(args, &["--confirm"], &[], true)?;
            let pos = positional_values(args, &["--confirm"]);
            let [disk] = pos.as_slice() else {
                return Err("test stick: give one disk (see 'test stick').".to_string());
            };
            let confirmation = opt_value(args, "--confirm").unwrap_or_default();
            if confirmation != sticktest::CONFIRM_WORD {
                return Err(format!(
                    "This destroys everything on {disk}: add --confirm {} (UI-4).",
                    sticktest::CONFIRM_WORD
                ));
            }
            Ok(TestRequest::Stick(sticktest::StickTestRequest {
                disk: disk.clone(),
                confirmation,
            }))
        })(),
        "stress" => (|| {
            check_test_args(args, &["--minutes"], &[], false)?;
            let minutes = match opt_value(args, "--minutes") {
                Some(v) => v
                    .parse::<u32>()
                    .ok()
                    .filter(|m| stress::MINUTES_RANGE.contains(m))
                    .ok_or_else(|| format!("--minutes must be 1..120, not '{v}'."))?,
                None => stress::DEFAULT_MINUTES,
            };
            Ok(TestRequest::Stress(stress::StressOptions {
                duration: std::time::Duration::from_secs(u64::from(minutes) * 60),
            }))
        })(),
        _ => return None,
    })
}

/// `--cli test <args>` commands that are not a [`crate::hwtest::TestRequest`]; `None` for the
/// others. Returns the exit code.
pub fn run_test_command(app: &App, args: &[String]) -> Option<i32> {
    let rest = args.get(1..).unwrap_or(&[]);
    let r = match args.first().map(String::as_str)? {
        "memtest86" => memtest86_cmd(app, rest),
        "stick" if rest.is_empty() => sticktest::candidates(app.backend.as_ref()).map(|v| {
            if v.is_empty() {
                "No disk other than the boot disk found.\n".to_string()
            } else {
                v.iter().map(|d| sticktest::label(d) + "\n").collect()
            }
        }),
        _ => return None,
    };
    Some(match r {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    })
}

/// `test memtest86 [--reboot]` (TST-2): exit code 0 if the entry exists.
fn memtest86_cmd(app: &App, args: &[String]) -> Result<String, String> {
    let reboot = match args {
        [] => false,
        [a] if a == "--reboot" => true,
        [a, ..] => return Err(format!("Unknown argument '{a}'.")),
    };
    let backend = app.backend.as_ref();
    if !memtest86::available(backend)? {
        return Err(crate::texts::MEMTEST86_MISSING.into());
    }
    if !reboot {
        return Ok(format!(
            "memtest86+ boot entry {} found; reboot into it with --reboot.\n",
            memtest86::MENU_MEMTEST
        ));
    }
    if let Some(t) = app.tasks.write_running() {
        return Err(format!("'{}' is still running.", t.name()));
    }
    memtest86::set_next_boot(backend)?;
    match crate::power::execute(crate::power::PowerPlan::reboot(), backend, &app.tasks) {
        Ok(crate::power::PowerOutcome::Simulated(m)) => Ok(format!("{m}\n")),
        Ok(_) => Ok("Rebooting into memtest86+...\n".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// `net speed [--base <url>] [--proxy <url>]` (NET-4 P2): download one OTA chunk; the result
/// is recorded with the test results. Exit code 1 if it failed.
pub fn net_speed(app: &App, args: &[String]) -> Result<String, String> {
    use crate::net::speed::SpeedOptions;
    check_test_args(args, &["--base", "--proxy"], &[], false)?;
    let mut opts = SpeedOptions::default();
    if let Some(b) = opt_value(args, "--base") {
        crate::net::http::check_base_url(&b)?;
        opts.base_url = b;
    }
    if let Some(p) = opt_value(args, "--proxy") {
        opts.proxy = Some(p).filter(|p| !p.is_empty());
    }
    let req = TestRequest::NetSpeed(opts);
    let env = crate::hwtest::TestEnv::of(app);
    let (kind, title) = (req.task_kind(), req.kind().title());
    let r = crate::cli::run_cli_task(app, title, kind, move |ctx| {
        Ok(crate::hwtest::run_request(&env, &req, ctx))
    })?;
    if r.outcome.exit_code() == 0 {
        Ok(r.text())
    } else {
        Err(r.text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::make_backend;
    use crate::backend::sim::SimBackend;
    use crate::hw::testutil::TempDir;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn memtest86_in_simulation() {
        let d = TempDir::new("cli-memtest86");
        SimBackend::create_example(d.path()).expect("example");
        let app = App::new(make_backend(Some(d.path().to_path_buf())).expect("backend"));
        assert!(memtest86_cmd(&app, &[]).is_err());
        d.write(
            "boot/EFI/BOOT/grub.cfg",
            b"menuentry memtest --id=MENU_MEMTEST {\n}\n",
        );
        assert!(memtest86_cmd(&app, &a(&["--bogus"])).is_err());
        assert!(memtest86_cmd(&app, &[]).unwrap().contains("found"));
        assert!(memtest86_cmd(&app, &a(&["--reboot"]))
            .unwrap()
            .contains("SIM: would run 'systemctl reboot'"));
        assert_eq!(run_test_command(&app, &a(&["memory"])), None);
    }

    #[test]
    fn test_requests() {
        let d = TempDir::new("cli-tests-req");
        SimBackend::create_example(d.path()).expect("example");
        let app = App::new(make_backend(Some(d.path().to_path_buf())).expect("backend"));
        let req = |w: &str, v: &[&str]| test_request(&app, w, &a(v));
        assert!(req("memory", &[]).is_none());
        assert_eq!(req("health", &[]), Some(Ok(TestRequest::Health(vec![]))));
        assert_eq!(
            req("health", &["sda", "sdb"]),
            Some(Ok(TestRequest::Health(a(&["sda", "sdb"]))))
        );
        assert!(matches!(req("health", &["--all"]), Some(Err(_))));
        assert!(matches!(req("stick", &["sdb"]), Some(Err(_))));
        assert!(matches!(
            req("stick", &["sdb", "--confirm", "yes"]),
            Some(Err(_))
        ));
        assert!(matches!(
            req("stick", &["sdb", "sdc", "--confirm", "ERASE"]),
            Some(Err(_))
        ));
        assert_eq!(
            req("stick", &["sdb", "--confirm", "ERASE"]),
            Some(Ok(TestRequest::Stick(sticktest::StickTestRequest {
                disk: "sdb".into(),
                confirmation: "ERASE".into()
            })))
        );
        assert_eq!(
            req("stress", &["--minutes", "2"]),
            Some(Ok(TestRequest::Stress(stress::StressOptions {
                duration: std::time::Duration::from_secs(120)
            })))
        );
        assert!(matches!(req("stress", &["--minutes", "0"]), Some(Err(_))));
        assert!(matches!(req("stress", &["5"]), Some(Err(_))));
        assert!(net_speed(&app, &a(&["--bsae", "http://x"])).is_err());
        assert!(net_speed(&app, &a(&["--base", "ftp://x"])).is_err());
        let list = sticktest::candidates(app.backend.as_ref()).expect("list");
        assert_eq!(list.len(), 1);
        assert!(sticktest::label(&list[0]).starts_with("sdb "));
    }
}
