#!/usr/bin/env bash
# W25-G \u2014 unified installer builder.
#
# One-shot wrapper that drives the three artefact builders in sequence
# so an operator can produce every bare-metal install medium with a
# single command:
#
#   scripts/build-installer.sh             # all three artefacts
#   scripts/build-installer.sh iso         # only the hybrid ISO
#   scripts/build-installer.sh pxe         # only the PXE stage
#   scripts/build-installer.sh usb /dev/sdX  # write USB after ISO build
#
# Pre-requisites are the same as the underlying scripts; this wrapper
# only orchestrates and centralises error handling.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPTS="$REPO_ROOT/scripts"
mode="${1:-all}"

build_iso() {
  echo "[w25-installer] === iso ==="
  "$SCRIPTS/build-iso.sh"
}

build_pxe() {
  echo "[w25-installer] === pxe ==="
  "$SCRIPTS/build-pxe.sh"
}

write_usb() {
  local target="${1:-}"
  if [[ -z "$target" ]]; then
    echo "error: 'usb' mode needs a target device (e.g. /dev/sdX)" >&2
    exit 2
  fi
  echo "[w25-installer] === usb -> $target ==="
  "$SCRIPTS/build-usb.sh" "$target"
}

case "$mode" in
  all)
    build_iso
    build_pxe
    echo "[w25-installer] all artefacts built; see build/celium.iso and build/pxe/"
    ;;
  iso) build_iso ;;
  pxe) build_pxe ;;
  usb)
    build_iso
    write_usb "${2:-}"
    ;;
  *)
    echo "usage: $0 [all|iso|pxe|usb /dev/sdX]" >&2
    exit 2
    ;;
esac
