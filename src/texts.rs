//! User-facing texts (UI-6), kept in one place for later translation.

pub const MENU_STATUS: &str = "Status";
pub const MENU_BOOT: &str = "Boot";
pub const MENU_UPDATE: &str = "Update";
pub const MENU_BACKUP: &str = "Backup";
pub const MENU_NETWORK: &str = "Network";
pub const MENU_TESTS: &str = "Tests";
pub const MENU_TOOLS: &str = "Tools";
pub const MENU_POWER: &str = "Power";

pub const STATUS_BAR_KEYS: &str = "Esc:menu Tab:move Enter:select q:close r:refresh";

pub const TITLE_STATUS: &str = "Status";
pub const TITLE_HARDWARE: &str = "Hardware";
pub const TITLE_STORAGE: &str = "Storage";
pub const TITLE_BOOT: &str = "Boot control";
pub const TITLE_ERROR: &str = "Error";
pub const TITLE_CONFIRM: &str = "Confirm";
pub const TITLE_NOT_IMPLEMENTED: &str = "Not implemented yet";
pub const TITLE_TASKS: &str = "Background tasks";
pub const TITLE_ABOUT: &str = "About";

pub const BTN_OK: &str = "OK";
pub const BTN_CLOSE: &str = "Close";
pub const BTN_CANCEL: &str = "Cancel";
pub const BTN_REFRESH: &str = "Refresh";
pub const BTN_YES: &str = "Yes";
pub const BTN_NO: &str = "No";
pub const BTN_RAW: &str = "Raw view";
pub const BTN_SEARCH: &str = "Search";
pub const BTN_CHECK_AGAIN: &str = "Check rescue";

pub const CTRL_C_HINT: &str = "Ctrl-C does not exit the rescue shell.\n\n\
Use Power -> Reboot / Power off, or Tools -> Subshell for a command line.";

pub fn not_implemented(feature: &str) -> String {
    format!("{feature} is not implemented yet in this version of the rescue shell.")
}

pub const RESET_EXPLANATION: &str = "Resetting a try counter to 0 makes GRUB try that slot \
again on the next boot (it is skipped while the counter is at MAX_TRIES).";

pub const NEXT_BOOT_EXPLANATION: &str = "NEXT_BOOT boots the chosen entry once, \
regardless of the try counters; GRUB clears it afterwards.";

pub const REBOOT_INTO_EXPLANATION: &str = "Resets the slot's try counter, sets NEXT_BOOT \
to the slot and reboots now.";

pub const ROTATION_EXPLANATION: &str = "Console rotation (ROTATION) is applied by GRUB on \
the next boot and to the running console now if possible.";

pub const GRUBENV_BROKEN: &str = "grubenv is missing or broken. GRUB cannot save boot \
counters. Recreate it with default values?";

pub const SUBSHELL_INFO: &str = "Starts /bin/sh as root on this terminal. Type 'exit' to \
return to the rescue shell.";

pub const TASK_RESCUE_HASH: &str = "Rescue image SHA-256";

pub const SIMULATION_NOTE: &str = "SIMULATION";

pub const RESTART_APP: &str = "Exit and restart the rescue shell? (On the device systemd \
starts it again; background tasks are cancelled.)";

pub const TITLE_EXPORT: &str = "Export backup to stick";
pub const TITLE_IMPORT: &str = "Import backup from stick";
pub const TITLE_CONFIG_RESET: &str = "Reset configuration";
pub const TITLE_FULL_RESET: &str = "Full factory reset";

pub const TASK_EXPORT: &str = "Backup export";
pub const TASK_IMPORT: &str = "Backup import";
pub const TASK_VERIFY: &str = "Backup verification";

pub const EXPORT_INTRO: &str = "Back up the persistent partitions to a USB stick. overlay \
holds the changes to /etc of the normal system, application holds /opt (apps, Docker data).";

pub const NO_STICK: &str = "No external stick found. Insert a USB stick (FAT32, exFAT or \
ext4) and choose Refresh.";

