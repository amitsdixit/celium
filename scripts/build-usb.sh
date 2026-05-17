#!/usr/bin/env bash
# W24-E — Write a Celium boot image to a USB stick.
#
# Companion to scripts/build-iso.sh. The ISO is bootable directly on
# modern UEFI machines, but on physical hardware you usually want a
# writable FAT32 stick (so logs, dumps, and configs survive across
# reboots) rather than a read-only ISO9660 disc.
#
# Usage:
#   sudo scripts/build-usb.sh /dev/sdX           # writes hybrid ISO
#   sudo scripts/build-usb.sh /dev/sdX --fat32   # mkfs + cp ESP
#
# Two modes:
#   * hybrid (default): `dd` the celium.iso onto the device. Read-only
#     stick but bootable on any UEFI box.
#   * fat32: `mkfs.fat -F 32` the device, then copy `build/esp/`
#     into it. Writable stick — install logs and dumps end up here.
#
# This script REFUSES to touch a path that is not a block device, is
# the root filesystem, or appears mounted. Removing the safety checks
# is a destructive shortcut and is not supported.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ISO="$REPO_ROOT/build/celium.iso"
ESP_DIR="$REPO_ROOT/build/esp"

usage() {
  echo "usage: $0 /dev/sdX [--fat32]" >&2
  echo "  --fat32   format the device as FAT32 and copy build/esp/ onto it" >&2
  echo "            (default: dd the celium.iso onto the device)" >&2
  exit 2
}

[[ $# -lt 1 ]] && usage
TARGET="$1"
MODE="hybrid"
if [[ "${2:-}" == "--fat32" ]]; then
  MODE="fat32"
elif [[ -n "${2:-}" ]]; then
  usage
fi

# ---- safety checks (refuse destructive defaults) ----
if [[ ! -b "$TARGET" ]]; then
  echo "error: $TARGET is not a block device." >&2
  exit 3
fi
if [[ "$TARGET" == "/" || "$TARGET" == "/dev/sda" || "$TARGET" == "/dev/nvme0n1" ]]; then
  echo "error: refusing to write to $TARGET (looks like the system disk)." >&2
  echo "       If you really mean it, override TARGET in your shell:" >&2
  echo "         CONFIRM_DESTROY=$TARGET $0 $TARGET ..." >&2
  if [[ "${CONFIRM_DESTROY:-}" != "$TARGET" ]]; then
    exit 3
  fi
fi
if grep -q "^$TARGET" /proc/mounts; then
  echo "error: $TARGET (or a partition) is currently mounted. Unmount first." >&2
  exit 3
fi
if [[ "$EUID" -ne 0 ]]; then
  echo "error: must run as root (writing to a raw block device)." >&2
  exit 3
fi

case "$MODE" in
  hybrid)
    if [[ ! -f "$ISO" ]]; then
      echo "error: $ISO missing; run scripts/build-iso.sh first." >&2
      exit 2
    fi
    echo "[w24-e-usb] writing $ISO → $TARGET (hybrid; read-only stick)"
    dd if="$ISO" of="$TARGET" bs=4M conv=fsync status=progress
    sync
    echo "[w24-e-usb] done."
    ;;
  fat32)
    if [[ ! -d "$ESP_DIR" ]]; then
      echo "error: $ESP_DIR missing; run scripts/run-qemu.sh first." >&2
      exit 2
    fi
    if ! command -v mkfs.fat >/dev/null 2>&1; then
      echo "error: mkfs.fat not installed (dosfstools)." >&2
      exit 2
    fi
    echo "[w24-e-usb] formatting $TARGET as FAT32 (label CELIUM)"
    # Single partition spanning the device; type EF00 (EFI system).
    parted -s "$TARGET" mklabel gpt
    parted -s "$TARGET" mkpart 'CELIUM' fat32 1MiB 100%
    parted -s "$TARGET" set 1 esp on
    # Wait for /dev/<name>1 to appear.
    PART="${TARGET}1"
    [[ -b "${TARGET}p1" ]] && PART="${TARGET}p1"
    udevadm settle || sleep 1
    mkfs.fat -F 32 -n CELIUM "$PART" >/dev/null

    MNT="$(mktemp -d)"
    mount "$PART" "$MNT"
    cp -r "$ESP_DIR"/* "$MNT"/
    sync
    umount "$MNT"
    rmdir "$MNT"
    echo "[w24-e-usb] FAT32 stick ready on $PART."
    ;;
esac
