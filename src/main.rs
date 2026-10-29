//! Entry point: argument parsing, logging, panic hook, then TUI or CLI (section 9.2).

use std::process::ExitCode;

use xoros_rescue_shell::app::{make_backend, sim_dir, App};
use xoros_rescue_shell::backend::sim::SimBackend;
use xoros_rescue_shell::cli::{
    parse_args, run_backup, run_diag, run_net, run_report, run_ssh, run_status, run_test, run_time,
    run_update, usage, Command,
};
use xoros_rescue_shell::constants::{APP_NAME, APP_VERSION};
use xoros_rescue_shell::{cli_admin, logging, terminal, ui};

fn main() -> ExitCode {
    let opts = match parse_args(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n\n{}", usage());
            return ExitCode::from(2);
        }
    };
    match opts.command {
        Command::Help => {
            print!("{}", usage());
            return ExitCode::SUCCESS;
        }
        Command::Version => {
            println!("{APP_NAME} {APP_VERSION}");
            return ExitCode::SUCCESS;
        }
        Command::SimInit(dir) => {
            return match SimBackend::create_example(&dir) {
                Ok(()) => {
                    println!("Simulation directory created: {}", dir.display());
                    println!("Run: xoros-rescue-shell --sim {}", dir.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Tui
        | Command::Status { .. }
        | Command::Backup(_)
        | Command::Update(_)
        | Command::Net(_)
        | Command::Time(_)
        | Command::Test(_)
        | Command::Report(_)
        | Command::Diag(_)
        | Command::Ssh(_)
        | Command::Boot(_)
        | Command::Power(_)
        | Command::Settings(_)
        | Command::Stick(_) => {}
    }

    let backend = match make_backend(sim_dir(opts.sim)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let tui = opts.command == Command::Tui;
    logging::init(&backend.run_dir(), log::LevelFilter::Info, tui);
    terminal::save_startup_state();
    terminal::install_panic_hook();
    log::info!("{APP_NAME} {APP_VERSION} starting ({})", backend.describe());

    // CFG-1: settings from the overlay partition (defaults when it is missing or broken).
    xoros_rescue_shell::settings::install(xoros_rescue_shell::settings::load(backend.as_ref()));
    let app = App::new(backend);
    match opts.command {
        Command::Status { json, hash } => match run_status(&app, json, hash) {
            0 => ExitCode::SUCCESS,
            _ => ExitCode::FAILURE,
        },
        Command::Backup(args) => exit(run_backup(&app, &args)),
        Command::Update(args) => exit(run_update(&app, &args)),
        Command::Net(args) => exit(run_net(&app, &args)),
        Command::Time(args) => exit(run_time(&app, &args)),
        Command::Test(args) => ExitCode::from(run_test(&app, &args).clamp(0, 255) as u8),
        Command::Report(args) => exit(run_report(&app, &args)),
        Command::Diag(args) => exit(run_diag(&app, &args)),
        Command::Ssh(args) => exit(run_ssh(&app, &args)),
        Command::Boot(args) => exit(cli_admin::run_boot(&app, &args)),
        Command::Power(args) => exit(cli_admin::run_power(&app, &args)),
        Command::Settings(args) => exit(cli_admin::run_settings(&app, &args)),
        Command::Stick(args) => exit(cli_admin::run_stick(&app, &args)),
        _ => match ui::run(app) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                terminal::restore_terminal();
                log::error!("UI error: {e}");
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

fn exit(code: i32) -> ExitCode {
    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
