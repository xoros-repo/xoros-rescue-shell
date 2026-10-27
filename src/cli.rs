//! Command line parsing and the non-interactive CLI (ARCH-2): status, backup, update, net,
//! time.

use std::path::PathBuf;

use crate::app::App;
use crate::constants::{APP_NAME, APP_VERSION, SIM_ENV_VAR};
use std::sync::{Arc, Mutex};

use crate::backend::Stick;
use crate::backup::{ImportMode, PartitionRole};
use crate::backup_jobs;
use crate::constants::{DEFAULT_HTTP_PROXY, DEFAULT_OTA_BASE_URL, WRITE_BOTH_CONFIRM_WORD};
use crate::net;
use crate::slots::RescueCheck;
use crate::update_jobs;
use xoros_core::env::Suffix;
use xoros_core::manifest::Target;
use crate::tasks::{lock, TaskCtx, TaskFailure, TaskKind, TaskState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Start the TUI (default).
    Tui,
    /// `--cli status [--json] [--no-hash]`.
    Status {
        json: bool,
        hash: bool,
    },
    /// `--cli backup <args>` (ARCH-2, BAK-*, RST-*).
    Backup(Vec<String>),
    /// `--cli update <args>` (OTA-*, USB-*).
    Update(Vec<String>),
    /// `--cli net <args>` (NET-1, NET-3, NET-4).
    Net(Vec<String>),
    /// `--cli time <args>` (NET-7).
    Time(Vec<String>),
    /// `--sim-init <dir>`: write an example simulation directory.
    SimInit(PathBuf),
    Help,
    Version,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub sim: Option<PathBuf>,
    pub command: Command,
}

pub fn usage() -> String {
    format!(
        "{APP_NAME} {APP_VERSION}

Usage:
  xoros-rescue-shell [--sim <dir>]                  start the TUI
  xoros-rescue-shell [--sim <dir>] --cli status [--json] [--no-hash]
  xoros-rescue-shell [--sim <dir>] --cli backup sticks
  xoros-rescue-shell [--sim <dir>] --cli backup list <stick>
  xoros-rescue-shell [--sim <dir>] --cli backup export <stick> [overlay] [application]
  xoros-rescue-shell [--sim <dir>] --cli backup verify <stick> <name>
  xoros-rescue-shell [--sim <dir>] --cli backup import <stick> <name> [--merge] [overlay] [application]
  xoros-rescue-shell [--sim <dir>] --cli backup reset config|full --confirm RESET [--reset-rescue]
  xoros-rescue-shell [--sim <dir>] --cli update check [<source>] [--no-hash]
  xoros-rescue-shell [--sim <dir>] --cli update scan <stick>
  xoros-rescue-shell [--sim <dir>] --cli update start --slot A|B|AB [<source>] [--image <timestamp>]
                     [--allow-downgrade] [--allow-unverified] [--no-resume] [--confirm WRITE]
  xoros-rescue-shell [--sim <dir>] --cli net up | dns | test [--base <url>] [--proxy <url>]
  xoros-rescue-shell [--sim <dir>] --cli time show | sync [<server>...] | set 'YYYY-MM-DD HH:MM[:SS]'
  xoros-rescue-shell --sim-init <dir>               create an example simulation dir
  xoros-rescue-shell --help | --version

Options:
  --sim <dir>   simulation mode: use <dir>/sim.json instead of the device
                (also env {SIM_ENV_VAR}=<dir>)
  --json        machine readable output
  --no-hash     do not compute the rescue image SHA-256

Update sources (default: the OTA server for the newest valid slot):
  --base <url> [--proxy <url>] [--version <v>] [--machine <m>] [--suffix <s>]
  --stick <id> --path <dir or image on the stick>   (see 'update scan')

Exit codes:
  0  success
  1  error: the operation failed or completed only partly
  2  the command line could not be parsed
"
    )
}

