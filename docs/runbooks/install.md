# Runbook \u2014 Install Celium on a new host

This runbook covers a fresh Celium install on either a QEMU VM or a
physical box.

## 1. Build the workspace

```powershell
# Windows
$env:PATH = 'C:\Users\<you>\scoop\apps\mingw\current\bin;' + $env:PATH
$env:CARGO_INCREMENTAL = '0'
cargo build --workspace --release
```

```bash
# Linux / macOS
cargo build --workspace --release
```

This produces `target/release/celctl(.exe)`.

## 2. Build the bare-metal kernel + loader

```bash
cd crates/celhyper
cargo build --target x86_64-unknown-none --release
cd ../../bootloader/celloader
cargo build --target x86_64-unknown-uefi --release --features real-handoff
```

Artefacts:

* `crates/celhyper/target/x86_64-unknown-none/release/celhyper` \u2014 ELF.
* `bootloader/celloader/target/x86_64-unknown-uefi/release/celloader.efi`.

## 3. Stage the EFI System Partition

```bash
mkdir -p build/esp/EFI/BOOT build/esp/EFI/CELIUM
cp bootloader/celloader/target/x86_64-unknown-uefi/release/celloader.efi \
   build/esp/EFI/BOOT/BOOTX64.EFI
cp crates/celhyper/target/x86_64-unknown-none/release/celhyper \
   build/esp/EFI/CELIUM/CELHYPER.ELF
```

## 4. Build the install medium

```bash
scripts/build-installer.sh           # iso + pxe
# or:
scripts/build-installer.sh usb /dev/sdX
```

## 5. Boot

* **QEMU smoke test:** `scripts/build-iso.sh smoke` boots the ISO
  under QEMU + OVMF, greps for `celhyper: alive`, fails after 10 s
  if not found.
* **Physical box:** boot from the medium and watch COM1 (see
  [INSTALLER.md](../INSTALLER.md) \u00a7 Boot verification).

## 6. Verify

`metrics_vm_exits_hlt = 0x2` on the serial log = healthy boot.
Anything else \u2014 see [first-boot.md](first-boot.md) \u00a7 Troubleshooting.
