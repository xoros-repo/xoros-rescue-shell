//! Network menu: Ethernet DHCP (NET-1), configuration per interface with Wi-Fi and static
//! addresses (NET-2), test (NET-4), date and time (NET-7), the remembered configuration
//! (NET-5) and the NetworkManager connections of the normal system (NET-6, NET-8).

use std::collections::BTreeMap;
use std::time::SystemTime;

use cursive::menu::Tree;
use cursive::traits::*;
use cursive::views::{
    Button, Checkbox, Dialog, EditView, LinearLayout, ListView, ScrollView, SelectView, TextView,
};
use cursive::Cursive;
use xoros_core::net::nm_keyfile::NetConfig;

use crate::net::profile::{self, ConfigInput, WifiInput};
use crate::net::{config, diag, info, nm, time, wifi, NetEnv};
use crate::tasks::{TaskFailure, TaskKind};
use crate::texts;
use crate::ui::dialogs::{self, closable, MAX_H, MAX_W};
use crate::ui::jobs::{self, JobEnd};
use crate::ui::{app, pop_to_base};

const JOB_DIALOG: &str = "network_job_progress";
const TIME_INPUT: &str = "manual_time_input";
const F_SSID: &str = "netcfg_ssid";
const F_HIDDEN: &str = "netcfg_hidden";
const F_SECURITY: &str = "netcfg_security";
const F_PASS: &str = "netcfg_pass";
const F_COUNTRY: &str = "netcfg_country";
const F_METHOD: &str = "netcfg_method";
const F_ADDRESS: &str = "netcfg_address";
const F_GATEWAY: &str = "netcfg_gateway";
const F_DNS: &str = "netcfg_dns";
const F_REMEMBER: &str = "netcfg_remember";

pub fn menu() -> Tree {
    Tree::new()
        .leaf("Ethernet DHCP", dhcp)
        .leaf("Configure", configure)
        .leaf("Test", test)
        .leaf("Speed test", |s| {
            pop_to_base(s);
            crate::ui::tests::run_test(
                s,
                crate::hwtest::TestRequest::NetSpeed(crate::net::speed::SpeedOptions::default()),
            );
        })
        .leaf("Date and time", date_time)
        .delimiter()
        .leaf("Remembered settings", remembered)
        .leaf("Use in normal system", use_in_normal_system)
        .leaf("Load from normal system", load_from_normal_system)
}

fn env(s: &mut Cursive) -> Option<NetEnv> {
    app(s).map(|a| NetEnv::for_backend(a.backend.as_ref()))
}

