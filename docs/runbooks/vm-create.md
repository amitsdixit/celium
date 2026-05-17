# Runbook \u2014 Creating a VM on Celium

The Core Layer ships two flows for VM creation: a host-only flow
(persists to the JSON state file, no kernel involvement \u2014 useful for
demos), and the full bridge flow (real VMs on real CelHyper).

## Flow A \u2014 Host-only (no kernel)

Used by the integration tests and by lab demos without bare-metal
hardware.

```bash
celctl vm create --label web --memory-mib 512 --cpus 1
celctl vm list
celctl vm start <id>
celctl vm stop <id>
celctl vm delete <id>
```

State lives at `--state-file` (default `./build/celctl-state.json`).

## Flow B \u2014 Through the kernel bridge

This is the production flow once the host is booted and the bridge
is reachable.

### 1. Stage a boot blob

CelHyper accepts a single small (\u22644 KiB in W26; raised in Tenancy)
boot blob that lands at GPA 0x1000 and is executed by the guest. The
canonical blob is the embedded "Celium Guest Alive!" hello payload;
any custom blob must:

* be \u2264 4096 bytes,
* start its `RIP` at 0x1000,
* end with a `HLT` so the supervisor sees a clean `Halted`.

```bash
# Compose the blob:
cargo run -p celimage -- assemble \
  --instructions hello.asm \
  --output build/boot.blob

# Hand it to celctl:
export CELIUM_BRIDGE_TCP=<node>:<bridge-port>
celctl vm create --label custom --image build/boot.blob \
       --vm-host celhyper-serial:$CELIUM_BRIDGE_TCP
```

`celctl`:

1. Reads `build/boot.blob`.
2. Wraps it in a `HyperRequest::ImageLoad { len, crc32c, bytes_hex }`.
3. Sends through `SerialHyperLink` over COM2.
4. Receives `HyperReply::ImageLoaded { len, crc32c }`. The CRC MUST
   round-trip; mismatch leaves the VM `Created` but not `Started`.
5. Issues `HyperRequest::Create { boot_blob_crc32c: Some(crc) }`.

### 2. Start

```bash
celctl vm start <id> --vm-host celhyper-serial:$CELIUM_BRIDGE_TCP
```

Kernel `vmlaunch`es the guest. It runs until `HLT`; the bridge then
reports `state=Halted last_exit=0xC`.

### 3. Observability

```bash
# Snapshot the kernel's hot-path counters via the bridge:
celctl invoke-path /kernel/metrics --vm-host celhyper-serial:$CELIUM_BRIDGE_TCP
```

Returns 11 `AtomicU64` counter values (see
[`crates/celhyper/src/metrics.rs`](../../crates/celhyper/src/metrics.rs)).

### 4. Delete

A VM must be in a terminal state (`Halted` / `Stopped` / `Faulted`)
before delete is accepted; otherwise the kernel returns
`HyperError::Denied("vm not in terminal state")`. `celctl vm delete`
calls `Stop` first if the VM is still `Running`.

## Common errors

| `CelError`                       | Meaning                                   |
|----------------------------------|-------------------------------------------|
| `Storage("boot blob missing")`   | The path passed to `--image` did not exist. |
| `Timeout("mesh rpc")`            | The bridge socket is not reachable.       |
| `Other("hyper: kernel: ...")`    | Kernel returned `Reply::Error`. The suffix carries the kernel's truncated 96-byte message. |
| `capability denied: vm.create`   | Peer lacks `VM_LIFECYCLE_WRITE` capability. |
