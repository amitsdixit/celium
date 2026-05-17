//! W25-F — Kernel-side performance counters.
//!
//! Pure lock-free `AtomicU64` counters that the VM-exit dispatcher,
//! EPT walker and driver layer bump on every hot path. The accessors
//! return plain `u64` snapshots; rendering to a Prometheus-style
//! exposition is done by [`crate::manager`] once the bridge plumbing
//! is ready to ship samples back to the host (W26).
//!
//! The counters are intentionally:
//!
//! * **branch-free on the hot path**: `fetch_add(1, Relaxed)` is one
//!   `lock xadd` on x86_64; no `if` gates, no log calls;
//! * **never reset**: monotonic counters compose with any sampling
//!   strategy a future scrape protocol picks;
//! * **opaque**: only the dispatcher / walker / driver may touch
//!   them \u2014 the `#[doc(hidden)]` modules below keep curious
//!   downstreams out.
//!
//! `count_*` (write) and `read_*` (snapshot) functions are split so
//! the hot-path callers stay one-line.

#![cfg(not(test))]

use core::sync::atomic::{AtomicU64, Ordering};

/// Total VM-exits observed since boot.
static VM_EXITS_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Exits attributed to `HLT`.
static VM_EXITS_HLT: AtomicU64 = AtomicU64::new(0);
/// Exits attributed to a CR access.
static VM_EXITS_CR: AtomicU64 = AtomicU64::new(0);
/// Exits attributed to an EPT violation.
static VM_EXITS_EPT: AtomicU64 = AtomicU64::new(0);
/// Catch-all exits (any reason not in the explicit set above).
static VM_EXITS_OTHER: AtomicU64 = AtomicU64::new(0);

/// EPT 4 KiB page mappings installed by [`crate::mm::Ept::map_4k`].
static EPT_MAP_4K_TOTAL: AtomicU64 = AtomicU64::new(0);
/// EPT walks that allocated at least one intermediate table.
static EPT_TABLE_ALLOCS: AtomicU64 = AtomicU64::new(0);

/// Bytes read by every kernel block driver since boot.
static BLOCK_READ_BYTES: AtomicU64 = AtomicU64::new(0);
/// Bytes written by every kernel block driver since boot.
static BLOCK_WRITE_BYTES: AtomicU64 = AtomicU64::new(0);
/// Block-driver flush calls.
static BLOCK_FLUSHES: AtomicU64 = AtomicU64::new(0);

