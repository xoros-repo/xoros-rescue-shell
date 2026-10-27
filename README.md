# Xoros Rescue Shell

## Description

Xoros Rescue Shell is a console (TUI) app that allows you to interact with the Xoros Rescue System.

It is designed to provide all necessary tools to manage the system and to perform system recovery operations.

It is written in Rust on top of [cursive](https://github.com/gyscos/cursive) (crossterm backend) and is packaged
for Yocto/OpenEmbedded via `cargo-bitbake` recipes.

## Status

Version `0.1.1-2`, milestones M1 and M2 (partly) of `xoros-distro/.../rescue-shell/REQUIREMENTS.md` (IDs in brackets):

- **Status** screen with the reason for rescue, both slots, rescue image check, persistent partitions,
  network and time [SYS-1]; hardware details [SYS-2]; storage [SYS-3].
- **Boot** control: counters, one-shot next boot, reset + reboot into slot, order, MAX_TRIES, disable slot,
  rotation, raw grubenv view, recreate grubenv [BOOT-1..7, ROT-1, ENV-2].
- **Power**: reboot, reboot into slot A/B/rescue, power off, restart the app [PWR-1, PWR-2].
- **Tools**: log viewer with search (app log, journal, dmesg) [LOG-1, LOG-3], background tasks, subshell [SH-1].
- **Tests**: rescue image SHA-256 as a background task with progress and cancel [UI-3].
- **Backup**: export the persistent partitions to a stick (split on FAT), import with mandatory
  checksum verification (refused with the exact reason otherwise), Replace/Merge, summary with
  per-file errors; configuration reset and full factory reset with typed `RESET`
  [BAK-1..5, BAK-4a, RST-1..3]. The rescue shell's own `rescue/` directory on the overlay is
  kept on import and (by default) on reset.
- **Update** (M2): from the network (OTA server, base URL/version/machine/suffix/proxy in the
  dialog) and from a USB stick (OTA tree or single `*.squashfs` with sidecars): manifest check,
  newer/same/older per slot with downgrade confirmation, slot A, B or A+B (A written, then copied
  to B and verified; typed `WRITE`), progress with cancel, resume (chunks already on the slot are
  skipped), retry after a failure, rescue images listed but never written, stick unmounted and
  synced afterwards [OTA-1..7, OTA-9..11, USB-2..5]. Signatures: status only ("unsigned" /
  "not checked") until trusted keys come with the settings file (OTA-8, CFG-1).
- **Network** (M2): Ethernet DHCP automatically at start (`.network` files in
  `/run/systemd/network` + `networkctl reload`, busybox `udhcpc` fallback) [NET-1]; the app's own
  `resolv.conf` and DNS resolver (no libc resolver for its own requests) [NET-3]; network test
  (carrier, address, gateway, DNS, TCP, HTTPS manifest GET, clock) [NET-4]; SNTP sync, manual
  time, automatic clock check before HTTPS [NET-7, OTA-10].
- Wi-Fi/static configuration, copying settings to the normal system and the other tests show
  "not implemented yet".

Device access (grubenv, boot disk detection, slot build info, sticks, partition mounts) goes
through the `SystemBackend` trait in `src/backend/`: `CoreBackend` on a device (shared crate
`xoros-core`), `SimBackend` in simulation mode. If the boot disk or the XorOS layout cannot be
detected, the app starts anyway and every device function reports that error.

`xoros-core` is currently a path dependency on a sibling checkout (`../xoros-xor-tool`); the
release form is the pinned SSH git dependency noted in `Cargo.toml` (CORE-1).

## Usage

| Key      | Action                                   |
|----------|------------------------------------------|
| `Esc`    | Open the menu bar / close a dialog       |
| `q`      | Close a dialog                           |
| `r`      | Refresh the Status screen                |
| `Ctrl-C` | Shows a hint (does not exit)             |

The app is restarted by systemd when it exits (Power -> Restart rescue shell).

### Simulation mode

```shell
cargo run -- --sim-init sim          # example: both slots exceeded MAX_TRIES
cargo run -- --sim sim               # TUI against sim/sim.json (or XOROS_RESCUE_SIM=sim)
cargo run -- --sim sim --cli status --json
```

`sim/sim.json` describes slot build info, disks, partition usage and optional fault injection
(`"fail": {"slot_info.B": "I/O error"}`). grubenv is a real 1024-byte file
`sim/boot/EFI/BOOT/grubenv` handled by xoros-core (delete it to simulate a missing grubenv; an
old `sim.json` with a `grubenv` object is migrated on open). Slot updates write
`sim/rootfs_a.img` / `rootfs_b.img` (created sparse when missing) through the crate's guarded
writer; the new build info is recorded in `sim.json`. `sim/rescue.img` is hashed by the rescue check.
Power actions only log. The log is `sim/run/rescue-shell.log` (`/run/xoros-rescue/rescue-shell.log` on a device).
External sticks are directories `sim/sticks/<name>/` (`"stick_fs": {"usb1": "vfat"}` makes one count as FAT),
the persistent partitions are `sim/parts/overlay/` and `sim/parts/application/`; `e2fsck` is only logged
(`"fail": {"e2fsck": "..."}` simulates a failing check).

### Trying updates in simulation mode

`--sim-init` also writes an OTA tree `sim/ota/1.0/qemux86-64/devel/` (8 MiB rootfs + rescue image,
newer than both example slots), the same tree on the stick (`sim/sticks/usb1/xoros/ota/...`) and a
single image `sim/sticks/usb1/images/rootfs.squashfs` with `.sha256`/`.json` sidecars.

```shell
python3 -m http.server -d sim/ota 8000 &                    # the "OTA server"
cargo run -- --sim sim --cli update check --base http://127.0.0.1:8000
cargo run -- --sim sim --cli update start --slot B --base http://127.0.0.1:8000
cargo run -- --sim sim --cli update scan usb1
cargo run -- --sim sim --cli update start --slot AB --confirm WRITE --stick usb1 --path xoros/ota/1.0/qemux86-64/devel
cargo run -- --sim sim                                      # TUI: Update -> From network, server http://127.0.0.1:8000
```

Network actions in simulation mode write below `sim/run/` (`systemd/network/*.network`,
`resolv.conf`) and only log tool calls and clock changes; the network test and SNTP queries use
the host's network.

### CLI

```shell
xoros-rescue-shell [--sim <dir>] --cli status [--json] [--no-hash]
xoros-rescue-shell [--sim <dir>] --cli backup sticks
xoros-rescue-shell [--sim <dir>] --cli backup list <stick>
xoros-rescue-shell [--sim <dir>] --cli backup export <stick> [overlay] [application]
xoros-rescue-shell [--sim <dir>] --cli backup verify <stick> <name>
xoros-rescue-shell [--sim <dir>] --cli backup import <stick> <name> [--merge] [overlay] [application]
xoros-rescue-shell [--sim <dir>] --cli backup reset config|full --confirm RESET [--reset-rescue]
xoros-rescue-shell [--sim <dir>] --cli update check [<source>]
xoros-rescue-shell [--sim <dir>] --cli update scan <stick>
xoros-rescue-shell [--sim <dir>] --cli update start --slot A|B|AB [<source>] [--image <timestamp>]
                   [--allow-downgrade] [--allow-unverified] [--no-resume] [--confirm WRITE]
xoros-rescue-shell [--sim <dir>] --cli net up | dns | test [--base <url>] [--proxy <url>]
xoros-rescue-shell [--sim <dir>] --cli time show | sync [<server>...] | set 'YYYY-MM-DD HH:MM[:SS]'
```

`<source>`: `--base <url> [--proxy <url>] [--version <v>] [--machine <m>] [--suffix <s>]` (default:
`https://ota.xoros.io` and the newest valid slot) or `--stick <id> --path <dir or image>`.

## Menu Structure

```
Status   Overview | Hardware | Storage | Network details* | Save system report*
Boot     Boot control | Reset try counter | Set next boot | Reset counter and reboot into slot |
         Slot order | MAX_TRIES | Disable slot | Console rotation | Raw grubenv view | Recreate grubenv
Update   From network (OTA) | From USB stick
Backup   Export to stick | Import from stick | Reset configuration | Full factory reset
Network  Ethernet DHCP | Test | Date and time | Configure* | Use in normal system*
Tests    Rescue integrity (SHA-256) | Slot integrity* | Memory test* | Disk read test* | File system check*
Tools    Logs | Background tasks | Diagnostics bundle* | Subshell | Remote access (SSH)* | About
Power    Reboot | Reboot into slot A/B | Reboot into rescue | Power off | Restart rescue shell
                                                                      (* not implemented yet)
```

## Build & Install

```shell
make build              # cargo build --release
make install            # installs target/release/xoros-rescue-shell to /usr/bin/xoros-rescue-shell
make all                # build + install

make all debug=1        # debug build, installed as /usr/bin/xoros-rescue-shelldebug
```

Or directly with Cargo:

```shell
cargo run
```

## Yocto / BitBake Recipes

Recipes `xoros-rescue-shell_<version>.bb` are generated with `cargo-bitbake`. They fetch the sources from
`git@github.com:xoros-repo/xoros-rescue-shell.git` (branch `main`) at a pinned `SRCREV` and list all crate
dependencies. Local overrides can be placed in `xoros-rescue-shell-${PV}.inc` or `xoros-rescue-shell.inc`.

### Update Dependencies

> See: https://github.com/meta-rust/cargo-bitbake

```shell
cargo bitbake
```

Regenerate the recipe after bumping the version in `Cargo.toml` or changing dependencies.

## Project Layout

```
src/
├── main.rs        # args, logging, panic hook, TUI or CLI
├── app.rs         # shared state: backend, task runner, rescue check
├── backend/       # SystemBackend trait; core.rs (xoros-core), sim.rs, unavailable.rs (fallback)
├── backup/        # pax tar.gz export/import, verification, factory reset (BAK-*, RST-*)
├── backup_jobs.rs # backup/import/reset jobs on top of the backend and the task runner
├── update_jobs/   # OTA/USB update jobs on xoros-core's sources and flash engine (OTA-*, USB-*)
├── boot.rs        # boot-control actions with logging
├── cli.rs         # argument parsing, --cli status
├── net/           # DHCP/.network files, resolv.conf + DNS resolver, HTTP options, SNTP, network test
├── clock.rs cmd.rs constants.rs hw.rs logging.rs logs.rs power.rs
├── report.rs      # text rendering of status/hardware/storage (80x24)
├── slots.rs status.rs sysinfo.rs tasks.rs terminal.rs texts.rs
└── ui/            # cursive screens, one module per menu (only place that uses cursive)
```

Rust 1.78 is the MSRV (`cargo +1.78.0 build`).
