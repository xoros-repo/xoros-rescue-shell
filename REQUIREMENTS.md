# XorOS Rescue Shell: Technical Implementation Requirements

Status: draft 5 (2026-11-12; draft 4: 2026-11-12; draft 3: 2026-11-10; draft 2: 2026-10-27). Target app: `xoros-rescue-shell` from github.com/xoros-repo/xoros-rescue-shell.
Related: `../xoros-xor-tool/REQUIREMENTS.md` (shared crate `xoros-core` and the `xor` tool, see [section 9.1](#91-shared-crate-xoros-core)).

This document contains everything needed to implement the rescue shell without access to the `xoros-distro`
repository: the device layout, the bootloader contract, the OTA format, the runtime environment of the rescue system
and the functional requirements. Facts about the distro are given as they are in `xoros-distro` (branch
`wrynose/feat/initial`) at the time of writing. When the app needs something the distro does not provide yet, the
requirement is listed in [section 12](#12-distro-side-dependencies) and the app must degrade gracefully until it lands.

Requirement keywords: **MUST**, **SHOULD**, **MAY** (RFC 2119). Every requirement has an ID (e.g. `OTA-3`) for
tracking. Priority: **P1** is needed for the first usable release, **P2** for the next one, **P3** is nice to have.

---

## 1. Purpose and scope

XorOS is an embedded Linux distribution (Yocto) for kiosk/appliance devices (Raspberry Pi 4, generic x86-64 PCs, QEMU).
Every device has three bootable systems on one drive: two root filesystem slots (A/B) and a rescue system. The rescue
system boots when both slots fail or when a user picks it in the boot menu. The rescue shell is the program a person
sees on the screen in the rescue system. It must let a technician with a keyboard (and optionally a USB stick and a
network) bring the device back to a working state without another computer. When an admin password is set
([section 4.7](#47-access-protection-admin-password)), only a technician who knows it can change anything; anybody
else at the console can only look and reboot.

In scope: everything in [section 6](#6-functional-requirements).
Out of scope: changing the partition layout, updating the bootloader/boot partition (planned later, see `BOOT-*`
notes), updating the rescue partition from the rescue system itself (see `OTA-9`).

## 2. Glossary

| Term | Meaning |
|---|---|
| Slot | One of the two root filesystem partitions, `ROOTFS_A` (`rootfs_a`) or `ROOTFS_B` (`rootfs_b`). |
| Normal system | XorOS booted from a slot (kiosk UI, NetworkManager, Docker). |
| Rescue system | XorOS Rescue booted from the `rescue` partition; runs this app. |
| Boot disk | The drive the rescue system was booted from (internal disk, SD card or, on the Pi, a USB drive). |
| External stick | Any other removable block device the user plugs in. |
| grubenv | 1024-byte GRUB environment file holding the boot counters and boot options. |
| OTA | Over-the-air update: a manifest plus chunk files on an HTTP(S) server or on a stick. |
| Persistent partitions | `overlay` (changes to `/etc` of the normal system) and `application` (`/opt`, apps/data). |

## 3. Target platforms

| Machine (`MACHINE`) | CPU / Rust target | Boot chain | Typical display |
|---|---|---|---|
| `raspberrypi4-64` | aarch64, `aarch64-unknown-linux-gnu` | RPi firmware -> U-Boot (EFI) -> GRUB -> kernel | Official 7" DSI panel (800x480 -> 100x30 text console), HDMI |
| `genericx86-64` | x86_64, `x86_64-unknown-linux-gnu` | UEFI -> GRUB -> kernel | Any (UEFI framebuffer or DRM driver) |
| `qemux86-64` | x86_64 | UEFI (OVMF) -> GRUB -> kernel | QEMU VGA |

The boot logic (grub.cfg) is the same on all machines. The app MUST NOT contain machine specific code except where
noted (hardware info, `HW-*`).

## 4. Device layout and boot contract

### 4.1 Partition table (GPT, identical on all machines)

| # | GPT PARTLABEL | Filesystem | Size | GPT PARTUUID | Content |
|---|---|---|---|---|---|
| 1 | `boot` | vfat (FS UUID `900D-B007`) | 64 MiB | `0bce0f6a-9fb1-4cc7-9d57-6278af875e19` | GRUB (`EFI/BOOT/boot{x64,aa64}.efi`), GRUB modules, `EFI/BOOT/grub.cfg`, `EFI/BOOT/grubenv`; on the Pi also firmware, `config.txt`, DTBs, U-Boot |
| 2 | `rootfs_a` | squashfs (raw, at offset 0) | 1 GiB | `aa508ba4-56d0-4f98-b098-21f51b7b6407` | Slot A. Kernel at `/bzImage` (x86) or `/Image` (arm64), build info at `/xoros.env` |
| 3 | `rootfs_b` | squashfs | 1 GiB | `bb91087f-3fce-4b60-a48f-9635e9f43a94` | Slot B, same layout |
| 4 | `rescue` | squashfs | 704 MiB | `c1e8de08-5724-494f-bb12-634765e37cfb` | Rescue system (this app) |
| 5 | `overlay` | ext4, label `overlay` | 256 MiB | random | Upper layer of `/etc` of the normal system |
| 6 | `application` | ext4, label `application` (FS UUID `e3921621-6463-4485-89ee-77e5c69fa7f3`) | 4 GiB (may be grown to fill the disk) | random | Mounted at `/opt` in the normal system |

Consequences the app MUST handle:

- **DEV-1 (P1)** PARTUUIDs, PARTLABELs and filesystem UUIDs are the same on every XorOS drive. A second XorOS drive or
  stick plugged in has the same labels, so `/dev/disk/by-partlabel/*` is ambiguous. The app MUST find the boot disk
  first (`DEV-2`) and address its partitions by disk + partition number, verifying the PARTLABEL.
- **DEV-2 (P1)** The boot disk is the parent disk of the block device mounted at `/` (read `/proc/self/mountinfo` for
  `/`, take the `major:minor`, resolve `/sys/dev/block/<maj>:<min>/..`). The kernel command line contains
  `root=PARTLABEL=rescue boot.slot=RESCUE`. If `/` is not on a `rescue` partition (development, see
  [section 10](#10-development-and-simulation-mode)), use the configured device map.
- **DEV-3 (P1)** On the Raspberry Pi the boot disk is usually itself a USB drive. "Is USB/removable" MUST NOT be used
  to tell the boot disk from external sticks; only `DEV-2` decides.
- **DEV-4 (P1)** Partition device names: `/dev/sdX<n>`, `/dev/mmcblk0p<n>`, `/dev/nvme0n1p<n>`. Build them from sysfs
  (`/sys/block/<disk>/<part>/partition`), never by string concatenation.
- Squashfs images are written raw at offset 0 of the partition. The rest of the partition is unused. The real image
  length is the squashfs superblock field `bytes_used` (little-endian u64 at byte offset 40) rounded up to 4096
  (mksquashfs pads to 4 KiB). Hashes in `xoros.env` and OTA manifests cover exactly the image file, so hash the first
  `round_up(bytes_used, 4096)` bytes (or the manifest `size`) of the partition, not the whole partition.

### 4.2 Bootloader (GRUB) behaviour

`EFI/BOOT/grub.cfg` on the boot partition does the following on each boot:

1. Sets defaults: `ORDER="ROOTFS_A ROOTFS_B"`, `ROOTFS_A_TRY=0`, `ROOTFS_B_TRY=0`, `ROTATION=0`, `MAX_TRIES=3`,
   `NEXT_BOOT=""`.
2. `load_env` with an explicit variable list (`BOOT-8`): only the variables of the table below override the defaults
   (so `ORDER`, `MAX_TRIES` and `ROTATION` can be changed by writing them to grubenv). Today `load_env` still loads
   every variable (`DIST-11`).
3. If `NEXT_BOOT` is set (`MENU_ROOTFS_A`, `MENU_ROOTFS_B` or `MENU_RESCUE`), boots that entry once and clears it
   (`save_env NEXT_BOOT` with an empty value).
4. Otherwise picks the first slot in `ORDER` whose `<SLOT>_TRY` is lower than `MAX_TRIES`; if none, picks `Rescue`.
5. The menu is shown for 3 seconds (`timeout=3`), entries: `RootFS Slot A (TRY=n)`, `RootFS Slot B (TRY=n)`, `Rescue`
   (x86 also `memtest86+`, `DIST-6`). With `MENU_MODE` (`BOOT-9`, `DIST-11`) the menu is hidden unless enabled.
   When an admin password exists (4.7) every entry still boots without a password (`--unrestricted`), but editing an
   entry (`e`) and the GRUB command line (`c`) require it.
6. Booting a slot increments `<SLOT>_TRY` (values 0..20 are supported, larger values stop incrementing) and saves
   `<SLOT>_TRY` and `LAST_BOOT=<SLOT>`. Booting rescue saves `LAST_BOOT=RESCUE`.
7. Kernel command line includes `boot.slot=ROOTFS_A|ROOTFS_B|RESCUE`, `root=PARTLABEL=<label>`,
   `rootfstype=squashfs`, `net.ifnames=0` (so interfaces are `eth0`, `wlan0`, ...), `fbcon=rotate:$ROTATION`,
   `console=ttyS0,115200` (and `console=tty1` on arm64).

The normal system is expected to reset its slot counter after a successful boot with
`grub-editenv /boot/EFI/BOOT/grubenv set <SLOT>_TRY=0` (`/usr/bin/mark-boot-ok.sh`). **Note:** at the time of
writing nothing calls `mark-boot-ok.sh` automatically (see `DIST-1`), so counters grow by one per boot and a device
ends up in rescue after `2 x MAX_TRIES` boots. The rescue shell MUST make this situation easy to see and fix
(`BOOT-2`, `BOOT-3`).

grubenv variables the app reads and writes:

| Variable | Values | Meaning | App access |
|---|---|---|---|
| `ROOTFS_A_TRY`, `ROOTFS_B_TRY` | `0`..`20` | Boot attempts since last success | read, reset |
| `MAX_TRIES` | `1`..`20` (default 3) | Attempts before a slot is skipped | read, write |
| `ORDER` | `"ROOTFS_A ROOTFS_B"` or `"ROOTFS_B ROOTFS_A"` | Slot preference | read, write |
| `NEXT_BOOT` | `""`, `MENU_ROOTFS_A`, `MENU_ROOTFS_B`, `MENU_RESCUE` | One-shot boot target | read, write |
| `LAST_BOOT` | `ROOTFS_A`, `ROOTFS_B`, `RESCUE` | Last entry GRUB booted | read only |
| `ROTATION` | `0`..`3` | Console rotation (fbcon) | read, write |
| `MENU_MODE` | `""`, `shown`, `hidden`, `once` | Boot menu visibility (`BOOT-9`); `""` = build default | read, write (admin) |

Unknown variables MUST be preserved when writing.

- **BOOT-8 (P2)** grub.cfg loads only the variables of the table above (`load_env ORDER ROOTFS_A_TRY ROOTFS_B_TRY
  MAX_TRIES NEXT_BOOT LAST_BOOT ROTATION MENU_MODE`) and validates the values it uses in commands, as it does for
  `ROTATION` today. A variable in grubenv MUST NOT be able to change `superusers`, the passwords, `CMDLINE` or the
  menu entries.
- **BOOT-9 (P2)** `MENU_MODE`: `shown` = menu with the 3 s timeout (today's behaviour); `hidden` =
  `timeout_style=hidden`, the default entry boots without showing the menu; `once` = shown on the next boot only, then
  GRUB saves `MENU_MODE=hidden`. Empty or unknown = the build default `XOROS_BOOT_MENU` (`shown` until the boot chain
  is proven, `hidden` later). The automatic choice of the default entry (A/B/rescue fallback, step 4) is the same in
  all modes. Only an admin session (`SEC-2`) or root in the normal system (`xor boot menu`) changes it. Further
  conditions that show the menu MAY be added later; they MUST NOT need a password.

### 4.3 grubenv file format and writing rules

- Path: `EFI/BOOT/grubenv` on the `boot` partition (vfat).
- Exactly 1024 bytes: the line `# GRUB Environment Block\n`, then `NAME=value\n` lines, then `#` padding to 1024 bytes.
- **ENV-1 (P1)** Values written by the app MUST match `[A-Za-z0-9_ ]*` (no newline, backslash or `#`); the app MUST
  reject other values.
- **ENV-2 (P1)** GRUB `save_env` writes the file in place through its block list, so the file MUST keep its size and
  location: open the existing file read/write, write all 1024 bytes at offset 0, `fsync`, then `sync` the filesystem.
  Never delete, rename, truncate or recreate it. If the file is missing or not exactly 1024 bytes, report an error and
  offer "Recreate grubenv" (create a new 1024-byte file) as an explicit action.
- **ENV-3 (P1)** Mount the boot partition only for the duration of a read or write
  (`/run/xoros-rescue/mnt/boot`, `vfat`, then unmount). The rescue system has no fstab entry for it.
- `grub-editenv` is installed in the rescue image and MAY be used instead of a native implementation, as long as
  `ENV-2` holds (it rewrites the file at the same size). A native Rust implementation is preferred (testable without
  root, `TEST-2`).

### 4.4 Build information (`/xoros.env`)

Each slot's squashfs contains `/xoros.env` (shell `KEY=value` format, `#` comments). Example from a Raspberry Pi
devel build:

```
### XOROS BUILD ENV FILE --- AUTOMATICALLY GENERATED --- DO NOT EDIT
DISTRO=xoros
VERSION=1.0
MACHINE=raspberrypi4-64
SUFFIX=devel
TIMESTAMP=20261026214512
RESCUE_IMAGE_SHA256=1960b487746edcd73db77ea3ed0673434bd70033883557527cd58812cedcf5d7
```

- `SUFFIX` is one of `stable`, `devel`, `audit`, `nightly`.
- `TIMESTAMP` is the build time `YYYYMMDDHHMMSS` (UTC of the build host). Newer builds compare greater as strings.
- `RESCUE_IMAGE_SHA256` is the SHA-256 of the rescue squashfs that was built together with this slot.
- The rescue squashfs itself has **no** `/xoros.env`. Its identity is `/etc/os-release` (`NAME="Xoros Rescue"`,
  `VERSION_ID`), the hostname `xoros-rescue-<machine>` and the SHA-256 of the rescue partition (`DEV`, 4.1).

To read a slot's `/xoros.env`, mount the slot read-only (`mount -t squashfs -o ro`) at
`/run/xoros-rescue/mnt/rootfs_a|b`. A slot that does not mount is reported as "invalid/empty".

### 4.5 Persistent partitions

- `overlay` (ext4): the normal system mounts it at `/overlay` before init and mounts an overlayfs on `/etc` with
  `lowerdir=/etc` (from the slot), `upperdir=/overlay/overlays/etc/upper`, `workdir=/overlay/overlays/etc/work`,
  options `index=off,xino=off,redirect_dir=off,metacopy=off`. Files changed in the normal system's `/etc` live in
  `overlays/etc/upper`; deletions are overlayfs whiteouts (character devices 0:0) and opaque directories (xattr
  `trusted.overlay.opaque=y`).
- `application` (ext4): mounted at `/opt` in the normal system (apps, Docker data, user data).
- The rescue shell MAY create its own top-level directory `rescue/` on the `overlay` partition for its settings
  (`CFG-1`). The normal system does not use anything outside `overlays/`.

### 4.6 Normal system network stack

The normal system uses **NetworkManager** (keyfile plugin, connections in `/etc/NetworkManager/system-connections/`,
`nmcli` available). Interface names are `eth0`, `wlan0` (`net.ifnames=0`). Network settings copied for the normal
system are therefore NetworkManager keyfiles written into the overlay upper dir (`NET-8`).

### 4.7 Access protection (admin password)

Goal: a device MUST always boot without anybody typing anything (slot A/B/rescue fallback, 4.2), but a person with
only local console access (keyboard, screen, serial port) MUST NOT be able to change the device or read its secrets
without the admin password. Out of scope: an attacker who removes the drive, boots another medium (see `DIST-15`) or
opens the case; that needs disk encryption or Secure Boot.

One admin password, user name `admin`, protects both places a console user can reach:

- **GRUB** (`DIST-11`): `superusers="admin"` plus `password_pbkdf2`. All boot entries are `--unrestricted` (they boot
  automatically and from the menu without a password); editing entries and the GRUB command line need the password.
  This closes kernel command line tricks such as `init=/bin/sh`.
- **The rescue shell**: read-only screens are open, every change and every export of data needs an admin session
  (`SEC-1`, `SEC-2`).

Storage (one format, read by GRUB and by the rescue shell):

- **Device password**: file `EFI/BOOT/admin.cfg` on the `boot` partition, exactly one line
  `password_pbkdf2 admin grub.pbkdf2.sha512.<iterations>.<salt hex>.<hash hex>` (the format of
  `grub-mkpasswd-pbkdf2`, PBKDF2-HMAC-SHA512) and a trailing newline, nothing else. grub.cfg runs it with `source`, the
  rescue shell parses it (`XC-37` in `../xoros-xor-tool/REQUIREMENTS.md`). Written only by an admin session (`SEC-4`) or by root
  in the normal system (`xor admin`): write a temporary file in `EFI/BOOT/`, `fsync`, rename over `admin.cfg`, `sync`.
- **Build default**: optional `XOROS_ADMIN_PASSWORD_HASH` (the `grub.pbkdf2.sha512...` string, never the clear text
  password) set in the distro build (`DIST-12`). It is written into grub.cfg on the boot partition and into
  `/etc/xoros/admin-password` (same one-line format) in the read-only rescue image.

Which password applies (GRUB and the rescue shell MUST decide the same way):

| `admin.cfg` | Build default | Result |
|---|---|---|
| valid | any | device password |
| missing | set | build default |
| missing | not set | protection off (today's behaviour) |
| present but invalid | set | build default |
| present but invalid | not set | locked: no password is accepted; GRUB menu entries still boot, the rescue shell offers only read-only screens and reboot; recovery needs the boot partition to be fixed from outside |

In GRUB this is: `set superusers="admin"` when a build default exists or `admin.cfg` is present; register the
build default with `password_pbkdf2` first, then `source admin.cfg` (a later `password_pbkdf2` for the same user
replaces the earlier one, an invalid line registers nothing). So deleting `admin.cfg` falls back to the build default and never switches protection off when a build default
exists. Known limitation: the boot partition is not updated by OTA (section 1), so after a rescue update with a new
build default the GRUB copy and the rescue copy can differ until a device password is set; a device password
(`admin.cfg`) is the same for both.

## 5. Rescue runtime environment

- **RT-1** The app binary is `/usr/bin/xoros-rescue-shell`. It runs as root on `tty1` instead of the login prompt,
  through a systemd drop-in for `getty@tty1.service` (shipped by the distro, `rescue-shell-service.bb`):

  ```ini
  [Service]
  ExecStart=
  ExecStart=-/usr/bin/xoros-rescue-shell
  StandardInput=tty
  StandardOutput=tty
  Environment=TERM=xterm-256color
  ```

  systemd starts it after boot has finished and restarts it whenever it exits. Exiting the app is therefore "restart
  the app", not "log out". A serial login (`ttyS0`) also exists, but root is locked there.
- **RT-2** The root filesystem is read-only squashfs. Writable locations: `/run` (tmpfs), `/tmp`, `/var/volatile`.
  `/etc` is read-only (no overlay in rescue). The app MUST keep runtime state under `/run/xoros-rescue/` and mounts
  under `/run/xoros-rescue/mnt/`.
- **RT-3** Locale is `LANG=C.UTF-8`; the console is in UTF-8 mode so box-drawing characters work. The Linux VT only
  shows 8 (16 with bold) colors even though `TERM=xterm-256color`: the theme MUST be readable with the 8 basic ANSI
  colors.
- **RT-4** Screen size can be as small as 80x24 (rotated panels, large fonts) and is typically 100x30 on the Pi DSI
  panel. All screens MUST fit 80x24. Input is a keyboard only (no mouse in the VT). The app MUST react to terminal
  resize (rotation change, `ROT-*`).
- **RT-5** Tools available in the rescue image (both machines): systemd + `systemd-networkd` (no `systemd-resolved`,
  no `systemd-timesyncd`), busybox (incl. `udhcpc`, `tar`, `gzip`, `ping`, `blkid`, `fdisk`), util-linux `mount` /
  `umount` / `fsck` / `agetty`, `e2fsprogs` (`e2fsck`, `mke2fs`, `badblocks`, `dumpe2fs`), `parted`, `hdparm`, `iw`,
  `wpa_supplicant` + `wpa_cli` + `wpa_passphrase`, `openssh` (`sshd`), `openssl`, `ca-certificates`
  (`/etc/ssl/certs/ca-certificates.crt`), `grub-editenv`, `kmod`, `strace`, `procps`, `lshw`, `rsync`, `nano`,
  `python3`, `nodejs`, `git`, `dosfstools`, `smartctl` (smartmontools), `nvme` (nvme-cli). **Not** present: `lsblk`,
  `memtester`, `stress-ng`, `fio`, `curl` CLI (only `libcurl`), NetworkManager, the `xor` tool (the rescue shell links `xoros-core` instead, 9.1).
- **RT-6** The app SHOULD implement the logic natively in Rust, through `xoros-core` where it exists (block device
  I/O, hashing, HTTP, grubenv, sysfs parsing) and in the app otherwise (tar, network configuration), and call external tools only for mounting, `e2fsck`, `mke2fs`, `wpa_supplicant`, `udhcpc`/networkd and
  `systemctl`. Every external tool call MUST check that the tool exists and turn a missing tool into a visible
  "not available in this rescue image" message, never a crash.
- **RT-7** Hardware clock: the Raspberry Pi has no RTC. After power-on the clock may be wrong (the systemd build time
  or 1970), which breaks TLS certificate validation. See `NET-7`.

## 6. Functional requirements

Menu structure (top-level menu bar, `Esc` opens it). The app MUST start on the **Status** screen.

```
Status            system summary + why rescue was entered (SYS-1)
Boot              counters, next boot, slot order, rotation (BOOT-*, ROT-*)
Update            from network (OTA-*), from USB stick (USB-*)
Backup            export/import persistent partitions (BAK-*), factory reset (RST-*)
Network           configure/test rescue network, copy to normal system (NET-*)
Tests             memory, disk, slot integrity, file system check (TST-*)
Tools             logs, diagnostics bundle, subshell, remote access, unlock/lock, admin password (LOG-*, SH-*, SSH-*, SEC-*)
Developer         only when enabled: image flavor, version, OTA server, signature check (DVM-*)
Power             reboot, reboot into slot, power off (PWR-*)
```

### 6.1 System information (`SYS`)

- **SYS-1 (P1)** Status screen shows:
  - why the device is in rescue: `NEXT_BOOT`/menu choice (if both slot counters are below `MAX_TRIES`) or
    "both slots exceeded MAX_TRIES" (with the counters) or "slot(s) invalid";
  - per slot: valid/invalid, `VERSION`, `SUFFIX`, `TIMESTAMP` (formatted), `MACHINE`, try counter vs `MAX_TRIES`,
    position in `ORDER`, whether it was `LAST_BOOT`;
  - rescue system: `VERSION_ID` from `/etc/os-release`, app version, rescue partition SHA-256 match against each
    slot's `RESCUE_IMAGE_SHA256` (computed in the background, shows "checking..." until done);
  - persistent partitions: present, mountable, used/free space, last fsck result if known;
  - network summary (interfaces, IPs, Internet reachable yes/no);
  - date/time and whether it is synchronized.
- **SYS-2 (P1)** Hardware details screen: board model (`/proc/device-tree/model` on arm64,
  `/sys/class/dmi/id/{sys_vendor,product_name,board_name,bios_version}` on x86), CPU model and cores, RAM total/free,
  CPU temperature (`/sys/class/thermal/thermal_zone*/temp`), on the Pi the throttling state
  (`/sys/devices/platform/soc/soc:firmware/get_throttled` if present, decoded: under-voltage, throttled, ...), kernel
  version and command line, uptime, serial number (`/proc/device-tree/serial-number` or DMI `product_serial`).
- **SYS-3 (P1)** Storage screen: all block devices with model, size, transport (usb/sata/nvme/mmc), removable flag,
  "boot disk" marker, partition table of the boot disk (number, PARTLABEL, PARTUUID, size, filesystem, mounted).
- **SYS-4 (P2)** Network details: per interface MAC, link/carrier, speed, IPs, Wi-Fi SSID/signal; default route; DNS.
- **SYS-5 (P2)** "Save system report" writes all of the above as text + JSON to a stick (part of `LOG-2`).

### 6.2 Boot control and counters (`BOOT`)

- **BOOT-1 (P1)** Show all grubenv variables from 4.2 (raw view also available).
- **BOOT-2 (P1)** Reset try counter: slot A, slot B or both, to `0`. Explain that a reset makes GRUB try that slot again
  on the next boot.
- **BOOT-3 (P1)** Set next boot (one-shot `NEXT_BOOT`): slot A, slot B, rescue, or clear. Offer "reset counter and
  reboot into slot X now" as a single action.
- **BOOT-4 (P2)** Set slot order (`ORDER`): A first or B first.
- **BOOT-5 (P2)** Set `MAX_TRIES` (1..20).
- **BOOT-6 (P1)** Warn before booting into a slot that is invalid (`SYS-1`) or empty.
- **BOOT-7 (P3)** Mark a slot "disabled" by setting its counter to `MAX_TRIES` (GRUB then skips it).
- **ROT-1 (P2)** Set console rotation `ROTATION` 0..3 (0/90/180/270 degrees); applied by GRUB on next boot. The app
  SHOULD also apply it immediately for the running console
  (`echo <n> > /sys/class/graphics/fbcon/rotate_all`) and redraw.

### 6.3 OTA update over the network (`OTA`)

Server layout (default base URL `https://ota.xoros.io`, configurable `CFG-1`):

```
<base>/<VERSION>/<MACHINE>/<SUFFIX>/manifest.json
<base>/<VERSION>/<MACHINE>/<SUFFIX>/<chunk file>         (chunk files are in the same directory)
<base>/<VERSION>/<MACHINE>/<SUFFIX>/signature.json       (optional, see OTA-8)
```

`manifest.json` (produced by the `xor` tool at build time):

```json
{
  "distro": "xoros",
  "version": "1.0",
  "machine": "raspberrypi4-64",
  "suffix": "stable",
  "images": [
    {
      "type": "rootfs",
      "sha256": "<sha256 of the whole image, hex>",
      "size": 412160000,
      "build_timestamp": "20261026214512",
      "index": 0,
      "chunk_size": 22000000,
      "chunks": [
        { "sha256": "<hex>", "size": 22000000, "file": "0_0_9f2c1a.chunk", "offset": 0, "index": 0 },
        { "sha256": "<hex>", "size": 22000000, "file": "0_1_51be07.chunk", "offset": 22000000, "index": 1 }
      ]
    },
    { "type": "rescue", "...": "same fields, index 1" }
  ]
}
```

- `type` is `rootfs`, `rescue` or `boot`. Chunk file names are `<image index>_<chunk index>_<first 6 hex of chunk sha256>.chunk`.
  The last chunk may be smaller than `chunk_size`. Image bytes = concatenation of chunks in `offset` order.
- The rootfs image is a squashfs (no partition table), written at offset 0 of the slot partition.
- A manifest MAY list several builds. The images of one build (`rootfs` and `rescue`) share its `build_timestamp`.
  The server keeps the two newest builds of each `<VERSION>/<MACHINE>/<SUFFIX>`, so a device can go back one build
  (`OTA-3`); publishing is described in `DIST-16`. Pinned builds (images with `"pinned": true`, `DIST-18`) are kept
  in addition, however old they are. Image `index` values are unique within a manifest but are not
  contiguous and need not start at 0 (older builds are removed); the app MUST identify an image by its `index` and
  `sha256`, never by its position in `images`.
- A chunk of a build that was removed from the server while it was being downloaded fails with HTTP 404: a download
  error (`OTA-5`). Checking again lists the builds that are available now.

Requirements:

- **OTA-1 (P1)** Determine `VERSION`, `MACHINE`, `SUFFIX` from the newest valid slot's `xoros.env`. If no slot is
  valid, ask the user (MACHINE preset from the running rescue system's hostname `xoros-rescue-<machine>`; VERSION and
  SUFFIX entered/selected). The user MAY switch SUFFIX (e.g. `devel` -> `stable`) with a warning. Developer
  overrides (`DVM-5`) replace VERSION and SUFFIX from the slot while they are in effect.
- **OTA-2 (P1)** Fetch the manifest, check `machine`, `version` and `suffix` match. Mismatch is an error.
- **OTA-3 (P1)** List `rootfs` images with formatted build timestamp, size, and whether it is newer/same/older than
  each slot; a pinned build (`DIST-18`) is marked "pinned" (P2). Default selection: the newest. Installing the same or an older build is allowed in rescue but needs an
  extra confirmation ("downgrade").
- **OTA-4 (P1)** Target selection: slot A, slot B or A+B. Check `size` fits into the target partition.
- **OTA-5 (P1)** Write procedure, per target slot:
  1. Set `<SLOT>_TRY=MAX_TRIES` in grubenv (slot is skipped by GRUB while being written).
  2. For each chunk in order: download (to memory, max `chunk_size`), verify its SHA-256, write it at `offset`,
     update progress (bytes, speed, ETA). Retry a failed download 3 times with backoff, then pause and let the user
     retry or cancel.
  3. `fsync` the partition, drop caches for it (`BLKFLSBUF` ioctl or `posix_fadvise(DONTNEED)`), re-read the first
     `size` bytes and compare the SHA-256 with the image `sha256`.
  4. On success set `<SLOT>_TRY=0`; on failure leave it at `MAX_TRIES` and show the error.
  For A+B: download and write slot A completely and verify it, then copy slot A to slot B from the local disk (no second
  download) and verify B against the same hash.
- **OTA-6 (P2)** Resume: before downloading a chunk, hash the partition range `[offset, offset+size)`; if it already
  matches, skip the download. This makes an interrupted update resumable and makes re-installing the same build
  quick.
- **OTA-7 (P1)** Cancel is possible at any time; a cancelled slot stays disabled (`MAX_TRIES`) and the UI says so.
- **OTA-8 (P2)** Signature (when `signature.json` exists or when `CFG-1 require_signature=true`):
  `{"mh": "<hex sha256(manifest.json bytes)>", "ms": "<hex 64-byte r||s ECDSA signature>", "pk": "<hex SEC1 public key>"}`,
  secp256k1, created with the Rust `k256` crate: `SigningKey::sign(sha256(manifest))`, i.e. the signed message is the
  32-byte manifest digest (which `k256` hashes again with SHA-256 internally). The app MUST verify `mh` against the
  downloaded manifest bytes and `ms` with a public key from the trusted key list (`CFG-1`), **not** with the `pk`
  field from the server. The build publishes `signature.json` when it has a signing key (`DIST-5`).
- **OTA-9 (P1)** `rescue` images in the manifest are shown (with "matches current rescue: yes/no") but MUST NOT be
  written: the rescue partition is the running root filesystem. Updating rescue is done from the normal system.
  `boot` images are not supported in this version.
- **OTA-10 (P1)** Before the first network request, check the system clock (`NET-7`).
- **OTA-11 (P2)** HTTP proxy support (`CFG-1 http_proxy`). HTTPS with system CA bundle; HTTP allowed only for base
  URLs explicitly configured by the user (local mirror).

### 6.4 Update rootfs from a USB stick (`USB`)

- **USB-1 (P1)** Detect external sticks: block devices that are not the boot disk (`DEV-2`), with partitions or a
  filesystem on the whole device. Support vfat (FAT32) and ext4 MUST, exFAT SHOULD (needs kernel support, `DIST-4`).
  Mount read-only for reading, read-write only for writing backups/reports, under `/run/xoros-rescue/mnt/usb/<dev>`.
  Hot-plug: refresh the list when devices appear/disappear (poll sysfs every 2 s or watch uevents).
- **USB-2 (P1)** Accepted update sources on the stick:
  1. **OTA tree** (preferred): a copy of the build output directory `ota/` (or any directory that contains
     `<VERSION>/<MACHINE>/<SUFFIX>/manifest.json` + chunks). Search the stick root and `xoros/`, `ota/`, `xoros/ota/`
     up to 4 levels deep for `manifest.json` files.
  2. **Single image**: `*.squashfs` with a sidecar `<name>.sha256` (`<hex>  <name>`, sha256sum format) and
     optionally `<name>.json` `{"machine","version","suffix","build_timestamp"}`. Without a sidecar hash the image
     can only be written after the user confirms "unverified image" (P2).
- **USB-3 (P1)** The same checks, target choice (A/B/A+B), write procedure, verification and counter handling as
  `OTA-2`..`OTA-7`, reading chunks from the stick instead of HTTP. Code MUST be shared (one "image source" trait with
  HTTP and directory implementations).
- **USB-4 (P1)** Before writing, read the image's squashfs superblock (magic `hsqs` at offset 0) and refuse non-squashfs
  data. After writing, mount the slot read-only and check `/xoros.env` `MACHINE` matches the device; show the new
  version.
- **USB-5 (P1)** Safe removal: unmount and `sync` the stick, then tell the user it can be removed.

### 6.5 Backup and restore of persistent partitions (`BAK`)

- **BAK-1 (P1)** Export to stick: user selects `overlay`, `application` or both (default both). The partitions are
  mounted read-only. Output is one archive:

  ```
  xoros-backup-<hostname>-<YYYYMMDD-HHMMSS>.tar.gz          (or .tar.gz.000, .001, ... when split, BAK-3)
  xoros-backup-<hostname>-<YYYYMMDD-HHMMSS>.tar.gz.sha256   (sha256sum format, one line per file)
  ```

  Archive content (POSIX pax tar, gzip):

  ```
  xoros-backup.json          first entry: metadata (below)
  overlay/...                content of the overlay partition root (excluding overlays/etc/work and lost+found)
  application/...            content of the application partition root (excluding lost+found)
  ```

  `xoros-backup.json`:
  ```json
  { "format": 1, "created": "2026-10-27T12:00:00Z", "machine": "raspberrypi4-64",
    "slots": { "ROOTFS_A": {"version":"1.0","suffix":"stable","timestamp":"20261026214512"}, "ROOTFS_B": null },
    "partitions": { "overlay": {"files": 120, "bytes": 81234}, "application": {"files": 5321, "bytes": 1234567890} },
    "app_version": "0.2.0" }
  ```
- **BAK-2 (P1)** The archive MUST preserve: regular files, directories, symlinks, hard links, character/block device
  nodes (overlayfs whiteouts are char devices 0:0), FIFOs, numeric uid/gid (no name mapping), mode incl.
  setuid/setgid/sticky, mtime, and extended attributes (pax `SCHILY.xattr.*`), in particular `trusted.overlay.*` and
  `security.*`. Sparse files MAY be stored expanded. Docker data in `application/` relies on all of the above.
- **BAK-3 (P1)** On vfat sticks split the archive into parts of at most 4 GiB - 1 byte (`.000`, `.001`, ...). Check
  free space on the stick against the used space of the selected partitions before starting and warn if it may not
  fit (the archive is compressed, so allow continuing).
- **BAK-4 (P1)** Import from stick: list backups found on the stick (root and `xoros/backups/`), show the metadata,
  verify the `.sha256` file before extracting. The user chooses which partitions to restore (only those in the
  archive) and the mode:
  - **Replace** (default): delete everything on the target partition (except `lost+found`), then extract.
  - **Merge**: extract over existing files (overwrite on conflict).
  For `overlay`, always recreate an empty `overlays/etc/work` directory afterwards. The rescue shell's own `rescue/`
  directory on the overlay partition is kept in both modes and not restored from the backup (the current rescue
  settings win). A machine mismatch with the device is a warning, not an error.
- **BAK-4a (P1)** A backup that cannot be verified MUST NOT be imported (there is no "import anyway"). Refusal happens
  before anything on the target is changed, and the dialog MUST name the exact reason and the file(s) involved, e.g.:
  - `.sha256` file missing: "Checksum file `<name>.tar.gz.sha256` not found next to the backup";
  - archive part missing: "Part `<name>.tar.gz.002` is listed in the checksum file but missing" (or present on the
    stick but not listed);
  - checksum mismatch: "Part `<file>`: expected SHA-256 `<first 16 hex>…`, got `<first 16 hex>…` (file damaged or
    changed)";
  - checksum file unreadable/malformed: the line number and content of the bad line;
  - `xoros-backup.json` missing, unreadable or of an unsupported `format`: what was found;
  - read error on the stick: the OS error text and the file.
  The same reasons are written to the app log (LOG-3).
- **BAK-5 (P1)** Before import run `e2fsck -p` on the target; after import `sync` and show a summary (files, bytes,
  errors). Extraction errors on single files are collected and shown at the end; they do not abort the import.
- **BAK-6 (P2)** If an ext4 partition cannot be mounted even after `e2fsck -y`, offer "Re-create file system"
  (`mke2fs -t ext4 -L <label> -U <fs uuid if known>` on that partition, with typed confirmation) before restoring.
- **BAK-7 (P3)** Raw image export of a partition (`.img.gz`) for forensic use.

### 6.6 Factory reset (`RST`)

- **RST-1 (P1)** Reset configuration: delete the content of `overlay` (keeps the rescue's own `rescue/` directory
  unless the user also chooses "reset rescue settings"). Result: the normal system starts with the `/etc` shipped in
  the slot.
- **RST-2 (P1)** Full reset: overlay as above plus delete all data on `application`.
- **RST-3 (P1)** Both require typed confirmation (`RESET`) and offer "export a backup first" (`BAK-1`) when a stick is
  present.

### 6.7 Network (`NET`)

Rescue side (runtime, nothing is written to the read-only rootfs):

- **NET-1 (P1)** On start, bring up Ethernet with DHCP automatically: for each `eth*`/`en*` interface write
  `/run/systemd/network/50-xoros-rescue-<if>.network` (`[Match] Name=<if>`, `[Network] DHCP=yes`) and run
  `networkctl reload` (systemd-networkd is installed; with no `.network` files it does nothing). If networkd is not
  running, fall back to `udhcpc -i <if>` from busybox.
- **NET-2 (P1)** Configure per interface: DHCP or static (address/prefix, gateway, DNS servers). Wi-Fi: scan
  (`iw dev <if> scan` or `wpa_cli scan_results`), select or enter SSID (hidden network option), security open /
  WPA2-PSK / WPA3-SAE, passphrase (masked input with "show"), regulatory country code (`iw reg set <CC>`).
  Wi-Fi is run by starting `wpa_supplicant -B -i <if> -c /run/xoros-rescue/wpa_supplicant-<if>.conf` (the stock
  `wpa_supplicant@.service` reads `/etc`, which is read-only) and the `.network` file from `NET-1` for addressing.
  Unblock rfkill (`/sys/class/rfkill/*/soft`) when Wi-Fi is enabled.
- **NET-3 (P1)** DNS: there is no `systemd-resolved` in the rescue image. Write the nameservers (from DHCP lease or
  static config) to `/run/xoros-rescue/resolv.conf` and have the app's own resolver use it. `/etc/resolv.conf` is
  read-only; whether it points to `/run` is not guaranteed (`DIST-3`), so the app MUST NOT rely on libc's resolver
  for its own requests (use a resolver that takes an explicit server list, e.g. `hickory-resolver`, or ship a static
  config). Busybox `ping` used in tests MAY fail name resolution: tests MUST then ping by IP.
- **NET-4 (P1)** Test: per step pass/fail with details and duration: link/carrier, IP address, gateway reachable
  (ICMP or ARP), DNS resolution of the OTA host, TCP connect to OTA host:443, HTTPS `GET` of the manifest URL (status
  code), clock check (`NET-7`). P2: download speed test (download one OTA chunk to `/dev/null`).
- **NET-5 (P2)** Remember the rescue network config for the next rescue boot in `overlay:/rescue/network.json`
  (only when the user chooses "remember"). Wi-Fi passphrases are stored as WPA PSK hex (`wpa_passphrase` output), not
  in clear text. File mode `0600`. WPA3-SAE has no PSK hash: its password is only remembered when the user agrees to
  store it in clear text (TUI question, CLI `--store-wpa3-password`); otherwise the network is remembered without
  password and asked for again on the next rescue boot.

Normal-system side:

- **NET-6 (P1)** "Use these settings in the normal system": for each configured interface, write a NetworkManager
  keyfile to the overlay partition at
  `overlays/etc/upper/NetworkManager/system-connections/xoros-<if>.nmconnection` (create missing directories with mode
  `0755`, `system-connections` with `0700`; file owner root:root, mode `0600`). If a whiteout (char device 0:0)
  exists at that path, remove it first. Keyfile template:

  ```ini
  [connection]
  id=xoros-wlan0
  uuid=<random UUID v4, kept stable when the file is rewritten>
  type=wifi                 ; or ethernet
  interface-name=wlan0
  autoconnect=true
  autoconnect-priority=10

  [wifi]                    ; Wi-Fi only
  mode=infrastructure
  ssid=MyNetwork
  hidden=false

  [wifi-security]           ; Wi-Fi with security only
  key-mgmt=wpa-psk          ; wpa-psk (WPA2), sae (WPA3)
  psk=<passphrase or 64-hex PSK>

  [ipv4]
  method=auto               ; or: manual
  ; address1=192.168.1.50/24,192.168.1.1
  ; dns=1.1.1.1;8.8.8.8;
  ; ignore-auto-dns=true    ; when static DNS is set with DHCP

  [ipv6]
  method=auto

  [proxy]
  ```
- **NET-7 (P1)** Time: show the current UTC time. Provide "Sync time" via SNTP (implemented in the app, servers
  `pool.ntp.org` and `time.cloudflare.com`, configurable) and manual date/time entry. Before HTTPS requests, if the
  clock is before the app's build date, sync automatically or ask the user to set it. P2: write the time to the RTC
  (`/dev/rtc0`) if it exists.
- **NET-8 (P2)** "Load from normal system": read existing keyfiles (overlay upper first, then the slot's
  `/etc/NetworkManager/system-connections`) and offer them as rescue network configs (only types ethernet/wifi with
  psk; others shown read-only).

### 6.8 Hardware tests (`TST`)

All tests show progress, can be cancelled, keep the result (pass/fail/aborted, duration, details) for the session and
include it in the diagnostics bundle (`LOG-2`).

- **TST-1 (P1)** Memory test (in the app, userspace): allocate and `mlock` a user-selected share of free memory
  (default 80% of `MemAvailable`, leave at least 64 MiB), run patterns: solid bits (0x00/0xFF), checkerboard,
  walking ones/zeros, address-in-address, random data with a seeded PRNG (verify), and a bit-flip test. Number of
  passes selectable (default 1). Report failing address offset and expected/actual values. State clearly that a
  userspace test cannot test memory used by the kernel and the app.
- **TST-2 (P3)** Offer "Reboot into memtest86+" when a boot entry for it exists (`DIST-6`).
- **TST-3 (P1)** Disk read test (non-destructive): read the whole boot disk (or a selected partition) sequentially
  with `O_DIRECT`, 1 MiB blocks; report read errors (sector ranges), slow blocks (> 500 ms) and throughput. Kernel
  I/O errors from `/dev/kmsg` during the test are shown too.
- **TST-4 (P1)** Slot integrity: for each slot, check the squashfs superblock (magic, `bytes_used` within the
  partition), then mount and read every file (detects I/O and decompression errors); if an OTA manifest for that build
  is available (network or stick), compare the SHA-256 of the first `size` bytes.
- **TST-5 (P1)** Rescue integrity: SHA-256 of the rescue partition (`round_up(bytes_used, 4096)` bytes) against each
  slot's `RESCUE_IMAGE_SHA256`. A mismatch with both slots is a warning (rescue may simply be older/newer).
- **TST-6 (P1)** File system check of `overlay` and `application`: `e2fsck -f -n` (check only) and, with confirmation,
  `e2fsck -f -y` (repair). Show the e2fsck output.
- **TST-7 (P2)** Disk health: SMART / NVMe health when the tools exist (`smartctl -H -A`, `nvme smart-log`, `DIST-2`);
  otherwise show what sysfs gives (e.g. `/sys/block/<dev>/device/` model, NVMe `critical_warning`) and say that SMART
  is unavailable. eMMC: `/sys/block/mmcblk*/device/life_time` and `pre_eol_info` when present.
- **TST-8 (P2)** Destructive stick test: write/read-back a random pattern over a selected external stick (detects
  fake-capacity sticks). Typed confirmation; never offered for the boot disk.
- **TST-9 (P3)** CPU stress / thermal test: run all cores for N minutes, sample temperature and (Pi) throttling flags.

### 6.9 Logs, diagnostics, shell, remote access (`LOG`, `SH`, `SSH`)

- **LOG-1 (P1)** View logs: the app's own log, `journalctl -b` of the rescue boot, kernel log (`dmesg`). Scrollable,
  with search.
- **LOG-2 (P1)** Diagnostics bundle to stick: `xoros-diag-<hostname>-<YYYYMMDD-HHMMSS>.tar.gz` with system report
  (`SYS-5`), grubenv, both `xoros.env`, partition table, `dmesg`, `journalctl -b`, app log, test results, network
  test results. Must not include Wi-Fi passphrases or backup content.
- **LOG-3 (P1)** The app writes its own log to `/run/xoros-rescue/rescue-shell.log` (and to the journal via stderr).
  Every destructive action is logged with parameters and result.
- **SH-1 (P1)** Subshell: suspend the TUI, run `/bin/sh -l` as root on the same tty with prompt `rescue-shell# `,
  restore the TUI on exit (existing feature, keep). Admin only when protection is on (`SEC-1`).
- **SSH-1 (P2)** Remote support: start `sshd` (installed, normally not running) on demand, with a one-time root
  password generated and shown on screen (set via `chpasswd` into a writable copy, or a dedicated `sshd_config` in
  `/run/xoros-rescue/` with `PasswordAuthentication yes` and an `AuthorizedKeysFile` the user can load from a stick).
  Show the IP addresses. Stop on demand and when the app exits. Admin only when protection is on (`SEC-1`).

### 6.10 Power (`PWR`)

- **PWR-1 (P1)** Reboot (`systemctl reboot`), power off (`systemctl poweroff`), "reboot into slot A/B" (sets
  `NEXT_BOOT` and optionally resets the counter, then reboots), "reboot into rescue" (`NEXT_BOOT=MENU_RESCUE`).
- **PWR-2 (P1)** Before reboot/power off: refuse while a write operation is running (offer cancel first), unmount
  everything under `/run/xoros-rescue/mnt`, `sync`.
- The existing "Reboot" menu leaf that only exits the app MUST be replaced.

### 6.11 Settings (`CFG`)

- **CFG-1 (P2)** Settings file `overlay:/rescue/rescue-shell.toml` (overlay partition, so it survives slot updates;
  defaults compiled in when missing or unreadable):

  ```toml
  ota_base_url = "https://ota.xoros.io"
  require_signature = false
  trusted_keys = []            # hex SEC1 secp256k1 public keys
  http_proxy = ""
  ntp_servers = ["pool.ntp.org", "time.cloudflare.com"]
  theme = "default"            # default, dark, mono, norton
  admin_idle_timeout = 300     # seconds without a key press before an admin session locks (SEC-2)
  developer_menu = false       # show the Developer menu (DVM-1)
  ota_version = ""             # developer overrides (DVM-5), empty = from the slot
  ota_suffix = ""
  skip_signature = false       # DVM-7
  ```

  The app MUST work when the overlay partition is missing or broken (settings are then read-only defaults).
- **CFG-2** Replaced by the admin password, [section 6.12](#612-access-protection-sec). The `pin_hash` setting from
  M4 is only read for the migration in `SEC-6`.

### 6.12 Access protection (`SEC`)

Applies when an admin password is in effect (4.7). Without one the app behaves as if an admin session were always
open.

- **SEC-1 (P2)** Without an admin session the app allows only actions that read or that the boot chain does anyway:
  - open: Status and all information screens (`SYS-1..4`), boot state view (`BOOT-1`), settings view, log viewer
    (`LOG-1`), update check without writing (`OTA-1`), network status and test (`SYS-4`, `NET-4`), non-destructive
    tests (`TST-1`, `TST-3`, `TST-4`, `TST-5`, `TST-6` check only, `TST-7`, `TST-9`), reboot and power off
    (`PWR-1` plain reboot/power off);
  - admin only: every write (grubenv incl. next boot, slot order, counters, rotation, `MENU_MODE`; slots, rescue,
    persistent partitions, sticks; settings; network configuration and "remember"; `TST-6` repair, `TST-8`; factory
    reset), every export of data to a stick (`BAK-*`, `SYS-5`, `LOG-2`, `BAK-7`), the subshell (`SH-1`), remote
    access (`SSH-1`), "reboot into slot/rescue/memtest" (`PWR-1`, `TST-2`), and the password functions (`SEC-4`).
  Admin-only menu entries stay visible and show the unlock dialog when chosen; after a successful unlock the chosen
  action continues.
- **SEC-2 (P2)** Admin session: unlock dialog (user name is fixed, masked password input). The session ends on
  "Lock" (Tools menu), after `admin_idle_timeout` seconds without a key press (default 300, minimum 30), and when the app
  restarts. The status bar always shows `LOCKED` or `ADMIN`. An operation started in a session runs to its end when
  the session locks; its result is shown, further actions need a new unlock. Time spent in the subshell does not count
  as idle; the session locks normally after returning. Settings and admin password changes made in the subshell
  (CLI, another process) apply when it returns: the app reloads both.
- **SEC-3 (P2)** Failed unlocks: 2 s delay after each failure; after 5 failures in a row the dialog is blocked for
  60 s, doubling with each further failure up to 15 min (kept in memory, so a reboot resets it; GRUB has its own
  handling). Every unlock, failure and lock is logged (`LOG-3`) without the entered text.
- **SEC-4 (P2)** Admin password management (admin session, Tools menu):
  - "Set/change admin password": new password twice; 8 to 64 printable ASCII characters (GRUB reads the keyboard with
    the US layout, so the dialog says so and warns about characters that differ between layouts). Writes
    `admin.cfg` (4.7) through `xoros-core` (`XC-37`, `BootPartition`).
  - "Remove device password": deletes `admin.cfg` after confirmation; the dialog says what applies afterwards (build
    default or no protection).
  - Factory reset (`RST-*`) and backup import (`BAK-*`) do not touch `admin.cfg` (it is on the boot partition).
- **SEC-5 (P2)** Password check through `xoros-core` (`XC-37`): the effective password is chosen exactly as in the 4.7
  table. The check runs in a worker thread (PBKDF2 takes up to about a second on the Pi) with a "checking..." state.
- **SEC-6 (P2)** Migration from the M4 PIN: while no admin password is in effect and `pin_hash` is set, the PIN is the
  session password (same rules as `SEC-1..3`) and the Status screen says the boot menu is not protected. Setting an
  admin password removes `pin_hash`. The CLI `settings set-pin` is removed; `clear-pin` stays for one release.
- **SEC-7 (P2)** Boot menu visibility (`BOOT-9`): in the Boot menu, "Boot menu: shown / hidden / show once on next
  boot" sets `MENU_MODE` (admin only).
- **SEC-8 (P2)** CLI (`ARCH-2`): runs only as root, and root on the rescue system is reachable only through the
  subshell or `SSH-1` (both admin only; root cannot log in, `RT-1`), so CLI commands do not ask for the password.
  Commands: `--cli admin status` (off / build default / device password / locked / legacy PIN), `--cli admin
  set-password` (password on stdin), `--cli admin clear-password`, `--cli boot menu <shown|hidden|once|default>`.

### 6.13 Developer menu (`DVM`)

For developers and testers who move a device between image flavors (`SUFFIX`, 4.4), versions and OTA servers. The
same things are possible today by typing into the Update dialog each time; the menu makes the choice stick, shows what
the server has, and keeps it out of sight on customer devices.

- **DVM-1 (P2)** Visibility: the `Developer` menu (menu bar, after `Tools`) exists only while `developer_menu = true`
  (`CFG-1`, default `false`). Nothing in the other menus, the Settings screen or the help text mentions it. It is
  enabled in one of two ways, both needing an admin session (`SEC-1`, settings write):
  - `Ctrl-D` on the Status screen: "Show the developer menu? It lets this device install development builds."
    -> sets `developer_menu = true`;
  - CLI `--cli settings set developer_menu true` (subshell or `SSH-1`, which are admin only).
- **DVM-2 (P2)** Flavor list: fetch the flavor index of the device's machine from the OTA server (`DIST-17`,
  `XC-112`) and list every published `<VERSION>/<SUFFIX>`: version, suffix, newest build (formatted), number of builds,
  signed yes/no. Mark the flavor of the newest valid slot (`OTA-1`, "installed") and the current override
  ("selected"). The index is informational only: choosing an entry selects it, the update check then fetches and
  checks the manifest as always (`OTA-2`, `OTA-8`). Without an index (HTTP 404, older server) or without network the
  list is the four suffixes of the installed version with "not checked" and a note why; the update check finds out
  whether the flavor exists. The first entry is "Follow the installed build" (clears the overrides).
- **DVM-3 (P2)** Choosing a flavor sets `ota_version` and `ota_suffix` (`CFG-1`) after a confirmation that says what
  changes:
  - to `devel` or `audit`: these images have debug access (empty root password, root login, audit user, no console
    lockdown, `DIST-14`); not for devices that leave the developer's hands;
  - to `stable` from another flavor: the next install locks root and removes the development tools;
  - another version than installed: the persistent partitions (`overlay`, `application`) are kept as they are and
    may not suit the other version (backup first, `BAK-1`).
  Nothing is downloaded or written by choosing; the user installs with Update -> From network.
- **DVM-4 (P2)** OTA server: edit `ota_base_url` (same check as the Settings screen and `OTA-11`; HTTP only for a URL
  typed in explicitly), with "Reset to default" (`https://ota.xoros.io`). The flavor list (`DVM-2`) is reloaded from
  the new server.
- **DVM-5 (P2)** Effect of the overrides: while `developer_menu = true` and `ota_version`/`ota_suffix` are set,
  Update -> From network and `--cli update` (network source) take VERSION and SUFFIX from them instead of the slot
  (`OTA-1`); MACHINE always comes from the device. The Update dialog says "developer override" next to the values and
  still allows editing them for one check. USB updates (`USB-*`) are not affected: the stick decides what is
  offered, the switch warning of `OTA-1` applies there as before.
- **DVM-6 (P2)** Visible state: while any override (`ota_version`, `ota_suffix`, `skip_signature`, or an
  `ota_base_url` other than the default) is in effect, the status bar shows `DEV` and the Status screen lists the
  overrides. The system report and diagnostics bundle (`SYS-5`, `LOG-2`) include them.
- **DVM-7 (P2)** Skip signature check: `skip_signature = true` (toggle in the menu, confirmation) loads manifests
  from the network and from sticks without checking `signature.json`, also when `require_signature = true`
  (`XC-58`). SHA-256 checks of chunks and images stay. Every update check and write in this mode logs a warning
  (`LOG-3`) and the update dialogs show "Signature check OFF (developer menu)".
- **DVM-8 (P2)** "Hide developer menu" sets `developer_menu = false` and clears `ota_version`, `ota_suffix` and
  `skip_signature` in the same save, so a hidden menu never leaves an invisible override behind. When the file has
  overrides but `developer_menu = false` (edited by hand), the overrides are ignored and the Settings screen warns.
  "Reset developer settings" clears the overrides and keeps the menu. Factory reset with "reset rescue settings"
  (`RST-1`) removes everything, as for every setting.
- **DVM-9 (P2)** Access: every change in the menu is a settings write and needs an admin session (`SEC-1`); viewing
  the flavor list is open while the menu is visible.
- **DVM-10 (P2)** CLI (`ARCH-2`): `--cli dev list [--base <url>] [--json]` (the `DVM-2` list), `--cli dev status
  [--json]` (overrides in effect), `--cli dev set <version> <suffix>`, `--cli dev skip-signature on|off`,
  `--cli dev reset`, `--cli dev hide`. The keys can also be set with `--cli settings set`; the same checks apply
  (`ota_suffix` one of the four suffixes, `ota_version` path-safe, `DVM-1` for `developer_menu`).
- **DVM-11 (P2)** Testing without the real server needs no special code: a local mirror made by the distro's
  `scripts/publish-ota.sh <ota dir> <mirror dir>` (`XD-8`, `XD-9`; it writes the index too), served with
  `python3 -m http.server -d <mirror dir> 8000` and `ota_base_url = "http://127.0.0.1:8000"`, works in simulation
  (`SIM-1`) and on a device.

## 7. User interface requirements

- **UI-1 (P1)** Toolkit: Rust + `ratatui` (crossterm backend). Dialogs, forms, lists and the menu bar come from a
  small widget layer in `ui/tk/` on top of ratatui. Use the same `crossterm` version as ratatui's backend (the
  subshell, `SH-1`, takes over keys that its event reader already read).
- **UI-2 (P1)** Keyboard: `Esc` menu, arrows/Tab to move, Enter to select, `q`/`Esc` to close a dialog. Shortcuts
  shown in the status bar. `Ctrl-C` MUST NOT exit the app silently (it is restarted by systemd anyway); show
  "Use Power -> ... or Tools -> Subshell".
- **UI-3 (P1)** Long operations (hashing, downloads, writes, tests) run in worker threads; the UI stays responsive,
  shows a progress bar (percent, bytes, speed, ETA) and a Cancel button. Only one write operation at a time on the
  boot disk.
- **UI-4 (P1)** Destructive actions (writing slots, import/replace, factory reset, mkfs, stick write test) show what
  will be overwritten (device, partition label, current content version) and require confirmation; the most
  dangerous ones (factory reset, mkfs, A+B write, stick test) require typing a word.
- **UI-5 (P1)** Errors are shown in a dialog with a short explanation and the underlying error text; the app never
  panics on bad input (no `unwrap()` on I/O, parsing or external command results). A panic hook MUST restore the
  terminal and log the panic.
- **UI-6 (P1)** All texts in English, kept in one module so they can be translated later.
- **UI-7 (P2)** A bottom log line/panel shows the last messages of background tasks.

## 8. Non-functional requirements

- **NF-1 (P1)** Rust edition 2021. The distro builds with **Rust 1.95** or newer (`meta-rust-bin`, `cargo_bin`
  class, `XD-7` in `../xoros-xor-tool/REQUIREMENTS.md`), so the MSRV is 1.95 and `Cargo.lock` MUST pin crate versions that build
  with 1.95 (`cargo +1.95 build` in CI). Raised from 1.78 together with `xoros-core` (`ureq` 3, `k256` 0.14, `sha2`
  0.11 need Rust 1.85+). Crates are fetched by cargo during the Yocto build; no git dependencies that need SSH.
- **NF-2 (P1)** Avoid C dependencies that need extra Yocto recipes: TLS via `rustls` with the system CA bundle
  (`/etc/ssl/certs/ca-certificates.crt`) or `webpki-roots`; gzip via `flate2` with the pure Rust backend; no OpenSSL
  linking. `ncurses` is available but not needed with crossterm.
- **NF-3 (P1)** Static facts from section 4 (labels, paths, variable names, URLs) live in one constants module.
- **NF-4 (P1)** Data safety: every write to the boot disk is preceded by checks (`DEV-1`, size, magic) and followed by
  `fsync` + verification. No write ever targets a whole-disk device, the `boot` or the `rescue` partition (except
  grubenv updates per `ENV-2` and `admin.cfg` per `SEC-4`). Implement this as a single guarded "open partition for
  write" function that every writer uses.
- **NF-5 (P1)** Memory: works on 1 GiB RAM devices; downloads are streamed chunk by chunk (max one chunk in memory),
  tar archives streamed.
- **NF-6 (P2)** Start-up time to the Status screen under 2 s; slow information (hashes, network checks) is filled in
  asynchronously.
- **NF-7 (P1)** Binary size: release build with `lto = true`, `opt-level = "s"`, `strip = true`, target < 15 MiB.

## 9. Architecture (recommended)

### 9.1 Shared crate `xoros-core`

The OTA, boot-control and device logic is shared with the `xor` command-line tool of the normal system through the
library crate **`xoros-core`**, specified in `../xoros-xor-tool/REQUIREMENTS.md` (IDs `XC-*`). The rescue shell MUST use it
instead of re-implementing these parts, so manifests, signatures, grubenv handling and the write guard behave the same
in both tools and are tested once.

- **CORE-1 (P1)** Dependency: `xoros-core = { git = "ssh://git@github.com/xoros-repo/xoros-xor-tool.git", rev = "<commit>" }`,
  default features (`http`, `device`). The repository is private: developers need read access (organization account
  or deploy key) and an SSH agent (`XC-2`). The Yocto build does not let cargo fetch it; the recipe fetches the same
  commit with bitbake and redirects the dependency with a cargo `[patch]` (`XD-6`), so the URL string above MUST NOT
  change without updating the recipe. Pin `rev`; update it
  deliberately.
- **CORE-2 (P1)** What comes from the crate (the rescue shell adds UI, policy and user confirmations on top):

  | Rescue shell requirement | `xoros-core` |
  |---|---|
  | DEV-1..4 (boot disk, partitions), NF-4 (guarded writer) | `device::boot_disk`, `Layout`, `PartitionWriter` (XC-40..44) |
  | ENV-1..3, BOOT-1..7, ROT-1 (grubenv values), PWR-1 (`NEXT_BOOT`) | `grubenv::GrubEnv`, `boot::BootState`, `BootPartition` (XC-30..36) |
  | SYS-1 build info and rescue reason, OTA-1 | `env::BuildInfo`, `BootState::rescue_reason`, `device::slot_info` (XC-21..23, 33, 45) |
  | OTA-2..8, OTA-11, USB-2..4 | `manifest`, `signature`, `source::{HttpSource, DirSource, FileSource}`, `flash::{update_slot, copy_slot}` (XC-50..72) |
  | DVM-2, DVM-7 (flavor index, skip signature) | `index::{FlavorIndex, fetch_index}`, `SignaturePolicy::skip` (XC-58, XC-110..112) |
  | TST-4, TST-5 (squashfs size, hashing) | `squashfs::Superblock`, `PartitionWriter::hash_range` (XC-43, 44) |
  | NET-4 (connectivity part), NET-6/NET-8 (keyfile format) | `net::connectivity_check`, `net::nm_keyfile` (XC-90, 91; P2 in the crate, the rescue shell MAY implement them locally until then and move them later) |

- **CORE-3 (P1)** Stays in the rescue shell: UI, settings, backup/restore (BAK-*), factory reset, rescue-side network
  configuration (networkd/wpa_supplicant/DNS/SNTP), hardware tests, logs/diagnostics, subshell, SSH, power actions
  (`systemctl`), simulation directory handling (SIM-*, which configures the crate through `SystemPaths`, XC-15).
- **CORE-4 (P1)** The crate API is blocking with progress callbacks and a cancel token (XC-12, XC-13): run every crate
  operation that touches disk or network in a worker thread (`tasks.rs`) and forward progress to the UI.
- **CORE-5 (P1)** The crate's `log` output is routed into the rescue shell log (LOG-3).
- **CORE-6 (P1)** Missing or wrong behaviour in the crate is fixed in the crate (`xoros-xor-tool` repository), not
  worked around in the rescue shell.

### 9.2 Module layout

```
src/
  main.rs            arg parsing (--cli, --sim), panic hook, starts UI or CLI
  constants.rs       rescue shell paths and defaults; device/boot facts come from xoros_core::consts (NF-3)
  device.rs          thin wrapper: SystemPaths for real/sim mode, mount helpers for sticks (DEV-*, uses xoros-core)
  slots.rs           slot summary for the UI: xoros_core build info + integrity checks (SYS-1, TST-4/5)
  update.rs          OTA/USB update flows: target selection, confirmations, calls xoros_core::flash (OTA-*, USB-*)
  backup/            pax tar export/import with xattrs, split files (BAK-*)
  net/               networkd/.network files, wpa_supplicant, dhcp fallback, DNS, SNTP, tests, NM keyfiles (NET-*)
  tests/             memtest, disk read, fsck, smart (TST-*)
  report.rs          system report / diagnostics bundle (SYS-5, LOG-2)
  tasks.rs           background job runner with progress + cancel (UI-3)
  ui/                screens and dialogs, one module per menu; ui/tk/ widget layer on ratatui
```

- **ARCH-1 (P1)** Business logic (everything outside `ui/`) MUST NOT depend on `ratatui`, so it can be unit tested and
  driven by a CLI.
- **ARCH-2 (P2)** Non-interactive CLI for automation and serial-console use, e.g.
  `xoros-rescue-shell --cli status --json`, `... --cli boot reset-counter a`, `... --cli boot next rootfs_b`,
  `... --cli update --from /run/media/... --slot a`, `... --cli backup export --to <dir>`. Exit code 0 on success.

## 10. Development and simulation mode

The app MUST be developable on a normal Linux PC without a device and without the distro repository.

- **SIM-1 (P1)** `--sim <dir>` (or env `XOROS_RESCUE_SIM=<dir>`): instead of real block devices, use files in `<dir>`:
  `boot.img` (vfat), `rootfs_a.img`, `rootfs_b.img`, `rescue.img`, `overlay.img`, `application.img` (ext4),
  `sticks/<name>/` (directories that act as mounted external sticks). Mount operations are replaced by loop mounts
  (when root) or by reading the images directly (squashfs superblock, `xoros.env` via `unsquashfs -cat` if present).
  Power actions only log. Network actions run against the real host network but write files under `<dir>/run`.
- **SIM-2 (P1)** Provide `scripts/make-sim.sh` in the app repo that creates a simulation directory:

  ```sh
  #!/bin/sh -e
  D=${1:-sim}; mkdir -p "$D/sticks/usb1" "$D/tmp/root"
  printf 'DISTRO=xoros\nVERSION=1.0\nMACHINE=qemux86-64\nSUFFIX=devel\nTIMESTAMP=20261001120000\nRESCUE_IMAGE_SHA256=0\n' > "$D/tmp/root/xoros.env"
  mksquashfs "$D/tmp/root" "$D/rootfs_a.img" -noappend -quiet
  cp "$D/rootfs_a.img" "$D/rootfs_b.img"; truncate -s 1G "$D/rootfs_a.img" "$D/rootfs_b.img"
  mksquashfs "$D/tmp/root" "$D/rescue.img" -noappend -quiet
  truncate -s 64M "$D/boot.img"; mkfs.vfat -n BOOT "$D/boot.img"
  # 1024-byte grubenv
  { printf '# GRUB Environment Block\nROOTFS_A_TRY=3\nROOTFS_B_TRY=3\nLAST_BOOT=ROOTFS_B\n'; } > "$D/tmp/grubenv"
  truncate -s 1024 "$D/tmp/grubenv"; sed -i 's/\x0/#/g' "$D/tmp/grubenv"
  mmd -i "$D/boot.img" ::EFI ::EFI/BOOT; mcopy -i "$D/boot.img" "$D/tmp/grubenv" ::EFI/BOOT/grubenv
  truncate -s 256M "$D/overlay.img"; mkfs.ext4 -q -L overlay "$D/overlay.img"
  truncate -s 512M "$D/application.img"; mkfs.ext4 -q -L application "$D/application.img"
  ```
  (Requires `squashfs-tools`, `dosfstools`, `mtools`, `e2fsprogs`.) An OTA test tree can be produced with the `xor`
  tool (github.com/xoros-repo/xoros-xor-tool): `xor update create manifest -v 1.0 -m qemux86-64 -s devel`, then
  `xor update create image -b <timestamp> --split 22000000 <squashfs>` (run in the output directory).
- **SIM-3 (P2)** Real-device testing without the distro repo: ask the distro maintainer for a `*.wic` image of the
  target machine (or use one from a release). In QEMU (x86-64 + OVMF) select "Rescue" in the GRUB menu. To test a new
  app build on a device, copy the binary to `/run` on the rescue system (e.g. via `SSH-1` or a stick) and run it from
  the subshell; the installed binary is on read-only squashfs.

## 11. Testing and acceptance

- **TEST-1 (P1)** Unit tests: grubenv parse/serialize (round trip keeps unknown vars, size stays 1024, invalid values
  rejected), manifest parse and checks, chunk/offset logic incl. short last chunk, squashfs superblock parsing, NM
  keyfile generation, `.network` file generation, tar export/import round trip with xattrs/whiteouts/hard links
  (run as root in CI or in a user namespace), boot disk detection from fixture sysfs/mountinfo trees.
- **TEST-2 (P1)** Integration tests in simulation mode (`SIM-1`): full OTA write from a local HTTP server and from a
  directory to `rootfs_b.img`, including interrupt + resume (`OTA-6`) and a corrupted chunk (must fail and leave the
  slot disabled).
- **TEST-3 (P1)** Acceptance on QEMU x86-64 and Raspberry Pi 4 (with the DSI panel):
  1. Both counters at 3 -> device boots rescue -> Status says "both slots exceeded MAX_TRIES".
  2. Reset counter A + reboot -> slot A boots.
  3. OTA update of slot B over Ethernet and over Wi-Fi; boot B via "reboot into slot B".
  4. Update A+B from a FAT32 stick with an `ota/` tree.
  5. Export both persistent partitions to FAT32 stick (application > 4 GiB of data forces a split), factory reset,
     import, normal system comes back with the same `/etc` changes and Docker containers.
  6. Configure Wi-Fi in rescue, copy to normal system, boot normal system: Wi-Fi connects without further input.
  7. Memory test 1 pass and disk read test complete; results in the diagnostics bundle on a stick.
  8. Screen at 80x24 and at 100x30 with `ROTATION=1` is fully usable.
  9. Pull the network cable during an OTA download, reconnect, resume completes and verifies.
  10. With an admin password (4.7): both counters at `MAX_TRIES` and the menu hidden, the device still boots slot B or
      rescue without input; in GRUB, `e` and `c` ask for `admin` and the password; in the rescue shell, every
      admin-only action of `SEC-1` asks for the password and the idle timeout locks the session; deleting `admin.cfg`
      makes the build default apply, a corrupted `admin.cfg` too (GRUB and rescue shell agree); `MENU_MODE=once` shows
      the menu exactly once.
  11. Developer menu (`DVM-*`): OTA server set to a local mirror, the flavor list read from its index, update of slot B
      from the mirror, boot B; hiding the menu clears the overrides. `update check` and `update start` show the
      signature status (`OTA-8`) that follows from the mirror and the image's trusted keys: verified when the image
      trusts the build's key, "signed, not checked" when it trusts no key, unsigned without `signature.json`; signed by
      a key the image does not trust, the update is refused and goes through only with the developer menu's signature
      check off (`DVM-7`, shown as "Signature check OFF").
  12. Signatures (`OTA-8`, `DVM-7`) on an image that trusts the build's signing key (`DIST-5`): a correctly signed
      mirror is shown as verified and slot B is updated from it. Refused by `update check` and `update start`, slot B
      left as it was: `signature.json` with a corrupted `ms`, `manifest.json` changed after signing (`mh`), signed by
      a key the image does not trust, and, with `require_signature = true`, no `signature.json`. With
      `require_signature = false` an unsigned mirror is accepted and shown as unsigned. With the developer menu's
      signature check off, the corrupted mirror is accepted and shown as "Signature check OFF".

  Automated in QEMU (x86-64) by `../xoros-distro/tests/qemu` (`scripts/test.sh` there): item 3 over Ethernet together
  with item 11 by `test_ota.py` (boots an image older than the OTA build under test, publishes that build into a local
  mirror with `publish-ota.sh`); item 10 by `test_access.py` and `test_access_nohash.py`; item 12 by
  `test_ota_signature.py` (boots a signed image older than the OTA build under test, same signing key).

## 12. Distro-side dependencies

Changes in `xoros-distro` (meta-xoros) that the app needs or benefits from. Until they exist the app MUST degrade as
described in the corresponding requirement.

| ID | Change | Needed by | Without it |
|---|---|---|---|
| DIST-1 | Done. Run `xor boot ok` (today `mark-boot-ok.sh`) automatically when the normal system has booted successfully: `xoros-boot-ok.service`, see `XD-3` in `../xoros-xor-tool/REQUIREMENTS.md`. | `BOOT-*` | Counters grow on every normal boot; device falls into rescue after 6 boots. Health checks: executables in `/etc/xoros/boot-ok.d/` must pass first (`XD-3`). |
| DIST-2 | Done. Add `smartmontools` and `nvme-cli` to `xoros-rescue-image`. | `TST-7` | Only sysfs health info. |
| DIST-3 | Done. Make `/etc/resolv.conf` in the rescue image a symlink to `/run/xoros-rescue/resolv.conf` (or add `systemd-resolved`). | `NET-3` | Only the app resolves names; tools in the subshell do not. |
| DIST-4 | Done. Rescue kernel: `CONFIG_EXFAT_FS`, `CONFIG_VFAT_FS`, `CONFIG_NLS_CODEPAGE_437`, `CONFIG_NLS_ISO8859_1`, `CONFIG_NLS_UTF8`, USB mass storage (check they are built in or installed as modules). Add `dosfstools` to the x86 rescue image. | `USB-1`, `BAK-*` | exFAT sticks not supported. |
| DIST-5 | Done (opt-in): set `XOROS_OTA_SIGNING_KEY` (PEM path) for both builds; the rescue image installs the public key(s) in `/etc/xoros/ota-trusted-keys` (plus `XOROS_OTA_TRUSTED_KEYS`). Sign `manifest.json` in `do_ota_create` (`xor update create signature -s <key>`), publish `signature.json`, and ship the public key with the rescue shell settings. | `OTA-8` | Only SHA-256 integrity, no authenticity. |
| DIST-6 | Done. x86: memtest86+ EFI binary on the boot partition plus a GRUB menu entry `--id=MENU_MEMTEST`. | `TST-2` | Userspace memory test only. |
| DIST-7 | Done. Rescue image: mask/fix `systemd-networkd-persistent-storage.service` (fails on the read-only root). | cosmetics | One failed unit in rescue. |
| DIST-8 | Bump `SRCREV` in `rescue-shell_git.bb` for each app release. The recipe installs `/usr/bin/xoros-rescue-shell` (via `cargo_bin`); the getty drop-in comes from `rescue-shell-service.bb`. | release | |
| DIST-9 | Done. Split `xor` into `xoros-core` + `xor` (same private repository) and update the recipes: `xor_git.bb` builds the `xor` workspace member, `xor boot` replaces `next-boot.sh`/`mark-boot-ok.sh` (`XD-1`, `XD-2`); `rescue-shell_git.bb` fetches `xoros-xor-tool` as a second `SRC_URI` (SSH, `SRCREV_core` = the `rev` in `Cargo.toml`) and adds the cargo `[patch]` (`XD-6`). | CORE-1 | The rescue shell cannot build against the crate. |
| DIST-10 | Normal images: NetworkManager is the only network manager for wired/Wi-Fi interfaces. systemd-conf's `80-wired.network` is removed from the normal rootfs (`remove_networkd_wired` in `xoros-image-core.bb`), otherwise systemd-networkd configures `eth*` first and NM only assumes it. systemd-networkd itself is masked there (its wait-online would hold `network-online.target` for 2 min). `net.ifnames=0` stays on the shared kernel command line, so `eth0`/`wlan0` match in both systems. Done. | `NET-6` | Exported keyfiles are loaded but never activated. |
| DIST-11 | grub.cfg: load only the listed variables and validate them (`BOOT-8`); `superusers`/`password_pbkdf2` with the build default and `source` of `EFI/BOOT/admin.cfg` as in 4.7; `--unrestricted` on every menu entry (incl. `MENU_MEMTEST`); `MENU_MODE` (`BOOT-9`) with the build default `XOROS_BOOT_MENU` (`shown` for now). | 4.7, `BOOT-8`, `BOOT-9` | Anybody at the console can edit the kernel command line in GRUB (e.g. `init=/bin/sh`). |
| DIST-12 | Build variable `XOROS_ADMIN_PASSWORD_HASH` (optional, `grub.pbkdf2.sha512...` only; the build fails on anything else, so a clear text password can never end up in an image): the `bootimg_efi_grubenv` wic plugin writes it into grub.cfg (placeholder line, like `CMDLINE_IMAGE`), the rescue image and the normal images install it as `/etc/xoros/admin-password` (`includes/admin-password.inc`; in the normal images `xor admin status` then sees what GRUB enforces). A hash is made with `grub-mkpasswd-pbkdf2` or `xor admin hash` (`XT-11`). | 4.7 | Only per-device passwords; a new device has no protection until one is set. |
| DIST-13 | Raspberry Pi: U-Boot autoboot cannot be interrupted (`CONFIG_BOOTDELAY=-2`, fragment `xoros-no-autoboot-prompt.cfg`), so the U-Boot shell is not reachable from the console. U-Boot is shared by all images of a build, so this is build wide: `XOROS_UBOOT_AUTOBOOT_PROMPT = "1"` in `local.conf` keeps the prompt for development builds. | 4.7 | The U-Boot shell bypasses GRUB and the password. |
| DIST-14 | Production images (`xoros-image-stable` and anything shipped to customers; devel/audit keep their debug access, `XOROS_CONSOLE_LOCKDOWN = "0"`): root locked (the build fails when the image features would give root an empty password or a password-less login), no `empty-root-password`/`allow-empty-password`/`allow-root-login`/`debug-tweaks`, no autologin getty; Weston `[keyboard] vt-switching=false` and no terminal or launcher in the kiosk session; `kernel.sysrq=0` (rescue image too); `sshd` key-only. | 4.7 | The normal system is the easier way in: a console user gets a root shell there. |
| DIST-15 | Deployment guide (documentation, not the image): x86 UEFI setup password and boot only from the internal disk (USB/PXE boot off or after the disk); on the Pi the EEPROM boot order cannot be locked, so USB ports and the SD slot need physical protection where this matters. | 4.7 | Booting another medium bypasses everything above. |
| DIST-16 | OTA artifacts are published to a Cloudflare R2 bucket served at `https://ota.xoros.io` (layout as in 6.3, unchanged for devices), replacing the Cloudflare Pages project, whose every deployment replaced the whole site with the board just built. Publishing merges the new build into the published `manifest.json` and keeps the two newest builds per `<VERSION>/<MACHINE>/<SUFFIX>`, plus pinned builds (`DIST-18`) (`XD-8` in `../xoros-xor-tool/REQUIREMENTS.md`). | `OTA-3` (downgrade to the previous build) | Only the newest build of one machine is online; no way back over the network. |
| DIST-17 | Publishing also maintains a flavor index per machine, `<base>/index/<MACHINE>.json`, listing every published `<VERSION>/<SUFFIX>` of that machine with its builds and which of them are pinned (`XD-9`, `XC-110` in `../xoros-xor-tool/REQUIREMENTS.md`). | `DVM-2` | The developer menu lists the four suffixes of the installed version unchecked. |
| DIST-18 | Pinned builds: a published build can be pinned (`publish-ota.sh --pin=<timestamp>`) and unpinned (`--unpin=`); a pinned build stays on the server and does not count towards the two builds kept by `DIST-16`, e.g. a release or a known-good build to go back to. The pin is a flag in the signed `manifest.json` (`XC-54`, `XC-83`, `XT-15`, `XD-10` in `../xoros-xor-tool/REQUIREMENTS.md`); devices that do not know it ignore it. | `OTA-3` | Only the two newest builds of a flavor are online. |

## 13. Current state of the app (for orientation)

Version 0.1.1-2 (SRCREV `b9e6e96` in the distro). Working: TUI start on tty1, menu bar, status bar, Recovery ->
Subshell. Stubs or broken: Configs (Backup/Restore/Reset dialogs only), Tests (cursive example code; Tests -> New
panics on `find_subtree("File")`), Recovery -> Force OTA flash (fake progress bar), `flash_chunk()` writes to
`ROOTFS_A.img` files in the current directory, Help -> About -> System Info shows placeholder text, Reboot only exits.
The `models` module holds an older manifest shape; replace it with the one in 6.3. Existing code MAY be rewritten.

## 14. Suggested delivery plan

1. **M1 (P1 core):** Status, Boot menu, Power, Subshell, logs, simulation mode, CLI `status`, on top of
   `xoros-core` C1 (boot disk detection, layout, grubenv/boot state, build info).
2. **M2:** OTA from network (incl. NET-1/3/4/7 basic Ethernet + time) and from stick, slot/rescue integrity tests,
   on top of `xoros-core` C2 (guarded writer, image sources, flash engine).
3. **M3:** Backup/restore, factory reset, fsck, Wi-Fi config, NM keyfile copy, memory and disk tests, diagnostics
   bundle.
4. **M4 (P2/P3):** resume, signatures, settings file, SSH remote support, SMART, stick test, rotation, PIN.
5. **M5 (P2):** access protection: admin session and password management (`SEC-*`), `MENU_MODE` (`BOOT-9`), on top
   of `xoros-core` C4 (`XC-37`, `XC-38`). Distro: `DIST-11`..`DIST-14` (grub.cfg, build default, U-Boot, image
   hardening), `DIST-15` as documentation.
6. **M6 (P2, distro):** OTA publishing to R2 with the previous build kept (`DIST-16`), on top of `xoros-core` C5
   (`XC-82`, `XT-12`). No app change: the app already lists several builds and offers a downgrade (`OTA-3`).
7. **M7 (P2):** developer menu (`DVM-*`), on top of `xoros-core` C6 (`XC-58`, `XC-110..112`). Distro: flavor index
   (`DIST-17`, `XD-9`).
8. **M8 (P2, mostly distro):** pinned builds (`DIST-18`), on top of `xoros-core` C7 (`XC-54`, `XC-83`, `XT-15`,
   `XD-10`). App: the "pinned" mark in the build list (`OTA-3`).
