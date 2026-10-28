//! CLI for network configuration (NET-2, NET-5, NET-6, NET-8), network details (SYS-4), the
//! system report (SYS-5), the diagnostics bundle (LOG-2) and remote access (SSH-1); called
//! from [`crate::cli`]. Every function returns the text to print or an error (exit code 1).

use std::io::BufRead;

use crate::app::App;
use crate::backend::Stick;
use crate::diagnostics;
use crate::net::profile::{self, ConfigInput, WifiInput};
use crate::net::{self, NetEnv};
use crate::tasks::TaskFailure;

pub const USAGE: &str = "\
\x20 xoros-rescue-shell [--sim <dir>] --cli net show [--json]
  xoros-rescue-shell [--sim <dir>] --cli net scan <if>
  xoros-rescue-shell [--sim <dir>] --cli net config <if> dhcp|static [<addr>/<prefix>]
                     [--gateway <ip>] [--dns <ip>[,<ip>...]] [--wifi <ssid> [--hidden]
                     [--security open|wpa2|wpa3] [--psk <passphrase>|-] [--country <CC>]]
                     [--no-apply] [--remember]
  xoros-rescue-shell [--sim <dir>] --cli net unconfig <if> [--remember]
  xoros-rescue-shell [--sim <dir>] --cli net list [--remembered]
  xoros-rescue-shell [--sim <dir>] --cli net remember | forget
  xoros-rescue-shell [--sim <dir>] --cli net export-nm [<if>...]
  xoros-rescue-shell [--sim <dir>] --cli net nm-list
  xoros-rescue-shell [--sim <dir>] --cli net import-nm <file> [--if <if>] [--no-apply] [--remember]
  xoros-rescue-shell [--sim <dir>] --cli report [--json] [--stick <stick>]
  xoros-rescue-shell [--sim <dir>] --cli diag <stick>
  xoros-rescue-shell [--sim <dir>] --cli ssh start | stop | status | add-keys <stick>|<file>
";

fn opt_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Options that take a value (their values are not positional arguments).
const VALUE_OPTS: &[&str] = &[
    "--gateway",
    "--dns",
    "--wifi",
    "--security",
    "--psk",
    "--country",
    "--if",
    "--stick",
];

fn positional(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if VALUE_OPTS.contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            out.push(a.as_str());
        }
    }
    out
}

fn find_stick(app: &App, id: &str) -> Result<Stick, String> {
    app.backend
        .external_sticks()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("No stick '{id}' (see 'backup sticks')."))
}

fn task_err(e: TaskFailure) -> String {
    match e {
        TaskFailure::Cancelled => "Cancelled.".into(),
        TaskFailure::Error(e) => e,
    }
}

fn read_secret_stdin() -> Result<String, String> {
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("Cannot read the passphrase from stdin: {e}"))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// `net config <if> dhcp|static ...` arguments to a [`ConfigInput`] and country.
pub fn parse_config(args: &[String]) -> Result<(ConfigInput, Option<String>), String> {
    let pos = positional(args);
    let ifname = pos.first().ok_or("net config: missing interface.")?;
    let mode = pos.get(1).ok_or("net config: missing dhcp|static.")?;
    let static_ip = match *mode {
        "dhcp" => None,
        "static" => {
            let cidr = pos
                .get(2)
                .ok_or("net config static: missing <addr>/<prefix>.")?;
            Some((
                cidr.to_string(),
                opt_value(args, "--gateway").unwrap_or_default(),
            ))
        }
        other => return Err(format!("net config: '{other}' is not dhcp or static.")),
    };
    let wifi = match opt_value(args, "--wifi") {
        None => None,
        Some(ssid) => {
            let mut passphrase = opt_value(args, "--psk").unwrap_or_default();
            if passphrase == "-" {
                passphrase = read_secret_stdin()?;
            }
            let security = opt_value(args, "--security").unwrap_or_else(|| {
                if passphrase.is_empty() {
                    "open".into()
                } else {
                    "wpa2".into()
                }
            });
            Some(WifiInput {
                ssid,
                hidden: has_flag(args, "--hidden"),
                security,
                passphrase,
            })
        }
    };
    let country = match opt_value(args, "--country") {
        Some(cc) => {
            let cc = cc.to_ascii_uppercase();
            if !net::wifi::valid_country(&cc) {
                return Err(format!("'{cc}' is not a two-letter country code."));
            }
            Some(cc)
        }
        None => None,
    };
    Ok((
        ConfigInput {
            interface: ifname.to_string(),
            wifi,
            static_ip,
            dns: opt_value(args, "--dns").unwrap_or_default(),
        },
        country,
    ))
}

