#!/usr/bin/env bash
# scripts/run-qemu.sh — Boot Celium in QEMU+OVMF (Week-6).
#
# Linux companion to run-qemu.ps1. Uses KVM with nested VMX when the
# host kernel has nested=1 for kvm_intel / kvm_amd; otherwise falls
# back to pure TCG with `-cpu max,+vmx`.
#
# Env / args
#   OVMF              path to OVMF firmware (auto-discovered if unset)
#   QEMU              qemu-system-x86_64 binary (default: PATH)
#   ACCEL             'kvm' (default if /dev/kvm exists) or 'tcg'
#   TIMEOUT           seconds to wait for QEMU to exit (default 60)
#   NO_BUILD=1        skip cargo build, reuse existing artifacts
#   ACCEPT_DEFERRED=1 treat "vmlaunch deferred" log as success
#                     (useful when host doesn't expose nested VMX)
#   VERBOSE=1         echo the full debugcon + COM1 logs

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

QEMU="${QEMU:-qemu-system-x86_64}"
OVMF="${OVMF:-}"
ACCEL="${ACCEL:-}"
TIMEOUT="${TIMEOUT:-60}"
NO_BUILD="${NO_BUILD:-0}"
ACCEPT_DEFERRED="${ACCEPT_DEFERRED:-0}"
VERBOSE="${VERBOSE:-0}"

banner() { printf '\n========================================================================\n %s\n========================================================================\n' "$*"; }

# 0. Auto-discover OVMF. Modern Debian/Ubuntu ships split firmware
#    (`OVMF_CODE_4M.fd` + `OVMF_VARS_4M.fd`); older distros ship a
#    monolithic `OVMF.fd`. We support both: split firmware uses the
#    pflash pair, monolithic uses `-bios`.
if [[ -z "$OVMF" ]]; then
    for cand in \
        /usr/share/OVMF/OVMF_CODE_4M.fd \
        /usr/share/OVMF/OVMF_CODE.fd \
        /usr/share/edk2-ovmf/OVMF_CODE_4M.fd \
        /usr/share/edk2-ovmf/OVMF_CODE.fd \
        /usr/share/edk2/x64/OVMF_CODE.fd \
        /usr/share/qemu/OVMF.fd; do
        [[ -e "$cand" ]] && { OVMF="$cand"; break; }
    done
fi
if [[ -z "$OVMF" || ! -e "$OVMF" ]]; then
    echo "error: OVMF firmware not found. Set \$OVMF to an OVMF.fd path." >&2
    exit 2
fi
# Detect split-firmware mode: filename contains _CODE or _4M → need a
# writable VARS copy and -drive pflash pair.
OVMF_SPLIT=0
OVMF_VARS_SRC=""
if [[ "$OVMF" == *_CODE* || "$OVMF" == *_4M* ]]; then
    OVMF_SPLIT=1
    case "$OVMF" in
        */OVMF_CODE_4M.fd) OVMF_VARS_SRC="$(dirname "$OVMF")/OVMF_VARS_4M.fd" ;;
        */OVMF_CODE.fd)    OVMF_VARS_SRC="$(dirname "$OVMF")/OVMF_VARS.fd"   ;;
        *)                 OVMF_VARS_SRC="$(dirname "$OVMF")/OVMF_VARS_4M.fd" ;;
    esac
    if [[ ! -e "$OVMF_VARS_SRC" ]]; then
        echo "error: OVMF code found at $OVMF but companion VARS not at $OVMF_VARS_SRC" >&2
        exit 2
    fi
fi
if ! command -v "$QEMU" >/dev/null 2>&1; then
    echo "error: '$QEMU' not on PATH." >&2
    exit 2
fi

# Pick accelerator.
if [[ -z "$ACCEL" ]]; then
    if [[ -e /dev/kvm ]]; then ACCEL=kvm; else ACCEL=tcg; fi
fi
case "$ACCEL" in
    kvm) cpu_model='host,+vmx' ;;
    tcg) cpu_model='max,+vmx'  ;;
    *)   echo "error: unknown ACCEL=$ACCEL" >&2; exit 2 ;;
esac

# 1. Build artifacts.
if [[ "$NO_BUILD" != 1 ]]; then
    banner 'building celloader (real-handoff)'
    ( cd bootloader/celloader && cargo build --release --features real-handoff )

    banner 'building celhyper kernel'
    ( cd crates/celhyper && cargo build --release )
fi

celloader='bootloader/celloader/target/x86_64-unknown-uefi/release/celloader.efi'
celhyper='crates/celhyper/target/x86_64-unknown-none/release/celhyper'
[[ -e "$celloader" ]] || { echo "missing $celloader" >&2; exit 1; }
[[ -e "$celhyper"  ]] || { echo "missing $celhyper"  >&2; exit 1; }

