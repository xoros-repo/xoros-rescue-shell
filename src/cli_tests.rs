//! CLI for the tests added after M3 (ARCH-2): reboot into memtest86+ (TST-2), disk health
//! (TST-7), destructive stick test (TST-8), CPU stress (TST-9) and the network speed test
//! (NET-4 P2); called from [`crate::cli`].

use crate::app::App;
use crate::hwtest::memtest86;

pub const USAGE: &str = "\
  xoros-rescue-shell [--sim <dir>] --cli test memtest86 [--reboot]
";

/// `--cli test <args>` commands that are not a [`crate::hwtest::TestRequest`]; `None` for the
/// others. Returns the exit code.
pub fn run_test_command(app: &App, args: &[String]) -> Option<i32> {
    let rest = args.get(1..).unwrap_or(&[]);
    let r = match args.first().map(String::as_str)? {
        "memtest86" => memtest86_cmd(app, rest),
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
}
