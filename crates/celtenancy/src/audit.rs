//! W30 — Tenant audit sink.
//!
//! [`AuditSink`] is a `Send + Sync` trait that absorbs
//! structured [`AuditEvent`]s emitted by [`crate::TenantVmHost`]
//! and [`crate::exec`] whenever the tenancy layer charges,
//! releases, denies, or executes a tenant-scoped op.
//!
//! Two shipped sinks:
//!
//! * [`MemAuditSink`] — in-memory ring of events for tests and
//!   diagnostics. Cloned via [`MemAuditSink::events`].
//! * [`FileAuditSink`] — append-only JSON-lines file at a
//!   configurable path. Reopen-safe; subsequent processes
//!   continue appending without truncating prior history.
//!
//! Recording is **best-effort**: `record` is infallible by
//! contract. The file sink swallows transient I/O errors after
//! logging them at `warn`, so audit failures never break a
//! tenant operation.

#![allow(clippy::module_name_repetitions)]

use std::fmt::Debug;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use celcommon::{CelError, CelResult};
use serde::{Deserialize, Serialize};

use crate::QuotaCharge;

/// Kind of audit event. `Charge` and `Release` track quota
/// movements; `Deny` records a refused op (either quota
/// exhaustion or a capability rejection); `Exec` is the
/// single-shot trip emitted by [`crate::exec`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    /// Tenant store was successfully charged for a resource-creating op.
    Charge,
    /// Tenant store was credited back after a delete or refund.
    Release,
    /// Op was rejected before reaching the Core Layer
    /// (quota exhausted, missing capability, etc.).
    Deny,
    /// One [`crate::exec::exec`] invocation finished.
    Exec,
}

/// One structured audit record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEvent {
    /// UNIX epoch milliseconds at record time.
    pub timestamp_millis: u64,
    /// Tenant name.
    pub tenant: String,
    /// Optional user that initiated the op.
    pub user: Option<String>,
    /// Action category.
    pub action: AuditAction,
    /// Stable Core-Layer capability tag (e.g. `"vm.create"`).
    pub op_capability_tag: Option<String>,
    /// Quota delta the wrapper planned, when relevant.
    pub charge: Option<QuotaCharge>,
    /// `true` for `Charge`/`Release`/`Exec` that succeeded,
    /// `false` for `Deny` or failed `Exec`.
    pub success: bool,
    /// Error string for `Deny` / failed `Exec`.
    pub error: Option<String>,
    /// Free-form annotation (`"refunded"`, `"dry-run"`, etc.).
    pub note: Option<String>,
}

impl AuditEvent {
    /// Build a new event stamped with the current wall clock.
    #[must_use]
    pub fn now(tenant: impl Into<String>, action: AuditAction) -> Self {
        Self {
            timestamp_millis: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            tenant: tenant.into(),
            user: None,
            action,
            op_capability_tag: None,
            charge: None,
            success: true,
            error: None,
            note: None,
        }
    }

    /// Attach the user name, if any.
    #[must_use]
    pub fn with_user(mut self, user: Option<String>) -> Self {
        self.user = user;
        self
    }

    /// Attach a capability tag.
    #[must_use]
    pub fn with_op_tag(mut self, tag: impl Into<String>) -> Self {
        self.op_capability_tag = Some(tag.into());
        self
    }

    /// Attach the planned charge.
    #[must_use]
    pub fn with_charge(mut self, charge: QuotaCharge) -> Self {
        self.charge = Some(charge);
        self
    }

    /// Flip to failure with an error message.
    #[must_use]
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.success = false;
        self.error = Some(error.into());
        self
    }

    /// Attach a free-form note.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// Sink for [`AuditEvent`]s. Implementors must be `Send + Sync`
/// and must not panic on `record`.
pub trait AuditSink: Send + Sync + Debug {
    /// Persist `event`. Best-effort; errors are swallowed.
    fn record(&self, event: AuditEvent);
}

/// In-memory audit sink — keeps all events in a `Vec` behind
/// a mutex. Cheap to clone the full history out of.
#[derive(Debug, Default)]
pub struct MemAuditSink {
    events: Mutex<Vec<AuditEvent>>,
}

impl MemAuditSink {
    /// Build an empty in-memory sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the recorded events.
    #[must_use]
    pub fn events(&self) -> Vec<AuditEvent> {
        match self.events.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// Number of events recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.events.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        }
    }

    /// `true` if no events were recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AuditSink for MemAuditSink {
    fn record(&self, event: AuditEvent) {
        match self.events.lock() {
            Ok(mut g) => g.push(event),
            Err(p) => p.into_inner().push(event),
        }
    }
}

