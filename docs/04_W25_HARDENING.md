# W25 \u2014 Core Layer hardening

Week 25 finishes the Core Layer (CelHyper). Five themes:

1. **Performance** \u2014 lock-free counters across every hot path
   (VM-exit dispatcher, EPT walker, block drivers, IPIs).
2. **Drivers** \u2014 real LAPIC driver, full PCI scanner, virtio-net
   already shipped in W24, plus W25 skeletons for virtio-console and
   NVMe.
3. **SMP** \u2014 `smp::send_ipi` is now live through the LAPIC; the
   actual AP wake (trampoline page + per-AP stacks) still returns
   `Unimplemented(W26)` so callers fail closed.
4. **Bare-metal validation** \u2014 unified `scripts/build-installer.sh`
   orchestrates the ISO + PXE + USB artefacts; the same hybrid ISO
   from W24 boots as CDROM and `dd`s to a USB stick.
5. **Production polish** \u2014 every fallible API still returns
   `HyperResult<T>`; every `unsafe` block has a `// SAFETY:` comment;
   no `unwrap()` / `panic!()` on production paths.

## Hot-path counters

`crates/celhyper/src/metrics.rs` defines 11 `AtomicU64` counters:

| Counter | Bumped by |
|---|---|
| `vm_exits_total` / `_hlt` / `_cr` / `_ept` / `_other` | `vmx::exit::vm_exit_dispatch` |
| `ept_map_4k_total` | `mm::Ept::map_4k` (every leaf write) |
| `ept_table_allocs` | `mm::Ept::map_4k` (every on-demand intermediate alloc) |
| `block_read_bytes` / `block_write_bytes` / `block_flushes` | block-driver completions (wired as the drivers fill out) |
| `ipi_sent` | `smp::send_ipi` after the LAPIC acknowledges delivery |

Each bump is a single `lock xadd` on x86_64. Snapshots are emitted
via `metrics::log_snapshot()` from `bringup::bring_up` just before
the function returns, so any boot log captures the full inventory.

## LAPIC driver (`lapic::Lapic`)

xAPIC MMIO path only. Reads `IA32_APIC_BASE` to discover the window
(falls back to `0xFEE00000` if firmware left it zero), enables the
LAPIC + software-enables the SVR, then exposes:

```rust
let lapic = Lapic::current()?;            // re-use the BSP's window
lapic.send_ipi(target, vec, mode, sh)?;   // generic IPI
lapic.init_sipi_sipi(target, page)?;      // INIT + SIPI + SIPI
```

x2APIC and ECAM/MSI-X are deliberately deferred to W26.

## PCI scanner (`pci::scan`)

Legacy port-IO (0xCF8 / 0xCFC). Bounded O(bus \u00d7 dev \u00d7 fn) walk; the
multifunction header bit is honoured so a function-0 with bit 7 clear
short-circuits the remaining 7 slots. Two entry points:

* `pci::find_first(vendor, device)` \u2014 returns the first match.
* `pci::scan(|info| ...)` \u2014 visits every present endpoint.

Used in W26 by every virtio + NVMe probe.

## Driver registry

`crates/celhyper/src/drivers/`:

| Driver | Status |
|---|---|
| `virtio_blk` | skeleton (W23-D), typed request header + req_type tags + MAX_INFLIGHT (W24) |
| `virtio_net` | skeleton (W24-C), MAC + features + config offsets |
| `virtio_console` | **new (W25-D)** skeleton, `ConsoleDevice` trait |
| `nvme` | **new (W25-E)** skeleton, PCI class 0x01/0x08/0x02 |

Every driver returns `HyperError::Unimplemented("\u2026: W26")` from
its deep paths so the bridge surfaces a structured `Reply::Error`
rather than hanging.

## Installer

```bash
scripts/build-installer.sh             # iso + pxe
scripts/build-installer.sh iso         # only ISO
scripts/build-installer.sh pxe         # only PXE staging
scripts/build-installer.sh usb /dev/sdX  # iso + dd to USB
```

The ISO is hybrid (`xorriso --isohybrid-gpt-basdat`), so the same
artefact boots from a CD-ROM emulation and `dd`s cleanly to a USB
stick. The PXE staging dir writes `boot.ipxe` + `README.md` for
deployment behind dnsmasq.

## What this does **NOT** do

* No real AP wake \u2014 `smp::bring_up_aps` still returns
  `Unimplemented(W26)` because the trampoline page + per-AP boot
  stacks are not allocated yet.
* No virtio I/O \u2014 the drivers know how to read/write config space
  but the virtqueue allocator and MSI-X programmer are W26.
* No NVMe I/O \u2014 the controller-init state machine lands in W26.
* No physical-hardware validation \u2014 we can't drive a VMX server
  from this workspace; the user owns physical hardware bring-up.

## Verification

```powershell
$env:PATH='C:\Users\amdix\scoop\apps\mingw\current\bin;'+$env:PATH
$env:CARGO_INCREMENTAL='0'
cargo check -p celhyper --target x86_64-unknown-none --release
cargo check --workspace --all-targets
cargo check -p celloader --target x86_64-unknown-uefi --release --features real-handoff
cargo test --workspace --no-fail-fast
```

Expected: all clean; workspace tests \u2248 201/0/7 with the two known
Windows gossip flakes optionally re-run in isolation.
