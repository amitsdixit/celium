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
