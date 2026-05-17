# Celium Core Layer — Operator Runbook

This runbook covers day-2 operations on the Celium **Core Layer**:
CelHyper (hypervisor TCB), CelMesh (gossip + federation), and
CelVault (volume store). It is the document an on-call operator
should read first when something looks off, and the reference for
routine cluster maintenance.

> **Conventions.** Examples use `celctl` against a state file under
> `~/.celium/state.json` and a transient mesh on `127.0.0.1`. In
> production set `--state-file`, `--bind`, `--advertise`, and
> `--seeds` explicitly. Every fallible command returns a non-zero
> exit code on failure; pipe to your supervision tool of choice.

---

## 1. Cluster lifecycle

### Start a node

```bash
celctl cluster start \
    --bind 0.0.0.0:7401 \
    --advertise <public-addr>:7401 \
    --node-id $(hostname -s) \
    --cluster prod \
    --epoch $(date +%s) \
    --seeds peer-a:7401,peer-b:7401 \
    --duration 0          # run until SIGINT
```

* `--epoch` **must** monotonically increase per node start. The
  failure detector uses it to order out-of-date views.
* `--seeds` may be empty for the first node. Subsequent nodes need
  at least one reachable seed to join the gossip plane.

### Inspect membership

```bash
celctl cluster members --bind 0.0.0.0:0 --node-id observer --cluster prod \
    --seeds <any-live-node>:7401 --settle 2

celctl cluster status  --bind 0.0.0.0:0 --node-id observer --cluster prod \
    --seeds <any-live-node>:7401 --settle 2
```

`status` is the one-shot dashboard: alive / suspect / dead counts,
the elected supervisor, and the federated VM table.

### Graceful shutdown

`celctl cluster start` traps SIGINT and emits a `Goodbye` envelope to
every known peer before exiting. Peers downgrade the row to
`owner_alive=false` immediately rather than waiting for the failure
detector to fire.

---

## 2. Observability

### Prometheus-format mesh counters

```bash
celctl cluster metrics --bind 0.0.0.0:0 --node-id observer --cluster prod \
    --seeds <any-live-node>:7401 --settle 3
```

Counters of interest:

| Counter                                | Read as                                                    |
|----------------------------------------|------------------------------------------------------------|
| `celmesh_gossip_sent_total`            | Outbound frames; should grow on every gossip interval.     |
| `celmesh_gossip_recv_total`            | Inbound frames; zero ⇒ no peers reachable.                 |
| `celmesh_decode_errors_total`          | Bad magic / version / oversized frames. Investigate.       |
| `celmesh_foreign_cluster_drops_total`  | Cross-cluster crosstalk. Audit your `--cluster` settings.  |
| `celmesh_suspect_promotions_total`     | Peers gone quiet past `timeout_suspect`.                   |
| `celmesh_dead_promotions_total`        | Peers declared dead after `timeout_dead`.                  |
| `celmesh_rpc_timeouts_total`           | `cluster invoke` round-trips that exhausted the deadline.  |
| `celmesh_supervisor_restarts_total`    | VMs the supervisor re-created for an `always`-policy VM.   |

The W20 surface is one-shot. A long-lived `/metrics` HTTP endpoint
is on the W21+ roadmap; for now the canonical pattern is to scrape
this output from a cron job.

### Local controller stats

```bash
celctl vm stats
```

Reports slot occupancy and per-state totals plus `with_boot_blob`
— how many VMs have a staged digest recorded. A value below the
allocated count after a deployment usually means staging failed for
the missing ones; check the per-VM log line on the next `start`.

### Per-node summary

The mesh emits a single human-readable summary line each gossip
interval at `info` level under target `celmesh::mesh`:

```
mesh summary self=node-a cluster=prod alive=3 suspect=0 dead=0
    vms=4 orphans=0 supervisor=true degraded=false
    gossip=412/1043 decode_err=0 rpc_err=0
```

`degraded=true` fires whenever this node sees ≤ 1 alive peer
(including itself). It is the single best signal for "this node is
partitioned away from the cluster".

---

## 3. VM lifecycle

```bash
celctl vm create --label web-1 --image /var/lib/celium/images/golden.raw \
                 --cpu 2 --memory 2G
celctl vm start  /vms/0          # stages boot blob, records CRC-32C
celctl vm state  /vms/0
celctl vm reset  /vms/0          # terminal -> Created (keeps digest)
celctl vm start  /vms/0          # re-stages; refuses if image bytes changed
```

### Image content drift

The flow above is the W19-A drift guard. If the operator (or a
supply-chain incident) mutates the image file between starts, the
second `celctl vm start` fails with:

```
Invalid: boot blob: image content changed since last start
```

and the VM stays in `Created` with the **original** CRC-32C still
recorded. To accept the new bytes intentionally:

1. `celctl image checksum /var/lib/celium/images/golden.raw` —
   confirm the new digest you expect.
2. `celctl vm delete /vms/0`.
3. `celctl vm create … --image …` with the new image. The fresh
   record has no prior digest, so the next start records the new
   one.

This is **intentional friction** — the drift guard exists precisely
so accidental image swaps cannot pass silently.

