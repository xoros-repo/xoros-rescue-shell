#!/bin/sh -e
# SIM-2: create a simulation directory for `xoros-rescue-shell --sim <dir>` (SIM-1).
#
#   scripts/make-sim.sh [<dir>] [<xoros-rescue-shell binary>]
#
# The app describes the simulated device in <dir>/sim.json (slots, grubenv, disks, sticks,
# fault injection; see src/backend/sim.rs); `--sim-init` writes a complete example with a
# 1024-byte grubenv (both counters at 3), slot/rescue images, persistent partitions as
# directories and a stick "usb1" with an OTA tree and a single image (USB-2). This script
# wraps it and, when the `xor` tool and squashfs-tools are installed, adds a second stick
# "usb2" with an OTA tree made by `xor` from a real squashfs.
D=${1:-sim}
BIN=${2:-}
if [ -z "$BIN" ]; then
    for b in ./target/debug/xoros-rescue-shell ./target/release/xoros-rescue-shell; do
        [ -x "$b" ] && BIN=$b && break
    done
fi
[ -n "$BIN" ] || BIN=xoros-rescue-shell
if [ -e "$D/sim.json" ]; then
    echo "$D/sim.json exists; remove $D first." >&2
    exit 1
fi
"$BIN" --sim-init "$D"

if command -v xor >/dev/null 2>&1 && command -v mksquashfs >/dev/null 2>&1; then
    T=$(mktemp -d)
    trap 'rm -rf "$T"' EXIT
    mkdir -p "$T/root"
    printf 'DISTRO=xoros\nVERSION=1.0\nMACHINE=qemux86-64\nSUFFIX=devel\nTIMESTAMP=20261101120000\nRESCUE_IMAGE_SHA256=0\n' \
        > "$T/root/xoros.env"
    mksquashfs "$T/root" "$T/rootfs.squashfs" -noappend -quiet
    OTA="$D/sticks/usb2/ota/1.0/qemux86-64/devel"
    mkdir -p "$OTA"
    (cd "$OTA" && xor update create manifest -v 1.0 -m qemux86-64 -s devel \
        && xor update create image -b 20261101120000 --split 22000000 "$T/rootfs.squashfs")
    echo "OTA tree: $OTA"
else
    echo "(xor or mksquashfs not found: no second stick with an xor-made OTA tree)"
fi
echo "Run: $BIN --sim $D"