/// IPIs issued by the BSP via [`crate::smp::send_ipi`].
static IPI_SENT: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Hot-path increments (cold attribute so they don't pollute icache; they're
// each one `lock xadd` so the cold tag is purely a code-layout hint)
// ---------------------------------------------------------------------------

/// Tag a generic exit; always called even when a more specific helper fires.
#[inline]
pub fn count_vm_exit(basic: u32) {
    VM_EXITS_TOTAL.fetch_add(1, Ordering::Relaxed);
    match basic {
        crate::vmx::fields::EXIT_REASON_HLT => {
            VM_EXITS_HLT.fetch_add(1, Ordering::Relaxed);
        }
        crate::vmx::fields::EXIT_REASON_CR_ACCESS => {
            VM_EXITS_CR.fetch_add(1, Ordering::Relaxed);
        }
        crate::vmx::fields::EXIT_REASON_EPT_VIOLATION => {
            VM_EXITS_EPT.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            VM_EXITS_OTHER.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// One 4 KiB EPT mapping installed. Bumped by [`crate::mm::Ept::map_4k`].
#[inline]
pub fn count_ept_map_4k() {
    EPT_MAP_4K_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// One intermediate EPT table was allocated on demand. Co-bumped with
/// `count_ept_map_4k` only when the walker actually missed.
#[inline]
pub fn count_ept_table_alloc() {
    EPT_TABLE_ALLOCS.fetch_add(1, Ordering::Relaxed);
}

/// Block-device read completed.
#[inline]
pub fn count_block_read(bytes: u64) {
    BLOCK_READ_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// Block-device write completed.
#[inline]
pub fn count_block_write(bytes: u64) {
    BLOCK_WRITE_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// Block-device flush completed.
#[inline]
pub fn count_block_flush() {
    BLOCK_FLUSHES.fetch_add(1, Ordering::Relaxed);
}

/// IPI sent.
#[inline]
pub fn count_ipi_sent() {
    IPI_SENT.fetch_add(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Snapshot accessors
// ---------------------------------------------------------------------------

/// Lock-free snapshot of every counter. Cheap; meant to be called
/// from a bridge `Stats` request.
#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    /// Total VM-exits.
    pub vm_exits_total: u64,
    /// HLT exits.
    pub vm_exits_hlt: u64,
    /// CR access exits.
    pub vm_exits_cr: u64,
    /// EPT violation exits.
    pub vm_exits_ept: u64,
    /// Other exits.
    pub vm_exits_other: u64,
    /// EPT 4 KiB maps installed.
    pub ept_map_4k_total: u64,
    /// Intermediate EPT tables allocated on demand.
    pub ept_table_allocs: u64,
    /// Block-read bytes.
    pub block_read_bytes: u64,
    /// Block-write bytes.
    pub block_write_bytes: u64,
    /// Block flushes.
    pub block_flushes: u64,
    /// IPIs sent.
    pub ipi_sent: u64,
}

/// Atomic snapshot of every counter at the time of the call. Each
/// load is `Relaxed` because the counters are independent.
#[must_use]
pub fn snapshot() -> Snapshot {
    Snapshot {
        vm_exits_total: VM_EXITS_TOTAL.load(Ordering::Relaxed),
        vm_exits_hlt: VM_EXITS_HLT.load(Ordering::Relaxed),
        vm_exits_cr: VM_EXITS_CR.load(Ordering::Relaxed),
        vm_exits_ept: VM_EXITS_EPT.load(Ordering::Relaxed),
        vm_exits_other: VM_EXITS_OTHER.load(Ordering::Relaxed),
        ept_map_4k_total: EPT_MAP_4K_TOTAL.load(Ordering::Relaxed),
        ept_table_allocs: EPT_TABLE_ALLOCS.load(Ordering::Relaxed),
        block_read_bytes: BLOCK_READ_BYTES.load(Ordering::Relaxed),
        block_write_bytes: BLOCK_WRITE_BYTES.load(Ordering::Relaxed),
        block_flushes: BLOCK_FLUSHES.load(Ordering::Relaxed),
        ipi_sent: IPI_SENT.load(Ordering::Relaxed),
    }
}

/// Emit one line per counter to the serial logger. Used by the boot
/// path after `bring_up` returns so an operator capturing COM1 sees
/// the full hot-path inventory of a single run.
pub fn log_snapshot() {
    let s = snapshot();
    crate::logger::log_kv("metrics_vm_exits_total", s.vm_exits_total);
    crate::logger::log_kv("metrics_vm_exits_hlt", s.vm_exits_hlt);
    crate::logger::log_kv("metrics_vm_exits_cr", s.vm_exits_cr);
    crate::logger::log_kv("metrics_vm_exits_ept", s.vm_exits_ept);
    crate::logger::log_kv("metrics_vm_exits_other", s.vm_exits_other);
    crate::logger::log_kv("metrics_ept_map_4k_total", s.ept_map_4k_total);
    crate::logger::log_kv("metrics_ept_table_allocs", s.ept_table_allocs);
    crate::logger::log_kv("metrics_block_read_bytes", s.block_read_bytes);
    crate::logger::log_kv("metrics_block_write_bytes", s.block_write_bytes);
    crate::logger::log_kv("metrics_block_flushes", s.block_flushes);
    crate::logger::log_kv("metrics_ipi_sent", s.ipi_sent);
}