### Fleet-wide image attribution

Every `RemoteVm` row carries `image_path`, `cpu_count`,
`memory_mib`, and `boot_blob_crc32c` (W18.4). From any node:

```bash
celctl cluster vms --bind 0.0.0.0:0 --node-id observer --cluster prod \
    --seeds <any-live-node>:7401 --settle 2
```

groups every guest by owner. After a node loss the last-known row
is retained with `owner_alive=false` so you can still see which
image a now-departed node was running — critical for post-mortem.

---

## 4. Volume operations

```bash
celctl cluster vol create   --target peer-a --name data-1 --size 1073741824
celctl cluster vol list     --target peer-a
celctl cluster vol snapshot --target peer-a --volume <id> --name pre-upgrade
celctl cluster vol restore  --target peer-a --snapshot <id>
```

`FileVolumeStore::integrity_check()` (exercised by 3 dedicated
tests, see [ADR 0002](adr/0002-atomic-state-persistence.md))
walks the on-disk manifest, opens every body file, and reports any
torn body / missing body / orphan manifest entry. There is no CLI
surface for this yet — it is invoked programmatically by future
recovery tooling. The intent is to wire a `celctl cluster vol scrub
--target X` command once it stabilises.

---

## 5. Recovery playbooks

### "A node thinks it's alone"

Symptom: `degraded=true` in the summary line; `alive_count() == 1`.

1. Confirm reachability to the seeds (TCP/UDP to the gossip port).
2. Check `celmesh_decode_errors_total` is zero. Non-zero ⇒ a peer
   is shipping malformed frames or a foreign cluster is bleeding
   in.
3. Inject a fresh seed at runtime — the join API records the
   address even if the immediate `Hello` fails, so the next
   gossip tick retries:

   ```bash
   # invoked from inside an existing `cluster start` process; or
   # use a fresh observer node with --seeds <peer-addr>
   ```

### "A peer is permanently gone"

The failure detector promotes Alive → Suspect → Dead after
`timeout_suspect` and `timeout_dead`. The federation table keeps
the last-known VM rows with `owner_alive=false` so operators can
still attribute the workloads. If the peer is truly retired:

1. Wait for `dead_promotions_total` to bump.
2. If a VM had `restart_policy=always`, the elected supervisor on
   the **lowest Alive node-id** will recreate it on its own host
   (counted by `celmesh_supervisor_restarts_total`).
3. To force a recovery pass without waiting for the next interval:

   ```bash
   celctl cluster recover --bind 0.0.0.0:0 --node-id observer \
       --cluster prod --seeds <any-live-node>:7401 --settle 2
   ```

### "VM refuses to start with `boot blob: image content changed`"

See *Image content drift* under §3. This is the W19-A drift guard
firing. If the new bytes are intentional, delete and recreate the
VM; if they are not, restore the image from your golden source and
retry.

### "Controller state file is unreadable"

The W19-B atomic-rename path makes this nearly impossible to
provoke in normal operation. If it does happen (manual edit, disk
corruption, …):

1. Look for a sidecar `<state>.tmp.<pid>` in the same directory —
   that is the in-flight write from a crashed process. Either
   discard it or, if its contents are intentional, atomically
   `mv state.tmp.<pid> state.json` yourself.
2. Otherwise restore the file from backup. The schema is plain
   JSON with a `version` field, so a 5-line `jq` patch can repair
   most field-level damage by hand.

---

## 6. Configuration reference

| Setting                | Default       | Effect                                             |
|------------------------|---------------|----------------------------------------------------|
| `gossip_interval`      | 100 ms        | How often the gossiper fires.                      |
| `timeout_suspect`      | 750 ms        | Alive → Suspect promotion deadline.                |
| `timeout_dead`         | 2500 ms       | Suspect → Dead promotion deadline.                 |
| `supervisor_interval`  | 1 s           | Auto-supervisor cadence (0 = disabled).            |
| `MAX_VMS`              | 4             | Slot table size; mirrors `celhyper::manager`.      |
| `BOOT_BLOB_LEN`        | 4096 B        | First-page staging size for drift detection.       |

Tighter intervals shorten failure detection but cost gossip
bandwidth. The defaults are tuned for ≤ 32 nodes; tune up
proportionally for larger fleets.

---

## 7. Test surfaces operators can rely on

The Core Layer ships 171 workspace tests + 3 ignored UDP soak
tests. The ones an operator should know about:

* `crates/celtest/tests/w17_core.rs` — gossip metrics + cross-node
  RPC round-trips, including timeout accounting.
* `crates/celtest/tests/w20_e2e.rs` — full image-metadata gossip
  propagation, LWW digest replacement, owner-departure retention.
* `crates/celtest/tests/multi_node.rs` — membership convergence
  and the federated path grammar (`/cluster/<node>/vms/<n>`).
* `crates/celcli/src/vm.rs::tests` (W19-A/B) — drift detection
  across processes and crash-safe controller persistence.

If you encounter behaviour that contradicts the assertions in those
tests, you have found a real bug — please attach the failing
trace and a minimal repro.
