# Celium — Usage Guide

This guide walks an operator through the live surface of the system
as of W12: local VM lifecycle, multi-node clustering, federated
operations, persistent volumes, and supervised restart.

All examples use `celctl`, the operator CLI in `crates/celcli/`.
Build it once with `cargo build --release` (see [INSTALL.md](INSTALL.md));
the rest of this guide assumes `celctl` is on `PATH`.

> **Convention.** Every command exits with a non-zero status on any
> `CelError`. Re-run with `RUST_LOG=debug` for structured `tracing`
> events.

---

## 1. The CLI surface

```
celctl
├── version                       # build info
├── probe                         # local hypervisor probe (stub)
├── vm
│   ├── create  --label <s>       # allocate a slot in the local controller
│   ├── list                      # list allocated slots
│   ├── start   <id|/vms/<id>>    # transition created → running → halted (model)
│   ├── stop    <id|/vms/<id>>    # idempotent
│   └── state   <id|/vms/<id>>
└── cluster
    ├── start          <StartArgs>           # run a CelMesh node
    ├── members        <StartArgs>           # one-shot membership snapshot
    ├── vms            <StartArgs>           # one-shot federated VM list
    ├── invoke         <StartArgs> --target <node> --op <op> [...]
    ├── invoke-path    <StartArgs> --path /cluster/<node>/vms/<id> --op <op>
    ├── recover        <StartArgs>           # one supervisor pass
    └── status         <StartArgs>           # full cluster snapshot
```

### Common `StartArgs`

| Flag | Default | Meaning |
| --- | --- | --- |
| `--node-id <s>` | required | Stable id for this node. |
| `--bind <addr>` | `127.0.0.1:0` | UDP bind address. `:0` lets the OS choose. |
| `--advertise <addr>` | `--bind` | Address peers should reach us at. |
| `--cluster <name>` | `celium` | Cluster tag; foreign frames are dropped. |
| `--seeds <a,b,...>` | `""` | Comma-separated `host:port` seeds. |
| `--epoch <u64>` | `1` | Bump on every node start (LWW tiebreak). |
| `--duration <secs>` | `0` (forever) | For `start`. |
| `--settle <secs>` | `1` | One-shot snapshots wait this long for convergence. |

---

## 2. Local VM lifecycle (single node, no cluster)

```bash
# Allocate a slot and remember its id.
celctl vm create --label demo            # -> created vm 0
celctl vm list                            # -> [{id:0, label:"demo", state:"created"}]

# Run the deterministic single-step guest. The model halts at exit 12.
celctl vm start 0                         # -> state: halted, last_exit: 12
celctl vm state 0
celctl vm stop 0                          # idempotent on terminal states
```

State persists in `./build/celctl-state.json` (override with the
global `--state-file` flag).

---

## 3. Bring up a 3-node cluster

Open three terminals.

```bash
# Terminal 1 — n1, the seed
celctl cluster start --node-id n1 --bind 127.0.0.1:7100

# Terminal 2 — n2
celctl cluster start --node-id n2 --bind 127.0.0.1:7101 \
                     --seeds 127.0.0.1:7100

# Terminal 3 — n3
celctl cluster start --node-id n3 --bind 127.0.0.1:7102 \
                     --seeds 127.0.0.1:7100
```

In a fourth terminal, snapshot the cluster:

```bash
celctl cluster status --node-id obs --bind 127.0.0.1:0 \
                      --seeds 127.0.0.1:7100 --settle 2
```

You should see three Alive members and an empty federated VM list.

---

## 4. Cross-node VM operations

Any node can drive any other node's host. There are two routing modes:

### 4.a. By target node id (`invoke`)

```bash
# Create a VM on n2, driven from n1
celctl cluster invoke --node-id n1 --bind 127.0.0.1:0 \
                      --seeds 127.0.0.1:7100 \
                      --target n2 --op create --label web --restart always

# Returns: Created { vm_id: <N> }

celctl cluster invoke --node-id n1 --bind 127.0.0.1:0 \
                      --seeds 127.0.0.1:7100 \
                      --target n2 --op start --vm-id <N>

celctl cluster invoke --node-id n1 --bind 127.0.0.1:0 \
                      --seeds 127.0.0.1:7100 \
                      --target n2 --op list
```

### 4.b. By federated path (`invoke-path`)

```bash
celctl cluster invoke-path --node-id n1 --bind 127.0.0.1:0 \
                           --seeds 127.0.0.1:7100 \
                           --path /cluster/n2/vms/0 --op start
```

`invoke-path` accepts `start | stop | delete` today. Volume ops use
`invoke` (see §5).

---

## 5. Persistent volumes (W12)

Volumes live on the node that creates them. Their id is
`<owner-node-id>/v<counter>`; bounds are name ≤ 64 chars,
mount ≤ 32, size ≤ 64 MiB.

