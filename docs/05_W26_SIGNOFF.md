# W26 \u2014 Core Layer sign-off

Week 26 is the final sprint for the **Core Layer**: CelHyper (Type-1
bare-metal hypervisor), CelMesh (clustered control plane), CelVault
(storage + networking primitives), CelCli (operator binary), and
CelCommon (shared types). With W26 closed, the Core Layer is **feature
complete** for the goals declared in `00_GLOBAL_CONVENTIONS.md` and
[`01_CELHYPER.md`](01_CELHYPER.md). The next milestone is the
**Tenancy Layer**.

## Sign-off matrix

| Theme | Status |
|---|---|
| Type-1 bare-metal boot (UEFI \u2192 CelLoader \u2192 CelHyper) | DONE (W23-D) |
| Real image loading from staged handoff blob | DONE (W23-E) |
| VMX bring-up + EPT + vCPU scheduling | DONE (W4\u2013W7) |
| Multi-VM namespace + capability IPC | DONE (W7\u2013W9) |
| Host bridge (NDJSON over TCP on COM2) | DONE (W22\u2013W23) |
| SWIM-lite gossip + namespace federation | DONE (W9\u2013W14) |
| Persistent volumes + snapshots (Mem + File backends) | DONE (W12\u2013W17) |
| Networking primitives (CIDR, NIC, secgroup, LB) | DONE (W15) |
| K8s-as-a-Service personality | DONE (W15) |
| CelHyper bridge wire (Create/Start/Stop/Delete/List + ImageLoad) | DONE (W22\u2013W23-E3) |
| Hybrid ISO + USB + PXE installer | DONE (W24-E) |
| Handoff v3 (SMP + framebuffer) | DONE (W24-A) |
| Real LAPIC driver (xAPIC MMIO) + live IPI | DONE (W25-A) |
| Legacy PCI scanner | DONE (W25-C) |
| virtio-blk / virtio-net / virtio-console / NVMe driver surfaces | DONE skeleton (W23-D, W24-C, W25-D, W25-E) |
| Hot-path metrics (11 atomic counters) | DONE (W25-F) |
| Operator runbooks + installer guide | DONE (W26) |
| ADR 0004 core-layer sign-off | DONE (W26) |
| W26 release tag | n/a (single-branch repo) |

## Deliberately deferred to Tenancy Layer (W27+)

| Item | Owner | Why deferred |
|---|---|---|
| AP wake (INIT-SIPI trampoline page + per-AP boot stacks) | Tenancy-A | Trampoline allocator depends on tenant memory model; safer to land alongside the multi-tenant memory manager. |
| Virtqueue allocator + MSI-X programmer | Tenancy-B | Same reason \u2014 tenant boundaries determine queue isolation. |
| Full virtio-blk / virtio-net / virtio-console / NVMe I/O paths | Tenancy-B | Depend on the virtqueue allocator. |
| ECAM PCI fast path (MCFG-driven) | Tenancy-B | CelLoader handoff does not yet carry MCFG; legacy port-IO scanner covers W25 needs. |
| x2APIC | Tenancy-A | LAPIC drift; will land with NUMA work. |
| Bare-metal validation on physical VMX server | Operator-owned | Cannot be performed from the dev workspace; runbook published in [INSTALLER.md](INSTALLER.md). |

## Test inventory

```text
cargo test --workspace --no-fail-fast
# 197 passed / 2 failed / 7 ignored
# Both failures are documented Windows gossip-timing flakes:
#   * multi_node::departed_owner_keeps_last_known_vms_with_owner_alive_false
#   * w20_e2e::owner_departure_preserves_image_fields_for_diagnosis
# Both pass in isolation with --test-threads=1.

cargo check --target x86_64-unknown-none --release -p celhyper      # PASS
cargo check --target x86_64-unknown-uefi --release \
            --features real-handoff -p celloader                    # PASS
cargo check --workspace --all-targets                               # PASS
```

## Discipline reminders the Tenancy Layer inherits

* Every fallible API returns `Result<T, CelError>` (host) or
  `HyperResult<T>` (kernel).
* No `unwrap()` / `panic!()` on production paths.
* Every `unsafe` block carries a `// SAFETY:` comment.
* `celmesh` and `celvault` are `#![forbid(unsafe_code)]`. Do not
  introduce `unsafe` into the control plane.
* Wire protocols are length-prefixed JSON envelopes; bump version
  when fields change. CelLoader handoff is at v3, CelMesh envelope
  at v1.

## Files added by W26

* `docs/05_W26_SIGNOFF.md` (this file)
* `docs/INSTALLER.md`
* `docs/runbooks/install.md`
* `docs/runbooks/first-boot.md`
* `docs/runbooks/cluster-join.md`
* `docs/runbooks/vm-create.md`
* `docs/adr/0004-core-layer-signoff.md`

## How to declare the Core Layer "done" in your repo

```bash
git tag -a core-layer-v1.0 -m "Core Layer signed off at W26"
# (left to the operator; the workspace does not tag itself)
```