pub const MODE_REPLACE: &str = "Replace: delete the current content first (default)";
pub const MODE_MERGE: &str = "Merge: extract over the current content";

pub const RESCUE_KEPT_NOTE: &str = "The rescue shell settings (overlay: rescue/) are kept as \
they are and are not restored from the backup.";

pub const RESET_OFFER_EXPORT: &str = "A USB stick is present. Export a backup of the \
persistent partitions before resetting?";

pub const CONFIG_RESET_EXPLANATION: &str = "Deletes all changes to /etc of the normal system \
(content of the overlay partition). The normal system then starts with the /etc shipped in \
its slot. Data on the application partition is kept.";

pub const FULL_RESET_EXPLANATION: &str = "Deletes all changes to /etc of the normal system \
(overlay partition) AND all data on the application partition (/opt: apps, Docker data, \
user data).";

// --- Tests menu (TST-*) ---

pub const TITLE_TEST_RESULTS: &str = "Test results";
pub const TITLE_MEMTEST: &str = "Memory test";
pub const TITLE_DISKTEST: &str = "Disk read test";
pub const TITLE_SLOTTEST: &str = "Slot integrity";
pub const TITLE_RESCUETEST: &str = "Rescue integrity";
pub const TITLE_FSCK: &str = "File system check";
pub const BTN_START: &str = "Start";
pub const BTN_REPAIR: &str = "Repair...";

pub const MEMTEST_NOTE: &str = "Note: a userspace memory test cannot test the memory used by \
the kernel and by the rescue shell itself. For a complete test use memtest86+ if the device \
offers it.";

pub const MEMTEST_INTRO: &str = "Allocates and locks a share of the free memory and runs \
solid, checkerboard, walking ones/zeros, address, random and bit-flip patterns. One pass \
takes a few minutes per GiB.";

pub const DISKTEST_INTRO: &str = "Reads the selected area sequentially (O_DIRECT, 1 MiB \
blocks) and reports read errors, slow blocks and the throughput. Nothing is written.";

pub const SLOTTEST_INTRO: &str = "Checks the squashfs superblock of each slot, mounts it \
read-only and reads every file, and compares the SHA-256 with the OTA manifest of the \
slot's build when one is found on a stick or the OTA server.";

pub const FSCK_INTRO: &str = "Runs e2fsck -f -n (check only, nothing is changed) on the \
selected persistent partitions. They are unmounted first if the rescue shell mounted them.";

pub const FSCK_REPAIR_CONFIRM: &str = "Run e2fsck -f -y on the partitions with errors? \
e2fsck answers yes to every question: damaged files may be moved to lost+found or deleted. \
Consider exporting a backup first (Backup -> Export).";

pub const FSCK_REPAIR_WORD: &str = "REPAIR";

pub const SSH_INFO: &str = "Remote support: starts sshd with a one-time root password. \
Anyone on the network who knows the password can log in as root. Stop it when done; it also \
stops when the rescue shell exits. Public keys can be loaded from 'authorized_keys' on a stick.";

// --- shell-b: TST-2, TST-7, TST-8, TST-9, NET-4 speed test, NET-5 WPA3 ---

pub const TITLE_MEMTEST86: &str = "Reboot into memtest86+";

pub const MEMTEST86_MISSING: &str = "This device has no memtest86+ boot entry (GRUB menu id \
MENU_MEMTEST). It is only installed on x86 images; use Tests -> Memory test instead.";

pub const MEMTEST86_CONFIRM: &str = "Reboot into memtest86+ now? It tests all memory, \
including the memory the kernel uses here, and runs until you stop it. Then switch the device \
off or press Esc in memtest86+ to reboot; the next boot is a normal one.";

pub const TITLE_HEALTH: &str = "Disk health";

pub const HEALTH_INTRO: &str = "Reads the SMART / NVMe health data (smartctl, nvme-cli or \
the NVMe log page) and what sysfs offers (eMMC wear, I/O error counters). Nothing is written. \
USB sticks and SD cards usually have no SMART data.";
