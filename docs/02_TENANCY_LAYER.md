# 02 — Tenancy Layer

> W27. Multi-tenant operator surface built **on top of** the existing
> Core Layer (celmesh + celvault) without changing any Core-Layer
> code.

## Goals

1. **Tenants** — named, isolated administrative subtrees of the global
   `/tenants/<name>/` namespace.
2. **Users** — per-tenant principals with **attenuated** capability sets
   that are a subset of the tenant's root capabilities.
3. **Quotas** — per-tenant ceilings on vCPU, memory, storage, network
   throughput, and IOPS, with explicit charge / release accounting.
4. **Capability projection** — every operation the Tenancy Layer
   authorises is projected to a `celmesh::Capabilities` bitset before
   it enters the Core Layer. The Core Layer's existing W14 capability
   check is the **only** enforcement point — the Tenancy Layer never
   short-circuits or duplicates it.

The Core Layer remains untouched: it has no idea tenants exist. All
tenant-aware code lives in `crates/celtenancy/`.

## Crate layout

```
crates/celtenancy/
├── Cargo.toml
└── src/
    ├── lib.rs           # public re-exports, crate-wide lints
    ├── caps.rs          # TenantCaps bitset + attenuate() + projection
    ├── namespace.rs     # TenantNamespace path builder + validation
    ├── quota.rs         # TenantQuotas, QuotaUsage, charge/release math
    ├── tenant.rs        # Tenant, TenantSpec, TenantId
    ├── user.rs          # User, UserId
    ├── store.rs         # TenantStore trait + Mem and File backends
    └── main.rs          # `celtenancy` admin binary (clap)
```

## Namespace convention

A tenant `acme` owns the prefix `/tenants/acme/`. Sub-paths:

| Path | Owner |
|------|-------|
| `/tenants/acme/vms`       | VM slots |
| `/tenants/acme/volumes`   | Persistent volumes |
| `/tenants/acme/networks`  | Virtual networks |
| `/tenants/acme/users`     | Per-tenant principals |
| `/tenants/acme/quotas`    | Quota state |
| `/tenants/acme/users/<n>` | Single user |

Tenant names are validated by `namespace::validate_segment`:
non-empty, ≤ 64 bytes, ASCII `[A-Za-z0-9_-]`. Anything else is rejected
with `CelError::Invalid("tenancy: ...")`.

## Capability model

`TenantCaps` is a `u32` bitset that mirrors `celmesh::Capabilities`
one-to-one. It is `#[serde(transparent)]` so the wire / on-disk form
is just the raw bits.

```rust
use celtenancy::TenantCaps;
let caps = TenantCaps::parse_tags("vm.read,vm.write,vol.read")?;
assert!(caps.contains(TenantCaps::VM_LIFECYCLE_READ));
let core = caps.to_mesh_capabilities();   // crosses into the Core Layer
```

`attenuate(root, requested)` rejects escalation:

```rust
celtenancy::attenuate(TenantCaps::VM_LIFECYCLE_READ,
                      TenantCaps::VOLUME_WRITE)
    .unwrap_err();   // CelError::CapabilityDenied("tenant.user.attenuate")
```

`to_mesh_capabilities()` is the **only** crossing point into
`celmesh::Capabilities`. Anywhere a tenant-owned operation needs to
hit the Core Layer, it goes through this projection — keeping
celmesh blissfully unaware of tenants.

## Quota model

| Field             | Type | Unit  |
|-------------------|------|-------|
| `max_vcpus`         | u32  | vCPUs |
| `max_memory_mib`    | u64  | MiB   |
| `max_storage_bytes` | u64  | bytes |
| `max_network_mbps`  | u32  | Mbps  |
| `max_iops`          | u32  | IOPS  |

`charge_quota(usage, quotas, charge)` uses `saturating_add` so an
overflow can never roll over; instead the per-resource ceiling check
returns one of the stable tags:

`quota.vcpus`, `quota.memory`, `quota.storage`, `quota.network`,
`quota.iops` → all surfaced as `CelError::Exhausted(&'static str)`.

`release_quota(usage, charge)` uses `saturating_sub`, never underflows.

A tenant cannot be deleted while `usage != QuotaUsage::default()`:

```rust
store.delete(t.id)?;   // CelError::Invalid("tenant in use") if anything is charged
```

## Persistence

Two store backends share a `StoreState` (next-ids + maps):