/// Parse arguments (without the program name).
pub fn parse_args<I, S>(args: I) -> Result<Options, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut it = args.into_iter().map(Into::into);
    let mut sim = None;
    let mut command = Command::Tui;
    let mut cli: Option<Vec<String>> = None;
    while let Some(a) = it.next() {
        if let Some(rest) = cli.as_mut() {
            rest.push(a);
            continue;
        }
        match a.as_str() {
            "--sim" => {
                let d = it.next().ok_or("--sim needs a directory")?;
                sim = Some(PathBuf::from(d));
            }
            "--sim-init" => {
                let d = it.next().ok_or("--sim-init needs a directory")?;
                command = Command::SimInit(PathBuf::from(d));
            }
            "--cli" => cli = Some(Vec::new()),
            "-h" | "--help" => command = Command::Help,
            "-V" | "--version" => command = Command::Version,
            other => {
                if let Some(d) = other.strip_prefix("--sim=") {
                    sim = Some(PathBuf::from(d));
                } else {
                    return Err(format!("Unknown argument '{other}'."));
                }
            }
        }
    }
    if let Some(rest) = cli {
        let mut words = rest.iter().map(String::as_str);
        match words.next() {
            Some("status") => {
                let (mut json, mut hash) = (false, true);
                for w in words {
                    match w {
                        "--json" => json = true,
                        "--no-hash" => hash = false,
                        "--sim" => return Err("Put --sim before --cli.".into()),
                        other => return Err(format!("Unknown status option '{other}'.")),
                    }
                }
                command = Command::Status { json, hash };
            }
            Some("backup") => command = Command::Backup(words.map(str::to_string).collect()),
            Some("update") => command = Command::Update(words.map(str::to_string).collect()),
            Some("net") => command = Command::Net(words.map(str::to_string).collect()),
            Some("time") => command = Command::Time(words.map(str::to_string).collect()),
            Some(other) => {
                return Err(format!(
                    "CLI command '{other}' is not implemented yet \
                     (available: status, backup, update, net, time)."
                ))
            }
            None => return Err("--cli needs a command (status, backup, update, net, time).".into()),
        }
    }
    Ok(Options { sim, command })
}

/// `--cli status`: print the status; returns the process exit code.
pub fn run_status(app: &App, json: bool, hash: bool) -> i32 {
    if hash {
        match app.start_rescue_check(None) {
            Ok(h) => {
                h.wait();
            }
            Err(e) => *lock(&app.rescue_check) = RescueCheck::Failed(e.to_string()),
        }
    }
    let st = app.status();
    if json {
        match serde_json::to_string_pretty(&st) {
            Ok(s) => {
                println!("{s}");
                0
            }
            Err(e) => {
                eprintln!("error: cannot encode status: {e}");
                1
            }
        }
    } else {
        print!("{}", crate::report::status_text(&st));
        0
    }
}

fn find_stick(app: &App, id: &str) -> Result<Stick, String> {
    app.backend
        .external_sticks()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("No stick '{id}' (see 'backup sticks')."))
}

/// Partition names in `args`; all of them when none is given.
fn roles_arg(args: &[String]) -> Result<Vec<PartitionRole>, String> {
    let mut roles = Vec::new();
    for a in args.iter().filter(|a| !a.starts_with("--")) {
        roles.push(PartitionRole::from_name(a).ok_or_else(|| format!("Unknown partition '{a}'."))?);
    }
    if roles.is_empty() {
        roles = PartitionRole::ALL.to_vec();
    }
    Ok(roles)
}

