# Xoros Rescue Shell

## Description

Xoros Rescue Shell is a console (TUI) app that allows you to interact with the Xoros Rescue System.

It is designed to provide all necessary tools to manage the system and to perform system recovery operations.

It is written in Rust on top of [cursive](https://github.com/gyscos/cursive) (crossterm backend) and is packaged
for Yocto/OpenEmbedded via `cargo-bitbake` recipes.

## Features

- **Bootloader troubleshooting**: Install, update, etc. _(planned)_
- **Partition management**: Force OTA flash of `ROOTFS_A` / `ROOTFS_B` / `RESCUE`, verify root partitions. _(in progress)_
- **System Configuration**: Backup/restore/reset system configuration. _(placeholder dialogs only)_
- **System Testing**: Run system tests, check system status, etc. _(placeholder menu only)_
- **Subshell**: Drop into `/bin/sh` with a `rescue-shell# ` prompt; the TUI is restored on exit.

## Status

The project is at an early stage (version `0.1.1-2`). Most menu entries currently only show stub dialogs:

- **Force OTA flash → ROOTFS_A** shows a confirmation and a simulated progress bar; `ROOTFS_B` and `RESCUE` show info dialogs only.
- `src/sys/actions/flash.rs` contains a naive `flash_chunk()` that verifies a chunk's SHA-256 and writes it to
  `ROOTFS_A.img` / `ROOTFS_B.img` / `RESCUE.img`. It is not wired into the UI yet.
- `src/models` defines the (WIP) OTA manifest structures (`distro`, `version`, `machine`, `suffix`,
  `build_timestamp`, `images[]` with `type`/`sha256`/`size`).
- **Help → About → System Info** shows placeholder data instead of real disk info.
- The **Tests** menu is example code from cursive.

## Usage

| Key      | Action             |
|----------|--------------------|
| `Esc`    | Open the menu bar  |
| `Ctrl-C` | Exit               |

The app beeps on startup and displays a status bar with its version at the bottom of the screen.

## Menu Structure

```
Configs
├── Backup
├── Restore
└── Reset
Tests
├── New
├── Recent
│   └── Item 1..99
└── Option 1..9
Recovery
├── Force OTA flash
│   ├── ROOTFS_A
│   ├── ROOTFS_B
│   └── RESCUE
├── Verify ROOT partitions
├── Subshell
└── Complete Reset
Help
├── About
│   └── [System Info]
└── Help
    ├── General
    └── Online
Reboot
```

> Note: **Reboot** currently just exits the application.

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
├── main.rs             # App entry point: theme, menubar, global callbacks, status bar
├── lib.rs
├── theme.rs            # Color palette
├── models/             # OTA manifest structures (serde)
├── sys/
│   ├── actions/        # beep, flash, quit handler
│   ├── common/         # SHA-256 helpers
│   └── info/           # System/disk info (placeholder)
└── ui/
    ├── menus/          # configs, tests, recovery, help, quit
    ├── dialogs/
    └── status_bar.rs   # Bottom status bar extension for Cursive
```
