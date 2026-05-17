#!/usr/bin/env bash
# W24-E — Stage a Celium PXE boot tree under build/pxe/.
#
# Produces:
#   build/pxe/celium.efi                  — CelLoader, served as
#                                            BOOTX64.EFI to UEFI PXE.
#   build/pxe/celhyper.elf                — kernel image, fetched by
#                                            CelLoader once it starts.
#   build/pxe/boot.ipxe                   — iPXE script template.
#   build/pxe/README.md                   — operator instructions.
#
# The script does NOT run a TFTP / iPXE server itself. It assembles a
# directory you can point dnsmasq, in.tftpd, or iPXE's HTTP serving at:
#
#   dnsmasq --enable-tftp --tftp-root=$PWD/build/pxe \
#           --dhcp-boot=celium.efi ...
#
# Or, if your DHCP server already advertises iPXE, point
# `chain http://<host>/boot.ipxe` at:
#
#   build/pxe/boot.ipxe

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ESP_DIR="$REPO_ROOT/build/esp"
OUT="$REPO_ROOT/build/pxe"

if [[ ! -f "$ESP_DIR/EFI/BOOT/BOOTX64.EFI" ]]; then
  echo "error: $ESP_DIR/EFI/BOOT/BOOTX64.EFI missing." >&2
  echo "       Run scripts/run-qemu.sh first to build CelLoader + CelHyper." >&2
  exit 2
fi

mkdir -p "$OUT"
cp -f "$ESP_DIR/EFI/BOOT/BOOTX64.EFI"    "$OUT/celium.efi"
cp -f "$ESP_DIR/EFI/CELIUM/CELHYPER.ELF" "$OUT/celhyper.elf"

cat > "$OUT/boot.ipxe" <<'EOF'
#!ipxe
# Celium W24-E iPXE boot script.
#
# Drop this on an HTTP server reachable from the target node and
# point DHCP option 67 (or your chained iPXE config) here. Adjust the
# `${base-url}` line to match your serving infrastructure.

set base-url http://${next-server}/celium

echo Celium PXE — fetching CelLoader from ${base-url}/celium.efi
chain --autofree ${base-url}/celium.efi
EOF

cat > "$OUT/README.md" <<'EOF'
# Celium PXE serving tree (W24-E)

This directory is the artefact root for booting Celium across a
network. It contains the pieces a UEFI client needs to chainload
straight into CelLoader, and a small `boot.ipxe` script for iPXE-
based fleets.

## Layout

| File              | Purpose                                                   |
|-------------------|-----------------------------------------------------------|
| `celium.efi`      | CelLoader (UEFI stage-0). Served as `BOOTX64.EFI`.        |
| `celhyper.elf`    | CelHyper kernel. CelLoader pulls this in once it starts.  |
| `boot.ipxe`       | iPXE chain script. Replace `${base-url}` with your server.|

## Quick start

```bash
# 1. Serve build/pxe/ over TFTP (and HTTP for the kernel image).
dnsmasq \
  --enable-tftp \
  --tftp-root=$(pwd)/build/pxe \
  --dhcp-boot=celium.efi \
  --interface=eth0 \
  --dhcp-range=10.0.0.10,10.0.0.50,255.255.255.0,12h
```

```bash
# 2. (alternative) Serve the iPXE chain over HTTP and rely on iPXE-
#    enabled clients to fetch boot.ipxe directly.
python3 -m http.server --directory build/pxe 8080
```

## Operator notes

* CelLoader fetches `\EFI\CELIUM\CELHYPER.ELF` relative to its own
  EFI image path. When chainloaded over PXE the path is conceptually
  `EFI\BOOT\BOOTX64.EFI` ⇒ `EFI\CELIUM\CELHYPER.ELF`; either keep the
  layout below intact or arrange for the same directory tree on the
  TFTP server.
* The PXE path is functionally identical to USB boot once CelLoader
  is running — the handoff block, ACPI probe, and ExitBootServices
  dance are the same. PXE just replaces the disk-read step.
* If a node fails to fetch `celhyper.elf`, CelLoader prints
  `[celloader] fatal: NotFound` to its UEFI console; capture the
  serial output via your BMC or a USB-to-RS232 dongle.
EOF

echo "[w24-e-pxe] PXE tree staged at $OUT"
ls -lh "$OUT"
