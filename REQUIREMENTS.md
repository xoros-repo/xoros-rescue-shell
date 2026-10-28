# XorOS Rescue Shell: Technical Implementation Requirements

Status: draft 2 (2026-10-27). Target app: `xoros-rescue-shell` from github.com/xoros-repo/xoros-rescue-shell.
Related: `../xor/REQUIREMENTS.md` (shared crate `xoros-core` and the `xor` tool, see [section 9.1](#91-shared-crate-xoros-core)).

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
network) bring the device back to a working state without another computer.

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
2. `load_env`: every variable in grubenv overrides the defaults above (so `ORDER`, `MAX_TRIES` and `ROTATION` can be
   changed by writing them to grubenv).
3. If `NEXT_BOOT` is set (`MENU_ROOTFS_A`, `MENU_ROOTFS_B` or `MENU_RESCUE`), boots that entry once and clears it
   (`save_env NEXT_BOOT` with an empty value).
4. Otherwise picks the first slot in `ORDER` whose `<SLOT>_TRY` is lower than `MAX_TRIES`; if none, picks `Rescue`.
5. The menu is shown for 3 seconds (`timeout=3`), entries: `RootFS Slot A (TRY=n)`, `RootFS Slot B (TRY=n)`, `Rescue`.
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

Unknown variables MUST be preserved when writing.

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
  `python3`, `nodejs`, `git`. `dosfstools` is present on the Pi only. **Not** present: `lsblk`, `smartctl`,
  `nvme`, `memtester`, `stress-ng`, `fio`, `curl` CLI (only `libcurl`), NetworkManager, the `xor` tool (the rescue shell links `xoros-core` instead, 9.1).
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
Tools             logs, diagnostics bundle, subshell, remote access (LOG-*, SH-*, SSH-*)
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

Requirements:

- **OTA-1 (P1)** Determine `VERSION`, `MACHINE`, `SUFFIX` from the newest valid slot's `xoros.env`. If no slot is
  valid, ask the user (MACHINE preset from the running rescue system's hostname `xoros-rescue-<machine>`; VERSION and
  SUFFIX entered/selected). The user MAY switch SUFFIX (e.g. `devel` -> `stable`) with a warning.
- **OTA-2 (P1)** Fetch the manifest, check `machine`, `version` and `suffix` match. Mismatch is an error.
- **OTA-3 (P1)** List `rootfs` images with formatted build timestamp, size, and whether it is newer/same/older than
  each slot. Default selection: the newest. Installing the same or an older build is allowed in rescue but needs an
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
  field from the server. Today the build does not publish `signature.json` (see `DIST-5`).
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
  in clear text. File mode `0600`.

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
  restore the TUI on exit (existing feature, keep).
- **SSH-1 (P2)** Remote support: start `sshd` (installed, normally not running) on demand, with a one-time root
  password generated and shown on screen (set via `chpasswd` into a writable copy, or a dedicated `sshd_config` in
  `/run/xoros-rescue/` with `PasswordAuthentication yes` and an `AuthorizedKeysFile` the user can load from a stick).
  Show the IP addresses. Stop on demand and when the app exits.

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
  theme = "default"
  ```

  The app MUST work when the overlay partition is missing or broken (settings are then read-only defaults).
- **CFG-2 (P3)** Optional access protection: a PIN (hash stored in the settings file) required before destructive
  actions and the subshell. Off by default.

## 7. User interface requirements

- **UI-1 (P1)** Toolkit: Rust + `cursive` (crossterm backend), as in the current code base.
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

- **NF-1 (P1)** Rust edition 2021. The distro builds with **Rust 1.78** (`meta-rust-bin`, `cargo_bin` class), so the
  MSRV is 1.78 and `Cargo.lock` MUST pin crate versions that build with 1.78 (`cargo +1.78 build` in CI). Crates are
  fetched by cargo during the Yocto build; no git dependencies that need SSH.
- **NF-2 (P1)** Avoid C dependencies that need extra Yocto recipes: TLS via `rustls` with the system CA bundle
  (`/etc/ssl/certs/ca-certificates.crt`) or `webpki-roots`; gzip via `flate2` with the pure Rust backend; no OpenSSL
  linking. `ncurses` is available but not needed with crossterm.
- **NF-3 (P1)** Static facts from section 4 (labels, paths, variable names, URLs) live in one constants module.
- **NF-4 (P1)** Data safety: every write to the boot disk is preceded by checks (`DEV-1`, size, magic) and followed by
  `fsync` + verification. No write ever targets a whole-disk device, the `boot` or the `rescue` partition (except
  grubenv updates per `ENV-2`). Implement this as a single guarded "open partition for write" function that every
  writer uses.
- **NF-5 (P1)** Memory: works on 1 GiB RAM devices; downloads are streamed chunk by chunk (max one chunk in memory),
  tar archives streamed.
- **NF-6 (P2)** Start-up time to the Status screen under 2 s; slow information (hashes, network checks) is filled in
  asynchronously.
- **NF-7 (P1)** Binary size: release build with `lto = true`, `opt-level = "s"`, `strip = true`, target < 15 MiB.

## 9. Architecture (recommended)

### 9.1 Shared crate `xoros-core`

The OTA, boot-control and device logic is shared with the `xor` command-line tool of the normal system through the
library crate **`xoros-core`**, specified in `../xor/REQUIREMENTS.md` (IDs `XC-*`). The rescue shell MUST use it
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
  ui/                cursive screens and dialogs, one module per menu
```

- **ARCH-1 (P1)** Business logic (everything outside `ui/`) MUST NOT depend on `cursive`, so it can be unit tested and
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

## 12. Distro-side dependencies

Changes in `xoros-distro` (meta-xoros) that the app needs or benefits from. Until they exist the app MUST degrade as
described in the corresponding requirement.

| ID | Change | Needed by | Without it |
|---|---|---|---|
| DIST-1 | Run `xor boot ok` (today `mark-boot-ok.sh`) automatically when the normal system has booted successfully: `xoros-boot-ok.service`, see `XD-3` in `../xor/REQUIREMENTS.md`. | `BOOT-*` | Counters grow on every normal boot; device falls into rescue after 6 boots. |
| DIST-2 | Add `smartmontools` and `nvme-cli` to `xoros-rescue-image`. | `TST-7` | Only sysfs health info. |
| DIST-3 | Make `/etc/resolv.conf` in the rescue image a symlink to `/run/xoros-rescue/resolv.conf` (or add `systemd-resolved`). | `NET-3` | Only the app resolves names; tools in the subshell do not. |
| DIST-4 | Rescue kernel: `CONFIG_EXFAT_FS`, `CONFIG_VFAT_FS`, `CONFIG_NLS_CP437`, `CONFIG_NLS_ISO8859_1`, `CONFIG_NLS_UTF8`, USB mass storage (check they are built in or installed as modules). Add `dosfstools` to the x86 rescue image. | `USB-1`, `BAK-*` | exFAT sticks not supported. |
| DIST-5 | Sign `manifest.json` in `do_ota_create` (`xor update create signature -s <key>`), publish `signature.json`, and ship the public key with the rescue shell settings. | `OTA-8` | Only SHA-256 integrity, no authenticity. |
| DIST-6 | x86: memtest86+ EFI binary on the boot partition plus a GRUB menu entry `--id=MENU_MEMTEST`. | `TST-2` | Userspace memory test only. |
| DIST-7 | Rescue image: mask/fix `systemd-networkd-persistent-storage.service` (fails on the read-only root). | cosmetics | One failed unit in rescue. |
| DIST-8 | Bump `SRCREV` in `rescue-shell_git.bb` for each app release. The recipe installs `/usr/bin/xoros-rescue-shell` (via `cargo_bin`); the getty drop-in comes from `rescue-shell-service.bb`. | release | |
| DIST-9 | Split `xor` into `xoros-core` + `xor` (same private repository) and update the recipes: `xor_git.bb` builds the `xor` workspace member, `xor boot` replaces `next-boot.sh`/`mark-boot-ok.sh` (`XD-1`, `XD-2`); `rescue-shell_git.bb` fetches `xoros-xor-tool` as a second `SRC_URI` (SSH, `SRCREV_core` = the `rev` in `Cargo.toml`) and adds the cargo `[patch]` (`XD-6`). | CORE-1 | The rescue shell cannot build against the crate. |
| DIST-10 | Normal images: NetworkManager is the only network manager for wired/Wi-Fi interfaces. systemd-conf's `80-wired.network` is removed from the normal rootfs (`remove_networkd_wired` in `xoros-image-core.bb`), otherwise systemd-networkd configures `eth*` first and NM only assumes it. `net.ifnames=0` stays on the shared kernel command line, so `eth0`/`wlan0` match in both systems. Done. | `NET-6` | Exported keyfiles are loaded but never activated. |

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
