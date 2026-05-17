#!/usr/bin/env bash
# W23-D — wrap the existing `build/esp/` directory in a bootable
# ISO9660 image so Celium can be tested in any QEMU build that
# accepts `-cdrom`, and on physical hardware via USB or PXE boot.
#
# This is the *plumbing* half of bare-metal preparation; W23-G will
# add an installer (write the ESP to a real disk, register a boot
# entry, optional partitioning) and validate on physical hardware.
#
# Usage:
#   scripts/build-iso.sh                  # produces build/celium.iso
#   scripts/build-iso.sh smoke            # builds + boots once in QEMU
#
# Requirements:
#   * `xorriso` (most Linux distros: `apt-get install xorriso`)
#   * `build/esp/EFI/BOOT/BOOTX64.EFI` and
#     `build/esp/EFI/CELIUM/CELHYPER.ELF` — created by the existing
#     `scripts/run-qemu.sh` build pipeline.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ESP_DIR="$REPO_ROOT/build/esp"
ISO_OUT="$REPO_ROOT/build/celium.iso"
ESP_IMG="$REPO_ROOT/build/celium-esp.img"

if [[ ! -f "$ESP_DIR/EFI/BOOT/BOOTX64.EFI" ]]; then
  echo "error: $ESP_DIR/EFI/BOOT/BOOTX64.EFI missing." >&2
  echo "       Run scripts/run-qemu.sh first to build CelLoader + CelHyper." >&2
  exit 2
fi

if ! command -v xorriso >/dev/null 2>&1; then
  echo "error: xorriso not installed; apt-get install xorriso" >&2
  exit 2
fi

# Stage 1 — wrap the EFI System Partition as a FAT image. xorriso's
# `-efi-boot` expects a single file, not a directory, so we mkfs a
# small FAT image and copy the ESP into it.
echo "[w23-d-iso] staging FAT EFI partition image..."
ESP_SIZE_KB=$(du -sk "$ESP_DIR" | awk '{print $1}')
# Round up to nearest MiB + 1 MiB headroom; mkfs.fat needs >= 1 MiB.
ESP_IMG_KB=$(( ((ESP_SIZE_KB / 1024) + 2) * 1024 ))
dd if=/dev/zero of="$ESP_IMG" bs=1024 count="$ESP_IMG_KB" status=none
mkfs.fat -F 12 -n CELIUM "$ESP_IMG" >/dev/null
# mtools copy preserves directory structure without requiring root.
mcopy -i "$ESP_IMG" -s "$ESP_DIR"/* ::/

# Stage 2 — wrap the FAT image in an El Torito ISO9660 that any
# EFI firmware can boot via `-cdrom`. W24-E: also emit an isohybrid
# GPT signature so the same artefact `dd`s cleanly to a USB stick.
echo "[w24-e-iso] building hybrid ISO9660 wrapper..."
xorriso -as mkisofs \
  -V 'CELIUM' \
  -e celium-esp.img \
  -no-emul-boot \
  -isohybrid-gpt-basdat \
  -o "$ISO_OUT" \
  -graft-points "celium-esp.img=$ESP_IMG" \
  >/dev/null

echo "[w24-e-iso] wrote $ISO_OUT ($(du -h "$ISO_OUT" | awk '{print $1}'))"
echo "[w24-e-iso] hybrid image — bootable as both ISO9660 and dd-able to USB."

if [[ "${1:-}" == "smoke" ]]; then
  : "${QEMU:=qemu-system-x86_64}"
  : "${OVMF_CODE:=/usr/share/OVMF/OVMF_CODE_4M.fd}"
  : "${OVMF_VARS_TEMPLATE:=/usr/share/OVMF/OVMF_VARS_4M.fd}"
  echo "[w23-d-iso] booting ISO under QEMU (smoke; 10s timeout)..."
  cp "$OVMF_VARS_TEMPLATE" "$REPO_ROOT/build/OVMF_VARS.iso.fd"
  timeout 10 "$QEMU" \
    -accel kvm -cpu host,+vmx \
    -drive pflash:"$OVMF_CODE" \
    -drive pflash:"$REPO_ROOT/build/OVMF_VARS.iso.fd" \
    -cdrom "$ISO_OUT" \
    -serial stdio -no-reboot -display none \
    2>&1 | tee "$REPO_ROOT/build/iso-smoke.log" || true
  if grep -q "celhyper: alive" "$REPO_ROOT/build/iso-smoke.log"; then
    echo "[w23-d-iso] SMOKE OK — kernel reached 'celhyper: alive' from the ISO"
  else
    echo "[w23-d-iso] SMOKE FAILED — kernel did not log 'alive' within 10s" >&2
    exit 3
  fi
fi