* `MemTenantStore` — `Mutex<StoreState>`, for tests and the
  `celtenancy` admin binary's default no-`--store` mode.
* `FileTenantStore` — same in-memory state plus an atomic write
  routine: serialise to JSON, write `<path>.tmp`, `sync_all`,
  rename onto `<path>`. Survives process crashes mid-write because
  rename is atomic and the temp file is never visible under the
  canonical name.

```rust
let store = FileTenantStore::open("./build/celctl-tenants.json")?;
```

## CLI surface

### `celtenancy` admin binary

Standalone administration without `celctl`.

```pwsh
celtenancy tenant create --name acme --max-vcpus 32
celtenancy tenant list
celtenancy tenant show --name acme
celtenancy user add --tenant acme --name alice --caps "vm.read,vm.write"
celtenancy quota charge --tenant acme --vcpus 4 --memory-mib 4096
celtenancy quota show --tenant acme
```

Pass `--store <path>` to use a persistent file store; omit it for the
ephemeral in-memory backend.

### `celctl tenant`

The same surface is wired into `celctl`. The `--store` flag defaults
to `./build/celctl-tenants.json`.

```pwsh
celctl tenant create --name acme --caps all
celctl tenant user add --tenant acme --name alice --caps vm.read
celctl tenant show --name acme
```

## Architectural contract

* **The Core Layer (celmesh + celvault) is not modified by the
  Tenancy Layer.** Every capability check still happens inside
  `celmesh::MemVmHost::apply` via `Capabilities::required(op)`. The
  only addition is upstream: the Tenancy Layer mints the
  `Capabilities` bitset the host runs with.
* **No `unwrap()` / `panic!()` on production paths.** Every fallible
  operation returns `Result<T, CelError>`. Tenancy errors map to
  `CelError::Invalid` (validation), `CelError::Exhausted` (quotas),
  `CelError::CapabilityDenied` (escalation), `CelError::Storage`
  (I/O / serialization).
* **`#![forbid(unsafe_code)]`** on every file in `crates/celtenancy`.
* **Capability tags are stable strings.** New code that wants to add
  a denial reason must reuse one of the existing `&'static str` tags
  or extend `caps.rs` deliberately.

---

## W28 — Tenant Runtime Binding

> Wires the W27 primitives into the **live** Core Layer host. The
> Core Layer remains untouched.

### `TenantVmHost`

`celtenancy::TenantVmHost` is a `celmesh::VmHost` wrapper that
auto-charges and refunds the tenant's `TenantStore` on every
`VmOp` it forwards to an inner `Arc<dyn VmHost>`:

```
+-------------+        +------------------+        +--------------+
| caller     ──▶│  TenantVmHost   │──▶│  MemVmHost   │
| (cli/api)   |        | (Tenancy Layer) |        | (Core Layer) |
+-------------+        +------------------+        +--------------+
                              │
                              ▼
                      +----------------+
                      |  TenantStore   |
                      |  (quota book)  |
                      +----------------+
```

For each forwarded `VmOp`:

1. **Plan** a `QuotaCharge` from the op (only `Create` and
   `CreateVolume` consume quota; everything else is zero-charge).
2. **Charge** the `TenantStore` *before* dispatch. Quota exhaustion
   short-circuits with `Err("tenant: quota: ...")` and the inner
   host is never called.
3. **Dispatch** to the inner `VmHost`. Inner-host failures
   (including `capability denied`) trigger a **refund** so the
   tenant's book never leaks on the error path.
4. **Track** the charge by `vm_id` / `volume_id` on success so that
   `Delete` / `DeleteVolume` can refund the original allocation.

This means a multi-tenant deployment can share one `MemVmHost` and
one `MemVolumeStore` across tenants while each tenant gets:

* its own isolated quota book,
* its own projected `Capabilities` bitset (root caps, or per-user
  attenuated caps via `TenantCaps::to_mesh_capabilities()`),
* automatic refund on both explicit delete *and* inner-host
  failure (capability denial, validation, etc.).

### Snapshot precondition

`MemVmHost` mints VM and volume ids relative to the owning node
and records the owner on first `snapshot(&node)`. Callers wrapping
a fresh `MemVmHost` in a `TenantVmHost` **must** prime it once
before issuing `Create` / `CreateVolume` / `CreateNetwork`:

```rust
let inner = Arc::new(MemVmHost::with_caps(caps));
let tenant_host = TenantVmHost::new(tid, store, inner);
let _ = tenant_host.snapshot(&node).await; // one-time prime
```

This is a Core-Layer property and is not changed by the Tenancy
Layer.

### Test coverage

`crates/celtest/tests/tenant_runtime_e2e.rs` covers, end-to-end:

1. **Independent quota books** — two tenants on one shared host
   keep separate usage counters.
2. **Quota isolation under pressure** — exhausting tenant A's
   quota does not affect tenant B.
3. **Per-user attenuated caps** — within one tenant, two users
   with different `TenantCaps` see different effective hosts even
   though they share the tenant's quota.
4. **Capability-denied paths do not charge** — refund-on-failure
   guarantees zero quota leak when the inner host rejects an op.
5. **Delete refunds resources** — capacity reclaimed after
   `Stop` + `Delete` is fully reusable.

---

## W29 — Tenant Exec Dispatcher

> Exposes the W28 [`TenantVmHost`] wrapper through a single-shot,
> ephemeral-host CLI surface for diagnostics and admin
> "would-this-op-succeed" dry-runs.

### `celtenancy::exec::exec`

```rust
pub async fn exec(
    store:       Arc<dyn TenantStore>,
    tenant:      &str,
    user:        Option<&str>,
    op:          VmOp,
    opts:        ExecOptions,
) -> CelResult<ExecAudit>;
```

Builds an ephemeral `MemVmHost` whose `Capabilities` come from
`TenantCaps::to_mesh_capabilities()` — root caps when `user` is
`None`, the user's already-attenuated caps otherwise — wraps it in
a `TenantVmHost` bound to the real `TenantStore`, dispatches one
`VmOp`, and returns a structured [`ExecAudit`] describing every
observable step.

* The host is **ephemeral** — VMs/volumes created via `exec` do
  not survive the call.
* Quota charges, however, hit the **real** `TenantStore`, so a
  successful `Create` leaves a persistent reservation behind by
  default.
* `ExecOptions::release_after_create = true` flips the trip into
  an atomic charge-and-refund **dry-run** that lands the store
  back at its starting usage — useful for "can this tenant
  currently allocate 2 vCPUs?" probes.

### `ExecAudit`

`serde`-serializable. Captures the full audit trail:

| field                 | meaning                                                |
| --------------------- | ------------------------------------------------------ |
| `tenant`, `user`      | resolved tenant + user names                           |
| `op`                  | variant tag (`Create`, `CreateVolume`, …)              |
| `op_capability_tag`   | the `Capabilities::op_tag` constant Core demands       |
| `effective_caps`      | `TenantCaps::to_tags()` of the host's projected caps   |
| `planned_charge`      | the `QuotaCharge` the wrapper planned (`None` for reads) |
| `dispatch_succeeded`  | `true` if the inner host accepted the op               |
| `error`               | failure string from the inner host or charge step      |
| `reply`               | brief reply summary on success                         |
| `usage_before/after`  | tenant `QuotaUsage` snapshots bracketing the call      |

### `celctl tenant exec`

```
celctl tenant exec vm-create \
    --tenant acme [--user alice] \
    --label web --cpus 2 --memory-mib 1024 \
    [--release-after] [--json]

celctl tenant exec volume-create \
    --tenant acme [--user alice] \
    --name data --size-bytes 4096 \
    [--release-after] [--json]
```

Both forms open the configured `FileTenantStore` (default
`./build/celctl-tenants.json`, override with `--store`), build a
single-threaded tokio runtime, drive `exec::exec`, and print
either a human summary or `--json`-serialized `ExecAudit`.

### Test coverage

* `crates/celtenancy/src/exec.rs` — 6 unit tests (success,
  release-after-create round-trip, quota exhaustion,
  capability-denied refund, unknown-user error, volume charge).
* `crates/celtest/tests/tenant_exec_e2e.rs` — 2 e2e tests
  proving:
  1. successful Create through `FileTenantStore` persists the
     reservation across process restarts;
  2. `release_after_create` round-trip leaves disk state at
     baseline (charge-and-refund dry-run).

### Limitations / out of scope for W29

* The host is per-invocation — there is **no** cross-call VM or
  volume state. `tenant exec vm-delete` would have nothing to
  delete; we deliberately omit it.
* Cluster-wide live-host integration (i.e. dispatching through a
  real running Core-Layer node instead of an ephemeral host)
  remains future work.

