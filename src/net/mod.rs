//! Rescue-side networking (NET-*, SYS-4, SSH-1) without ratatui (ARCH-1).
//!
//! - [`info`]: interface summary for the status screen;
//! - [`config`]: automatic DHCP on Ethernet (`.network` files for systemd-networkd, `udhcpc`
//!   fallback) and the app's `resolv.conf`;
//! - [`dns`]: a stub resolver with an explicit server list, [`http`]: the app's HTTP client and
//!   OTA image source using it;
//! - [`sntp`] and [`time`]: time sync and manual time setting;
//! - [`diag`]: the network test, [`speed`]: the download speed test (NET-4 P2);
//! - [`profile`]: per-interface configuration (DHCP/static, Wi-Fi via [`wifi`]) and the
//!   remembered configuration (NET-2, NET-5);
//! - [`nm`]: NetworkManager keyfiles for the normal system (NET-6, NET-8);
//! - [`details`]: the network details screen (SYS-4);
//! - [`ssh`]: remote support (SSH-1).
//!
//! Every effect on the system goes through a [`NetEnv`]: on a device files are written under
//! `/run` and tools are run; in simulation mode (SIM-1) files go to `<sim>/run/...` and tool
//! calls and clock changes are only logged. Read-only checks (sockets, DNS, HTTP, ping) run
//! against the real host network in both modes.

pub mod config;
pub mod details;
pub mod diag;
pub mod dns;
pub mod http;
pub mod info;
pub mod nm;
pub mod profile;
pub mod sntp;
pub mod speed;
pub mod ssh;
pub mod time;
pub mod wifi;

use std::path::PathBuf;

use crate::backend::SystemBackend;
use crate::cmd::{self, ToolError};
use crate::constants::{NETWORKD_LEASES_DIR, NETWORKD_RUN_DIR, RESOLV_CONF_NAME};
use crate::hw::HostPaths;

/// Where network configuration goes and whether system changes are really made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetEnv {
    /// Runtime directory (`/run/xoros-rescue` or `<sim>/run`): `resolv.conf`, udhcpc script.
    pub run_dir: PathBuf,
    /// systemd-networkd runtime config directory (`/run/systemd/network` or
    /// `<sim>/run/systemd/network`).
    pub networkd_dir: PathBuf,
    /// systemd-networkd DHCP leases (read only; the host's in simulation mode).
    pub leases_dir: PathBuf,
    /// `/etc/resolv.conf` of the host (read only, fallback for the server list).
    pub etc_resolv_conf: PathBuf,
    /// procfs/sysfs roots (interfaces, routes).
    pub host: HostPaths,
    /// Simulation: tools and clock changes are logged, not executed.
    pub simulation: bool,
}

impl NetEnv {
    pub fn for_backend(b: &dyn SystemBackend) -> NetEnv {
        let run_dir = b.run_dir();
        let networkd_dir = if b.is_simulation() {
            run_dir.join("systemd/network")
        } else {
            PathBuf::from(NETWORKD_RUN_DIR)
        };
        NetEnv {
            networkd_dir,
            leases_dir: PathBuf::from(NETWORKD_LEASES_DIR),
            etc_resolv_conf: PathBuf::from("/etc/resolv.conf"),
            host: b.host_paths(),
            simulation: b.is_simulation(),
            run_dir,
        }
    }

    /// The app's own resolver configuration (NET-3).
    pub fn resolv_conf(&self) -> PathBuf {
        self.run_dir.join(RESOLV_CONF_NAME)
    }

    /// Run a tool that changes the system; only logged in simulation mode (SIM-1).
    pub fn run_tool(&self, tool: &str, args: &[&str]) -> Result<(), ToolError> {
        if self.simulation {
            log::info!("SIM: would run '{tool} {}'", args.join(" "));
            return Ok(());
        }
        cmd::run_logged(tool, args)
    }

    /// Run a read-only query tool (e.g. `systemctl is-active`); `None` in simulation mode.
    pub fn query_tool(&self, tool: &str, args: &[&str]) -> Option<Result<String, ToolError>> {
        if self.simulation {
            log::info!("SIM: would query '{tool} {}'", args.join(" "));
            return None;
        }
        Some(cmd::run_capture(tool, args))
    }
}
