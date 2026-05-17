# W24 — Bare-metal boot, SMP, and driver foundations

Status: **W24 done.** Celium can now be staged as a bootable ISO,
USB stick, or PXE-served image. CelHyper accepts a v3 handoff
carrying SMP topology and an optional framebuffer; it registers the
BSP, reads the AP APIC id list, and gracefully degrades to
single-CPU operation when AP bring-up (or any new field) is
unavailable.

> This document is the operator-facing companion to
> `docs/01_CELHYPER.md`. Architectural rationale lives there; the
> mechanics of "boot Celium on a real box" live here.

## Goals achieved this week

| Goal                                                | Status        |
|-----------------------------------------------------|---------------|
| Bootable installer (ISO / USB / PXE)                | ✅ shipped    |
| `virtio-net` driver surface                         | ✅ skeleton   |
| `virtio-blk` improvements (flush, ready, typed req) | ✅ shipped    |
| SMP scheduler foundation + AP bring-up surface      | ✅ skeleton   |
| Handoff v3 (SMP topology + framebuffer)             | ✅ shipped    |
| End-to-end QEMU validation pipeline                 | ✅ unchanged  |

The deep impl of AP INIT-SIPI, MSI-X, and virtqueue I/O is
intentionally deferred to W25; every entry point returns
`HyperError::Unimplemented("...: W25")` instead of pretending to
work. This is the same typed-TODO pattern W23-D established for
the driver registry — auditable, fails closed.

## Boot media

The repository now ships three ways to put Celium on a machine.

### ISO (cdrom, virtual or burnt)

```bash
./scripts/run-qemu.sh        # builds celloader + celhyper
./scripts/build-iso.sh       # wraps build/esp/ → build/celium.iso
./scripts/build-iso.sh smoke # also boots the ISO under QEMU
```

W24-E switches the ISO emitter to `--isohybrid-gpt-basdat`, so the
exact same `build/celium.iso` is now also a valid USB-dd image. The
output is bit-identical for QEMU `-cdrom` and `dd if=… of=/dev/sdX`.

### USB stick

```bash
sudo ./scripts/build-usb.sh /dev/sdX          # hybrid mode (dd ISO)
sudo ./scripts/build-usb.sh /dev/sdX --fat32  # writable FAT32 stick
```

`--fat32` is the recommended mode for a hardware install: the stick
stays writable so logs, dumps, and per-node configs are persisted on
the same medium that booted them. The script refuses to touch any
path that looks like the system disk; override via
`CONFIRM_DESTROY=/dev/sdX` if you really mean it.

### PXE

```bash
./scripts/build-pxe.sh              # stages build/pxe/
dnsmasq --enable-tftp \
        --tftp-root=$(pwd)/build/pxe \
        --dhcp-boot=celium.efi \
        ...
```

`build/pxe/boot.ipxe` is a ready-to-edit iPXE chain script for
fleets that already advertise iPXE. The serving topology mirrors any
other UEFI PXE rollout — `BOOTX64.EFI` over TFTP, kernel ELF over
HTTP / TFTP at the same path.

## Handoff v3 layout

| Offset | Field                | W24 change                                    |
|--------|----------------------|-----------------------------------------------|
| —      | `cpu_count`          | new (CelLoader walks ACPI MADT)               |
| —      | `bsp_apic_id`        | new                                           |
| —      | `ap_apic_ids_phys`   | new (pointer to `[u32; cpu_count-1]`)         |
| —      | `fb_phys`            | new (GOP linear framebuffer base, or 0)       |
| —      | `fb_width/_height`   | new                                           |
| —      | `fb_pitch`           | new (bytes per scanline)                      |
| —      | `fb_format`          | new (0=unknown, 1=BGRA8, 2=RGBA8)             |

All fields default to zero. A handoff with `cpu_count == 1` and
`fb_phys == 0` is indistinguishable from a v2 boot from the kernel's
point of view, so existing W23 QEMU validations remain green
without modification.

## SMP foundation (`celhyper::smp`)

`crates/celhyper/src/smp.rs` introduces:

* `MAX_PCPUS = 8` and `static PCPUS: [PcpuState; MAX_PCPUS]`. Each
  entry owns its own atomic active-VM slot — the foundation for
  retiring the global `sched::ACTIVE` mutex.
* `Topology::from_handoff(&CeliumHandoff)` typed view + validation.
* `mark_bsp_online(&Topology)` registers slot 0 and bumps the
  global online counter.
* `bring_up_aps(&Topology)` returns `Unimplemented("…W25")` for now
  — the real INIT-SIPI sequence needs a real-mode trampoline page
  and per-AP boot stacks, both pending the LAPIC driver in W25.
* `send_ipi(target_pcpu, kind)` ditto.

`bringup::bring_up` reads the topology, logs `smp_cpu_count` /
`smp_bsp_apic_id`, calls `mark_bsp_online`, and *non-fatally* asks
`bring_up_aps` to run. The unimplemented return is logged and
ignored so multi-CPU hardware boots single-CPU until W25 lands.

## Driver registry refresh

* `drivers::BlockDevice` gains `flush()` (default no-op) and
  `is_ready()` (default false) — backward-compatible for the
  virtio_blk skeleton, future-proof for NVMe.
* `drivers::virtio_blk` now exposes the canonical request header
  layout, request-type tags, and an in-flight cap (`MAX_INFLIGHT
  = 16`); `read_sectors` / `write_sectors` distinguish
  `Invalid` (bad input), `Denied` (device not ready), and
  `Unimplemented` (W25). Submission tracker still skeleton.
* `drivers::virtio_net` adds the matching network surface:
  `NetDevice` trait (`mac`, `send_frame`, `recv_frame`,
  `link_up`), modern PCI device ids, feature constants, and
  config-space offsets. All fallible calls return
  `Unimplemented("…W25")`.

## What this DOES NOT do

* No real AP wake. `bring_up_aps` is a typed TODO.
* No PCI scanner. Both `probe_pci()` paths return `Unimplemented`.
* No live virtio I/O. The skeletons fail closed.
* No bare-metal validation on physical hardware. That is W25's
  acceptance criterion; W24 lands the artefacts that make it
  possible (ISO/USB/PXE + handoff v3).

## Verification

* `cargo build --release` (workspace) on Windows: green.
* `cargo check --target x86_64-unknown-none -p celhyper`: green.
* `cargo build --release --features real-handoff -p celloader`: green.
* Workspace `cargo test --workspace`: 203 passed / 0 failed / 7
  ignored (unchanged from end of W23-E3).