---

## W30 — Tenant Audit Sink

> Adds a persistent, structured audit trail for every tenant op.
> The `AuditSink` trait + `MemAuditSink` / `FileAuditSink`
> implementations are plumbed into both `TenantVmHost` and
> `exec::exec`, so charge / release / deny / dispatch outcomes are
> recorded automatically without changing call sites.

### `celtenancy::audit`

```rust
pub trait AuditSink: Send + Sync + Debug {
    fn record(&self, event: AuditEvent); // best-effort, infallible
}

pub enum AuditAction { Charge, Release, Deny, Exec }

pub struct AuditEvent {
    pub timestamp_millis:  u64,
    pub tenant:            String,
    pub user:              Option<String>,
    pub action:            AuditAction,
    pub op_capability_tag: Option<String>,
    pub charge:            Option<QuotaCharge>,
    pub success:           bool,
    pub error:             Option<String>,
    pub note:              Option<String>,
}
```

* `MemAuditSink` — `Vec<AuditEvent>` behind a `Mutex`. Tests and
  diagnostics use `MemAuditSink::events()` to snapshot history.
* `FileAuditSink` — append-only JSON-lines on disk. `record` is
  best-effort (errors logged at `warn`); `read_all`,  `tail(n)`,
  and `count` parse the log back, silently skipping malformed
  lines so a crash mid-write cannot poison the history.

### Integration

* `TenantVmHost::with_audit(sink).with_audit_user(name)` builder
  hooks. Wrapper emits:
  - `Charge { op_tag, charge }` after a successful pre-bill,
  - `Deny  { op_tag, charge, error }` on quota exhaustion,
  - `Deny  { op_tag, charge, error, note="refunded" }` when the
    inner host rejects a Create and the wrapper refunds,
  - `Deny  { op_tag, error }` for any other inner-host error
    (capability-denied reads, malformed ops, etc.),
  - `Release { op_tag, charge }` after a successful `Delete*`.
* `ExecOptions { audit: Option<Arc<dyn AuditSink>> }` propagates
  the sink into the ephemeral wrapper and adds one **terminal**
  `Exec` event per call summarizing the trip (`note="op=…
  released=true reply=…"`).

### CLI

```text
celctl tenant exec vm-create     ... [--audit-log PATH]
celctl tenant exec volume-create ... [--audit-log PATH]
celctl tenant audit tail  --audit-log PATH [-n N] [--json]
celctl tenant audit stats --audit-log PATH
```

`tail` prints a fixed-format line per event (or `--json` for
pretty-printed `Vec<AuditEvent>`). `stats` returns one line:
`total / charges / releases / execs / denied`.

### Test coverage

* `crates/celtenancy/src/audit.rs` — 7 unit tests
  (event builder, mem sink, file sink round-trip, reopen+append,
  tail-n, malformed-line tolerance).
* `crates/celtenancy/src/exec.rs` — 3 additional unit tests for
  the sink integration (Charge+Exec on success, Deny+Exec on
  quota exhaustion, Charge+Release(dry-run)+Exec on
  `release_after_create`).
* `crates/celtest/tests/tenant_audit_e2e.rs` — 2 e2e tests
  proving FileAuditSink history survives multiple process
  restarts and records dry-run releases correctly.

### Limitations

* The sink is **process-local** — there is no cluster-wide audit
  bus or remote shipping. Operators are expected to point each
  node's `--audit-log` at a tail-collected file (Vector, Fluent
  Bit, …) if they want fan-in.
* Recording is best-effort by design. A blown disk does not fail
  a tenant op; you'll see `warn!` lines instead.
* `ExecOptions` no longer derives `Serialize` / `Deserialize`
  because it now holds an `Arc<dyn AuditSink>`. The output
  `ExecAudit` shape is unchanged and remains fully serializable.

---

## W31 — Nested Tenants

> Adds a parent/child hierarchy to the tenant store. Subtenants
> inherit a subset of the parent's caps and per-dimension quotas;
> charge/release calls propagate up the ancestor chain so a
> parent's usage always equals the sum of its own direct charges
> plus every descendant. The Core Layer is untouched — projection
> still happens through `TenantCaps::to_mesh_capabilities()` at
> the same single seam.

### `celtenancy::tenant`

