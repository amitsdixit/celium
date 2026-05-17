# Celium installer guide

This document is the operator-facing companion to
[`scripts/build-installer.sh`](../scripts/build-installer.sh). It
explains how to build install media for Celium, how to choose between
ISO / USB / PXE, and how to verify a bare-metal boot succeeded.

## Choosing the medium

| Medium | When to pick it | Notes |
|---|---|---|
| **ISO** (hybrid) | Lab / dev VM, CD-ROM emulation, or fleet imaging | The W24 hybrid ISO is `dd`-able to USB and bootable as a CD-ROM with bit-identical content. |
| **USB** | A single physical box | Wipes the target disk. The script refuses `/dev/sda`, `/dev/nvme0n1`, etc. unless `CONFIRM_DESTROY=path` matches. |
| **PXE** | Fleet roll-out via DHCP/TFTP | Generates `build/pxe/` with `celium.efi`, `celhyper.elf`, `boot.ipxe`, and an operator README. |

## Build the artefacts

All three artefacts are produced by `scripts/build-installer.sh`:

```bash
# Everything:
scripts/build-installer.sh

# Just the hybrid ISO:
scripts/build-installer.sh iso

# PXE staging only:
scripts/build-installer.sh pxe

# ISO + write to USB (replace /dev/sdX):
scripts/build-installer.sh usb /dev/sdX
```

Prerequisites:

* `xorriso` for the ISO step.
* `mkfs.fat` + `parted` for USB FAT32 mode (the default `dd` mode
  needs neither).
* On Linux only \u2014 the installer step calls `xorriso` and `dd`.

## What the ISO actually contains

```
celium.iso (ISO9660 + hybrid GPT)
\u2514\u2500\u2500 celium-esp.img (FAT12, El Torito)
    \u2514\u2500\u2500 EFI/
        \u251c\u2500\u2500 BOOT/BOOTX64.EFI   <- CelLoader
        \u2514\u2500\u2500 CELIUM/CELHYPER.ELF <- the kernel
```

UEFI firmware boots `BOOTX64.EFI` (CelLoader). CelLoader probes
CPU / ACPI / framebuffer, builds the v3 handoff block, and jumps
into `CELHYPER.ELF`.

## Boot verification

1. Connect a serial console (USB-RS232 dongle, or a virtio-serial
   from QEMU). Celium logs to COM1 at 38400 8N1.
2. Power-on. Within ~3 seconds the log shows:

```text
celhyper: alive
celhyper: vmx runtime initialised
celhyper: installing host gdt+tss...
lapic_base=0xfee00000
lapic_id=0x0
smp_cpu_count=0x1
celhyper: bring_up complete
metrics_vm_exits_total=0x2
metrics_vm_exits_hlt=0x2
```

`metrics_vm_exits_hlt = 2` confirms both bring-up VMs ran to a clean
`HLT`. Any of the per-counter lines missing means the kernel did not
reach `metrics::log_snapshot()`, which is the last call before
`bring_up` returns.

## Joining a cluster

After single-node bring-up, hook the box into a CelMesh cluster by
running `celctl cluster start` against COM2 (the bridge). See
[runbooks/cluster-join.md](runbooks/cluster-join.md).

## Safety reminders

* `build-usb.sh` refuses to write to `/dev/sda`, `/dev/nvme0n1`, or
  any block device already in `/proc/mounts`.
* The PXE staging dir has no DHCP / TFTP server bundled; the README
  shows a dnsmasq quick-start.
* CelLoader does NOT partition the target disk \u2014 it boots the ESP
  on the install media itself. Persistent storage (CelVault on disk)
  is a Tenancy-Layer concern.