fn remember_now(app: &App, env: &NetEnv) -> Result<String, String> {
    let state = profile::load_state(env);
    let p = profile::remember(app.backend.as_ref(), &state)?;
    Ok(format!(
        "Remembered {} connection(s) in overlay:/{} for the next rescue boot.\n",
        state.connections.len(),
        p.display()
    ))
}

fn list_text(cfg: &profile::RescueNetConfig) -> String {
    if cfg.connections.is_empty() {
        return "No network configuration.\n".into();
    }
    let mut t: String = cfg
        .connections
        .iter()
        .map(|c| profile::describe(c) + "\n")
        .collect();
    if let Some(cc) = &cfg.country {
        t.push_str(&format!("Wi-Fi country: {cc}\n"));
    }
    t
}

/// `--cli net <args>` for the commands of this module; `None` for other commands.
pub fn net_cmd(app: &App, args: &[String]) -> Option<Result<String, String>> {
    let env = NetEnv::for_backend(app.backend.as_ref());
    let rest = args.get(1..).unwrap_or(&[]);
    let r = match args.first().map(String::as_str)? {
        "show" => {
            let d = net::details::collect(&env);
            if has_flag(rest, "--json") {
                serde_json::to_string_pretty(&d)
                    .map(|s| s + "\n")
                    .map_err(|e| e.to_string())
            } else {
                Ok(net::details::text(&d))
            }
        }
        "scan" => (|| {
            let ifname = positional(rest)
                .first()
                .copied()
                .ok_or("net scan: missing interface.")?;
            let v = net::wifi::scan(&env, ifname)?;
            if v.is_empty() {
                return Ok(format!("No networks found on {ifname}.\n"));
            }
            Ok(v.iter().map(|e| e.label() + "\n").collect())
        })(),
        "config" => (|| {
            let (input, country) = parse_config(rest)?;
            let c = profile::build(&input)?;
            let apply = !has_flag(rest, "--no-apply");
            let rep = profile::configure(&env, c, country, apply).map_err(|e| e.to_string())?;
            let mut t = rep.text();
            if has_flag(rest, "--remember") {
                t.push_str(&remember_now(app, &env)?);
            }
            Ok(t)
        })(),
        "unconfig" => (|| {
            let ifname = positional(rest)
                .first()
                .copied()
                .ok_or("net unconfig: missing interface.")?;
            let found = profile::unconfigure(&env, ifname).map_err(|e| e.to_string())?;
            let mut t = if found {
                format!("Configuration of {ifname} removed.\n")
            } else {
                format!("{ifname} had no configuration; its files were cleaned up.\n")
            };
            if has_flag(rest, "--remember") {
                t.push_str(&remember_now(app, &env)?);
            }
            Ok(t)
        })(),
        "list" => {
            if has_flag(rest, "--remembered") {
                profile::load_remembered(app.backend.as_ref()).map(|r| match r {
                    Some(c) => list_text(&c.redacted()),
                    None => "No remembered configuration.\n".into(),
                })
            } else {
                Ok(list_text(&profile::load_state(&env)))
            }
        }
        "remember" => {
            if profile::load_state(&env).connections.is_empty() {
                Err("No network configuration to remember (use 'net config' first).".into())
            } else {
                remember_now(app, &env)
            }
        }
        "forget" => profile::forget(app.backend.as_ref()).map(|found| {
            if found {
                "Remembered network configuration deleted.\n".into()
            } else {
                "There was no remembered network configuration.\n".into()
            }
        }),
        "export-nm" => (|| {
            let state = profile::load_state(&env);
            let wanted = positional(rest);
            let conns: Vec<_> = if wanted.is_empty() {
                state.connections.clone()
            } else {
                wanted
                    .iter()
                    .map(|i| {
                        state.get(i).cloned().ok_or_else(|| {
                            format!("{i} has no rescue configuration (use 'net config').")
                        })
                    })
                    .collect::<Result<_, String>>()?
            };
            let files = net::nm::export(app.backend.as_ref(), &conns)?;
            Ok(files
                .iter()
                .map(|f| format!("wrote overlay:/{}\n", f.display()))
                .collect())
        })(),
        "nm-list" => {
            let (v, notes) = net::nm::list_normal_system(app.backend.as_ref());
            let mut t: String = notes.iter().map(|n| format!("note: {n}\n")).collect();
            if v.is_empty() {
                t.push_str("No NetworkManager connections found.\n");
            }
            for p in &v {
                t.push_str(&p.label());
                t.push('\n');
            }
            Ok(t)
        }
        "import-nm" => (|| {
            let file = positional(rest)
                .first()
                .copied()
                .ok_or("net import-nm: missing file name.")?;
            let (v, _) = net::nm::list_normal_system(app.backend.as_ref());
            let p = v
                .iter()
                .find(|p| p.file == file || p.file == format!("{file}.nmconnection"))
                .ok_or_else(|| format!("No connection '{file}' (see 'net nm-list')."))?;
            let c = p
                .config
                .clone()
                .map_err(|e| format!("{file} cannot be used: {e}"))?;
            let ifname = opt_value(rest, "--if")
                .or(c.interface.clone())
                .ok_or("The connection is not bound to an interface: give --if <if>.")?;
            let c = import_as(c, &ifname)?;
            let rep = profile::configure(&env, c, None, !has_flag(rest, "--no-apply"))
                .map_err(|e| e.to_string())?;
            let mut t = rep.text();
            if has_flag(rest, "--remember") {
                t.push_str(&remember_now(app, &env)?);
            }
            Ok(t)
        })(),
        _ => return None,
    };
    Some(r)
}