```bash
# 5.1 Create a 1 MiB volume on n3
celctl cluster invoke --node-id n1 --bind 127.0.0.1:0 \
                      --seeds 127.0.0.1:7100 \
                      --target n3 --op create-volume \
                      --label scratch --volume-size 1048576

# Returns: VolumeCreated { volume: { id: "n3/v1", owner: "n3", ... } }

# 5.2 List volumes on a remote node
celctl cluster invoke --node-id n1 --bind 127.0.0.1:0 \
                      --seeds 127.0.0.1:7100 \
                      --target n3 --op list-volumes

# 5.3 Attach a volume to a VM (vm and volume must live on the same node
#      for a direct attach; supervisor-preserved attachment handles the
#      cross-node case across a restart — see §6).
celctl cluster invoke --node-id n1 --bind 127.0.0.1:0 \
                      --seeds 127.0.0.1:7100 \
                      --target n3 --op attach-volume \
                      --vm-id 0 --volume-id n3/v1 --mount-name data0

# 5.4 Detach (idempotent)
celctl cluster invoke --node-id n1 --bind 127.0.0.1:0 \
                      --seeds 127.0.0.1:7100 \
                      --target n3 --op detach-volume \
                      --vm-id 0 --volume-id n3/v1

# 5.5 Delete (rejected if any slot still attaches it)
celctl cluster invoke --node-id n1 --bind 127.0.0.1:0 \
                      --seeds 127.0.0.1:7100 \
                      --target n3 --op delete-volume --volume-id n3/v1
```

Operation-by-flag reference:

| Op | Required flags |
| --- | --- |
| `create-volume`  | `--target`, `--label` (volume name), `--volume-size` |
| `delete-volume`  | `--target`, `--volume-id` |
| `list-volumes`   | `--target` |
| `attach-volume`  | `--target`, `--vm-id`, `--volume-id`, `--mount-name` |
| `detach-volume`  | `--target`, `--vm-id`, `--volume-id` |

---

## 6. Resiliency: supervised restart

Set `--restart always` on `create` and run `cluster recover` (or rely
on the live supervisor that ticks at `MeshConfig::supervisor_interval`,
default ≈ 200 ms in the integration harness).

When a node owning a VM goes Dead, the lowest-id Alive node:

1. Recreates the VM on itself, with label `"<original-label>@<dead-node>"`.
2. Calls `VmHost::attach_preserved` to restore the original
   `Vec<VolumeAttachment>` — even if the volume's vault still lives
   on a third (Alive) node.

A typical sequence:

```bash
# Mark a VM as restartable on n2
celctl cluster invoke ... --target n2 --op create --label web --restart always

# Kill n2 (Ctrl-C its terminal). Within ~ timeout_dead + supervisor_interval,
#   cluster status from any surviving node shows the VM relabelled
#   "web@n2" and now owned by the new supervisor.
celctl cluster status --node-id obs --seeds 127.0.0.1:7100 --settle 3
```

`cluster recover` runs exactly one supervisor pass and prints the list
of recreations — useful in tests and demos.

---

## 7. Cluster snapshots and inspection

```bash
celctl cluster members --node-id obs --seeds 127.0.0.1:7100 --settle 2
celctl cluster vms     --node-id obs --seeds 127.0.0.1:7100 --settle 2
celctl cluster status  --node-id obs --seeds 127.0.0.1:7100 --settle 2
```

`status` is the most informative: it prints membership counters
(alive/suspect/dead), the elected supervisor, and the federated VM
table including each row's `volumes` attachments.

---

## 8. Embedding `celmesh` directly

Operators who want to drive the fabric from their own Rust binary can
depend on `celmesh` and use the public API directly. Minimal example:

```rust
use std::sync::Arc;
use std::time::Duration;
use celmesh::{Mesh, MeshConfig, MemVmHost, NodeId, VmHost, VmOp, UdpTransport};

#[tokio::main]
async fn main() -> celcommon::CelResult<()> {
    let t = Arc::new(UdpTransport::bind("127.0.0.1:0").await?);
    let cfg = MeshConfig::defaults("n1", &t.local_addr());
    let mesh = Mesh::start(cfg, t).await?;
    mesh.set_host(Arc::new(MemVmHost::new()) as Arc<dyn VmHost>).await;
    // ... drive ops via mesh.invoke() / mesh.invoke_path() ...
    let _ = mesh.shutdown().await;
    Ok(())
}
```

See `crates/celtest/tests/multi_node_volume.rs` for a full
3-node-with-volumes example.

---

## 9. Quick reference card

| Goal | Command |
| --- | --- |
| Build everything | `cargo build --workspace --release` |
| Run all tests serially | `cargo test --workspace -- --test-threads=1` |
| Boot a node | `celctl cluster start --node-id n1 --bind 127.0.0.1:7100` |
| Add a peer | `celctl cluster start --node-id n2 --bind 127.0.0.1:7101 --seeds 127.0.0.1:7100` |
| Inspect cluster | `celctl cluster status --node-id obs --seeds 127.0.0.1:7100 --settle 2` |
| Create restartable VM | `celctl cluster invoke ... --target n2 --op create --label web --restart always` |
| Create volume | `celctl cluster invoke ... --target n3 --op create-volume --label scratch --volume-size 1048576` |
| Attach volume | `celctl cluster invoke ... --target n3 --op attach-volume --vm-id 0 --volume-id n3/v1 --mount-name data0` |
| One supervisor pass | `celctl cluster recover --node-id obs --seeds 127.0.0.1:7100 --settle 2` |
