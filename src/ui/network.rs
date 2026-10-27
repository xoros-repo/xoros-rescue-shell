//! Network menu: Ethernet DHCP (NET-1), test (NET-4), date and time (NET-7). Wi-Fi and static
//! configuration (NET-2) and copying to the normal system (NET-6) come later.

use std::time::SystemTime;

use cursive::menu::Tree;
use cursive::traits::*;
use cursive::views::{Dialog, EditView, LinearLayout, TextView};
use cursive::Cursive;

use crate::constants::{DEFAULT_HTTP_PROXY, DEFAULT_OTA_BASE_URL};
use crate::net::{config, diag, http, time, NetEnv};
use crate::tasks::{TaskFailure, TaskKind};
use crate::texts;
use crate::ui::dialogs::{self, closable, MAX_W};
use crate::ui::jobs::{self, JobEnd};
use crate::ui::{app, pop_to_base, todo_leaf};

const JOB_DIALOG: &str = "network_job_progress";
const TIME_INPUT: &str = "manual_time_input";

pub fn menu() -> Tree {
    let t = Tree::new()
        .leaf("Ethernet DHCP", dhcp)
        .leaf("Test", test)
        .leaf("Date and time", date_time);
    let t = todo_leaf(t, "Configure", "Wi-Fi and static configuration (NET-2)");
    todo_leaf(
        t,
        "Use in normal system",
        "Copying settings to the normal system (NET-6)",
    )
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
    let url = match &dev.target {
        Some(t) => format!(
            "{}/manifest.json",
            http::ota_dir_url(DEFAULT_OTA_BASE_URL, t)
        ),
        None => DEFAULT_OTA_BASE_URL.to_string(),
    };
    let opts = diag::TestOptions {
        url,
        proxy: Some(DEFAULT_HTTP_PROXY.to_string()).filter(|p| !p.is_empty()),
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
            Ok::<_, TaskFailure>(diag::report_text(&steps))
        },
        |s, end| show_end(s, "Network test", end),
    );
}

fn time_text() -> String {
    let now = SystemTime::now();
    let mut t = format!("Current time: {}\n", time::format_utc(now));
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
                match time::parse_manual(&text).and_then(|t| {
                    time::set_system_time(&env, t)?;
                    Ok(t)
                }) {
                    Ok(t) => {
                        s.pop_layer();
                        dialogs::info(
                            s,
                            "Date and time",
                            &format!("Clock set to {}.", time::format_utc(t)),
                        );
                    }
                    Err(e) => dialogs::error(s, "Cannot set the time.", &e),
                }
            })
            .dismiss_button(texts::BTN_CLOSE),
    ));
}
