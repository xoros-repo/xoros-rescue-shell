//! Fallback backend when the boot disk or the XorOS layout cannot be detected (DEV-1, DEV-2),
//! e.g. when the app is started on a development PC without `--sim`.
//!
//! It reports every boot/device operation as failing with the detection error (so nothing on
//! the device is touched) and only reads harmless information (os-release, hostname) itself.

use std::path::PathBuf;

use super::{
    BackendError, BlockDevice, BootPartition, BootState, BootTarget, BuildInfo, ErrorKind, FsUsage, Layout,
    MountGuard, PartRole, PartitionWriter, ProgressFn, RescueReason, RescueSystemInfo, Result, Slot, Stick,
    SystemBackend,
};
use crate::backup::tools::{CommandRunner, SystemRunner};
use crate::constants::{OS_RELEASE, RUN_DIR};
use crate::hw::HostPaths;
use crate::tasks::CancelToken;

/// Device access is disabled; `reason` says why (shown in every error and the status bar).
pub struct UnavailableBackend {
    reason: String,
}

impl UnavailableBackend {
    pub fn new(reason: impl Into<String>) -> Self {
        UnavailableBackend {
            reason: reason.into(),
        }
    }

    fn na<T>(&self) -> Result<T> {
        Err(BackendError::new(
            ErrorKind::NotAvailable,
            format!("Device access is not available: {}", self.reason),
        ))
    }
}

impl SystemBackend for UnavailableBackend {
    fn describe(&self) -> String {
        format!("device not detected: {}", self.reason)
    }
    fn is_simulation(&self) -> bool {
        false
    }
    fn run_dir(&self) -> PathBuf {
        PathBuf::from(RUN_DIR)
    }
    fn host_paths(&self) -> HostPaths {
        HostPaths::default()
    }
    fn boot_state(&self) -> Result<BootState> {
        self.na()
    }
    fn current_boot(&self) -> Result<BootTarget> {
        self.na()
    }
    fn predict(&self) -> Result<BootTarget> {
        self.na()
    }
    fn rescue_reason(&self) -> Result<RescueReason> {
        self.na()
    }
    fn reset_tries(&self, _slot: Slot) -> Result<()> {
        self.na()
    }
    fn disable(&self, _slot: Slot) -> Result<()> {
        self.na()
    }
    fn set_next_boot(&self, _target: Option<BootTarget>) -> Result<()> {
        self.na()
    }
    fn set_order(&self, _first: Slot) -> Result<()> {
        self.na()
    }
    fn set_max_tries(&self, _max_tries: u8) -> Result<()> {
        self.na()
    }
    fn set_rotation(&self, _rotation: u8) -> Result<()> {
        self.na()
    }
    fn recreate_grubenv(&self) -> Result<()> {
        self.na()
    }
    fn slot_info(&self, _slot: Slot) -> Result<Option<BuildInfo>> {
        self.na()
    }
    fn rescue_system(&self) -> Result<RescueSystemInfo> {
        Ok(crate::sysinfo::rescue_system_info(std::path::Path::new(
            OS_RELEASE,
        )))
    }
    fn block_devices(&self) -> Result<Vec<BlockDevice>> {
        self.na()
    }
    fn boot_disk(&self) -> Result<BlockDevice> {
        self.na()
    }
    fn layout(&self) -> Result<Layout> {
        self.na()
    }
    fn partition_usage(&self, _role: PartRole) -> Result<Option<FsUsage>> {
        self.na()
    }
    fn hash_image(
        &self,
        _role: PartRole,
        _progress: ProgressFn<'_>,
        _cancel: &CancelToken,
    ) -> Result<String> {
        self.na()
    }
    fn external_sticks(&self) -> Result<Vec<Stick>> {
        self.na()
    }
    fn mount_stick(&self, _stick: &Stick, _writable: bool) -> Result<MountGuard> {
        self.na()
    }
    fn mount_partition(&self, _role: PartRole, _writable: bool) -> Result<MountGuard> {
        self.na()
    }
    fn command_runner(&self) -> Box<dyn CommandRunner> {
        Box::new(SystemRunner::default())
    }
    fn boot_partition(&self) -> Result<BootPartition> {
        self.na()
    }
    fn open_slot_writer(&self, _slot: Slot) -> Result<PartitionWriter> {
        self.na()
    }
    fn open_slot_reader(&self, _slot: Slot) -> Result<std::fs::File> {
        self.na()
    }
}