/// A connection of the normal system as rescue configuration of `ifname` (NET-8): own id and
/// a new UUID (the original keeps its own), secret hashed.
pub fn import_as(
    mut c: xoros_core::net::nm_keyfile::NetConfig,
    ifname: &str,
) -> Result<xoros_core::net::nm_keyfile::NetConfig, String> {
    use xoros_core::net::nm_keyfile::{new_uuid, Medium};
    c.interface = Some(ifname.to_string());
    c.id = format!("xoros-{ifname}");
    c.uuid = new_uuid();
    c.validate().map_err(|e| e.to_string())?;
    if let Medium::Wifi(w) = &mut c.medium {
        net::wifi::hash_secret(w);
    }
    Ok(c)
}

/// `--cli report [--json] [--stick <id>]` (SYS-5).
pub fn report_cmd(app: &App, args: &[String]) -> Result<String, String> {
    if let Some(id) = opt_value(args, "--stick") {
        let stick = find_stick(app, &id)?;
        let names = diagnostics::save_report(app, &stick).map_err(task_err)?;
        return Ok(names
            .iter()
            .map(|n| format!("wrote {}:/{n}\n", stick.id))
            .collect());
    }
    let r = diagnostics::collect(app);
    Ok(if has_flag(args, "--json") {
        diagnostics::report_json(&r)
    } else {
        diagnostics::report_text(&r)
    })
}

/// `--cli diag <stick>` (LOG-2).
pub fn diag_cmd(app: &App, args: &[String]) -> Result<String, String> {
    let id = positional(args)
        .first()
        .copied()
        .ok_or("diag: missing stick (see 'backup sticks').")?;
    let stick = find_stick(app, id)?;
    eprintln!("Collecting diagnostics...");
    let name = diagnostics::write_bundle(app, &stick, None).map_err(task_err)?;
    Ok(format!("wrote {}:/{name}\n", stick.id))
}

/// Read an `authorized_keys` file from a stick (first of [`net::ssh::STICK_KEY_FILES`]).
pub fn keys_from_stick(app: &App, stick: &Stick) -> Result<String, String> {
    let guard = app
        .backend
        .mount_stick(stick, false)
        .map_err(|e| e.to_string())?;
    let found = net::ssh::STICK_KEY_FILES
        .iter()
        .find_map(|rel| std::fs::read_to_string(guard.path().join(rel)).ok());
    drop(guard);
    found.ok_or_else(|| {
        format!(
            "No {} on stick {}.",
            net::ssh::STICK_KEY_FILES.join(" or "),
            stick.id
        )
    })
}

