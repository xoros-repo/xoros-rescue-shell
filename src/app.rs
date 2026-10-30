//! Application state shared by the UI and the CLI (no ratatui, ARCH-1).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::backend::core::CoreBackend;
use crate::backend::sim::SimBackend;
use crate::backend::unavailable::UnavailableBackend;
use crate::backend::{BackendError, SystemBackend};
use crate::constants::SIM_ENV_VAR;
use crate::hwtest::{ResultStore, TestKind};
use crate::slots::{start_rescue_check, RescueCheck};
use crate::status::{collect_status, Reason, SystemStatus};
use crate::tasks::{lock, OnFinish, SpawnError, TaskHandle, TaskRunner};

pub struct App {
    pub backend: Arc<dyn SystemBackend>,
    pub tasks: TaskRunner,
    pub rescue_check: Arc<Mutex<RescueCheck>>,
    /// Hardware test results of this boot (TST-*).
    pub test_results: Arc<ResultStore>,
    /// Result of the last background "Internet reachable" check (SYS-1); `None` = not yet.
    pub internet: Arc<Mutex<Option<bool>>>,
    /// Why rescue was entered, captured the first time the boot state was readable, so
    /// later counter changes in this session do not change the answer (SYS-1).
    boot_reason: Mutex<Option<Reason>>,
}

/// Simulation directory from `--sim` or the environment (SIM-1).
pub fn sim_dir(cli: Option<PathBuf>) -> Option<PathBuf> {
    cli.or_else(|| {
        std::env::var_os(SIM_ENV_VAR)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    })
}

/// Choose the backend: simulation if a directory is given, otherwise the real system through
/// `xoros-core`. If the boot disk or its layout cannot be detected, a fallback backend reports
/// that error for every device operation (the UI still starts).
pub fn make_backend(sim: Option<PathBuf>) -> Result<Arc<dyn SystemBackend>, BackendError> {
    match sim {
        Some(dir) => Ok(Arc::new(SimBackend::open(dir)?)),
        None => {
            let core = CoreBackend::system();
            match core.probe() {
                Ok(_) => Ok(Arc::new(core)),
                Err(e) => {
                    log::error!("boot disk detection failed: {e}");
                    Ok(Arc::new(UnavailableBackend::new(e.to_string())))
                }
            }
        }
    }
}

impl App {
    pub fn new(backend: Arc<dyn SystemBackend>) -> App {
        App {
            test_results: Arc::new(ResultStore::new(&backend.run_dir())),
            backend,
            tasks: TaskRunner::new(),
            rescue_check: Arc::new(Mutex::new(RescueCheck::NotChecked)),
            internet: Arc::new(Mutex::new(None)),
            boot_reason: Mutex::new(None),
        }
    }

    /// Start (or restart) the rescue image check.
    pub fn start_rescue_check(
        &self,
        on_finish: Option<OnFinish>,
    ) -> Result<TaskHandle, SpawnError> {
        start_rescue_check(
            Arc::clone(&self.backend),
            &self.tasks,
            Arc::clone(&self.rescue_check),
            on_finish,
        )
    }

    /// NET-5/NET-1: the remembered network configuration (if any) and DHCP on the other
    /// Ethernet interfaces in the background at start-up, then the app's `resolv.conf`
    /// (NET-3). In simulation mode only files below `<sim>/run` are written.
    pub fn start_network_setup(&self) {
        let env = crate::net::NetEnv::for_backend(self.backend.as_ref());
        let backend = Arc::clone(&self.backend);
        let internet = Arc::clone(&self.internet);
        let spawned = std::thread::Builder::new()
            .name("net-setup".into())
            .spawn(move || {
                for line in crate::net::profile::startup(&env, backend.as_ref()) {
                    log::info!("{line}");
                }
                // Give DHCP some time, then pick up the DNS servers.
                for _ in 0..3 {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    if let Ok(v) = crate::net::config::refresh_resolv_conf(&env) {
                        if !v.is_empty() {
                            break;
                        }
                    }
                }
                // SYS-1: keep "Internet reachable" current while the app runs.
                loop {
                    *lock(&internet) = Some(crate::reach::internet_reachable(&env));
                    std::thread::sleep(std::time::Duration::from_secs(30));
                }
            });
        if let Err(e) = spawned {
            log::warn!("network setup not started: {e}");
        }
    }

    pub fn status(&self) -> SystemStatus {
        let check = lock(&self.rescue_check).clone();
        let mut st = collect_status(self.backend.as_ref(), check);
        let mut cached = lock(&self.boot_reason);
        match cached.as_ref() {
            Some(r) => st.reason = r.clone(),
            None if st.boot.is_some() => *cached = Some(st.reason.clone()),
            None => {}
        }
        st.network.internet_reachable = *lock(&self.internet);
        // SYS-1: a file system check of this session beats the superblock information.
        let results = self.test_results.all();
        for p in st.persistent.iter_mut() {
            let last = results.iter().rev().find(|r| {
                matches!(r.kind, TestKind::FsCheck | TestKind::FsRepair)
                    && (r.target.contains(&p.label) || r.details.contains(&p.label))
            });
            if let Some(r) = last {
                p.fsck = Some(format!("{}: {} ({} UTC)", r.kind.title(), r.outcome, r.started));
            }
        }
        st
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Slot;
    use crate::hw::testutil::TempDir;
    use crate::status::ReasonCode;

    #[test]
    fn reason_is_kept_for_the_session() {
        let d = TempDir::new("app");
        SimBackend::create_example(d.path()).expect("example");
        let app = App::new(make_backend(Some(d.path().to_path_buf())).expect("backend"));
        assert_eq!(app.status().reason.code, ReasonCode::SlotsExhausted);
        app.backend.reset_tries(Slot::A).expect("reset");
        let st = app.status();
        assert_eq!(st.reason.code, ReasonCode::SlotsExhausted);
        assert_eq!(st.boot.map(|b| b.tries), Some([0, 3]));
    }
}