fn dhcp(s: &mut Cursive) {
    pop_to_base(s);
    jobs::run(
        s,
        "Ethernet DHCP",
        TaskKind::ReadOnly,
        JOB_DIALOG,
        |app, ctx| {
            let env = NetEnv::for_backend(app.backend.as_ref());
            ctx.set_message("Writing .network files");
            let rep = config::bring_up_ethernet(&env)?;
            ctx.set_message("Waiting for DHCP");
            for _ in 0..20 {
                ctx.check_cancelled()?;
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            let dns = config::refresh_resolv_conf(&env)?;
            let list: Vec<String> = dns.iter().map(|d| d.to_string()).collect();
            Ok(format!(
                "{}\nDNS servers ({}): {}",
                rep.text(),
                env.resolv_conf().display(),
                if list.is_empty() {
                    "none yet".to_string()
                } else {
                    list.join(", ")
                }
            ))
        },
        |s, end| show_end(s, "Ethernet DHCP", end),
    );
}

fn show_end(s: &mut Cursive, title: &str, end: JobEnd<String>) {
    match end {
        JobEnd::Ok(t) => s.add_layer(dialogs::text_screen(title, &t)),
        JobEnd::Cancelled => dialogs::info(s, title, "Cancelled."),
        JobEnd::Failed(e) => s.add_layer(dialogs::text_screen(title, &e)),
    }
}

fn test(s: &mut Cursive) {
    pop_to_base(s);
    let Some(a) = app(s) else { return };
    let dev = crate::update_jobs::device_target(a.backend.as_ref());
    let cfg = crate::settings::current();
    let url = match &dev.target {
        Some(t) => xoros_core::source::ota_url(&cfg.ota_base_url, t, "manifest.json"),
        None => cfg.ota_base_url.clone(),
    };
    let opts = diag::TestOptions {
        url,
        proxy: cfg.proxy(),
    };
    jobs::run(
        s,
        "Network test",
        TaskKind::ReadOnly,
        JOB_DIALOG,
        move |app, ctx| {
            let env = NetEnv::for_backend(app.backend.as_ref());
            let steps = diag::run(&env, &opts, ctx.cancel_token(), &mut |st| {
                ctx.set_message(diag::step_line(st))
            });
            ctx.check_cancelled()?;
            diag::record(&env, &steps);
            Ok::<_, TaskFailure>(diag::report_text(&steps))
        },
        |s, end| show_end(s, "Network test", end),
    );
}

fn time_text() -> String {
    let now = SystemTime::now();
    let mut t = format!("Current time: {}\n", time::format_utc(now));
    if let Some(rtc) = time::rtc_text() {
        t.push_str(&format!("Hardware clock: {rtc}\n"));
    }
    t.push_str(&format!(
        "Time servers: {}\n",
        crate::settings::current().ntp_servers().join(", ")
    ));
    if !time::clock_plausible() {
        t.push_str(&format!(
            "\nThe clock is before the build date of this app ({}): HTTPS will fail.\n\
             Sync it or set it manually.\n",
            time::format_utc(time::build_time())
        ));
    }
    t.push_str("\nManual setting (UTC, YYYY-MM-DD HH:MM[:SS]):");
    t
}

fn date_time(s: &mut Cursive) {
    pop_to_base(s);
    let now = time::format_utc(SystemTime::now());
    let layout = LinearLayout::vertical()
        .child(TextView::new(time_text()).max_width(MAX_W))
        .child(
            EditView::new()
                .content(now.trim_end_matches(" UTC"))
                .with_name(TIME_INPUT)
                .fixed_width(22),
        );
    s.add_layer(closable(
        Dialog::around(layout)
            .title("Date and time")
            .button("Sync now (SNTP)", |s| {
                s.pop_layer();
                jobs::run(
                    s,
                    "Time sync",
                    TaskKind::ReadOnly,
                    JOB_DIALOG,
                    |app, ctx| {
                        ctx.set_message("Asking the time servers");
                        let env = NetEnv::for_backend(app.backend.as_ref());
                        time::sync(&env, &[])
                            .map(|r| r.text())
                            .map_err(TaskFailure::Error)
                    },
                    |s, end| show_end(s, "Time sync", end),
                );
            })
            .button("Set", |s| {
                let text = s
                    .call_on_name(TIME_INPUT, |v: &mut EditView| v.get_content().to_string())
                    .unwrap_or_default();
                let Some(env) = env(s) else { return };
                match time::parse_manual(&text).and_then(|t| time::set_manual(&env, t)) {
                    Ok(msg) => {
                        s.pop_layer();
                        dialogs::info(s, "Date and time", &msg);
                    }
                    Err(e) => dialogs::error(s, "Cannot set the time.", &e),
                }
            })
            .dismiss_button(texts::BTN_CLOSE),
    ));
}

// --- NET-2: configuration per interface ----------------------------------------------------

/// Interfaces (without `lo`) with "wireless" flag and current configuration.
fn interfaces(env: &NetEnv) -> Vec<(String, bool)> {
    info::interfaces_from_sysfs(&env.host, &BTreeMap::new())
        .into_iter()
        .map(|i| (i.name, i.wireless))
        .collect()
}

fn configure(s: &mut Cursive) {
    pop_to_base(s);
    let Some(net_env) = env(s) else { return };
    pick_interface(s, &net_env, "Configure", |s, name, wireless| {
        let Some(env) = env(s) else { return };
        let state = profile::load_state(&env);
        let existing = state.get(name).cloned();
        let input = existing
            .as_ref()
            .map(profile::input_from)
            .unwrap_or_else(|| ConfigInput {
                interface: name.to_string(),
                ..ConfigInput::default()
            });
        config_form(s, input, wireless, existing, state.country);
    });
}

/// Choose an interface; `on_pick(name, wireless)`.
fn pick_interface<F>(s: &mut Cursive, env: &NetEnv, title: &str, on_pick: F)
where
    F: Fn(&mut Cursive, &str, bool) + Send + Sync + 'static,
{
    let state = profile::load_state(env);
    let items: Vec<(String, (String, bool))> = interfaces(env)
        .into_iter()
        .map(|(n, w)| {
            let cfg = state
                .get(&n)
                .map(profile::describe)
                .map(|d| d.split_once(": ").map(|(_, r)| r.to_string()).unwrap_or(d))
                .unwrap_or_else(|| {
                    if env
                        .networkd_dir
                        .join(config::network_file_name(&n))
                        .exists()
                    {
                        "DHCP (automatic)".into()
                    } else {
                        "not configured".into()
                    }
                });
            let kind = if w { "Wi-Fi" } else { "wired" };
            let label = crate::report::ellipsize(&format!("{n:<10} {kind:<6} {cfg}"), MAX_W - 4);
            (label, (n, w))
        })
        .collect();
    if items.is_empty() {
        dialogs::info(s, title, "No network interface found.");
        return;
    }
    dialogs::choose(s, title, "Interface:", items, move |s, (n, w)| {
        on_pick(s, n, *w)
    });
}

const SECURITIES: [(&str, &str); 3] = [
    ("WPA2 personal (PSK)", "wpa2"),
    ("WPA3 personal (SAE)", "wpa3"),
    ("Open (no password)", "open"),
];

fn security_index(sec: &str) -> usize {
    SECURITIES.iter().position(|(_, v)| *v == sec).unwrap_or(0)
}

/// The configuration form of one interface (NET-2). `existing` keeps its secret when the
/// passphrase is left empty.
fn config_form(
    s: &mut Cursive,
    input: ConfigInput,
    wireless: bool,
    existing: Option<NetConfig>,
    country: Option<String>,
) {
    let ifname = input.interface.clone();
    let mut list = ListView::new();
    let wifi_in = input
        .wifi
        .clone()
        .or_else(|| wireless.then(WifiInput::default));
    if let Some(w) = &wifi_in {
        let mut sec = SelectView::<&'static str>::new().popup();
        for (label, v) in SECURITIES {
            sec.add_item(label, v);
        }
        let sec_i = if w.security.is_empty() {
            0
        } else {
            security_index(&w.security)
        };
        sec.set_selection(sec_i);
        let ifn = ifname.clone();
        list.add_child(
            "SSID",
            LinearLayout::horizontal()
                .child(
                    EditView::new()
                        .content(w.ssid.clone())
                        .with_name(F_SSID)
                        .fixed_width(34),
                )
                .child(Button::new("Scan", move |s| scan(s, &ifn))),
        );
        list.add_child(
            "Hidden network",
            Checkbox::new().with_checked(w.hidden).with_name(F_HIDDEN),
        );
        list.add_child("Security", sec.with_name(F_SECURITY));
        let hint = if existing
            .as_ref()
            .is_some_and(|c| matches!(c.medium, xoros_core::net::nm_keyfile::Medium::Wifi(_)))
        {
            " (empty: keep)"
        } else {
            ""
        };
        list.add_child(
            &format!("Passphrase{hint}"),
            EditView::new().secret().with_name(F_PASS).fixed_width(34),
        );
        list.add_child(
            "Show passphrase",
            Checkbox::new().on_change(|s, on| {
                s.call_on_name(F_PASS, |v: &mut EditView| v.set_secret(!on));
            }),
        );
        list.add_child(
            "Country (e.g. DE)",
            EditView::new()
                .content(country.clone().unwrap_or_default())
                .max_content_width(2)
                .with_name(F_COUNTRY)
                .fixed_width(4),
        );
        list.add_delimiter();
    }
    let mut method = SelectView::<bool>::new().popup();
    method.add_item("DHCP", false);
    method.add_item("Static", true);
    method.set_selection(usize::from(input.static_ip.is_some()));
    let (addr, gw) = input.static_ip.clone().unwrap_or_default();
    list.add_child("IPv4", method.with_name(F_METHOD));
    list.add_child(
        "Address/prefix",
        EditView::new()
            .content(addr)
            .with_name(F_ADDRESS)
            .fixed_width(20),
    );
    list.add_child(
        "Gateway",
        EditView::new()
            .content(gw)
            .with_name(F_GATEWAY)
            .fixed_width(20),
    );
    list.add_child(
        "DNS servers",
        EditView::new()
            .content(input.dns.clone())
            .with_name(F_DNS)
            .fixed_width(34),
    );
    list.add_delimiter();
    list.add_child(
        "Remember (next boot)",
        Checkbox::new().with_name(F_REMEMBER),
    );

    let is_wifi = wifi_in.is_some();
    let ifn = ifname.clone();
    let ex = existing.clone();
    let title = format!("Configure {ifname}");
    s.add_layer(closable(
        Dialog::around(ScrollView::new(list).max_height(MAX_H).max_width(MAX_W))
            .title(title)
            .button("Apply", move |s| {
                submit_form(s, &ifn, is_wifi, ex.as_ref(), true)
            })
            .button("Save only", {
                let ifn = ifname.clone();
                let ex = existing.clone();
                move |s| submit_form(s, &ifn, is_wifi, ex.as_ref(), false)
            })
            .button("Remove", {
                let ifn = ifname.clone();
                move |s| remove_config(s, &ifn)
            })
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

fn text_of(s: &mut Cursive, name: &str) -> String {
    s.call_on_name(name, |v: &mut EditView| v.get_content().to_string())
        .unwrap_or_default()
}

fn checked(s: &mut Cursive, name: &str) -> bool {
    s.call_on_name(name, |v: &mut Checkbox| v.is_checked())
        .unwrap_or(false)
}

fn read_form(s: &mut Cursive, ifname: &str, is_wifi: bool) -> (ConfigInput, Option<String>) {
    let wifi = is_wifi.then(|| WifiInput {
        ssid: text_of(s, F_SSID),
        hidden: checked(s, F_HIDDEN),
        security: s
            .call_on_name(F_SECURITY, |v: &mut SelectView<&'static str>| {
                v.selection().map(|r| r.to_string())
            })
            .flatten()
            .unwrap_or_else(|| "wpa2".into()),
        passphrase: text_of(s, F_PASS),
    });
    let is_static = s
        .call_on_name(F_METHOD, |v: &mut SelectView<bool>| {
            v.selection().map(|r| *r)
        })
        .flatten()
        .unwrap_or(false);
    let static_ip = is_static.then(|| (text_of(s, F_ADDRESS), text_of(s, F_GATEWAY)));
    let country = Some(text_of(s, F_COUNTRY).trim().to_ascii_uppercase()).filter(|c| !c.is_empty());
    (
        ConfigInput {
            interface: ifname.to_string(),
            wifi,
            static_ip,
            dns: text_of(s, F_DNS),
        },
        country,
    )
}

fn submit_form(
    s: &mut Cursive,
    ifname: &str,
    is_wifi: bool,
    existing: Option<&NetConfig>,
    apply: bool,
) {
    let (mut input, country) = read_form(s, ifname, is_wifi);
    if let Some(cc) = &country {
        if !wifi::valid_country(cc) {
            dialogs::error(
                s,
                "Invalid Wi-Fi country.",
                &format!("'{cc}': use two letters, e.g. DE."),
            );
            return;
        }
    }
    profile::keep_secret(&mut input, existing);
    let c = match profile::build(&input) {
        Ok(c) => c,
        Err(e) => {
            dialogs::error(s, "The settings are not valid.", &e);
            return;
        }
    };
    let remember = checked(s, F_REMEMBER);
    s.pop_layer();
    // NET-5: a WPA3 password is remembered only with explicit consent.
    let mut after = env(s).map(|e| profile::load_state(&e)).unwrap_or_default();
    after.upsert(c.clone());
    let sae = profile::sae_networks(&after);
    if remember && !sae.is_empty() {
        ask_store_sae(s, &sae, move |s, store_sae| {
            start_form_job(s, c.clone(), country.clone(), apply, remember, store_sae)
        });
    } else {
        start_form_job(s, c, country, apply, remember, false);
    }
}

/// NET-5: ask whether WPA3 passwords may be stored in clear text; `then(s, store)`.
pub fn ask_store_sae<F>(s: &mut Cursive, sae: &[(String, String)], then: F)
where
    F: Fn(&mut Cursive, bool) + Send + Sync + 'static,
{
    let names = sae
        .iter()
        .map(|(i, ssid)| format!("'{ssid}' ({i})"))
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!("WPA3 network(s): {names}\n\n{}", texts::NET_SAE_STORE_QUESTION);
    let then = std::sync::Arc::new(then);
    let t2 = std::sync::Arc::clone(&then);
    s.add_layer(closable(
        Dialog::around(dialogs::scroll_text(text))
            .title("Remember WPA3 password?")
            .button("Store password", move |s| {
                s.pop_layer();
                then(s, true);
            })
            .button("Without password", move |s| {
                s.pop_layer();
                t2(s, false);
            })
            .dismiss_button(texts::BTN_CANCEL),
    ));
}

fn start_form_job(
    s: &mut Cursive,
    c: NetConfig,
    country: Option<String>,
    apply: bool,
    remember: bool,
    store_sae: bool,
) {
    let title = if apply {
        "Apply network settings"
    } else {
        "Save network settings"
    };
    // Remembering writes the overlay partition (one write task at a time, UI-3).
    let kind = if remember {
        TaskKind::Write
    } else {
        TaskKind::ReadOnly
    };
    jobs::run(
        s,
        title,
        kind,
        JOB_DIALOG,
        move |app, ctx| {
            let env = NetEnv::for_backend(app.backend.as_ref());
            ctx.set_message(if apply { "Configuring" } else { "Saving" });
            let mut t = profile::configure(&env, c, country, apply)?.text();
            if apply {
                ctx.set_message("Waiting for the address");
                for _ in 0..12 {
                    ctx.check_cancelled()?;
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                let _ = config::refresh_resolv_conf(&env);
            }
            if remember {
                ctx.set_message("Saving to the overlay partition");
                let state = profile::load_state(&env);
                let r = profile::remember_with(app.backend.as_ref(), &state, store_sae)
                    .map_err(TaskFailure::Error)?;
                t.push_str(&r.text());
            }
            if apply {
                t.push('\n');
                t.push_str(&crate::net::details::text(&crate::net::details::collect(
                    &env,
                )));
            }
            Ok(t)
        },
        move |s, end| show_end(s, title, end),
    );
}

fn remove_config(s: &mut Cursive, ifname: &str) {
    let ifname = ifname.to_string();
    dialogs::confirm(
        s,
        "Remove configuration",
        &format!(
            "Remove the rescue configuration of {ifname} (stops Wi-Fi/DHCP on it)? \
             The remembered settings are not changed; use Network -> Remembered settings."
        ),
        "Remove",
        move |s| {
            s.pop_layer();
            let Some(env) = env(s) else { return };
            match profile::unconfigure(&env, &ifname) {
                Ok(_) => dialogs::info(
                    s,
                    "Configure",
                    &format!("Configuration of {ifname} removed."),
                ),
                Err(e) => dialogs::error(s, "Cannot remove the configuration.", &e),
            }
        },
    );
}

/// NET-2: scan, then put the chosen network into the form.
fn scan(s: &mut Cursive, ifname: &str) {
    let ifname = ifname.to_string();
    jobs::run(
        s,
        "Wi-Fi scan",
        TaskKind::ReadOnly,
        JOB_DIALOG,
        move |app, ctx| {
            ctx.set_message(format!("Scanning on {ifname}"));
            let env = NetEnv::for_backend(app.backend.as_ref());
            wifi::scan(&env, &ifname).map_err(TaskFailure::Error)
        },
        |s, end| match end {
            JobEnd::Ok(list) if list.is_empty() => dialogs::info(
                s,
                "Wi-Fi scan",
                "No networks found. Enter the SSID manually.",
            ),
            JobEnd::Ok(list) => {
                let items = list.iter().map(|e| (e.label(), e.clone())).collect();
                dialogs::choose(s, "Wi-Fi networks", "", items, |s, e: &wifi::ScanEntry| {
                    let ssid = e.ssid.clone();
                    s.call_on_name(F_SSID, |v: &mut EditView| v.set_content(ssid));
                    let sec = match e.security {
                        wifi::ScanSecurity::Open => "open",
                        wifi::ScanSecurity::Sae => "wpa3",
                        _ => "wpa2",
                    };
                    s.call_on_name(F_SECURITY, |v: &mut SelectView<&'static str>| {
                        v.set_selection(security_index(sec))
                    });
                    if e.security == wifi::ScanSecurity::Unsupported {
                        dialogs::info(
                            s,
                            "Wi-Fi",
                            "This network uses enterprise (802.1X) or WEP security, which the \
                             rescue shell does not support.",
                        );
                    }
                });
            }
            JobEnd::Cancelled => {}
            JobEnd::Failed(e) => dialogs::error(s, "The Wi-Fi scan failed.", &e),
        },
    );
}

// --- NET-5: remembered settings ------------------------------------------------------------

fn remembered(s: &mut Cursive) {
    pop_to_base(s);
    jobs::run(
        s,
        "Remembered settings",
        TaskKind::ReadOnly,
        JOB_DIALOG,
        |app, _ctx| {
            let env = NetEnv::for_backend(app.backend.as_ref());
            let current = profile::load_state(&env);
            let rem = profile::load_remembered(app.backend.as_ref());
            Ok((current, rem))
        },
        |s, end| match end {
            JobEnd::Ok((current, rem)) => {
                let lines = |c: &profile::RescueNetConfig| -> String {
                    if c.connections.is_empty() {
                        return "  (none)\n".into();
                    }
                    c.connections
                        .iter()
                        .map(|x| format!("  {}\n", profile::describe(x)))
                        .collect()
                };
                let rem_text = match &rem {
                    Ok(Some(r)) => lines(r),
                    Ok(None) => "  (none)\n".into(),
                    Err(e) => format!("  cannot read: {e}\n"),
                };
                let text = format!(
                    "Settings applied on the next rescue boot (overlay:/{}):\n{rem_text}\n\
                     Current settings:\n{}",
                    profile::remembered_rel().display(),
                    lines(&current)
                );
                let has_current = !current.connections.is_empty();
                s.add_layer(closable(
                    Dialog::around(dialogs::scroll_text(text))
                        .title("Remembered settings (NET-5)")
                        .button("Remember current", move |s| {
                            if !has_current {
                                dialogs::info(s, "Remember", "Configure an interface first.");
                                return;
                            }
                            s.pop_layer();
                            remember_job(s, true);
                        })
                        .button("Forget", |s| {
                            s.pop_layer();
                            remember_job(s, false);
                        })
                        .dismiss_button(texts::BTN_CLOSE),
                ));
            }
            JobEnd::Cancelled => {}
            JobEnd::Failed(e) => dialogs::error(s, "Cannot read the settings.", &e),
        },
    );
}

fn remember_job(s: &mut Cursive, remember: bool) {
    if remember {
        let sae = env(s)
            .map(|e| profile::sae_networks(&profile::load_state(&e)))
            .unwrap_or_default();
        if !sae.is_empty() {
            ask_store_sae(s, &sae, |s, store| remember_job_run(s, true, store));
            return;
        }
    }
    remember_job_run(s, remember, false);
}

fn remember_job_run(s: &mut Cursive, remember: bool, store_sae: bool) {
    let title = if remember {
        "Remember settings"
    } else {
        "Forget settings"
    };
    jobs::run(
        s,
        title,
        TaskKind::Write,
        JOB_DIALOG,
        move |app, _ctx| {
            let env = NetEnv::for_backend(app.backend.as_ref());
            if remember {
                let r = profile::remember_with(app.backend.as_ref(), &profile::load_state(&env), store_sae)
                    .map_err(TaskFailure::Error)?;
                Ok(r.text())
            } else {
                Ok(
                    match profile::forget(app.backend.as_ref()).map_err(TaskFailure::Error)? {
                        true => "The remembered settings were deleted.".into(),
                        false => "There were no remembered settings.".into(),
                    },
                )
            }
        },
        move |s, end| show_end(s, title, end),
    );
}

// --- NET-6 / NET-8: normal system ----------------------------------------------------------

fn use_in_normal_system(s: &mut Cursive) {
    pop_to_base(s);
    let Some(env) = env(s) else { return };
    let state = profile::load_state(&env);
    if state.connections.is_empty() {
        dialogs::info(
            s,
            "Use in normal system",
            "There is no rescue network configuration yet. Configure an interface with \
             Network -> Configure (use \"Save only\" for a network that is not reachable here).",
        );
        return;
    }
    let list: String = state
        .connections
        .iter()
        .map(|c| {
            format!(
                "  {}\n    -> {}\n",
                profile::describe(c),
                nm::keyfile_name(c.interface.as_deref().unwrap_or("?"))
            )
        })
        .collect();
    let text = format!(
        "Write these connections as NetworkManager keyfiles for the normal system \
         (overlay partition, /etc/{}):\n\n{list}\n\
         Existing xoros-<if> connections are replaced; others are not touched.",
        crate::constants::NM_CONNECTIONS_DIR
    );
    dialogs::confirm(s, "Use in normal system", &text, "Write", move |s| {
        let conns = state.connections.clone();
        jobs::run(
            s,
            "Use in normal system",
            TaskKind::Write,
            JOB_DIALOG,
            move |app, ctx| {
                ctx.set_message("Writing NetworkManager keyfiles");
                let files = nm::export(app.backend.as_ref(), &conns).map_err(TaskFailure::Error)?;
                Ok(format!(
                    "Written:\n{}\nThe normal system uses them on its next boot.",
                    files
                        .iter()
                        .map(|f| format!("  overlay:/{}\n", f.display()))
                        .collect::<String>()
                ))
            },
            |s, end| show_end(s, "Use in normal system", end),
        );
    });
}

fn load_from_normal_system(s: &mut Cursive) {
    pop_to_base(s);
    jobs::run(
        s,
        "Load from normal system",
        TaskKind::ReadOnly,
        JOB_DIALOG,
        |app, ctx| {
            ctx.set_message("Reading NetworkManager connections");
            Ok(nm::list_normal_system(app.backend.as_ref()))
        },
        |s, end| match end {
            JobEnd::Ok((list, notes)) => {
                let mut text: String = notes.iter().map(|n| format!("Note: {n}\n")).collect();
                if list.is_empty() {
                    text.push_str("No NetworkManager connections found in the normal system.");
                    dialogs::info(s, "Load from normal system", &text);
                    return;
                }
                text.push_str("Ethernet and Wi-Fi (open/WPA2/WPA3) connections can be used; others are read-only.");
                let items = list
                    .into_iter()
                    .map(|p| (crate::report::ellipsize(&p.label(), MAX_W - 4), p))
                    .collect();
                dialogs::choose(s, "Load from normal system", &text, items, pick_nm_profile);
            }
            JobEnd::Cancelled => {}
            JobEnd::Failed(e) => dialogs::error(s, "Cannot read the normal system.", &e),
        },
    );
}

fn pick_nm_profile(s: &mut Cursive, p: &nm::NmProfile) {
    let c = match &p.config {
        Ok(c) => c.clone(),
        Err(e) => {
            dialogs::info(
                s,
                &p.file,
                &format!("This connection cannot be used in rescue:\n{e}"),
            );
            return;
        }
    };
    let open_form =
        move |s: &mut Cursive, ifname: &str, wireless: bool| match crate::cli_net::import_as(
            c.clone(),
            ifname,
        ) {
            Ok(imported) => {
                let input = profile::input_from(&imported);
                let country = env(s).and_then(|e| profile::load_state(&e).country);
                config_form(s, input, wireless, Some(imported), country);
            }
            Err(e) => dialogs::error(s, "The connection cannot be used.", &e),
        };
    let Some(env) = env(s) else { return };
    match p.config.as_ref().ok().and_then(|c| c.interface.clone()) {
        Some(ifname) => {
            let wireless = interfaces(&env).iter().any(|(n, w)| *n == ifname && *w);
            open_form(s, &ifname, wireless);
        }
        None => pick_interface(s, &env, "Use connection on", open_form),
    }
}