/// `--cli ssh start | stop | status | add-keys <stick>|<file>` (SSH-1).
pub fn ssh_cmd(app: &App, args: &[String]) -> Result<String, String> {
    let env = NetEnv::for_backend(app.backend.as_ref());
    match args.first().map(String::as_str) {
        Some("start") => net::ssh::start(&env)
            .map(|s| s.text())
            .map_err(|e| e.to_string()),
        Some("stop") => net::ssh::stop(&env)
            .map(|was| {
                if was {
                    "sshd stopped.\n".into()
                } else {
                    "sshd was not running.\n".into()
                }
            })
            .map_err(|e| e.to_string()),
        Some("status") | None => Ok(net::ssh::status(&env).text()),
        Some("add-keys") => {
            let src = args.get(1).ok_or("ssh add-keys: missing stick or file.")?;
            let text = if src.contains('/') {
                std::fs::read_to_string(src).map_err(|e| format!("{src}: {e}"))?
            } else {
                keys_from_stick(app, &find_stick(app, src)?)?
            };
            let n = net::ssh::add_authorized_keys(&env, &text).map_err(|e| e.to_string())?;
            Ok(format!("{n} key(s) added.\n"))
        }
        Some(other) => Err(format!(
            "ssh: unknown command '{other}' (start, stop, status, add-keys)."
        )),
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
    fn parse_config_args() {
        let (i, cc) = parse_config(&a(&[
            "eth0",
            "static",
            "10.0.2.50/24",
            "--gateway",
            "10.0.2.2",
            "--dns",
            "1.1.1.1",
        ]))
        .unwrap();
        assert_eq!(i.interface, "eth0");
        assert_eq!(
            i.static_ip,
            Some(("10.0.2.50/24".into(), "10.0.2.2".into()))
        );
        assert_eq!(i.dns, "1.1.1.1");
        assert_eq!(cc, None);
        let (i, cc) = parse_config(&a(&[
            "wlan0",
            "dhcp",
            "--wifi",
            "My Net",
            "--psk",
            "secret123",
            "--country",
            "de",
            "--hidden",
        ]))
        .unwrap();
        let w = i.wifi.unwrap();
        assert_eq!(
            (w.ssid.as_str(), w.security.as_str(), w.hidden),
            ("My Net", "wpa2", true)
        );
        assert_eq!(cc.as_deref(), Some("DE"));
        assert!(parse_config(&a(&["eth0"])).is_err());
        assert!(parse_config(&a(&["eth0", "auto"])).is_err());
        assert!(parse_config(&a(&["eth0", "static"])).is_err());
        assert!(parse_config(&a(&["wlan0", "dhcp", "--wifi", "x", "--country", "DEU"])).is_err());
    }

    #[test]
    fn cli_flow_in_simulation() {
        let d = TempDir::new("cli-net");
        SimBackend::create_example(d.path()).expect("example");
        let app = App::new(make_backend(Some(d.path().to_path_buf())).expect("backend"));
        let run = |v: &[&str]| net_cmd(&app, &a(v)).expect("handled");
        assert!(net_cmd(&app, &a(&["up"])).is_none());
        let out = run(&[
            "config",
            "eth0",
            "static",
            "10.0.2.50/24",
            "--gateway",
            "10.0.2.2",
            "--remember",
        ])
        .unwrap();
        assert!(out.contains("Remembered 1 connection"), "{out}");
        assert!(run(&["list"])
            .unwrap()
            .contains("eth0: ethernet, static 10.0.2.50/24"));
        assert!(run(&["list", "--remembered"])
            .unwrap()
            .contains("10.0.2.50/24"));
        let out = run(&["export-nm"]).unwrap();
        assert!(out.contains("xoros-eth0.nmconnection"), "{out}");
        let kf = d.path().join(
            "parts/overlay/overlays/etc/upper/NetworkManager/system-connections/xoros-eth0.nmconnection",
        );
        assert!(std::fs::read_to_string(kf)
            .unwrap()
            .contains("address1=10.0.2.50/24,10.0.2.2"));
        assert!(run(&["export-nm", "wlan9"]).is_err());
        let out = run(&["nm-list"]).unwrap();
        assert!(out.contains("xoros-eth0.nmconnection [overlay]"), "{out}");
        assert!(
            out.contains("eth0.nmconnection [overlay] read-only"),
            "{out}"
        );
        let out = run(&["import-nm", "xoros-eth0", "--if", "eth1", "--no-apply"]).unwrap();
        assert!(out.contains("Saved (not applied): eth1"), "{out}");
        assert!(run(&["import-nm", "eth0"]).is_err());
        assert!(run(&["show"]).unwrap().contains("DNS"));
        assert!(run(&["show", "--json"]).unwrap().contains("\"interfaces\""));
        assert!(run(&["scan", "wlan0"]).unwrap().contains("XorOS-Lab"));
        assert!(run(&["unconfig", "eth1"]).unwrap().contains("removed"));
        assert!(run(&["forget"]).unwrap().contains("deleted"));

        let out = report_cmd(&app, &a(&["--stick", "usb1"])).unwrap();
        assert!(out.contains("xoros-report-"));
        assert!(report_cmd(&app, &a(&["--json"]))
            .unwrap()
            .contains("\"status\""));
        assert!(diag_cmd(&app, &a(&["usb1"])).unwrap().contains(".tar.gz"));
        assert!(diag_cmd(&app, &a(&["nope"])).is_err());

        assert!(ssh_cmd(&app, &a(&["start"]))
            .unwrap()
            .contains("One-time root password"));
        assert!(ssh_cmd(&app, &a(&["status"])).unwrap().contains("running"));
        assert!(ssh_cmd(&app, &a(&["add-keys", "usb1"])).is_err());
        assert!(ssh_cmd(&app, &a(&["stop"])).unwrap().contains("stopped"));
    }
}
