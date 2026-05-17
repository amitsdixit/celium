# ADR 0004 \u2014 Core Layer sign-off (W26)

* **Status:** Accepted
* **Date:** 2026-05-17
* **Context tag:** W26

## Context

The Core Layer of Celium has been built up over 26 weekly milestones:

* W1\u2013W6   Foundation \u2014 workspace, errors, tracing, metrics, base mesh.
* W7\u2013W9   VMX bring-up, VM-exit dispatcher, capability IPC.
* W9\u2013W14  Clustering \u2014 SWIM-lite gossip, federation, K8s personality.
* W12\u2013W17 CelVault \u2014 volumes, snapshots, integrity, file backend.
* W15     Networking primitives (CIDR / NIC / secgroup / LB).
* W17     Core hardening (metrics, timeouts, flush/stats/integrity).
* W18\u2013W21 Image abstraction, admin surface, scheduler polish.
* W22\u2013W23 Host \u21d4 kernel bridge over COM2, end-to-end live wire.
* W24    Bare-metal foundation \u2014 handoff v3, SMP skeleton, virtio-net,
         hybrid ISO + USB + PXE installer.
* W25    LAPIC driver, PCI scanner, hot-path metrics, virtio-console +
         NVMe skeletons, live `smp::send_ipi`.
* W26    Documentation, runbooks, polish, sign-off (this ADR).

The Tenancy Layer (W27+) will land multi-tenant memory, virtqueue I/O,
AP wake, and physical-hardware validation.

## Decision

We declare the **Core Layer feature complete** as of commit on which
this ADR lands. Subsequent work is in scope of the Tenancy Layer.

The Tenancy Layer inherits a hard contract:

1. **No `unwrap()` / `panic!()` on production paths.** Every fallible
   call returns `Result<T, CelError>` (host) or `HyperResult<T>`
   (kernel).
2. **Every `unsafe` block carries a `// SAFETY:` comment** \u2014 audited
   in W26 across both `celhyper` and `celloader`.
3. **`celmesh` and `celvault` remain `#![forbid(unsafe_code)]`.**
   The Tenancy Layer must not relax this.
4. **Wire formats are versioned.** CelLoader handoff is at v3,
   CelMesh envelope at v1. Bump the version when fields change.
5. **Bridge timeouts are typed.** `CelError::Timeout(String)` for
   RPCs that exceeded their deadline; never collapsed to `Io`.
6. **Capabilities are required.** Every mutating VmOp checks a
   capability bit; the receiving host returns the stable string
   `capability denied: <op_tag>` on rejection.

## Consequences

* The Tenancy Layer can change crate-internal APIs freely. The
  external wire (handoff v3, mesh envelope v1, bridge NDJSON) must
  remain backwards-compatible within a major version or bump the
  version field.
* Two known Windows gossip-timing tests
  (`multi_node::departed_owner_keeps_last_known_vms_with_owner_alive_false`
  and `w20_e2e::owner_departure_preserves_image_fields_for_diagnosis`)
  flake under full workspace parallelism. They pass in isolation and
  reliably on Linux. The Tenancy Layer is expected to either (a) lower
  the gossip detection timeouts on Windows CI or (b) replace the SWIM
  tick scheduler with a deterministic test harness.
* Deferred-to-W26 typed-TODO returns (`HyperError::Unimplemented`
  carrying `W26` tags) are now re-tagged `W27` and become the
  Tenancy Layer's day-one backlog: real INIT-SIPI AP wake, virtqueue
  allocator + MSI-X programmer, real virtio / NVMe I/O paths, ECAM
  PCI fast path, x2APIC support, physical-hardware validation.

## Alternatives considered

* **Ship Tenancy Layer features inside the Core Layer.** Rejected
  because the Tenancy boundary is precisely where multi-tenant memory
  layouts and virtqueue ownership are decided; mixing those into the
  Core Layer would muddy what "feature complete" means.
* **Wait for physical-hardware validation before signing off.**
  Rejected because the dev workspace has no physical Intel VMX-capable
  server access. The runbooks ([docs/runbooks/](../runbooks/)) and
  the installer ([docs/INSTALLER.md](../INSTALLER.md)) hand the bring-up
  to the operator; on-metal validation is now an operator-driven
  acceptance gate, not a code-author one.