# 2. Stage ESP.
esp='build/esp'
mkdir -p "$esp/EFI/BOOT" "$esp/EFI/CELIUM"
cp -f "$celloader" "$esp/EFI/BOOT/BOOTX64.EFI"
cp -f "$celhyper"  "$esp/EFI/CELIUM/CELHYPER.ELF"

banner "ESP staged at $esp"
find "$esp" -type f -printf '%p (%s bytes)\n'

# 3. Boot QEMU.
mkdir -p build
debugcon_log='build/debugcon.log'
com1_log='build/com1.log'
rm -f "$debugcon_log" "$com1_log"

banner "launching QEMU (accel=$ACCEL, cpu=$cpu_model, ovmf_split=$OVMF_SPLIT)"

# Build the firmware argv chunk: pflash pair when split, -bios when monolithic.
ovmf_args=()
if [[ "$OVMF_SPLIT" == 1 ]]; then
    cp -f "$OVMF_VARS_SRC" build/OVMF_VARS.fd
    ovmf_args=(
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF"
        -drive "if=pflash,format=raw,file=build/OVMF_VARS.fd"
    )
    echo "$QEMU -accel $ACCEL -cpu $cpu_model -drive pflash:$OVMF -drive pflash:build/OVMF_VARS.fd"
else
    ovmf_args=(-bios "$OVMF")
    echo "$QEMU -accel $ACCEL -cpu $cpu_model -bios $OVMF"
fi

# Optional bridge UART (COM2) → TCP, for host-side SerialHyperLink
# end-to-end tests. Enabled when BRIDGE_TCP is set to host:port. When
# unset, COM2 is wired to a null device so the kernel can still
# initialise it.
BRIDGE_TCP="${BRIDGE_TCP:-}"
bridge_serial=(-serial null)
if [[ -n "$BRIDGE_TCP" ]]; then
    # `server,nowait` makes QEMU listen on the address; the host-side
    # SerialHyperLink connects whenever it is ready.
    bridge_serial=(-serial "tcp:${BRIDGE_TCP},server=on,wait=off")
    echo "bridge UART (COM2) listening on tcp:$BRIDGE_TCP"
fi

timeout "${TIMEOUT}s" "$QEMU" \
    -machine q35 \
    -accel  "$ACCEL" \
    -cpu    "$cpu_model" \
    -m      512 \
    "${ovmf_args[@]}" \
    -drive  "format=raw,file=fat:rw:$esp" \
    -debugcon "file:$debugcon_log" \
    -serial   "file:$com1_log" \
    "${bridge_serial[@]}" \
    -no-reboot \
    -display none \
    > build/qemu.stdout.log 2> build/qemu.stderr.log || true

# 4. Result.
debugcon="$(cat "$debugcon_log" 2>/dev/null || true)"
com1="$(cat "$com1_log" 2>/dev/null || true)"

if [[ "$VERBOSE" == 1 ]]; then
    banner 'QEMU debug console (port 0xE9)'
    printf '%s\n' "$debugcon"
    banner 'COM1 (kernel log)'
    printf '%s\n' "$com1"
fi

if grep -q 'Celium Guest Alive!' "$debugcon_log" 2>/dev/null; then
    banner 'PASS — guest produced the marker on port 0xE9'
    exit 0
elif grep -q 'GUEST OK' "$com1_log" 2>/dev/null; then
    banner 'PASS — dispatcher logged GUEST OK on COM1'
    exit 0
elif [[ "$ACCEPT_DEFERRED" == 1 ]] && grep -q 'vmlaunch deferred' "$com1_log" 2>/dev/null; then
    multi_vm=0
    grep -q 'vm_a_id' "$com1_log" 2>/dev/null && grep -q 'vm_b_id' "$com1_log" 2>/dev/null && multi_vm=1
    bring_up_done=0
    grep -q 'bring_up complete' "$com1_log" 2>/dev/null && bring_up_done=1
    if [[ "$multi_vm" == 1 && "$bring_up_done" == 1 ]]; then
        banner 'PASS (deferred, multi-VM) — both VMs reached terminal state'
    else
        banner 'PASS (deferred) — kernel reached vmlaunch on a CPU without VT-x'
    fi
    echo '   (rerun on KVM/nested=1 or real hardware to observe live guest)'
    exit 0
else
    banner 'FAIL — no success marker; printing tail of logs'
    echo '----- COM1 (kernel log) -----'; printf '%s\n' "$com1"
    echo '----- debugcon (port 0xE9) -----'; printf '%s\n' "$debugcon"
    exit 1
fi
