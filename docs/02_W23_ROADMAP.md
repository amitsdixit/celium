# W23 Roadmap — removing the QEMU dependency

> Status as of W23-D: foundation merged. Bare-metal and virtio-blk
> still pending.

## Where we are after W23-D

* **Image abstraction**: [`celhyper::image_loader::BootImage`] is the
  single source of truth for the next guest. `bringup.rs` and
  `bridge.rs`' `Request::Create` both go through
  `CreateVmRequest::from_boot_image`. The embedded `HELLO_BLOB`
  remains the in-kernel fallback so every existing test (W22 + W23-A
  + W23-B + W23-C bridge smoke and QEMU live-bridge smoke) passes
  unchanged.
* **Handoff v2**: `CeliumHandoff` (mirrored in `celloader` and
  `celhyper`) gains `boot_image_phys` / `boot_image_len` /
  `boot_image_crc32c`. CelLoader passes zeros — image staging across
  `ExitBootServices` lands in W23-E. The kernel accepts both
  "all-zero" (use embedded fallback) and a populated triple (lift the
  region into a `&'static [u8]`).
* **Driver registry**: `celhyper::drivers::{mod, virtio_blk}` shipped
  as a typed-TODO skeleton. Every fallible call returns
  `HyperError::Unimplemented("...: W23-F")`. The W23-C bridge layer
  surfaces this to the host as `Reply::Error { message: "kernel: ..." }`,
  so partial wiring fails fast instead of timing out.
* **ISO build**: `scripts/build-iso.sh` wraps `build/esp/` in a
  bootable ISO9660 image with `xorriso`; `scripts/build-iso.sh smoke`
  boots the ISO under QEMU and asserts `celhyper: alive` appears on
  the console within 10 s.

## What's still required to actually run without QEMU

### W23-E — bridge-streamed boot images

**Problem.** The kernel can load *any* `&'static [u8]` boot image, but
the bridge wire still carries only the metadata (`image_path`,
`boot_blob_crc32c`). The image bytes themselves never leave the host.
Until they do, every Create still installs the embedded `HELLO_BLOB`.

**Plan.**

1. Extend the wire (`crates/celhyper/src/wire.rs` and the host mirror
   in `celmesh::hyper_serial`):
   * `Request::ImageBegin { vm_id, total_len, crc32c }`.
   * `Request::ImageChunk { vm_id, offset, bytes }` — bytes capped at
     `MAX_FRAME_BYTES - JSON_OVERHEAD`, so ~192 B per chunk for the
     current 256 B kernel RX buffer. Bump RX to 4 KiB to amortise.
   * `Request::ImageCommit { vm_id }` — kernel validates length, CRC,
     and "no prior commit for this slot", then installs the image
     into a kernel-managed staging buffer indexed by `vm_id`.
2. Make `CelhyperVmHost::handle(VmOp::Create { image_path: Some(p), .. })`
   stream the file referenced by `p` through `ImageBegin/Chunk/Commit`
   before issuing the actual `HyperRequest::Create`.
3. Lift `MAX_IMAGE_BYTES` from one page to one VMX-supported guest
   memory size (start at 2 MiB; ultimately driven by W23-F's EPT
   multi-page mapper).
4. Acceptance: `celctl vm create --image /path/to/raw` on a real
   QEMU+KVM v-build run launches a different guest blob each call,
   distinguishable in the kernel log.

### W23-F — virtio-blk driver

**Problem.** Guests have no persistent storage today; every image
sits in EPT-backed RAM and disappears on Delete.

**Plan.**

1. Implement a minimal PCI bus scanner in `celhyper::drivers::pci`
   (enumerate config space, walk capability list).
2. Implement the modern virtio-blk transport: feature negotiation
   (RW, FLUSH; reject SCSI commands), one virtqueue, descriptor
   table allocated from `KernelFrames`.
3. MSI-X interrupt handler that walks the used ring and wakes the
   corresponding request future. (For W23-F the IRQ tail is a
   polling loop driven from the bridge thread — full IRQ delivery
   waits for the W25 scheduler.)
4. Wire `BlockDevice` reads/writes into `manager::start_vm` so a
   guest can `outb` to a "load next sector" magic port and the
   kernel synchronously fills the EPT page from the drive.
5. Acceptance: a guest image that's larger than `MAX_IMAGE_BYTES`
   boots by demand-paging sectors from a `qcow2`-backed virtio-blk
   drive plumbed through CelVault.

### W23-G — bare-metal validation

**Problem.** Every W22 / W23-A/B/C/D validation runs under
`qemu-system-x86_64 -accel kvm -cpu host,+vmx`. We have *zero*
evidence the kernel boots on real hardware.

**Plan.**

1. Identify one (1) physical VMX-capable Intel box (NUC-class is
   enough). Document hardware-quirk discovery (`dmidecode`, IOMMU
   group layout, ACPI version).
2. Build a USB-writable image: `dd if=build/celium.iso of=/dev/sdX`
   path validated; alternatively `mkfs.fat + cp` for a writable
   stick.
3. Validate boot to `celhyper: alive` on the physical box; capture
   serial-console output over an RS-232 / USB-UART dongle so we
   don't depend on QEMU's `-debugcon stdio` channel.
4. Wire a PXE recipe (iPXE script + per-node `boot.ipxe`) so a fleet
   of build farm nodes can boot Celium without a USB stick each.
5. Update `docs/INSTALL.md` with the canonical bare-metal procedure
   and remove the "QEMU only" caveat from the README.

## Open design questions (decide before W23-E lands)

* **Per-VM image staging vs. shared blob pool?** Per-VM is simpler
  but caps total live VMs by `MAX_IMAGE_BYTES × MAX_VMS`. A shared
  pool with reference counting matches the CelVault content-addressed
  model better. Provisionally going per-VM for W23-E and revisiting
  in W23-F when EPT mapping handles larger guests.
* **Should `Request::ImageChunk` carry a per-chunk CRC?** Current
  plan is one final CRC at commit time. Per-chunk CRC would let us
  retry single chunks but balloons wire overhead. Default to commit-
  time only and re-evaluate if we see truncated streams in practice.
* **Virtio-blk legacy vs. modern only?** QEMU emits both; physical
  hardware is overwhelmingly modern. Sticking to modern (vendor
  `0x1AF4`, device `0x1042`) keeps the W23-F driver small.
