# ADR 0003 — Cluster Gossip of Image Metadata

Status: Accepted (2026-05-17, W18.4)

## Context

W18.1–W18.3 added host-side disk-image support to `celcli`: a VM may
be created with `--image <path> --cpu N --memory M`, and W18.3
stages the first 4 KiB of that image into `build/stage/vm-N/boot.blob`
recording the staged length and a Castagnoli CRC-32C on the
`VmRecord`.

W19-A then introduced content-drift detection: on a subsequent
`start_vm` the controller re-stages and compares the new digest
against the recorded one, refusing to launch if they differ.

That story works for a single host. In a cluster the operator
needs the same answers from any node:

* *"Which image is node `a`'s VM 0 actually running?"* — for change
  control and audit.
* *"Has anyone's boot blob drifted?"* — post-mortem after a fleet
  rotation or supply-chain incident.
* *"Why did node `a`'s VM refuse to start?"* — without SSHing into
  `a`.

We need the image-attribution fields on the wire.

## Decision

Extend the cluster-federation row type `celmesh::RemoteVm` with four
new optional fields, all `#[serde(default,
skip_serializing_if = "Option::is_none")]`:

| Field              | Type            | Source                                                 |
|--------------------|-----------------|--------------------------------------------------------|
| `image_path`       | `Option<String>` | `VmRecord::image_path` (operator-supplied at create) |
| `cpu_count`        | `Option<u32>`   | `VmRecord::cpu_count`                                  |
| `memory_mib`       | `Option<u64>`   | `VmRecord::memory_mib`                                 |
| `boot_blob_crc32c` | `Option<u32>`   | `VmRecord::boot_blob_crc32c` (set by W18.3 staging)   |

The serde defaults are the load-bearing back-compatibility lever:

* W11/W12/W17-era senders ship `Sync` envelopes whose JSON omits
  these keys entirely. W18.4 receivers deserialise them as `None`
  — no version bump, no breakage. The reverse direction is
  symmetric: a W18.4 sender omits absent fields on the wire, so a
  W11 receiver sees the same shape it always did.
* The federation merge is unchanged. The existing
  `(epoch, hlc)` last-writer-wins rule already does the right
  thing for these fields: a re-stage on the owner bumps `hlc` so
  every peer converges on the new CRC.

## Consequences

* `celctl cluster vms` (and the W20 `celctl cluster status`) now
  surface image identity and the boot digest from any node.
  Operators can attribute every running guest to a specific image
  without per-node access.
* `RemoteVm` row size grows by four optional fields. The on-wire
  cost is zero for VMs that have no image set, which matches the
  pre-W18 fleet exactly.
* Cross-version gossip (mixed W17 and W18.4 nodes) continues to
  work. We exercise this with
  `legacy_wire_payload_without_image_fields_still_deserialises` in
  `celmesh::federation::tests`.
* The W20-C integration suite
  (`crates/celtest/tests/w20_e2e.rs`) asserts the full E2E story
  over a real 3-node `MemTransport` cluster:
  `image_metadata_propagates_to_every_peer`,
  `boot_blob_digest_update_overtakes_previous_value`, and
  `owner_departure_preserves_image_fields_for_diagnosis`.

## Alternatives considered

* **Bump the protocol version (`PROTO_VERSION = 2`)** — rejected.
  Optional fields with serde defaults give us wire compatibility
  without forcing every operator to roll the whole cluster in
  lock-step. A version bump is reserved for incompatible *shape*
  changes (renamed enum variants, removed fields).
* **Side-channel the image fields via a separate RPC** — rejected.
  An image-attribution surface that requires a successful RPC
  to every peer fails exactly when the operator most needs it
  (degraded cluster, post-mortem).
* **Embed the full image manifest** — rejected. A CRC-32C digest
  plus the path is enough to detect drift and identify the source.
  Full manifests can land in a future content-addressed image
  store without touching `RemoteVm` again.

## References

* W18.4 federation tests:
  `crates/celmesh/src/federation.rs::tests::{image_metadata_propagates_through_merge,legacy_wire_payload_without_image_fields_still_deserialises}`.
* W20-C E2E suite: `crates/celtest/tests/w20_e2e.rs`.
* W18.3 staging path: `crates/celcli/src/boot.rs`.
* W19-A drift detection: `crates/celcli/src/vm.rs::Controller::start_vm`.