```rust
pub struct TenantSpec {
    pub name:   String,
    pub quotas: TenantQuotas,
    #[serde(default)]
    pub parent: Option<TenantId>,   // NEW
}

impl TenantSpec {
    pub fn new(name, quotas) -> CelResult<Self>;        // parent = None
    pub fn with_parent(self, parent: TenantId) -> Self; // builder
}

pub struct Tenant {
    /* …existing fields… */
    #[serde(default)]
    pub parent: Option<TenantId>,   // NEW; migration-safe
}
```

Existing on-disk `tenants.json` files written by W27..W30 reopen
unchanged: every record gets `parent = None` from `#[serde(default)]`.

### `celtenancy::TenantStore` — new default methods

```rust
pub trait TenantStore: Send + Sync {
    /* …existing required methods… */

    fn create_subtenant(&self, parent: TenantId, spec: TenantSpec, caps: TenantCaps)
        -> CelResult<Tenant>;       // sugar over create(spec.with_parent(p), caps)

    fn children(&self, parent: TenantId) -> CelResult<Vec<Tenant>>;
    fn ancestors(&self, id:     TenantId) -> CelResult<Vec<Tenant>>;
}
```

`children` filters `list()` by `parent == Some(...)`. `ancestors`
walks the parent chain (refuses depth > 64 as a corruption guard,
surfacing `CelError::Internal("tenant hierarchy too deep")`).

### Validation rules (enforced inside `create`)

When `spec.parent = Some(p)`:

1. **Parent must exist** — else `CelError::Invalid("parent tenant id unknown")`.
2. **Caps ⊆ parent caps** — else `CelError::CapabilityDenied("subtenant caps exceed parent")`.
3. **Each quota dimension ≤ parent quota** — else `CelError::Invalid("subtenant quotas exceed parent quotas")`.

### Charge / release propagation

`charge(child, c)` now walks `[child, parent, grandparent, …]`:

1. **Validate pass** — `charge_quota` is called against every
   ancestor's *current* usage/quota. If any level would exceed,
   the whole call fails with the standard `CelError::Exhausted("quota.…")`
   tag of the first dimension that ran out. **No partial state
   ever lands** (mutex held the whole time).
2. **Apply pass** — the validated charge is added to every level.

`release(child, c)` does the same walk with saturating subtraction
so a double-release floors at zero on every level. Both methods
return the **child's** new `QuotaUsage`.

### Delete semantics

`delete(parent)` now checks subtenants *before* usage:

* `CelError::Invalid("tenant has subtenants")` — refuses while
  any child points at `parent` (more actionable than the
  cascading "tenant in use" that propagated usage would otherwise
  surface).
* `CelError::Invalid("tenant in use")` — only after the subtenant
  guard clears, for direct usage on the tenant itself.

### CLI

```text
celctl tenant subtenant create --parent NAME --name NAME [--max-vcpus N …]
                               [--caps inherit|tag,tag,…]
celctl tenant subtenant list   --parent NAME
celctl tenant tree
```

* `--caps inherit` (default) copies the parent's `root_caps` verbatim.
  Any other value goes through the standard tag parser; the store
  enforces ⊆ parent at insert time.
* `tenant tree` walks the store and prints every top-level tenant
  with its descendants indented, including per-node `vcpus=used/max`
  and `mem=used/max MiB` so an operator can read pressure at a
  glance.

### Test coverage

* `crates/celtenancy/src/store.rs` — 8 unit tests
  (`subtenant_inherits_parent_field`,
  `subtenant_caps_must_be_subset_of_parent`,
  `subtenant_quotas_cannot_exceed_parent`,
  `charge_propagates_to_ancestors`,
  `charge_fails_when_ancestor_exhausted`,
  `release_propagates_to_ancestors`,
  `cannot_delete_parent_with_live_subtenant`,
  `file_store_persists_parent_field_across_reopen`).
* `crates/celtest/tests/tenant_nested_e2e.rs` — 5 e2e tests
  driving a `FileTenantStore` end-to-end through subtenant
  lifecycle, cap escalation, quota overshoot, atomic-on-failure
  charging, and process-restart durability.

### Limitations

* Names are still **globally unique** across the store (subtenants
  cannot share a name with anything else). A future iteration may
  introduce parent-scoped names; doing so today would ripple into
  the namespace shape that the Core Layer projects.
* Sibling quotas are not co-validated. The parent's quota acts as
  the ceiling on actual usage via propagation; operators can
  intentionally overcommit child quotas if they want
  best-effort sharing.

---