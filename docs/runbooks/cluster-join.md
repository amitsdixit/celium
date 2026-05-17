# Runbook \u2014 Joining a Celium cluster

Once a host boots, hook it into a CelMesh cluster.

## Prerequisites

* Host has booted; COM2 bridge is reachable from the operator
  workstation as `<host>:<bridge-port>`.
* The operator workstation has `celctl` built (see
  [install.md](install.md)).
* A seed node is already running. If this IS the seed, skip step 2.

## 1. Choose a node id and bind address

```bash
NODE_ID=node-3
BIND_ADDR=10.0.0.13:7001
STATE=/var/celium/celctl-state-$NODE_ID.json
```

## 2. Start the local mesh agent

```bash
celctl --state-file "$STATE" cluster start \
       --node-id "$NODE_ID" \
       --bind "$BIND_ADDR" \
       --seed 10.0.0.10:7001 \
       --duration 0          # 0 = run until Ctrl-C
```

`--seed` may be repeated. `--duration 0` runs indefinitely; finite
values are useful for demos.

## 3. Verify membership

From a second terminal:

```bash
celctl --state-file "$STATE" cluster status \
       --node-id status-probe-$NODE_ID \
       --bind 127.0.0.1:0 \
       --seed 10.0.0.10:7001
```

Expected:

```text
cluster=default size=3 alive=3 suspect=0 dead=0
node-1   alive  10.0.0.10:7001
node-2   alive  10.0.0.11:7001
node-3   alive  10.0.0.13:7001
```

## 4. Wire the bridge

If this node will run real VMs (not just gossip), attach `celctl` to
the kernel bridge:

```bash
export CELIUM_BRIDGE_TCP=<node-3>:<bridge-port>
celctl cluster vm list --vm-host celhyper-serial:$CELIUM_BRIDGE_TCP \
       --node-id "$NODE_ID" --bind 127.0.0.1:0 --seed 10.0.0.10:7001
```

Two bring-up VMs come back in `Halted` state.

## 5. Healing a split

If a partition heals after a node was marked `dead`, kick a join
explicitly:

```bash
celctl --state-file "$STATE" cluster join \
       --target 10.0.0.10:7001
```

The receiving node bumps `join_calls` and immediately sends a HELLO.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `capability denied: vm.create` | Mint a `Capabilities` envelope with `VM_LIFECYCLE_WRITE` for the calling peer. |
| `mesh rpc: timeout` | UDP loss or firewall; check `:7001/udp` is open both directions. |
| `foreign_cluster_drops` rising | Two clusters with different `cluster` tags speaking on the same port; rebind one. |