/// Append-only JSON-lines audit sink on disk.
///
/// Each `record` call serializes the event as a single line of
/// JSON and appends it (followed by `\n`) to the file. Reads via
/// [`FileAuditSink::read_all`] / [`FileAuditSink::tail`] tolerate
/// malformed lines (skipped silently) so a process killed
/// mid-write does not poison the history.
#[derive(Debug)]
pub struct FileAuditSink {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileAuditSink {
    /// Open (creating if needed) the audit log at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`CelError::Io`] if the parent directory cannot
    /// be created or the file cannot be touched.
    pub fn open(path: impl Into<PathBuf>) -> CelResult<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| CelError::Io(format!("create_dir_all {}: {e}", parent.display())))?;
            }
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| CelError::Io(format!("open {}: {e}", path.display())))?;
        Ok(Self {
            path,
            lock: Mutex::new(()),
        })
    }

    /// Path the sink is writing to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Parse every well-formed line in the log into an
    /// [`AuditEvent`]. Malformed lines are skipped.
    ///
    /// # Errors
    ///
    /// Returns [`CelError::Io`] if the file cannot be read.
    pub fn read_all(&self) -> CelResult<Vec<AuditEvent>> {
        let _g = self.lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes = std::fs::read(&self.path)
            .map_err(|e| CelError::Io(format!("read {}: {e}", self.path.display())))?;
        let s = String::from_utf8_lossy(&bytes);
        let mut out = Vec::new();
        for line in s.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<AuditEvent>(trimmed) {
                out.push(ev);
            }
        }
        Ok(out)
    }

    /// Last `n` events, oldest-first. Equivalent to
    /// `read_all()?.into_iter().rev().take(n).rev().collect()`.
    ///
    /// # Errors
    ///
    /// Returns [`CelError::Io`] if the file cannot be read.
    pub fn tail(&self, n: usize) -> CelResult<Vec<AuditEvent>> {
        let all = self.read_all()?;
        let start = all.len().saturating_sub(n);
        Ok(all[start..].to_vec())
    }

    /// Count of well-formed events in the log.
    ///
    /// # Errors
    ///
    /// Returns [`CelError::Io`] if the file cannot be read.
    pub fn count(&self) -> CelResult<usize> {
        Ok(self.read_all()?.len())
    }
}

impl AuditSink for FileAuditSink {
    fn record(&self, event: AuditEvent) {
        let Ok(line) = serde_json::to_string(&event) else {
            return;
        };
        let _g = self.lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path);
        match opened {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{line}") {
                    tracing::warn!(
                        target: "celtenancy::audit",
                        path = %self.path.display(),
                        "audit write failed: {e}",
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "celtenancy::audit",
                    path = %self.path.display(),
                    "audit open failed: {e}",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn event_builder_chains() {
        let ev = AuditEvent::now("acme", AuditAction::Charge)
            .with_user(Some("alice".into()))
            .with_op_tag("vm.create")
            .with_charge(QuotaCharge {
                vcpus: 2,
                memory_mib: 1024,
                ..QuotaCharge::default()
            })
            .with_note("test");
        assert_eq!(ev.tenant, "acme");
        assert_eq!(ev.user.as_deref(), Some("alice"));
        assert_eq!(ev.action, AuditAction::Charge);
        assert_eq!(ev.op_capability_tag.as_deref(), Some("vm.create"));
        assert_eq!(ev.charge.as_ref().unwrap().vcpus, 2);
        assert!(ev.success);
        assert_eq!(ev.note.as_deref(), Some("test"));
        assert!(ev.timestamp_millis > 0);
    }

    #[test]
    fn event_with_error_flips_success() {
        let ev = AuditEvent::now("acme", AuditAction::Deny).with_error("boom");
        assert!(!ev.success);
        assert_eq!(ev.error.as_deref(), Some("boom"));
    }

    #[test]
    fn mem_sink_records_events() {
        let sink = MemAuditSink::new();
        assert!(sink.is_empty());
        sink.record(AuditEvent::now("acme", AuditAction::Charge));
        sink.record(AuditEvent::now("acme", AuditAction::Release));
        assert_eq!(sink.len(), 2);
        let events = sink.events();
        assert_eq!(events[0].action, AuditAction::Charge);
        assert_eq!(events[1].action, AuditAction::Release);
    }

    #[test]
    fn file_sink_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");
        let sink = FileAuditSink::open(&path).unwrap();
        sink.record(
            AuditEvent::now("acme", AuditAction::Charge)
                .with_op_tag("vm.create")
                .with_charge(QuotaCharge {
                    vcpus: 1,
                    ..QuotaCharge::default()
                }),
        );
        sink.record(AuditEvent::now("acme", AuditAction::Release));
        let events = sink.read_all().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].op_capability_tag.as_deref(), Some("vm.create"));
        assert_eq!(events[0].charge.as_ref().unwrap().vcpus, 1);
        assert_eq!(events[1].action, AuditAction::Release);
    }

    #[test]
    fn file_sink_survives_reopen_and_appends() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");
        {
            let sink = FileAuditSink::open(&path).unwrap();
            sink.record(AuditEvent::now("acme", AuditAction::Charge));
        }
        {
            let sink = FileAuditSink::open(&path).unwrap();
            sink.record(AuditEvent::now("acme", AuditAction::Deny).with_error("nope"));
            let events = sink.read_all().unwrap();
            assert_eq!(events.len(), 2);
            assert!(events[0].success);
            assert!(!events[1].success);
            assert_eq!(events[1].error.as_deref(), Some("nope"));
        }
    }

    #[test]
    fn file_sink_tail_returns_last_n() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");
        let sink = FileAuditSink::open(&path).unwrap();
        for _ in 0..5 {
            sink.record(AuditEvent::now("acme", AuditAction::Charge));
        }
        let tail = sink.tail(2).unwrap();
        assert_eq!(tail.len(), 2);
        let all = sink.read_all().unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(sink.count().unwrap(), 5);
    }

    #[test]
    fn file_sink_skips_malformed_lines() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("audit.jsonl");
        {
            let sink = FileAuditSink::open(&path).unwrap();
            sink.record(AuditEvent::now("acme", AuditAction::Charge));
        }
        // Append garbage manually.
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "this is not json").unwrap();
        writeln!(f, "").unwrap();
        drop(f);
        let sink = FileAuditSink::open(&path).unwrap();
        sink.record(AuditEvent::now("acme", AuditAction::Release));
        let events = sink.read_all().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].action, AuditAction::Charge);
        assert_eq!(events[1].action, AuditAction::Release);
    }
}
