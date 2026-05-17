#!/usr/bin/env bash
# scripts/run-qemu-bridge-smoke.sh — W23-A end-to-end smoke.
#
# Boots CelHyper in QEMU with COM2 redirected to TCP 127.0.0.1:5555,
# then drives the live bridge from the host via the `w23_qemu_bridge`
# ignored test. Designed to be run on the v-build VM where
# `/mnt/data/target/celium` already has the celtest binaries built.
set -euo pipefail

PORT="${PORT:-5555}"
TIMEOUT="${TIMEOUT:-180}"
OVMF="${OVMF:-/usr/share/OVMF/OVMF_CODE_4M.fd}"

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Ensure no stale QEMU instance is holding the port.
pkill -f qemu-system-x86_64 || true
sleep 1

# Boot the kernel asynchronously with bridge UART → TCP.
rm -f /tmp/qemu.run.log
(
    unset CARGO_TARGET_DIR
    export PATH=/mnt/data/cargo/bin:/usr/bin:/bin
    OVMF="$OVMF" TIMEOUT="$TIMEOUT" BRIDGE_TCP="127.0.0.1:${PORT}" \
        bash scripts/run-qemu.sh
) > /tmp/qemu.run.log 2>&1 < /dev/null &
QEMU_GROUP_PID=$!
echo "qemu wrapper pid=$QEMU_GROUP_PID"

# Wait up to ~30 seconds for the QEMU `-serial tcp:` to start
# listening. That happens immediately at QEMU startup (the kernel
# need not be up yet) — but we still wait so the test can connect.
LISTENING=0
for i in $(seq 1 60); do
    if ss -ltn | grep -q ":${PORT} "; then
        echo "bridge listening on :${PORT} after $((i*500))ms"
        LISTENING=1
        break
    fi
    sleep 0.5
done
ss -ltn | grep ":${PORT} " || true
if [[ "$LISTENING" == 0 ]]; then
    echo "FATAL: no listener on :${PORT}; QEMU run log:"
    cat /tmp/qemu.run.log
    pkill -f qemu-system-x86_64 || true
    exit 1
fi

# Wait for the kernel to actually reach bridge::run() — otherwise
# the host's first List would queue inside QEMU until bring_up
# finishes, and the host's 1s call timeout would tear request
# ordering apart on the TCP stream. We watch the COM1 log file for
# the "bridge ready on COM2" marker emitted by bridge::run().
BRIDGE_READY=0
for i in $(seq 1 120); do
    if grep -q 'bridge ready on COM2' build/com1.log 2>/dev/null; then
        echo "kernel bridge ready after $((i*500))ms"
        BRIDGE_READY=1
        break
    fi
    sleep 0.5
done
if [[ "$BRIDGE_READY" == 0 ]]; then
    echo "FATAL: kernel did not reach bridge::run within ~60s; com1 tail:"
    tail -40 build/com1.log 2>/dev/null || true
    pkill -f qemu-system-x86_64 || true
    exit 1
fi

# Drive the bridge from the host.
export PATH=/mnt/data/cargo/bin:/usr/bin:/bin
export RUSTUP_HOME=/mnt/data/rustup
export CARGO_HOME=/mnt/data/cargo
export CARGO_TARGET_DIR=/mnt/data/target/celium
export CELIUM_BRIDGE_TCP="127.0.0.1:${PORT}"
set +e
cargo test -p celtest --test w23_qemu_bridge -- --ignored --nocapture 2>&1 | tee /tmp/w23_test.log
TEST_RC=${PIPESTATUS[0]}
set -e

echo '--- com1.log tail (kernel) ---'
tail -30 build/com1.log 2>/dev/null || true
echo '--- qemu run.log tail ---'
tail -30 /tmp/qemu.run.log 2>/dev/null || true

# Best-effort cleanup; QEMU has TIMEOUT seconds before it self-exits.
pkill -f qemu-system-x86_64 || true

exit "$TEST_RC"