/// Run a job like the UI does: in a write task, printing progress to stderr.
fn run_cli_job<T: Send + 'static>(
    app: &App,
    name: &str,
    body: impl FnOnce(&TaskCtx) -> Result<T, TaskFailure> + Send + 'static,
) -> Result<T, String> {
    let out: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
    let o = Arc::clone(&out);
    let h = app
        .tasks
        .spawn(
            name,
            TaskKind::Write,
            move |ctx| {
                let v = body(ctx)?;
                *lock(&o) = Some(v);
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
        TaskState::Succeeded(_) => lock(&out).take().ok_or_else(|| "no result".to_string()),
        TaskState::Cancelled => Err("Cancelled.".into()),
        TaskState::Failed(e) => Err(e),
        TaskState::Running => Err("still running".into()),
    }
}

fn backup_cmd(app: &App, args: &[String]) -> Result<String, String> {
    let backend = Arc::clone(&app.backend);
    let arg = |i: usize, what: &str| {
        args.get(i)
            .cloned()
            .ok_or_else(|| format!("backup {}: missing {what}.", args[0]))
    };
    match args.first().map(String::as_str) {
        Some("sticks") => {
            let sticks = backend.external_sticks().map_err(|e| e.to_string())?;
            Ok(sticks
                .iter()
                .map(|s| format!("{}\t{}\n", s.id, s.description))
                .collect())
        }
        Some("list") => {
            let stick = find_stick(app, &arg(1, "stick")?)?;
            let machine = backup_jobs::device_identity(backend.as_ref()).machine;
            let list = backup_jobs::list_stick_backups(backend.as_ref(), &stick)?;
            Ok(list
                .iter()
                .map(|b| backup_jobs::describe_backup(b, machine.as_deref()) + "\n")
                .collect())
        }
        Some("export") => {
            let stick = find_stick(app, &arg(1, "stick")?)?;
            let req = backup_jobs::ExportRequest {
                roles: roles_arg(&args[2..])?,
                stick: stick.clone(),
            };
            let preview = backup_jobs::export_preview(backend.as_ref(), &req)?;
            eprint!("{}", preview.text());
            let rep = run_cli_job(app, "Backup export", move |ctx| {
                backup_jobs::run_export(backend.as_ref(), &req, ctx)
            })?;
            Ok(backup_jobs::export_summary(&rep, &stick) + "\n")
        }
        Some(cmd @ ("verify" | "import")) => {
            let stick = find_stick(app, &arg(1, "stick")?)?;
            let name = arg(2, "backup name")?;
            let list = backup_jobs::list_stick_backups(backend.as_ref(), &stick)?;
            let found = list
                .into_iter()
                .find(|b| b.info.name == name)
                .ok_or_else(|| format!("No backup '{name}' on {}.", stick.id))?;
            let mut req = backup_jobs::ImportRequest {
                stick,
                dir_rel: found.dir_rel,
                name: name.clone(),
                roles: roles_arg(&args[3..])?,
                mode: if args.iter().any(|a| a == "--merge") {
                    ImportMode::Merge
                } else {
                    ImportMode::Replace
                },
            };
            if cmd == "verify" {
                return run_cli_job(app, "Backup verification", move |ctx| {
                    backup_jobs::run_verify(backend.as_ref(), &req, ctx)
                })
                .map(|m| m + "\n");
            }
            if args[3..].iter().all(|a| a.starts_with("--")) {
                // default: every partition in the backup
                if let Ok(m) = &found.info.metadata {
                    req.roles = m.roles();
                }
            }
            let out = run_cli_job(app, "Backup import", move |ctx| {
                backup_jobs::run_import(backend.as_ref(), &req, ctx)
            })?;
            let summary = backup_jobs::import_summary(&name, &out) + "\n";
            if out.report.error_count > 0 {
                // Partly restored: a failure for scripts.
                return Err(summary);
            }
            Ok(summary)
        }
        Some("reset") => {
            let full = match arg(1, "config|full")?.as_str() {
                "config" => false,
                "full" => true,
                other => return Err(format!("Unknown reset kind '{other}'.")),
            };
            let confirmation = args
                .iter()
                .position(|a| a == "--confirm")
                .and_then(|i| args.get(i + 1))
                .cloned()
                .unwrap_or_default();
            let req = backup_jobs::ResetRequest {
                full,
                reset_rescue_settings: args.iter().any(|a| a == "--reset-rescue"),
                confirmation,
            };
            let req2 = req.clone();
            let rep = run_cli_job(app, "Factory reset", move |ctx| {
                backup_jobs::run_reset(backend.as_ref(), &req, ctx)
            })?;
            let summary = backup_jobs::reset_summary(&req2, &rep) + "\n";
            if !rep.errors.is_empty() {
                return Err(summary);
            }
            Ok(summary)
        }
        _ => Err("backup needs: sticks | list | export | verify | import | reset.".into()),
    }
}

/// `--cli backup ...`; returns the process exit code.
pub fn run_backup(app: &App, args: &[String]) -> i32 {
    match backup_cmd(app, args) {
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

/// Value after `--name`.
fn opt_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// The update source from `--stick/--path` or `--base/--version/--machine/--suffix`.
fn origin_arg(app: &App, args: &[String]) -> Result<update_jobs::Origin, String> {
    use std::str::FromStr;
    if let Some(id) = opt_value(args, "--stick") {
        let stick = find_stick(app, &id)?;
        let path = opt_value(args, "--path").ok_or("--stick needs --path (see 'update scan').")?;
        let rel = PathBuf::from(path.trim_start_matches('/'));
        let is_image = rel.extension().is_some_and(|e| e == "squashfs");
        return Ok(if is_image {
            update_jobs::Origin::StickImage { stick, rel }
        } else {
            update_jobs::Origin::StickTree { stick, rel }
        });
    }
    let dev = update_jobs::device_target(app.backend.as_ref());
    let base = opt_value(args, "--base").unwrap_or_else(|| DEFAULT_OTA_BASE_URL.to_string());
    let version = opt_value(args, "--version").or(dev.target.as_ref().map(|t| t.version.clone()));
    let machine = opt_value(args, "--machine").or(dev.machine.clone());
    let suffix = match opt_value(args, "--suffix") {
        Some(s) => Some(Suffix::from_str(&s).map_err(|e| e.to_string())?),
        None => dev.target.as_ref().map(|t| t.suffix),
    };
    let (Some(version), Some(machine), Some(suffix)) = (version, machine, suffix) else {
        return Err("No valid slot: give --version, --machine and --suffix (OTA-1).".into());
    };
    if let Some(t) = &dev.target {
        if t.suffix != suffix {
            eprintln!("warning: switching suffix from '{}' to '{suffix}'.", t.suffix);
        }
    }
    let proxy = opt_value(args, "--proxy").or_else(|| {
        Some(DEFAULT_HTTP_PROXY.to_string()).filter(|p| !p.is_empty())
    });
    Ok(update_jobs::Origin::Network {
        base_url: base,
        target: Target::new(version, machine, suffix),
        proxy,
    })
}

/// SHA-256 of the running rescue image (for "matches current rescue", OTA-9). Computed here
/// (the CLI has no background check) unless `--no-hash` is given.
fn rescue_sha(app: &App, args: &[String]) -> Option<String> {
    if has_flag(args, "--no-hash") {
        return None;
    }
    if lock(&app.rescue_check).sha256().is_none() {
        eprintln!("Computing the SHA-256 of the rescue partition (skip with --no-hash)...");
        match app.start_rescue_check(None) {
            Ok(h) => {
                h.wait();
            }
            Err(e) => *lock(&app.rescue_check) = RescueCheck::Failed(e.to_string()),
        }
    }
    let check = lock(&app.rescue_check).clone();
    if let RescueCheck::Failed(e) = &check {
        eprintln!("warning: rescue image SHA-256 not available: {e}");
    }
    check.sha256().map(str::to_string)
}

fn update_cmd(app: &App, args: &[String]) -> Result<String, String> {
    let backend = Arc::clone(&app.backend);
    match args.first().map(String::as_str) {
        Some("scan") => {
            let id = args.get(1).ok_or("update scan: missing stick.")?;
            let stick = find_stick(app, id)?;
            let items = update_jobs::scan_stick(backend.as_ref(), &stick)?;
            if items.is_empty() {
                return Ok(format!("No update found on {id}.\n"));
            }
            Ok(items.iter().map(|i| i.label() + "\n").collect())
        }
        Some("check") => {
            let origin = origin_arg(app, &args[1..])?;
            let sha = rescue_sha(app, args);
            let c = run_cli_job(app, "Update check", move |ctx| {
                update_jobs::check(backend.as_ref(), &origin, sha.as_deref(), ctx)
            })?;
            Ok(c.text())
        }
        Some("start") => {
            let targets: update_jobs::Targets = opt_value(args, "--slot")
                .ok_or("update start: missing --slot A|B|AB.")?
                .parse()?;
            if targets == update_jobs::Targets::Both
                && opt_value(args, "--confirm").as_deref() != Some(WRITE_BOTH_CONFIRM_WORD)
            {
                return Err(format!(
                    "Writing both slots needs --confirm {WRITE_BOTH_CONFIRM_WORD} (UI-4)."
                ));
            }
            let origin = origin_arg(app, &args[1..])?;
            let sha = rescue_sha(app, args);
            let b2 = Arc::clone(&backend);
            let o2 = origin.clone();
            let c = run_cli_job(app, "Update check", move |ctx| {
                update_jobs::check(b2.as_ref(), &o2, sha.as_deref(), ctx)
            })?;
            eprint!("{}", c.text());
            let row = match opt_value(args, "--image") {
                Some(ts) => c
                    .rootfs
                    .iter()
                    .find(|r| r.image.build_timestamp.as_str() == ts)
                    .ok_or_else(|| format!("No rootfs image with timestamp {ts}."))?,
                None => c.rootfs.first().ok_or("The manifest has no rootfs image.")?,
            };
            if row.is_downgrade(targets) && !has_flag(args, "--allow-downgrade") {
                return Err(
                    "This is the same or an older build (downgrade): add --allow-downgrade.".into(),
                );
            }
            if c.unverified && !has_flag(args, "--allow-unverified") {
                return Err(
                    "The image has no .sha256 sidecar (unverified): add --allow-unverified.".into(),
                );
            }
            let mut options = xoros_core::flash::FlashOptions::default();
            options.resume = !has_flag(args, "--no-resume");
            let req = update_jobs::UpdateRequest {
                origin,
                image: row.image.clone(),
                targets,
                options,
            };
            let b3 = Arc::clone(&backend);
            match run_cli_job(app, "Slot update", move |ctx| {
                update_jobs::run_update(b3.as_ref(), &req, ctx)
            }) {
                Ok(o) => Ok(update_jobs::update_summary(&o)),
                Err(e) => {
                    let note = update_jobs::disabled_note(backend.as_ref(), &targets.slots());
                    Err(format!("{e}\n{note}").trim().to_string())
                }
            }
        }
        _ => Err("update needs: check | scan | start.".into()),
    }
}

fn net_cmd(app: &App, args: &[String]) -> Result<String, String> {
    let env = net::NetEnv::for_backend(app.backend.as_ref());
    match args.first().map(String::as_str) {
        Some("up") => {
            let rep = net::config::bring_up_ethernet(&env).map_err(|e| e.to_string())?;
            let mut t = rep.text();
            for f in &rep.files {
                t.push_str(&format!("wrote {}\n", f.display()));
            }
            if rep.interfaces.is_empty() || !rep.notes.is_empty() {
                return Err(t);
            }
            Ok(t)
        }
        Some("dns") => {
            let v = net::config::refresh_resolv_conf(&env).map_err(|e| e.to_string())?;
            Ok(format!(
                "{}: {}\n",
                env.resolv_conf().display(),
                v.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" ")
            ))
        }
        Some("test") => {
            let dev = update_jobs::device_target(app.backend.as_ref());
            let base = opt_value(args, "--base").unwrap_or_else(|| DEFAULT_OTA_BASE_URL.to_string());
            let url = match &dev.target {
                Some(t) => format!("{}/manifest.json", net::http::ota_dir_url(&base, t)),
                None => base.clone(),
            };
            let opts = net::diag::TestOptions {
                url,
                proxy: opt_value(args, "--proxy"),
            };
            let steps = net::diag::run(&env, &opts, &crate::tasks::CancelToken::new(), &mut |s| {
                eprintln!("{}", net::diag::step_line(s))
            });
            let failed = steps
                .iter()
                .any(|s| s.status == net::diag::StepStatus::Fail);
            let text = net::diag::report_text(&steps);
            if failed {
                Err(text)
            } else {
                Ok(text)
            }
        }
        _ => Err("net needs: up | dns | test.".into()),
    }
}

fn time_cmd(app: &App, args: &[String]) -> Result<String, String> {
    use net::time;
    let env = net::NetEnv::for_backend(app.backend.as_ref());
    match args.first().map(String::as_str) {
        Some("show") | None => Ok(format!(
            "{}{}\n",
            time::format_utc(std::time::SystemTime::now()),
            if time::clock_plausible() {
                ""
            } else {
                "  (before the build date of this app: sync it)"
            }
        )),
        Some("sync") => time::sync(&env, &args[1..]).map(|r| r.text() + "\n"),
        Some("set") => {
            let t = time::parse_manual(&args[1..].join(" "))?;
            time::set_system_time(&env, t)?;
            Ok(format!("Clock set to {}.\n", time::format_utc(t)))
        }
        _ => Err("time needs: show | sync | set.".into()),
    }
}

/// Print the result of a command; returns the process exit code.
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

/// `--cli update ...`.
pub fn run_update(app: &App, args: &[String]) -> i32 {
    print_result(update_cmd(app, args))
}

/// `--cli net ...`.
pub fn run_net(app: &App, args: &[String]) -> i32 {
    print_result(net_cmd(app, args))
}

/// `--cli time ...`.
pub fn run_time(app: &App, args: &[String]) -> i32 {
    print_result(time_cmd(app, args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let o = parse_args(Vec::<String>::new()).expect("empty");
        assert_eq!(o.command, Command::Tui);
        let o = parse_args(["--sim", "d", "--cli", "status", "--json"]).expect("status");
        assert_eq!(o.sim, Some(PathBuf::from("d")));
        assert_eq!(
            o.command,
            Command::Status {
                json: true,
                hash: true
            }
        );
        let o = parse_args(["--cli", "status", "--no-hash"]).expect("status");
        assert_eq!(
            o.command,
            Command::Status {
                json: false,
                hash: false
            }
        );
        let o = parse_args(["--sim=x"]).expect("sim=");
        assert_eq!(o.sim, Some(PathBuf::from("x")));
        assert_eq!(
            parse_args(["--sim-init", "s"]).map(|o| o.command),
            Ok(Command::SimInit("s".into()))
        );
        assert!(parse_args(["--sim"]).is_err());
        assert!(parse_args(["--cli"]).is_err());
        assert!(parse_args(["--cli", "boot"]).is_err());
        assert_eq!(
            parse_args(["--cli", "update", "start", "--slot", "B"]).map(|o| o.command),
            Ok(Command::Update(vec!["start".into(), "--slot".into(), "B".into()]))
        );
        assert_eq!(
            parse_args(["--cli", "time", "sync"]).map(|o| o.command),
            Ok(Command::Time(vec!["sync".into()]))
        );
        assert_eq!(
            parse_args(["--cli", "net", "test"]).map(|o| o.command),
            Ok(Command::Net(vec!["test".into()]))
        );
        assert_eq!(
            parse_args(["--cli", "backup", "list", "usb1"]).map(|o| o.command),
            Ok(Command::Backup(vec!["list".into(), "usb1".into()]))
        );
        assert!(parse_args(["--cli", "status", "--bogus"]).is_err());
        assert!(parse_args(["--what"]).is_err());
    }
}
